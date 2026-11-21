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
        scenario = {
            "test_cancel_interrupts_pending_provider_request": "stall",
            "test_incomplete_tool_stream_stops_before_dispatch": "partial_tool",
            "test_retries_after_tool_result_without_repeating_tool_call": "tool_retry",
            "test_retries_rate_limit_after_tool_result": "tool_rate_limit",
            "test_fallback_receives_existing_tool_result": "fallback_after_tool",
            "test_legacy_fallback_does_not_inherit_primary_key": "fallback_auth",
            "test_partial_text_prevents_fallback": "partial_text",
            "test_retries_disconnected_request_before_streaming": "disconnect_once",
            "test_iteration_budget_requests_summary_without_dispatching_more_tools": "iteration_limit",
            "test_rejects_tool_call_in_iteration_summary": "iteration_summary_tool",
        }.get(self._testMethodName, "tool")
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

    def chat_with_profile(self, extra_config="", dotenv="", args=()):
        home = self.workdir / "hermes"
        home.mkdir()
        (home / "config.yaml").write_text(
            f"model:\n  default: primary\n  provider: custom\n  base_url: {self.base}/v1\n"
            + extra_config,
            encoding="utf-8",
        )
        if dotenv:
            (home / ".env").write_text(dotenv, encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home), "HERMES_API_KEY": "fixture"}
        result = subprocess.run(
            [str(BINARY), "chat", "-q", "read the fixture", *args], cwd=self.workdir,
            env=env, capture_output=True, text=True, timeout=15,
        )
        with urlopen(self.base + "/requests", timeout=5) as response:
            return result, json.load(response)

    def test_fallback_receives_existing_tool_result(self):
        result, requests = self.chat_with_profile(
            "agent:\n  api_max_retries: 1\n"
            "fallback_providers:\n"
            "  - provider: unsupported\n    model: skipped\n"
            f"  - provider: openai\n    model: backup\n    base_url: {self.base}/v1\n    key_env: BACKUP_KEY\n"
            f"fallback_model:\n  provider: custom\n  model: legacy\n  base_url: {self.base}/v1\n",
            dotenv="BACKUP_KEY=backup-fixture\n",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Ответ модели", result.stdout)
        self.assertIn('Model fallback: "backup"', result.stderr)
        self.assertEqual([request["model"] for request in requests], ["primary", "primary", "backup"])
        primary = requests[1]["messages"]
        backup = requests[2]["messages"]
        self.assertEqual(primary[1:], backup[1:])
        primary_prefix, _, primary_route = primary[0]["content"].rpartition("\n\n")
        backup_prefix, _, backup_route = backup[0]["content"].rpartition("\n\n")
        self.assertTrue(primary_prefix)
        self.assertEqual(primary_prefix, backup_prefix)
        self.assertEqual(primary_route, "Model: primary\nProvider: custom\nPlatform: cli")
        self.assertEqual(backup_route, "Model: backup\nProvider: openai\nPlatform: cli")
        tool_result = requests[2]["messages"][-1]
        self.assertEqual(tool_result["tool_call_id"], "fixture-call")
        self.assertEqual(json.loads(tool_result["content"])["content"], "1|fixture content")

    def test_legacy_fallback_does_not_inherit_primary_key(self):
        result, requests = self.chat_with_profile(
            f"fallback_model:\n  provider: custom\n  model: backup\n  base_url: {self.base}/v1\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([request["model"] for request in requests], ["primary", "backup"])

    def test_partial_text_prevents_fallback(self):
        result, requests = self.chat_with_profile(
            f"fallback_model:\n  provider: custom\n  model: backup\n  base_url: {self.base}/v1\n"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Ответ модели", result.stdout)
        self.assertEqual([request["model"] for request in requests], ["primary"])

    def test_retries_disconnected_request_before_streaming(self):
        result, requests = self.chat_with_profile()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Ответ модели", result.stdout)
        self.assertEqual(len(requests), 2)
        self.assertEqual(requests[0], requests[1])

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
        system, user = requests[0]["messages"]
        self.assertEqual(system["role"], "system")
        identity, tool_guidance, platform_hint, route = system["content"].split("\n\n")
        self.assertTrue(identity.startswith("You are Hermes Agent, built by Nous Research."))
        self.assertIn("read_file", tool_guidance)
        self.assertIn("plain terminal", platform_hint)
        self.assertEqual(route, "Model: fixture\nProvider: custom\nPlatform: cli")
        self.assertEqual(user, {"role": "user", "content": "Прочитай fixture.txt"})
        self.assertEqual(requests[1]["messages"][:2], [system, user])
        self.assertEqual(result.stdout, "Ответ модели\n")
        self.assertEqual(result.stderr.splitlines(), [
            'Tool started: "read_file"', 'Tool completed: "read_file"',
        ])
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

    def test_config_set_updates_selected_profile_and_preserves_empty_override(self):
        home = self.workdir / "hermes"
        profile = home / "profiles" / "coder"
        profile.mkdir(parents=True)
        root_config = home / "config.yaml"
        root_content = "model: other-profile\n"
        root_config.write_text(root_content, encoding="utf-8")
        config = profile / "config.yaml"
        config.write_text("model: initial\nplugin_extra:\n  keep_me: retained\n", encoding="utf-8")
        dotenv = profile / ".env"
        dotenv_content = "HERMES_API_KEY=fixture\n"
        dotenv.write_text(dotenv_content, encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home)}

        def hermes(*args):
            return subprocess.run([str(BINARY), "-p", "coder", *args], cwd=self.workdir,
                                  env=env, capture_output=True, text=True, timeout=15)

        for key, value in [("model.base_url", self.base + "/v1"), ("model", "")]:
            result = hermes("config", "set", key, value)
            self.assertEqual(result.returncode, 0, result.stderr)
        empty = hermes("chat", "-q", "test")
        self.assertEqual(empty.returncode, 1)
        self.assertIn("model name is required", empty.stderr)
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(json.load(response), [])

        override = hermes("chat", "-q", "test", "--model", "explicit")
        self.assertEqual(override.returncode, 0, override.stderr)
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(json.load(response)[0]["model"], "explicit")
        self.assertIn("keep_me: retained", config.read_text(encoding="utf-8"))
        self.assertEqual(root_config.read_text(encoding="utf-8"), root_content)
        self.assertEqual(dotenv.read_text(encoding="utf-8"), dotenv_content)

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
        self.assertNotIn("Tool started:", result.stderr)
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
        self.assertEqual([item["role"] for item in requests[2]["messages"]], ["system", "user", "assistant", "tool"])
        self.assertEqual(requests[2]["messages"][-1]["tool_call_id"], "fixture-call")
        self.assertEqual(result.stderr.count('Tool started: "read_file"'), 1)

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

    def test_iteration_budget_requests_summary_without_dispatching_more_tools(self):
        result, requests = self.chat_with_profile(
            "agent:\n  max_turns: 3\n", args=("--max-turns", "2")
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "Ответ модели\n")
        self.assertEqual(result.stderr.splitlines(), [
            'Tool started: "read_file"', 'Tool completed: "read_file"',
            'Tool started: "read_file"', 'Tool completed: "read_file"',
            "Iteration limit reached (2). Requesting summary.",
        ])
        self.assertEqual(len(requests), 3)
        second = requests[1]["messages"]
        summary = requests[2]["messages"]
        self.assertEqual(summary[:len(second)], second)
        self.assertEqual([item["role"] for item in summary],
                         ["system", "user", "assistant", "tool", "assistant", "tool", "user"])
        self.assertEqual(summary[-2]["tool_call_id"], "iteration-2")
        self.assertIn("summarizing what you've found", summary[-1]["content"])
        self.assertEqual(requests[0]["tools"], requests[2]["tools"])

    def test_rejects_tool_call_in_iteration_summary(self):
        result, requests = self.chat_with_profile("agent:\n  max_turns: 1\n")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr.count('Tool started: "read_file"'), 1)
        self.assertIn("Iteration limit reached (1)", result.stderr)
        self.assertEqual(len(requests), 2)

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
        self.assertEqual(process.returncode, 130)
        self.assertIn("interrupted", stderr)
        self.assertNotIn("Ответ модели", stdout)


if __name__ == "__main__":
    unittest.main()
