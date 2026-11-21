import json
import os
import signal
import socket
import sqlite3
import stat
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from urllib.request import urlopen


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug/hermes"


@unittest.skipUnless(os.name == "posix", "requires Unix sockets and signals")
class OwnerTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.workdir = Path(directory.name)
        self.home = self.workdir / "profile"
        (self.workdir / "fixture.txt").write_text("fixture content", encoding="utf-8")
        self.env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home)}
        scenario = "burst" if self._testMethodName == "test_owner_preserves_bursts_of_text_events" else "stall_on_resume"
        model = subprocess.Popen(
            ["python3", str(ROOT / "compat/model_server.py"), "--scenario", scenario],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(self.stop_process, model)
        self.base = model.stdout.readline().strip()
        self.assertTrue(self.base)

    def stop_process(self, process):
        if process.poll() is None:
            process.terminate()
        try:
            return process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            return process.communicate(timeout=5)

    def start_owner(self):
        owner = subprocess.Popen([str(BINARY), "owner"], cwd=self.workdir, env=self.env,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop_process, owner)
        path = self.home / "rust-owner" / "owner.sock"
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and owner.poll() is None:
            if path.exists() and stat.S_IMODE(path.stat().st_mode) == 0o600:
                with socket.socket(socket.AF_UNIX) as connection:
                    try:
                        connection.connect(str(path))
                    except (FileNotFoundError, ConnectionRefusedError):
                        pass
                    else:
                        return owner
            time.sleep(0.02)
        _, stderr = self.stop_process(owner)
        self.fail(f"local owner did not start: {stderr}")

    def chat(self, query, *args):
        return subprocess.Popen(
            [str(BINARY), "chat", "-q", query, "--model", "fixture", "--base-url", self.base + "/v1", *args],
            cwd=self.workdir, env={**self.env, "HERMES_API_KEY": "fixture"},
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )

    def initial_session(self):
        process = self.chat("Read the fixture", "--save-session")
        self.addCleanup(self.stop_process, process)
        stdout, stderr = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertEqual(stdout, "Ответ модели\n")
        self.assertIn('Tool started: "read_file"', stderr)
        session = stderr.splitlines()[-1].removeprefix("Session: ")
        self.assertTrue(session)
        return session

    def begin_held_turn(self, session, request_count):
        held = self.chat("hold resume", "--resume", session)
        self.addCleanup(self.stop_process, held)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            with urlopen(self.base + "/requests", timeout=2) as response:
                if len(json.load(response)) == request_count:
                    return held
            time.sleep(0.02)
        self.fail("owner did not start the provider request")

    def test_owner_restarts_after_a_crash_and_resumes_saved_history(self):
        owner = self.start_owner()
        socket = self.home / "rust-owner" / "owner.sock"
        self.assertEqual(stat.S_IMODE(socket.parent.stat().st_mode), 0o700)
        duplicate = subprocess.run([str(BINARY), "owner"], cwd=self.workdir, env=self.env,
                                   capture_output=True, text=True, timeout=5)
        self.assertNotEqual(duplicate.returncode, 0)
        self.assertIn("already running", duplicate.stderr)
        session = self.initial_session()
        owner.kill()
        owner.communicate(timeout=5)
        self.assertTrue(socket.exists())
        stopped = self.chat("No owner is listening", "--resume", session)
        self.addCleanup(self.stop_process, stopped)
        _, stderr = stopped.communicate(timeout=5)
        self.assertNotEqual(stopped.returncode, 0)
        self.assertIn("cannot connect to local owner", stderr)

        self.start_owner()
        resumed = self.chat("Continue", "--resume", session)
        self.addCleanup(self.stop_process, resumed)
        stdout, stderr = resumed.communicate(timeout=10)
        self.assertEqual(resumed.returncode, 0, stderr)
        self.assertEqual(stdout, "Ответ модели\n")
        self.assertIn(f"Session: {session}", stderr)
        with sqlite3.connect(self.home / "state.db") as db:
            self.assertEqual(db.execute("SELECT count(*) FROM messages WHERE session_id = ?", (session,)).fetchone(), (6,))

    def test_client_cancel_and_disconnect_release_owner_lease(self):
        owner = self.start_owner()
        session = self.initial_session()
        for count, signum in enumerate((signal.SIGINT, signal.SIGKILL), start=3):
            held = self.begin_held_turn(session, count)
            if signum == signal.SIGINT:
                overlapping = self.chat("Overlapping request", "--resume", session)
                self.addCleanup(self.stop_process, overlapping)
                _, stderr = overlapping.communicate(timeout=5)
                self.assertEqual(overlapping.returncode, 1, stderr)
                self.assertIn("already has an active turn", stderr)
            held.send_signal(signum)
            _, stderr = held.communicate(timeout=5)
            if signum == signal.SIGINT:
                self.assertEqual(held.returncode, 130, stderr)
                self.assertIn("interrupted", stderr)
            deadline = time.monotonic() + 5
            with sqlite3.connect(self.home / "state.db") as db:
                while time.monotonic() < deadline:
                    if db.execute("SELECT count(*) FROM session_turn_leases").fetchone() == (0,):
                        break
                    time.sleep(0.02)
                else:
                    self.fail("owner did not release the interrupted turn")
                self.assertEqual(db.execute("SELECT count(*) FROM messages").fetchone(), (4,))
        self.assertIsNone(owner.poll())
        retry = self.chat("Continue", "--resume", session)
        self.addCleanup(self.stop_process, retry)
        _, stderr = retry.communicate(timeout=10)
        self.assertEqual(retry.returncode, 0, stderr)

    def test_owner_preserves_bursts_of_text_events(self):
        self.start_owner()
        process = self.chat("Return a burst of text")
        self.addCleanup(self.stop_process, process)
        stdout, stderr = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, stderr)
        self.assertEqual(stdout, "x" * 256 + "\n")

    def test_graceful_owner_restart_aborts_an_in_flight_turn(self):
        owner = self.start_owner()
        session = self.initial_session()
        client = self.begin_held_turn(session, 3)
        owner.terminate()
        owner.communicate(timeout=5)
        self.assertEqual(owner.returncode, 0)
        _, stderr = client.communicate(timeout=5)
        self.assertNotEqual(client.returncode, 0)
        self.assertIn("local owner disconnected", stderr)
        with sqlite3.connect(self.home / "state.db") as db:
            self.assertEqual(db.execute("SELECT count(*) FROM session_turn_leases").fetchone(), (0,))
            self.assertEqual(db.execute("SELECT count(*) FROM messages").fetchone(), (4,))
        self.start_owner()
        resumed = self.chat("Continue", "--resume", session)
        self.addCleanup(self.stop_process, resumed)
        _, stderr = resumed.communicate(timeout=10)
        self.assertEqual(resumed.returncode, 0, stderr)
