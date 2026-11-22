//! `cron add|edit|pause|resume|run|remove` replayed from recordings of the Python reference
//! (`compat/create_cron_jobs_fixture.py`). Each case is a sequence of steps on one profile; after
//! every step the store must hold exactly what the reference wrote: the same fields, the same
//! key order and the same bytes, and a refused step must give the reference's message.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use hermes_runtime::cron::manage::{
    add_job_with_id, claim_manual, edit_job, AddParams, EditParams, ManualClaim,
};
use hermes_runtime::cron::store::{pause_job, remove_job, resume_job};
use hermes_runtime::cron::tick::{mark_job_run, Outcome};
use serde_json::{json, Value};

fn clock(step: &Value, zone: Tz) -> DateTime<Tz> {
    DateTime::parse_from_rfc3339(step["now"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
        .with_timezone(&zone)
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The reference prints the refusal after a fixed prefix; a lookup that lists candidates prints
/// more lines than the one recorded.
fn same_message(actual: &str, recorded: &str) -> bool {
    actual == recorded || actual.starts_with(&format!("{recorded}\n"))
}

fn replay_step(home: &Path, zone: Tz, label: &str, step: &Value) -> Result<(), String> {
    let now = clock(step, zone);
    let argv = step["argv"].as_array().unwrap();
    let command = argv[0].as_str().unwrap();
    let args = &step["args"];
    let reference = args["job_id"].as_str().unwrap_or_default();
    let recorded_error = step["error"].as_str();
    let mut failure: Option<String> = None;

    match command {
        "add" => {
            let params: AddParams = serde_json::from_value(args.clone()).unwrap();
            let id = step["id"].as_str().unwrap();
            failure = add_job_with_id(home, now, id, &params).err();
        }
        "edit" => {
            let params: EditParams = serde_json::from_value(args.clone()).unwrap();
            failure = edit_job(home, now, reference, &params).err();
        }
        "pause" => failure = pause_job(home, now, reference, None).err(),
        "resume" => failure = resume_job(home, now, reference).err(),
        "remove" => failure = remove_job(home, now, reference).err(),
        "run" => {
            let claimed = &step["claimed"];
            let by = claimed["jobs"]
                .as_array()
                .and_then(|jobs| {
                    jobs.iter()
                        .find(|job| job["id"].as_str() == Some(reference))
                })
                .and_then(|job| job["fire_claim"]["by"].as_str())
                .unwrap_or("owner:1:token");
            match claim_manual(home, now, reference, by) {
                Err(error) => failure = Some(error),
                Ok(ManualClaim::Skipped(reason)) => {
                    let shown = step["stdout"].as_str().unwrap();
                    if !shown.ends_with(&format!("  {reason}")) {
                        return Err(format!(
                            "{label}: skipped for {reason:?}, expected {shown:?}"
                        ));
                    }
                    if !claimed.is_null() {
                        return Err(format!("{label}: the reference ran it"));
                    }
                }
                Ok(ManualClaim::Claimed(_)) => {
                    if claimed.is_null() {
                        return Err(format!("{label}: claimed, but the reference skipped it"));
                    }
                    // What the claim stamped, before the run: the same as the reference's.
                    let stored: Value = serde_json::from_str(
                        &std::fs::read_to_string(home.join("cron/jobs.json")).unwrap(),
                    )
                    .unwrap();
                    if serde_json::to_string(&stored).unwrap()
                        != serde_json::to_string(claimed).unwrap()
                    {
                        return Err(format!(
                            "{label}: claimed store differs\n  ours:   {}\n  theirs: {claimed}",
                            serde_json::to_string(&stored).unwrap()
                        ));
                    }
                    let run = &step["run"];
                    let error = run["error"].as_str();
                    mark_job_run(
                        home,
                        now,
                        reference,
                        &Outcome {
                            success: run["success"].as_bool().unwrap(),
                            error,
                            status: None,
                            owner: Some(by),
                        },
                    )
                    .unwrap();
                }
            }
        }
        other => panic!("unknown command {other}"),
    }

    match (failure, recorded_error) {
        (Some(actual), Some(recorded)) => {
            if !same_message(&actual, recorded) {
                return Err(format!(
                    "{label}: error differs\n  ours:   {actual:?}\n  theirs: {recorded:?}"
                ));
            }
        }
        (Some(actual), None) => {
            return Err(format!(
                "{label}: refused ({actual}), the reference did not"
            ))
        }
        (None, Some(recorded)) => {
            return Err(format!(
                "{label}: accepted, the reference refused: {recorded}"
            ))
        }
        (None, None) => {}
    }

    let path = home.join("cron/jobs.json");
    match step["store"].as_object() {
        None => {
            if path.exists() {
                return Err(format!(
                    "{label}: a store was written, the reference wrote none"
                ));
            }
        }
        Some(_) => {
            let raw = std::fs::read(&path).map_err(|error| format!("{label}: {error}"))?;
            let actual: Value = serde_json::from_slice(&raw).unwrap();
            let ours = serde_json::to_string(&actual).unwrap();
            let theirs = serde_json::to_string(&step["store"]).unwrap();
            if ours != theirs {
                // Name the first job that differs.
                let a = actual["jobs"].as_array().unwrap();
                let b = step["store"]["jobs"].as_array().unwrap();
                for (mine, other) in a.iter().zip(b.iter()) {
                    if serde_json::to_string(mine).unwrap() != serde_json::to_string(other).unwrap()
                    {
                        return Err(format!(
                            "{label}: store differs\n  ours:   {}\n  theirs: {}",
                            serde_json::to_string(mine).unwrap(),
                            serde_json::to_string(other).unwrap()
                        ));
                    }
                }
                return Err(format!(
                    "{label}: store differs\n  ours:   {ours}\n  theirs: {theirs}"
                ));
            }
            // Byte for byte: the formatting is the reference's too.
            if sha256(&raw) != step["store_sha256"].as_str().unwrap() {
                return Err(format!(
                    "{label}: the store's bytes differ from the reference's"
                ));
            }
        }
    }
    let output_dir = home.join("cron/output").is_dir();
    if output_dir != step["output_dir"].as_bool().unwrap() {
        return Err(format!(
            "{label}: output directory exists={output_dir}, the reference's was {}",
            step["output_dir"]
        ));
    }
    Ok(())
}

#[test]
fn matches_the_python_reference() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/cron_jobs_cases.json")).unwrap();
    let mut failures = Vec::new();
    let mut steps = 0;
    for case in &cases {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let zone: Tz = case["zone"].as_str().unwrap().parse().unwrap();
        if let Some(seed) = case["seed"].as_array() {
            std::fs::create_dir_all(home.join("cron")).unwrap();
            std::fs::write(
                home.join("cron/jobs.json"),
                serde_json::to_string_pretty(&json!({"jobs": seed})).unwrap(),
            )
            .unwrap();
        }
        for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            steps += 1;
            let label = format!(
                "{} #{index} {:?}",
                case["name"].as_str().unwrap(),
                step["argv"]
            );
            if let Err(failure) = replay_step(home, zone, &label, step) {
                failures.push(failure);
            }
        }
    }
    assert!(steps > 100, "only {steps} steps replayed");
    assert!(
        failures.is_empty(),
        "{} of {steps} steps differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
