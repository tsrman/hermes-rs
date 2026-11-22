//! Execution flags, grep operands, segment tokens and heredoc masks against the Python reference
//! (`compat/create_approval_exec_corpus.py`).

use hermes_runtime::approval_detect::budget::Text;
use hermes_runtime::approval_detect::command_starts::iter_shell_command_word_spans;
use hermes_runtime::approval_detect::exec_findings::{
    eval_payloads, execution_flag_findings, nested_payloads,
};
use hermes_runtime::approval_detect::shell_heredoc::mask_heredoc_bodies_for_shell_scan;
use hermes_runtime::approval_detect::shell_tokens::{
    grep_safe_detection_variant, iter_top_level_shell_segments, quoted_grep_pattern_spans,
    shell_segment_tokens, shell_tokens_with_spans,
};
use serde_json::{json, Value};

#[test]
fn exec_primitives_match_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_exec_corpus.json")).unwrap();
    let mut failures = Vec::new();
    let mut check = |what: &str, command: &str, got: Value, want: &Value| {
        if got != *want {
            let shown: String = command.chars().take(50).collect();
            failures.push(format!("{what} {shown:?}: got {got} want {want}"));
        }
    };
    for row in corpus.as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        check(
            "hd_data",
            command,
            json!(mask_heredoc_bodies_for_shell_scan(command, false)),
            &row["heredoc_data"],
        );
        check(
            "hd_code",
            command,
            json!(mask_heredoc_bodies_for_shell_scan(command, true)),
            &row["heredoc_code"],
        );
        let segments = iter_top_level_shell_segments(command).unwrap();
        check("segments", command, json!(segments), &row["segments"]);
        let mut tokens = Vec::new();
        let mut span_tokens = Vec::new();
        for segment in &segments {
            let text = Text::new(segment);
            for (start, _, _) in iter_shell_command_word_spans(&text).unwrap() {
                tokens.push(json!([
                    segment,
                    start,
                    shell_segment_tokens(segment, start)
                ]));
                let spans = shell_tokens_with_spans(&text, start).unwrap().map(|ts| {
                    ts.iter()
                        .map(|t| json!([t.value, t.start, t.end, t.inert]))
                        .collect::<Vec<_>>()
                });
                span_tokens.push(json!([segment, start, spans]));
            }
        }
        check("tokens", command, Value::Array(tokens), &row["tokens"]);
        check(
            "span_tokens",
            command,
            Value::Array(span_tokens),
            &row["span_tokens"],
        );
        let (spans, malformed) = quoted_grep_pattern_spans(command).unwrap();
        let spans: Vec<Value> = spans.iter().map(|(s, e)| json!([s, e])).collect();
        check("grep", command, json!([spans, malformed]), &row["grep"]);
        let (safe, bad) = grep_safe_detection_variant(command).unwrap();
        check("grep_safe", command, json!([safe, bad]), &row["grep_safe"]);
        let findings: Vec<Value> = execution_flag_findings(command)
            .unwrap()
            .iter()
            .map(|(d, p)| json!([d, p]))
            .collect();
        check(
            "findings",
            command,
            Value::Array(findings),
            &row["findings"],
        );
        check(
            "evals",
            command,
            json!(eval_payloads(command).unwrap()),
            &row["evals"],
        );
        for (depth, nested_key, capped_key) in
            [(0, "nested0", "capped0"), (8, "nested8", "capped8")]
        {
            let mut capped = Vec::new();
            let nested: Vec<Value> = nested_payloads(command, depth, Some(&mut capped))
                .unwrap()
                .iter()
                .map(|(p, n)| json!([p, n]))
                .collect();
            check(nested_key, command, Value::Array(nested), &row[nested_key]);
            check(capped_key, command, json!(capped), &row[capped_key]);
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
