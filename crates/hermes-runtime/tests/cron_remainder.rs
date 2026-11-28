use chrono::DateTime;
use chrono_tz::Tz;
use hermes_runtime::cron::doctor;
use hermes_runtime::cron::incidents;
use hermes_runtime::cron::manage::{self, AddParams};
use hermes_runtime::cron::monitor;
use hermes_runtime::cron::notepad;
use hermes_runtime::cron::script;
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/cron_remainder_cases.json"))
        .expect("valid cron_remainder_cases.json")
}

fn utc_now() -> DateTime<Tz> {
    DateTime::parse_from_rfc3339("2026-11-23T12:00:00+00:00")
        .unwrap()
        .with_timezone(&Tz::UTC)
}

#[test]
fn notepad_matches_reference_corpus() {
    let cases = &fixture()["notepad"];
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let now = utc_now();

    assert_eq!(
        notepad::render_notepad_section(home, "job1"),
        cases["empty_render"].as_str().unwrap()
    );
    notepad::set_note(home, "job1", "beta", "second", now).unwrap();
    notepad::set_note(home, "job1", "alpha", "first", now).unwrap();

    assert_eq!(
        notepad::get_note(home, "job1", "alpha").unwrap().as_deref(),
        cases["get_alpha"].as_str()
    );
    assert_eq!(notepad::get_note(home, "job1", "missing").unwrap(), None);
    let keys: Vec<String> = notepad::list_notes(home, "job1")
        .unwrap()
        .into_iter()
        .map(|n| n.key)
        .collect();
    let expected_keys: Vec<String> = cases["list_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(keys, expected_keys);
    assert_eq!(
        notepad::render_notepad_section(home, "job1"),
        cases["rendered"].as_str().unwrap()
    );
    assert_eq!(
        notepad::delete_note(home, "job1", "alpha").unwrap(),
        cases["delete_alpha"].as_bool().unwrap()
    );
    assert_eq!(
        notepad::delete_note(home, "job1", "alpha").unwrap(),
        cases["delete_alpha_again"].as_bool().unwrap()
    );

    // Validation error messages match reference verbatim.
    let errs = &cases["errors"];
    assert_eq!(
        notepad::set_note(home, "job1", &"k".repeat(129), "v", now).unwrap_err(),
        errs["long_key"].as_str().unwrap()
    );
    assert_eq!(
        notepad::set_note(home, "job1", "k", &"v".repeat(16 * 1024 + 1), now).unwrap_err(),
        errs["big_value"].as_str().unwrap()
    );
    for i in 0..3 {
        notepad::set_note(
            home,
            "budget",
            &format!("k{i}"),
            &"x".repeat(16 * 1024),
            now,
        )
        .unwrap();
    }
    assert_eq!(
        notepad::set_note(home, "budget", "k3", &"x".repeat(16 * 1024), now).unwrap_err(),
        errs["total_budget"].as_str().unwrap()
    );
}

#[test]
fn incidents_match_reference_corpus() {
    let cases = &fixture()["incidents"];
    for row in cases["normalize_and_sig"].as_array().unwrap() {
        let job_id = row["job_id"].as_str().unwrap();
        let err = row["error"].as_str().unwrap();
        assert_eq!(
            incidents::normalize_error(err),
            row["normalized"].as_str().unwrap()
        );
        let sig = incidents::error_signature(job_id, err);
        assert_eq!(sig, row["sig"].as_str().unwrap());
        assert_eq!(
            incidents::incident_id(job_id, &sig),
            row["incident_id"].as_str().unwrap()
        );
    }
    for row in cases["classify"].as_array().unwrap() {
        let err = row["error"].as_str().unwrap();
        assert_eq!(
            incidents::classify_failure_type(err),
            row["failure_type"].as_str().unwrap(),
            "error: {err}"
        );
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let now = utc_now();
    let lc = &cases["lifecycle"];

    let (iid1, new1) = incidents::upsert_incident(
        home,
        "incjob1",
        "Timed out after 10s",
        Some("/tmp/a.md"),
        now,
    )
    .unwrap();
    assert_eq!(iid1, lc["first"]["id"].as_str().unwrap());
    assert_eq!(new1, lc["first"]["is_new"].as_bool().unwrap());
    assert_eq!(
        incidents::get_incident(home, &iid1).unwrap().unwrap().state,
        lc["first"]["state"].as_str().unwrap()
    );

    let (iid2, new2) = incidents::upsert_incident(
        home,
        "incjob1",
        "Timed out after 45s",
        Some("/tmp/b.md"),
        now,
    )
    .unwrap();
    assert_eq!(iid2, lc["dedup"]["id"].as_str().unwrap());
    assert_eq!(new2, lc["dedup"]["is_new"].as_bool().unwrap());

    incidents::set_incident_state(home, &iid1, "alerted", now).unwrap();
    assert_eq!(
        incidents::get_incident(home, &iid1).unwrap().unwrap().state,
        lc["after_alert"].as_str().unwrap()
    );

    let resolved = incidents::close_incidents_for_recovered_job(home, "incjob1", now).unwrap();
    assert_eq!(resolved, lc["resolved_count"].as_u64().unwrap() as usize);
    assert_eq!(
        incidents::get_incident(home, &iid1).unwrap().unwrap().state,
        lc["after_recover"].as_str().unwrap()
    );

    let (iid3, new3) =
        incidents::upsert_incident(home, "incjob1", "Timed out after 99s", None, now).unwrap();
    assert_eq!(iid3, lc["reopened"]["id"].as_str().unwrap());
    assert_eq!(new3, lc["reopened"]["is_new"].as_bool().unwrap());
    assert_eq!(
        incidents::get_incident(home, &iid3).unwrap().unwrap().state,
        lc["reopened"]["state"].as_str().unwrap()
    );

    let acked = incidents::ack_incident(home, &iid1, now).unwrap();
    assert_eq!(acked, lc["acked"].as_bool().unwrap());
    assert_eq!(
        incidents::get_incident(home, &iid1).unwrap().unwrap().state,
        lc["after_ack"].as_str().unwrap()
    );

    let (iid4, new4) =
        incidents::upsert_incident(home, "incjob1", "Timed out after 12s", None, now).unwrap();
    assert_eq!(iid4, lc["after_ack_recur"]["id"].as_str().unwrap());
    assert_eq!(new4, lc["after_ack_recur"]["is_new"].as_bool().unwrap());
    assert_eq!(
        incidents::get_incident(home, &iid4).unwrap().unwrap().state,
        lc["after_ack_recur"]["state"].as_str().unwrap()
    );
}

#[test]
fn monitor_and_wake_gate_and_script_validation_match_reference_corpus() {
    let data = fixture();
    for row in data["monitor"]["hashes"].as_array().unwrap() {
        assert_eq!(
            monitor::hash_monitor_output(row["input"].as_str().unwrap()),
            row["hash"].as_str().unwrap()
        );
    }
    assert_eq!(
        monitor::build_monitor_diff(
            "line1\nline2\n",
            "line1\nline2-changed\n",
            monitor::MAX_DIFF_CHARS
        ),
        data["monitor"]["diff_simple"].as_str().unwrap()
    );
    let old_big: String = (0..400)
        .map(|i| format!("old-{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    let new_big: String = (0..400)
        .map(|i| format!("new-{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        monitor::build_monitor_diff(&old_big, &new_big, monitor::MAX_DIFF_CHARS),
        data["monitor"]["diff_truncated"].as_str().unwrap()
    );

    for row in data["wake_gate"].as_array().unwrap() {
        assert_eq!(
            script::parse_wake_gate(row["input"].as_str().unwrap()),
            row["wake"].as_bool().unwrap()
        );
    }

    for row in data["split_monitor"].as_array().unwrap() {
        let args = row["args"].as_array().unwrap();
        let (ms, mu) =
            monitor::split_monitor_arg(args[0].as_str(), args[1].as_str(), args[2].as_str());
        assert_eq!(ms.as_deref(), row["monitor_script"].as_str());
        assert_eq!(mu.as_deref(), row["monitor_url"].as_str());
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join("scripts")).unwrap();
    std::fs::write(home.join("scripts/ok.py"), "print('hi')\n").unwrap();
    let sv = &data["script_validation"];
    for (label, input) in [
        ("valid", "ok.py"),
        ("abs", "/etc/passwd"),
        ("tilde", "~/foo.py"),
        ("drive", "C:\\foo.py"),
        ("traversal", "../outside.py"),
        ("missing", "nope.py"),
    ] {
        let res = script::validate_script_path_for_create(home, input);
        match sv[label].as_str() {
            None => assert!(res.is_ok(), "{label}: {res:?}"),
            Some(expected) => {
                let actual = res
                    .unwrap_err()
                    .replace(&home.to_string_lossy().to_string(), "<HOME>");
                // Windows joins the scripts directory with `\`; the reference output uses `/`.
                let actual = hermes_runtime::paths::forward_slashes_after(&actual, "<HOME>");
                assert_eq!(actual, expected, "{label}");
            }
        }
    }
}

#[test]
fn doctor_matches_reference_corpus() {
    let data = &fixture()["doctor"];
    let now = DateTime::parse_from_rfc3339(data["now"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Tz::UTC);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join("scripts")).unwrap();
    let home_str = home.to_string_lossy().to_string();
    let canon_home = hermes_runtime::paths::canonical(home)
        .unwrap_or_else(|_| home.to_path_buf())
        .to_string_lossy()
        .to_string();

    for case in data["cases"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        // The path is substituted into JSON text, so its backslashes (Windows) must be escaped.
        let json_home = serde_json::to_string(&home_str).unwrap();
        let raw_job = case["job"]
            .to_string()
            .replace("<HOME>", &json_home[1..json_home.len() - 1]);
        let job: Value = serde_json::from_str(&raw_job).unwrap();
        let actual: Vec<String> = doctor::doctor_issues_for_job(home, &job, now)
            .into_iter()
            .map(|s| {
                let s = s
                    .replace(&canon_home, "<HOME>")
                    .replace(&home_str, "<HOME>");
                // Windows joins paths with `\`; the reference output uses `/`.
                hermes_runtime::paths::forward_slashes_after(&s, "<HOME>")
            })
            .collect();
        let expected: Vec<String> = case["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(actual, expected, "doctor case {label}");
    }
}

#[tokio::test]
async fn script_and_monitor_execution_and_ssrf_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let now = utc_now();
    std::fs::create_dir_all(home.join("scripts")).unwrap();
    std::fs::write(home.join("scripts/hello.sh"), "echo hello-from-script\n").unwrap();

    let (ok, out) = script::run_job_script(home, "hello.sh", None, None, 10, || None).await;
    assert!(ok, "{out}");
    assert_eq!(out, "hello-from-script");

    {
        // Monitor baseline -> unchanged -> changed.
        let state_file = tmp.path().join("state.txt");
        std::fs::write(&state_file, "v1").unwrap();
        std::fs::write(
            home.join("scripts/mon.sh"),
            // bash reads the path, so it is spelled with `/` on Windows too.
            format!(
                "cat '{}'\n",
                state_file.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        let job = manage::add_job_with_id(
            home,
            now,
            "monjob",
            &AddParams {
                schedule: "30m".into(),
                prompt: Some("Watch state".into()),
                monitor_script: Some("mon.sh".into()),
                ..AddParams::default()
            },
        )
        .unwrap();

        let first = monitor::check_monitor(home, &job, now, || None).await;
        assert!(first.ok && first.changed && first.first_run);
        assert!(first.context_block.contains("Monitor Baseline (first run)"));
        assert!(first.context_block.contains("v1"));

        let updated = manage::find_job(home, now, "monjob").unwrap();
        let second = monitor::check_monitor(home, &updated, now, || None).await;
        assert!(second.ok && !second.changed);

        std::fs::write(&state_file, "v2").unwrap();
        let third = monitor::check_monitor(home, &updated, now, || None).await;
        assert!(third.ok && third.changed && !third.first_run);
        assert!(third.context_block.contains("MONITOR CHANGE DETECTED"));
        assert!(third.context_block.contains("-v1"));
        assert!(third.context_block.contains("+v2"));
    }

    // Monitor URL blocks loopback SSRF by default.
    let ssrf_job = json!({"id": "ssrf", "monitor_url": "http://127.0.0.1:1/secret"});
    let blocked = monitor::check_monitor(home, &ssrf_job, now, || None).await;
    assert!(!blocked.ok);
    assert!(blocked.error.unwrap().contains("private/loopback"));

    // Monitor URL enforces website_policy blocklist.
    std::fs::write(
        home.join("config.yaml"),
        "security:\n  website_blocklist:\n    enabled: true\n    domains:\n      - blocked.example.com\n",
    )
    .unwrap();
    let pol_job = json!({"id": "pol", "monitor_url": "https://blocked.example.com/status"});
    let pol_blocked = monitor::check_monitor(home, &pol_job, now, || None).await;
    assert!(!pol_blocked.ok);
    assert!(pol_blocked.error.unwrap().contains("blocked.example.com"));
}

#[test]
fn executions_cancel_and_cronjob_tool_roundtrip() {
    use hermes_runtime::cron::{cancel, executions};
    use hermes_runtime::cronjob_tool;
    use std::path::Path;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let now = utc_now();

    // Create via cronjob_manage tool (and verify model is NOT forwarded from model args).
    let created = cronjob_tool::call(
        home,
        &json!({
            "action": "create",
            "schedule": "30m",
            "prompt": "Check build status",
            "name": "BuildCheck",
            "model": "expensive-model-should-be-ignored"
        })
        .to_string(),
    );
    assert_eq!(created["success"], true, "{created}");
    let job_id = created["job_id"].as_str().unwrap().to_owned();
    let stored = manage::find_job(home, now, &job_id).unwrap();
    assert!(
        stored["model"].is_null(),
        "model must not be forwarded from tool"
    );

    // List via tool.
    let listed = cronjob_tool::call(home, &json!({"action": "list"}).to_string());
    assert_eq!(listed["count"], 1);

    // Pause and resume via tool.
    let paused = cronjob_tool::call(
        home,
        &json!({"action": "pause", "job_id": job_id, "reason": "maint"}).to_string(),
    );
    assert_eq!(paused["job"]["state"], "paused");
    let resumed = cronjob_tool::call(
        home,
        &json!({"action": "resume", "job_id": job_id}).to_string(),
    );
    assert_eq!(resumed["job"]["state"], "scheduled");

    // Cancel when idle -> cancelled=false.
    let idle = cronjob_tool::call(
        home,
        &json!({"action": "cancel", "job_id": job_id}).to_string(),
    );
    assert_eq!(idle["success"], true);
    assert_eq!(idle["cancelled"], false);

    // Create a live execution, then cancel -> writes marker file!
    let exec_id = executions::create_execution(home, &job_id, "manual", now, None).unwrap();
    executions::mark_execution_running(home, &exec_id, now).unwrap();
    let cancelled = cronjob_tool::call(
        home,
        &json!({"action": "cancel", "job_id": job_id, "reason": "operator stop"}).to_string(),
    );
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["execution_id"], exec_id);
    assert!(cancel::cancel_requested(home, &exec_id));
    assert_eq!(
        cancel::cancel_reason(home, &exec_id).as_deref(),
        Some("operator stop")
    );
    // First-writer-wins: second request keeps original reason.
    assert!(!cancel::request_cancel(home, &exec_id, Some("second reason")).unwrap());
    assert_eq!(
        cancel::cancel_reason(home, &exec_id).as_deref(),
        Some("operator stop")
    );
    cancel::clear_cancel(home, &exec_id);
    assert!(!cancel::cancel_requested(home, &exec_id));

    executions::finish_execution(
        home,
        &exec_id,
        "cancelled",
        Some("operator stop"),
        Some("local"),
        now,
    )
    .unwrap();
    let runs = executions::list_executions(home, Some(&job_id), 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "cancelled");

    // Remove via tool.
    let removed = cronjob_tool::call(
        home,
        &json!({"action": "remove", "job_id": job_id}).to_string(),
    );
    assert_eq!(removed["success"], true);
    assert!(Path::new(&home.join("cron/jobs.json")).is_file());
    let after = cronjob_tool::call(home, &json!({"action": "list"}).to_string());
    assert_eq!(after["count"], 0);
}
