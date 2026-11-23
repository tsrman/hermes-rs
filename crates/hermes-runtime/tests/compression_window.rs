//! Which rows a compression keeps at the head and the tail and which it summarizes, replayed against
//! the Python reference. `compat/create_compression_window_corpus.py` runs the reference's own
//! compressor (`_plan_compaction_window`, `_prune_old_tool_results`, `_scan_window_handoffs`, and
//! `_summary_placement`) for hundreds of synthetic histories and writes
//! `fixtures/compression_window_cases.json`; every case here must come out identical.

use std::time::{Duration, Instant};

use hermes_runtime::config::{
    merge_summary_into_tail_row, plan_summary_placement, CompactionBlockReason, CompactionGuard,
    CompactionSettings,
};
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/compression_window_cases.json")).unwrap()
}

/// What the generator stores as `[text, pad, pad_char]` or `{"summary", "text"}`.
fn expand(content: &Value, markers: &Value) -> Value {
    let marker = |name: &str| markers[name].as_str().unwrap().to_owned();
    if let Some(kind) = content["summary"].as_str() {
        let text = content["text"].as_str().unwrap();
        let body = if kind == "historical" {
            marker("historical")
        } else {
            marker("current")
        };
        let merged = |prior: String| format!("{prior}\n\n{}\n{body} {text}", marker("delimiter"));
        return json!(match kind {
            "port" => format!("[CONTEXT SUMMARY]:\nUse this handoff as historical context. {text}"),
            "merged" => merged("kept request".into()),
            "merged_header" => merged(format!("{}\nkept request", marker("header"))),
            "merged_empty" => merged(marker("header")),
            "ended" => format!("{body} {text}\n\n{}", marker("end")),
            "ended_live" => format!("{body} {text}\n\n{}\nlive follow-up", marker("end")),
            _ => format!("{body} {text}"),
        });
    }
    if let Some([text, pad, fill]) = content.as_array().map(Vec::as_slice) {
        if let (Some(text), Some(pad), Some(fill)) = (text.as_str(), pad.as_u64(), fill.as_str()) {
            return json!(format!("{text}{}", fill.repeat(pad as usize)));
        }
    }
    content.clone()
}

/// The generator's `unpack`: a row is `[code, content]`, `[code, content, call numbers]` for an
/// assistant row with tool calls, `["t", content, call number]` for a tool result, or `["x", row]`.
fn unpack(item: &Value) -> Value {
    let call = |number: &Value| {
        let number = number.as_u64().unwrap();
        json!({"id": format!("call_{number}"), "type": "function",
               "function": {"name": "read_file", "arguments": format!("{{\"path\": \"f{number}.txt\"}}")}})
    };
    match item[0].as_str().unwrap() {
        "x" => item[1].clone(),
        "t" => {
            json!({"role": "tool", "tool_call_id": format!("call_{}", item[2]), "content": item[1]})
        }
        code => {
            let role = match code {
                "u" => "user",
                "a" => "assistant",
                "s" => "system",
                _ => "developer",
            };
            let mut row = json!({"role": role, "content": item[1]});
            if let Some(numbers) = item.get(2) {
                row["tool_calls"] =
                    Value::Array(numbers.as_array().unwrap().iter().map(call).collect());
            }
            row
        }
    }
}

fn messages(case: &Value, markers: &Value) -> Vec<Value> {
    case["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let mut row = unpack(item);
            row["content"] = expand(&row["content"], markers);
            row
        })
        .collect()
}

fn settings(section: &str, window: Option<u64>) -> CompactionSettings {
    let model = window
        .map(|window| format!("model: {{context_length: {window}}}\n"))
        .unwrap_or_default();
    let config = serde_yaml::from_str(&format!("{model}compression: {section}\n")).unwrap();
    CompactionSettings::from_config(&config)
}

fn expected_full(case: &Value) -> Option<std::ops::Range<usize>> {
    if case["full_noop"].as_bool().unwrap() {
        return None;
    }
    Some(
        case["full_start"].as_u64().unwrap() as usize
            ..case["full_tail_start"].as_u64().unwrap() as usize,
    )
}

#[test]
fn the_window_matches_the_reference_on_every_recorded_history_including_pruned_cases() {
    let data = fixture();
    let markers = &data["markers"];
    let mut wrong = Vec::new();
    let (mut checked, mut pruned_differs, mut preflight) = (0, 0, 0);
    for case in data["cases"].as_array().unwrap() {
        let section = case["compression"].as_str().unwrap();
        let ours = settings(section, case["window"].as_u64());
        let rows = messages(case, markers);
        let label = format!(
            "{section} window {} {} {} after_pass={} rows={}",
            case["window"],
            case["shape"],
            case["ending"],
            case["after_pass"],
            rows.len()
        );
        let budget = case["tail_token_budget"].as_u64().unwrap() as usize;
        if ours.tail_budget() != Some(budget) {
            wrong.push(format!(
                "{label}: tail budget {:?}, reference {budget}",
                ours.tail_budget()
            ));
            continue;
        }
        let after_pass = case["after_pass"].as_bool().unwrap();
        let want = expected_full(case);
        let got = ours.plan_window(&rows, after_pass);
        let got_rows = ours.plan_window_rows(&rows, after_pass);
        if let (Some((_, count)), Some(want_rows)) = (&got_rows, case["full_window_rows"].as_u64())
        {
            if *count as u64 != want_rows {
                wrong.push(format!(
                    "{label}: summarizer reads {count} rows, reference {want_rows}"
                ));
            }
        }
        checked += 1;
        if got != want {
            wrong.push(format!("{label}: ours {got:?}, reference {want:?}"));
        }
        if case["prune_clean"].as_bool().unwrap() {
            assert_eq!(case["start"], case["full_start"], "{label}: corpus");
            assert_eq!(case["end"], case["full_end"], "{label}: corpus");
        } else if case["start"] != case["full_start"] || case["end"] != case["full_end"] {
            pruned_differs += 1;
        }
        // A saved history has no system prompt and not yet the query: the preflight adapter stands both in.
        if case["ending"] == "query" && rows[0]["role"] == "system" {
            let history = &rows[1..rows.len() - 1];
            let query = rows.last().unwrap()["content"].as_str().unwrap();
            let want = want.map(|span| span.start - 1..span.end - 1);
            let got = ours.plan_history(history, query, after_pass);
            preflight += 1;
            if got != want {
                wrong.push(format!(
                    "{label}: preflight ours {got:?}, reference {want:?}"
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {checked} differ:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert!(
        checked >= 400 && preflight >= 80,
        "{checked} cases, {preflight} preflight"
    );
    assert!(
        pruned_differs >= 30,
        "expected >= 30 pruned-window cases, got {pruned_differs}"
    );
}

#[test]
fn prune_old_tool_results_matches_the_reference_corpus() {
    let data = fixture();
    let cases = data["prune_cases"].as_array().unwrap();
    assert!(cases.len() >= 4);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let ours = settings(
            case["compression"].as_str().unwrap(),
            case["window"].as_u64(),
        );
        let rows = case["messages"].as_array().unwrap();
        let (pruned, count) = ours.prune_tool_results(rows).unwrap();
        let want_count = case["pruned_count"].as_u64().unwrap() as usize;
        assert_eq!(count, want_count, "{label}: pruned_count");
        let want_contents = case["contents"].as_array().unwrap();
        assert_eq!(pruned.len(), want_contents.len(), "{label}: len");
        for (idx, (row, want)) in pruned.iter().zip(want_contents.iter()).enumerate() {
            assert_eq!(&row["content"], want, "{label}[{idx}]");
        }
    }
}

#[test]
fn summary_placement_matches_the_reference_corpus() {
    let data = fixture();
    let cases = data["placement_cases"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let head = case["head"].as_array().unwrap();
        let tail = case["tail"].as_array().unwrap();
        let start = case["compress_start"].as_u64().unwrap() as usize;
        let got = plan_summary_placement(head, tail, start);
        assert_eq!(
            got.summary_role,
            case["summary_role"].as_str().unwrap(),
            "{label}: summary_role"
        );
        assert_eq!(
            got.merge_into_tail,
            case["merge_into_tail"].as_bool().unwrap(),
            "{label}: merge_into_tail"
        );
        assert_eq!(
            got.force_user_leading,
            case["force_user_leading"].as_bool().unwrap(),
            "{label}: force_user_leading"
        );
        assert_eq!(
            got.first_tail_visible_idx,
            case["first_tail_visible_idx"].as_u64().map(|v| v as usize),
            "{label}: first_tail_visible_idx"
        );
        if let Some(preview) = case["merged_preview"].as_object() {
            let target_idx = got.merge_target_idx();
            assert_eq!(
                target_idx,
                preview["target_idx"].as_u64().unwrap() as usize,
                "{label}: target_idx"
            );
            let mut copy = tail[target_idx].clone();
            merge_summary_into_tail_row(&mut copy, "SUM", &got);
            assert_eq!(copy["role"], preview["role"], "{label}: merged role");
            assert_eq!(
                copy["content"], preview["content"],
                "{label}: merged content"
            );
        }
    }
}

#[test]
fn compaction_guard_enforces_cooldown_ladder_ineffective_breaker_and_probation() {
    let mut guard = CompactionGuard::new();
    let t0 = Instant::now();

    // Timeout / truncated ladder: 60s -> 300s -> 900s.
    guard.record_summary_failure_at(t0, true);
    assert_eq!(
        guard.check_blocked_at(t0 + Duration::from_secs(59), false),
        Some(CompactionBlockReason::Cooldown)
    );
    // Overflow bypasses cooldown without clearing it.
    assert_eq!(
        guard.check_blocked_at(t0 + Duration::from_secs(59), true),
        None
    );
    assert_eq!(
        guard.check_blocked_at(t0 + Duration::from_secs(60), false),
        None
    );

    let t1 = t0 + Duration::from_secs(61);
    guard.record_summary_failure_at(t1, true);
    assert_eq!(
        guard.check_blocked_at(t1 + Duration::from_secs(299), false),
        Some(CompactionBlockReason::Cooldown)
    );
    assert_eq!(
        guard.check_blocked_at(t1 + Duration::from_secs(300), false),
        None
    );

    let t2 = t1 + Duration::from_secs(301);
    guard.record_summary_failure_at(t2, true);
    assert_eq!(
        guard.check_blocked_at(t2 + Duration::from_secs(899), false),
        Some(CompactionBlockReason::Cooldown)
    );
    assert_eq!(
        guard.check_blocked_at(t2 + Duration::from_secs(900), false),
        None
    );

    // Non-timeout failure arms 600s cooldown; manual reset clears it immediately.
    let t3 = t2 + Duration::from_secs(901);
    guard.record_summary_failure_at(t3, false);
    assert_eq!(
        guard.check_blocked_at(t3 + Duration::from_secs(599), false),
        Some(CompactionBlockReason::Cooldown)
    );
    guard.reset_for_manual();
    assert_eq!(
        guard.check_blocked_at(t3 + Duration::from_secs(1), false),
        None
    );

    // Anti-thrash breaker: 1 strike does not block; 2 strikes trip for 300s, then 1 probation probe.
    guard.record_ineffective();
    assert_eq!(guard.ineffective_count(), 1);
    assert_eq!(guard.check_blocked_at(t3, false), None);

    guard.record_ineffective();
    assert_eq!(guard.ineffective_count(), 2);
    assert_eq!(
        guard.check_blocked_at(t3, false),
        Some(CompactionBlockReason::Ineffective)
    );
    // Overflow does NOT bypass the ineffective breaker.
    assert_eq!(
        guard.check_blocked_at(t3 + Duration::from_secs(10), true),
        Some(CompactionBlockReason::Ineffective)
    );
    // After 300s, one probation probe is allowed and strikes drop to 1.
    assert_eq!(
        guard.check_blocked_at(t3 + Duration::from_secs(300), false),
        None
    );
    assert_eq!(guard.ineffective_count(), 1);
    // If the probation probe is ineffective again, it re-trips immediately.
    guard.record_ineffective();
    assert_eq!(
        guard.check_blocked_at(t3 + Duration::from_secs(301), false),
        Some(CompactionBlockReason::Ineffective)
    );
    // A healthy compaction below threshold clears the breaker.
    guard.record_success(false);
    assert_eq!(guard.ineffective_count(), 0);
    assert_eq!(
        guard.check_blocked_at(t3 + Duration::from_secs(302), false),
        None
    );
}

#[test]
fn tail_mode_target_ratio_and_min_tail_user_messages_are_read_like_the_reference() {
    let data = fixture();
    let mut wrong = Vec::new();
    for row in data["values"].as_array().unwrap() {
        let section = row["compression"].as_str().unwrap();
        let (legacy, ratio, users) = settings(section, Some(128_000)).tail_settings();
        // The reference refuses a section it cannot coerce (an error at start-up); the port reads the
        // default instead.
        let (want_legacy, want_ratio, want_users) = if row["error"].is_string() {
            (false, 0.2, 1)
        } else {
            (
                row["tail_mode"] == "legacy",
                row["target_ratio"].as_f64().unwrap(),
                row["min_tail_users"].as_u64().unwrap() as usize,
            )
        };
        // `min_tail_user_messages: abc` is not a refusal in the reference: it reads 1.
        if legacy != want_legacy || (ratio - want_ratio).abs() > 1e-9 || users != want_users {
            wrong.push(format!(
                "{section}: ours ({legacy}, {ratio}, {users}), reference ({want_legacy}, {want_ratio}, {want_users})"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn without_a_known_window_the_budget_uses_the_reference_fallback_window() {
    let data = fixture();
    let fallback = data["fallback_window"].as_u64().unwrap();
    // The corpus records the `window: null` cases with the reference's fallback window as the window.
    let none = settings("{threshold_tokens: 2500, protect_last_n: 3}", None);
    let with = settings(
        "{threshold_tokens: 2500, protect_last_n: 3}",
        Some(fallback),
    );
    assert_eq!(none.tail_budget(), with.tail_budget());
}
