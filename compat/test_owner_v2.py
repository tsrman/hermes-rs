"""End-to-end tests for the shared v2 local session owner protocol (used by hermes-gui)."""

from __future__ import annotations

import http.server
import json
import os
import socket
import socketserver
import struct
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "hsr"


def _write_frame(sock: socket.socket, payload: dict) -> None:
    data = json.dumps(payload).encode("utf-8")
    sock.sendall(struct.pack("!I", len(data)) + data)


def _read_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("socket closed")
        buf += chunk
    return buf


def _read_frame(sock: socket.socket) -> dict:
    length = struct.unpack("!I", _read_exact(sock, 4))[0]
    return json.loads(_read_exact(sock, length).decode("utf-8"))


class _ScriptedSseServer:
    """Thread-safe local OpenAI-compatible SSE server supporting text, slow streams, and tool calls."""

    def __init__(self, handler_fn) -> None:
        self.requests: list[dict] = []
        reqs = self.requests

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length).decode("utf-8"))
                reqs.append(body)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                try:
                    handler_fn(self.wfile, len(reqs), body)
                except BrokenPipeError:
                    pass

            def log_message(self, *_args):
                pass

        class ThreadedTCPServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
            allow_reuse_address = True
            daemon_threads = True

        self._server = ThreadedTCPServer(("127.0.0.1", 0), Handler)
        self.base_url = f"http://127.0.0.1:{self._server.server_address[1]}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()


def _send_sse_chunk(wfile, delta: dict, finish_reason: str | None = None) -> None:
    chunk = json.dumps(
        {
            "id": "c1",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        }
    )
    wfile.write(f"data: {chunk}\n\n".encode("utf-8"))
    wfile.flush()


def _send_sse_done(wfile) -> None:
    wfile.write(b"data: [DONE]\n\n")
    wfile.flush()


class OwnerV2ProtocolTest(unittest.TestCase):
    def setUp(self) -> None:
        self.assertTrue(BINARY.is_file(), f"Build hsr first: {BINARY}")
        self.tmp = tempfile.TemporaryDirectory(prefix="hsr-owner-v2-")
        self.profile = Path(self.tmp.name) / "profile"
        self.profile.mkdir()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _start_owner(self, profile: Path, base_url: str) -> subprocess.Popen:
        (profile / "config.yaml").write_text(
            f"model:\n  provider: custom\n  base_url: {base_url}/v1\n  default: fixture\n",
            encoding="utf-8",
        )
        env = dict(os.environ)
        env["HSR_HOME"] = str(profile)
        env["HERMES_API_KEY"] = "owner-v2-test-key"
        proc = subprocess.Popen(
            [str(BINARY), "owner"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        sock_path = profile / "rust-owner" / "owner.sock"
        deadline = time.time() + 5.0
        while time.time() < deadline and not sock_path.exists():
            time.sleep(0.05)
        self.assertTrue(sock_path.exists(), f"owner socket did not appear in {profile}")
        return proc

    def _stop_owner(self, proc: subprocess.Popen) -> None:
        proc.terminate()
        proc.wait(timeout=5)
        if proc.stdout:
            proc.stdout.close()
        if proc.stderr:
            proc.stderr.close()

    def _connect(self, profile: Path) -> socket.socket:
        sock_path = profile / "rust-owner" / "owner.sock"
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(10.0)
        s.connect(str(sock_path))
        return s

    def _rpc(self, profile: Path, command: dict) -> dict:
        with self._connect(profile) as s:
            _write_frame(s, {"version": 2, "command": command})
            return _read_frame(s)

    def test_v2_start_turn_disconnect_reconnect_replay_and_resume_in_cli(self) -> None:
        if os.name != "posix":
            self.skipTest("Unix socket helper test runs on POSIX")

        def respond(wfile, call_idx, _body):
            pieces = ["Hello ", "from ", "owner v2!"] if call_idx == 1 else ["Resumed in CLI!"]
            for piece in pieces:
                _send_sse_chunk(wfile, {"content": piece})
                time.sleep(0.04)
            _send_sse_chunk(wfile, {}, finish_reason="stop")
            _send_sse_done(wfile)

        llm = _ScriptedSseServer(respond)
        llm.start()
        owner = self._start_owner(self.profile, llm.base_url)
        try:
            pong = self._rpc(self.profile, {"type": "ping"})
            self.assertEqual(pong["type"], "pong")

            started = self._rpc(
                self.profile,
                {
                    "type": "start_turn",
                    "session_id": None,
                    "query": "Say hi",
                    "model": "fixture",
                    "cwd": str(self.profile),
                    "yolo": False,
                },
            )
            self.assertEqual(started["type"], "turn_started")
            turn_id = started["turn_id"]

            # Client 1 subscribes briefly, reads first event, then disconnects mid-stream
            with self._connect(self.profile) as s1:
                _write_frame(
                    s1,
                    {
                        "version": 2,
                        "command": {"type": "subscribe_turn", "turn_id": turn_id, "after_seq": 0},
                    },
                )
                first = _read_frame(s1)
                self.assertEqual(first["type"], "turn_event")
                first_seq = first["seq"]

            # Client 2 reconnects with after_seq=0 and replays the entire stream to Done!
            collected_text = []
            session_id = None
            with self._connect(self.profile) as s2:
                _write_frame(
                    s2,
                    {
                        "version": 2,
                        "command": {"type": "subscribe_turn", "turn_id": turn_id, "after_seq": 0},
                    },
                )
                while True:
                    msg = _read_frame(s2)
                    self.assertEqual(msg["type"], "turn_event")
                    ev = msg["event"]
                    if ev["kind"] == "text_delta":
                        collected_text.append(ev["text"])
                    elif ev["kind"] == "session_assigned":
                        session_id = ev["session_id"]
                    elif ev["kind"] == "done":
                        self.assertEqual(ev["exit_code"], 0)
                        break

            self.assertGreaterEqual(first_seq, 1)
            self.assertEqual("".join(collected_text).strip(), "Hello from owner v2!")
            self.assertIsNotNone(session_id)

            # Verify session shows up in ListSessions & LoadSession
            listed = self._rpc(
                self.profile, {"type": "list_sessions", "limit": 20, "query": "owner v2"}
            )
            self.assertEqual(listed["type"], "sessions")
            self.assertTrue(any(s["id"] == session_id for s in listed["sessions"]))

            transcript = self._rpc(
                self.profile, {"type": "load_session", "session_id": session_id}
            )
            self.assertEqual(transcript["type"], "transcript")
            self.assertEqual(transcript["session"]["id"], session_id)
            self.assertTrue(
                any("Hello from owner v2!" in m["content"] for m in transcript["messages"])
            )

            # Continue the exact same GUI-created session directly via CLI `--resume`!
            env = dict(os.environ)
            env["HSR_HOME"] = str(self.profile)
            env["HERMES_API_KEY"] = "owner-v2-test-key"
            cli_res = subprocess.run(
                [
                    str(BINARY),
                    "chat",
                    "--oneshot",
                    "--resume",
                    session_id,
                    "-q",
                    "Continue from CLI",
                ],
                env=env,
                cwd=str(self.profile),
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(cli_res.returncode, 0, cli_res.stderr)
            self.assertIn("Resumed in CLI!", cli_res.stdout)
        finally:
            self._stop_owner(owner)
            llm.stop()

    def test_v2_pending_approval_survives_client_disconnect_and_completes_after_reconnect(
        self,
    ) -> None:
        if os.name != "posix":
            self.skipTest("Unix socket helper test runs on POSIX")

        work_dir = Path(self.tmp.name) / "work"
        work_dir.mkdir()

        def respond(wfile, call_idx, _body):
            if call_idx == 1:
                _send_sse_chunk(
                    wfile,
                    {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_write_1",
                                "type": "function",
                                "function": {
                                    "name": "write_file",
                                    "arguments": json.dumps(
                                        {"path": "created.txt", "content": "approved via v2"}
                                    ),
                                },
                            }
                        ]
                    },
                )
                _send_sse_chunk(wfile, {}, finish_reason="tool_calls")
                _send_sse_done(wfile)
            else:
                _send_sse_chunk(wfile, {"content": "File written!"})
                _send_sse_chunk(wfile, {}, finish_reason="stop")
                _send_sse_done(wfile)

        llm = _ScriptedSseServer(respond)
        llm.start()
        owner = self._start_owner(self.profile, llm.base_url)
        try:
            started = self._rpc(
                self.profile,
                {
                    "type": "start_turn",
                    "session_id": None,
                    "query": "Create created.txt",
                    "model": "fixture",
                    "cwd": str(work_dir),
                    "yolo": False,
                },
            )
            turn_id = started["turn_id"]

            # Client 1 sees approval_required and disconnects WITHOUT answering!
            approval_id = None
            with self._connect(self.profile) as s1:
                _write_frame(
                    s1,
                    {
                        "version": 2,
                        "command": {"type": "subscribe_turn", "turn_id": turn_id, "after_seq": 0},
                    },
                )
                while True:
                    msg = _read_frame(s1)
                    if msg["type"] == "turn_event" and msg["event"]["kind"] == "approval_required":
                        approval_id = msg["event"]["approval_id"]
                        break
            self.assertIsNotNone(approval_id)

            # After disconnect, ActiveTurns still reports pending approval!
            active = self._rpc(self.profile, {"type": "active_turns"})
            self.assertEqual(active["type"], "active_turns")
            matching = [t for t in active["turns"] if t["turn_id"] == turn_id]
            self.assertEqual(len(matching), 1)
            self.assertTrue(matching[0]["has_pending_approval"])
            self.assertFalse(matching[0]["finished"])

            # Client 2 reconnects, approves, and streams to completion
            ack = self._rpc(
                self.profile,
                {
                    "type": "respond_approval",
                    "turn_id": turn_id,
                    "approval_id": approval_id,
                    "decision": "Approved",
                },
            )
            self.assertEqual(ack["type"], "ack")

            final_text = []
            with self._connect(self.profile) as s2:
                _write_frame(
                    s2,
                    {
                        "version": 2,
                        "command": {"type": "subscribe_turn", "turn_id": turn_id, "after_seq": 0},
                    },
                )
                while True:
                    msg = _read_frame(s2)
                    ev = msg["event"]
                    if ev["kind"] == "text_delta":
                        final_text.append(ev["text"])
                    elif ev["kind"] == "done":
                        self.assertEqual(ev["exit_code"], 0)
                        break

            self.assertEqual((work_dir / "created.txt").read_text(encoding="utf-8"), "approved via v2")
            self.assertIn("File written!", "".join(final_text))
        finally:
            self._stop_owner(owner)
            llm.stop()

    def test_v2_cancel_turn_and_profile_isolation(self) -> None:
        if os.name != "posix":
            self.skipTest("Unix socket helper test runs on POSIX")

        profile_b = Path(self.tmp.name) / "profile-b"
        profile_b.mkdir()

        def respond(wfile, _call_idx, body):
            user_text = body["messages"][-1]["content"]
            if "SLOW" in str(user_text):
                for i in range(40):
                    _send_sse_chunk(wfile, {"content": f"chunk{i} "})
                    time.sleep(0.1)
            else:
                _send_sse_chunk(wfile, {"content": "Isolated profile A reply"})
            _send_sse_chunk(wfile, {}, finish_reason="stop")
            _send_sse_done(wfile)

        llm = _ScriptedSseServer(respond)
        llm.start()
        owner_a = self._start_owner(self.profile, llm.base_url)
        owner_b = self._start_owner(profile_b, llm.base_url)
        try:
            # Create a session in profile A
            started_a = self._rpc(
                self.profile,
                {
                    "type": "start_turn",
                    "session_id": None,
                    "query": "Secret profile A note",
                    "model": "fixture",
                    "cwd": str(self.profile),
                    "yolo": False,
                },
            )
            with self._connect(self.profile) as sa:
                _write_frame(
                    sa,
                    {
                        "version": 2,
                        "command": {
                            "type": "subscribe_turn",
                            "turn_id": started_a["turn_id"],
                            "after_seq": 0,
                        },
                    },
                )
                while _read_frame(sa)["event"]["kind"] != "done":
                    pass

            # Profile A sees its session; Profile B sees zero sessions!
            sessions_a = self._rpc(
                self.profile, {"type": "list_sessions", "limit": 20, "query": None}
            )["sessions"]
            sessions_b = self._rpc(
                profile_b, {"type": "list_sessions", "limit": 20, "query": None}
            )["sessions"]
            self.assertEqual(len(sessions_a), 1)
            self.assertEqual(len(sessions_b), 0)

            # Start a slow turn on profile B and cancel it mid-stream
            slow = self._rpc(
                profile_b,
                {
                    "type": "start_turn",
                    "session_id": None,
                    "query": "SLOW stream please",
                    "model": "fixture",
                    "cwd": str(profile_b),
                    "yolo": False,
                },
            )
            tid = slow["turn_id"]
            with self._connect(profile_b) as sb:
                _write_frame(
                    sb,
                    {
                        "version": 2,
                        "command": {"type": "subscribe_turn", "turn_id": tid, "after_seq": 0},
                    },
                )
                # Wait for first chunk, then cancel!
                while True:
                    msg = _read_frame(sb)
                    if msg["event"]["kind"] == "text_delta":
                        break
                ack = self._rpc(profile_b, {"type": "cancel_turn", "turn_id": tid})
                self.assertEqual(ack["type"], "ack")
                cancelled_seen = False
                while True:
                    msg = _read_frame(sb)
                    if msg["event"]["kind"] == "done":
                        cancelled_seen = bool(msg["event"]["cancelled"])
                        break
                self.assertTrue(cancelled_seen)
        finally:
            self._stop_owner(owner_a)
            self._stop_owner(owner_b)
            llm.stop()


if __name__ == "__main__":
    unittest.main()
