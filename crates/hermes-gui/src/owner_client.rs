//! Asynchronous bridge between the GPUI frontend and the profile's `hsr owner` daemon.
//!
//! Automatically spawns `hsr owner` for the chosen profile if none is running yet, exposes
//! non-blocking RPC helpers, and streams `SubscribeTurn` events with automatic reconnect and
//! `after_seq` replay recovery if the local socket drops.

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
    Reconnecting {
        profile: PathBuf,
        message: String,
    },
    OwnerRestarted {
        profile: PathBuf,
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
    SkillsSynced {
        profile: PathBuf,
        count: usize,
        message: String,
        is_error: bool,
    },
    OAuthProgress {
        profile: PathBuf,
        message: String,
    },
    OAuthFinished {
        profile: PathBuf,
        message: String,
        is_error: bool,
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

    pub fn sync_bundled_skills(&self, profile: PathBuf) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let _ = hermes_runtime::config::ensure_profile_scaffold(&profile);
            match hermes_runtime::skills_sync::sync_skills(
                &profile,
                hermes_runtime::skills_sync::SkillsSyncOptions::default(),
            )
            .await
            {
                Ok(report) => {
                    let count = hermes_runtime::skills_sync::list_installed_skills(&profile).len();
                    let msg = if report.opted_out {
                        "Bundled skills opted out (.no-bundled-skills).".into()
                    } else {
                        format!(
                            "Skills synced: {} installed, {} kept ({count} total).",
                            report.installed.len(),
                            report.skipped_existing.len()
                        )
                    };
                    let _ = tx.send(BackendUpdate::SkillsSynced {
                        profile,
                        count,
                        message: msg,
                        is_error: false,
                    });
                }
                Err(err) => {
                    let count = hermes_runtime::skills_sync::list_installed_skills(&profile).len();
                    let _ = tx.send(BackendUpdate::SkillsSynced {
                        profile,
                        count,
                        message: format!("Skills download failed: {err}"),
                        is_error: true,
                    });
                }
            }
        });
    }

    pub fn run_oauth_login(&self, profile: PathBuf, provider: String) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let tx_cb = tx.clone();
            let prof_cb = profile.clone();
            let opts = hermes_runtime::providers::oauth_login::LoginOptions {
                provider: provider.clone(),
                device_code: false,
                no_browser: false,
                port: None,
                region: None,
                portal_url: None,
                timeout_secs: 300,
                token: None,
            };
            let res =
                hermes_runtime::providers::oauth_login::login(&profile, opts, move |prompt| {
                    let msg = match prompt {
                        hermes_runtime::providers::oauth_login::LoginPrompt::OpenUrl { url } => {
                            format!("Opened browser sign-in: {url}")
                        }
                        hermes_runtime::providers::oauth_login::LoginPrompt::DeviceCode {
                            verification_uri,
                            user_code,
                            ..
                        } => format!("Open {verification_uri} and enter code: {user_code}"),
                        hermes_runtime::providers::oauth_login::LoginPrompt::Info(info) => info,
                    };
                    let _ = tx_cb.send(BackendUpdate::OAuthProgress {
                        profile: prof_cb.clone(),
                        message: msg,
                    });
                })
                .await;
            match res {
                Ok(done) => {
                    let _ = tx.send(BackendUpdate::OAuthFinished {
                        profile,
                        message: format!("Authenticated '{}' in auth.json (0600).", done.provider),
                        is_error: false,
                    });
                }
                Err(err) => {
                    let _ = tx.send(BackendUpdate::OAuthFinished {
                        profile,
                        message: format!("OAuth sign-in failed: {err}"),
                        is_error: true,
                    });
                }
            }
        });
    }
}

async fn stream_turn_events(
    profile: PathBuf,
    turn_id: String,
    initial_after_seq: u64,
    tx: Sender<BackendUpdate>,
) {
    let mut last_seq = initial_after_seq;
    let backoff_ms = [200u64, 500, 1000, 1500];
    let mut attempt = 0usize;

    loop {
        let mut stream = match local_owner::connect(&profile).await {
            Ok(Some(s)) => s,
            Ok(None) | Err(_) => {
                if attempt >= backoff_ms.len() {
                    let _ = tx.send(BackendUpdate::Error(
                        "Lost connection to local session owner.".into(),
                    ));
                    return;
                }
                let _ = tx.send(BackendUpdate::Reconnecting {
                    profile: profile.clone(),
                    message: format!(
                        "Owner disconnected; reconnecting ({}/{})...",
                        attempt + 1,
                        backoff_ms.len()
                    ),
                });
                tokio::time::sleep(Duration::from_millis(backoff_ms[attempt])).await;
                attempt += 1;
                let _ = ensure_owner_running(&profile).await;
                continue;
            }
        };

        let env = V2Envelope {
            version: PROTOCOL_V2,
            command: GuiCommand::SubscribeTurn {
                turn_id: turn_id.clone(),
                after_seq: last_seq,
            },
        };
        if write_frame(&mut stream, &env).await.is_err() {
            if attempt >= backoff_ms.len() {
                let _ = tx.send(BackendUpdate::Error(
                    "Failed subscribing to local owner turn stream.".into(),
                ));
                return;
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(backoff_ms[attempt - 1])).await;
            continue;
        }

        let mut received_any = false;
        loop {
            match read_frame::<GuiReply>(&mut stream).await {
                Ok(GuiReply::TurnEvent(ev)) => {
                    received_any = true;
                    attempt = 0;
                    if ev.seq > last_seq {
                        last_seq = ev.seq;
                    }
                    let done = matches!(ev.event, TurnStreamEvent::Done { .. });
                    let _ = tx.send(BackendUpdate::TurnEvent {
                        turn_id: turn_id.clone(),
                        event: ev,
                    });
                    if done {
                        return;
                    }
                }
                Ok(GuiReply::Error { message }) => {
                    if message.contains("Unknown turn_id") {
                        // Owner restarted cleanly; tell the UI to re-sync sessions & transcript.
                        let _ = tx.send(BackendUpdate::OwnerRestarted {
                            profile: profile.clone(),
                        });
                    } else {
                        let _ = tx.send(BackendUpdate::Error(message));
                    }
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    // Socket dropped mid-stream; retry outer loop with `after_seq = last_seq`.
                    if !received_any && attempt >= backoff_ms.len() {
                        let _ = tx.send(BackendUpdate::Error(
                            "Local owner stream closed unexpectedly.".into(),
                        ));
                        return;
                    }
                    let delay = backoff_ms[attempt.min(backoff_ms.len() - 1)];
                    attempt = (attempt + 1).min(backoff_ms.len());
                    let _ = tx.send(BackendUpdate::Reconnecting {
                        profile: profile.clone(),
                        message: "Reconnecting to local owner stream...".into(),
                    });
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let _ = ensure_owner_running(&profile).await;
                    break;
                }
            }
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
