//! Reference corpus replay for `agent.stall_guards` (`compat/create_stall_guards_corpus.py`).

use hermes_runtime::stall_guards::{
    looks_like_degenerate_final, parse_stall_guards, strip_think_blocks, trailing_continue_intent,
    IntentAckContinuation, StallGuardTracker,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct ObserveCall {
    tool: String,
    args: Value,
    result: String,
    id: String,
    failed: bool,
    notice: Option<String>,
    stub: Option<String>,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ObserveSequence {
    name: String,
    calls: Vec<ObserveCall>,
}

#[derive(Deserialize)]
struct ContinueCase {
    text: String,
    expected: bool,
}

#[derive(Deserialize)]
struct DegenerateCase {
    text: String,
    user_message: String,
    expected: bool,
}

#[derive(Deserialize)]
struct ConfigCase {
    label: String,
    present: bool,
    value: Value,
    expected: bool,
}

#[derive(Deserialize)]
struct IntentAckCase {
    label: String,
    present: bool,
    value: Value,
    model: String,
    is_responses: bool,
    expected: String,
}

#[derive(Deserialize)]
struct Corpus {
    observe_sequences: Vec<ObserveSequence>,
    continue_intent_cases: Vec<ContinueCase>,
    degenerate_final_cases: Vec<DegenerateCase>,
    config_cases: Vec<ConfigCase>,
    intent_ack_cases: Vec<IntentAckCase>,
}

fn corpus() -> Corpus {
    serde_json::from_str(include_str!("fixtures/stall_guards_cases.json")).unwrap()
}

#[test]
fn observe_call_matches_reference_corpus() {
    let data = corpus();
    assert!(!data.observe_sequences.is_empty());
    for seq in data.observe_sequences {
        let mut tracker = StallGuardTracker::new();
        for (idx, call) in seq.calls.iter().enumerate() {
            let obs =
                tracker.observe_call(&call.tool, &call.args, &call.result, &call.id, call.failed);
            assert_eq!(
                obs.notice, call.notice,
                "seq={} step={} tool={} notice mismatch",
                seq.name, idx, call.tool
            );
            assert_eq!(
                obs.stub, call.stub,
                "seq={} step={} tool={} stub mismatch",
                seq.name, idx, call.tool
            );
            assert_eq!(
                obs.kind.map(str::to_owned),
                call.kind,
                "seq={} step={} tool={} kind mismatch",
                seq.name,
                idx,
                call.tool
            );
        }
    }
}

#[test]
fn trailing_continue_intent_matches_reference_corpus() {
    let data = corpus();
    assert!(!data.continue_intent_cases.is_empty());
    for case in data.continue_intent_cases {
        assert_eq!(
            trailing_continue_intent(&case.text),
            case.expected,
            "text={:?}",
            case.text
        );
    }
}

#[test]
fn looks_like_degenerate_final_matches_reference_corpus() {
    let data = corpus();
    assert!(!data.degenerate_final_cases.is_empty());
    for case in data.degenerate_final_cases {
        assert_eq!(
            looks_like_degenerate_final(&case.text, &case.user_message),
            case.expected,
            "text={:?} user={:?}",
            case.text,
            case.user_message
        );
    }
}

#[test]
fn config_parsing_matches_reference_corpus() {
    let data = corpus();
    assert!(!data.config_cases.is_empty());
    for case in data.config_cases {
        let yaml = if case.present {
            serde_yaml::to_value(serde_json::json!({
                "agent": { "stall_guards": case.value }
            }))
            .unwrap()
        } else {
            serde_yaml::to_value(serde_json::json!({ "agent": {} })).unwrap()
        };
        assert_eq!(
            parse_stall_guards(&yaml),
            case.expected,
            "label={}",
            case.label
        );
    }
    // Missing `agent` section also defaults to true.
    let empty: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
    assert!(parse_stall_guards(&empty));
}

#[test]
fn think_blocks_are_stripped_before_continue_intent_check() {
    let text = "<think>planning long chain of thought...</think>\nLet me now check the file.";
    assert!(trailing_continue_intent(&strip_think_blocks(text)));
}

#[test]
fn intent_ack_continuation_matches_reference_corpus() {
    let data = corpus();
    assert!(!data.intent_ack_cases.is_empty());
    for case in data.intent_ack_cases {
        let yaml = if case.present {
            serde_yaml::to_value(serde_json::json!({
                "agent": { "intent_ack_continuation": case.value }
            }))
            .unwrap()
        } else {
            serde_yaml::to_value(serde_json::json!({ "agent": {} })).unwrap()
        };
        let mode = IntentAckContinuation::from_config(&yaml);
        assert_eq!(
            mode.resolve(&case.model, case.is_responses),
            case.expected,
            "label={}",
            case.label
        );
    }
}
