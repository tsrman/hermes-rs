"""End-to-end CLI tests for `hsr auth`, `hsr browser`, `/browser connect`, and live_provider_smoke."""

from __future__ import annotations

import base64
import hashlib
import http.server
import json
import os
import re
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "hsr"


class _MockOpenAiSseServer:
    """In-process OpenAI Chat Completions SSE mock that records every request body."""

    def __init__(self) -> None:
        self.requests: list[dict] = []
        requests = self.requests

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                requests.append(json.loads(raw.decode("utf-8")))
                chunk1 = json.dumps(
                    {
                        "id": "chatcmpl-test",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": "OK"}, "finish_reason": None}],
                    }
                )
                chunk2 = json.dumps(
                    {
                        "id": "chatcmpl-test",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    }
                )
                body = f"data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n".encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        self._server = socketserver.TCPServer(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self.base_url = f"http://127.0.0.1:{self.port}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()


def _make_fake_jwt(exp: int) -> str:
    h = base64.urlsafe_b64encode(b'{"alg":"RS256","typ":"JWT"}').rstrip(b"=").decode()
    p = (
        base64.urlsafe_b64encode(json.dumps({"sub": "u1", "exp": exp}).encode())
        .rstrip(b"=")
        .decode()
    )
    return f"{h}.{p}.sig"


class _MockCdpWsServer:
    """Minimal RFC 6455 WebSocket + HTTP `/json/version` mock CDP endpoint (stdlib only)."""

    def __init__(self) -> None:
        self._stop = threading.Event()
        self.ws_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.ws_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.ws_sock.bind(("127.0.0.1", 0))
        self.ws_sock.listen(8)
        self.ws_sock.settimeout(0.2)
        self.ws_port = self.ws_sock.getsockname()[1]
        self.ws_url = f"ws://127.0.0.1:{self.ws_port}/devtools/browser/test-live"

        ws_url = self.ws_url

        class VersionHandler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                payload = json.dumps(
                    {
                        "Browser": "Chrome/131.0.6778.0",
                        "Protocol-Version": "1.3",
                        "webSocketDebuggerUrl": ws_url,
                    }
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *_args):
                pass

        self.http_server = socketserver.TCPServer(("127.0.0.1", 0), VersionHandler)
        self.http_port = self.http_server.server_address[1]

        self._http_thread = threading.Thread(target=self.http_server.serve_forever, daemon=True)
        self._ws_thread = threading.Thread(target=self._serve_ws, daemon=True)

    def start(self) -> None:
        self._http_thread.start()
        self._ws_thread.start()

    def stop(self) -> None:
        self._stop.set()
        self.http_server.shutdown()
        self.http_server.server_close()
        try:
            self.ws_sock.close()
        except OSError:
            pass

    def _serve_ws(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self.ws_sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=self._handle_ws_conn, args=(conn,), daemon=True).start()

    def _handle_ws_conn(self, conn: socket.socket) -> None:
        conn.settimeout(2.0)
        try:
            raw = b""
            while b"\r\n\r\n" not in raw:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                raw += chunk
            key = ""
            for line in raw.decode("latin1").splitlines():
                if line.lower().startswith("sec-websocket-key:"):
                    key = line.split(":", 1)[1].strip()
            accept = base64.b64encode(
                hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
            ).decode()
            handshake = (
                "HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            conn.sendall(handshake.encode())

            while not self._stop.is_set():
                msg = self._recv_ws_frame(conn)
                if msg is None:
                    break
                req = json.loads(msg)
                req_id = req.get("id")
                method = req.get("method", "")
                if method == "Target.getTargets":
                    result = {
                        "targetInfos": [
                            {
                                "targetId": "TAB1",
                                "type": "page",
                                "title": "Live CDP Tab",
                                "url": "https://example.com/",
                            }
                        ]
                    }
                elif method == "Target.attachToTarget":
                    result = {"sessionId": "SID1"}
                else:
                    result = {}
                self._send_ws_text(conn, json.dumps({"id": req_id, "result": result}))
        except OSError:
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    @staticmethod
    def _recv_exact(conn: socket.socket, n: int) -> bytes | None:
        buf = b""
        while len(buf) < n:
            chunk = conn.recv(n - len(buf))
            if not chunk:
                return None
            buf += chunk
        return buf

    def _recv_ws_frame(self, conn: socket.socket) -> str | None:
        hdr = self._recv_exact(conn, 2)
        if not hdr:
            return None
        opcode = hdr[0] & 0x0F
        if opcode == 0x8:
            return None
        masked = bool(hdr[1] & 0x80)
        length = hdr[1] & 0x7F
        if length == 126:
            ext = self._recv_exact(conn, 2)
            if not ext:
                return None
            length = struct.unpack("!H", ext)[0]
        elif length == 127:
            ext = self._recv_exact(conn, 8)
            if not ext:
                return None
            length = struct.unpack("!Q", ext)[0]
        mask = self._recv_exact(conn, 4) if masked else b""
        payload = self._recv_exact(conn, length)
        if payload is None:
            return None
        if masked:
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        return payload.decode("utf-8", errors="replace")

    @staticmethod
    def _send_ws_text(conn: socket.socket, text: str) -> None:
        data = text.encode("utf-8")
        n = len(data)
        if n < 126:
            frame = bytes([0x81, n]) + data
        elif n < 65536:
            frame = bytes([0x81, 126]) + struct.pack("!H", n) + data
        else:
            frame = bytes([0x81, 127]) + struct.pack("!Q", n) + data
        conn.sendall(frame)


class AuthAndBrowserCliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.assertTrue(BINARY.is_file(), f"Build hsr first: {BINARY}")
        self.tmp = tempfile.TemporaryDirectory(prefix="hsr-auth-browser-")
        self.profile = Path(self.tmp.name) / "profile"
        self.work = Path(self.tmp.name) / "work"
        self.profile.mkdir()
        self.work.mkdir()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _env(self, **extra: str) -> dict[str, str]:
        env = dict(os.environ)
        env["HSR_HOME"] = str(self.profile)
        env["CODEX_HOME"] = str(self.profile / "no_codex")
        env["HOME"] = str(self.profile / "fake_home")
        env.pop("ANTHROPIC_API_KEY", None)
        env.pop("ANTHROPIC_TOKEN", None)
        env.pop("CLAUDE_CODE_OAUTH_TOKEN", None)
        env.pop("BROWSER_CDP_URL", None)
        env.update(extra)
        return env

    def test_auth_device_login_status_and_logout_for_nous(self) -> None:
        jwt_tok = _make_fake_jwt(int(time.time()) + 7200)
        polls = {"n": 0}

        class OAuthHandler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                _ = self.rfile.read(length)
                if self.path == "/api/oauth/device/code":
                    body = json.dumps(
                        {
                            "device_code": "dev-code-1",
                            "user_code": "TEST-9876",
                            "verification_uri": "https://portal.nousresearch.com/device",
                            "interval": 1,
                            "expires_in": 60,
                        }
                    ).encode()
                    self.send_response(200)
                else:
                    polls["n"] += 1
                    if polls["n"] == 1:
                        body = json.dumps({"error": "authorization_pending"}).encode()
                        self.send_response(400)
                    else:
                        body = json.dumps(
                            {
                                "access_token": jwt_tok,
                                "refresh_token": "rt-nous-secret",
                                "inference_base_url": "https://inference-api.nousresearch.com/v1",
                            }
                        ).encode()
                        self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        srv = socketserver.TCPServer(("127.0.0.1", 0), OAuthHandler)
        port = srv.server_address[1]
        t = threading.Thread(target=srv.serve_forever, daemon=True)
        t.start()
        try:
            env = self._env(HERMES_OAUTH_POLL_INTERVAL_MS="50")
            login_proc = subprocess.run(
                [
                    str(BINARY),
                    "auth",
                    "login",
                    "nous",
                    "--no-browser",
                    "--portal-url",
                    f"http://127.0.0.1:{port}",
                ],
                cwd=self.work,
                env=env,
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(login_proc.returncode, 0, login_proc.stderr)
            self.assertIn("TEST-9876", login_proc.stdout)
            self.assertNotIn("rt-nous-secret", login_proc.stdout)
            self.assertNotIn(jwt_tok, login_proc.stdout)

            auth_path = self.profile / "auth.json"
            self.assertTrue(auth_path.is_file())
            if os.name == "posix":
                self.assertEqual(auth_path.stat().st_mode & 0o777, 0o600)

            status_proc = subprocess.run(
                [str(BINARY), "auth", "status", "--json"],
                cwd=self.work,
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(status_proc.returncode, 0, status_proc.stderr)
            items = json.loads(status_proc.stdout)
            nous = next(i for i in items if i["provider"] == "nous")
            self.assertTrue(nous["authenticated"])
            self.assertTrue(nous["has_refresh_token"])
            self.assertFalse(nous["expired"])
            self.assertNotIn(jwt_tok, status_proc.stdout)

            logout_proc = subprocess.run(
                [str(BINARY), "auth", "logout", "nous"],
                cwd=self.work,
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(logout_proc.returncode, 0, logout_proc.stderr)
            self.assertIn("nous", logout_proc.stdout)
        finally:
            srv.shutdown()
            srv.server_close()

    def test_auth_pkce_browser_login_for_codex_and_anthropic_token(self) -> None:
        jwt_tok = _make_fake_jwt(int(time.time()) + 7200)

        class TokenHandler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                _ = self.rfile.read(length)
                body = json.dumps(
                    {
                        "access_token": jwt_tok,
                        "refresh_token": "rt-codex-1",
                    }
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        srv = socketserver.TCPServer(("127.0.0.1", 0), TokenHandler)
        port = srv.server_address[1]
        t = threading.Thread(target=srv.serve_forever, daemon=True)
        t.start()
        try:
            env = self._env(
                HERMES_CODEX_AUTHORIZE_URL=f"http://127.0.0.1:{port}/authorize",
                HERMES_CODEX_TOKEN_URL=f"http://127.0.0.1:{port}/oauth/token",
            )
            proc = subprocess.Popen(
                [
                    str(BINARY),
                    "auth",
                    "login",
                    "codex",
                    "--no-browser",
                    "--port",
                    "0",
                    "--timeout",
                    "20",
                ],
                cwd=self.work,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            assert proc.stdout is not None
            auth_url = None
            for line in proc.stdout:
                m = re.search(r"(http://127\.0\.0\.1:\d+/authorize\S+)", line)
                if m:
                    auth_url = m.group(1)
                    break
            self.assertIsNotNone(auth_url, "CLI did not print authorize URL")
            qs = urllib.parse.parse_qs(urllib.parse.urlparse(auth_url).query)
            redirect_uri = qs["redirect_uri"][0]
            state = qs["state"][0]
            cb_url = f"{redirect_uri}?code=test-auth-code&state={state}"
            with urllib.request.urlopen(cb_url, timeout=5) as resp:
                self.assertEqual(resp.status, 200)
            stdout_rest, stderr = proc.communicate(timeout=15)
            self.assertEqual(proc.returncode, 0, stderr)
            self.assertIn("Authenticated 'openai-codex'", stdout_rest)

            # Also verify anthropic token save does not leak secret to stdout
            anth = subprocess.run(
                [
                    str(BINARY),
                    "auth",
                    "login",
                    "anthropic",
                    "--token",
                    "sk-ant-oat01-exampletesttokenvalue1234",
                ],
                cwd=self.work,
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(anth.returncode, 0, anth.stderr)
            self.assertNotIn("sk-ant-oat01-exampletesttokenvalue1234", anth.stdout)
            status = subprocess.run(
                [str(BINARY), "auth", "status", "--json"],
                cwd=self.work,
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            items = json.loads(status.stdout)
            anth_item = next(i for i in items if i["provider"] == "anthropic")
            self.assertTrue(anth_item["authenticated"])
        finally:
            srv.shutdown()
            srv.server_close()

    def test_browser_connect_cli_and_live_repl_tool_activation(self) -> None:
        cdp = _MockCdpWsServer()
        cdp.start()
        llm = _MockOpenAiSseServer()
        llm.start()
        try:
            env = self._env(HERMES_API_KEY="test-key")
            # 1. Standalone CLI `hsr browser connect <port> --save`
            conn_proc = subprocess.run(
                [str(BINARY), "browser", "connect", str(cdp.http_port), "--save"],
                cwd=self.work,
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(conn_proc.returncode, 0, conn_proc.stderr)
            self.assertIn("Connected to Chrome/131.0.6778.0", conn_proc.stdout)
            self.assertIn("1 page tab(s)", conn_proc.stdout)
            cfg_text = (self.profile / "config.yaml").read_text(encoding="utf-8")
            self.assertIn(cdp.ws_url, cfg_text)

            # 2. Interactive REPL without configured browser: /browser connect enables browser tools
            # on the very next turn, and /browser disconnect removes them again.
            (self.profile / "config.yaml").write_text("{}\n", encoding="utf-8")
            script = (
                f"/browser connect {cdp.http_port}\n"
                "hello with browser\n"
                "/browser status\n"
                "/browser disconnect\n"
                "hello without browser\n"
                "/exit\n"
            )
            repl = subprocess.run(
                [
                    str(BINARY),
                    "chat",
                    "--ui",
                    "plain",
                    "--base-url",
                    f"{llm.base_url}/v1",
                    "--model",
                    "fixture",
                ],
                cwd=self.work,
                env=env,
                input=script,
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(repl.returncode, 0, repl.stderr)
            self.assertIn("Connected to Chrome/131.0.6778.0", repl.stdout)
            self.assertIn("Live CDP Tab", repl.stdout)
            self.assertIn("Disconnected live browser session", repl.stdout)

            self.assertEqual(len(llm.requests), 2)
            tools_turn1 = {
                t["function"]["name"] for t in llm.requests[0].get("tools", [])
            }
            tools_turn2 = {
                t["function"]["name"] for t in llm.requests[1].get("tools", [])
            }
            self.assertIn("browser_cdp", tools_turn1)
            self.assertIn("browser_dialog", tools_turn1)
            self.assertIn("browser_exec", tools_turn1)
            self.assertNotIn("browser_cdp", tools_turn2)
            self.assertNotIn("browser_dialog", tools_turn2)
            self.assertNotIn("browser_exec", tools_turn2)

            # 3. Smoke runner against mock server
            smoke = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "compat" / "live_provider_smoke.py"),
                    "--binary",
                    str(BINARY),
                    "--mock-url",
                    f"{llm.base_url}/v1",
                    "--model",
                    "fixture",
                ],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(smoke.returncode, 0, smoke.stdout + smoke.stderr)
            self.assertIn("mock (fixture): OK", smoke.stdout)
        finally:
            llm.stop()
            cdp.stop()


if __name__ == "__main__":
    unittest.main()
