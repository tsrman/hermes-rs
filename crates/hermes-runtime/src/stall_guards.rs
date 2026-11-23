//! Per-turn anti-stall guardrails (`agent.stall_guards`, default on).
//!
//! Port of the reference's `ToolCallGuardrailController.observe_call` (`agent/tool_guardrails.py`)
//! and the turn-end re-prompt detectors `trailing_continue_intent` and
//! `looks_like_degenerate_final` (`agent/agent_runtime_helpers.py`, `agent/turn_final_response.py`).

use std::collections::{BTreeMap, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use ring::digest::{digest, SHA256};
use serde_json::Value;
use serde_yaml::Value as Yaml;

/// Nth consecutive identical `(tool, canonical_args, result_hash)` call that emits the notice.
pub const IDENTICAL_CALL_THRESHOLD: usize = 3;
/// Maximum batch-cycle period checked for repeating multi-call loops (`A, B, A, B, ...`).
const MAX_CYCLE_PERIOD: usize = 4;
/// Sliding call history window for batch-cycle detection.
const CYCLE_HISTORY: usize = 64;
/// Minimum non-error result length (in Unicode characters) for 2nd+ byte-identical stubbing.
pub const IDENTICAL_RESULT_STUB_MIN_CHARS: usize = 512;
/// Maximum preview length (in Unicode characters) for canonical arguments inside a stub.
const RESULT_STUB_ARGS_PREVIEW_CHARS: usize = 120;
/// Maximum continuation re-prompts per turn (matching `codex_ack_continuations < 2`).
pub const MAX_STALL_CONTINUATIONS: usize = 2;

const TRAILING_CONTINUE_INTENT_MAX_CHARS: usize = 400;
const DEGENERATE_FINAL_MAX_CHARS: usize = 24;

/// Re-prompt sent when the model promises a next action with no tool calls (`_CODEX_ACK_CONTINUATION_NUDGE`).
pub const CONTINUE_INTENT_NUDGE: &str =
    "[System: Continue now. Execute the required tool calls and only send your final answer after completing the task.]";

/// Re-prompt sent when a turn stops on a collapsed fragment after real tool work (`_DEGENERATE_FINAL_NUDGE`).
pub const DEGENERATE_FINAL_NUDGE: &str =
    "[System: Your previous message ended the turn with a fragment that is not a usable answer. If the task is unfinished, continue it and then give the complete answer. If that fragment WAS your complete answer, send it again exactly as before.]";

/// Read `agent.stall_guards` from `config.yaml` with Python `bool(section.get("stall_guards", True))`
/// semantics (absent -> `true`; explicit `null`/`false`/`0`/`""`/`[]`/`{}` -> `false`).
pub fn parse_stall_guards(config: &Yaml) -> bool {
    let Some(agent) = config.get("agent").and_then(Yaml::as_mapping) else {
        return true;
    };
    let Some(raw) = agent.get(Yaml::String("stall_guards".to_owned())) else {
        return true;
    };
    crate::terminal_init::py_truthy(raw)
}

/// Configured `agent.intent_ack_continuation` mode (default `"auto"`: Responses API only).
/// Mirrors `agent/agent_runtime_helpers.py::intent_ack_continuation_mode`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum IntentAckContinuation {
    #[default]
    Auto,
    All,
    Off,
    Models(Vec<String>),
}

impl IntentAckContinuation {
    pub fn from_config(config: &Yaml) -> Self {
        let Some(agent) = config.get("agent").and_then(Yaml::as_mapping) else {
            return Self::Auto;
        };
        let Some(raw) = agent.get(Yaml::String("intent_ack_continuation".to_owned())) else {
            return Self::Auto;
        };
        match raw {
            Yaml::Bool(true) => Self::All,
            Yaml::Bool(false) => Self::Off,
            Yaml::String(word) => match word.to_ascii_lowercase().as_str() {
                "true" | "always" | "yes" | "on" => Self::All,
                "false" | "never" | "no" | "off" => Self::Off,
                _ => Self::Auto,
            },
            Yaml::Sequence(items) => Self::Models(
                items
                    .iter()
                    .filter_map(Yaml::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => Self::Auto,
        }
    }

    /// Resolved mode for `(model, is_responses)`: `"all"`, `"codex_only"`, or `"off"`.
    pub fn resolve(&self, model: &str, is_responses: bool) -> &'static str {
        match self {
            Self::All => "all",
            Self::Off => "off",
            Self::Models(patterns) => {
                let model_lower = model.to_ascii_lowercase();
                if patterns
                    .iter()
                    .any(|p| model_lower.contains(&p.to_ascii_lowercase()))
                {
                    "all"
                } else {
                    "off"
                }
            }
            Self::Auto => {
                if is_responses {
                    "codex_only"
                } else {
                    "off"
                }
            }
        }
    }
}

/// Pollers exempt from the identical-call/cycle notice (still eligible for result stubbing).
pub fn is_stall_guard_repeatable(tool_name: &str) -> bool {
    tool_name == "process_manage"
        || tool_name.ends_with("_get_result")
        || tool_name.ends_with("_poll")
}

/// Observation returned by [`StallGuardTracker::observe_call`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdenticalCallObservation {
    pub notice: Option<String>,
    pub stub: Option<String>,
    pub kind: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CallSignature {
    tool_name: String,
    args_hash: String,
}

/// Per-turn state for identical-call streak, repeating batch-cycle, and duplicate-result stubbing.
#[derive(Clone, Debug, Default)]
pub struct StallGuardTracker {
    streak_sig: Option<CallSignature>,
    streak_result_hash: String,
    streak_count: usize,
    streak_first_call_id: String,
    call_history: VecDeque<(CallSignature, String, bool)>,
}

impl StallGuardTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_call(
        &mut self,
        tool_name: &str,
        args: &Value,
        result: &str,
        tool_call_id: &str,
        failed: bool,
    ) -> IdenticalCallObservation {
        let canonical_args = canonical_tool_args(args);
        let signature = CallSignature {
            tool_name: tool_name.to_owned(),
            args_hash: sha256_hex(&canonical_args),
        };
        let result_hash = hash_result(result, tool_name);

        if self.streak_sig.as_ref() == Some(&signature) && self.streak_result_hash == result_hash {
            self.streak_count += 1;
        } else {
            self.streak_sig = Some(signature.clone());
            self.streak_result_hash = result_hash.clone();
            self.streak_count = 1;
            self.streak_first_call_id = tool_call_id.to_owned();
        }
        let count = self.streak_count;
        let repeatable = is_stall_guard_repeatable(tool_name);

        let mut notice = None;
        let mut kind = None;
        if !repeatable && count >= IDENTICAL_CALL_THRESHOLD {
            notice = Some(format!(
                "[hermes note: this is the {} consecutive identical call to {tool_name} with identical arguments returning the same result. Do not repeat it — change arguments, use a different tool, or proceed with what you have.]",
                ordinal(count)
            ));
            kind = Some("identical_call_streak");
        }

        if self.call_history.len() == CYCLE_HISTORY {
            self.call_history.pop_front();
        }
        self.call_history
            .push_back((signature, result_hash, repeatable));

        if notice.is_none() {
            if let Some((period, laps)) = self.detect_identical_cycle() {
                notice = Some(format!(
                    "[hermes note: the last {laps} rounds repeated the same cycle of {period} tool calls (ending with {tool_name}) with identical arguments and identical results. Do not repeat the batch — change arguments, use a different tool, or proceed with what you have.]"
                ));
                kind = Some("identical_cycle");
            }
        }

        let stub =
            if count >= 2 && !failed && result.chars().count() >= IDENTICAL_RESULT_STUB_MIN_CHARS {
                Some(self.build_result_reference_stub(tool_name, &canonical_args))
            } else {
                None
            };

        IdenticalCallObservation { notice, stub, kind }
    }

    /// Observe a tool call's raw result and return the content to place in the `tool` message:
    /// stubbing replaces the body first, then any notice is appended after a blank line.
    pub fn apply(
        &mut self,
        tool_name: &str,
        raw_arguments: &str,
        mut result: String,
        tool_call_id: &str,
        failed: bool,
    ) -> String {
        let parsed_args = serde_json::from_str::<Value>(raw_arguments)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Default::default()));
        let obs = self.observe_call(tool_name, &parsed_args, &result, tool_call_id, failed);
        if let Some(stub) = obs.stub {
            result = stub;
        }
        if let Some(notice) = obs.notice {
            result.push_str("\n\n");
            result.push_str(&notice);
        }
        result
    }

    fn detect_identical_cycle(&self) -> Option<(usize, usize)> {
        let len = self.call_history.len();
        for period in 2..=MAX_CYCLE_PERIOD {
            if len < period * IDENTICAL_CALL_THRESHOLD {
                continue;
            }
            let mut laps = 1usize;
            loop {
                let needed = period * (laps + 1);
                if len < needed {
                    break;
                }
                let base = len - needed;
                let tail_start = len - period;
                let lap_equal = (0..period).all(|i| {
                    self.call_history[base + i].0 == self.call_history[tail_start + i].0
                        && self.call_history[base + i].1 == self.call_history[tail_start + i].1
                });
                if !lap_equal {
                    break;
                }
                laps += 1;
            }
            if laps >= IDENTICAL_CALL_THRESHOLD {
                let tail_start = len - period;
                let all_repeatable = (0..period).all(|i| self.call_history[tail_start + i].2);
                if all_repeatable {
                    continue;
                }
                return Some((period, laps));
            }
        }
        None
    }

    fn build_result_reference_stub(&self, tool_name: &str, canonical_args: &str) -> String {
        let preview = if canonical_args.chars().count() > RESULT_STUB_ARGS_PREVIEW_CHARS {
            let prefix: String = canonical_args
                .chars()
                .take(RESULT_STUB_ARGS_PREVIEW_CHARS)
                .collect();
            format!("{prefix}…")
        } else {
            canonical_args.to_owned()
        };
        let reference = if self.streak_first_call_id.is_empty() {
            String::new()
        } else {
            format!(" (tool_call_id {})", self.streak_first_call_id)
        };
        format!(
            "[hermes note: this result is byte-identical to the {tool_name} result earlier this turn{reference}. Refer to that result; it has not changed. Args: {preview}]"
        )
    }
}

/// Strip reasoning/think XML blocks (`<think>...</think>`, `<thought>...</thought>`,
/// `<thinking>...</thinking>`, `<reasoning>...</reasoning>`) before evaluating reply text.
pub fn strip_think_blocks(text: &str) -> String {
    static THINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?is)<(think|thought|thinking|reasoning)>.*?</(think|thought|thinking|reasoning)>",
        )
        .unwrap()
    });
    THINK_RE.replace_all(text, "").into_owned()
}

/// Narrow trailing continue-intent detector (`agent.agent_runtime_helpers.trailing_continue_intent`):
/// true when a short reply (`<= 400` chars) ends with an announced next action.
pub fn trailing_continue_intent(text: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(?:\blet me now\b|\bi(?:['\u{2019}])?ll now\b|\bi will now\b|\bnow i(?:['\u{2019}]ll| will)\b|\bnext[,:] i\b)[^.!?\n]{0,100}[.:\u{2026}]?\s*$",
        )
        .unwrap()
    });
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count == 0 || count > TRAILING_CONTINUE_INTENT_MAX_CHARS {
        return false;
    }
    let tail: String = if count > 160 {
        trimmed.chars().skip(count - 160).collect()
    } else {
        trimmed.to_owned()
    };
    RE.is_match(&tail)
}

/// Collapsed-fragment detector (`agent.agent_runtime_helpers.looks_like_degenerate_final`):
/// true when a text stop after real tool work is `<= 24` chars, lacks terminal punctuation, and
/// either opens with stray punctuation before a letter or is pure non-ASCII letters in an ASCII prompt.
pub fn looks_like_degenerate_final(text: &str, user_message: &str) -> bool {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count == 0 || count > DEGENERATE_FINAL_MAX_CHARS {
        return false;
    }
    if matches!(
        trimmed.chars().last(),
        Some('.' | '!' | '?' | '\u{3002}' | '\u{ff01}' | '\u{ff1f}')
    ) {
        return false;
    }
    let mut chars = trimmed.chars();
    let first = chars.next().unwrap();
    if matches!(first, '?' | '!' | ',' | ';' | ':' | ')' | ']' | '}')
        && chars.next().is_some_and(char::is_alphabetic)
    {
        return true;
    }
    if !trimmed.chars().any(char::is_alphabetic)
        || trimmed
            .chars()
            .any(|ch| ch.is_ascii() && ch.is_alphanumeric())
    {
        return false;
    }
    !user_message
        .chars()
        .any(|ch| ch.is_alphabetic() && !ch.is_ascii())
}

/// Count `tool` rows after the most recent `user` row (`agent.agent_runtime_helpers.tool_results_this_turn`).
/// Any `user` row (including a continuation nudge) ends the window, bounding degenerate re-prompts.
pub fn tool_results_since_last_user(messages: &[Value]) -> usize {
    let mut count = 0usize;
    for msg in messages.iter().rev() {
        match msg["role"].as_str() {
            Some("user") => break,
            Some("tool") => count += 1,
            _ => {}
        }
    }
    count
}

fn ordinal(count: usize) -> String {
    let suffix = if (11..=13).contains(&(count % 100)) {
        "th"
    } else {
        match count % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{count}{suffix}")
}

fn canonical_tool_args(args: &Value) -> String {
    let obj = if args.is_object() {
        args.clone()
    } else {
        Value::Object(Default::default())
    };
    canonical_json(&obj)
}

fn hash_result(result: &str, tool_name: &str) -> String {
    match serde_json::from_str::<Value>(result) {
        Ok(mut parsed) => {
            if tool_name == "execute_code" {
                strip_execute_code_metadata(&mut parsed);
            }
            sha256_hex(&canonical_json(&parsed))
        }
        Err(_) => sha256_hex(result),
    }
}

fn strip_execute_code_metadata(parsed: &mut Value) {
    let Some(map) = parsed.as_object_mut() else {
        return;
    };
    map.remove("duration_seconds");
    if let Some(kernel) = map.get_mut("kernel").and_then(Value::as_object_mut) {
        kernel.remove("execution_count");
    }
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(&sort_value(value)).unwrap_or_else(|_| value.to_string())
}

fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_value(v)))
                .collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

fn sha256_hex(text: &str) -> String {
    let hash = digest(&SHA256, text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash.as_ref() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
