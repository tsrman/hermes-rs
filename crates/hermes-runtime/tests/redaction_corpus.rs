//! The redactor against the Python reference (`compat/create_redaction_corpus.py`).
//!
//! The port is not finished, so some rows differ. `redaction_baseline.json` lists the rows
//! that still differ; this test fails when a row outside the list regresses and when a row
//! on the list starts to pass (remove it from the list: `UPDATE_REDACTION_BASELINE=1`).

use std::collections::BTreeMap;

use hermes_runtime::redact::redact_sensitive_text;
use serde_json::Value;

const BASELINE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/redaction_baseline.json"
);

fn failing() -> BTreeMap<String, Vec<String>> {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/redaction_corpus.json")).unwrap();
    let mut failing = BTreeMap::new();
    for (category, rows) in corpus.as_object().unwrap() {
        let mut inputs = Vec::new();
        for row in rows.as_array().unwrap() {
            let input = row["input"].as_str().unwrap();
            if redact_sensitive_text(input) != row["output"].as_str().unwrap() {
                inputs.push(input.to_owned());
            }
        }
        failing.insert(category.clone(), inputs);
    }
    failing
}

#[test]
fn rows_match_the_reference_except_the_listed_ones() {
    let now = failing();
    if std::env::var_os("UPDATE_REDACTION_BASELINE").is_some() {
        std::fs::write(
            BASELINE_PATH,
            serde_json::to_string_pretty(&now).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let baseline: BTreeMap<String, Vec<String>> = serde_json::from_str(
        &std::fs::read_to_string(BASELINE_PATH).unwrap_or_else(|_| "{}".into()),
    )
    .unwrap();
    let mut problems = Vec::new();
    for (category, inputs) in &now {
        let known = baseline.get(category).cloned().unwrap_or_default();
        for input in inputs.iter().filter(|input| !known.contains(input)) {
            problems.push(format!(
                "regressed [{category}]: {input:?} -> {:?}",
                redact_sensitive_text(input)
            ));
        }
        for input in known.iter().filter(|input| !inputs.contains(input)) {
            problems.push(format!(
                "now passes [{category}], remove it from the baseline: {input:?}"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_category_that_is_fully_ported_has_no_failing_rows() {
    let now = failing();
    for category in [
        "private_key",
        "jwt",
        "plain",
        "auth_header",
        "telegram",
        "phone",
        "webhook",
    ] {
        assert_eq!(now[category], Vec::<String>::new(), "{category}");
    }
}

#[test]
fn large_hostile_inputs_finish_quickly() {
    // Patterns that retry at every start (long key runs, digit runs, repeated URLs) must stay linear.
    let inputs = [
        "ghp_".repeat(50_000),
        "sk-a".repeat(50_000),
        format!("{}:{}", "1".repeat(100_000), "x".repeat(100_000)),
        "https://hooks.slack.com/services/T/B/".repeat(5_000),
        "postgres://u:".repeat(20_000),
        "-----BEGIN PRIVATE KEY-----\n".repeat(5_000),
        "Authorization: ".repeat(20_000),
        "\u{1b}[32m".repeat(50_000),
        "+1".repeat(50_000),
    ];
    for input in &inputs {
        let started = std::time::Instant::now();
        let _ = redact_sensitive_text(input);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{} bytes took {:?}",
            input.len(),
            started.elapsed()
        );
    }
}
