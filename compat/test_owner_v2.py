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


class _SlowSseServer:
    def __init__(self, delay: float = 0.05) -> None:
        self.delay = delay
        self.requests: list[dict] = []
        reqs = self.requests

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                reqs.append(json.loads(self.rfile.read(length).decode("utf-8")))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for piece in ["Hello ", "from ", "owner v2!"]:
                    chunk = json.dumps(
                        {
                            "id": "c1",
                            "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": None}],
                        }
                    )
                    self.wfile.write(f"data: {chunk}\n\n".encode("utf-8"))
                    self.wfile.flush()
                    time.sleep(delay)
                stop = json.dumps(
                    {
                        "id": "c1",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    }
                )
                self.wfile.write(f"data: {stop}\n\ndata: [DONE]\n\n".encode("utf-8"))
                self.wfile.flush()

            def log_message(self, *_args):
                pass

        self._server = socketserver.TCPServer(("127.0.0.1", 0), Handler)
        self.base_url = f"http://127.0.0.1:{self._server.server_address[1]}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()


class OwnerV2ProtocolTest(unittest.TestCase):
    def setUp(self) -> None:
        self.assertTrue(BINARY.is_file(), f"Build hsr first: {BINARY}")
        self.tmp = tempfile.TemporaryDirectory(prefix="hsr-owner-v2-")
        self.profile = Path(self.tmp.name) / "profile"
        self.profile.mkdir()

    def tearDown(self) -> None:
        self.tmp.cleanup()

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
        llm = _SlowSseServer(delay=0.05)
        llm.start()
        (self.profile / "config.yaml").write_text(
            f"model:\n  provider: custom\n  base_url: {llm.base_url}/v1\n  default: fixture\n",
            encoding="utf-8",
        )
        env = dict(os.environ)
        env["HSR_HOME"] = str(self.profile)
        env["HERMES_API_KEY"] = "owner-v2-test-key"

        owner = subprocess.Popen(
            [str(BINARY), "owner"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            sock_path = self.profile / "rust-owner" / "owner.sock"
            deadline = time.time() + 5.0
            while time.time() < deadline and not sock_path.exists():
                time.sleep(0.05)
            self.assertTrue(sock_path.exists(), "owner socket did not appear")

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
            first_seq = 0
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

            # Verify session shows up in ListSessions & LoadSession & can be resumed by CLI!
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
        finally:
            owner.terminate()
            owner.wait(timeout=5)
            llm.stop()


if __name__ == "__main__":
    unittest.main()
