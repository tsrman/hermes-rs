//! Wrapper peeling, heredoc bodies, masking and `env -S` against the Python reference
//! (`compat/create_approval_peel_corpus.py`).

use hermes_runtime::approval_detect::env_split::{env_split_payload, split_env_string};
use hermes_runtime::approval_detect::text_masks::{
    command_parser_limit_exceeded, join_escaped_word_chars, mask_quoted_newlines,
};
use hermes_runtime::approval_detect::wrapper_prefix::{
    blank_literal_heredoc_bodies, dequote_rm_operands, literal_heredoc_bodies,
    strip_wrapper_prefixes,
};
use serde_json::{json, Value};

fn build(kind: &str, n: usize) -> String {
    match kind {
        "chars" => format!("{};", "a".repeat(n - 1)),
        "nosep" => "a".repeat(n),
        "depth" => format!("echo {}{}", "$(".repeat(n), ")".repeat(n)),
        "depthq" => format!("echo {}", "\\$(".repeat(n)),
        "seps" => ";".repeat(n),
        "nl" => "\n".repeat(n),
        other => panic!("unknown kind {other}"),
    }
}

#[test]
fn peel_helpers_match_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_peel_corpus.json")).unwrap();
    let mut failures = Vec::new();
    let mut check = |what: &str, command: &str, got: Value, want: &Value| {
        if got != *want {
            let shown: String = command.chars().take(50).collect();
            failures.push(format!("{what} {shown:?}: got {got} want {want}"));
        }
    };
    for row in corpus["commands"].as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        check(
            "mask_nl",
            command,
            json!(mask_quoted_newlines(command).unwrap()),
            &row["mask_nl"],
        );
        check(
            "join",
            command,
            json!(join_escaped_word_chars(command)),
            &row["join"],
        );
        check(
            "peel",
            command,
            json!(strip_wrapper_prefixes(command).unwrap()),
            &row["peel"],
        );
        let spans: Vec<Value> = literal_heredoc_bodies(command)
            .iter()
            .map(|(s, e)| json!([s, e]))
            .collect();
        check("heredocs", command, Value::Array(spans), &row["heredocs"]);
        check(
            "blank",
            command,
            json!(blank_literal_heredoc_bodies(command)),
            &row["blank"],
        );
        check(
            "dequote",
            command,
            json!(dequote_rm_operands(command).unwrap()),
            &row["dequote"],
        );
        check(
            "limit",
            command,
            json!(command_parser_limit_exceeded(command)),
            &row["limit"],
        );
    }
    for row in corpus["large"].as_array().unwrap() {
        let command = build(
            row["kind"].as_str().unwrap(),
            row["n"].as_u64().unwrap() as usize,
        );
        check(
            "large",
            &row["kind"].to_string(),
            json!(command_parser_limit_exceeded(&command)),
            &row["limit"],
        );
    }
    for row in corpus["splits"].as_array().unwrap() {
        let payload = row["payload"].as_str().unwrap();
        check(
            "split",
            payload,
            json!(split_env_string(payload)),
            &row["args"],
        );
    }
    for row in corpus["envs"].as_array().unwrap() {
        let tokens: Vec<String> = row["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        check(
            "env",
            &tokens.join(" "),
            json!(env_split_payload(&tokens, 0)),
            &row["payload"],
        );
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
