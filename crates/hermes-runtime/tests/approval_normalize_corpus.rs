//! Detection-time normalization against the Python reference
//! (`compat/create_approval_normalize_corpus.py`), in two home environments.

use hermes_runtime::approval_detect::normalize::{
    lower_preserving_flags, normalize_command_for_detection, HomeEnv,
};
use serde_json::Value;

fn env(name: &str) -> HomeEnv {
    let hermes_home = match name {
        "custom" => "/opt/hermes-data",
        _ => "/home/user/.hermes",
    };
    HomeEnv {
        user_home: "/home/user".into(),
        home_var: "/home/user".into(),
        hermes_home: hermes_home.into(),
        ..HomeEnv::default()
    }
}

#[test]
fn normalization_matches_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_normalize_corpus.json")).unwrap();
    let mut failures = Vec::new();
    for row in corpus.as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        let env = env(row["env"].as_str().unwrap());
        let got = normalize_command_for_detection(command, &env);
        let shown: String = command.chars().take(50).collect();
        if got != row["normalized"].as_str().unwrap() {
            failures.push(format!(
                "[{}] {shown:?}: got {got:?} want {:?}",
                row["env"], row["normalized"]
            ));
        } else if lower_preserving_flags(&got) != row["lowered"].as_str().unwrap() {
            failures.push(format!("[{}] lowered {shown:?}", row["env"]));
        }
    }
    assert!(
        failures.is_empty(),
        "{} rows differ:\n{}",
        failures.len(),
        failures
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
