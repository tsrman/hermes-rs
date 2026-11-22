//! `classify_osascript` against the Python reference (`compat/create_approval_osascript_corpus.py`).

use hermes_runtime::approval_detect::osascript::classify_osascript;
use serde_json::{json, Value};

#[test]
fn osascript_classification_matches_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_osascript_corpus.json")).unwrap();
    let mut failures = Vec::new();
    let rows = corpus.as_array().unwrap();
    for row in rows {
        let command = row["command"].as_str().unwrap();
        let found: Vec<&str> = classify_osascript(command).unwrap().into_iter().collect();
        if json!(found) != row["found"] {
            let shown: String = command.chars().take(70).collect();
            failures.push(format!("{shown:?}: got {found:?} want {}", row["found"]));
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
