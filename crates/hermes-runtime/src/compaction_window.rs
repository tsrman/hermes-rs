//! Which messages a compression keeps at the head, keeps verbatim at the tail and summarizes in
//! between. A port of the reference's `ContextCompressor._plan_compaction_window` (`_prune_old_tool_results`
//! followed by `_compress_window` and `_scan_window_handoffs`) and `_summary_placement`: the head is the
//! system prompt plus `protect_first_n` rows (none once a handoff exists), the tail is a token budget
//! walked backwards from the newest row, and anchors keep the latest user request, the latest assistant
//! reply and the last `min_tail_user_messages` user turns out of the summary. A tool call and its
//! results are never split. `compat/create_compression_window_corpus.py` records the reference's
//! answer for hundreds of synthetic histories; `tests/compression_window.rs` replays them.
//!
//! Remaining differences from the reference are listed in the migration plan: images and the Codex
//! replay fields cost nothing, and a route that echoes stale thinking is not priced separately.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use serde_json::{json, Value};

const SUMMARY_PREFIXES: [&str; 3] = [
    "[CONTEXT SUMMARY]:",
    "[CONTEXT COMPACTION — REFERENCE ONLY]",
    "[CONTEXT COMPACTION",
];
pub(crate) const END_MARKER: &str =
    "--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---";
pub(crate) const PRIOR_HEADER: &str = "[PRIOR CONTEXT — for reference only; not a new message]";
pub(crate) const MERGED_DELIMITER: &str = "[END OF PRIOR CONTEXT — COMPACTION SUMMARY BELOW]";
/// Rows the reference looks at past `protect_first_n` for a handoff left by an earlier run.
const RESTART_PROBE_EXTRA: usize = 4;
/// The most recent rows the walk keeps whatever they cost (`_MAX_TAIL_MESSAGE_FLOOR`).
const MAX_TAIL_FLOOR: usize = 8;
/// Under pressure, pass 4 keeps this many trailing messages verbatim before last-resort demotion.
const PRESSURE_KEEP_RECENT: usize = 3;
const ROW_OVERHEAD_TOKENS: usize = 10;
const PRUNE_MIN_CHARS: usize = 200;
const SKILL_PRUNE_RECENT_WINDOW: usize = 10;
const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool output cleared to save context space]";
const DUPLICATE_TOOL_PLACEHOLDER: &str =
    "[Duplicate tool output — same content as a more recent call]";
const SKILL_PRUNED_MARKER_PREFIX: &str = "[SKILL_PRUNED:";

/// Rows the reference treats as its own scaffolding rather than a human turn.
const SYNTHETIC_TEXTS: [&str; 11] = [
    "Continue from the compressed conversation context above. This marker exists because no human user turn was available.",
    "Continue from the compressed conversation context above. This marker exists because the compacted transcript contained no preserved user turn.",
    "You've reached the maximum number of tool-calling iterations allowed. Please provide a final response summarizing what you've found and accomplished so far, without calling any more tools.",
    "[System: Your previous response contained only internal reasoning and never produced a visible answer or tool call. Do not keep thinking. Produce your final answer as plain text now (or make the tool call you were planning).]",
    "[System: Continue now. Execute the required tool calls and only send your final answer after completing the task.]",
    "[System: Your previous message ended the turn with a fragment that is not a usable answer. If the task is unfinished, continue it and then give the complete answer. If that fragment WAS your complete answer, send it again exactly as before.]",
    "Your previous turn indicated a tool call but none was included. Do not narrate a plan or restate intent — issue the actual tool call now to continue the task.",
    "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.",
    "[System: The previous response was cut off by a network error mid-stream. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]",
    "[System: Your previous response was truncated by the output length limit. Continue exactly where you left off. Do not restart or repeat prior text. Finish the answer directly.]",
    "[System: The previous response was cut off by a network error mid-stream — a transport interruption, NOT a change in your capabilities. Your tools are still fully available; call them as normal and ignore any earlier claim that you lack tool access. Continue the task from where you left off. Do not restart or repeat prior text. Finish the answer directly.]",
];
const SYNTHETIC_PREFIXES: [&str; 3] = [
    "[IMPORTANT: Background process ",
    "[Your active task list was preserved across context compression]\n",
    "[System: Your previous tool call ",
];

/// What the planner needs from the compression settings.
#[derive(Clone, Copy)]
pub(crate) struct WindowRules {
    pub(crate) protect_first: usize,
    pub(crate) protect_last: usize,
    /// The tail token budget (`tail_token_budget`).
    pub(crate) tail_budget: usize,
    /// The context window the budget and the soft ceiling are held to.
    pub(crate) context_length: usize,
    pub(crate) min_tail_users: usize,
    /// A pass already ran: `compression_count >= 1`, so the head is only the system prompt.
    pub(crate) after_pass: bool,
}

/// The reference's `estimate_tokens_rough`: CJK and Hangul count one token a character, the rest one
/// token per four UTF-8 bytes, rounded up.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.is_ascii() {
        return text.len().div_ceil(4);
    }
    let dense = text.chars().filter(|c| is_dense(*c)).count();
    let rest: usize = text
        .chars()
        .filter(|c| !is_dense(*c))
        .map(char::len_utf8)
        .sum();
    dense + rest.div_ceil(4)
}

fn is_dense(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x2E80..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// `str()` of a JSON value as Python prints the dict it was loaded into.
pub(crate) fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => python_str(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, value)| format!("{}: {}", python_str(key), python_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn python_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {
                if (c as u32) < 0x100 {
                    out.push_str(&format!("\\x{:02x}", c as u32));
                } else {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn content_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.as_str()),
                Value::Object(_) => part["text"].as_str(),
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Characters of a replay field as the reference measures it: a string by length, other values by
/// their JSON text.
fn serialized_chars(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::String(text) => text.chars().count(),
        other => other.to_string().chars().count(),
    }
}

fn thinking_chars(value: &Value) -> usize {
    let parts: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        Value::String(text) => return text.chars().count(),
        _ => return 0,
    };
    parts
        .iter()
        .map(|part| match part {
            Value::String(text) => text.chars().count(),
            Value::Object(_) => ["thinking", "text", "summary"]
                .iter()
                .filter_map(|key| part[*key].as_str())
                .map(|text| text.chars().count())
                .sum(),
            _ => 0,
        })
        .sum()
}

/// The reference's `_estimate_msg_budget_tokens`. Thinking fields are charged for the newest
/// assistant turn only, as on a route that does not echo stale thinking.
pub(crate) fn row_tokens(message: &Value, charge_thinking: bool) -> usize {
    let text_tokens = match &message["content"] {
        Value::String(text) => estimate_tokens(text),
        Value::Array(parts) => {
            parts
                .iter()
                .map(|part| match part {
                    Value::String(text) => text.chars().count(),
                    Value::Object(_) => part["text"].as_str().map_or(0, |t| t.chars().count()),
                    _ => 0,
                })
                .sum::<usize>()
                / 4
        }
        Value::Null => 0,
        other => other.to_string().chars().count() / 4,
    };
    let mut tokens = text_tokens + ROW_OVERHEAD_TOKENS;
    if let Some(calls) = message["tool_calls"].as_array() {
        tokens += calls
            .iter()
            .filter(|call| call.is_object())
            .map(|call| estimate_tokens(&python_repr(call)))
            .sum::<usize>();
    }
    if !charge_thinking {
        return tokens;
    }
    let echoed = message["reasoning_content"]
        .as_str()
        .is_some_and(|text| !text.trim().is_empty());
    for key in ["reasoning", "reasoning_content"] {
        if key == "reasoning" && echoed {
            continue;
        }
        tokens += serialized_chars(&message[key]) / 4;
    }
    let has_text = |value: &Value| match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => true,
    };
    if !(has_text(&message["reasoning"]) || has_text(&message["reasoning_content"])) {
        tokens += thinking_chars(&message["reasoning_details"]) / 4;
    }
    tokens
}

/// How a message relates to a compaction handoff: `Standalone` is the handoff itself, `Merged`
/// carries prior content before the handoff.
#[derive(PartialEq, Eq)]
enum Handoff {
    Standalone,
    Merged,
}

fn starts_with_prefix(text: &str) -> bool {
    SUMMARY_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn classify(content: &Value) -> Option<Handoff> {
    let text = content_text(content);
    let text = text.trim_start();
    if let Some((_, after)) = text.split_once(MERGED_DELIMITER) {
        return starts_with_prefix(after.trim_start()).then_some(Handoff::Merged);
    }
    starts_with_prefix(text).then_some(Handoff::Standalone)
}

/// A handoff by metadata flag or by content.
pub(crate) fn is_summary_message(message: &Value) -> bool {
    truthy_other(&message["_compressed_summary"]) || classify(&message["content"]).is_some()
}

fn truthy_other(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Bool(true) => true,
    }
}

fn is_blank_user(message: &Value) -> bool {
    if message["role"] != "user" || is_summary_message(message) {
        return false;
    }
    match &message["content"] {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(parts) => parts.iter().all(|part| match part {
            Value::String(text) => text.trim().is_empty(),
            Value::Object(_) => {
                matches!(part["type"].as_str(), Some("text" | "input_text"))
                    && part["text"].as_str().is_some_and(|t| t.trim().is_empty())
            }
            _ => false,
        }),
        _ => false,
    }
}

fn is_synthetic_user(message: &Value) -> bool {
    if message["role"] != "user" {
        return false;
    }
    if is_summary_message(message) {
        return true;
    }
    let text = content_text(&message["content"]);
    let text = text.trim();
    SYNTHETIC_TEXTS.contains(&text) || SYNTHETIC_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// An actionable user turn that is not scaffolding (`_is_real_user_turn`).
fn is_real_user(message: &Value) -> bool {
    if message["role"] != "user" {
        return false;
    }
    let display = &message["display_kind"];
    if truthy_other(display) && display.as_str() != Some("steer") {
        return false;
    }
    !is_blank_user(message) && !is_summary_message(message) && !is_synthetic_user(message)
}

fn is_text_only(content: &Value) -> bool {
    match content {
        Value::String(_) => true,
        Value::Array(parts) => parts.iter().all(|part| {
            part.is_string()
                || (matches!(part["type"].as_str(), Some("text" | "input_text"))
                    && part["text"].is_string())
        }),
        _ => false,
    }
}

fn has_tool_calls(message: &Value) -> bool {
    message["tool_calls"]
        .as_array()
        .is_some_and(|c| !c.is_empty())
        || (!message["tool_calls"].is_null() && truthy_other(&message["tool_calls"]))
}

struct Planner<'a> {
    messages: &'a [Value],
    rules: &'a WindowRules,
}

impl Planner<'_> {
    fn len(&self) -> usize {
        self.messages.len()
    }

    fn role(&self, index: usize) -> &str {
        self.messages[index]["role"].as_str().unwrap_or("")
    }

    /// `_effective_protect_first_n`: nothing once a handoff exists in the head or after a pass.
    fn effective_first(&self) -> usize {
        if self.rules.after_pass {
            return 0;
        }
        if self.rules.protect_first > 0 {
            let first = usize::from(self.len() > 0 && self.role(0) == "system");
            let end = self
                .len()
                .min(first + self.rules.protect_first + RESTART_PROBE_EXTRA);
            if self.messages[first.min(end)..end]
                .iter()
                .any(is_summary_message)
            {
                return 0;
            }
        }
        self.rules.protect_first
    }

    fn head_size(&self) -> usize {
        usize::from(self.len() > 0 && self.role(0) == "system") + self.effective_first()
    }

    fn align_forward(&self, mut index: usize) -> usize {
        while index < self.len() && self.role(index) == "tool" {
            index += 1;
        }
        index
    }

    /// Pull a cut back so that a tool group is not split.
    fn align_backward(&self, index: usize) -> usize {
        if index == 0 || index >= self.len() {
            return index;
        }
        let parent = (0..index).rev().find(|&i| self.role(i) != "tool");
        match parent {
            Some(check)
                if self.role(check) == "assistant" && has_tool_calls(&self.messages[check]) =>
            {
                check
            }
            _ => index,
        }
    }

    fn last_user(&self, head_end: usize) -> Option<usize> {
        (head_end..self.len())
            .rev()
            .find(|&i| is_real_user(&self.messages[i]))
    }

    fn last_assistant(&self, head_end: usize) -> Option<usize> {
        let mut last_any = None;
        for index in (head_end..self.len()).rev() {
            let message = &self.messages[index];
            if message["role"] != "assistant" || is_summary_message(message) {
                continue;
            }
            last_any.get_or_insert(index);
            let visible = match &message["content"] {
                Value::String(text) => !text.trim().is_empty(),
                Value::Array(parts) => parts.iter().any(|part| {
                    part.is_object()
                        && part["text"]
                            .as_str()
                            .or_else(|| part["content"].as_str())
                            .is_some_and(|t| !t.trim().is_empty())
                }),
                _ => false,
            };
            if visible {
                return Some(index);
            }
        }
        last_any
    }

    fn newest_assistant(&self) -> Option<usize> {
        (0..self.len()).rev().find(|&i| self.role(i) == "assistant")
    }

    fn price(&self, index: usize, newest_assistant: Option<usize>) -> usize {
        row_tokens(&self.messages[index], Some(index) == newest_assistant)
    }

    /// `_walk_tail_budget`: the first protected index, and what the walk accumulated.
    fn walk(
        &self,
        head_end: usize,
        ceiling: usize,
        min_tail: usize,
        cut_at_break: bool,
        zero_through: Option<usize>,
    ) -> (usize, usize) {
        let n = self.len();
        let newest = self.newest_assistant();
        let mut accumulated = 0;
        let mut cut = n;
        for index in (head_end..n).rev() {
            let tokens = if zero_through.is_some_and(|newest_handoff| index <= newest_handoff) {
                0
            } else {
                self.price(index, newest)
            };
            if accumulated + tokens > ceiling && n - index >= min_tail {
                return (if cut_at_break { index } else { cut }, accumulated);
            }
            accumulated += tokens;
            cut = index;
        }
        (cut, accumulated)
    }

    fn soft_ceiling(&self, budget: usize) -> usize {
        let mut ceiling = budget * 3 / 2;
        if self.rules.context_length > 0 {
            ceiling = ceiling.min(self.rules.context_length / 5);
        }
        ceiling.max(budget)
    }

    fn newest_handoff(&self, head_end: usize) -> Option<usize> {
        (head_end..self.len())
            .rev()
            .find(|&i| is_summary_message(&self.messages[i]))
    }

    /// `_ensure_last_user_message_in_tail`.
    fn anchor_last_user(&self, cut: usize, head_end: usize) -> usize {
        let Some(user) = self.last_user(head_end) else {
            return cut;
        };
        if user >= cut {
            return cut;
        }
        let adjusted = user.max(head_end + 1);
        if adjusted > user {
            // The head clamp would strand the request without its reply: summarize the whole pair.
            let next = user + 1;
            let pair_end = if next >= self.len() || self.role(next) != "assistant" {
                next
            } else {
                self.align_forward(next + 1)
            };
            return pair_end.max(head_end + 1);
        }
        adjusted
    }

    fn anchor_last_assistant(&self, cut: usize, head_end: usize) -> usize {
        match self.last_assistant(head_end) {
            Some(index) if index < cut => self.align_backward(index).max(head_end + 1),
            _ => cut,
        }
    }

    fn anchor_last_users(&self, cut: usize, head_end: usize, count: usize) -> usize {
        let users: Vec<usize> = (head_end..self.len())
            .rev()
            .filter(|&i| is_real_user(&self.messages[i]))
            .collect();
        match users.get(count.min(users.len()).wrapping_sub(1)) {
            Some(&oldest) if oldest < cut => oldest.max(head_end + 1),
            _ => cut,
        }
    }

    /// `tail_cut_progress_floor`.
    fn progress_floor(&self, head_end: usize, cut: usize) -> usize {
        let floor = head_end + 1;
        let Some(newest) = self.newest_handoff(head_end) else {
            return floor;
        };
        let upto = cut.max(newest + 1).min(self.len());
        if !(head_end..upto).all(|i| is_summary_message(&self.messages[i])) {
            return floor;
        }
        let raised = self.align_forward(newest + 2);
        let last_user = self.last_user(newest + 1);
        if raised >= self.len() || last_user.is_some_and(|user| user < raised) {
            floor
        } else {
            raised
        }
    }

    /// `_find_tail_cut_by_tokens`.
    fn tail_cut(&self, head_end: usize) -> usize {
        let n = self.len();
        let rules = self.rules;
        let available = n.saturating_sub(head_end + 1);
        let floor = 3.max(rules.protect_last.min(MAX_TAIL_FLOOR));
        let cap = 3.max(available.saturating_sub(2));
        let min_tail = if available > 1 {
            floor.min(cap).min(available)
        } else {
            0
        };
        let soft = self.soft_ceiling(rules.tail_budget);
        let walk_floor = if soft >= min_tail * ROW_OVERHEAD_TOKENS {
            0
        } else {
            min_tail
        };
        let handoff = self.newest_handoff(head_end);
        let (mut cut, accumulated) = self.walk(head_end, soft, walk_floor, false, handoff);
        if cut <= head_end && accumulated > 0 && accumulated <= soft {
            cut = self
                .walk(head_end, rules.tail_budget, min_tail, true, handoff)
                .0;
        }
        let fallback = n.saturating_sub(min_tail);
        cut = cut.min(n.saturating_sub(walk_floor.max(1)));
        if cut <= head_end {
            cut = fallback.max(head_end + 1);
        }
        cut = self.align_backward(cut);
        let last_user = self.last_user(head_end);
        let user_anchored = self.anchor_last_user(cut, head_end);
        let split = user_anchored < cut
            && last_user.is_some_and(|user| {
                is_text_only(&self.messages[user]["content"])
                    && row_tokens(&self.messages[user], true) <= soft
                    && (user..cut).any(|i| has_tool_calls(&self.messages[i]))
                    && self.walk(user_anchored, soft, 0, false, None).0 > user_anchored
            });
        if !split {
            cut = user_anchored;
            let assistant = self.anchor_last_assistant(cut, head_end);
            let newest = self.newest_assistant();
            let oversized = assistant < cut
                && (assistant..cut).any(|i| has_tool_calls(&self.messages[i]))
                && (assistant..cut)
                    .map(|i| self.price(i, newest))
                    .sum::<usize>()
                    > soft;
            if !oversized {
                cut = assistant;
            }
        }
        if rules.min_tail_users > 1 {
            cut = self.anchor_last_users(cut, head_end, rules.min_tail_users);
        }
        n.min(self.align_forward(cut.max(self.progress_floor(head_end, cut))))
    }

    /// `_compress_window`: the span before the handoff scan.
    fn compress_window(&self) -> (usize, usize) {
        let start = self.align_forward(self.head_size());
        let mut end = self.tail_cut(start);
        // Keep an older assistant or tool bridge ahead of the active request so the summary never
        // lands against it, unless that would leave only handoff rows to summarize.
        if let Some(latest) = self.last_user(0) {
            if end == latest && latest > 0 {
                let bridge = latest - 1;
                let candidate = match self.role(bridge) {
                    "tool" => Some(self.align_backward(latest)),
                    "assistant" => Some(bridge),
                    _ => None,
                };
                if let Some(candidate) = candidate {
                    if candidate >= self.progress_floor(start, candidate) {
                        end = candidate;
                    }
                }
            }
        }
        (start, end)
    }

    /// The prior content a merged handoff carried, as `_strip_context_summary_handoff_message`
    /// unwraps it; `None` for a standalone handoff or a handoff row without live content.
    fn unwrapped_prior(message: &Value) -> bool {
        let Value::String(content) = &message["content"] else {
            return false;
        };
        let prior = if let Some((before, _)) = content.split_once(MERGED_DELIMITER) {
            let before = before.trim();
            before
                .strip_prefix(PRIOR_HEADER)
                .map_or(before, str::trim_start)
                .to_owned()
        } else if let Some((_, after)) = content.split_once(END_MARKER) {
            after.trim_start().to_owned()
        } else {
            String::new()
        };
        !prior.is_empty()
    }
}

/// The span `compress()` would summarize, or `None` when it would leave the history alone.
pub(crate) fn plan(messages: &[Value], rules: &WindowRules) -> Option<Range<usize>> {
    plan_counted(messages, rules).map(|(span, _)| span)
}

/// `plan` and the number of rows the summarizer would read: runs `_prune_old_tool_results` on a
/// working copy first (as `_plan_compaction_window` does) so old bulky tool results do not inflate
/// the tail cut. Handoffs inside the span are not read; a merged one contributes the content it
/// carried.
pub(crate) fn plan_counted(
    messages: &[Value],
    rules: &WindowRules,
) -> Option<(Range<usize>, usize)> {
    let pre_planner = Planner { messages, rules };
    if messages.len() <= pre_planner.head_size() + 4 {
        return None;
    }
    let (working, _) = prune_old_tool_results(messages, rules);
    let planner = Planner {
        messages: &working,
        rules,
    };
    let (start, end) = planner.compress_window();
    if start >= end {
        return None;
    }
    // The handoff scan: earlier handoffs inside the window are not summarized again, and one beyond
    // the window pulls the tail start past itself.
    let search_start = usize::from(working.first().is_some_and(|m| m["role"] == "system"));
    let newest = (search_start..working.len())
        .rev()
        .find(|&i| is_summary_message(&working[i]));
    let Some(newest) = newest else {
        return Some((start..end, end - start));
    };
    let live = |index: usize| {
        let row = &working[index];
        !is_summary_message(row) || Planner::unwrapped_prior(row)
    };
    let rows = (start..newest.max(start)).filter(|&i| live(i)).count()
        + usize::from(live(newest))
        + (newest + 1..end).count();
    if rows == 0 {
        return None;
    }
    Some((start..if newest >= end { newest + 1 } else { end }, rows))
}

/// How the reference places the compaction summary relative to its head and tail neighbors
/// (`ContextCompressor._summary_placement`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryPlacement {
    pub summary_role: &'static str,
    pub merge_into_tail: bool,
    pub force_user_leading: bool,
    pub first_tail_visible_idx: Option<usize>,
}

impl SummaryPlacement {
    /// Index in `tail` that absorbs the summary when `merge_into_tail` is true.
    pub fn merge_target_idx(&self) -> usize {
        if self.force_user_leading {
            self.first_tail_visible_idx.unwrap_or(0)
        } else {
            0
        }
    }
}

/// Role as counted by strict chat-template alternation checks (`_template_visible_role`).
/// Tool rows and assistant rows carrying `tool_calls` are exempt.
fn template_visible_role(message: &Value) -> Option<&str> {
    let role = message["role"].as_str()?;
    if role == "tool" || (role == "assistant" && has_tool_calls(message)) {
        None
    } else {
        Some(role)
    }
}

fn last_template_visible_role(messages: &[Value]) -> Option<&str> {
    messages.iter().rev().find_map(template_visible_role)
}

/// Pick the summary row's role and whether it must merge into the first tail row so template-visible
/// alternation holds (`ContextCompressor._summary_placement`).
pub fn plan_summary_placement(
    head: &[Value],
    tail: &[Value],
    compress_start: usize,
) -> SummaryPlacement {
    let last_head_role = if head.is_empty() {
        Some("user")
    } else {
        last_template_visible_role(head)
    };
    let (first_tail_visible_idx, first_tail_role) = tail
        .iter()
        .enumerate()
        .find_map(|(idx, msg)| template_visible_role(msg).map(|role| (Some(idx), Some(role))))
        .unwrap_or((None, None));
    let has_text_user = head
        .iter()
        .chain(tail.iter())
        .any(|msg| msg["role"] == "user" && !content_text(&msg["content"]).trim().is_empty());
    let force_user_leading =
        compress_start == 0 || last_head_role == Some("system") || !has_text_user;
    let mut summary_role = if matches!(last_head_role, None | Some("assistant") | Some("tool"))
        || force_user_leading
    {
        "user"
    } else {
        "assistant"
    };
    let mut merge_into_tail = false;
    if let Some(tail_role) = first_tail_role {
        if tail_role == summary_role {
            let flipped = if summary_role == "user" {
                "assistant"
            } else {
                "user"
            };
            if Some(flipped) != last_head_role && last_head_role.is_some() && !force_user_leading {
                summary_role = flipped;
            } else {
                merge_into_tail = !tail.is_empty();
            }
        }
    }
    SummaryPlacement {
        summary_role,
        merge_into_tail,
        force_user_leading,
        first_tail_visible_idx,
    }
}

/// Format the merged content when `placement.merge_into_tail` is true
/// (`ContextCompressor._merge_summary_into_tail_row`).
pub fn merge_summary_content(
    old_content: Option<&str>,
    summary: &str,
    summary_role: &str,
    force_user_leading: bool,
) -> String {
    let old = old_content.unwrap_or("");
    if force_user_leading && summary_role == "user" {
        format!("{summary}\n\n{END_MARKER}\n\n{old}")
    } else {
        format!("{PRIOR_HEADER}\n{old}\n\n{MERGED_DELIMITER}\n\n{summary}\n\n{END_MARKER}")
    }
}

/// Fold `summary` into `row["content"]` in place (matching `_merge_summary_into_tail_row`).
pub fn merge_summary_into_tail_row(row: &mut Value, summary: &str, placement: &SummaryPlacement) {
    let old = match &row["content"] {
        Value::Null => None,
        Value::String(text) => Some(text.as_str()),
        other => {
            let rendered = other.to_string();
            let merged = merge_summary_content(
                Some(&rendered),
                summary,
                placement.summary_role,
                placement.force_user_leading,
            );
            row["content"] = Value::String(merged);
            return;
        }
    };
    let merged = merge_summary_content(
        old,
        summary,
        placement.summary_role,
        placement.force_user_leading,
    );
    row["content"] = Value::String(merged);
}

/// Port of `ContextCompressor._prune_old_tool_results`: dedupe identical >=200-char tool outputs,
/// replace old >200-char tool outputs before `prune_boundary` with 1-line summaries, and run pass-4
/// pressure demotion inside the protected tail when it alone exceeds the soft ceiling.
pub(crate) fn prune_old_tool_results(
    messages: &[Value],
    rules: &WindowRules,
) -> (Vec<Value>, usize) {
    if messages.is_empty() {
        return (Vec::new(), 0);
    }
    let mut result = messages.to_vec();
    let calls = tool_calls_by_id(&result);
    let mut prune_boundary = prune_boundary_for(&result, rules);
    let spared = spared_pending_tool_round(&result, rules.context_length);
    if !spared.is_empty() {
        prune_boundary = prune_boundary.min(spared.start);
    }
    let mut pruned = dedupe_tool_results(&mut result);
    let protected_skills = collect_protected_skill_names(&result, prune_boundary);
    for idx in 0..prune_boundary {
        if demote_tool_result_at(
            &mut result,
            idx,
            &calls,
            PRUNE_MIN_CHARS,
            Some(&protected_skills),
        ) {
            pruned += 1;
        }
    }
    if rules.tail_budget > 0 && !result.is_empty() {
        pruned += pressure_demote_tail(
            &mut result,
            prune_boundary,
            rules,
            &calls,
            PRUNE_MIN_CHARS,
            &spared,
        );
    }
    (result, pruned)
}

fn prune_boundary_for(messages: &[Value], rules: &WindowRules) -> usize {
    let n = messages.len();
    if rules.tail_budget == 0 {
        return n.saturating_sub(rules.protect_last);
    }
    let min_protect = rules.protect_last.min(n).min(MAX_TAIL_FLOOR);
    let planner = Planner { messages, rules };
    let (boundary, _) = planner.walk(0, rules.tail_budget, min_protect, true, None);
    boundary.min(n.saturating_sub(min_protect))
}

fn pending_tool_round(messages: &[Value]) -> Range<usize> {
    let mut end = messages.len();
    while end > 0 && messages[end - 1]["display_kind"].as_str() == Some("steer") {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && messages[start - 1]["role"].as_str() == Some("tool") {
        start -= 1;
    }
    start..end
}

fn spared_pending_tool_round(messages: &[Value], context_length: usize) -> Range<usize> {
    let mut pending = pending_tool_round(messages);
    if !pending.is_empty() && pending.start > 0 && has_tool_calls(&messages[pending.start - 1]) {
        pending.start -= 1;
    }
    if pending.is_empty() {
        return 0..0;
    }
    let hard_share = context_length / 5;
    let tokens: usize = pending
        .clone()
        .map(|i| row_tokens(&messages[i], true))
        .sum();
    if tokens <= hard_share {
        pending
    } else {
        0..0
    }
}

fn dedupe_tool_results(result: &mut [Value]) -> usize {
    let mut pruned = 0;
    let mut seen: HashSet<String> = HashSet::new();
    for msg in result.iter_mut().rev() {
        if msg["role"].as_str() != Some("tool") {
            continue;
        }
        let Some(content) = msg["content"].as_str() else {
            continue;
        };
        if content.chars().count() < PRUNE_MIN_CHARS {
            continue;
        }
        if !seen.insert(content.to_owned()) {
            msg["content"] = Value::String(DUPLICATE_TOOL_PLACEHOLDER.to_owned());
            pruned += 1;
        }
    }
    pruned
}

fn tool_calls_by_id(messages: &[Value]) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for msg in messages {
        if msg["role"].as_str() != Some("assistant") {
            continue;
        }
        let Some(calls) = msg["tool_calls"].as_array() else {
            continue;
        };
        for call in calls {
            let id = call["id"].as_str().unwrap_or("").to_owned();
            let func = &call["function"];
            let name = func["name"].as_str().unwrap_or("unknown").to_owned();
            let args = func["arguments"].as_str().unwrap_or("").to_owned();
            map.insert(id, (name, args));
        }
    }
    map
}

fn collect_protected_skill_names(messages: &[Value], prune_boundary: usize) -> HashSet<String> {
    let total = messages.len();
    if total == 0 {
        return HashSet::new();
    }
    let recent_start = total.saturating_sub(SKILL_PRUNE_RECENT_WINDOW);
    let tail_start = prune_boundary.min(total);
    let tail_user_texts: Vec<String> = messages[tail_start..]
        .iter()
        .filter(|m| m["role"].as_str() == Some("user"))
        .filter_map(|m| m["content"].as_str().filter(|s| !s.is_empty()))
        .map(str::to_lowercase)
        .collect();
    let mut protected = HashSet::new();
    for (idx, msg) in messages.iter().enumerate() {
        if msg["role"].as_str() != Some("assistant") {
            continue;
        }
        let Some(calls) = msg["tool_calls"].as_array() else {
            continue;
        };
        for call in calls {
            let func = &call["function"];
            if func["name"].as_str() != Some("skill_view") {
                continue;
            }
            let Some(args_str) = func["arguments"].as_str() else {
                continue;
            };
            let args = json_dict(args_str);
            let Some(skill) = args
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let lower = skill.to_lowercase();
            if idx >= recent_start.min(tail_start)
                || tail_user_texts.iter().any(|text| text.contains(&lower))
            {
                protected.insert(lower);
            }
        }
    }
    protected
}

fn is_summary_stub(content: &str) -> bool {
    (content.starts_with('[')
        && (content.contains(" chars)") || content.contains(" chars result)"))
        && content.chars().count() < 400)
        || content.starts_with("[Tool output archived")
}

fn demote_tool_result_at(
    result: &mut [Value],
    idx: usize,
    calls: &HashMap<String, (String, String)>,
    min_prune_chars: usize,
    protected_skills: Option<&HashSet<String>>,
) -> bool {
    let msg = &result[idx];
    if msg["role"].as_str() != Some("tool") {
        return false;
    }
    let Some(content) = msg["content"].as_str() else {
        return false;
    };
    if content.is_empty()
        || content == PRUNED_TOOL_PLACEHOLDER
        || content.starts_with("[Duplicate tool output")
        || content.starts_with("[screenshot removed")
        || is_summary_stub(content)
        || content.chars().count() <= min_prune_chars
    {
        return false;
    }
    let call_id = msg["tool_call_id"].as_str().unwrap_or("");
    let (tool_name, tool_args) = calls
        .get(call_id)
        .map(|(n, a)| (n.as_str(), a.as_str()))
        .unwrap_or(("unknown", ""));
    if let Some(protected) = protected_skills {
        if tool_name == "skill_view" {
            if let Some(skill) = json_dict(tool_args).get("name").and_then(Value::as_str) {
                if protected.contains(&skill.to_lowercase()) {
                    return false;
                }
            }
        }
    }
    let summary = summarize_tool_result(tool_name, tool_args, content);
    result[idx]["content"] = Value::String(summary);
    true
}

fn pressure_demote_tail(
    result: &mut [Value],
    prune_boundary: usize,
    rules: &WindowRules,
    calls: &HashMap<String, (String, String)>,
    min_prune_chars: usize,
    spared: &Range<usize>,
) -> usize {
    let soft = {
        let mut ceiling = rules.tail_budget * 3 / 2;
        if rules.context_length > 0 {
            ceiling = ceiling.min(rules.context_length / 5);
        }
        ceiling.max(rules.tail_budget)
    };
    let n = result.len();
    let demote_end = n.saturating_sub(PRESSURE_KEEP_RECENT.min(n));
    let start = prune_boundary;
    let region_tokens = |rows: &[Value]| -> usize {
        (start..rows.len())
            .map(|i| row_tokens(&rows[i], true))
            .sum()
    };
    if demote_end <= prune_boundary || region_tokens(result) <= soft {
        return 0;
    }
    let mut demoted = 0;
    for i in start..demote_end {
        if !spared.contains(&i) && demote_tool_result_at(result, i, calls, min_prune_chars, None) {
            demoted += 1;
        }
        if region_tokens(result) <= soft {
            break;
        }
    }
    if region_tokens(result) > soft {
        let last_tool = (0..n)
            .rev()
            .find(|&i| result[i]["role"].as_str() == Some("tool"));
        for i in start..n {
            if Some(i) == last_tool || spared.contains(&i) {
                continue;
            }
            if demote_tool_result_at(result, i, calls, min_prune_chars, None) {
                demoted += 1;
            }
        }
        if let Some(last) = last_tool {
            if !spared.contains(&last)
                && last >= prune_boundary
                && region_tokens(result) > soft
                && demote_tool_result_at(result, last, calls, min_prune_chars, None)
            {
                demoted += 1;
            }
        }
    }
    demoted
}

fn json_dict(text: &str) -> serde_json::Map<String, Value> {
    if text.is_empty() {
        return serde_json::Map::new();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

fn format_commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn py_val_str(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => python_repr(other),
    }
}

fn str_arg(args: &serde_json::Map<String, Value>, key: &str, default: &str) -> String {
    match args.get(key) {
        None | Some(Value::Null) => default.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_val_str(other),
    }
}

fn take_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn failure_suffix_labeled(reason: Option<&Value>, label: &str) -> String {
    let text = match reason {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) if s.is_empty() => String::new(),
        Some(other) => {
            let rendered = py_val_str(other);
            let collapsed = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
            take_chars(&collapsed, 80)
        }
    };
    if text.is_empty() {
        format!(" {label}")
    } else {
        format!(" {label}: {text}")
    }
}

fn payload_failure_suffix(payload: &serde_json::Map<String, Value>) -> String {
    let error = payload.get("error").filter(|v| truthy_other(v));
    let failed = payload.get("success") == Some(&Value::Bool(false));
    if error.is_none() && !failed {
        return String::new();
    }
    failure_suffix_labeled(error, "FAILED")
}

fn result_failure_suffix(content: &str) -> String {
    payload_failure_suffix(&json_dict(content))
}

fn summarize_refused_tool_result(
    tool_name: &str,
    args: &serde_json::Map<String, Value>,
    content: &str,
) -> Option<String> {
    let payload = json_dict(content);
    let status = payload.get("status").and_then(Value::as_str);
    let mut error = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if error.is_empty() && content.trim_start().starts_with("BLOCKED") {
        error = content.trim().to_owned();
    }
    let outcome = if status == Some("pending_approval") {
        "awaiting the user's approval, not run".to_owned()
    } else if !error.is_empty()
        && (status == Some("blocked") || error.trim_start().starts_with("BLOCKED"))
    {
        let mut s = "BLOCKED, not run".to_owned();
        if error.contains("NOT consented") {
            s.push_str(
                "; the user did NOT consent, do not retry or reach the same outcome another way",
            );
        }
        s
    } else {
        return None;
    };
    let raw_target = {
        let cmd = str_arg(args, "command", "");
        if !cmd.is_empty() {
            cmd
        } else {
            str_arg(args, "path", "")
        }
    };
    let target = if raw_target.is_empty() {
        String::new()
    } else if raw_target.chars().count() <= 60 {
        format!(" `{raw_target}`")
    } else {
        format!(" `{}...`", take_chars(&raw_target, 57))
    };
    Some(take_chars(
        &format!("[{tool_name}]{target} {outcome}"),
        PRUNE_MIN_CHARS - 1,
    ))
}

fn extract_json_int(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = content.find(&needle)?;
    let rest = content[pos + needle.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let mut chars = rest.chars().peekable();
    let mut num = String::new();
    if matches!(chars.peek(), Some('-')) {
        num.push(chars.next().unwrap());
    }
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            num.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if num.is_empty() || num == "-" {
        None
    } else {
        Some(num)
    }
}

fn stub_arguments(args: &serde_json::Map<String, Value>) -> String {
    let mut out = String::new();
    for (key, value) in args {
        let raw = py_val_str(value);
        let one_line = raw.lines().collect::<Vec<_>>().join(" ");
        let preview = take_chars(&one_line, 40);
        out.push_str(&format!(" {key}={preview}"));
        if out.chars().count() >= 120 {
            return take_chars(&out, 120);
        }
    }
    out
}

/// Port of `_summarize_tool_result` from `agent/context_compressor.py`.
fn summarize_tool_result(tool_name: &str, tool_args: &str, content: &str) -> String {
    let args = json_dict(tool_args);
    if let Some(refused) = summarize_refused_tool_result(tool_name, &args, content) {
        return refused;
    }
    let content_len = content.chars().count();
    let line_count = if content.trim().is_empty() {
        0
    } else {
        content.bytes().filter(|&b| b == b'\n').count() + 1
    };
    let len_fmt = format_commas(content_len);
    match tool_name {
        "terminal" => {
            let raw_cmd = str_arg(&args, "command", "");
            let cmd_joined = raw_cmd.lines().collect::<Vec<_>>().join(" ");
            let cmd = if cmd_joined.chars().count() <= 80 {
                cmd_joined
            } else {
                format!("{}...", take_chars(&cmd_joined, 77))
            };
            let exit_code = extract_json_int(content, "exit_code").unwrap_or_else(|| "?".into());
            let payload = json_dict(content);
            let output_lines: Vec<&str> = payload
                .get("output")
                .and_then(Value::as_str)
                .map(|s| s.lines().collect())
                .unwrap_or_default();
            let count = if payload.get("output").and_then(Value::as_str).is_some() {
                output_lines.len()
            } else {
                line_count
            };
            let word = if count == 1 { "line" } else { "lines" };
            let mut stub =
                format!("[terminal] ran `{cmd}` -> exit {exit_code}, {count} {word} output");
            let last = output_lines
                .iter()
                .rev()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            let overhead = stub.chars().count() + " | last: ".len();
            let room = (PRUNE_MIN_CHARS - 1).saturating_sub(overhead).min(120);
            if exit_code != "?" && exit_code != "0" && !last.is_empty() && room >= 20 {
                stub.push_str(" | last: ");
                stub.push_str(&take_chars(last, room));
            }
            stub
        }
        "read_file" => {
            let path = str_arg(&args, "path", "?");
            let offset = str_arg(&args, "offset", "1");
            format!(
                "[read_file] read {path} from line {offset} ({len_fmt} chars){}",
                result_failure_suffix(content)
            )
        }
        "write_file" => {
            let path = str_arg(&args, "path", "?");
            let failed = result_failure_suffix(content);
            if !failed.is_empty() {
                return format!("[write_file] {path}{failed}");
            }
            let written_lines = match args.get("content") {
                Some(v) if truthy_other(v) => {
                    let s = str_arg(&args, "content", "");
                    (s.bytes().filter(|&b| b == b'\n').count() + 1).to_string()
                }
                _ => "?".into(),
            };
            format!("[write_file] wrote to {path} ({written_lines} lines)")
        }
        "search_files" => {
            let count = extract_json_int(content, "total_count")
                .filter(|c| !c.starts_with('-'))
                .unwrap_or_else(|| "?".into());
            let target = str_arg(&args, "target", "content");
            let pattern = str_arg(&args, "pattern", "?");
            let path = str_arg(&args, "path", ".");
            format!("[search_files] {target} search for '{pattern}' in {path} -> {count} matches")
        }
        "patch" => {
            let mode = str_arg(&args, "mode", "replace");
            let path = str_arg(&args, "path", "?");
            format!(
                "[patch] {mode} in {path} ({len_fmt} chars result){}",
                result_failure_suffix(content)
            )
        }
        "browser_navigate" | "browser_click" | "browser_snapshot" | "browser_type"
        | "browser_scroll" | "browser_vision" => {
            let url = str_arg(&args, "url", "");
            let r = str_arg(&args, "ref", "");
            let detail = if !url.is_empty() {
                format!(" {url}")
            } else if !r.is_empty() {
                format!(" ref={r}")
            } else {
                String::new()
            };
            format!(
                "[{tool_name}]{detail} ({len_fmt} chars){}",
                result_failure_suffix(content)
            )
        }
        "web_search" => {
            let query = str_arg(&args, "query", "?");
            format!(
                "[web_search] query='{query}' ({len_fmt} chars result){}",
                result_failure_suffix(content)
            )
        }
        "web_extract" => {
            let urls = args.get("urls").and_then(Value::as_array);
            let mut first = match urls.and_then(|u| u.first()) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Object(map)) => map
                    .get("url")
                    .or_else(|| map.get("href"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                _ => "?".into(),
            };
            if let Some(list) = urls {
                if list.len() > 1 {
                    first.push_str(&format!(" (+{} more)", list.len() - 1));
                }
            }
            format!(
                "[web_extract] {first} ({len_fmt} chars){}",
                result_failure_suffix(content)
            )
        }
        "delegate_task" => {
            let tasks = args
                .get("tasks")
                .and_then(Value::as_array)
                .filter(|t| !t.is_empty());
            let goal_source = tasks
                .and_then(|t| t.first())
                .and_then(Value::as_object)
                .unwrap_or(&args);
            let raw_goal = str_arg(goal_source, "goal", "");
            let joined = raw_goal.lines().collect::<Vec<_>>().join(" ");
            let goal = if joined.chars().count() <= 60 {
                joined
            } else {
                format!("{}...", take_chars(&joined, 57))
            };
            let detail = match tasks {
                Some(list) => format!("{} tasks, first: '{goal}'", list.len()),
                None => format!("'{goal}'"),
            };
            format!(
                "[delegate_task] {detail} ({len_fmt} chars result){}",
                result_failure_suffix(content)
            )
        }
        "execute_code" => {
            let code_str = str_arg(&args, "code", "");
            let head = take_chars(&code_str, 60).replace('\n', " ");
            let suffix = if code_str.chars().count() > 60 {
                "..."
            } else {
                ""
            };
            format!("[execute_code] `{head}{suffix}` ({line_count} lines output)")
        }
        "skill_view" => {
            let skill = str_arg(&args, "name", "?");
            let failed = {
                let s = result_failure_suffix(content);
                if !s.is_empty() {
                    s
                } else if content.starts_with("[skill_view] ")
                    && content
                        .split(" (")
                        .next()
                        .is_some_and(|h| h.contains(" FAILED"))
                {
                    " FAILED".into()
                } else {
                    String::new()
                }
            };
            let keep = if is_summary_stub(content) {
                content.contains(SKILL_PRUNED_MARKER_PREFIX)
            } else {
                failed.is_empty()
            };
            let marker = if skill != "?" && keep {
                format!(
                    " {SKILL_PRUNED_MARKER_PREFIX} content lost in compression; reload with skill_view(name='{skill}')]"
                )
            } else {
                String::new()
            };
            format!("[skill_view] name={skill}{failed} ({len_fmt} chars){marker}")
        }
        "skills_list" => {
            let category = str_arg(&args, "category", "");
            let scope = if category.is_empty() {
                String::new()
            } else {
                format!(" category={category}")
            };
            let payload = json_dict(content);
            let listed = payload
                .get("count")
                .and_then(Value::as_i64)
                .map_or(String::new(), |c| format!(" {c} skills"));
            format!(
                "[skills_list]{scope}{listed}{} ({len_fmt} chars)",
                result_failure_suffix(content)
            )
        }
        "skill_manage" => {
            let ops = args
                .get("operations")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty());
            let summary = if let Some(list) = ops {
                let mut rendered = Vec::new();
                for op in list {
                    if let Some(map) = op.as_object() {
                        let action = str_arg(map, "action", "?");
                        let op_name = str_arg(map, "name", "?");
                        rendered.push(format!("{action} {op_name}"));
                    }
                }
                let head: String = rendered
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ");
                let mut s = format!("[skill_manage] {head}");
                if list.len() > 3 {
                    s.push_str(&format!(" (+{} more)", list.len() - 3));
                }
                s
            } else {
                let action = str_arg(&args, "action", "?");
                let op_name = str_arg(&args, "name", "?");
                format!("[skill_manage] {action} {op_name}")
            };
            format!(
                "{summary}{} ({len_fmt} chars)",
                result_failure_suffix(content)
            )
        }
        "vision_analyze" => {
            let q = take_chars(&str_arg(&args, "question", ""), 50);
            format!(
                "[vision_analyze] '{q}' ({len_fmt} chars){}",
                result_failure_suffix(content)
            )
        }
        "memory" => {
            let action = str_arg(&args, "action", "?");
            let target = str_arg(&args, "target", "?");
            format!(
                "[memory] {action} on {target}{}",
                result_failure_suffix(content)
            )
        }
        "todo_list" => "[todo] updated task list".into(),
        "clarify" => summarize_clarify(content),
        "text_to_speech" => {
            format!(
                "[text_to_speech] generated audio ({len_fmt} chars){}",
                result_failure_suffix(content)
            )
        }
        "cronjob_manage" => {
            let stub = format!("[cronjob] {}", str_arg(&args, "action", "?"));
            let payload = json_dict(content);
            let mut suffix = payload_failure_suffix(&payload);
            if suffix.is_empty() {
                if let Some(job) = payload.get("job").and_then(Value::as_object) {
                    let error = job.get("execution_error").filter(|v| !v.is_null());
                    let skipped = job.get("execution_skipped").filter(|v| !v.is_null());
                    if error.is_some()
                        || (skipped.is_none()
                            && job.get("execution_success") == Some(&Value::Bool(false)))
                    {
                        suffix = failure_suffix_labeled(error, "FAILED");
                    } else if skipped.is_some() {
                        suffix = failure_suffix_labeled(skipped, "SKIPPED");
                    }
                }
            }
            format!("{stub}{suffix}")
        }
        "process_manage" => {
            let stub = format!(
                "[process] {} session={}",
                str_arg(&args, "action", "?"),
                str_arg(&args, "session_id", "?")
            );
            let payload = json_dict(content);
            let mut suffix = payload_failure_suffix(&payload);
            if suffix.is_empty() {
                if let Some(code) = payload.get("exit_code").and_then(Value::as_i64) {
                    if code != 0
                        && payload.get("completion_reason").and_then(Value::as_str)
                            != Some("killed")
                    {
                        suffix = format!(" FAILED: exit code {code}");
                    }
                }
            }
            format!("{stub}{suffix}")
        }
        _ => {
            let first_arg = stub_arguments(&args);
            format!(
                "[{tool_name}]{first_arg} ({len_fmt} chars result){}",
                result_failure_suffix(content)
            )
        }
    }
}

fn summarize_clarify(content: &str) -> String {
    let payload = json_dict(content);
    let responses: Vec<&Value> = match payload.get("responses") {
        Some(Value::Array(list)) => list.iter().collect(),
        _ if payload.contains_key("user_response") => vec![],
        _ => vec![],
    };
    let single = if responses.is_empty() && payload.contains_key("user_response") {
        Some(Value::Object(payload.clone()))
    } else {
        None
    };
    let iter: Vec<&Value> = match &single {
        Some(val) => vec![val],
        None => responses,
    };
    let mut answers: Vec<String> = Vec::new();
    for entry in iter {
        let Some(map) = entry.as_object() else {
            continue;
        };
        let is_answered = map.get("status").and_then(Value::as_str) == Some("answered")
            || (!map.contains_key("status") && map.contains_key("user_response"));
        if !is_answered {
            continue;
        }
        let values: Vec<&Value> = match map.get("user_response") {
            Some(Value::Array(list)) => list.iter().collect(),
            Some(val) => vec![val],
            None => vec![],
        };
        for item in values {
            if let Some(s) = item.as_str().filter(|s| !s.is_empty()) {
                answers.push(s.to_owned());
            }
        }
    }
    if answers.is_empty() {
        return "[clarify] asked user a question".into();
    }
    let serialized = if answers.len() == 1 {
        json!(answers[0]).to_string()
    } else {
        crate::session_search::py_json_dumps(&json!(answers))
    };
    let full = format!("[clarify] user responded: {serialized}");
    let max_len = PRUNE_MIN_CHARS - 1;
    if full.chars().count() <= max_len {
        full
    } else {
        let kept = max_len.saturating_sub(3);
        format!("{}...", take_chars(&full, kept))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_tool_call_is_priced_by_the_python_repr_of_its_dict() {
        // Recorded with `str()` in the reference interpreter.
        let call = json!({"a": "it's", "b": "a\"b", "c": "it's \"x\"", "d": "l1\nl2\t\\",
                          "e": null, "f": true, "g": [1, 2.5, "я"], "h": {}, "i": "\u{1}\u{7f}"});
        assert_eq!(
            python_repr(&call),
            "{'a': \"it's\", 'b': 'a\"b', 'c': 'it\\'s \"x\"', 'd': 'l1\\nl2\\t\\\\', 'e': None, \
             'f': True, 'g': [1, 2.5, 'я'], 'h': {}, 'i': '\\x01\\x7f'}"
        );
    }

    #[test]
    fn tokens_are_estimated_like_the_reference() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // Cyrillic is two bytes a character; CJK and Hangul count a token each.
        assert_eq!(estimate_tokens("яяяя"), 2);
        assert_eq!(estimate_tokens("语言语言"), 4);
        assert_eq!(estimate_tokens("한국어 abcd"), 3 + 2);
    }

    #[test]
    fn a_row_costs_its_text_its_calls_and_ten_tokens_of_overhead() {
        let plain = json!({"role": "user", "content": "x".repeat(40)});
        assert_eq!(row_tokens(&plain, true), 10 + 10);
        let call = json!({"id": "c1", "type": "function"});
        let with_call = json!({"role": "assistant", "content": null, "tool_calls": [call]});
        assert_eq!(
            row_tokens(&with_call, true),
            10 + estimate_tokens(&python_repr(&call))
        );
        // Thinking is charged for the newest assistant turn only.
        let thinking =
            json!({"role": "assistant", "content": "", "reasoning_content": "y".repeat(80)});
        assert_eq!(row_tokens(&thinking, true), 10 + 20);
        assert_eq!(row_tokens(&thinking, false), 10);
        // `reasoning_content` wins: the wire carries one thinking field, not both.
        let both = json!({"role": "assistant", "content": "", "reasoning": "z".repeat(80),
                          "reasoning_content": "y".repeat(80)});
        assert_eq!(row_tokens(&both, true), 10 + 20);
        let only = json!({"role": "assistant", "content": "", "reasoning": "z".repeat(80)});
        assert_eq!(row_tokens(&only, true), 10 + 20);
    }

    #[test]
    fn handoffs_and_scaffolding_are_not_user_turns() {
        let user = |text: &str| json!({"role": "user", "content": text});
        assert!(is_real_user(&user("hello")));
        assert!(!is_real_user(&user("   ")));
        assert!(!is_real_user(&user("[CONTEXT SUMMARY]:\nx")));
        assert!(!is_real_user(&user(
            "[IMPORTANT: Background process p1 done]"
        )));
        assert!(!is_real_user(
            &json!({"role": "user", "content": "x", "_compressed_summary": true})
        ));
        assert!(!is_real_user(&json!({"role": "assistant", "content": "x"})));
    }
}
