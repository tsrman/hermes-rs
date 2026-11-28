//! Local session owner server and CLI client built on `hermes_runtime::local_owner`.
//!
//! Serves both:
//! - Protocol v1 (`hsr chat -q`): exact framed Event stream compatible with `compat/test_owner.py`.
//! - Protocol v2 (`hermes-gui`): multi-client reconnectable session RPCs (`ListProfiles`,
//!   `ListSessions`, `LoadSession`, `ActiveTurns`, `StartTurn`, `SubscribeTurn`, `CancelTurn`,
//!   `RespondApproval`).

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use hermes_runtime::local_owner::{
    self, discover_profiles, list_profile_sessions, load_profile_transcript, read_frame,
    read_frame_bytes, write_frame, GuiCommand, GuiReply, OwnerListener, OwnerStream, TurnHandle,
    TurnHub, TurnStreamEvent, V2Envelope, CONTROL_APPROVAL_RESPONSE, CONTROL_CANCEL, IO_TIMEOUT,
    PROTOCOL_V1, PROTOCOL_V2,
};
use hermes_runtime::turn::ApprovalDecision;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinSet;
use tokio::time::{timeout, timeout_at, Instant};

use crate::approval;
use crate::chat::{self, ChatRequest, Event};
use crate::ui::{parse_stderr_event, ParsedStderr};

const MAX_OUTPUT_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Start {
    version: u32,
    request: ChatRequest,
}

#[derive(Deserialize, Serialize)]
struct ApprovalResponse {
    id: u64,
    decision: ApprovalDecision,
}

type PendingApproval = Mutex<Option<(u64, oneshot::Sender<ApprovalDecision>)>>;

pub async fn connect(home: &Path) -> Result<Option<OwnerStream>, String> {
    local_owner::connect(home).await
}

pub async fn serve(home: &Path) -> Result<(), String> {
    let listener = OwnerListener::bind(home).await?;
    eprintln!(
        "Local session owner listening on {}",
        listener.display_address()
    );

    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| error.to_string())?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);

    let mut connections = JoinSet::new();
    let home = Arc::<Path>::from(home);
    let hub = TurnHub::new();

    // When auto-spawned by `hsr-gui`, the GUI holds our stdin pipe open and drops it when the
    // window closes. Once stdin reaches EOF and no turns are still running, exit cleanly so no
    // background `hsr-gui` process lingers in Task Manager after the window is closed.
    let exit_on_stdin_eof =
        std::env::var_os("HSR_OWNER_EXIT_ON_STDIN_EOF").is_some_and(|v| !v.is_empty() && v != "0");
    let (eof_tx, mut eof_rx) = tokio::sync::watch::channel(false);
    if exit_on_stdin_eof {
        std::thread::Builder::new()
            .name("hsr-owner-stdin-watch".into())
            .spawn(move || {
                use std::io::Read;
                let mut stdin = std::io::stdin().lock();
                let mut buf = [0u8; 64];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                let _ = eof_tx.send(true);
            })
            .ok();
    }
    let mut parent_gone = false;
    let mut idle_poll = tokio::time::interval(std::time::Duration::from_millis(250));
    idle_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    #[cfg(unix)]
    let result = loop {
        if parent_gone && hub.active_turns().iter().all(|t| t.finished) {
            break Ok(());
        }
        tokio::select! {
            result = &mut interrupt => break result.map_err(|error| error.to_string()),
            _ = terminate.recv() => break Ok(()),
            changed = eof_rx.changed(), if exit_on_stdin_eof && !parent_gone => {
                if changed.is_err() || *eof_rx.borrow() {
                    parent_gone = true;
                }
            }
            _ = idle_poll.tick(), if parent_gone => {}
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let home = home.clone();
                    let hub = hub.clone();
                    connections.spawn(async move { let _ = handle_connection(stream, home, hub).await; });
                }
                Err(error) => break Err(error.to_string()),
            }
        }
    };

    #[cfg(not(unix))]
    let result = loop {
        if parent_gone && hub.active_turns().iter().all(|t| t.finished) {
            break Ok(());
        }
        tokio::select! {
            result = &mut interrupt => break result.map_err(|error| error.to_string()),
            changed = eof_rx.changed(), if exit_on_stdin_eof && !parent_gone => {
                if changed.is_err() || *eof_rx.borrow() {
                    parent_gone = true;
                }
            }
            _ = idle_poll.tick(), if parent_gone => {}
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let home = home.clone();
                    let hub = hub.clone();
                    connections.spawn(async move { let _ = handle_connection(stream, home, hub).await; });
                }
                Err(error) => break Err(error.to_string()),
            }
        }
    };

    drop(listener);
    connections.shutdown().await;
    result
}

pub async fn forward(mut stream: OwnerStream, request: ChatRequest) -> Result<u8, String> {
    let events_json = request.events_json();
    let start = Start {
        version: PROTOCOL_V1,
        request,
    };
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let result = tokio::select! {
        result = timeout(IO_TIMEOUT, write_frame(&mut stream, &start)) => {
            result.map_err(|_| "local owner request timed out")?
        }
        signal = &mut interrupt => {
            signal.map_err(|error| format!("signal handler failed: {error}"))?;
            if events_json {
                chat::write_stdout_line(&chat::json_cancelled(130));
                chat::write_stdout_line(&chat::json_done(130, "cancelled"));
            } else {
                eprintln!("interrupted");
            }
            return Ok(130);
        }
    };
    result.map_err(|error| format!("cannot send request to local owner: {error}"))?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut cancelled = None;
    loop {
        let event = {
            let frame = read_frame::<Event>(&mut reader);
            tokio::pin!(frame);
            if let Some(deadline) = cancelled {
                read_until_cancelled(deadline, frame).await
            } else {
                tokio::select! {
                    biased;
                    signal = &mut interrupt => {
                        signal.map_err(|error| format!("signal handler failed: {error}"))?;
                        let deadline = cancel(&mut writer).await?;
                        cancelled = Some(deadline);
                        read_until_cancelled(deadline, frame).await
                    }
                    frame = &mut frame => frame.map_err(owner_disconnected),
                }
            }
        }?;
        match event {
            Event::Exit(code) => return Ok(code),
            Event::Approval { id, request } if cancelled.is_none() => {
                let decision = tokio::select! {
                    biased;
                    signal = &mut interrupt => {
                        signal.map_err(|error| format!("signal handler failed: {error}"))?;
                        cancelled = Some(cancel(&mut writer).await?);
                        continue;
                    }
                    decision = approval::prompt(request) => decision,
                };
                timeout(IO_TIMEOUT, async {
                    writer.write_u8(CONTROL_APPROVAL_RESPONSE).await?;
                    write_frame(&mut writer, &ApprovalResponse { id, decision }).await
                })
                .await
                .map_err(|_| "local owner approval response timed out")?
                .map_err(|error| error.to_string())?;
            }
            event => chat::display(event).map_err(|error| error.to_string())?,
        }
    }
}

fn owner_disconnected(error: io::Error) -> String {
    format!("local owner disconnected before reporting the turn result: {error}")
}

async fn cancel(writer: &mut (impl AsyncWrite + Unpin)) -> Result<Instant, String> {
    timeout(IO_TIMEOUT, writer.write_u8(CONTROL_CANCEL))
        .await
        .map_err(|_| "local owner did not accept cancellation")?
        .map_err(|error| error.to_string())?;
    Ok(Instant::now() + IO_TIMEOUT)
}

async fn read_until_cancelled(
    deadline: Instant,
    frame: impl std::future::Future<Output = io::Result<Event>>,
) -> Result<Event, String> {
    timeout_at(deadline, frame)
        .await
        .map_err(|_| "local owner did not acknowledge cancellation")?
        .map_err(owner_disconnected)
}

async fn read_controls(
    reader: &mut (impl AsyncRead + Unpin),
    pending: &PendingApproval,
) -> io::Result<()> {
    loop {
        match reader.read_u8().await? {
            CONTROL_CANCEL => return Ok(()),
            CONTROL_APPROVAL_RESPONSE => {
                let response: ApprovalResponse = timeout(IO_TIMEOUT, read_frame(reader)).await??;
                let mut waiting = pending.lock().unwrap_or_else(|e| e.into_inner());
                if waiting.as_ref().is_some_and(|(id, _)| *id == response.id) {
                    let (_, sender) = waiting.take().expect("matched approval");
                    let _ = sender.send(response.decision);
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid local owner control byte",
                ))
            }
        }
    }
}

async fn handle_connection(
    mut stream: OwnerStream,
    home: Arc<Path>,
    hub: TurnHub,
) -> io::Result<()> {
    let first_bytes = timeout(IO_TIMEOUT, read_frame_bytes(&mut stream)).await??;
    let probe: Value = serde_json::from_slice(&first_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid local owner frame"))?;
    let version = probe.get("version").and_then(Value::as_u64).unwrap_or(0) as u32;
    match version {
        PROTOCOL_V1 => {
            let start: Start = serde_json::from_slice(&first_bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid v1 local owner frame")
            })?;
            handle_v1(stream, &home, start.request).await
        }
        PROTOCOL_V2 => {
            let env: V2Envelope = serde_json::from_slice(&first_bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid v2 local owner frame")
            })?;
            handle_v2(stream, home, hub, env.command).await
        }
        _ => {
            write_frame(
                &mut stream,
                &Event::Stderr("incompatible local owner protocol".into()),
            )
            .await?;
            write_frame(&mut stream, &Event::Exit(1)).await
        }
    }
}

async fn handle_v1(mut stream: OwnerStream, home: &Path, request: ChatRequest) -> io::Result<()> {
    let events_json = request.events_json();
    let (mut reader, mut writer) = tokio::io::split(&mut stream);
    let (events, mut buffered) = mpsc::unbounded_channel();
    let queued = AtomicUsize::new(0);
    let stalled = Notify::new();
    let execute = async {
        let pending = PendingApproval::default();
        let next_id = AtomicU64::new(1);
        let queue = |event| {
            let bytes = match &event {
                Event::Stdout(text) | Event::Stderr(text) => text.capacity() + 64,
                Event::Approval { request, .. } => {
                    request.target.capacity() + request.content.capacity() + 128
                }
                Event::Exit(_) => 64,
            };
            if queued
                .try_update(Relaxed, Relaxed, |total| {
                    total
                        .checked_add(bytes)
                        .filter(|total| *total <= MAX_OUTPUT_BUFFER)
                })
                .is_err()
                || events.send((event, bytes)).is_err()
            {
                stalled.notify_one();
                return false;
            }
            true
        };
        let approve = |request| {
            let (sender, receiver) = oneshot::channel();
            let id = next_id.fetch_add(1, Relaxed);
            *pending.lock().unwrap_or_else(|e| e.into_inner()) = Some((id, sender));
            let queued = queue(Event::Approval { id, request });
            async move {
                if queued {
                    receiver.await.unwrap_or(ApprovalDecision::Unavailable)
                } else {
                    ApprovalDecision::Unavailable
                }
            }
        };
        let mut cancelled = false;
        let result = tokio::select! {
            biased;
            control = read_controls(&mut reader, &pending) => match control {
                Ok(()) => {
                    cancelled = true;
                    Ok(130)
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(error.to_string()),
                Err(_) => return Ok(()),
            },
            _ = stalled.notified() => Err("local owner output buffer is full".into()),
            result = chat::execute(Some(home), request, &queue, approve) => result,
        };
        if cancelled {
            hermes_runtime::mcp::flush_cancellations().await;
            if events_json {
                let _ = events.send((Event::Stdout(chat::json_cancelled(130)), 0));
                let _ = events.send((Event::Stdout(chat::json_done(130, "cancelled")), 0));
            } else {
                let _ = events.send((Event::Stderr("interrupted".into()), 0));
            }
        }
        let code = match result {
            Ok(code) => code,
            Err(error) => {
                if events_json {
                    let _ = events.send((Event::Stdout(chat::json_error(&error)), 0));
                    let _ = events.send((Event::Stdout(chat::json_done(1, "error")), 0));
                } else {
                    let _ = events.send((Event::Stderr(error), 0));
                }
                1
            }
        };
        let _ = events.send((Event::Exit(code), 0));
        drop(events);
        Ok::<(), io::Error>(())
    };
    let output = async {
        while let Some((event, bytes)) = buffered.recv().await {
            timeout(IO_TIMEOUT, write_frame(&mut writer, &event)).await??;
            queued.fetch_sub(bytes, Relaxed);
        }
        Ok(())
    };
    tokio::try_join!(execute, output)?;
    Ok(())
}

async fn handle_v2(
    mut stream: OwnerStream,
    home: Arc<Path>,
    hub: TurnHub,
    command: GuiCommand,
) -> io::Result<()> {
    match command {
        GuiCommand::Ping => {
            write_frame(
                &mut stream,
                &GuiReply::Pong {
                    profile_home: home.display().to_string(),
                },
            )
            .await
        }
        GuiCommand::ListProfiles => {
            let profiles = discover_profiles(&home);
            write_frame(&mut stream, &GuiReply::Profiles { profiles }).await
        }
        GuiCommand::ListSessions { limit, query } => {
            let reply = match list_profile_sessions(&home, limit, query.as_deref()) {
                Ok(sessions) => GuiReply::Sessions { sessions },
                Err(message) => GuiReply::Error { message },
            };
            write_frame(&mut stream, &reply).await
        }
        GuiCommand::LoadSession { session_id } => {
            let reply = match load_profile_transcript(&home, &session_id) {
                Ok((session, messages)) => {
                    let active_turn_id = hub.active_turn_for_session(&session.id);
                    GuiReply::Transcript {
                        session,
                        messages,
                        active_turn_id,
                    }
                }
                Err(message) => GuiReply::Error { message },
            };
            write_frame(&mut stream, &reply).await
        }
        GuiCommand::DeleteSession { session_id } => {
            let reply = if hub.active_turn_for_session(&session_id).is_some() {
                GuiReply::Error {
                    message:
                        "Cannot delete a conversation while a turn is running. Cancel it first."
                            .into(),
                }
            } else {
                match hermes_runtime::storage::delete_session(&home, &session_id) {
                    Ok(_) => {
                        hermes_runtime::terminal::forget_session(&session_id);
                        hub.forget_session_grants(&session_id);
                        GuiReply::Ack
                    }
                    Err(err) => GuiReply::Error {
                        message: err.to_string(),
                    },
                }
            };
            write_frame(&mut stream, &reply).await
        }
        GuiCommand::ActiveTurns => {
            let turns = hub.active_turns();
            write_frame(&mut stream, &GuiReply::ActiveTurns { turns }).await
        }
        GuiCommand::StartTurn {
            session_id,
            query,
            model,
            cwd,
            yolo,
        } => {
            let request = match chat::prepare_gui_turn(
                &home,
                session_id.clone(),
                query.clone(),
                model,
                cwd,
                yolo,
            ) {
                Ok(req) => req,
                Err(message) => {
                    return write_frame(&mut stream, &GuiReply::Error { message }).await;
                }
            };
            hermes_runtime::approval_policy::warm_up();
            let policy = request.approval_policy();
            if let (Some(sid), Some(pol)) = (session_id.as_deref(), policy.as_deref()) {
                if let Some(saved) = hub.session_grants(sid) {
                    pol.apply_session_grants(&saved);
                }
            }
            let handle = hub.create_turn_with_policy(session_id, query, policy);
            let turn_id = handle.turn_id();
            let home_bg = Arc::clone(&home);
            tokio::spawn(async move {
                run_managed_turn(home_bg, request, handle).await;
            });
            write_frame(&mut stream, &GuiReply::TurnStarted { turn_id }).await
        }
        GuiCommand::SubscribeTurn { turn_id, after_seq } => {
            let mut cursor = after_seq;
            loop {
                let (missed, mut rx, finished) = match hub.subscribe(&turn_id, cursor) {
                    Ok(sub) => sub,
                    Err(message) => {
                        return write_frame(&mut stream, &GuiReply::Error { message }).await
                    }
                };
                for event in missed {
                    cursor = event.seq;
                    write_frame(&mut stream, &GuiReply::TurnEvent(event)).await?;
                }
                if finished {
                    return Ok(());
                }
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            cursor = event.seq;
                            let done = matches!(event.event, TurnStreamEvent::Done { .. });
                            write_frame(&mut stream, &GuiReply::TurnEvent(event)).await?;
                            if done {
                                return Ok(());
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
        GuiCommand::CancelTurn { turn_id } => {
            let reply = match hub.cancel(&turn_id) {
                Ok(()) => GuiReply::Ack,
                Err(message) => GuiReply::Error { message },
            };
            write_frame(&mut stream, &reply).await
        }
        GuiCommand::RespondApproval {
            turn_id,
            approval_id,
            decision,
            scope,
        } => {
            let reply = match hub.respond_approval(&turn_id, approval_id, decision, scope) {
                Ok(None) => GuiReply::Ack,
                Ok(Some(warn)) => GuiReply::Error {
                    message: format!("Approved, but failed saving permanent setting: {warn}"),
                },
                Err(message) => GuiReply::Error { message },
            };
            write_frame(&mut stream, &reply).await
        }
    }
}

async fn run_managed_turn(home: Arc<Path>, request: ChatRequest, handle: TurnHandle) {
    let (emitter, cancel_rx) = handle.split();
    let emit_event = |event: Event| {
        match event {
            Event::Stdout(text) => emitter.emit(TurnStreamEvent::TextDelta { text }),
            Event::Stderr(text) => match parse_stderr_event(&text) {
                ParsedStderr::Session(session_id) => {
                    emitter.emit(TurnStreamEvent::SessionAssigned { session_id })
                }
                ParsedStderr::ToolStarted { name, detail } => {
                    emitter.emit(TurnStreamEvent::ToolStarted { name, detail })
                }
                ParsedStderr::ToolOutput { name, text } => {
                    emitter.emit(TurnStreamEvent::ToolOutput { name, text })
                }
                ParsedStderr::ToolFinished {
                    name,
                    succeeded,
                    summary,
                } => emitter.emit(TurnStreamEvent::ToolFinished {
                    name,
                    succeeded,
                    summary,
                }),
                ParsedStderr::Reasoning(text) => {
                    emitter.emit(TurnStreamEvent::ReasoningDelta { text })
                }
                _ => emitter.emit(TurnStreamEvent::Status { text }),
            },
            Event::Approval { .. } | Event::Exit(_) => {}
        }
        true
    };
    let approve = |req| {
        let rx = emitter.request_approval(req);
        async move { rx.await.unwrap_or(ApprovalDecision::Unavailable) }
    };

    let mut cancelled = false;
    let outcome = tokio::select! {
        biased;
        _ = cancel_rx => {
            cancelled = true;
            Ok(130)
        }
        res = chat::execute(Some(&home), request, &emit_event, approve) => res,
    };

    if cancelled {
        hermes_runtime::mcp::flush_cancellations().await;
        emitter.emit(TurnStreamEvent::Done {
            exit_code: 130,
            cancelled: true,
        });
        return;
    }

    match outcome {
        Ok(exit_code) => emitter.emit(TurnStreamEvent::Done {
            exit_code,
            cancelled: false,
        }),
        Err(message) => {
            emitter.emit(TurnStreamEvent::Error { message });
            emitter.emit(TurnStreamEvent::Done {
                exit_code: 1,
                cancelled: false,
            });
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn lagged_subscriber_gets_contiguous_replay_through_done() {
        let hub = TurnHub::new();
        let turn = hub.create_turn(None, String::new());
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_v2(
            OwnerStream::Unix(server),
            Arc::from(Path::new(".")),
            hub,
            GuiCommand::SubscribeTurn {
                turn_id: turn.turn_id(),
                after_seq: 0,
            },
        ));
        turn.emit(TurnStreamEvent::Status {
            text: "ready".into(),
        });
        let first: GuiReply = read_frame(&mut client).await.unwrap();
        assert!(matches!(first, GuiReply::TurnEvent(ev) if ev.seq == 1));

        // The single-thread executor cannot poll the subscriber during this burst.
        for _ in 0..300 {
            turn.emit(TurnStreamEvent::TextDelta { text: "a".into() });
        }
        turn.emit(TurnStreamEvent::Done {
            exit_code: 0,
            cancelled: false,
        });
        for seq in 2..=302 {
            let reply: GuiReply = read_frame(&mut client).await.unwrap();
            assert!(matches!(reply, GuiReply::TurnEvent(ev) if ev.seq == seq));
        }
        task.await.unwrap().unwrap();
    }
}
