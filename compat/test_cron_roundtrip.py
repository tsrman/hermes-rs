"""`jobs.json` round trip between this CLI and the Python reference, both ways, on a live store.

Needs the reference checkout and its interpreter (HERMES_REFERENCE, default ~/ref/hermes-main, and
HERMES_REFERENCE_PYTHON, default ~/ref/hermes-reference-venv/bin/python); the tests are skipped
without them. compat/create_cron_jobs_fixture.py records the same behaviour with a pinned clock
for CI.
"""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug/hermes"
REFERENCE = Path(os.environ.get("HERMES_REFERENCE", Path.home() / "ref/hermes-main"))
PYTHON = Path(os.environ.get("HERMES_REFERENCE_PYTHON", Path.home() / "ref/hermes-reference-venv/bin/python"))

# Runs inside the reference: opens the store the way the gateway does, and reports whether doing so
# changed it (an auto-repair rewrites the file) together with what it loaded.
PRELUDE = """
import json, logging, os, sys
sys.path.insert(0, os.getcwd())
import hermes_time
hermes_time.reset_cache()
import cron.jobs as jobs

class Capture(logging.Handler):
    def __init__(self):
        super().__init__()
        self.lines = []
    def emit(self, record):
        self.lines.append(record.getMessage())

capture = Capture()
logging.getLogger().addHandler(capture)
logging.getLogger().setLevel(logging.DEBUG)
path = jobs._current_cron_store().jobs_file

def open_store():
    before = path.read_bytes()
    loaded = jobs.load_jobs()
    return {"unchanged": path.read_bytes() == before, "loaded": loaded,
            "on_disk": json.loads(before.decode("utf-8-sig"))["jobs"],
            "repairs": [line for line in capture.lines if "repair" in line.lower()]}
"""


@unittest.skipUnless(REFERENCE.is_dir() and PYTHON.exists() and BINARY.exists(),
                     "the Python reference or the built CLI is not available")
class RoundTripTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name) / "profile"
        self.home.mkdir()
        self.env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home), "HERMES_TIMEZONE": "UTC",
                    "HERMES_API_KEY": "fixture"}

    def hermes(self, *arguments, expect=0):
        result = subprocess.run([str(BINARY), "cron", *arguments], env=self.env, capture_output=True, text=True,
                                timeout=30)
        self.assertEqual(result.returncode, expect, result.stderr)
        return json.loads(result.stdout) if result.stdout.strip() else None

    def reference(self, code):
        script = PRELUDE + code
        result = subprocess.run([str(PYTHON), "-I", "-c", script], env={**self.env, "PYTHONPATH": ""},
                                cwd=REFERENCE, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr[-2000:])
        return json.loads(result.stdout.strip().splitlines()[-1])

    def assert_reference_accepts_the_store(self):
        """The reference loads the store without repairing it, and reads every job as it is on disk."""
        report = self.reference("print(json.dumps(open_store()))")
        self.assertTrue(report["unchanged"], "the reference rewrote the store")
        self.assertEqual(report["repairs"], [])
        self.assertEqual(report["loaded"], report["on_disk"])
        return report["loaded"]

    def test_what_this_cli_writes_the_reference_reads_and_updates(self):
        self.hermes("add", "every 2h", "Summarise the inbox", "--name", "Inbox", "--repeat", "3", "--model", "m1")
        self.hermes("add", "0 9 * * *", "Morning brief", "--timezone", "Asia/Hong_Kong", "--grace-minutes", "10",
                    "--catch-up", "on", "--no-wrap-response", "--deliver", "local")
        self.hermes("add", "in 2h", "Remind me", "--paused", "--paused-reason", "later")
        jobs = self.assert_reference_accepts_the_store()
        self.assertEqual([job["name"] for job in jobs], ["Inbox", "Morning brief", "Remind me"])
        inbox, brief, remind = jobs
        self.assertRegex(inbox["id"], r"^[0-9a-f]{12}$")
        self.assertEqual((brief["timezone"], brief["grace_minutes"], brief["catch_up"]), ("Asia/Hong_Kong", 10, True))
        self.assertEqual((remind["enabled"], remind["state"], remind["paused_reason"]), (False, "paused", "later"))

        self.hermes("edit", "inbox", "--schedule", "every 3h", "--name", "Inbox 3h")
        self.hermes("pause", "Morning brief")
        self.hermes("resume", "morning brief")
        self.assert_reference_accepts_the_store()

        # The reference looks the jobs up by name, lists them, and edits them.
        listing = self.reference("""
store = open_store()
listed = jobs.list_jobs(include_disabled=True)
by_name = jobs.resolve_job_ref("INBOX 3H")
updated = jobs.update_job(by_name["id"], {"name": "Inbox, python"})
paused = jobs.pause_job("Morning brief", reason="from python")
resumed = jobs.resume_job("Morning brief")
removed = jobs.remove_job("Remind me")
print(json.dumps({"listed": [j["name"] for j in listed], "found": by_name["schedule_display"],
                  "updated": updated["name"], "paused": paused["state"], "resumed": resumed["state"],
                  "removed": removed, "repairs": open_store()["repairs"]}))
""")
        self.assertEqual(listing["listed"], ["Inbox 3h", "Morning brief", "Remind me"])
        self.assertEqual(listing["found"], "every 180m")
        self.assertEqual((listing["updated"], listing["paused"], listing["resumed"], listing["removed"]),
                         ("Inbox, python", "paused", "scheduled", True))
        self.assertEqual(listing["repairs"], [])
        # And this CLI reads what the reference wrote back.
        after = self.hermes("list", "--all")
        self.assertEqual([job["name"] for job in after], ["Inbox, python", "Morning brief"])
        self.assertEqual([job["state"] for job in after], ["scheduled", "scheduled"])

    def test_what_the_reference_writes_this_cli_reads_and_updates(self):
        self.reference("""
jobs.create_job("Check the build", "30m", name="Builds", repeat=5)
jobs.create_job("Brief", "0 9 * * *", name="Brief", timezone="Europe/London", grace_minutes=20, catch_up=False)
jobs.create_job("Remind", "in 3h", name="Reminder")
jobs.create_job("Held", "1d", name="Held", paused=True, paused_reason="review")
print(json.dumps(open_store()["repairs"]))
""")
        listed = self.hermes("list", "--all")
        self.assertEqual([job["name"] for job in listed], ["Builds", "Brief", "Reminder", "Held"])
        self.assertEqual([job["state"] for job in listed], ["scheduled", "scheduled", "scheduled", "paused"])
        self.assertEqual(listed[1]["timezone"], "Europe/London")
        self.hermes("edit", "builds", "--prompt", "Check the build and the tests", "--repeat", "0")
        self.hermes("edit", "BRIEF", "--grace-minutes", "", "--timezone", "")
        self.hermes("pause", "Builds")
        self.hermes("resume", "Builds")
        self.hermes("resume", "Held")
        self.hermes("remove", "Reminder")
        jobs = self.assert_reference_accepts_the_store()
        builds, brief, held = jobs
        self.assertEqual((builds["prompt"], builds["repeat"], builds["state"]),
                         ("Check the build and the tests", {"times": None, "completed": 0}, "scheduled"))
        self.assertNotIn("grace_minutes", brief)
        self.assertNotIn("timezone", brief)
        self.assertEqual((held["enabled"], held["state"], held["paused_reason"]), (True, "scheduled", None))
        # The reference still edits a record that this CLI rewrote.
        updated = self.reference("""
updated = jobs.update_job(jobs.resolve_job_ref("Held")["id"], {"prompt": "Held, edited"})
print(json.dumps({"prompt": updated["prompt"], "repairs": open_store()["repairs"]}))
""")
        self.assertEqual(updated, {"prompt": "Held, edited", "repairs": []})

    def test_a_job_finished_by_this_cli_is_read_as_the_reference_reads_its_own(self):
        """`run` books the outcome in the reference's fields: status, streak, claims and next run."""
        server = subprocess.Popen(["python3", str(ROOT / "compat" / "model_server.py"), "--scenario", "text"],
                                  stdout=subprocess.PIPE, text=True)
        self.addCleanup(server.stdout.close)
        self.addCleanup(server.wait)
        self.addCleanup(server.kill)
        base = server.stdout.readline().strip()
        (self.home / "config.yaml").write_text(
            f"model:\n  default: primary\n  provider: custom\n  base_url: {base}/v1\ntimezone: UTC\n", encoding="utf-8")
        self.hermes("add", "every 2h", "Report", "--name", "Report", "--repeat", "2")
        report = self.hermes("run", "Report")
        self.assertTrue(report["success"])
        self.assert_reference_accepts_the_store()
        status = self.reference("""
job = jobs.resolve_job_ref("Report")
print(json.dumps({"status": job["last_status"], "claim": job.get("fire_claim"), "repeat": job["repeat"],
                  "runnable": jobs.is_job_runnable(job), "state": job["state"]}))
""")
        self.assertEqual(status["status"], "ok")
        self.assertIsNone(status["claim"])
        self.assertEqual((status["repeat"]["completed"], status["runnable"], status["state"]), (0, True, "scheduled"))

    def test_the_reference_refuses_what_this_cli_refuses_to_finish(self):
        """A finished one-shot cannot be paused or resumed by either side, and both say why."""
        self.reference("""
jobs.create_job("Once", "in 30m", name="Once")
job = jobs.resolve_job_ref("Once")
jobs.update_job(job["id"], {"enabled": False, "state": "completed", "next_run_at": None})
print(json.dumps(open_store()["repairs"]))
""")
        message = ("Cannot activate terminal cron job 'Once' through update_job; "
                   "use cron resume --run-now or --at.")
        for command in ("pause", "resume"):
            result = subprocess.run([str(BINARY), "cron", command, "Once"], env=self.env, capture_output=True,
                                    text=True, timeout=15)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(message, result.stderr)
        raised = self.reference("""
try:
    jobs.pause_job("Once")
    error = None
except ValueError as exc:
    error = str(exc)
print(json.dumps(error))
""")
        self.assertEqual(raised, message)
        # Re-arming is the way back, and the reference accepts the result.
        self.hermes("resume", "Once", "--at", "in 10m")
        (job,) = self.assert_reference_accepts_the_store()
        self.assertEqual((job["enabled"], job["state"]), (True, "scheduled"))


if __name__ == "__main__":
    unittest.main()
