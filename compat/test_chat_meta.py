"""End-to-end tests for `@` context references, `/<skill-name>`, `/skills`, `/undo`, `/reset`, `/clear`, and `!<cmd>`."""

from __future__ import annotations

import http.server
import json
import os
import socketserver
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "hsr"


class _RecordingSseServer:
    def __init__(self) -> None:
        self.requests: list[dict] = []
        reqs = self.requests

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length).decode("utf-8"))
                reqs.append(body)
                idx = len(reqs)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                c1 = json.dumps(
                    {
                        "id": f"c{idx}",
                        "object": "chat.completion.chunk",
                        "choices": [
                            {"index": 0, "delta": {"content": f"reply-{idx}"}, "finish_reason": None}
                        ],
                    }
                )
                c2 = json.dumps(
                    {
                        "id": f"c{idx}",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    }
                )
                self.wfile.write(f"data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n".encode("utf-8"))
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


class ChatMetaFeaturesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.assertTrue(BINARY.is_file(), f"Build hsr first: {BINARY}")
        self.tmp = tempfile.TemporaryDirectory(prefix="hsr-chat-meta-")
        self.profile = Path(self.tmp.name) / "profile"
        self.work = Path(self.tmp.name) / "work"
        self.profile.mkdir()
        self.work.mkdir()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _env(self) -> dict[str, str]:
        env = dict(os.environ)
        env["HSR_HOME"] = str(self.profile)
        env["HERMES_API_KEY"] = "chat-meta-test-key"
        return env

    def test_at_references_expand_file_range_and_folder_in_oneshot(self) -> None:
        srv = _RecordingSseServer()
        srv.start()
        try:
            (self.profile / "config.yaml").write_text(
                f"model:\n  provider: custom\n  base_url: {srv.base_url}/v1\n  default: fixture\n",
                encoding="utf-8",
            )
            (self.work / "sub").mkdir()
            (self.work / "sub" / "notes.txt").write_text(
                "alpha\nbeta\ngamma\ndelta\n", encoding="utf-8"
            )
            (self.work / ".env").write_text("TOP_SECRET=do-not-leak\n", encoding="utf-8")

            res = subprocess.run(
                [
                    str(BINARY),
                    "chat",
                    "--oneshot",
                    "-q",
                    "Review @file:sub/notes.txt:2-3 and @folder:sub, ignore @file:.env and foo@bar.com",
                ],
                env=self._env(),
                cwd=str(self.work),
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            self.assertEqual(len(srv.requests), 1)
            user_msg = srv.requests[0]["messages"][-1]["content"]
            self.assertIn("2 | beta", user_msg)
            self.assertIn("3 | gamma", user_msg)
            self.assertNotIn("1 | alpha", user_msg)
            self.assertIn("notes.txt", user_msg)
            self.assertNotIn("TOP_SECRET=do-not-leak", user_msg)
            self.assertIn("secret", res.stderr.lower())
        finally:
            srv.stop()

    def test_interactive_skill_command_bang_shell_and_undo(self) -> None:
        srv = _RecordingSseServer()
        srv.start()
        try:
            (self.profile / "config.yaml").write_text(
                f"model:\n  provider: custom\n  base_url: {srv.base_url}/v1\n  default: fixture\n",
                encoding="utf-8",
            )
            skill_dir = self.profile / "skills" / "dev" / "audit"
            skill_dir.mkdir(parents=True)
            (skill_dir / "SKILL.md").write_text(
                "---\nname: audit\ndescription: Security audit helper\n---\nCHECKLIST: verify auth.\n",
                encoding="utf-8",
            )

            script = "\n".join(
                [
                    "!echo bang_shell_works",
                    "/skills",
                    "/audit check login",
                    "second message",
                    "/undo",
                    "after undo message",
                    "/clear",
                    "/reset",
                    "/exit",
                    "",
                ]
            )
            res = subprocess.run(
                [str(BINARY), "chat", "--ui", "plain"],
                input=script,
                env=self._env(),
                cwd=str(self.work),
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            self.assertIn("bang_shell_works", res.stdout)
            self.assertIn("/audit", res.stdout)
            self.assertIn("Security audit helper", res.stdout)
            self.assertIn("Undid last exchange", res.stdout)

            # 3 LLM calls total: `/audit check login`, `second message`, `after undo message`
            self.assertEqual(len(srv.requests), 3)
            # Call 1 received the skill instructions
            first_user = srv.requests[0]["messages"][-1]["content"]
            self.assertIn("CHECKLIST: verify auth.", first_user)
            self.assertIn("check login", first_user)
            # Call 3 after `/undo` must NOT contain `second message` in its history!
            third_history = json.dumps(srv.requests[2]["messages"])
            self.assertNotIn("second message", third_history)
            self.assertIn("after undo message", third_history)
        finally:
            srv.stop()


if __name__ == "__main__":
    unittest.main()
