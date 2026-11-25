import json
import os
import select
import sqlite3
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from urllib.request import urlopen

if sys.platform in ("linux", "darwin"):
    import fcntl
    import pty
    import termios


ROOT = Path(__file__).resolve().parent.parent
BINARY = Path(os.environ.get("HERMES_TEST_BINARY", ROOT / "target/debug/hsr")).resolve()


# Flagged by the dangerous-command detector (a recursive delete) but harmless: the path does not exist.
FLAGGED = "rm -rf ./no-such-dir-for-approval"

class ToolTestCase(unittest.TestCase):
    scenario = "local_tools"

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.profile = self.root / "profile"
        self.profile.mkdir()
        self.server = subprocess.Popen(
            ["python3", str(ROOT / "compat/model_server.py"), "--scenario", self.scenario],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(self.stop, self.server)
        self.url = self.server.stdout.readline().strip()
        self.env = {
            **os.environ, "HERMES_HOME": str(self.profile), "HERMES_API_KEY": "fixture",
        }
        self.env.pop("TERMINAL_ENV", None)
        self.config = (
            f"model: {{default: fixture, provider: custom, base_url: '{self.url}/v1'}}\n"
            "approvals:\n  timeout: 2\n"
        )
        (self.profile / "config.yaml").write_text(self.config, encoding="utf-8")

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.kill()
        process.communicate(timeout=5)

    def command(self, path="notes.txt", content="updated text", read=False, oneshot=False, tool=None, arguments=None):
        fixture = {"path": path, "content": content, "read": read}
        if tool is not None:
            fixture.update(tool=tool, arguments=arguments)
        args = [str(BINARY), "chat", "-q", json.dumps(fixture)]
        return [*args, "--oneshot"] if oneshot else args

    def interactive(self, **fixture):
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            process = subprocess.Popen(
                self.command(**fixture), cwd=self.root, env=self.env,
                stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        self.addCleanup(self.stop, process)
        return process, master

    def wait_for_prompt(self, process, master, marker=(b"[y/N]", b"[y/s/a/N]"), transcript=None):
        markers = marker if isinstance(marker, tuple) else (marker,)
        transcript = bytearray() if transcript is None else transcript
        deadline = time.monotonic() + 20
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    transcript.extend(os.read(master, 16384))
                except OSError:
                    break
            if any(m in transcript for m in markers):
                return transcript.decode(errors="replace")
        details = ""
        if process.poll() is not None:
            _, stderr = process.communicate(timeout=1)
            details = f"; exit status {process.returncode}; stderr was {stderr!r}"
        self.fail(f"approval prompt missing; terminal output was {transcript!r}{details}")

    @staticmethod
    def finish_terminal(process, master):
        # macOS waits for the controlling terminal output to drain before
        # the session leader exits. Keep reading echoed input until then.
        deadline = time.monotonic() + 20
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    if not os.read(master, 16384):
                        break
                except OSError:
                    break
        return process.communicate(timeout=max(0.1, deadline - time.monotonic()))

    def last_tool_result(self):
        with urlopen(self.url + "/requests", timeout=4) as response:
            requests = json.load(response)
        return json.loads(requests[-1]["messages"][-1]["content"]), requests

    def unattended(self, tool, arguments):
        run = subprocess.run(
            self.command(tool=tool, arguments=arguments), cwd=self.root, env=self.env,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        return self.last_tool_result()

    def start_owner(self):
        owner = subprocess.Popen(
            [str(BINARY), "owner"], cwd=self.root, env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(self.stop, owner)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if (self.profile / "rust-owner/owner.sock").exists():
                return owner
            time.sleep(0.02)
        self.fail("local owner did not open its socket")


@unittest.skipUnless(sys.platform == "linux", "requires Linux file approvals")
class FileToolTest(ToolTestCase):
    def test_write_after_full_read_and_explicit_approval(self):
        target = self.root / "notes.txt"
        target.write_text("original\n", encoding="utf-8")
        process, tty = self.interactive(read=True)
        prompt = self.wait_for_prompt(process, tty)
        self.assertIn("notes.txt", prompt)
        self.assertIn("updated text", prompt)
        self.assertEqual(target.read_text(), "original\n")
        os.write(tty, b"y\n")
        output, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual(output, "Ответ модели\n")
        self.assertEqual(target.read_text(), "updated text")
        result, requests = self.last_tool_result()
        self.assertTrue(result["verified"])
        self.assertIn("write_file", [tool["function"]["name"] for tool in requests[0]["tools"]])

    def test_denial_timeout_and_cancel_do_not_create_the_file(self):
        for mode in ("deny", "timeout", "cancel"):
            with self.subTest(mode=mode):
                process, tty = self.interactive(path=f"{mode}.txt")
                self.wait_for_prompt(process, tty)
                if mode == "deny":
                    os.write(tty, b"n\n")
                if mode == "cancel":
                    process.send_signal(signal.SIGINT)
                _, error = process.communicate(timeout=20)
                self.assertEqual(process.returncode, 130 if mode == "cancel" else 0, error)
                self.assertFalse((self.root / f"{mode}.txt").exists())
                if mode != "cancel":
                    result, _ = self.last_tool_result()
                    self.assertEqual(result["status"], "approval_" + ("denied" if mode == "deny" else "timeout"))

    def ask(self, questions, config=""):
        if config:
            (self.profile / "config.yaml").write_text(self.config + config, encoding="utf-8")
        process, tty = self.interactive(tool="clarify", arguments={"questions": questions})
        return process, tty

    def answer_clarify(self, process, tty, markers, replies):
        screen = bytearray()
        for marker, reply in zip(markers, replies):
            self.wait_for_prompt(process, tty, marker=marker, transcript=screen)
            screen.clear()
            os.write(tty, reply)
        return process.communicate(timeout=20)

    def test_clarify_asks_each_question_and_reports_the_answers(self):
        process, tty = self.ask([
            {"question": "Which database?", "choices": ["Postgres", "SQLite"]},
            {"question": "Which checks?", "choices": ["lint", "tests", "types"], "multi_select": True},
            {"question": "Anything else?"},
            {"question": "Skip me", "choices": ["x", "y"]},
        ])
        _, error = self.answer_clarify(process, tty, [b"own answer (Enter", b"own answer (Enter", b"to skip): ", b"own answer (Enter"],
                                       [b"1\n", b"2, 3,coverage\n", b"Use the staging cluster\n", b"\n"])
        self.assertEqual(process.returncode, 0, error)
        result, requests = self.last_tool_result()
        self.assertIn("clarify", [tool["function"]["name"] for tool in requests[0]["tools"]])
        self.assertEqual(result["outcome"], "submitted")
        answers = [(r["status"], r["user_response"]) for r in result["responses"]]
        self.assertEqual(answers, [("answered", "Postgres"), ("answered", ["tests", "types", "coverage"]),
                                   ("answered", "Use the staging cluster"), ("skipped", None)])
        self.assertEqual(result["responses"][0]["choices_offered"], ["Postgres", "SQLite"])

    def test_clarify_shows_the_recommendation_and_neutralises_escape_sequences(self):
        process, tty = self.ask([{"question": "Pick \u001b[31mred\u001b[0m", "choices": ["First", "Second"]}])
        screen = bytearray()
        prompt = self.wait_for_prompt(process, tty, marker=b"own answer (Enter", transcript=screen)
        self.assertIn("1) First (Recommended)", prompt)
        self.assertIn("2) Second", prompt)
        self.assertNotIn("\x1b[31m", prompt)
        os.write(tty, b"1 (Recommended)\n")
        process.communicate(timeout=20)
        result, _ = self.last_tool_result()
        # The label is presentation only: a typed answer that carries it is cleaned too.
        self.assertEqual(result["responses"][0]["user_response"], "1")

    def test_clarify_reports_timeout_and_cancellation(self):
        process, tty = self.ask([{"question": "Anyone there?"}], config="clarify:\n  timeout: 1\n")
        self.wait_for_prompt(process, tty, marker=b"(Enter to skip)")
        process.communicate(timeout=20)
        result, _ = self.last_tool_result()
        self.assertEqual(result["outcome"], "timed_out")
        self.assertEqual(result["notice"], "[user did not respond within 1m]")
        self.assertEqual(result["responses"][0]["status"], "unanswered")

        process, tty = self.ask([{"question": "Cancel me"}], config="")
        self.wait_for_prompt(process, tty, marker=b"(Enter to skip)")
        os.write(tty, b"\x04")  # end of input at the start of a line
        process.communicate(timeout=20)
        self.assertEqual(self.last_tool_result()[0]["outcome"], "cancelled")

    def test_clarify_is_unavailable_without_a_person_and_rejects_bad_questions(self):
        result, _ = self.unattended("clarify", {"questions": [{"question": "Hello?"}]})
        self.assertEqual(result, {"error": "Clarify tool is not available in this execution context."})
        result, _ = self.unattended("clarify", {"questions": []})
        self.assertIn("questions must be a non-empty array", result["error"])

    def test_unattended_write_is_hidden_and_refused(self):
        run = subprocess.run(
            self.command(), cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        result, requests = self.last_tool_result()
        self.assertNotIn("write_file", [tool["function"]["name"] for tool in requests[0]["tools"]])
        self.assertIn("error", result)
        self.assertFalse((self.root / "notes.txt").exists())
        process, _ = self.interactive(oneshot=True)
        _, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, stderr)
        result, requests = self.last_tool_result()
        self.assertEqual(result["status"], "approval_unavailable")
        self.assertNotIn("write_file", [tool["function"]["name"] for tool in requests[-1]["tools"]])
        self.assertFalse((self.root / "notes.txt").exists())

    def test_refuses_sensitive_paths_symlinks_and_blind_overwrites(self):
        original = self.root / "notes.txt"
        original.write_text("original\n", encoding="utf-8")
        (self.root / "linked.txt").symlink_to(original)
        (self.root / ".git").mkdir()
        for path, error in (
            ("notes.txt", "Read the entire current file"),
            ("linked.txt", "symbolic link"),
            ("profile/config.yaml", "protected"),
            (".git/config", "protected"),
            (".env.local", "protected"),
            ("../escaped.txt", "parent traversal"),
        ):
            with self.subTest(path=path):
                process, _ = self.interactive(path=path)
                _, stderr = process.communicate(timeout=20)
                self.assertEqual(process.returncode, 0, stderr)
                result, _ = self.last_tool_result()
                self.assertIn(error, result["error"])
        self.assertEqual(original.read_text(), "original\n")
        self.assertEqual((self.profile / "config.yaml").read_text(), self.config)
        self.assertFalse((self.root / ".git/config").exists())
        self.assertFalse((self.root / ".env.local").exists())

    def test_owner_forwards_approval_and_rejects_a_stale_overwrite(self):
        self.start_owner()
        target = self.root / "notes.txt"
        target.write_text("original\n", encoding="utf-8")
        process, tty = self.interactive(read=True)
        self.wait_for_prompt(process, tty)
        target.write_text("external edit\n", encoding="utf-8")
        os.write(tty, b"yes\n")
        _, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual(target.read_text(), "external edit\n")
        result, _ = self.last_tool_result()
        self.assertIn("changed", result["error"])
        process, tty = self.interactive(path="created.txt")
        self.wait_for_prompt(process, tty)
        os.write(tty, b"yes\n")
        _, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual((self.root / "created.txt").read_text(), "updated text")
        process, tty = self.interactive(path="cancelled.txt")
        self.wait_for_prompt(process, tty)
        process.send_signal(signal.SIGINT)
        _, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 130, error)
        self.assertFalse((self.root / "cancelled.txt").exists())

    def test_patch_requires_explicit_permission_and_a_unique_match(self):
        target = self.root / "notes.txt"
        target.write_text("keep before\nchange here\nkeep after\n", encoding="utf-8")
        process, tty = self.interactive(tool="patch", arguments={
            "path": "notes.txt", "old_string": "change here", "new_string": "changed",
        })
        prompt = self.wait_for_prompt(process, tty)
        self.assertIn("changed", prompt)
        self.assertIn("change here", target.read_text())
        os.write(tty, b"yes\n")
        _, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual(target.read_text(), "keep before\nchanged\nkeep after\n")
        patched, _ = self.last_tool_result()
        self.assertTrue(patched["success"])
        self.assertEqual(len(patched["files_modified"]), 1)
        self.assertTrue(patched["files_modified"][0].endswith("/notes.txt"))
        self.assertIn("+changed", patched["diff"])
        process, _ = self.interactive(tool="patch", arguments={
            "path": "notes.txt", "old_string": "keep", "new_string": "remove",
        })
        _, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, error)
        failed, _ = self.last_tool_result()
        self.assertIn("Found 2 matches", failed["error"])
        self.assertEqual(target.read_text(), "keep before\nchanged\nkeep after\n")
        arguments = {"mode": "replace", "path": "notes.txt", "old_string": "keep",
                     "new_string": "save", "replace_all": True}
        for answer, outcome in ((b"n\n", "keep"), (b"y\n", "save")):
            process, tty = self.interactive(tool="patch", arguments=arguments)
            self.wait_for_prompt(process, tty)
            os.write(tty, answer)
            _, error = process.communicate(timeout=20)
            self.assertEqual(process.returncode, 0, error)
            self.assertEqual(target.read_text(), f"{outcome} before\nchanged\n{outcome} after\n")

    def test_search_files_pages_results_and_omits_protected_paths(self):
        (self.root / "a.txt").write_text("marker marker one\nnot a match\nmarker two\n", encoding="utf-8")
        (self.root / "b.txt").write_text("marker three\n", encoding="utf-8")
        (self.root / ".env").write_text("marker secret\n", encoding="utf-8")
        (self.profile / "hidden.txt").write_text("marker profile secret\n", encoding="utf-8")
        (self.root / "link.txt").symlink_to(self.profile / "hidden.txt")
        results, requests = self.unattended("search_files", {"pattern": "marker", "file_glob": "*.txt", "limit": 1, "offset": 1})
        self.assertIn("search_files", [tool["function"]["name"] for tool in requests[0]["tools"]])
        self.assertEqual(results["matches"], [{"path": "a.txt", "line": 3, "content": "marker two"}])
        self.assertTrue(results["truncated"])
        found, _ = self.unattended("search_files", {"pattern": "*.txt", "target": "files"})
        self.assertEqual(found["files"], ["a.txt", "b.txt"])
        counts, _ = self.unattended("search_files", {"pattern": "marker", "output_mode": "count"})
        self.assertEqual(counts["counts"], {"a.txt": 2, "b.txt": 1})
        self.assertEqual(counts["total_count"], 3)
        blocked, _ = self.unattended("search_files", {"pattern": "marker", "path": ".env"})
        self.assertIn("error", blocked)


@unittest.skipUnless(sys.platform in ("linux", "darwin"), "requires a supported POSIX terminal")
class TerminalToolTest(ToolTestCase):
    def test_terminal_runs_in_the_approved_directory_without_profile_environment(self):
        (self.root / "subdir").mkdir()
        command = f"printf '%s\\n' \"${{HERMES_API_KEY:-no-key}}\"; pwd; {FLAGGED}"
        process, tty = self.interactive(tool="terminal", arguments={"command": command, "workdir": "subdir"})
        prompt = self.wait_for_prompt(process, tty)
        self.assertIn("subdir", prompt)
        self.assertIn("no-key", prompt)
        os.write(tty, b"yes\n")
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        result, requests = self.last_tool_result()
        tools = [tool["function"]["name"] for tool in requests[0]["tools"]]
        self.assertIn("terminal", tools)
        self.assertIn("write_file", tools)
        self.assertIn("patch", tools)
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(result["output"].splitlines(), ["no-key", str(self.root / "subdir")])
        process, tty = self.interactive(tool="terminal", arguments={"command": f"{FLAGGED}; exit 7"})
        self.wait_for_prompt(process, tty)
        os.write(tty, b"yes\n")
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        self.assertIn('Tool failed: "terminal"', error)
        result, _ = self.last_tool_result()
        self.assertEqual(result["exit_code"], 7)

    def test_terminal_does_not_start_without_approval(self):
        arguments = {"command": f"printf touched > marker.txt; {FLAGGED}"}
        blocked, requests = self.unattended("terminal", arguments)
        self.assertEqual(blocked["status"], "approval_unavailable")
        # With nobody to ask the terminal is still offered, and a flagged command is refused.
        self.assertIn("terminal", [tool["function"]["name"] for tool in requests[0]["tools"]])
        self.assertFalse((self.root / "marker.txt").exists())
        for mode in ("deny", "timeout", "cancel"):
            with self.subTest(mode=mode):
                process, tty = self.interactive(tool="terminal", arguments=arguments)
                self.wait_for_prompt(process, tty)
                if mode == "deny":
                    os.write(tty, b"n\n")
                if mode == "cancel":
                    process.send_signal(signal.SIGINT)
                _, error = self.finish_terminal(process, tty)
                self.assertEqual(process.returncode, 130 if mode == "cancel" else 0, error)
                self.assertFalse((self.root / "marker.txt").exists())
                if mode != "cancel":
                    result, _ = self.last_tool_result()
                    self.assertEqual(result["status"], "approval_" + ("denied" if mode == "deny" else "timeout"))

    def test_a_harmless_command_runs_without_a_question_and_a_flagged_one_says_why(self):
        # The reference's default: only what the detector flags needs an answer.
        process, tty = self.interactive(tool="terminal", arguments={"command": "echo ok"})
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        result, _ = self.last_tool_result()
        self.assertEqual(result["output"], "ok\n", result)
        process, tty = self.interactive(tool="terminal", arguments={"command": FLAGGED})
        prompt = self.wait_for_prompt(process, tty)
        self.assertIn("Flagged: recursive delete", prompt)
        os.write(tty, b"n\n")
        self.finish_terminal(process, tty)
        result, _ = self.last_tool_result()
        self.assertEqual(result["status"], "approval_denied")

    def test_always_saves_the_flagged_pattern_and_session_does_not(self):
        config = self.profile / "config.yaml"
        process, tty = self.interactive(tool="terminal", arguments={"command": FLAGGED})
        self.wait_for_prompt(process, tty)
        os.write(tty, b"s\n")
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        self.assertNotIn("command_allowlist", config.read_text())
        process, tty = self.interactive(tool="terminal", arguments={"command": FLAGGED})
        self.wait_for_prompt(process, tty)
        os.write(tty, b"a\n")
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        self.assertIn("recursive delete", config.read_text())
        # A later run finds the stored answer and does not ask.
        process, tty = self.interactive(tool="terminal", arguments={"command": FLAGGED})
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        result, _ = self.last_tool_result()
        self.assertEqual(result["exit_code"], 0, result)

    def test_terminal_respects_backend_and_workspace(self):
        arguments = {"command": "printf touched > marker.txt"}
        process, _ = self.interactive(tool="terminal", arguments={**arguments, "workdir": ".."})
        _, error = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 0, error)
        result, _ = self.last_tool_result()
        self.assertIn("outside the working directory", result["error"])
        config = self.profile / "config.yaml"
        for source in ("config", "dotenv", "environment"):
            with self.subTest(source=source):
                config.write_text(self.config + ("terminal: {backend: docker}\n" if source == "config" else ""))
                (self.profile / ".env").write_text("TERMINAL_ENV=docker\n" if source == "dotenv" else "")
                self.env.pop("TERMINAL_ENV", None)
                if source == "environment":
                    self.env["TERMINAL_ENV"] = "docker"
                process, _ = self.interactive(tool="terminal", arguments=arguments)
                _, error = process.communicate(timeout=20)
                self.assertEqual(process.returncode, 0, error)
                result, requests = self.last_tool_result()
                self.assertEqual(result["status"], "backend_unavailable")
                self.assertNotIn("terminal", [tool["function"]["name"] for tool in requests[-1]["tools"]])
                self.assertFalse((self.root / "marker.txt").exists())

    def test_terminal_refuses_a_directory_replaced_during_approval(self):
        directory = self.root / "workdir"
        directory.mkdir()
        arguments = {"command": f"printf touched > marker.txt; {FLAGGED}", "workdir": "workdir"}
        process, tty = self.interactive(tool="terminal", arguments=arguments)
        self.wait_for_prompt(process, tty)
        original = directory.rename(self.root / "original")
        directory.mkdir()
        os.write(tty, b"y\n")
        _, error = self.finish_terminal(process, tty)
        self.assertEqual(process.returncode, 0, error)
        result, _ = self.last_tool_result()
        self.assertIn("directory changed", result["error"])
        self.assertFalse((directory / "marker.txt").exists())
        self.assertFalse((original / "marker.txt").exists())

    def test_terminal_finishes_child_group_on_exit_timeout_and_cancellation(self):
        for mode in ("finish", "timeout", "cancel", "owner_cancel"):
            with self.subTest(mode=mode):
                if mode == "owner_cancel":
                    self.start_owner()
                pid_file = self.root / "child.pid"
                pid_file.unlink(missing_ok=True)
                command = f"{FLAGGED}; sleep 30 & printf '%s' \"$!\" > child.pid"
                if mode != "finish":
                    command += "; wait"
                arguments = {"command": command, "timeout": 1 if mode == "timeout" else 30}
                process, tty = self.interactive(tool="terminal", arguments=arguments)
                self.wait_for_prompt(process, tty)
                os.write(tty, b"y\n")
                deadline = time.monotonic() + 5
                child = ""
                while time.monotonic() < deadline:
                    if pid_file.exists():
                        child = pid_file.read_text().strip()
                        if child.isdigit():
                            break
                    time.sleep(0.02)
                self.assertTrue(child.isdigit(), "shell did not report the child process ID")
                cancelled = mode in ("cancel", "owner_cancel")
                if cancelled:
                    process.send_signal(signal.SIGINT)
                _, error = self.finish_terminal(process, tty)
                self.assertEqual(process.returncode, 130 if cancelled else 0, error)
                if mode == "timeout":
                    result, _ = self.last_tool_result()
                    self.assertEqual(result["status"], "timeout")
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    status = subprocess.run(
                        ["ps", "-o", "stat=", "-p", child], capture_output=True, text=True, check=False,
                    )
                    if status.returncode == 1 or status.stdout.lstrip().startswith("Z"):
                        break
                    self.assertEqual(status.returncode, 0, status.stderr)
                    time.sleep(0.02)
                else:
                    self.fail(f"child {child} remained active")


@unittest.skipUnless(sys.platform == "linux", "requires Linux file approvals")
class UnattendedWorkTest(ToolTestCase):
    """What makes a long task workable: approval rules, the user's environment, a remembered
    directory, directories created on write, and a journal of the run."""

    def run_tool(self, tool, arguments, *flags, config=""):
        if config:
            (self.profile / "config.yaml").write_text(self.config + config, encoding="utf-8")
        command = [*self.command(tool=tool, arguments=arguments), "--oneshot", *flags]
        run = subprocess.run(command, cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        return self.last_tool_result()

    def test_tool_output_limits_cut_what_the_model_sees(self):
        config = "tool_output:\n  max_bytes: 100\n"
        result, _ = self.run_tool(
            "terminal", {"command": "head -c 1000 /dev/zero | tr '\\0' x"}, "--yolo", config=config)
        self.assertEqual(result["truncated"], True, result)
        self.assertIn("... [OUTPUT TRUNCATED - 900 chars omitted out of 1,000 total] ...", result["output"])
        self.assertEqual(result["output"].count("x"), 100)

    def test_terminal_timeout_sets_the_default_foreground_timeout(self):
        result, _ = self.run_tool("terminal", {"command": "sleep 5"}, "--yolo",
                                  config="terminal:\n  timeout: 1\n")
        self.assertEqual(result["status"], "timeout", result)
        # A call's own timeout still wins.
        quick, _ = self.run_tool("terminal", {"command": "sleep 2; echo done", "timeout": 10}, "--yolo",
                                 config="terminal:\n  timeout: 1\n")
        self.assertEqual(quick["output"], "done\n", quick)

    def test_yolo_approves_work_without_a_terminal_and_the_deny_list_still_blocks(self):
        result, _ = self.run_tool(
            "write_file", {"path": "made/by/yolo.txt", "content": "hello"}, "--yolo")
        self.assertTrue(result["dirs_created"], result)
        self.assertEqual((self.root / "made/by/yolo.txt").read_text(), "hello")
        denied, _ = self.run_tool(
            "terminal", {"command": "rm -rf precious"}, "--yolo",
            config="  deny: ['rm -rf*']\n")
        self.assertEqual(denied["status"], "approval_denied")
        ran, _ = self.run_tool("terminal", {"command": "echo ok"}, "--yolo")
        self.assertEqual(ran["output"], "ok\n")

    def test_configured_allow_list_needs_no_person_but_a_chain_is_refused(self):
        config = "  allow: ['echo *']\n"
        ran, requests = self.run_tool("terminal", {"command": "echo fine"}, config=config)
        self.assertEqual(ran["output"], "fine\n", ran)
        # A chain no pattern covers still needs a person when the detector flags it...
        chained, _ = self.run_tool("terminal", {"command": f"echo a; {FLAGGED}"}, config=config)
        self.assertEqual(chained["status"], "approval_unavailable")
        # ...and a harmless one runs without a question (the reference's default).
        harmless, _ = self.run_tool("terminal", {"command": "echo a; echo b"}, config=config)
        self.assertEqual(harmless["output"], "a\nb\n", harmless)

    def test_the_terminal_is_offered_without_a_person_and_the_modes_decide_flagged_commands(self):
        # The reference's defaults: an unflagged command runs, a flagged one is refused...
        ran, requests = self.run_tool("terminal", {"command": "echo ok"})
        self.assertEqual(ran["output"], "ok\n", ran)
        self.assertIn("terminal", [tool["function"]["name"] for tool in requests[0]["tools"]])
        self.assertNotIn("execute_code", [tool["function"]["name"] for tool in requests[0]["tools"]])
        refused, _ = self.run_tool("terminal", {"command": FLAGGED})
        self.assertEqual(refused["status"], "approval_unavailable", refused)
        # ...unless approvals.single_query_mode says approve; the floor stays whatever the mode says.
        mode = "  single_query_mode: approve\n"
        approved, _ = self.run_tool("terminal", {"command": FLAGGED}, config=mode)
        self.assertEqual(approved["exit_code"], 0, approved)
        floor, _ = self.run_tool("terminal", {"command": "mkfs.ext4 /no/such/device-for-approval-probe"}, config=mode)
        self.assertEqual(floor["status"], "approval_denied", floor)
        # Any other word, `true` included, is deny, as in the reference.
        denied, _ = self.run_tool("terminal", {"command": FLAGGED}, config="  single_query_mode: true\n")
        self.assertEqual(denied["status"], "approval_unavailable", denied)

    def test_shell_init_files_extend_path_but_leak_nothing_else(self):
        home = self.root / "login-home"
        tools = home / "tools"
        tools.mkdir(parents=True)
        probe = tools / "probe-tool"
        probe.write_text("#!/bin/sh\necho probe-ran\n", encoding="utf-8")
        probe.chmod(0o755)
        (home / ".bashrc").write_text(
            f'export PATH="{tools}:$PATH"\nexport LEAKED_TOKEN=secret-from-rc\n', encoding="utf-8")
        self.env["HOME"] = str(home)
        command = {"command": "probe-tool; echo ${LEAKED_TOKEN:-withheld}"}
        found, _ = self.run_tool("terminal", command)
        self.assertEqual(found["output"], "probe-ran\nwithheld\n", found)
        # auto_source_bashrc: false leaves the rc files unread; an explicit list replaces the defaults.
        off, _ = self.run_tool("terminal", {"command": "probe-tool"}, config="terminal:\n  auto_source_bashrc: false\n")
        self.assertNotEqual(off["exit_code"], 0, off)
        listed, _ = self.run_tool("terminal", command,
                                  config=f"terminal:\n  shell_init_files: ['{home}/.bashrc']\n  auto_source_bashrc: false\n")
        self.assertEqual(listed["output"], "probe-ran\nwithheld\n", listed)

    def test_the_hardline_floor_refuses_even_under_yolo(self):
        # mkfs on a device that does not exist: refused by the floor, and harmless if it ever ran.
        command = "mkfs.ext4 /no/such/device-for-approval-probe"
        refused, _ = self.run_tool("terminal", {"command": command}, "--yolo")
        self.assertEqual(refused["status"], "approval_denied", refused)

    def test_command_allowlist_answers_for_a_flagged_command_with_no_person(self):
        config = "  allow: ['pwd']\n"
        asked, _ = self.run_tool("terminal", {"command": FLAGGED}, config=config)
        self.assertEqual(asked["status"], "approval_unavailable", asked)
        stored, _ = self.run_tool(
            "terminal", {"command": FLAGGED}, config=config + "command_allowlist: ['recursive delete']\n")
        self.assertEqual(stored["exit_code"], 0, stored)

    def test_commands_see_the_toolchain_environment_and_keep_the_directory(self):
        (self.root / "sub").mkdir()
        self.env["HERMES_FIXTURE_TOOL"] = "visible"
        (self.profile / "config.yaml").write_text(
            self.config + "terminal:\n  env_passthrough: [HERMES_FIXTURE_TOOL]\n", encoding="utf-8")
        result, _ = self.run_tool(
            "terminal",
            {"command": "echo $HERMES_FIXTURE_TOOL; echo ${HERMES_API_KEY:-withheld}; command -v git cargo sh | head -1; cd sub"},
            "--yolo")
        lines = result["output"].splitlines()
        self.assertEqual(lines[:2], ["visible", "withheld"])
        self.assertTrue(lines[2].startswith("/"), lines)
        self.assertEqual(result["cwd"], str(self.root / "sub"))

    def test_the_redaction_switch_follows_the_config_and_the_environment(self):
        (self.root / ".netrc").write_text("machine example.test login alice password hunter2hunter2\n",
                                          encoding="utf-8")
        arguments = {"path": ".netrc"}
        masked, _ = self.run_tool("read_file", arguments)
        self.assertNotIn("hunter2hunter2", masked["content"])
        off, _ = self.run_tool("read_file", arguments, config="security:\n  redact_secrets: false\n")
        self.assertIn("hunter2hunter2", off["content"])
        # The environment wins over the config.
        self.env["HERMES_REDACT_SECRETS"] = "true"
        again, _ = self.run_tool("read_file", arguments, config="security:\n  redact_secrets: false\n")
        self.assertNotIn("hunter2hunter2", again["content"])
        self.env["HERMES_REDACT_SECRETS"] = "off"
        env_off, _ = self.run_tool("read_file", arguments)
        self.assertIn("hunter2hunter2", env_off["content"])

    def test_the_debug_journal_records_requests_responses_and_tools_redacted(self):
        journal = self.root / "journal.jsonl"
        self.env["HERMES_DEBUG_LOG"] = str(journal)
        key = "ghp_abcdefghijklmnop1234567890"
        self.run_tool("terminal", {"command": f"echo {key}"}, "--yolo")
        events = [json.loads(line) for line in journal.read_text().splitlines()]
        kinds = [event["event"] for event in events]
        self.assertEqual(kinds[0], "request")
        self.assertIn("response", kinds)
        tool = next(event for event in events if event["event"] == "tool")
        self.assertEqual(tool["name"], "terminal")
        self.assertNotIn(key, journal.read_text())
        self.assertEqual(oct(journal.stat().st_mode & 0o777), "0o600")


@unittest.skipUnless(sys.platform == "linux", "requires Linux file approvals")
class DelegationTest(ToolTestCase):
    scenario = "delegate"

    def delegate(self, *flags, config=""):
        if config:
            (self.profile / "config.yaml").write_text(self.config + config, encoding="utf-8")
        journal = self.root / "journal.jsonl"
        command = [str(BINARY), "chat", "--oneshot", "--debug-log", str(journal), "-q", "delegate", *flags]
        run = subprocess.run(command, cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
                             capture_output=True, text=True, timeout=40)
        self.assertEqual(run.returncode, 0, run.stderr)
        with urlopen(self.url + "/requests", timeout=4) as response:
            requests = json.load(response)
        events = [json.loads(line) for line in journal.read_text().splitlines()]
        return run, requests, events

    def test_delegation_model_pins_the_subagents_route(self):
        run, requests, _ = self.delegate("--yolo", config="delegation:\n  model: child-model\n")
        children = [r for r in requests if r["messages"][0]["content"].startswith("You are a focused subagent")]
        parents = [r for r in requests if r not in children]
        self.assertTrue(children)
        self.assertEqual({r["model"] for r in children}, {"child-model"})
        self.assertEqual({r["model"] for r in parents}, {"fixture"})
        result = json.loads(next(m for m in parents[1]["messages"] if m.get("role") == "tool")["content"])
        self.assertEqual({r["model"] for r in result["results"]}, {"child-model"})

    def test_a_delegation_endpoint_without_a_model_is_refused_clearly(self):
        (self.profile / "config.yaml").write_text(
            self.config + "delegation:\n  provider: custom\n  base_url: https://elsewhere.example/v1\n"
            "  model: m\n  api_key: k\n", encoding="utf-8")
        run = subprocess.run([str(BINARY), "chat", "--oneshot", "-q", "delegate", "--yolo"], cwd=self.root,
                             env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=40)
        # A complete section is accepted at startup (the endpoint is only used when a child runs).
        self.assertNotIn("delegation.model is required", run.stderr)
        (self.profile / "config.yaml").write_text(
            self.config + "delegation:\n  provider: openai\n  base_url: https://elsewhere.example/v1\n",
            encoding="utf-8")
        refused = subprocess.run([str(BINARY), "chat", "--oneshot", "-q", "delegate"], cwd=self.root,
                                 env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                 timeout=40)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("delegation.model is required", refused.stderr)

    def test_children_run_in_isolation_and_their_summaries_come_back(self):
        run, requests, events = self.delegate("--yolo")
        children = [r for r in requests if r["messages"][0]["content"].startswith("You are a focused subagent")]
        parents = [r for r in requests if r not in children]
        self.assertGreaterEqual(len(children), 3)  # the mover needs two requests, the other one
        names = {tool["function"]["name"] for tool in children[0]["tools"]}
        self.assertIn("terminal", names)
        for blocked in ("delegate_task", "clarify", "memory"):
            self.assertNotIn(blocked, names)
        self.assertIn("delegate_task", {t["function"]["name"] for t in parents[0]["tools"]})
        first = next(c for c in children if "ctx A" in c["messages"][1]["content"])
        self.assertEqual(first["messages"][1]["content"], "CONTEXT:\nctx A\n\nTASK:\nMOVE around the project")
        # Only the goal and context reach a child: nothing of the parent's conversation.
        for child in children:
            self.assertEqual([m["role"] for m in child["messages"][:2]], ["system", "user"])
            self.assertNotIn("delegate", [m.get("content") for m in child["messages"]])
        result = json.loads(next(m for m in parents[1]["messages"] if m.get("role") == "tool")["content"])
        self.assertEqual([r["status"] for r in result["results"]], ["completed", "completed"])
        self.assertEqual([r["exit_reason"] for r in result["results"]], ["completed", "completed"])
        self.assertEqual(result["results"][0]["summary"], "child-done:MOVE around th", result)
        self.assertEqual(result["results"][0]["tool_trace"], [{"tool": "terminal", "status": "ok"}])
        self.assertEqual(result["results"][1]["tool_trace"], [])
        self.assertIn("total_duration_seconds", result)
        # The child's `cd` did not move the parent.
        pwd = json.loads(next(m for m in reversed(parents[-1]["messages"]) if m.get("role") == "tool")["content"])
        self.assertEqual(pwd["cwd"], str(self.root))
        self.assertTrue((self.root / "moved").is_dir())
        # The journal tells children apart.
        self.assertEqual({e.get("child") for e in events if e["event"] == "request"}, {None, 0, 1})

    def test_a_child_cannot_be_asked_so_what_the_rules_leave_open_is_refused(self):
        run, requests, events = self.delegate(config="  allow: ['pwd']\n")
        move = next(e for e in events if e["event"] == "tool" and e.get("child") == 0)
        self.assertEqual(json.loads(move["output"])["status"], "approval_denied")
        self.assertFalse((self.root / "moved").exists())
        # With subagent_auto_approve the same child goes ahead.
        run, requests, events = self.delegate(config="  allow: ['pwd']\ndelegation:\n  subagent_auto_approve: true\n")
        self.assertTrue((self.root / "moved").is_dir())

    def test_the_one_shot_budget_refuses_a_third_child(self):
        run, requests, events = self.delegate("--yolo", config="delegation:\n  oneshot_max_children: 1\n")
        result = json.loads(next(m for m in requests[1]["messages"] if m.get("role") == "tool")["content"])
        self.assertIn("Delegation budget for this one-shot run is exhausted (0/1", result["error"])


@unittest.skipUnless(sys.platform == "linux", "requires Linux file approvals")
class ExecuteCodeTest(ToolTestCase):
    scenario = "code"

    def cells(self, *cells, flags=("--yolo",), config="", extra_env=None):
        if config:
            (self.profile / "config.yaml").write_text(self.config + config, encoding="utf-8")
        env = {**self.env, **(extra_env or {})}
        command = [str(BINARY), "chat", "--oneshot", *flags, "-q", json.dumps({"cells": list(cells)})]
        run = subprocess.run(command, cwd=self.root, env=env, stdin=subprocess.DEVNULL,
                             capture_output=True, text=True, timeout=60)
        self.assertEqual(run.returncode, 0, run.stderr)
        with urlopen(self.url + "/requests", timeout=4) as response:
            requests = json.load(response)
        results = [json.loads(m["content"]) for m in requests[-1]["messages"] if m.get("role") == "tool"]
        return results, requests

    def test_state_persists_and_tools_run_through_the_normal_path(self):
        results, requests = self.cells(
            "x = 41",
            "print(x + 1)",
            "from hermes_tools import terminal, read_file, write_file\n"
            "write_file('made.txt', 'hello')\n"
            "print(terminal('echo hi')['output'], end='')\n"
            "print(read_file('made.txt')['content'])",
        )
        self.assertEqual([r["status"] for r in results], ["success"] * 3)
        self.assertEqual(results[1]["output"], "42\n")
        self.assertEqual(results[1]["kernel"], {"mode": "session", "reused": True, "execution_count": 2, "state_reset": False})
        self.assertEqual(results[2]["tool_calls_made"], 3)
        self.assertEqual(results[2]["output"], "hi\n1|hello\n")
        self.assertEqual((self.root / "made.txt").read_text(), "hello")
        offered = [t["function"]["name"] for t in requests[0]["tools"]]
        self.assertIn("execute_code", offered)

    def test_errors_exit_and_big_output_are_reported_like_the_reference(self):
        results, _ = self.cells("raise ValueError('boom')", "print('x' * 120000)", "import sys; print('bye'); sys.exit(0)", "print('fresh')")
        self.assertEqual(results[0]["status"], "error")
        self.assertEqual(results[0]["exit_code"], 1)
        self.assertIn("ValueError: boom", results[0]["error"])
        self.assertTrue(results[1]["stdout_truncated"])
        self.assertEqual(results[1]["stdout_bytes_total"], 120001)
        self.assertIn("OUTPUT TRUNCATED", results[1]["output"])
        self.assertTrue(results[2]["kernel"]["ended"])
        self.assertEqual(results[3]["kernel"]["reused"], False)

    def test_the_kernel_never_sees_the_model_credentials(self):
        results, _ = self.cells(
            "import os\nprint(os.environ.get('HERMES_API_KEY'), os.environ.get('OPENAI_API_KEY'), 'PATH' in os.environ)",
            extra_env={"OPENAI_API_KEY": "sk-should-not-leak"})
        self.assertEqual(results[0]["output"], "None None True\n")

    def test_the_deny_list_covers_inner_terminal_calls_and_the_call_budget_holds(self):
        results, _ = self.cells(
            "from hermes_tools import terminal\nprint(terminal('rm -rf /tmp/never-here'))",
            "from hermes_tools import read_file\n"
            "print([('error' in read_file('made.txt')) for _ in range(4)])",
            config="  deny: ['rm -rf*']\ncode_execution:\n  max_tool_calls: 2\n")
        self.assertIn("approval_denied", results[0]["output"])
        self.assertEqual(results[1]["output"], "[True, True, True, True]\n")
        self.assertEqual(results[1]["tool_calls_made"], 2)

    def test_a_timeout_kills_the_kernel_and_its_state(self):
        results, _ = self.cells("x = 1", "import time; time.sleep(30)", "print(x)", config="code_execution:\n  timeout: 2\n")
        self.assertEqual(results[1]["status"], "timeout")
        self.assertEqual(results[1]["exit_code"], -1)
        self.assertIn("Cell timed out after 2s", results[1]["error"])
        self.assertEqual(results[2]["status"], "error")
        self.assertIn("NameError", results[2]["error"])

    def test_embedded_timeout_stops_the_running_interpreter(self):
        results, _ = self.cells(
            "import os\nfrom hermes_tools import write_file\n"
            "write_file('worker-pid', str(os.getpid()))",
            "while True: pass",
            "import os\nfrom hermes_tools import read_file\n"
            "pid = read_file('worker-pid')['content'].split('|', 1)[1].strip()\n"
            "try:\n os.stat('/proc/' + pid)\nexcept OSError:\n print('stopped')\n"
            "else:\n print('still running')",
            config="code_execution:\n  timeout: 3\n",
            extra_env={"HERMES_PYTHON_ENGINE": "embedded"},
        )
        self.assertEqual([r["status"] for r in results], ["success", "timeout", "success"])
        self.assertEqual(results[2]["output"], "stopped\n")

    def test_code_needs_approval_even_when_commands_are_allowed(self):
        results, requests = self.cells("print(1)", flags=(), config="  allow: ['*']\n")
        self.assertEqual(results[0]["status"], "approval_unavailable")
        (self.profile / "config.yaml").write_text(self.config, encoding="utf-8")
        results, requests = self.cells("print(1)", flags=())
        self.assertNotIn("execute_code", [t["function"]["name"] for t in requests[-1]["tools"]])


class CustomEndpointConfigTest(ToolTestCase):
    """A config that keeps the key in `model.api_key` and per-endpoint request fields under `providers`."""

    def ask(self, model="qwen-flash", extra=""):
        (self.profile / "config.yaml").write_text(
            f"model:\n  api_key: sk-key\n  base_url: {self.url}/v1\n  default: {model}\n"
            "  max_concurrency: 1\n  provider: custom\n  context_length: 1000000\n"
            f"providers:\n  llm-api:\n    name: llm-api\n    base_url: {self.url}/v1\n"
            "    model: qwen-flash\n    extra_body:\n      chat_template_kwargs:\n"
            f"        enable_thinking: true\n{extra}", encoding="utf-8")
        env = {key: value for key, value in self.env.items() if key != "HERMES_API_KEY"}
        run = subprocess.run(
            self.command(oneshot=True), cwd=self.root, env=env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        with urlopen(self.url + "/requests", timeout=4) as response:
            return json.load(response)

    def test_the_key_comes_from_the_model_section_and_extra_body_reaches_the_request(self):
        request = self.ask()[0]
        self.assertEqual(request["chat_template_kwargs"], {"enable_thinking": True})
        self.assertEqual(request["model"], "qwen-flash")

    def test_extra_body_applies_only_to_the_model_it_names(self):
        self.assertNotIn("chat_template_kwargs", self.ask(model="other")[0])

    def test_a_disabled_provider_adds_nothing(self):
        request = self.ask(extra="  second:\n    enabled: false\n    base_url: x\n")[0]
        self.assertEqual(request["chat_template_kwargs"], {"enable_thinking": True})

    def test_references_in_any_config_value_expand_from_the_profile_env(self):
        (self.profile / ".env").write_text(f"SERVER={self.url}\nMODEL_KEY=sk-from-env\n", encoding="utf-8")
        (self.profile / "config.yaml").write_text(
            "model:\n  api_key: ${env:MODEL_KEY}\n  base_url: ${SERVER}/v1\n  default: qwen-flash\n"
            "  provider: custom\n", encoding="utf-8")
        env = {key: value for key, value in self.env.items() if key != "HERMES_API_KEY"}
        run = subprocess.run(self.command(oneshot=True), cwd=self.root, env=env, stdin=subprocess.DEVNULL,
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        with urlopen(self.url + "/requests", timeout=4) as response:
            self.assertEqual(json.load(response)[0]["model"], "qwen-flash")

    def test_extra_headers_are_sent_but_never_journaled(self):
        journal = self.root / "journal.jsonl"
        (self.profile / "config.yaml").write_text(
            f"model:\n  api_key: sk-key\n  base_url: {self.url}/v1\n  default: qwen-flash\n"
            "  provider: custom\n  default_headers: {X-Default: d, X-Gateway-Token: old}\n"
            f"providers:\n  gw:\n    base_url: {self.url}/v1\n"
            "    extra_headers: {X-Gateway-Token: secret-token, X-Num: 5, X-Skip: null}\n",
            encoding="utf-8")
        env = {key: value for key, value in self.env.items() if key != "HERMES_API_KEY"}
        run = subprocess.run(
            [*self.command(oneshot=True), "--debug-log", str(journal)], cwd=self.root, env=env,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        with urlopen(self.url + "/headers", timeout=4) as response:
            seen = json.load(response)[0]
        self.assertEqual(seen["x-gateway-token"], "secret-token")
        self.assertEqual(seen["x-default"], "d")
        self.assertEqual(seen["x-num"], "5")
        self.assertNotIn("x-skip", seen)
        self.assertEqual(seen["authorization"], "Bearer sk-key")
        self.assertNotIn("secret-token", journal.read_text(encoding="utf-8"))
        self.assertNotIn("secret-token", run.stdout + run.stderr)


class PrefillMessagesTest(ToolTestCase):
    scenario = "text"

    def ask(self, config, prefill=None, raw=None):
        if prefill is not None or raw is not None:
            (self.profile / "prefill.json").write_text(
                raw if raw is not None else json.dumps(prefill), encoding="utf-8")
        (self.profile / "config.yaml").write_text(self.config + config, encoding="utf-8")
        run = subprocess.run([str(BINARY), "chat", "--oneshot", "--save-session", "-q", "real question"],
                             cwd=self.root, env=self.env, stdin=subprocess.DEVNULL, capture_output=True,
                             text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        with urlopen(self.url + "/requests", timeout=4) as response:
            return json.load(response)[-1]["messages"], run

    def test_prefill_messages_follow_the_system_prompt_and_are_not_saved(self):
        examples = [{"role": "user", "content": "example question"},
                    {"role": "assistant", "content": "example answer"}]
        messages, _ = self.ask("prefill_messages_file: prefill.json\n", prefill=examples)
        self.assertEqual([m["role"] for m in messages], ["system", "user", "assistant", "user"])
        self.assertEqual(messages[1:3], examples)
        self.assertEqual(messages[3]["content"], "real question")
        with sqlite3.connect(self.profile / "state.db") as db:
            contents = [row[0] for row in db.execute("SELECT content FROM messages ORDER BY id")]
        self.assertNotIn("example question", contents)
        self.assertIn("real question", contents)

    def test_the_environment_names_the_file_first_and_problems_only_warn(self):
        messages, run = self.ask("prefill_messages_file: missing.json\n")
        self.assertEqual([m["role"] for m in messages], ["system", "user"])
        self.assertIn("Prefill messages file not found", run.stderr)
        self.env["HERMES_PREFILL_MESSAGES_FILE"] = str(self.profile / "other.json")
        (self.profile / "other.json").write_text('[{"role": "user", "content": "from env"}]')
        messages, _ = self.ask("prefill_messages_file: missing.json\n")
        self.assertEqual(messages[1]["content"], "from env")
        del self.env["HERMES_PREFILL_MESSAGES_FILE"]
        messages, run = self.ask("agent:\n  prefill_messages_file: prefill.json\n", raw='{"not": "a list"}')
        self.assertEqual(len(messages), 2)
        self.assertIn("must contain a JSON array", run.stderr)
