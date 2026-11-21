import json
import os
import select
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
BINARY = Path(os.environ.get("HERMES_TEST_BINARY", ROOT / "target/debug/hermes")).resolve()


class ToolTestCase(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.profile = self.root / "profile"
        self.profile.mkdir()
        self.server = subprocess.Popen(
            ["python3", str(ROOT / "compat/model_server.py"), "--scenario", "local_tools"],
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

    def wait_for_prompt(self, process, master):
        transcript = bytearray()
        deadline = time.monotonic() + 8
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    transcript.extend(os.read(master, 16384))
                except OSError:
                    break
            if b"[y/N]" in transcript:
                return transcript.decode(errors="replace")
        details = ""
        if process.poll() is not None:
            _, stderr = process.communicate(timeout=1)
            details = f"; exit status {process.returncode}; stderr was {stderr!r}"
        self.fail(f"approval prompt missing; terminal output was {transcript!r}{details}")

    def last_tool_result(self):
        with urlopen(self.url + "/requests", timeout=4) as response:
            requests = json.load(response)
        return json.loads(requests[-1]["messages"][-1]["content"]), requests

    def unattended(self, tool, arguments):
        run = subprocess.run(
            self.command(tool=tool, arguments=arguments), cwd=self.root, env=self.env,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=8,
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
        output, error = process.communicate(timeout=8)
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
                _, error = process.communicate(timeout=8)
                self.assertEqual(process.returncode, 130 if mode == "cancel" else 0, error)
                self.assertFalse((self.root / f"{mode}.txt").exists())
                if mode != "cancel":
                    result, _ = self.last_tool_result()
                    self.assertEqual(result["status"], "approval_" + ("denied" if mode == "deny" else "timeout"))

    def test_unattended_write_is_hidden_and_refused(self):
        run = subprocess.run(
            self.command(), cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=8,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        result, requests = self.last_tool_result()
        self.assertNotIn("write_file", [tool["function"]["name"] for tool in requests[0]["tools"]])
        self.assertIn("error", result)
        self.assertFalse((self.root / "notes.txt").exists())
        process, _ = self.interactive(oneshot=True)
        _, stderr = process.communicate(timeout=8)
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
                _, stderr = process.communicate(timeout=8)
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
        _, error = process.communicate(timeout=8)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual(target.read_text(), "external edit\n")
        result, _ = self.last_tool_result()
        self.assertIn("changed", result["error"])
        process, tty = self.interactive(path="created.txt")
        self.wait_for_prompt(process, tty)
        os.write(tty, b"yes\n")
        _, error = process.communicate(timeout=8)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual((self.root / "created.txt").read_text(), "updated text")
        process, tty = self.interactive(path="cancelled.txt")
        self.wait_for_prompt(process, tty)
        process.send_signal(signal.SIGINT)
        _, error = process.communicate(timeout=8)
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
        _, error = process.communicate(timeout=8)
        self.assertEqual(process.returncode, 0, error)
        self.assertEqual(target.read_text(), "keep before\nchanged\nkeep after\n")
        patched, _ = self.last_tool_result()
        self.assertTrue(patched["success"])
        self.assertEqual(patched["files_modified"], ["notes.txt"])
        process, _ = self.interactive(tool="patch", arguments={
            "path": "notes.txt", "old_string": "keep", "new_string": "remove",
        })
        _, error = process.communicate(timeout=8)
        self.assertEqual(process.returncode, 0, error)
        failed, _ = self.last_tool_result()
        self.assertIn("multiple", failed["error"])
        self.assertEqual(target.read_text(), "keep before\nchanged\nkeep after\n")
        arguments = {"mode": "replace", "path": "notes.txt", "old_string": "keep",
                     "new_string": "save", "replace_all": True}
        for answer, outcome in ((b"n\n", "keep"), (b"y\n", "save")):
            process, tty = self.interactive(tool="patch", arguments=arguments)
            self.wait_for_prompt(process, tty)
            os.write(tty, answer)
            _, error = process.communicate(timeout=8)
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
        command = "printf '%s\\n' \"${HERMES_API_KEY:-no-key}\"; pwd"
        process, tty = self.interactive(tool="terminal", arguments={"command": command, "workdir": "subdir"})
        prompt = self.wait_for_prompt(process, tty)
        self.assertIn("subdir", prompt)
        self.assertIn("no-key", prompt)
        os.write(tty, b"yes\n")
        _, error = process.communicate(timeout=8)
        self.assertEqual(process.returncode, 0, error)
        result, requests = self.last_tool_result()
        tools = [tool["function"]["name"] for tool in requests[0]["tools"]]
        self.assertIn("terminal", tools)
        if sys.platform == "darwin":
            self.assertNotIn("write_file", tools)
            self.assertNotIn("patch", tools)
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(result["output"].splitlines(), ["no-key", str(self.root / "subdir")])
        process, tty = self.interactive(tool="terminal", arguments={"command": "exit 7"})
        self.wait_for_prompt(process, tty)
        os.write(tty, b"yes\n")
        _, error = process.communicate(timeout=8)
        self.assertEqual(process.returncode, 0, error)
        self.assertIn('Tool failed: "terminal"', error)
        result, _ = self.last_tool_result()
        self.assertEqual(result["exit_code"], 7)

    def test_terminal_does_not_start_without_approval(self):
        arguments = {"command": "printf touched > marker.txt"}
        blocked, requests = self.unattended("terminal", arguments)
        self.assertEqual(blocked["status"], "approval_unavailable")
        self.assertNotIn("terminal", [tool["function"]["name"] for tool in requests[0]["tools"]])
        for mode in ("deny", "timeout", "cancel"):
            with self.subTest(mode=mode):
                process, tty = self.interactive(tool="terminal", arguments=arguments)
                self.wait_for_prompt(process, tty)
                if mode == "deny":
                    os.write(tty, b"n\n")
                if mode == "cancel":
                    process.send_signal(signal.SIGINT)
                _, error = process.communicate(timeout=8)
                self.assertEqual(process.returncode, 130 if mode == "cancel" else 0, error)
                self.assertFalse((self.root / "marker.txt").exists())
                if mode != "cancel":
                    result, _ = self.last_tool_result()
                    self.assertEqual(result["status"], "approval_" + ("denied" if mode == "deny" else "timeout"))

    def test_terminal_respects_backend_and_workspace(self):
        arguments = {"command": "printf touched > marker.txt"}
        process, _ = self.interactive(tool="terminal", arguments={**arguments, "workdir": ".."})
        _, error = process.communicate(timeout=8)
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
                _, error = process.communicate(timeout=8)
                self.assertEqual(process.returncode, 0, error)
                result, requests = self.last_tool_result()
                self.assertEqual(result["status"], "backend_unavailable")
                self.assertNotIn("terminal", [tool["function"]["name"] for tool in requests[-1]["tools"]])
                self.assertFalse((self.root / "marker.txt").exists())

    def test_terminal_refuses_a_directory_replaced_during_approval(self):
        directory = self.root / "workdir"
        directory.mkdir()
        arguments = {"command": "printf touched > marker.txt", "workdir": "workdir"}
        process, tty = self.interactive(tool="terminal", arguments=arguments)
        self.wait_for_prompt(process, tty)
        original = directory.rename(self.root / "original")
        directory.mkdir()
        os.write(tty, b"y\n")
        _, error = process.communicate(timeout=8)
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
                command = "sleep 30 & printf '%s' \"$!\" > child.pid"
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
                _, error = process.communicate(timeout=8)
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
