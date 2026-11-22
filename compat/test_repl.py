"""The interactive conversation: several turns in one process, on a terminal and on a pipe."""
import fcntl
import json
import os
import pty
import select
import sqlite3
import subprocess
import sys
import termios
import time
import unittest
from urllib.request import urlopen

from test_file_tools import BINARY, ToolTestCase

PROMPT = "› ".encode()


class ReplBase(ToolTestCase):
    def start(self, *args, tty=True):
        command = [str(BINARY), "chat", *args]
        if not tty:
            process = subprocess.Popen(
                command, cwd=self.root, env=self.env, stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(self.stop, process)
            return process, None

        master, slave = pty.openpty()
        self.addCleanup(os.close, master)

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            process = subprocess.Popen(
                command, cwd=self.root, env=self.env, stdin=slave, stdout=slave, stderr=slave,
                preexec_fn=controlling_terminal)
        finally:
            os.close(slave)
        self.addCleanup(self.stop_tty, process)
        self.transcript = bytearray()
        return process, master

    def stop_tty(self, process):
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)

    def wait_for(self, master, marker, count=1, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.transcript.count(marker) >= count:
                return
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    self.transcript.extend(os.read(master, 16384))
                except OSError:
                    break
        self.fail(f"{marker!r} x{count} not seen in {bytes(self.transcript)!r}")

    def finish(self, process, master, timeout=20):
        deadline = time.monotonic() + timeout
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    self.transcript.extend(os.read(master, 16384))
                except OSError:
                    break
        self.assertIsNotNone(process.poll(), bytes(self.transcript))
        return process.returncode

    def requests(self):
        with urlopen(self.url + "/requests", timeout=4) as response:
            return json.load(response)

    def sessions(self):
        with sqlite3.connect(self.profile / "state.db") as db:
            return db.execute("SELECT id, message_count FROM sessions ORDER BY started_at").fetchall()



@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplTest(ReplBase):
    scenario = "text"

    def test_three_turns_in_one_process_share_one_saved_session(self):
        process, master = self.start()
        self.wait_for(master, PROMPT)
        for index, line in enumerate(["first question", "second question", "third question"], 1):
            os.write(master, line.encode() + b"\n")
            self.wait_for(master, PROMPT, count=index + 1)
        os.write(master, b"\x04")
        self.assertEqual(self.finish(process, master), 0)
        requests = self.requests()
        self.assertEqual(len(requests), 3)
        users = [m["content"] for m in requests[2]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["first question", "second question", "third question"])
        sessions = self.sessions()
        self.assertEqual(len(sessions), 1, sessions)
        self.assertIn(b"hermes chat --resume " + sessions[0][0].encode(), bytes(self.transcript))
        self.assertEqual(self.transcript.count("Ответ модели".encode()), 3)

    def test_piped_input_runs_each_line_and_help_and_unknown_commands_do_not_call_the_model(self):
        process, _ = self.start(tty=False)
        out, err = process.communicate("one\n/help\n/nope\ntwo\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        self.assertIn("/new", out)
        self.assertIn("Unknown command /nope", err)
        self.assertEqual(len(self.requests()), 2)
        self.assertEqual(len(self.sessions()), 1)

    def test_new_starts_another_conversation(self):
        process, _ = self.start(tty=False)
        _, err = process.communicate("a\n/new\nb\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        users = [[m["content"] for m in r["messages"] if m["role"] == "user"] for r in self.requests()]
        self.assertEqual(users, [["a"], ["b"]])
        self.assertEqual(len(self.sessions()), 2)

    def test_a_trailing_backslash_continues_the_input(self):
        process, _ = self.start(tty=False)
        _, err = process.communicate("line one\\\nline two\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        user = [m["content"] for m in self.requests()[0]["messages"] if m["role"] == "user"][-1]
        self.assertEqual(user, "line one\nline two")

    def test_the_session_can_be_resumed_by_a_later_conversation(self):
        first, _ = self.start(tty=False)
        first.communicate("remember apples\n", timeout=30)
        session = self.sessions()[0][0]
        second, _ = self.start("--resume", session, tty=False)
        _, err = second.communicate("and pears\n", timeout=30)
        self.assertEqual(second.returncode, 0, err)
        users = [m["content"] for m in self.requests()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["remember apples", "and pears"])
        self.assertEqual(len(self.sessions()), 1)


@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplToolTest(ReplBase):
    scenario = "local_tools"

    def test_the_working_directory_survives_between_turns(self):
        (self.root / "sub").mkdir()
        process, _ = self.start("--yolo", tty=False)
        lines = [json.dumps({"path": "x", "content": "", "tool": "terminal", "arguments": {"command": command}})
                 for command in ("cd sub", "pwd")]
        _, err = process.communicate("\n".join(lines) + "\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        result, _ = self.last_tool_result()
        self.assertEqual(result["output"].strip(), str(self.root / "sub"))


@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplInterruptTest(ReplBase):
    scenario = "stall"

    def test_ctrl_c_stops_the_turn_and_returns_to_the_prompt(self):
        process, master = self.start()
        self.wait_for(master, PROMPT)
        os.write(master, b"this will hang\n")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not self.requests():
            time.sleep(0.05)
        self.assertTrue(self.requests(), "the model never saw the request")
        os.write(master, b"\x03")
        self.wait_for(master, b"interrupted")
        self.wait_for(master, PROMPT, count=2)
        self.assertIsNone(process.poll())
        os.write(master, b"/exit\n")
        self.assertEqual(self.finish(process, master), 0)


if __name__ == "__main__":
    unittest.main()
