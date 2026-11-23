#!/usr/bin/env python3
"""Record exact reference behaviors for cron remainder features into a Rust test fixture."""

import json
import os
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

REF_ROOT = Path("/home/tsr/ref/hermes-main")
sys.path.insert(0, str(REF_ROOT))

OUT_PATH = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "hermes-runtime"
    / "tests"
    / "fixtures"
    / "cron_remainder_cases.json"
)


def main():
    cases = {}

    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp) / "profile"
        (home / "cron").mkdir(parents=True)
        (home / "scripts").mkdir(parents=True)
        os.environ["HERMES_HOME"] = str(home)
        os.environ["HERMES_TIMEZONE"] = "UTC"

        # 1. Notepad
        from cron import notepad

        notepad_cases = {}
        notepad_cases["empty_render"] = notepad.render_notepad_section("job1")
        notepad.set_note("job1", "beta", "second")
        notepad.set_note("job1", "alpha", "first")
        notepad_cases["get_alpha"] = notepad.get_note("job1", "alpha")
        notepad_cases["get_missing"] = notepad.get_note("job1", "missing")
        notepad_cases["list_keys"] = [n["key"] for n in notepad.list_notes("job1")]
        notepad_cases["rendered"] = notepad.render_notepad_section("job1")
        notepad_cases["delete_alpha"] = notepad.delete_note("job1", "alpha")
        notepad_cases["delete_alpha_again"] = notepad.delete_note("job1", "alpha")

        # Validation errors
        errors = {}
        for label, fn in [
            ("empty_key", lambda: notepad.set_note("job1", "   ", "v")),
            ("long_key", lambda: notepad.set_note("job1", "k" * 129, "v")),
            ("big_value", lambda: notepad.set_note("job1", "k", "v" * (16 * 1024 + 1))),
        ]:
            try:
                fn()
            except ValueError as exc:
                errors[label] = str(exc)
        # Fill total budget to exceed 64 KiB
        for i in range(3):
            notepad.set_note("budget", f"k{i}", "x" * (16 * 1024))
        try:
            notepad.set_note("budget", "k3", "x" * (16 * 1024))
        except ValueError as exc:
            errors["total_budget"] = str(exc)
        notepad_cases["errors"] = errors
        cases["notepad"] = notepad_cases

        # 2. Incidents
        from cron import incidents

        inc_cases = {
            "normalize_and_sig": [],
            "classify": [],
            "lifecycle": {},
        }
        for job_id, err in [
            ("abcdef123456", "Timed out after 30.5s waiting for response"),
            ("abcdef123456", "Timed out after 120s waiting for response"),
            ("job99", "  HTTP 429   Too Many Requests  in  500ms  "),
            ("job99", "Script exited with code 2\nstderr: boom after 2 mins"),
        ]:
            norm = incidents._normalize_error(err)
            sig = incidents._error_signature(job_id, err)
            iid = incidents._incident_id(job_id, sig)
            inc_cases["normalize_and_sig"].append({
                "job_id": job_id,
                "error": err,
                "normalized": norm,
                "sig": sig,
                "incident_id": iid,
            })

        for err in [
            "HTTP 429 Too Many Requests",
            "quota exceeded for plan",
            "Request timed out after 60s",
            "401 Unauthorized: invalid api key",
            "Failed to deliver to telegram",
            "Invalid cron schedule config",
            "Script exited with code 1",
            "Agent loop error: tool failed",
            "Something completely unexpected happened",
        ]:
            inc_cases["classify"].append({
                "error": err,
                "failure_type": incidents._classify_failure_type(err),
            })

        # Lifecycle transitions
        iid1, new1 = incidents.upsert_incident("incjob1", "Timed out after 10s", output_file="/tmp/a.md")
        iid2, new2 = incidents.upsert_incident("incjob1", "Timed out after 45s", output_file="/tmp/b.md")
        inc_cases["lifecycle"]["first"] = {"id": iid1, "is_new": new1, "state": incidents.get_incident(iid1)["state"]}
        inc_cases["lifecycle"]["dedup"] = {"id": iid2, "is_new": new2, "state": incidents.get_incident(iid2)["state"]}
        incidents.set_incident_state(iid1, "alerted")
        inc_cases["lifecycle"]["after_alert"] = incidents.get_incident(iid1)["state"]
        resolved_count = incidents.close_incidents_for_recovered_job("incjob1")
        inc_cases["lifecycle"]["resolved_count"] = resolved_count
        inc_cases["lifecycle"]["after_recover"] = incidents.get_incident(iid1)["state"]
        # Re-open resolved on recurrence
        iid3, new3 = incidents.upsert_incident("incjob1", "Timed out after 99s")
        inc_cases["lifecycle"]["reopened"] = {"id": iid3, "is_new": new3, "state": incidents.get_incident(iid3)["state"]}
        # Ack -> closed (terminal)
        acked = incidents.ack_incident(iid1)
        inc_cases["lifecycle"]["acked"] = acked
        inc_cases["lifecycle"]["after_ack"] = incidents.get_incident(iid1)["state"]
        # Recurrence after ack stays closed
        iid4, new4 = incidents.upsert_incident("incjob1", "Timed out after 12s")
        inc_cases["lifecycle"]["after_ack_recur"] = {"id": iid4, "is_new": new4, "state": incidents.get_incident(iid4)["state"]}
        cases["incidents"] = inc_cases

        # 3. Monitor
        from cron import monitor

        mon_cases = {
            "hashes": [
                {"input": "", "hash": monitor.hash_monitor_output("")},
                {"input": "status: ok\n", "hash": monitor.hash_monitor_output("status: ok\n")},
                {"input": "привет мир", "hash": monitor.hash_monitor_output("привет мир")},
            ],
            "diff_simple": monitor.build_monitor_diff("line1\nline2\n", "line1\nline2-changed\n"),
            "diff_truncated": monitor.build_monitor_diff(
                "\n".join(f"old-{i:04d}" for i in range(400)),
                "\n".join(f"new-{i:04d}" for i in range(400)),
            ),
        }
        cases["monitor"] = mon_cases

        # 4. Wake gate & split_monitor_arg & script validation
        from cron.scheduler_prompt import _parse_wake_gate
        from cron.job_args import _split_monitor_arg, _validate_cron_script_path

        wake_cases = []
        for sample in [
            "",
            "hello world",
            '{"wakeAgent": false}',
            'log line\n{"wakeAgent": false}\n  ',
            '{"wakeAgent": true}',
            '{"wakeAgent": 0}',
            '{"wakeAgent": false}\nextra line',
            '[1, 2, 3]',
        ]:
            wake_cases.append({"input": sample, "wake": _parse_wake_gate(sample)})
        cases["wake_gate"] = wake_cases

        split_cases = []
        for args in [
            (None, None, None),
            (None, "a.py", None),
            ("", "a.py", "https://example.com"),
            ("https://example.com/status", None, None),
            ("HTTP://example.com/status", "old.py", None),
            ("check.sh", None, "https://old.example"),
        ]:
            ms, mu = _split_monitor_arg(*args)
            split_cases.append({"args": list(args), "monitor_script": ms, "monitor_url": mu})
        cases["split_monitor"] = split_cases

        (home / "scripts" / "ok.py").write_text("print('hi')\n", encoding="utf-8")
        script_val = {}
        for label, s in [
            ("valid", "ok.py"),
            ("abs", "/etc/passwd"),
            ("tilde", "~/foo.py"),
            ("drive", "C:\\foo.py"),
            ("traversal", "../outside.py"),
            ("missing", "nope.py"),
        ]:
            err = _validate_cron_script_path(s)
            if err is not None:
                err = err.replace(str(home), "<HOME>")
            script_val[label] = err
        cases["script_validation"] = script_val

        # 5. Doctor issues
        from hermes_cli.cron import _cron_doctor_issues_for_job
        import hermes_time

        now = hermes_time.now()
        doctor_inputs = [
            ("healthy", {
                "id": "j1", "name": "Healthy", "prompt": "hi", "enabled": True, "state": "scheduled",
                "schedule": {"kind": "cron", "expr": "0 9 * * *"},
                "next_run_at": (now + timedelta(hours=1)).isoformat(),
                "last_status": "ok",
            }),
            ("failed_and_overdue", {
                "id": "j2", "name": "Broken", "prompt": "hi", "enabled": True, "state": "scheduled",
                "schedule": {"kind": "cron", "expr": "0 9 * * *"},
                "next_run_at": (now - timedelta(minutes=30)).isoformat(),
                "last_status": "error", "last_error": "boom",
                "last_delivery_error": "send failed",
                "last_fire_error": {"at": "2026-11-23T11:00:00+00:00", "detail": "fire boom"},
            }),
            ("no_agent_no_script", {
                "id": "j3", "name": "NoScript", "prompt": "", "enabled": True, "state": "scheduled",
                "no_agent": True,
                "schedule": {"kind": "interval", "minutes": 5},
                "next_run_at": (now + timedelta(minutes=5)).isoformat(),
            }),
            ("bad_workdir_and_tz", {
                "id": "j4", "name": "BadCfg", "prompt": "hi", "enabled": True, "state": "scheduled",
                "workdir": str(home / "does_not_exist"),
                "timezone": "Mars/Base",
                "grace_minutes": 10,
                "schedule": {"kind": "interval", "minutes": 5},
                "next_run_at": None,
            }),
            ("script_missing", {
                "id": "j5", "name": "MissingScript", "prompt": "hi", "enabled": True, "state": "scheduled",
                "script": "missing_script.py",
                "schedule": {"kind": "cron", "expr": "0 9 * * *"},
                "next_run_at": (now + timedelta(hours=1)).isoformat(),
            }),
        ]
        doctor_cases = []
        for label, job in doctor_inputs:
            issues = _cron_doctor_issues_for_job(job)
            issues = [i.replace(str(home), "<HOME>") for i in issues]
            # Normalize relative timestamps for test stability
            job_norm = json.loads(json.dumps(job).replace(str(home), "<HOME>"))
            doctor_cases.append({"label": label, "job": job_norm, "issues": issues})
        cases["doctor"] = {
            "now": now.isoformat(),
            "cases": doctor_cases,
        }

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(cases, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Wrote {OUT_PATH}")


if __name__ == "__main__":
    main()
