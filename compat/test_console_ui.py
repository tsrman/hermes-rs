"""End-to-end PTY and pipe tests for the Console UI: Inline streaming REPL, Fullscreen TUI, and Plain fallback."""

import fcntl
import json
import os
import pty
import select
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / ("hsr.exe" if os.name == "nt" else "hsr")


def _start_mock_server(reply_text="Hello **world**\n```rust\nfn main() {}\n```", with_tool=False):
    recorded = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length))
            recorded.append(body)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            if with_tool and len(recorded) == 1:
                chunks = [
                    {
                        "choices": [
                            {
                                "index": 0,
                                "delta": {
                                    "tool_calls": [
                                        {
                                            "index": 0,
                                            "id": "call_ui_1",
                                            "type": "function",
                                            "function": {
                                                "name": "read_file",
                                                "arguments": json.dumps({"path": "sample.txt"}),
                                            },
                                        }
                                    ]
                                },
                                "finish_reason": None,
                            }
                        ]
                    },
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            else:
                chunks = [
                    {"choices": [{"index": 0, "delta": {"content": reply_text}, "finish_reason": "stop"}]}
                ]
            for chunk in chunks:
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode("utf-8"))
            self.wfile.write(b"data: [DONE]\n\n")

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, f"http://127.0.0.1:{server.server_port}/v1", recorded


def _close_server(server):
    server.shutdown()
    server.server_close()


class ConsoleUiTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.profile = self.root / "hermes"
        self.profile.mkdir()
        (self.root / "sample.txt").write_text("file body", encoding="utf-8")

    def _write_config(self, base_url, extra=""):
        (self.profile / "config.yaml").write_text(
            f"model:\n  default: ui-model\n  provider: custom\n  base_url: {base_url}\n{extra}",
            encoding="utf-8",
        )

    def _base_env(self, **overrides):
        env = {
            "PATH": os.environ.get("PATH", ""),
            "HERMES_HOME": str(self.profile),
            "HERMES_API_KEY": "fixture",
            "TERM": "xterm-256color",
        }
        env.update(overrides)
        return env

    def _spawn_pty(self, args, env):
        master, slave = pty.openpty()
        # Give PTY a deterministic 100x30 window size so ratatui renders cleanly.
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        self.addCleanup(os.close, master)

        def setup_ctty():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            proc = subprocess.Popen(
                [str(BINARY), "chat", *args],
                cwd=self.root,
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                preexec_fn=setup_ctty,
            )
        finally:
            os.close(slave)

        def cleanup():
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=5)

        self.addCleanup(cleanup)
        return proc, master

    def _read_until(self, master, buf: bytearray, marker: bytes, count=1, start=0, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if buf[start:].count(marker) >= count:
                idx = start
                for _ in range(count):
                    idx = buf.index(marker, idx) + len(marker)
                return idx
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    data = os.read(master, 16384)
                    if not data:
                        break
                    buf.extend(data)
                except OSError:
                    break
        self.fail(f"marker {marker!r} x{count} (from {start}) not seen in {bytes(buf)!r}")

    def _finish_pty(self, proc, master, buf: bytearray, timeout=15):
        deadline = time.monotonic() + timeout
        while proc.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    data = os.read(master, 16384)
                    if data:
                        buf.extend(data)
                except OSError:
                    break
        # Drain any trailing bytes
        while True:
            ready, _, _ = select.select([master], [], [], 0.05)
            if not ready:
                break
            try:
                data = os.read(master, 16384)
                if not data:
                    break
                buf.extend(data)
            except OSError:
                break
        self.assertIsNotNone(proc.poll(), f"process did not exit; output={bytes(buf)!r}")
        return proc.returncode

    def test_pipe_falls_back_to_plain_even_when_tui_or_inline_requested(self):
        server, base_url, _ = _start_mock_server(reply_text="Plain reply")
        self.addCleanup(_close_server, server)
        self._write_config(base_url)

        for flag in ("--ui",):
            for mode in ("tui", "inline", "plain", "auto"):
                proc = subprocess.Popen(
                    [str(BINARY), "chat", flag, mode],
                    cwd=self.root,
                    env=self._base_env(),
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                out, err = proc.communicate("/ui\nhi\n", timeout=20)
                self.assertEqual(proc.returncode, 0, err)
                self.assertIn("Current UI mode: plain", out)
                self.assertIn("Plain reply", out)
                self.assertNotIn("\x1b[", out)
                self.assertNotIn("\x1b[", err)

    @unittest.skipUnless(sys.platform == "linux", "requires Linux PTY")
    def test_inline_repl_banner_markdown_tool_badges_and_tab_completion(self):
        server, base_url, recorded = _start_mock_server(
            reply_text="## Summary\nFound **ok** in `sample.txt`\n```rust\nfn demo() {}\n```",
            with_tool=True,
        )
        self.addCleanup(_close_server, server)
        self._write_config(base_url)

        proc, master = self._spawn_pty(["--ui", "inline"], self._base_env())
        buf = bytearray()
        pos = self._read_until(master, buf, "› ".encode())
        # Banner shows Hermes + model name + ANSI styling
        self.assertIn(b"Hermes", bytes(buf))
        self.assertIn(b"ui-model", bytes(buf))
        self.assertIn(b"\x1b[", bytes(buf))

        # Tab-complete `/mo` -> `/model`
        os.write(master, b"/mo\t\r")
        pos = self._read_until(master, buf, b"ui-model", start=pos)
        pos = self._read_until(master, buf, "› ".encode(), start=pos)

        # Send prompt that triggers a tool call and Markdown response
        os.write(master, b"read sample.txt\r")
        pos = self._read_until(master, buf, b"demo()", start=pos)
        self._read_until(master, buf, "› ".encode(), start=pos)
        os.write(master, b"\x04")
        self.assertEqual(self._finish_pty(proc, master, buf), 0)

        out = bytes(buf)
        self.assertEqual(len(recorded), 2)
        # Timed green checkmark tool badge + rendered Markdown + code fence frame
        self.assertIn("✓".encode(), out)
        self.assertIn(b"read_file", out)
        self.assertIn(b"Summary", out)
        self.assertIn("╭─ ".encode(), out)
        self.assertIn(b"rust", out)

    @unittest.skipUnless(sys.platform == "linux", "requires Linux PTY")
    def test_no_color_and_term_dumb_fallback_to_plain_on_tty(self):
        server, base_url, _ = _start_mock_server(reply_text="Simple text")
        self.addCleanup(_close_server, server)
        self._write_config(base_url)

        # NO_COLOR=1 with default auto mode -> plain
        proc, master = self._spawn_pty([], self._base_env(NO_COLOR="1"))
        buf = bytearray()
        pos = self._read_until(master, buf, "› ".encode())
        os.write(master, b"/ui\r")
        pos = self._read_until(master, buf, b"Current UI mode: plain", start=pos)
        self._read_until(master, buf, "› ".encode(), start=pos)
        os.write(master, b"\x04")
        self.assertEqual(self._finish_pty(proc, master, buf), 0)
        self.assertNotIn(b"\x1b[?1049h", bytes(buf))

        # TERM=dumb with --ui tui -> still falls back to plain (never enters alt screen)
        proc2, master2 = self._spawn_pty(["--ui", "tui"], self._base_env(TERM="dumb"))
        buf2 = bytearray()
        pos2 = self._read_until(master2, buf2, "› ".encode())
        os.write(master2, b"/ui\r")
        pos2 = self._read_until(master2, buf2, b"Current UI mode: plain", start=pos2)
        self._read_until(master2, buf2, "› ".encode(), start=pos2)
        os.write(master2, b"\x04")
        self.assertEqual(self._finish_pty(proc2, master2, buf2), 0)
        self.assertNotIn(b"\x1b[?1049h", bytes(buf2))

    @unittest.skipUnless(sys.platform == "linux", "requires Linux PTY")
    def test_fullscreen_tui_turn_persistence_and_runtime_mode_switch(self):
        server, base_url, recorded = _start_mock_server(reply_text="TuiReplyToken")
        self.addCleanup(_close_server, server)
        self._write_config(base_url, extra="ui:\n  mode: tui\n")

        # 1. Config `ui.mode: tui` opens fullscreen TUI directly
        proc, master = self._spawn_pty([], self._base_env())
        buf = bytearray()
        self._read_until(master, buf, b"\x1b[?1049h")
        self._read_until(master, buf, b"HERMES")
        pos = self._read_until(master, buf, b"ui-model")

        # Send a message in TUI and wait for model reply + turn completion notice
        os.write(master, b"hello from tui\r")
        pos = self._read_until(master, buf, b"TuiReplyToken", start=pos)
        pos = self._read_until(master, buf, b"s]", start=pos)

        # Switch at runtime from TUI -> Inline via `/ui inline`
        os.write(master, b"/ui inline\r")
        pos = self._read_until(master, buf, b"\x1b[?1049l", start=pos)
        pos = self._read_until(master, buf, "› ".encode(), start=pos)

        # Switch back from Inline -> TUI via `/ui tui`
        os.write(master, b"/ui tui\r")
        self._read_until(master, buf, b"\x1b[?1049h", start=pos)
        self._read_until(master, buf, b"HERMES", start=pos)

        # Exit TUI cleanly with Ctrl-D
        os.write(master, b"\x04")
        self.assertEqual(self._finish_pty(proc, master, buf), 0)
        self.assertGreaterEqual(buf.count(b"\x1b[?1049l"), 2)

        self.assertEqual(len(recorded), 1)
        with sqlite3.connect(self.profile / "state.db") as db:
            rows = db.execute("SELECT id, message_count FROM sessions").fetchall()
            self.assertEqual(len(rows), 1)


if __name__ == "__main__":
    unittest.main()
