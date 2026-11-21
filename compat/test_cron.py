import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug/hermes"


class CronCliTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name) / "profile"
        (self.home / "cron").mkdir(parents=True)

    def hermes(self, *arguments, timezone=None):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home)}
        if timezone:
            env["HERMES_TIMEZONE"] = timezone
        return subprocess.run([str(BINARY), "cron", *arguments], env=env, capture_output=True,
                              text=True, timeout=15)

    def read_json(self, *arguments, **options):
        result = self.hermes(*arguments, **options)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def write_jobs(self, payload, prefix=""):
        (self.home / "cron" / "jobs.json").write_text(prefix + json.dumps(payload), encoding="utf-8-sig")

    def test_preview_uses_the_profile_zone_across_a_dst_change(self):
        result = self.read_json("preview", "30 9 * * *", "--count", "2", timezone="America/Toronto")
        self.assertEqual(result["timezone"], "America/Toronto")
        self.assertEqual(result["schedule"], {"kind": "cron", "expr": "30 9 * * *", "display": "30 9 * * *"})
        first, second = result["next_runs"]
        self.assertTrue(first.endswith(("-04:00", "-05:00")), first)
        self.assertEqual((first[11:16], second[11:16]), ("09:30", "09:30"))

    def test_preview_pins_a_job_zone_and_stops_after_a_one_shot(self):
        pinned = self.read_json("preview", "0 9 * * *", "--timezone", "Asia/Hong_Kong", timezone="UTC")
        self.assertTrue(all(run.endswith("T09:00:00+08:00") for run in pinned["next_runs"]))
        once = self.read_json("preview", "in 30m", "--count", "5", timezone="UTC")
        self.assertEqual(len(once["next_runs"]), 1)

    def test_preview_rejects_unsupported_input(self):
        for arguments in (("not a schedule",), ("0 9 * * * *",), ("0 9 * * *", "--timezone", "Mars/Base")):
            result = self.hermes("preview", *arguments)
            self.assertNotEqual(result.returncode, 0, arguments)
            self.assertEqual(result.stdout, "")

    def test_list_reads_the_reference_store_without_rewriting_it(self):
        self.write_jobs({"jobs": [
            {"id": "daily", "name": "Daily", "schedule": {"kind": "cron", "expr": "0 9 * * *", "display": "0 9 * * *"},
             "enabled": True, "state": "scheduled", "next_run_at": "2026-11-22T09:00:00+00:00",
             "last_run_at": "2026-11-21T09:00:00+00:00", "timezone": "Europe/London"},
            {"id": "paused", "prompt": "Check the build " + "x" * 80, "schedule": {"kind": "interval", "minutes": 30},
             "enabled": False, "state": "paused"},
            {"id": "done", "schedule_display": "once at 2026-01-01 09:00", "enabled": False, "state": "completed"},
            "junk",
        ]}, prefix="")
        path = self.home / "cron" / "jobs.json"
        before = path.read_bytes()
        active = self.read_json("list")
        self.assertEqual([job["id"] for job in active], ["daily"])
        self.assertEqual(active[0]["schedule"], "0 9 * * *")
        self.assertEqual(active[0]["timezone"], "Europe/London")
        everything = self.read_json("list", "--all")
        self.assertEqual([job["id"] for job in everything], ["daily", "paused", "done"])
        self.assertEqual(everything[1]["name"], ("Check the build " + "x" * 80)[:50])
        self.assertEqual(everything[1]["schedule"], "?")
        self.assertEqual([job["state"] for job in everything], ["scheduled", "paused", "completed"])
        self.assertEqual(path.read_bytes(), before)

    def test_list_accepts_bare_lists_id_keyed_maps_and_a_bom(self):
        self.write_jobs([{"id": "a", "schedule_display": "every 5m"}])
        self.assertEqual(self.read_json("list")[0]["id"], "a")
        self.write_jobs({"jobs": {"key-id": {"name": "Keyed", "schedule_display": "30m"},
                                  "x": {"id": "inline", "schedule_display": "1h"}, "bad": 3}})
        self.assertEqual([job["id"] for job in self.read_json("list")], ["key-id", "inline"])
        path = self.home / "cron" / "jobs.json"
        path.write_text("﻿" + json.dumps({"jobs": []}), encoding="utf-8")
        self.assertEqual(self.read_json("list"), [])

    def test_list_handles_missing_and_corrupt_stores(self):
        self.assertEqual(self.read_json("list"), [])
        (self.home / "cron" / "jobs.json").write_text("{broken", encoding="utf-8")
        result = self.hermes("list")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not valid JSON", result.stderr)


@unittest.skipUnless(os.name == "posix", "cron job changes require Unix locking")
class CronWriteTest(CronCliTest):
    def jobs(self):
        raw = (self.home / "cron" / "jobs.json").read_text(encoding="utf-8-sig")
        return raw, json.loads(raw, object_pairs_hook=dict)

    def seed(self, *extra):
        self.write_jobs({"jobs": [
            {"id": "daily", "name": "Привет", "prompt": "p", "custom": {"keep": [1, 2.5, None]},
             "schedule": {"kind": "cron", "expr": "0 9 * * *", "display": "0 9 * * *"},
             "enabled": True, "state": "scheduled", "next_run_at": "2030-01-01T09:00:00+00:00",
             "pending_slot": {"at": "x"}, "paused_at": None},
            {"id": "other", "name": "Other", "schedule": {"kind": "interval", "minutes": 30, "display": "every 30m"},
             "enabled": True, "state": "scheduled", "next_run_at": "2030-01-01T00:30:00+00:00"},
            *extra,
        ]})

    def test_pause_updates_only_the_lifecycle_fields(self):
        self.seed()
        _, before = self.jobs()
        result = self.read_json("pause", "привет", "--reason", "maintenance", timezone="UTC")
        self.assertEqual((result["id"], result["state"], result["enabled"]), ("daily", "paused", False))
        raw, after = self.jobs()
        job = after["jobs"][0]
        self.assertEqual((job["enabled"], job["state"], job["paused_reason"]), (False, "paused", "maintenance"))
        self.assertTrue(job["paused_at"].endswith("+00:00"))
        self.assertNotIn("pending_slot", job)
        self.assertEqual(job["custom"], {"keep": [1, 2.5, None]})
        self.assertEqual(list(job)[:5], list(before["jobs"][0])[:5])
        self.assertEqual(after["jobs"][1], before["jobs"][1])
        self.assertIn("updated_at", after)
        self.assertTrue(raw.startswith('{\n  "jobs": [\n    {\n      "id": "daily"'))
        self.assertIn("Привет", raw)
        self.assertEqual((self.home / "cron" / "jobs.json").stat().st_mode & 0o777, 0o600)

    def test_resume_recomputes_a_future_slot_and_keeps_an_elapsed_one(self):
        self.seed()
        self.hermes("pause", "daily")
        resumed = self.read_json("resume", "daily", timezone="UTC")
        self.assertEqual((resumed["state"], resumed["enabled"]), ("scheduled", True))
        _, data = self.jobs()
        job = data["jobs"][0]
        self.assertRegex(job["next_run_at"], r"T09:00:00\+00:00$")
        self.assertIsNone(job["paused_at"])
        self.assertIsNone(job["paused_reason"])
        # An elapsed recurring slot stays due for the scheduler's catch-up policy.
        job["next_run_at"] = "2020-01-01T09:00:00+00:00"
        self.write_jobs(data)
        self.hermes("pause", "daily")
        self.assertEqual(self.read_json("resume", "daily")["next_run_at"], "2020-01-01T09:00:00+00:00")

    def test_resume_rejects_a_one_shot_in_the_past(self):
        self.seed({"id": "once", "name": "Once", "schedule": {"kind": "once", "run_at": "2020-01-01T00:00:00+00:00"},
                   "enabled": False, "state": "paused", "next_run_at": None})
        result = self.hermes("resume", "once")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("in the past", result.stderr)
        self.assertEqual(self.jobs()[1]["jobs"][2]["state"], "paused")

    def test_remove_deletes_the_job_and_its_output_by_id_or_name(self):
        self.seed()
        output = self.home / "cron" / "output" / "other"
        output.mkdir(parents=True)
        (output / "run.md").write_text("result", encoding="utf-8")
        self.assertEqual(self.read_json("remove", "OTHER"), {"removed": "other"})
        self.assertFalse(output.exists())
        self.assertEqual([job["id"] for job in self.jobs()[1]["jobs"]], ["daily"])
        self.assertNotEqual(self.hermes("remove", "missing").returncode, 0)

    def test_ambiguous_names_and_terminal_jobs_are_refused_without_changes(self):
        self.seed({"id": "twin", "name": "other", "schedule": {"kind": "interval", "minutes": 5},
                   "enabled": False, "state": "completed"})
        before = (self.home / "cron" / "jobs.json").read_bytes()
        ambiguous = self.hermes("pause", "Other")
        self.assertIn("ambiguous", ambiguous.stderr)
        terminal = self.hermes("pause", "twin")
        self.assertIn("terminal state", terminal.stderr)
        escape = self.hermes("remove", "x")
        self.assertNotEqual(escape.returncode, 0)
        self.assertEqual((self.home / "cron" / "jobs.json").read_bytes(), before)

    def test_unsupported_layouts_are_not_rewritten(self):
        for payload in ([{"id": "a", "name": "a"}], {"jobs": {"a": {"name": "a"}}}, {"jobs": [{"id": "a", "name": "a"}, 3]}):
            self.write_jobs(payload)
            before = (self.home / "cron" / "jobs.json").read_bytes()
            result = self.hermes("pause", "a")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("layout", result.stderr)
            self.assertEqual((self.home / "cron" / "jobs.json").read_bytes(), before)

    def test_concurrent_changes_do_not_lose_updates(self):
        self.write_jobs({"jobs": [
            {"id": f"job{i}", "name": f"job{i}", "schedule": {"kind": "interval", "minutes": 5},
             "enabled": True, "state": "scheduled", "next_run_at": "2030-01-01T00:00:00+00:00"} for i in range(8)]})
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home)}
        processes = [subprocess.Popen([str(BINARY), "cron", "pause", f"job{i}"], env=env,
                                      stdout=subprocess.DEVNULL) for i in range(8)]
        self.assertEqual([process.wait(timeout=30) for process in processes], [0] * 8)
        self.assertEqual({job["state"] for job in self.jobs()[1]["jobs"]}, {"paused"})


if __name__ == "__main__":
    unittest.main()
