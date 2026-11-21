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


if __name__ == "__main__":
    unittest.main()
