//! Which messages a compression keeps at the head, keeps verbatim at the tail and summarizes in
//! between. A port of the reference's `ContextCompressor._compress_window`: the head is the system
//! prompt plus `protect_first_n` rows (none once a handoff exists), the tail is a token budget walked
//! backwards from the newest row, and anchors keep the latest user request, the latest assistant
//! reply and the last `min_tail_user_messages` user turns out of the summary. A tool call and its
//! results are never split. `compat/create_compression_window_corpus.py` records the reference's
//! answer for hundreds of synthetic histories; `tests/compression_window.rs` replays them.
//!
//! Differences from the reference are deliberate and listed in the migration plan: rows are priced
//! as the reference prices them (`estimate_tokens_rough` plus the Python `repr` of each tool call)
//! but the port never prunes old tool results before planning, images and the Codex replay fields
//! cost nothing, and a route that echoes stale thinking is not priced separately.

use std::ops::Range;

use serde_json::Value;

const SUMMARY_PREFIXES: [&str; 3] = [
    "[CONTEXT SUMMARY]:",
    "[CONTEXT COMPACTION — REFERENCE ONLY]",
    "[CONTEXT COMPACTION",
];
const END_MARKER: &str =
    "--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---";
const PRIOR_HEADER: &str = "[PRIOR CONTEXT — for reference only; not a new message]";
const MERGED_DELIMITER: &str = "[END OF PRIOR CONTEXT — COMPACTION SUMMARY BELOW]";
/// Rows the reference looks at past `protect_first_n` for a handoff left by an earlier run.
const RESTART_PROBE_EXTRA: usize = 4;
/// The most recent rows the walk keeps whatever they cost (`_MAX_TAIL_MESSAGE_FLOOR`).
const MAX_TAIL_FLOOR: usize = 8;
const ROW_OVERHEAD_TOKENS: usize = 10;

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

/// `plan` and the number of rows the summarizer would read: handoffs inside the span are not read, a
/// merged one contributes the content it carried.
pub(crate) fn plan_counted(
    messages: &[Value],
    rules: &WindowRules,
) -> Option<(Range<usize>, usize)> {
    let planner = Planner { messages, rules };
    if messages.len() <= planner.head_size() + 4 {
        return None;
    }
    let (start, end) = planner.compress_window();
    if start >= end {
        return None;
    }
    // The handoff scan: earlier handoffs inside the window are not summarized again, and one beyond
    // the window pulls the tail start past itself.
    let search_start = usize::from(messages.first().is_some_and(|m| m["role"] == "system"));
    let newest = (search_start..messages.len())
        .rev()
        .find(|&i| is_summary_message(&messages[i]));
    let Some(newest) = newest else {
        return Some((start..end, end - start));
    };
    let live = |index: usize| {
        let row = &messages[index];
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
