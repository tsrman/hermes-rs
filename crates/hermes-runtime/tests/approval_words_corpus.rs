//! Command positions and shell-word primitives against the Python reference
//! (`compat/create_approval_words_corpus.py`).

use hermes_runtime::approval_detect::budget::Text;
use hermes_runtime::approval_detect::command_starts::{
    iter_shell_command_starts, iter_shell_command_word_spans, mark_command_starts,
    shell_command_segment,
};
use hermes_runtime::approval_detect::shell_words::{
    deobfuscate_shell_word_for_detection, literal_command_substitution_output,
};
use serde_json::{json, Value};

#[test]
fn word_primitives_match_the_reference() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/approval_words_corpus.json")).unwrap();
    let mut failures = Vec::new();
    for row in corpus["commands"].as_array().unwrap() {
        let command = row["command"].as_str().unwrap();
        let shown: String = command.chars().take(50).collect();
        let text = Text::new(command);
        let starts = iter_shell_command_starts(&text).unwrap();
        if json!(starts) != row["starts"] {
            failures.push(format!(
                "starts {shown:?}: got {starts:?} want {}",
                row["starts"]
            ));
        }
        let mark = mark_command_starts(&text, "\n", &[]).unwrap();
        if mark != row["mark"].as_str().unwrap() {
            failures.push(format!("mark {shown:?}"));
        }
        let mark_skip = mark_command_starts(&text, "\n", &[(2, 5), (9, 12)]).unwrap();
        if mark_skip != row["mark_skip"].as_str().unwrap() {
            failures.push(format!("mark_skip {shown:?}"));
        }
        let spans = iter_shell_command_word_spans(&text).unwrap();
        let spans_json: Vec<Value> = spans.iter().map(|(s, e, w)| json!([s, e, w])).collect();
        if Value::Array(spans_json) != row["spans"] {
            failures.push(format!(
                "spans {shown:?}: got {spans:?} want {}",
                row["spans"]
            ));
        }
        let deob: Vec<String> = spans
            .iter()
            .map(|(_, _, w)| deobfuscate_shell_word_for_detection(w).unwrap())
            .collect();
        if json!(deob) != row["deob"] {
            failures.push(format!("deob {shown:?}: got {deob:?} want {}", row["deob"]));
        }
        if let Some(expected) = row.get("segments") {
            let got: Vec<String> = starts
                .iter()
                .map(|&s| shell_command_segment(&text, s).unwrap())
                .collect();
            if json!(got) != *expected {
                failures.push(format!("segments {shown:?}: got {got:?} want {expected}"));
            }
        }
    }
    for row in corpus["words"].as_array().unwrap() {
        let word = row["word"].as_str().unwrap();
        let got = deobfuscate_shell_word_for_detection(word).unwrap();
        if got != row["deob"].as_str().unwrap() {
            failures.push(format!("word {word:?}: got {got:?} want {}", row["deob"]));
        }
    }
    for row in corpus["scripts"].as_array().unwrap() {
        let script = row["script"].as_str().unwrap();
        let got = literal_command_substitution_output(script).unwrap();
        if json!(got) != row["out"] {
            failures.push(format!(
                "script {script:?}: got {got:?} want {}",
                row["out"]
            ));
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
