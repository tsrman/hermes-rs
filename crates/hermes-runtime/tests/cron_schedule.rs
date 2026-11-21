//! Schedule parsing and next-run results recorded from the Python reference
//! (`compat/create_cron_fixture.py`).

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use hermes_runtime::cron::schedule::{compute_next_run, parse_schedule, Schedule};
use serde_json::Value;

fn now(case: &Value) -> DateTime<Tz> {
    let zone: Tz = case["zone"].as_str().unwrap().parse().unwrap();
    DateTime::parse_from_rfc3339(case["now"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
        .with_timezone(&zone)
}

#[test]
fn matches_the_python_reference() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/cron_schedule_cases.json")).unwrap();
    let mut failures = Vec::new();
    for case in &cases {
        let now = now(case);
        match case["op"].as_str().unwrap() {
            "parse" => {
                let text = case["text"].as_str().unwrap();
                let result = parse_schedule(text, now);
                if case["error"].as_bool() == Some(true) {
                    if result.is_ok() {
                        failures.push(format!("{text:?}: expected an error, got {result:?}"));
                    }
                } else {
                    let expected: Schedule =
                        serde_json::from_value(case["expect"].clone()).unwrap();
                    if result.as_ref() != Ok(&expected) {
                        failures.push(format!(
                            "{text:?} at {}: expected {expected:?}, got {result:?}",
                            case["now"]
                        ));
                    }
                }
            }
            _ => {
                let schedule: Schedule = serde_json::from_value(case["schedule"].clone()).unwrap();
                let job_zone: Option<Tz> =
                    case["job_zone"].as_str().map(|zone| zone.parse().unwrap());
                let result =
                    compute_next_run(&schedule, case["last_run_at"].as_str(), job_zone, now);
                let expected = case["expect"].as_str().map(str::to_owned);
                if result != expected {
                    failures.push(format!(
                        "{:?} now={} zone={} job_zone={:?} last={}: expected {expected:?}, got {result:?}",
                        schedule.expr.as_deref().or(schedule.run_at.as_deref()).unwrap_or(&schedule.display),
                        case["now"], case["zone"], job_zone, case["last_run_at"]
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
