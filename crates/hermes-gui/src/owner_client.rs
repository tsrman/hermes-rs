//! Asynchronous bridge between the GPUI frontend and the profile's `hsr owner` daemon.
//!
//! Automatically spawns `hsr owner` for the chosen profile if none is running yet, exposes
//! non-blocking RPC helpers, and streams `SubscribeTurn` events over an unbounded channel so the
//! GPUI view refreshes smoothly without blocking the UI thread.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use hermes_runtime::local_owner::{
    self, read_frame, send_v2_command, write_frame, ActiveTurnSummary, GuiCommand, GuiReply,
    ProfileEntry, SequencedTurnEvent, SessionSummary, TranscriptItem, TurnStreamEvent, V2Envelope,
    PROTOCOL_V2,
};
use hermes_runtime::turn::ApprovalDecision;
use tokio::runtime::Runtime;

#[derive(Debug)]
pub enum BackendUpdate {
    Connected {
        profile: PathBuf,
        profiles: Vec<ProfileEntry>,
        sessions: Vec<SessionSummary>,
        active_turns: Vec<ActiveTurnSummary>,
    },
    SessionsLoaded(Vec<SessionSummary>),
    TranscriptLoaded {
        session: SessionSummary,
        messages: Vec<TranscriptItem>,
        active_turn_id: Option<String>,
    },
    TurnStarted {
        turn_id: String,
    },
    TurnEvent {
        turn_id: String,
        event: SequencedTurnEvent,
    },
    Error(String),
}

pub struct OwnerClient {
    rt: Runtime,
    tx: Sender<BackendUpdate>,
}

impl OwnerClient {
    pub fn new() -> Result<(Self, Receiver<BackendUpdate>), String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("Failed creating Tokio runtime: {e}"))?;
        let (tx, rx) = mpsc::channel();
        Ok((Self { rt, tx }, rx))
    }

    pub fn connect_and_sync(&self, profile: PathBuf, query: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            if let Err(err) = ensure_owner_running(&profile).await {
                let _ = tx.send(BackendUpdate::Error(err));
                return;
            }
            let profiles = match send_v2_command(&profile, GuiCommand::ListProfiles).await {
                Ok(GuiReply::Profiles { profiles }) => profiles,
                _ => local_owner::discover_profiles(&profile),
            };
            let sessions = match send_v2_command(
                &profile,
                GuiCommand::ListSessions {
                    limit: 50,
                    query: (!query.trim().is_empty()).then_some(query),
                },
            )
            .await
            {
                Ok(GuiReply::Sessions { sessions }) => sessions,
                _ => Vec::new(),
            };
            let active_turns = match send_v2_command(&profile, GuiCommand::ActiveTurns).await {
                Ok(GuiReply::ActiveTurns { turns }) => turns,
                _ => Vec::new(),
            };
            let _ = tx.send(BackendUpdate::Connected {
                profile,
                profiles,
                sessions,
                active_turns,
            });
        });
    }

    pub fn list_sessions(&self, profile: PathBuf, query: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let cmd = GuiCommand::ListSessions {
                limit: 50,
                query: (!query.trim().is_empty()).then_some(query),
            };
            match send_v2_command(&profile, cmd).await {
                Ok(GuiReply::Sessions { sessions }) => {
                    let _ = tx.send(BackendUpdate::SessionsLoaded(sessions));
                }
                Ok(GuiReply::Error { message }) => {
                    let _ = tx.send(BackendUpdate::Error(message));
                }
                Err(err) => {
                    let _ = tx.send(BackendUpdate::Error(err));
                }
                _ => {}
            }
        });
    }

    pub fn load_session(&self, profile: PathBuf, session_id: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            match send_v2_command(&profile, GuiCommand::LoadSession { session_id }).await {
                Ok(GuiReply::Transcript {
                    session,
                    messages,
                    active_turn_id,
                }) => {
                    let _ = tx.send(BackendUpdate::TranscriptLoaded {
                        session,
                        messages,
                        active_turn_id,
                    });
                }
                Ok(GuiReply::Error { message }) => {
                    let _ = tx.send(BackendUpdate::Error(message));
                }
                Err(err) => {
                    let _ = tx.send(BackendUpdate::Error(err));
                }
                _ => {}
            }
        });
    }

    pub fn start_turn(
        &self,
        profile: PathBuf,
        session_id: Option<String>,
        query: String,
        yolo: bool,
    ) {
        let tx = self.tx.clone();
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string());
        self.rt.spawn(async move {
            if let Err(err) = ensure_owner_running(&profile).await {
                let _ = tx.send(BackendUpdate::Error(err));
                return;
            }
            let cmd = GuiCommand::StartTurn {
                session_id,
                query,
                model: None,
                cwd,
                yolo,
            };
            match send_v2_command(&profile, cmd).await {
                Ok(GuiReply::TurnStarted { turn_id }) => {
                    let _ = tx.send(BackendUpdate::TurnStarted {
                        turn_id: turn_id.clone(),
                    });
                    stream_turn_events(profile, turn_id, 0, tx).await;
                }
                Ok(GuiReply::Error { message }) => {
                    let _ = tx.send(BackendUpdate::Error(message));
                }
                Err(err) => {
                    let _ = tx.send(BackendUpdate::Error(err));
                }
                _ => {}
            }
        });
    }

    pub fn subscribe_turn(&self, profile: PathBuf, turn_id: String, after_seq: u64) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            stream_turn_events(profile, turn_id, after_seq, tx).await;
        });
    }

    pub fn cancel_turn(&self, profile: PathBuf, turn_id: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            if let Err(err) = send_v2_command(&profile, GuiCommand::CancelTurn { turn_id }).await {
                let _ = tx.send(BackendUpdate::Error(err));
            }
        });
    }

    pub fn respond_approval(
        &self,
        profile: PathBuf,
        turn_id: String,
        approval_id: u64,
        decision: ApprovalDecision,
    ) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let cmd = GuiCommand::RespondApproval {
                turn_id,
                approval_id,
                decision,
            };
            if let Err(err) = send_v2_command(&profile, cmd).await {
                let _ = tx.send(BackendUpdate::Error(err));
            }
        });
    }
}

async fn stream_turn_events(
    profile: PathBuf,
    turn_id: String,
    after_seq: u64,
    tx: Sender<BackendUpdate>,
) {
    let mut stream = match local_owner::connect(&profile).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = tx.send(BackendUpdate::Error(
                "Local session owner disconnected.".into(),
            ));
            return;
        }
        Err(err) => {
            let _ = tx.send(BackendUpdate::Error(err));
            return;
        }
    };
    let env = V2Envelope {
        version: PROTOCOL_V2,
        command: GuiCommand::SubscribeTurn {
            turn_id: turn_id.clone(),
            after_seq,
        },
    };
    if let Err(err) = write_frame(&mut stream, &env).await {
        let _ = tx.send(BackendUpdate::Error(format!(
            "Failed subscribing to turn: {err}"
        )));
        return;
    }
    loop {
        match read_frame::<GuiReply>(&mut stream).await {
            Ok(GuiReply::TurnEvent(ev)) => {
                let done = matches!(ev.event, TurnStreamEvent::Done { .. });
                let _ = tx.send(BackendUpdate::TurnEvent {
                    turn_id: turn_id.clone(),
                    event: ev,
                });
                if done {
                    break;
                }
            }
            Ok(GuiReply::Error { message }) => {
                let _ = tx.send(BackendUpdate::Error(message));
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn find_hsr_binary() -> PathBuf {
    if let Ok(explicit) = std::env::var("HSR_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(windows) { "hsr.exe" } else { "hsr" };
            let sibling = dir.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(if cfg!(windows) { "hsr.exe" } else { "hsr" })
}

async fn ensure_owner_running(profile: &Path) -> Result<(), String> {
    if matches!(
        send_v2_command(profile, GuiCommand::Ping).await,
        Ok(GuiReply::Pong { .. })
    ) {
        return Ok(());
    }
    let hsr = find_hsr_binary();
    Command::new(&hsr)
        .arg("owner")
        .env("HSR_HOME", profile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            format!(
                "Failed spawning local owner ({}): {e}. Ensure `hsr` is built or in PATH.",
                hsr.display()
            )
        })?;

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if matches!(
            send_v2_command(profile, GuiCommand::Ping).await,
            Ok(GuiReply::Pong { .. })
        ) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    Err(format!(
        "Local session owner for {} did not start within 4s.",
        profile.display()
    ))
}
