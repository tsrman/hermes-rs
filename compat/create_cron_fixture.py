#!/usr/bin/env python3
"""Record schedule parsing and next-run results from the Python reference.

Usage: create_cron_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter (needs croniter). The output is read by
crates/hermes-runtime/tests/cron_schedule.rs.
"""

import json
import os
import sys
import tempfile
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

SCHEDULES = [
    "30m", "every 30m", "every 2h", "every 1d", "hour", "every hour", "1d", "90 minutes", "every 5 mins",
    "every monday 9am", "every day at 9am", "weekdays at 9am", "every weekend 10:30pm",
    "monday, wednesday at 9am", "every mon and fri at 17:45", "every day at noon", "every day at midnight",
    "every tuesday at 12am", "every day at 13pm",
    "0 9 * * *", "*/15 * * * *", "0 9 * * mon-fri", "0 0 1 jan *", "0 9 */2 * 1", "0 9 1,15 * 1",
    "30 2 * * *", "30 1 * * *", "0 0 29 2 *", "0 12 * * 7", "5-10/2 * * * *", "0 0 * * 0", "15/20 8 * * *",
    "61 * * * *", "0 9 * * funday", "0 0 31 4 *",
    "2026-06-01T09:00:00Z", "2026-06-01T09:00:00", "2026-06-01T09:00", "2026-06-01", "2026-03-08T02:30:00",
    "2026-06-01T09:00:00+05:30", "2026-06-01T09:00:00.250000+00:00", "not-a-time", "in 30m", "in 2h", "in 1d", "in banana",
    "every", "every banana",
]
PROFILES = [
    ("America/Toronto", datetime(2026, 3, 7, 12, 0, 30)),
    ("Europe/London", datetime(2026, 11, 21, 9, 15, 0)),
]
RUNS = [
    # (profile zone, now wall clock, job timezone, last_run_at)
    ("America/Toronto", datetime(2026, 3, 8, 1, 30), None, None),
    ("America/Toronto", datetime(2026, 3, 7, 23, 59, 59), None, None),
    ("America/Toronto", datetime(2026, 11, 1, 0, 30), None, None),
    ("America/Toronto", datetime(2026, 11, 1, 1, 30), None, "2026-11-01T01:30:00-04:00"),
    ("America/Toronto", datetime(2026, 11, 1, 1, 45), None, "2026-11-01T01:30:00-05:00"),
    ("America/Toronto", datetime(2026, 11, 21, 9, 0), None, "2026-11-20T14:00:00+00:00"),
    ("Europe/London", datetime(2026, 3, 29, 0, 59), None, None),
    ("Europe/London", datetime(2026, 10, 25, 0, 30), None, None),
    ("Asia/Kolkata", datetime(2026, 6, 1, 8, 59, 59), None, None),
    ("America/Los_Angeles", datetime(2026, 6, 1, 0, 0), "America/New_York", None),
    ("America/New_York", datetime(2026, 6, 1, 0, 0), "Asia/Hong_Kong", "2026-06-01T01:00:00+00:00"),
    ("Australia/Lord_Howe", datetime(2026, 10, 3, 1, 59), None, None),
    ("UTC", datetime(2026, 2, 28, 23, 59, 59, 500000), None, None),
]
CRON_EXPRS = [
    "0 9 * * *", "30 2 * * *", "30 1 * * *", "*/15 * * * *", "0 9 * * mon-fri", "0 0 29 2 *", "0 0 1 * *",
    "0 */6 * * *", "45 23 31 * *", "0 9 * * 1", "0 9 15 * 1", "0 3 * * sun", "10 2 * * *", "59 23 * * *",
]
ONESHOTS = [
    # (run_at, job timezone, last_run_at, profile zone, now wall clock)
    ("2026-03-08T01:30:00-05:00", None, None, "UTC", datetime(2026, 3, 8, 6, 31, 0)),
    ("2026-03-08T01:30:00-05:00", None, None, "UTC", datetime(2026, 3, 8, 6, 32, 30)),
    ("2026-03-08T01:30:00-05:00", None, "2026-03-08T01:30:05-05:00", "UTC", datetime(2026, 3, 8, 6, 30, 30)),
    ("2026-06-01T09:00:00", "Asia/Hong_Kong", None, "UTC", datetime(2026, 6, 1, 1, 0, 30)),
    # Offset-less run_at without a pin means host-local time in the reference, so it is not recorded.
]


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    home = tempfile.mkdtemp()
    os.environ["HERMES_HOME"] = home
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import cron.jobs as jobs

    def use(zone, naive):
        os.environ["HERMES_TIMEZONE"] = zone
        # Reset the reference's per-process zone cache between profiles.
        import hermes_time
        hermes_time.reset_cache()
        # `naive` is the wall clock in `zone`; resolve it as that zone's first reading.
        now = naive.replace(tzinfo=ZoneInfo(zone))
        jobs._hermes_now = lambda: now
        return now

    cases = []
    for zone, naive in PROFILES:
        now = use(zone, naive)
        for text in SCHEDULES:
            case = {"op": "parse", "zone": zone, "now": now.isoformat(), "text": text}
            try:
                case["expect"] = jobs.parse_schedule(text)
            except ValueError:
                case["error"] = True
            cases.append(case)
    for zone, naive, job_zone, last in RUNS:
        now = use(zone, naive)
        schedules = [{"kind": "cron", "expr": e, "display": e} for e in CRON_EXPRS]
        schedules += [{"kind": "interval", "minutes": m, "display": f"every {m}m"} for m in (1, 90, 1440)]
        for schedule in schedules:
            expect = jobs.compute_next_run(schedule, last, timezone=job_zone)
            cases.append({"op": "next", "zone": zone, "now": now.isoformat(), "job_zone": job_zone,
                          "last_run_at": last, "schedule": schedule, "expect": expect})
    for run_at, job_zone, last, zone, naive in ONESHOTS:
        now = use(zone, naive)
        schedule = {"kind": "once", "run_at": run_at, "display": "once"}
        cases.append({"op": "next", "zone": zone, "now": now.isoformat(), "job_zone": job_zone,
                      "last_run_at": last, "schedule": schedule,
                      "expect": jobs.compute_next_run(schedule, last, timezone=job_zone)})
    Path(output).write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"{len(cases)} cases")


if __name__ == "__main__":
    main()
