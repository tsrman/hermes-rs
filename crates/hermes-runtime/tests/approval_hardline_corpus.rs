//! The hardline floor against the Python reference (`compat/create_approval_corpus.py`,
//! field `hardline`).

use hermes_runtime::approval_detect::hardline::detect_hardline_command;
use hermes_runtime::approval_detect::normalize::HomeEnv;
use serde_json::Value;

fn env() -> HomeEnv {
    HomeEnv {
        user_home: "/home/user".into(),
        home_var: "/home/user".into(),
        hermes_home: "/home/user/.hermes".into(),
        ..HomeEnv::default()
    }
}

#[test]
fn hardline_verdicts_match_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_corpus.json")).unwrap();
    let env = env();
    let mut failures = Vec::new();
    let mut total = 0;
    for (category, rows) in corpus.as_object().unwrap() {
        for row in rows.as_array().unwrap() {
            total += 1;
            let command = row["command"].as_str().unwrap();
            let verdict = detect_hardline_command(command, &env);
            let want_blocked = row["hardline"][0].as_bool().unwrap();
            let want_description = row["hardline"][1].as_str();
            if verdict.blocked != want_blocked || verdict.description.as_deref() != want_description
            {
                let shown: String = command.chars().take(60).collect();
                failures.push(format!(
                    "[{category}] {shown:?}: got ({}, {:?}) want ({want_blocked}, {want_description:?})",
                    verdict.blocked, verdict.description
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} rows differ:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn hardline_verdicts_match_the_reference_on_crossed_contexts() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_hardline_corpus.json")).unwrap();
    let env = env();
    let mut failures = Vec::new();
    let rows = corpus.as_array().unwrap();
    for row in rows {
        let command = row["command"].as_str().unwrap();
        let verdict = detect_hardline_command(command, &env);
        let want_blocked = row["hardline"][0].as_bool().unwrap();
        let want_description = row["hardline"][1].as_str();
        if verdict.blocked != want_blocked || verdict.description.as_deref() != want_description {
            let shown: String = command.chars().take(70).collect();
            failures.push(format!(
                "{shown:?}: got ({}, {:?}) want ({want_blocked}, {want_description:?})",
                verdict.blocked, verdict.description
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} rows differ:\n{}",
        failures.len(),
        rows.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
