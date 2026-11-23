"""End-to-end tests for the supported Rust CLI extension interfaces.

Covers:
- versioned NDJSON event streaming (`hermes chat --events-json`) and Python SDK wrapper
- direct execution and execution through `hermes owner`
- cancellation (SIGINT) for both in-flight model turns and custom Python MCP tools
- profile and environment isolation (SDK profiles, MCP plugin env, execute_code env)
- missing interpreter / missing binary error reporting
- Markdown skills compatibility via the SDK
"""

from __future__ import annotations

import json
import os
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parent.parent
BINARY = Path(os.environ.get("HERMES_TEST_BINARY", str(ROOT / "target/debug/hermes")))
PLUGIN_SCRIPT = ROOT / "examples/extensions/python_mcp_plugin.py"

sys.path.insert(0, str(ROOT / "examples/extensions"))
import hermes_sdk  # noqa: E402


class ExtensionsInterfaceTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.workdir = Path(directory.name)
        self.home = self.workdir / "profile"
        self.home.mkdir(mode=0o700)
        (self.workdir / "fixture.txt").write_text("fixture content\n", encoding="utf-8")

    def stop_process(self, process):
        if process.poll() is None:
            process.terminate()
        try:
            return process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            return process.communicate(timeout=5)

    def start_model(self, scenario):
        server = subprocess.Popen(
            [sys.executable, str(ROOT / "compat/model_server.py"), "--scenario", scenario],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self.stop_process, server)
        base = server.stdout.readline().strip()
        self.assertTrue(base, "model server did not print its address")
        return base

    def start_owner(self, home):
        owner = subprocess.Popen(
            [str(BINARY), "owner"],
            cwd=self.workdir,
            env={"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home)},
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self.stop_process, owner)
        sock = home / "rust-owner" / "owner.sock"
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and owner.poll() is None:
            if sock.exists() and stat.S_IMODE(sock.stat().st_mode) == 0o600:
                with socket.socket(socket.AF_UNIX) as conn:
                    try:
                        conn.connect(str(sock))
                    except (FileNotFoundError, ConnectionRefusedError):
                        pass
                    else:
                        return owner
            time.sleep(0.02)
        _, stderr = self.stop_process(owner)
        self.fail(f"local owner did not start: {stderr}")

    def test_sdk_streams_versioned_events_and_resumes_session(self):
        base = self.start_model("tool")
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="fixture",
        )
        seen = []
        first = client.run_turn("Read fixture.txt", save_session=True, on_event=seen.append)
        self.assertEqual(first.status, "completed")
        self.assertEqual(first.exit_code, 0)
        self.assertEqual(first.text, "Ответ модели")
        self.assertIsNotNone(first.session_id)
        self.assertTrue(all(ev.version == 1 for ev in seen))
        names = [ev.event for ev in seen]
        self.assertIn("tool_started", names)
        self.assertIn("tool_finished", names)
        self.assertIn("text", names)
        self.assertIn("usage", names)
        self.assertIn("session", names)
        self.assertEqual(names[-1], "done")
        started = next(ev for ev in seen if ev.event == "tool_started")
        finished = next(ev for ev in seen if ev.event == "tool_finished")
        self.assertEqual(started.data["name"], "read_file")
        self.assertEqual(finished.data, {"name": "read_file", "succeeded": True})

        # Second turn continues the same session through the SDK.
        second = client.run_turn("Follow-up question", resume=first.session_id)
        self.assertEqual(second.status, "completed")
        self.assertEqual(second.session_id, first.session_id)
        with sqlite3.connect(self.home / "state.db") as db:
            count = db.execute(
                "SELECT count(*) FROM messages WHERE session_id = ?", (first.session_id,)
            ).fetchone()[0]
        self.assertEqual(count, 6)

    @unittest.skipUnless(os.name == "posix", "requires Unix sockets and signals")
    def test_sdk_works_through_local_owner_and_cancels_cleanly(self):
        base = self.start_model("stall_on_resume")
        self.start_owner(self.home)
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="fixture",
        )
        first = client.run_turn("Initial turn", save_session=True)
        self.assertEqual(first.status, "completed")
        self.assertIsNotNone(first.session_id)

        # Start a held turn through the owner, wait until the model receives it, then cancel via SDK.
        cancel_event = threading.Event()
        result_box = {}

        def _run():
            result_box["result"] = client.run_turn(
                "hold resume",
                resume=first.session_id,
                cancel_event=cancel_event,
                raise_on_error=False,
            )

        thread = threading.Thread(target=_run)
        thread.start()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            with urlopen(base + "/requests", timeout=2) as response:
                if len(json.load(response)) >= 3:
                    break
            time.sleep(0.02)
        else:
            self.fail("owner did not dispatch the resumed turn to the model server")

        cancel_event.set()
        thread.join(timeout=10)
        self.assertFalse(thread.is_alive(), "cancelled SDK turn did not exit")
        cancelled = result_box["result"]
        self.assertEqual(cancelled.status, "cancelled")
        self.assertEqual(cancelled.exit_code, 130)
        names = [ev.event for ev in cancelled.events]
        self.assertIn("cancelled", names)
        self.assertEqual(names[-1], "done")

        # Lease must be released and no partial rows saved.
        deadline = time.monotonic() + 5
        with sqlite3.connect(self.home / "state.db") as db:
            while time.monotonic() < deadline:
                if db.execute("SELECT count(*) FROM session_turn_leases").fetchone() == (0,):
                    break
                time.sleep(0.02)
            else:
                self.fail("owner did not release session lease after SDK cancellation")
            self.assertEqual(db.execute("SELECT count(*) FROM messages").fetchone(), (4,))

    def test_sdk_enforces_profile_isolation(self):
        base = self.start_model("text")
        profile_a = self.workdir / "profile-a"
        profile_b = self.workdir / "profile-b"
        profile_a.mkdir(mode=0o700)
        profile_b.mkdir(mode=0o700)

        client_a = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=profile_a,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="key-a",
        )
        saved = client_a.run_turn("Remember profile A", save_session=True)
        self.assertIsNotNone(saved.session_id)

        # Profile B cannot resume Profile A's session, and ambient caller keys do not leak into it.
        prev_api = os.environ.get("HERMES_API_KEY")
        prev_home = os.environ.get("HERMES_HOME")
        os.environ["HERMES_API_KEY"] = "leaked-caller-key"
        os.environ["HERMES_HOME"] = str(profile_a)
        try:
            client_b = hermes_sdk.HermesClient(
                binary=BINARY,
                profile_home=profile_b,
                workdir=self.workdir,
                base_url=base + "/v1",
                model="fixture",
                api_key="key-b",
            )
            with self.assertRaises(hermes_sdk.HermesTurnError) as raised:
                client_b.run_turn("Steal session A", resume=saved.session_id)
            self.assertEqual(raised.exception.exit_code, 1)
            self.assertEqual(raised.exception.result.status, "error")
            self.assertTrue(raised.exception.result.error)

            # Without explicit api_key, ambient HERMES_API_KEY must not leak into `client_c`.
            client_c = hermes_sdk.HermesClient(
                binary=BINARY,
                profile_home=profile_b,
                workdir=self.workdir,
                base_url=base + "/v1",
                model="fixture",
            )
            env = client_c._build_env()
            self.assertNotIn("HERMES_API_KEY", env)
            self.assertEqual(env["HERMES_HOME"], str(profile_b))
        finally:
            if prev_api is None:
                os.environ.pop("HERMES_API_KEY", None)
            else:
                os.environ["HERMES_API_KEY"] = prev_api
            if prev_home is None:
                os.environ.pop("HERMES_HOME", None)
            else:
                os.environ["HERMES_HOME"] = prev_home

    def test_python_mcp_plugin_tools_resources_env_isolation_and_cancellation(self):
        log_path = self.workdir / "plugin.jsonl"
        (self.home / "config.yaml").write_text(
            json.dumps({
                "mcp_servers": {
                    "pyext": {
                        "command": sys.executable,
                        "args": ["-I", str(PLUGIN_SCRIPT)],
                        "env": {
                            "PLUGIN_TOKEN": "explicit-plugin-token",
                            "HERMES_PLUGIN_LOG": str(log_path),
                        },
                        "timeout": 10,
                        "connect_timeout": 10,
                    }
                }
            }),
            encoding="utf-8",
        )

        # 1. Verify tool execution (`env_audit`) and environment isolation.
        base = self.start_model("local_tools")
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="super-secret-model-key",
            extra_env={"HERMES_UNRELATED_SECRET": "must-not-reach-plugin"},
        )
        query = json.dumps({"tool": "mcp__pyext__env_audit", "arguments": {}})
        result = client.run_turn(query)
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.warnings, [])
        with urlopen(base + "/requests", timeout=2) as response:
            requests = json.load(response)
        tool_msg = next(m for m in requests[-1]["messages"] if m.get("role") == "tool")
        tool_payload = json.loads(tool_msg["content"])
        audit = json.loads(tool_payload["result"])
        self.assertEqual(
            audit,
            {
                "has_hermes_api_key": False,
                "has_hermes_home": False,
                "has_unrelated_secret": False,
                "plugin_token": "explicit-plugin-token",
            },
        )

        # 2. Verify custom tool (`word_stats`) and resource reading (`plugin://status`).
        for tool_name, arguments, expected_fragment in (
            ("mcp__pyext__word_stats", {"text": "one two\nthree"}, '"words": 3'),
            ("mcp__pyext__read_resource", {"uri": "plugin://status"}, "python-mcp-plugin ok"),
        ):
            r = client.run_turn(json.dumps({"tool": tool_name, "arguments": arguments}))
            self.assertEqual(r.status, "completed")
            with urlopen(base + "/requests", timeout=2) as response:
                reqs = json.load(response)
            msg = next(m for m in reversed(reqs[-1]["messages"]) if m.get("role") == "tool")
            payload = json.loads(msg["content"])
            self.assertIn(expected_fragment, payload["result"])

        # 3. Verify cancellation during `slow_task` stops the plugin process cleanly.
        if os.name == "posix":
            log_path.unlink(missing_ok=True)
            cancel_event = threading.Event()
            box = {}

            def _run_slow():
                box["result"] = client.run_turn(
                    json.dumps({"tool": "mcp__pyext__slow_task", "arguments": {}}),
                    cancel_event=cancel_event,
                    raise_on_error=False,
                )

            thread = threading.Thread(target=_run_slow)
            thread.start()
            deadline = time.monotonic() + 5
            plugin_pid = None
            while time.monotonic() < deadline:
                if log_path.exists():
                    rows = [json.loads(line) for line in log_path.read_text(encoding="utf-8").splitlines() if line]
                    for row in rows:
                        if "pid" in row:
                            plugin_pid = row["pid"]
                    if any(row.get("method") == "tools/call" for row in rows):
                        break
                time.sleep(0.02)
            else:
                self.fail("python MCP plugin did not receive slow_task call")

            cancel_event.set()
            thread.join(timeout=10)
            self.assertFalse(thread.is_alive())
            self.assertEqual(box["result"].status, "cancelled")
            self.assertEqual(box["result"].exit_code, 130)
            event_names = [ev.event for ev in box["result"].events]
            self.assertIn("cancelled", event_names)
            self.assertEqual(event_names[-1], "done")
            self.assertIsNotNone(plugin_pid)
            # Confirm the plugin process exited (or is only a zombie awaiting PID 1) and did not linger.
            def _is_alive(pid: int) -> bool:
                stat_path = Path(f"/proc/{pid}/stat")
                if stat_path.exists():
                    try:
                        return ") Z " not in stat_path.read_text(encoding="utf-8")
                    except OSError:
                        return False
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    return False
                return True

            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if not _is_alive(plugin_pid):
                    break
                time.sleep(0.02)
            else:
                self.fail(f"python MCP plugin process {plugin_pid} survived turn cancellation")

    def test_missing_interpreter_and_missing_binary_errors(self):
        # 1. Missing `hermes` binary raises a dedicated SDK exception.
        with self.assertRaises(hermes_sdk.HermesBinaryNotFoundError):
            hermes_sdk.HermesClient(binary="/nonexistent/hermes-binary")

        # 2. Missing Python interpreter for an MCP plugin emits a structured warning,
        # while the turn still completes using built-in tools.
        (self.home / "config.yaml").write_text(
            json.dumps({
                "mcp_servers": {
                    "missing_py": {
                        "command": "/nonexistent/python3-missing",
                        "args": [str(PLUGIN_SCRIPT)],
                    }
                }
            }),
            encoding="utf-8",
        )
        base = self.start_model("tool")
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="fixture",
        )
        result = client.run_turn("Read fixture.txt")
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.exit_code, 0)
        self.assertTrue(
            any("missing_py" in warning for warning in result.warnings),
            f"expected warning for missing interpreter, got {result.warnings!r}",
        )

        # 3. Missing `python3` for `execute_code` returns a structured tool error without crashing.
        if os.name == "posix":
            code_home = self.workdir / "code-profile"
            code_home.mkdir(mode=0o700)
            (code_home / "config.yaml").write_text(
                "terminal:\n  auto_source_bashrc: false\n", encoding="utf-8"
            )
            empty_bin = self.workdir / "empty-bin"
            empty_bin.mkdir()
            code_base = self.start_model("code")
            code_client = hermes_sdk.HermesClient(
                binary=BINARY,
                profile_home=code_home,
                workdir=self.workdir,
                base_url=code_base + "/v1",
                model="fixture",
                api_key="fixture",
                yolo=True,
                extra_env={"PATH": str(empty_bin)},
            )
            code_result = code_client.run_turn(json.dumps({"cells": ["print(1 + 1)"]}))
            self.assertEqual(code_result.status, "completed")
            with urlopen(code_base + "/requests", timeout=2) as response:
                reqs = json.load(response)
            tool_msg = next(m for m in reqs[-1]["messages"] if m.get("role") == "tool")
            payload = json.loads(tool_msg["content"])
            self.assertEqual(payload["status"], "error")
            self.assertIn("No python3 was found", payload["error"])

    def test_events_json_setup_errors_and_missing_query(self):
        # `--events-json` without `-q` emits structured JSON error + done and exits 1.
        proc = subprocess.run(
            [str(BINARY), "chat", "--events-json"],
            cwd=self.workdir,
            env={"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home)},
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(proc.returncode, 1)
        events = [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]
        self.assertEqual([e["event"] for e in events], ["error", "done"])
        self.assertIn("--query", events[0]["message"])
        self.assertEqual(events[1]["exit_code"], 1)
        self.assertEqual(events[1]["status"], "error")

        # Missing API key / base_url through SDK raises HermesTurnError with parsed error event.
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
        )
        with self.assertRaises(hermes_sdk.HermesTurnError) as raised:
            client.run_turn("hello")
        self.assertEqual(raised.exception.exit_code, 1)
        self.assertIn("base URL", str(raised.exception))

    def test_markdown_skill_works_through_sdk(self):
        skill_dir = self.home / "skills" / "demo"
        skill_dir.mkdir(parents=True)
        (skill_dir / "SKILL.md").write_text(
            "---\nname: demo\ndescription: Example Markdown skill\n---\n\nFollow the demo checklist.\n",
            encoding="utf-8",
        )
        base = self.start_model("skill")
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="fixture",
        )
        result = client.run_turn("demo")
        self.assertEqual(result.status, "completed")
        self.assertTrue(
            any(ev.event == "tool_finished" and ev.data.get("name") == "skill_view" and ev.data.get("succeeded")
                for ev in result.events)
        )
        with urlopen(base + "/requests", timeout=2) as response:
            reqs = json.load(response)
        tool_msg = next(m for m in reqs[-1]["messages"] if m.get("role") == "tool")
        self.assertIn("Follow the demo checklist.", tool_msg["content"])

    def test_events_json_iteration_limit_and_protocol_version_check(self):
        base = self.start_model("iteration_limit")
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            base_url=base + "/v1",
            model="fixture",
            api_key="fixture",
            max_turns=2,
        )
        result = client.run_turn("Keep reading", raise_on_error=False)
        self.assertEqual(result.status, "iteration_limit")
        self.assertEqual(result.exit_code, 1)
        limit_events = [ev for ev in result.events if ev.event == "iteration_limit"]
        self.assertEqual(len(limit_events), 1)
        self.assertEqual(limit_events[0].data["limit"], 2)
        self.assertEqual(result.events[-1].event, "done")
        self.assertEqual(result.events[-1].data, {"exit_code": 1, "status": "iteration_limit"})

        # Incompatible protocol version is rejected by the SDK.
        with self.assertRaises(hermes_sdk.HermesProtocolError):
            hermes_sdk._parse_event_line('{"version": 99, "event": "text", "text": "x"}')
        with self.assertRaises(hermes_sdk.HermesProtocolError):
            hermes_sdk._parse_event_line("not json")

    def test_events_json_fallback_and_startup_warning(self):
        base = self.start_model("fallback_after_tool")
        (self.home / "config.yaml").write_text(
            json.dumps({
                "security": {"redact_secrets": False},
                "agent": {"api_max_retries": 1},
                "model": {
                    "provider": "custom",
                    "default": "primary",
                    "base_url": base + "/v1",
                },
                "fallback_providers": [{
                    "provider": "custom",
                    "model": "backup",
                    "base_url": base + "/v1",
                    "key_env": "BACKUP_API_KEY",
                }],
            }),
            encoding="utf-8",
        )
        client = hermes_sdk.HermesClient(
            binary=BINARY,
            profile_home=self.home,
            workdir=self.workdir,
            api_key="primary-fixture",
            extra_env={"BACKUP_API_KEY": "backup-fixture"},
        )
        result = client.run_turn("Read fixture.txt")
        self.assertEqual(result.status, "completed")
        self.assertTrue(
            any("redact_secrets" in w for w in result.warnings),
            f"expected startup warning for redact_secrets, got {result.warnings!r}",
        )
        fallbacks = [ev for ev in result.events if ev.event == "fallback"]
        self.assertEqual(len(fallbacks), 1)
        self.assertEqual(fallbacks[0].data["model"], "backup")


if __name__ == "__main__":
    unittest.main()
