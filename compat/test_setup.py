"""End-to-end tests for `hsr setup`, `hsr model`, and `hsr doctor`."""

from __future__ import annotations

import http.server
import json
import os
import socketserver
import stat
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "hsr"

EXPECTED_DIRS = [
    "memories",
    "skills",
    "plugins",
    "scripts",
    "cron",
    "cron/output",
    "cron/cancels",
    "cache/scratch",
    "cache/images",
    "cache/audio",
    "cache/documents",
    "cache/spillover",
    "logs",
    "vault",
    "mcp_oauth",
    "rust-owner",
]


class _SimpleSseServer:
    def __init__(self, reply: str = "Hello from setup!") -> None:
        self.reply = reply

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                _ = self.rfile.read(length)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                c1 = json.dumps(
                    {
                        "id": "c1",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": reply}, "finish_reason": None}],
                    }
                )
                c2 = json.dumps(
                    {
                        "id": "c1",
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


class SetupWizardCliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.assertTrue(BINARY.is_file(), f"Build hsr first: {BINARY}")
        self.tmp = tempfile.TemporaryDirectory(prefix="hsr-setup-test-")
        self.profile = Path(self.tmp.name) / "fresh-profile"

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _clean_env(self) -> dict[str, str]:
        env = dict(os.environ)
        for key in (
            "HERMES_API_KEY",
            "HERMES_BASE_URL",
            "HERMES_MODEL",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "TAVILY_API_KEY",
        ):
            env.pop(key, None)
        env["HSR_HOME"] = str(self.profile)
        return env

    def test_interactive_setup_creates_all_dirs_populates_files_and_chats(self) -> None:
        llm = _SimpleSseServer("Setup wizard verified!")
        llm.start()
        try:
            env = self._clean_env()

            # Before setup, doctor reports action required (exit 1)
            before = subprocess.run(
                [str(BINARY), "doctor", "--json"],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(before.returncode, 1)
            before_json = json.loads(before.stdout)
            self.assertFalse(before_json["ready"])

            # Walk through the interactive wizard:
            # 1) provider = 10 (custom)
            # 2) base_url = llm.base_url + /v1
            # 3) api_mode = openai
            # 4) HERMES_API_KEY = super-secret-setup-token
            # 5) default model = setup-fixture-model
            # 6) ui mode = inline
            # 7) memory enabled = y
            # 8) web search backend = tavily
            # 9) TAVILY_API_KEY = tvly-test-secret
            wizard_input = "\n".join(
                [
                    "10",
                    f"{llm.base_url}/v1",
                    "openai",
                    "super-secret-setup-token",
                    "setup-fixture-model",
                    "inline",
                    "y",
                    "tavily",
                    "tvly-test-secret",
                    "",
                ]
            )
            res = subprocess.run(
                [str(BINARY), "setup"],
                input=wizard_input,
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            self.assertIn("Setup complete!", res.stdout)
            # Secrets must never be echoed back to stdout
            self.assertNotIn("super-secret-setup-token", res.stdout)
            self.assertNotIn("tvly-test-secret", res.stdout)

            # Verify every required runtime directory exists
            for rel in EXPECTED_DIRS:
                self.assertTrue((self.profile / rel).is_dir(), f"Missing directory: {rel}")

            # Verify starter files & permissions
            self.assertTrue((self.profile / "config.yaml").is_file())
            self.assertTrue((self.profile / ".env").is_file())
            self.assertTrue((self.profile / "memories" / "MEMORY.md").is_file())
            self.assertTrue((self.profile / "memories" / "USER.md").is_file())
            self.assertTrue((self.profile / "cron" / "jobs.json").is_file())
            self.assertTrue((self.profile / "state.db").is_file())

            if os.name == "posix":
                env_mode = stat.S_IMODE((self.profile / ".env").stat().st_mode)
                self.assertEqual(env_mode, 0o600)

            env_text = (self.profile / ".env").read_text(encoding="utf-8")
            self.assertIn("HERMES_API_KEY=super-secret-setup-token", env_text)
            self.assertIn("TAVILY_API_KEY=tvly-test-secret", env_text)

            cfg_text = (self.profile / "config.yaml").read_text(encoding="utf-8")
            self.assertIn("provider: custom", cfg_text)
            self.assertIn("default: setup-fixture-model", cfg_text)
            self.assertIn("mode: inline", cfg_text)
            self.assertIn("search_backend: tavily", cfg_text)

            # Doctor now reports ready (exit 0) and never prints secret values
            after = subprocess.run(
                [str(BINARY), "doctor", "--json"],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(after.returncode, 0, after.stdout + after.stderr)
            after_json = json.loads(after.stdout)
            self.assertTrue(after_json["ready"])
            self.assertNotIn("super-secret-setup-token", after.stdout)

            # A real chat turn works immediately using ONLY the setup-generated profile!
            chat = subprocess.run(
                [str(BINARY), "chat", "--oneshot", "-q", "Hi"],
                env=env,
                cwd=str(self.profile),
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(chat.returncode, 0, chat.stderr)
            self.assertIn("Setup wizard verified!", chat.stdout)

            # Re-running setup preserves existing memory notes without clobbering
            mem_file = self.profile / "memories" / "MEMORY.md"
            mem_file.write_text("Keep this note\n", encoding="utf-8")
            rerun = subprocess.run(
                [
                    str(BINARY),
                    "setup",
                    "--non-interactive",
                    "--provider",
                    "custom",
                    "--base-url",
                    f"{llm.base_url}/v1",
                    "--model",
                    "updated-model",
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(rerun.returncode, 0, rerun.stderr)
            self.assertEqual(mem_file.read_text(encoding="utf-8"), "Keep this note\n")

            # `hsr model` switches provider:model cleanly
            sw = subprocess.run(
                [str(BINARY), "model", "deepseek:deepseek-reasoner"],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(sw.returncode, 0, sw.stderr)
            cfg_after = (self.profile / "config.yaml").read_text(encoding="utf-8")
            self.assertIn("provider: deepseek", cfg_after)
            self.assertIn("default: deepseek-reasoner", cfg_after)
        finally:
            llm.stop()


if __name__ == "__main__":
    unittest.main()
