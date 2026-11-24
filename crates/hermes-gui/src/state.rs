//! Pure UI state model for `hermes-gui` (unit-testable without a display server).

use std::path::PathBuf;

use hermes_runtime::local_owner::{
    ActiveTurnSummary, ProfileEntry, SequencedTurnEvent, SessionSummary, TranscriptItem,
    TurnStreamEvent,
};
use hermes_runtime::turn::ApprovalRequest;

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
        state: ToolCardState,
    },
    Notice {
        is_error: bool,
        text: String,
    },
}

#[derive(Clone, Debug)]
pub struct PendingApproval {
    pub turn_id: String,
    pub approval_id: u64,
    pub request: ApprovalRequest,
}

#[derive(Clone, Debug)]
pub struct GuiState {
    pub active_profile: PathBuf,
    pub profiles: Vec<ProfileEntry>,
    pub sessions: Vec<SessionSummary>,
    pub search_query: String,
    pub selected_session_id: Option<String>,
    pub selected_session_title: Option<String>,
    pub feed: Vec<FeedRow>,
    pub active_turn_id: Option<String>,
    pub last_seq: u64,
    pub streaming: bool,
    pub pending_approval: Option<PendingApproval>,
    pub composer: String,
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
            search_query: String::new(),
            selected_session_id: None,
            selected_session_title: None,
            feed: Vec::new(),
            active_turn_id: None,
            last_seq: 0,
            streaming: false,
            pending_approval: None,
            composer: String::new(),
            yolo: false,
            owner_connected: false,
            status_message: "Connecting to local session owner...".into(),
        }
    }

    pub fn reset_for_profile(&mut self, profile: PathBuf) {
        self.active_profile = profile;
        self.sessions.clear();
        self.selected_session_id = None;
        self.selected_session_title = None;
        self.feed.clear();
        self.active_turn_id = None;
        self.last_seq = 0;
        self.streaming = false;
        self.pending_approval = None;
        self.owner_connected = false;
        self.status_message = "Switching profile...".into();
    }

    pub fn start_new_chat(&mut self) {
        self.selected_session_id = None;
        self.selected_session_title = None;
        self.feed.clear();
        self.active_turn_id = None;
        self.last_seq = 0;
        self.streaming = false;
        self.pending_approval = None;
        self.status_message = "New conversation".into();
    }

    pub fn load_transcript(
        &mut self,
        session: SessionSummary,
        items: Vec<TranscriptItem>,
        active_turn_id: Option<String>,
    ) {
        self.selected_session_id = Some(session.id.clone());
        self.selected_session_title = session.title.clone();
        self.feed.clear();
        for item in items {
            match item.role.as_str() {
                "user" => self.feed.push(FeedRow::User(item.content)),
                "assistant" if !item.content.trim().is_empty() => {
                    self.feed.push(FeedRow::Assistant(item.content));
                }
                "tool" => self.feed.push(FeedRow::Tool {
                    name: item.tool_name.unwrap_or_else(|| "tool".into()),
                    detail: None,
                    summary: Some(truncate_preview(&item.content, 120)),
                    state: ToolCardState::Succeeded,
                }),
                _ => {}
            }
        }
        self.pending_approval = None;
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
        let running = turns.iter().find(|t| !t.finished)?;
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
    pub fn apply_turn_event(&mut self, turn_id: &str, ev: SequencedTurnEvent) {
        if ev.seq <= self.last_seq {
            return;
        }
        self.last_seq = ev.seq;
        match ev.event {
            TurnStreamEvent::TextDelta { text } => {
                if let Some(FeedRow::Assistant(existing)) = self.feed.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.feed.push(FeedRow::Assistant(text));
                }
            }
            TurnStreamEvent::Status { text } => {
                self.status_message = text;
            }
            TurnStreamEvent::SessionAssigned { session_id } => {
                self.selected_session_id = Some(session_id.clone());
                self.status_message = format!("Session {session_id}");
            }
            TurnStreamEvent::ToolStarted { name, detail } => {
                self.feed.push(FeedRow::Tool {
                    name,
                    detail,
                    summary: None,
                    state: ToolCardState::Running,
                });
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
                    *state = target_state;
                } else {
                    self.feed.push(FeedRow::Tool {
                        name,
                        detail: None,
                        summary,
                        state: target_state,
                    });
                }
            }
            TurnStreamEvent::ApprovalRequired {
                approval_id,
                request,
            } => {
                self.pending_approval = Some(PendingApproval {
                    turn_id: turn_id.to_owned(),
                    approval_id,
                    request,
                });
                self.status_message = "Waiting for your approval...".into();
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
                self.pending_approval = None;
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
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_runtime::turn::ApprovalAction;

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
}
