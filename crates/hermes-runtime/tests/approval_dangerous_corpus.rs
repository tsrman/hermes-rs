//! The dangerous-command layer against the Python reference
//! (`compat/create_approval_dangerous_corpus.py`). The reference runs without its `osascript`
//! classifier and the `cron.lifecycle_guard` splice check, which the port does not have yet.

use hermes_runtime::approval_detect::dangerous::{
    detect_content_rule_keys, detect_dangerous_command,
};
use hermes_runtime::approval_detect::normalize::HomeEnv;
use serde_json::{json, Value};

fn env() -> HomeEnv {
    HomeEnv {
        user_home: "/home/user".into(),
        home_var: "/home/user".into(),
        hermes_home: "/home/user/.hermes".into(),
        ..HomeEnv::default()
    }
}

#[test]
fn dangerous_verdicts_match_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_dangerous_corpus.json")).unwrap();
    let env = env();
    let mut failures = Vec::new();
    let rows = corpus.as_array().unwrap();
    for row in rows {
        let command = row["command"].as_str().unwrap();
        // The scratch-file exemption names the OS temp directory, which is `/tmp` only on Unix.
        if cfg!(windows) && command.starts_with("rm -f /tmp/hermes-") {
            continue;
        }
        let verdict = detect_dangerous_command(command, &env);
        let got = json!([verdict.dangerous, verdict.description]);
        let content = json!(detect_content_rule_keys(command, &env));
        let shown: String = command.chars().take(70).collect();
        if got != row["dangerous"] {
            failures.push(format!(
                "dangerous {shown:?}: got {got} want {}",
                row["dangerous"]
            ));
        }
        if content != row["content"] {
            failures.push(format!(
                "content {shown:?}: got {content} want {}",
                row["content"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} problems in {} rows:\n{}",
        failures.len(),
        rows.len(),
        failures
            .iter()
            .take(50)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
