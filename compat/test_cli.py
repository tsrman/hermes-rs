import json
import os
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from urllib.request import urlopen


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "hermes"


class CliTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.workdir = Path(self.directory.name)
        (self.workdir / "fixture.txt").write_text("fixture content", encoding="utf-8")
        scenario = {"test_cancel_interrupts_pending_provider_request": "stall", "test_incomplete_tool_stream_stops_before_dispatch": "partial_tool", "test_retries_after_tool_result_without_repeating_tool_call": "tool_retry", "test_retries_rate_limit_after_tool_result": "tool_rate_limit"}.get(self._testMethodName, "tool")
        self.server = subprocess.Popen(
            ["python3", str(ROOT / "compat" / "model_server.py"), "--scenario", scenario],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self.stop_server)
        self.base = self.server.stdout.readline().strip()
        self.assertTrue(self.base, self.server.stderr.read() if self.server.poll() is not None else "")

    def stop_server(self):
        self.server.terminate()
        self.server.communicate(timeout=5)

    def test_reads_file_and_sends_result_before_answer(self):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_API_KEY": "fixture"}
        result = subprocess.run(
            [str(BINARY), "chat", "--oneshot", "-q", "Прочитай fixture.txt", "--base-url", self.base + "/v1", "--model", "fixture"],
            cwd=self.workdir,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Ответ модели", result.stdout)
        with urlopen(self.base + "/requests", timeout=5) as response:
            requests = json.load(response)
        self.assertEqual(len(requests), 2)
        self.assertEqual(requests[0]["messages"][-1]["content"], "Прочитай fixture.txt")
        tool = requests[1]["messages"][-1]
        self.assertEqual(tool["role"], "tool")
        self.assertEqual(tool["tool_call_id"], "fixture-call")
        self.assertEqual(json.loads(tool["content"]), {
            "content": "1|fixture content", "total_lines": 1, "file_size": 15,
            "truncated": False, "is_binary": False, "is_image": False, "not_found": False,
        })

    def test_loads_named_profile_configuration_and_secret(self):
        home = self.workdir / "hermes"
        profile = home / "profiles" / "coder"
        profile.mkdir(parents=True)
        (profile / "config.yaml").write_text(
            f"model:\n  default: profile-model\n  provider: custom\n  base_url: {self.base}/v1\n",
            encoding="utf-8",
        )
        (profile / ".env").write_text("HERMES_API_KEY='profile-secret'\n", encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home)}
        result = subprocess.run(
            [str(BINARY), "-p", "coder", "chat", "-q", "test"], cwd=self.workdir,
            env=env, capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        with urlopen(self.base + "/requests", timeout=5) as response:
            requests = json.load(response)
        self.assertEqual(requests[0]["model"], "profile-model")

    def test_missing_profile_fails_instead_of_using_default(self):
        home = self.workdir / "hermes"
        home.mkdir()
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home), "HERMES_API_KEY": "fixture"}
        result = subprocess.run(
            [str(BINARY), "-p", "missing", "chat", "-q", "test", "--base-url", self.base + "/v1", "--model", "fixture"],
            cwd=self.workdir, env=env, capture_output=True, text=True, timeout=15,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("profile", result.stderr.lower())
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(json.load(response), [])

    def test_openai_key_is_only_used_for_its_paired_endpoint(self):
        home = self.workdir / "hermes"
        home.mkdir()
        (home / "config.yaml").write_text(
            f"model:\n  default: fixture\n  provider: custom\n  base_url: {self.base}/v1\n", encoding="utf-8",
        )
        (home / ".env").write_text("OPENAI_API_KEY=secret\nOPENAI_BASE_URL=https://unrelated.example/v1\n", encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home)}
        result = subprocess.run([str(BINARY), "chat", "-q", "test"], cwd=self.workdir,
                                env=env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(json.load(response), [])
        (home / ".env").write_text(f"OPENAI_API_KEY=secret\nOPENAI_BASE_URL={self.base}/v1\n", encoding="utf-8")
        result = subprocess.run([str(BINARY), "chat", "-q", "test"], cwd=self.workdir,
                                env=env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_active_profile_and_command_line_model_override(self):
        home = self.workdir / "hermes"
        profile = home / "profiles" / "active"
        profile.mkdir(parents=True)
        (home / "active_profile").write_text("active\n", encoding="utf-8")
        (profile / "config.yaml").write_text(
            f"model:\n  default: profile-model\n  provider: custom\n  base_url: {self.base}/v1\n  extra: preserved\n", encoding="utf-8",
        )
        (profile / ".env").write_text("HERMES_API_KEY=fixture\n", encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home)}
        result = subprocess.run([str(BINARY), "chat", "-q", "test", "--model", "override-model"],
                                cwd=self.workdir, env=env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        with urlopen(self.base + "/requests", timeout=5) as response:
            requests = json.load(response)
        self.assertEqual(requests[0]["model"], "override-model")

    def test_deleted_profile_is_not_recreated(self):
        home = self.workdir / "hermes"
        profile = home / "profiles" / "coder"
        profile.mkdir(parents=True)
        (profile / "config.yaml").write_text("model: fixture\n", encoding="utf-8")
        deleted = home / "profiles" / ".deleted"
        deleted.mkdir()
        (deleted / "coder").write_text("deleted", encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home), "HERMES_API_KEY": "fixture"}
        result = subprocess.run([str(BINARY), "-p", "coder", "chat", "-q", "test", "--base-url", self.base + "/v1"],
                                cwd=self.workdir, env=env, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("profile", result.stderr.lower())
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(json.load(response), [])

    def test_incomplete_tool_stream_stops_before_dispatch(self):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_API_KEY": "fixture"}
        result = subprocess.run(
            [str(BINARY), "chat", "-q", "test", "--base-url", self.base + "/v1", "--model", "fixture"],
            cwd=self.workdir, env=env, capture_output=True, text=True, timeout=15,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("Ответ модели", result.stdout)
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(len(json.load(response)), 1)

    def test_retries_after_tool_result_without_repeating_tool_call(self):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_API_KEY": "fixture"}
        result = subprocess.run(
            [str(BINARY), "chat", "-q", "test", "--base-url", self.base + "/v1", "--model", "fixture"],
            cwd=self.workdir, env=env, capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        with urlopen(self.base + "/requests", timeout=5) as response:
            requests = json.load(response)
        self.assertEqual(len(requests), 3)
        self.assertEqual(requests[1], requests[2])
        self.assertEqual([item["role"] for item in requests[2]["messages"]], ["user", "assistant", "tool"])
        self.assertEqual(requests[2]["messages"][-1]["tool_call_id"], "fixture-call")

    def test_retries_rate_limit_after_tool_result(self):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_API_KEY": "fixture"}
        result = subprocess.run(
            [str(BINARY), "chat", "-q", "test", "--base-url", self.base + "/v1", "--model", "fixture"],
            cwd=self.workdir, env=env, capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        with urlopen(self.base + "/requests", timeout=5) as response:
            requests = json.load(response)
        self.assertEqual(len(requests), 3)
        self.assertEqual(requests[1], requests[2])
        self.assertEqual(requests[2]["messages"][-1]["role"], "tool")

    @unittest.skipUnless(os.name == "posix", "requires SIGINT")
    def test_cancel_interrupts_pending_provider_request(self):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_API_KEY": "fixture"}
        process = subprocess.Popen(
            [str(BINARY), "chat", "-q", "test", "--base-url", self.base + "/v1", "--model", "fixture"],
            cwd=self.workdir, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(lambda: process.kill() if process.poll() is None else None)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            with urlopen(self.base + "/requests", timeout=5) as response:
                if json.load(response):
                    break
            time.sleep(0.05)
        else:
            self.fail("provider did not receive request")
        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("interrupted", stderr)
        self.assertNotIn("Ответ модели", stdout)


if __name__ == "__main__":
    unittest.main()
