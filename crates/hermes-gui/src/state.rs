//! Pure UI state model for `hermes-gui` (unit-testable without a display server).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use hermes_runtime::context_refs::{complete_at_references, list_skill_commands};

use hermes_runtime::local_owner::{
    ActiveTurnSummary, ProfileEntry, SequencedTurnEvent, SessionSummary, TranscriptItem,
    TurnStreamEvent,
};
use hermes_runtime::turn::{ApprovalAction, ApprovalDecision, ApprovalRequest};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::editor::TextBuffer;
use crate::workspace::AttachedFile;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCardState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedRow {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        detail: Option<String>,
        summary: Option<String>,
        #[allow(dead_code)]
        live_output: Vec<String>,
        state: ToolCardState,
    },
    Notice {
        is_error: bool,
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ClarifyQuestionItem {
    pub qid: String,
    pub question: String,
    pub choices: Option<Vec<String>>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Clone, Debug)]
pub struct PendingClarify {
    pub turn_id: String,
    pub approval_id: u64,
    pub questions: Vec<ClarifyQuestionItem>,
    pub active_index: usize,
    /// Selected choice labels per `qid` (single or multi-select).
    pub selected: BTreeMap<String, BTreeSet<String>>,
    /// Freeform / custom text per `qid`.
    pub custom_answers: BTreeMap<String, TextBuffer>,
}

impl PendingClarify {
    pub fn from_request(turn_id: String, approval_id: u64, raw_json: &str) -> Option<Self> {
        let questions: Vec<ClarifyQuestionItem> = serde_json::from_str(raw_json).ok()?;
        if questions.is_empty() {
            return None;
        }
        let mut custom_answers = BTreeMap::new();
        for q in &questions {
            custom_answers.insert(q.qid.clone(), TextBuffer::new());
        }
        Some(Self {
            turn_id,
            approval_id,
            questions,
            active_index: 0,
            selected: BTreeMap::new(),
            custom_answers,
        })
    }

    pub fn active_buffer_mut(&mut self) -> Option<&mut TextBuffer> {
        let qid = self.questions.get(self.active_index)?.qid.clone();
        Some(self.custom_answers.entry(qid).or_default())
    }

    pub fn toggle_choice(&mut self, qid: &str, choice: &str, multi_select: bool) {
        let entry = self.selected.entry(qid.to_owned()).or_default();
        if multi_select {
            if !entry.remove(choice) {
                entry.insert(choice.to_owned());
            }
        } else {
            entry.clear();
            entry.insert(choice.to_owned());
        }
    }

    pub fn build_submitted_decision(&self) -> ApprovalDecision {
        let mut answers = serde_json::Map::new();
        for q in &self.questions {
            let custom = self
                .custom_answers
                .get(&q.qid)
                .map(|b| b.as_str().trim())
                .unwrap_or("");
            let chosen = self.selected.get(&q.qid);
            let value = if q.multi_select {
                let mut list: Vec<Value> = chosen
                    .into_iter()
                    .flatten()
                    .map(|c| Value::String(c.clone()))
                    .collect();
                if !custom.is_empty() {
                    list.push(Value::String(custom.to_owned()));
                }
                if list.is_empty() {
                    Value::Null
                } else {
                    Value::Array(list)
                }
            } else if !custom.is_empty() {
                Value::String(custom.to_owned())
            } else if let Some(first) = chosen.and_then(|set| set.iter().next()) {
                Value::String(first.clone())
            } else {
                Value::Null
            };
            answers.insert(q.qid.clone(), value);
        }
        ApprovalDecision::Answered(
            json!({
                "answers": answers,
                "outcome": "submitted",
            })
            .to_string(),
        )
    }

    pub fn build_cancelled_decision() -> ApprovalDecision {
        ApprovalDecision::Answered(
            json!({
                "answers": {},
                "outcome": "cancelled",
                "notice": "User cancelled the clarification prompt.",
            })
            .to_string(),
        )
    }
}

#[derive(Clone, Debug)]
pub struct PendingApproval {
    pub turn_id: String,
    pub approval_id: u64,
    pub request: ApprovalRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionItem {
    pub replacement: String,
    pub display: String,
    pub detail: String,
    pub at_start: Option<usize>,
}

const GUI_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands & @ / ! shortcuts"),
    (
        "/cd",
        "switch project working directory (or open folder picker)",
    ),
    ("/attach", "attach file(s) to message / copy into workspace"),
    ("/setup", "open profile setup & provider settings"),
    ("/new", "start a new conversation"),
    ("/reset", "alias for /new"),
    ("/clear", "clear the visible chat feed"),
    ("/undo", "roll back the last exchange in this session"),
    ("/skills", "list or search installed skills"),
    ("/yolo", "toggle auto-approve mode"),
];

#[derive(Clone, Debug)]
pub struct GuiState {
    pub active_profile: PathBuf,
    pub profiles: Vec<ProfileEntry>,
    pub sessions: Vec<SessionSummary>,
    pub workspace_path_input: TextBuffer,
    pub search_input: TextBuffer,
    pub selected_session_id: Option<String>,
    pub selected_session_title: Option<String>,
    pub confirm_delete_session_id: Option<String>,
    pub feed: Vec<FeedRow>,
    pub live_reasoning: Vec<String>,
    pub live_reasoning_partial: String,
    pub active_turn_id: Option<String>,
    pub last_seq: u64,
    pub streaming: bool,
    pub loading_session: bool,
    pub reload_after_gap: bool,
    resume_any_on_connect: bool,
    pub pending_approval: Option<PendingApproval>,
    pub pending_clarify: Option<PendingClarify>,
    pub composer_input: TextBuffer,
    pub attachments: Vec<AttachedFile>,
    pub palette_index: usize,
    pub palette_dismissed: bool,
    pub yolo: bool,
    pub owner_connected: bool,
    pub status_message: String,
}

impl GuiState {
    pub fn new(active_profile: PathBuf) -> Self {
        Self {
            active_profile,
            profiles: Vec::new(),
            sessions: Vec::new(),
            workspace_path_input: TextBuffer::new(),
            search_input: TextBuffer::new(),
            selected_session_id: None,
            selected_session_title: None,
            confirm_delete_session_id: None,
            feed: Vec::new(),
            live_reasoning: Vec::new(),
            live_reasoning_partial: String::new(),
            active_turn_id: None,
            last_seq: 0,
            streaming: false,
            loading_session: false,
            reload_after_gap: false,
            resume_any_on_connect: true,
            pending_approval: None,
            pending_clarify: None,
            composer_input: TextBuffer::new(),
            attachments: Vec::new(),
            palette_index: 0,
            palette_dismissed: false,
            yolo: false,
            owner_connected: false,
            status_message: "Connecting to local session owner...".into(),
        }
    }

    pub fn set_workspace_path_text(&mut self, path: &Path) {
        self.workspace_path_input.clear();
        self.workspace_path_input
            .insert_str(&path.display().to_string());
    }

    pub fn add_attachment(&mut self, att: AttachedFile) -> bool {
        if self.attachments.iter().any(|a| a.rel_path == att.rel_path) {
            return false;
        }
        self.attachments.push(att);
        true
    }

    pub fn remove_attachment(&mut self, index: usize) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
        }
    }

    /// Compute `@` context-reference or `/` command+skill completions for the composer.
    pub fn compute_completions(&self, root: &Path) -> Option<(&'static str, Vec<CompletionItem>)> {
        if self.streaming
            || self.pending_approval.is_some()
            || self.pending_clarify.is_some()
            || self.palette_dismissed
        {
            return None;
        }
        let text = self.composer_input.as_str();
        let cursor = self.composer_input.cursor();
        if let Some((at_start, token)) = active_at_token(text, cursor) {
            let items: Vec<CompletionItem> = complete_at_references(root, token, 8)
                .into_iter()
                .map(|c| CompletionItem {
                    replacement: c.replacement,
                    display: c.display,
                    detail: c.detail,
                    at_start: Some(at_start),
                })
                .collect();
            if !items.is_empty() {
                return Some((
                    "Context (@file / @folder / @diff) — Tab or click to insert",
                    items,
                ));
            }
        }
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            let lower = text.to_ascii_lowercase();
            let mut items: Vec<CompletionItem> = GUI_SLASH_COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(&lower))
                .map(|(cmd, desc)| CompletionItem {
                    replacement: (*cmd).to_owned(),
                    display: (*cmd).to_owned(),
                    detail: (*desc).to_owned(),
                    at_start: None,
                })
                .collect();
            let skill_needle = &lower[1..];
            for skill in list_skill_commands(Some(&self.active_profile)) {
                if skill_needle.is_empty() || skill.name.starts_with(skill_needle) {
                    let slash = format!("/{}", skill.name);
                    if !items.iter().any(|i| i.replacement == slash) {
                        items.push(CompletionItem {
                            replacement: slash.clone(),
                            display: slash,
                            detail: format!("[skill] {}", skill.description),
                            at_start: None,
                        });
                    }
                }
            }
            if !items.is_empty() {
                return Some(("Commands & Skills — Tab to complete, Enter to run", items));
            }
        }
        None
    }

    /// Insert a selected `@` or `/` completion into the composer.
    pub fn insert_completion(&mut self, item: &CompletionItem) {
        if let Some(at_start) = item.at_start {
            let mut text = item.replacement.clone();
            if !text.ends_with(':') {
                text.push(' ');
            }
            self.composer_input.replace_range_to_cursor(at_start, &text);
        } else {
            self.composer_input.clear();
            let mut text = item.replacement.clone();
            if matches!(text.as_str(), "/skills" | "/cd" | "/attach")
                || item.detail.starts_with("[skill]")
            {
                text.push(' ');
            }
            self.composer_input.insert_str(&text);
        }
        self.palette_index = 0;
        self.palette_dismissed = false;
    }

    pub fn reset_for_profile(&mut self, profile: PathBuf) {
        self.active_profile = profile;
        self.sessions.clear();
        self.start_new_chat();
        self.resume_any_on_connect = true;
        self.composer_input.clear();
        self.search_input.clear();
        self.yolo = false;
        self.owner_connected = false;
        self.status_message = "Switching profile...".into();
    }

    pub fn start_new_chat(&mut self) {
        self.loading_session = false;
        self.reload_after_gap = false;
        self.resume_any_on_connect = false;
        self.selected_session_id = None;
        self.selected_session_title = None;
        self.confirm_delete_session_id = None;
        self.feed.clear();
        self.attachments.clear();
        self.active_turn_id = None;
        self.last_seq = 0;
        self.streaming = false;
        self.pending_approval = None;
        self.pending_clarify = None;
        self.status_message = "New conversation".into();
    }

    pub fn select_session(&mut self, id: String) {
        self.start_new_chat();
        self.selected_session_id = Some(id);
        self.loading_session = true;
        self.status_message = "Loading session...".into();
    }

    pub fn handle_session_deleted(&mut self, session_id: &str) {
        self.sessions.retain(|s| s.id != session_id);
        if self.confirm_delete_session_id.as_deref() == Some(session_id) {
            self.confirm_delete_session_id = None;
        }
        if self.selected_session_id.as_deref() == Some(session_id) {
            self.start_new_chat();
        }
        self.status_message = format!("Deleted session {session_id}");
    }

    pub fn load_transcript(
        &mut self,
        session: SessionSummary,
        items: Vec<TranscriptItem>,
        active_turn_id: Option<String>,
    ) {
        self.loading_session = false;
        self.reload_after_gap = false;
        self.resume_any_on_connect = false;
        self.selected_session_id = Some(session.id.clone());
        self.selected_session_title = session.title.clone();
        self.feed.clear();
        for item in items {
            match item.role.as_str() {
                "user" => self.feed.push(FeedRow::User(item.content)),
                "assistant" if !item.content.trim().is_empty() => {
                    self.feed.push(FeedRow::Assistant(item.content));
                }
                "tool" => {
                    let (state, summary) = summarize_tool_payload(&item.content);
                    self.feed.push(FeedRow::Tool {
                        name: item.tool_name.unwrap_or_else(|| "tool".into()),
                        detail: None,
                        summary: Some(summary),
                        live_output: Vec::new(),
                        state,
                    });
                }
                _ => {}
            }
        }
        self.pending_approval = None;
        self.pending_clarify = None;
        if let Some(tid) = active_turn_id {
            self.active_turn_id = Some(tid);
            self.last_seq = 0;
            self.streaming = true;
            self.status_message = "Reattaching to active turn...".into();
        } else {
            self.active_turn_id = None;
            self.last_seq = 0;
            self.streaming = false;
            self.status_message = format!("Loaded session {}", session.id);
        }
    }

    pub fn note_active_turns(&mut self, turns: &[ActiveTurnSummary]) -> Option<(String, u64)> {
        if self.loading_session || (self.streaming && self.active_turn_id.is_none()) {
            return None;
        }
        let running = turns.iter().find(|t| {
            !t.finished
                && if let Some(turn_id) = &self.active_turn_id {
                    &t.turn_id == turn_id
                } else if self.selected_session_id.is_some() {
                    t.session_id == self.selected_session_id
                } else {
                    self.resume_any_on_connect
                }
        })?;
        self.resume_any_on_connect = false;
        if self.active_turn_id.as_deref() == Some(running.turn_id.as_str()) {
            return Some((running.turn_id.clone(), self.last_seq));
        }
        self.active_turn_id = Some(running.turn_id.clone());
        if self.selected_session_id.is_none() {
            self.selected_session_id = running.session_id.clone();
        }
        self.last_seq = 0;
        self.streaming = true;
        self.status_message = format!("Resumed stream for {}", running.turn_id);
        Some((running.turn_id.clone(), 0))
    }

    /// Apply one sequenced event from the local owner, ignoring duplicates on reconnect.
    pub fn apply_turn_event(&mut self, turn_id: &str, ev: SequencedTurnEvent) -> bool {
        if self.active_turn_id.as_deref() != Some(turn_id) || ev.seq <= self.last_seq {
            return false;
        }
        self.last_seq = ev.seq;
        match ev.event {
            TurnStreamEvent::ReplayGap {
                session_id,
                pending_approval,
            } => {
                self.selected_session_id = session_id;
                self.reload_after_gap = true;
                self.pending_approval = None;
                self.pending_clarify = None;
                self.feed.push(FeedRow::Notice {
                    is_error: false,
                    text: "Some live output expired. The saved conversation will reload when the turn ends.".into(),
                });
                if let Some((id, request)) = pending_approval {
                    self.show_approval(turn_id, id, request);
                }
            }
            TurnStreamEvent::TextDelta { text } => {
                if !text.trim().is_empty() {
                    self.live_reasoning.clear();
                    self.live_reasoning_partial.clear();
                }
                if let Some(FeedRow::Assistant(existing)) = self.feed.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.feed.push(FeedRow::Assistant(text));
                }
            }
            TurnStreamEvent::ReasoningDelta { text } => {
                push_rolling_lines(
                    &mut self.live_reasoning,
                    &mut self.live_reasoning_partial,
                    &text,
                    3,
                );
            }
            TurnStreamEvent::Status { text } => {
                self.status_message = text;
            }
            TurnStreamEvent::SessionAssigned { session_id } => {
                self.selected_session_id = Some(session_id.clone());
                self.status_message = format!("Session {session_id}");
            }
            TurnStreamEvent::ToolStarted { name, detail } => {
                self.live_reasoning.clear();
                self.live_reasoning_partial.clear();
                self.feed.push(FeedRow::Tool {
                    name,
                    detail,
                    summary: None,
                    live_output: Vec::new(),
                    state: ToolCardState::Running,
                });
            }
            TurnStreamEvent::ToolOutput { name, text } => {
                if let Some(FeedRow::Tool { live_output, .. }) =
                    self.feed.iter_mut().rev().find(|row| {
                        matches!(
                            row,
                            FeedRow::Tool {
                                name: n,
                                state: ToolCardState::Running,
                                ..
                            } if n == &name
                        )
                    })
                {
                    let mut partial = String::new();
                    push_rolling_lines(live_output, &mut partial, &text, 3);
                    let trimmed = partial.trim();
                    if !trimmed.is_empty() {
                        live_output.push(trimmed.to_owned());
                        if live_output.len() > 3 {
                            live_output.drain(..live_output.len() - 3);
                        }
                    }
                }
            }
            TurnStreamEvent::ToolFinished {
                name,
                succeeded,
                summary,
            } => {
                let target_state = if succeeded {
                    ToolCardState::Succeeded
                } else {
                    ToolCardState::Failed
                };
                if let Some(FeedRow::Tool {
                    summary: slot,
                    live_output,
                    state,
                    ..
                }) = self.feed.iter_mut().rev().find(|row| {
                    matches!(
                        row,
                        FeedRow::Tool {
                            name: n,
                            state: ToolCardState::Running,
                            ..
                        } if n == &name
                    )
                }) {
                    *slot = summary;
                    live_output.clear();
                    *state = target_state;
                } else {
                    self.feed.push(FeedRow::Tool {
                        name,
                        detail: None,
                        summary,
                        live_output: Vec::new(),
                        state: target_state,
                    });
                }
            }
            TurnStreamEvent::ApprovalRequired {
                approval_id,
                request,
            } => {
                self.show_approval(turn_id, approval_id, request);
            }
            TurnStreamEvent::ApprovalResolved { approval_id } => {
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|p| p.approval_id == approval_id)
                {
                    self.pending_approval = None;
                    self.status_message = "Approval sent; continuing...".into();
                }
                if self
                    .pending_clarify
                    .as_ref()
                    .is_some_and(|p| p.approval_id == approval_id)
                {
                    self.pending_clarify = None;
                    self.status_message = "Answer sent; continuing...".into();
                }
            }
            TurnStreamEvent::Error { message } => {
                self.feed.push(FeedRow::Notice {
                    is_error: true,
                    text: message.clone(),
                });
                self.status_message = message;
            }
            TurnStreamEvent::Done {
                exit_code,
                cancelled,
            } => {
                self.streaming = false;
                self.live_reasoning.clear();
                self.live_reasoning_partial.clear();
                self.pending_approval = None;
                self.pending_clarify = None;
                self.active_turn_id = None;
                self.status_message = if cancelled {
                    "Turn cancelled.".into()
                } else if exit_code == 0 {
                    "Ready".into()
                } else {
                    format!("Turn ended with exit code {exit_code}")
                };
            }
        }
        true
    }

    fn show_approval(&mut self, turn_id: &str, approval_id: u64, request: ApprovalRequest) {
        if request.action == ApprovalAction::Clarify {
            if let Some(clarify) =
                PendingClarify::from_request(turn_id.to_owned(), approval_id, &request.content)
            {
                self.pending_clarify = Some(clarify);
                self.pending_approval = None;
                self.status_message = "Hermes has a question for you...".into();
                return;
            }
        }
        self.pending_clarify = None;
        self.pending_approval = Some(PendingApproval {
            turn_id: turn_id.to_owned(),
            approval_id,
            request,
        });
        self.status_message = "Waiting for your approval...".into();
    }
}

fn active_at_token(input: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = input.get(..cursor)?;
    let start = before.rfind(char::is_whitespace).map_or(0, |idx| {
        idx + before[idx..].chars().next().map_or(1, char::len_utf8)
    });
    let token = &before[start..];
    token.starts_with('@').then_some((start, token))
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max_chars {
        single
    } else {
        let mut s: String = single.chars().take(max_chars).collect();
        s.push('…');
        s
    }
}

fn summarize_tool_payload(content: &str) -> (ToolCardState, String) {
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(content) {
        let error_text = obj
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let status = obj.get("status").and_then(Value::as_str).unwrap_or("");
        let failed = error_text.is_some()
            || obj.get("success") == Some(&Value::Bool(false))
            || matches!(
                status,
                "error"
                    | "timeout"
                    | "approval_denied"
                    | "approval_unavailable"
                    | "approval_timeout"
                    | "backend_unavailable"
            )
            || obj
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);

        if failed {
            let reason = error_text
                .map(|e| truncate_preview(e, 140))
                .or_else(|| (!status.is_empty()).then(|| status.replace('_', " ")))
                .unwrap_or_else(|| truncate_preview(content, 120));
            return (ToolCardState::Failed, reason);
        }

        if let Some(out) = obj
            .get("output")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return (ToolCardState::Succeeded, truncate_preview(out, 120));
        }
        if let Some(n) = obj.get("total_available").and_then(Value::as_u64) {
            return (ToolCardState::Succeeded, format!("{n} matching tool(s)"));
        }
        if !status.is_empty() {
            return (ToolCardState::Succeeded, status.to_owned());
        }
    }
    (ToolCardState::Succeeded, truncate_preview(content, 120))
}

fn push_rolling_lines(
    lines: &mut Vec<String>,
    partial: &mut String,
    chunk: &str,
    max_lines: usize,
) {
    let max_lines = max_lines.max(1);
    for ch in chunk.chars() {
        match ch {
            '\n' => {
                let cleaned = partial.trim();
                if !cleaned.is_empty() {
                    lines.push(cleaned.to_owned());
                    if lines.len() > max_lines {
                        let excess = lines.len() - max_lines;
                        lines.drain(..excess);
                    }
                }
                partial.clear();
            }
            '\r' => partial.clear(),
            c if !c.is_control() && partial.len() < 400 => {
                partial.push(c);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_turn_events_cannot_change_a_new_chat() {
        let mut state = GuiState::new(PathBuf::from("/profile"));
        state.active_turn_id = Some("old".into());
        state.start_new_chat();
        state.apply_turn_event(
            "old",
            SequencedTurnEvent {
                seq: 1,
                event: TurnStreamEvent::SessionAssigned {
                    session_id: "old-session".into(),
                },
            },
        );
        assert_eq!(state.selected_session_id, None);
        assert_eq!(state.last_seq, 0);
    }

    #[test]
    fn reconnect_to_selected_session_ignores_unrelated_running_turns() {
        let mut state = GuiState::new(PathBuf::from("/profile"));
        state.selected_session_id = Some("mine".into());
        let other = ActiveTurnSummary {
            turn_id: "unrelated".into(),
            session_id: Some("other".into()),
            query: String::new(),
            latest_seq: 10,
            finished: false,
            has_pending_approval: false,
        };
        assert_eq!(state.note_active_turns(&[other]), None);
        assert!(state.active_turn_id.is_none());
    }

    #[test]
    fn reconnect_deduplicates_sequence_and_restores_pending_approval() {
        let mut st = GuiState::new(PathBuf::from("/tmp/p1"));
        st.active_turn_id = Some("turn-1".into());
        st.streaming = true;

        let ev1 = SequencedTurnEvent {
            seq: 1,
            event: TurnStreamEvent::TextDelta {
                text: "Hello ".into(),
            },
        };
        let ev2 = SequencedTurnEvent {
            seq: 2,
            event: TurnStreamEvent::ApprovalRequired {
                approval_id: 7,
                request: ApprovalRequest {
                    target: "foo.rs".into(),
                    content: "+line".into(),
                    action: ApprovalAction::ReplaceFile,
                    timeout_secs: 60,
                },
            },
        };
        st.apply_turn_event("turn-1", ev1.clone());
        st.apply_turn_event("turn-1", ev1); // duplicate ignored
        st.apply_turn_event("turn-1", ev2);
        assert_eq!(st.feed, vec![FeedRow::Assistant("Hello ".into())]);
        assert_eq!(st.pending_approval.as_ref().unwrap().approval_id, 7);

        st.apply_turn_event(
            "turn-1",
            SequencedTurnEvent {
                seq: 3,
                event: TurnStreamEvent::ApprovalResolved { approval_id: 7 },
            },
        );
        assert!(st.pending_approval.is_none());

        st.apply_turn_event(
            "turn-1",
            SequencedTurnEvent {
                seq: 4,
                event: TurnStreamEvent::Done {
                    exit_code: 0,
                    cancelled: false,
                },
            },
        );
        assert!(!st.streaming);
        assert!(st.active_turn_id.is_none());
    }

    #[test]
    fn clarify_approval_parses_questions_and_builds_submitted_json() {
        let mut st = GuiState::new(PathBuf::from("/tmp/p1"));
        st.active_turn_id = Some("turn-9".into());
        let payload = json!([
            {
                "qid": "q0",
                "question": "Pick targets",
                "choices": ["Linux (Recommended)", "Windows"],
                "multi_select": true
            },
            {
                "qid": "q1",
                "question": "Release tag?",
                "choices": null,
                "multi_select": false
            }
        ])
        .to_string();

        st.apply_turn_event(
            "turn-9",
            SequencedTurnEvent {
                seq: 1,
                event: TurnStreamEvent::ApprovalRequired {
                    approval_id: 3,
                    request: ApprovalRequest {
                        target: "clarify".into(),
                        content: payload,
                        action: ApprovalAction::Clarify,
                        timeout_secs: 90,
                    },
                },
            },
        );
        assert!(st.pending_approval.is_none());
        let clarify = st.pending_clarify.as_mut().expect("clarify parsed");
        clarify.toggle_choice("q0", "Linux (Recommended)", true);
        clarify.toggle_choice("q0", "Windows", true);
        clarify.active_index = 1;
        clarify
            .active_buffer_mut()
            .unwrap()
            .insert_str("gui-v0.1.0");

        let ApprovalDecision::Answered(raw) = clarify.build_submitted_decision() else {
            panic!("expected Answered decision");
        };
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["outcome"], "submitted");
        assert_eq!(
            parsed["answers"]["q0"],
            json!(["Linux (Recommended)", "Windows"])
        );
        assert_eq!(parsed["answers"]["q1"], json!("gui-v0.1.0"));
    }

    #[test]
    fn tool_lifecycle_and_profile_reset_maintain_isolation() {
        let mut st = GuiState::new(PathBuf::from("/tmp/profile-a"));
        st.active_turn_id = Some("turn-2".into());
        st.streaming = true;

        st.apply_turn_event(
            "turn-2",
            SequencedTurnEvent {
                seq: 1,
                event: TurnStreamEvent::ToolStarted {
                    name: "read_file".into(),
                    detail: Some("Cargo.toml".into()),
                },
            },
        );
        st.apply_turn_event(
            "turn-2",
            SequencedTurnEvent {
                seq: 2,
                event: TurnStreamEvent::ToolFinished {
                    name: "read_file".into(),
                    succeeded: true,
                    summary: Some("ok".into()),
                },
            },
        );
        assert_eq!(
            st.feed,
            vec![FeedRow::Tool {
                name: "read_file".into(),
                detail: Some("Cargo.toml".into()),
                summary: Some("ok".into()),
                live_output: Vec::new(),
                state: ToolCardState::Succeeded,
            }]
        );

        st.reset_for_profile(PathBuf::from("/tmp/profile-b"));
        assert_eq!(st.active_profile, PathBuf::from("/tmp/profile-b"));
        assert!(st.sessions.is_empty());
        assert!(st.feed.is_empty());
        assert!(st.selected_session_id.is_none());
        assert!(st.active_turn_id.is_none());
        assert_eq!(st.last_seq, 0);
        assert!(!st.streaming);
        assert!(st.pending_approval.is_none());
        assert!(st.pending_clarify.is_none());
    }

    #[test]
    fn load_transcript_reattaches_active_turn_and_note_active_turns_resumes() {
        let mut st = GuiState::new(PathBuf::from("/tmp/profile-a"));
        let session = SessionSummary {
            id: "sess-42".into(),
            title: Some("Investigate".into()),
            model: Some("test-model".into()),
            started_at: 100.0,
            message_count: 2,
        };
        let items = vec![
            TranscriptItem {
                role: "user".into(),
                content: "Check status".into(),
                tool_name: None,
            },
            TranscriptItem {
                role: "assistant".into(),
                content: "   ".into(),
                tool_name: None,
            },
            TranscriptItem {
                role: "tool".into(),
                content: "exit 0".into(),
                tool_name: Some("terminal".into()),
            },
        ];
        st.load_transcript(session, items, Some("turn-live".into()));
        assert_eq!(st.selected_session_id.as_deref(), Some("sess-42"));
        assert_eq!(st.active_turn_id.as_deref(), Some("turn-live"));
        assert!(st.streaming);
        assert_eq!(st.feed.len(), 2);

        st.last_seq = 5;
        let sub = st.note_active_turns(&[ActiveTurnSummary {
            turn_id: "turn-live".into(),
            session_id: Some("sess-42".into()),
            query: "Check status".into(),
            latest_seq: 8,
            has_pending_approval: false,
            finished: false,
        }]);
        assert_eq!(sub, Some(("turn-live".into(), 5)));
    }

    #[test]
    fn computes_and_inserts_at_and_skill_completions() {
        let dir = std::env::temp_dir().join(format!("hsr-gui-comp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        let profile = dir.join("profile");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(work.join("src/lib.rs"), "pub fn x() {}").unwrap();
        std::fs::create_dir_all(profile.join("skills/dev/audit")).unwrap();
        std::fs::write(
            profile.join("skills/dev/audit/SKILL.md"),
            "---\nname: audit\ndescription: Run audit\n---\nBody",
        )
        .unwrap();

        let mut st = GuiState::new(profile);
        st.composer_input.insert_str("Review @lib");
        let (_, at_items) = st.compute_completions(&work).expect("at completions");
        let file_item = at_items
            .iter()
            .find(|i| i.replacement == "@file:src/lib.rs")
            .cloned()
            .expect("lib.rs completion");
        st.insert_completion(&file_item);
        assert_eq!(st.composer_input.as_str(), "Review @file:src/lib.rs ");

        st.composer_input.clear();
        st.composer_input.insert_str("/au");
        let (_, slash_items) = st.compute_completions(&work).expect("slash completions");
        assert!(slash_items.iter().any(|i| i.replacement == "/audit"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_transcript_marks_failed_tool_results_and_summarizes_cleanly() {
        let mut st = GuiState::new(PathBuf::from("/tmp/profile-a"));
        let session = SessionSummary {
            id: "sess-tools".into(),
            title: Some("Tool failures".into()),
            model: Some("test-model".into()),
            started_at: 200.0,
            message_count: 3,
        };
        let items = vec![
            TranscriptItem {
                role: "tool".into(),
                content: json!({
                    "error": "Code execution requires user approval, which is unavailable in this environment.",
                    "status": "approval_unavailable"
                })
                .to_string(),
                tool_name: Some("execute_code".into()),
            },
            TranscriptItem {
                role: "tool".into(),
                content: json!({
                    "output": "hello world",
                    "exit_code": 0
                })
                .to_string(),
                tool_name: Some("terminal".into()),
            },
            TranscriptItem {
                role: "tool".into(),
                content: json!({
                    "status": "ok",
                    "total_available": 3
                })
                .to_string(),
                tool_name: Some("search_tools".into()),
            },
        ];

        st.load_transcript(session, items, None);
        assert_eq!(
            st.feed,
            vec![
                FeedRow::Tool {
                    name: "execute_code".into(),
                    detail: None,
                    summary: Some(
                        "Code execution requires user approval, which is unavailable in this environment."
                            .into()
                    ),
                    live_output: Vec::new(),
                    state: ToolCardState::Failed,
                },
                FeedRow::Tool {
                    name: "terminal".into(),
                    detail: None,
                    summary: Some("hello world".into()),
                    live_output: Vec::new(),
                    state: ToolCardState::Succeeded,
                },
                FeedRow::Tool {
                    name: "search_tools".into(),
                    detail: None,
                    summary: Some("3 matching tool(s)".into()),
                    live_output: Vec::new(),
                    state: ToolCardState::Succeeded,
                },
            ]
        );
    }

    #[test]
    fn handle_session_deleted_removes_entry_and_clears_active_view_if_selected() {
        let mut st = GuiState::new(PathBuf::from("/tmp/profile-a"));
        st.sessions = vec![
            SessionSummary {
                id: "sess-1".into(),
                title: Some("First".into()),
                model: None,
                started_at: 1.0,
                message_count: 2,
            },
            SessionSummary {
                id: "sess-2".into(),
                title: Some("Second".into()),
                model: None,
                started_at: 2.0,
                message_count: 4,
            },
        ];
        st.selected_session_id = Some("sess-1".into());
        st.feed.push(FeedRow::User("hello".into()));
        st.confirm_delete_session_id = Some("sess-2".into());

        // Deleting a non-selected session keeps the active conversation intact.
        st.handle_session_deleted("sess-2");
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.sessions[0].id, "sess-1");
        assert_eq!(st.selected_session_id.as_deref(), Some("sess-1"));
        assert_eq!(st.feed.len(), 1);
        assert!(st.confirm_delete_session_id.is_none());

        // Deleting the currently selected session resets to a clean new chat.
        st.handle_session_deleted("sess-1");
        assert!(st.sessions.is_empty());
        assert!(st.selected_session_id.is_none());
        assert!(st.feed.is_empty());
    }
}
