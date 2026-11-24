//! Fullscreen Rust TUI built on `ratatui` + `crossterm`.
//!
//! Layout:
//! - Top status bar: Hermes title, profile, provider:model, session ID, YOLO badge, token/context meter
//! - Center scrollable conversation feed: Markdown-styled messages, framed tool cards, notices
//! - Floating `/` slash-command completion palette above the input box
//! - Multiline input composer with cursor, history, Alt+Enter/Shift+Enter newlines, live spinner
//! - Centered interactive modals for `approval` and `clarify`

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use hermes_runtime::turn::{ApprovalAction, ApprovalDecision, ApprovalRequest};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::approval::{self, Grant};
use crate::chat::{self, Event as ChatEvent, ManualCompress};
use crate::ui::markdown::render_markdown_lines;
use crate::ui::{
    format_duration_ms, format_tokens_compact, match_slash_commands, parse_stderr_event, ActiveUi,
    FeedEntry, NoticeKind, ParsedStderr, ReplState, ToolStatus, UiOutcome, HELP_TEXT,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// RAII guard that guarantees terminal raw mode and alternate screen are restored on drop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

enum TurnMsg {
    Event(ChatEvent),
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    Finished(Result<u8, String>),
}

struct ClarifyQuestion {
    qid: String,
    prompt: String,
    choices: Vec<String>,
    multi_select: bool,
}

enum ActiveModal {
    Approval {
        request: ApprovalRequest,
        reason: Option<String>,
        reply: Option<oneshot::Sender<ApprovalDecision>>,
        scroll: u16,
    },
    Clarify {
        questions: Vec<ClarifyQuestion>,
        current: usize,
        cursor: usize,
        selected: BTreeSet<usize>,
        custom: String,
        answers: Map<String, Value>,
        reply: Option<oneshot::Sender<ApprovalDecision>>,
    },
}

pub struct TuiApp {
    input: String,
    /// Byte index of cursor in `input` (always on a UTF-8 char boundary).
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft_before_history: String,
    palette_index: usize,
    scroll_from_bottom: usize,
    spinner_tick: usize,
    streaming_assistant: String,
    active_tools: HashMap<String, Instant>,
    turn_started: Option<Instant>,
    turn_saved_session: Option<String>,
    turn_usage: Option<(u64, u64)>,
    turn_query: Option<String>,
    modal: Option<ActiveModal>,
    should_exit: Option<UiOutcome>,
}

impl TuiApp {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            draft_before_history: String::new(),
            palette_index: 0,
            scroll_from_bottom: 0,
            spinner_tick: 0,
            streaming_assistant: String::new(),
            active_tools: HashMap::new(),
            turn_started: None,
            turn_saved_session: None,
            turn_usage: None,
            turn_query: None,
            modal: None,
            should_exit: None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.turn_started.is_some()
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.palette_index = 0;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.input.drain(prev..self.cursor);
        self.cursor = prev;
        self.palette_index = 0;
    }

    fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let head = &self.input[..self.cursor];
        let trimmed_end = head.trim_end().len();
        let new_pos = head[..trimmed_end]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.input.drain(new_pos..self.cursor);
        self.cursor = new_pos;
    }

    fn delete_char_forward(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = self.input[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor);
        self.input.drain(self.cursor..next);
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.input.len() {
            if let Some(c) = self.input[self.cursor..].chars().next() {
                self.cursor += c.len_utf8();
            }
        }
    }

    fn slash_palette(&self) -> Vec<&'static crate::ui::SlashCommand> {
        if self.is_busy() || self.modal.is_some() || self.input.contains('\n') {
            return Vec::new();
        }
        if self.input.starts_with('/') && !self.input.contains(char::is_whitespace) {
            match_slash_commands(&self.input)
        } else {
            Vec::new()
        }
    }

    fn push_notice(state: &mut ReplState, kind: NoticeKind, text: impl Into<String>) {
        state.feed_entries.push(FeedEntry::Notice {
            kind,
            text: text.into(),
        });
    }
}

/// Run the fullscreen TUI loop until the user exits or switches to another UI mode (`/ui inline`).
pub async fn run(home: Option<&Path>, state: &mut ReplState) -> Result<UiOutcome, String> {
    let _guard = TerminalGuard::enter().map_err(|e| format!("cannot enter fullscreen TUI: {e}"))?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("cannot initialize TUI terminal: {e}"))?;
    let _ = terminal.clear();

    let mut app = TuiApp::new();
    let stop_reader = Arc::new(AtomicBool::new(false));
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<TermEvent>();
    let stop_flag = Arc::clone(&stop_reader);
    let reader_thread = std::thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(25)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if ev_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut tick = tokio::time::interval(Duration::from_millis(65));
    let mut active_handle: Option<JoinHandle<()>> = None;
    let mut turn_rx: Option<mpsc::UnboundedReceiver<TurnMsg>> = None;

    if state.feed_entries.is_empty() {
        TuiApp::push_notice(
            state,
            NoticeKind::Info,
            "Welcome to Hermes TUI. Type a message, /help for commands, or /ui inline to return to streaming mode.",
        );
    }

    let mut dirty = true;
    let outcome = loop {
        if dirty {
            terminal
                .draw(|frame| render_frame(frame, &app, state))
                .map_err(|e| format!("TUI draw failed: {e}"))?;
            dirty = false;
        }

        if let Some(outcome) = app.should_exit {
            break outcome;
        }

        tokio::select! {
            _ = tick.tick(), if app.is_busy() => {
                app.spinner_tick = app.spinner_tick.wrapping_add(1);
                dirty = true;
            }
            maybe_msg = async {
                match turn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                dirty = true;
                match maybe_msg {
                    Some(TurnMsg::Event(ev)) => {
                        handle_turn_event(&mut app, state, ev);
                    }
                    Some(TurnMsg::Approval { request, reply }) => {
                        open_modal(&mut app, request, reply);
                    }
                    Some(TurnMsg::Finished(res)) => {
                        active_handle = None;
                        turn_rx = None;
                        finish_turn(&mut app, state, res);
                    }
                    None => {
                        active_handle = None;
                        turn_rx = None;
                    }
                }
            }
            maybe_ev = ev_rx.recv() => {
                match maybe_ev {
                    Some(TermEvent::Key(key)) => {
                        dirty = true;
                        if let Some(query) = handle_key_event(
                            &mut app,
                            state,
                            home,
                            key,
                            &mut active_handle,
                            &mut turn_rx,
                        ).await {
                            start_turn(home, &mut app, state, query, &mut active_handle, &mut turn_rx);
                        }
                    }
                    Some(TermEvent::Paste(pasted)) if !app.is_busy() && app.modal.is_none() => {
                        app.input.insert_str(app.cursor, &pasted);
                        app.cursor += pasted.len();
                        dirty = true;
                    }
                    Some(TermEvent::Resize(_, _)) => {
                        dirty = true;
                    }
                    None => break UiOutcome::Exit,
                    _ => {}
                }
            }
        }
    };

    stop_reader.store(true, Ordering::Relaxed);
    let _ = reader_thread.join();
    if let Some(handle) = active_handle.take() {
        handle.abort();
        hermes_runtime::mcp::flush_cancellations().await;
    }
    Ok(outcome)
}

fn open_modal(
    app: &mut TuiApp,
    request: ApprovalRequest,
    reply: oneshot::Sender<ApprovalDecision>,
) {
    if matches!(request.action, ApprovalAction::Clarify) {
        let questions = parse_clarify_questions(&request.content);
        if questions.is_empty() {
            let _ = reply.send(ApprovalDecision::Unavailable);
            return;
        }
        app.modal = Some(ActiveModal::Clarify {
            questions,
            current: 0,
            cursor: 0,
            selected: BTreeSet::new(),
            custom: String::new(),
            answers: Map::new(),
            reply: Some(reply),
        });
    } else {
        let reason = if matches!(request.action, ApprovalAction::RunCommand) {
            approval::explain_flagged(&request)
        } else {
            None
        };
        app.modal = Some(ActiveModal::Approval {
            request,
            reason,
            reply: Some(reply),
            scroll: 0,
        });
    }
}

fn parse_clarify_questions(content: &str) -> Vec<ClarifyQuestion> {
    let Ok(list) = serde_json::from_str::<Vec<Value>>(content) else {
        return Vec::new();
    };
    list.into_iter()
        .map(|q| ClarifyQuestion {
            qid: q["qid"].as_str().unwrap_or_default().to_owned(),
            prompt: approval::sanitize_terminal_text(q["question"].as_str().unwrap_or("")),
            choices: q["choices"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.as_str().map(approval::sanitize_terminal_text))
                        .collect()
                })
                .unwrap_or_default(),
            multi_select: q["multi_select"].as_bool() == Some(true),
        })
        .collect()
}

fn handle_turn_event(app: &mut TuiApp, state: &mut ReplState, ev: ChatEvent) {
    match ev {
        ChatEvent::Stdout(chunk) => {
            if chunk == "\n" && app.streaming_assistant.is_empty() {
                return;
            }
            app.streaming_assistant.push_str(&chunk);
        }
        ChatEvent::Stderr(text) => match parse_stderr_event(&text) {
            ParsedStderr::Session(id) => app.turn_saved_session = Some(id),
            ParsedStderr::Usage { input, output } => app.turn_usage = Some((input, output)),
            ParsedStderr::ToolStarted(name) => {
                if !app.streaming_assistant.trim().is_empty() {
                    let flushed = std::mem::take(&mut app.streaming_assistant);
                    state
                        .feed_entries
                        .push(FeedEntry::Assistant(flushed.trim_end().to_owned()));
                } else {
                    app.streaming_assistant.clear();
                }
                app.active_tools.insert(name.clone(), Instant::now());
                state.feed_entries.push(FeedEntry::Tool {
                    name,
                    status: ToolStatus::Running,
                    elapsed_ms: None,
                });
            }
            ParsedStderr::ToolFinished { name, succeeded } => {
                let elapsed = app
                    .active_tools
                    .remove(&name)
                    .map(|t| t.elapsed().as_millis() as u64);
                let new_status = if succeeded {
                    ToolStatus::Succeeded
                } else {
                    ToolStatus::Failed
                };
                // Update the most recent matching Running tool card in place.
                let updated = state.feed_entries.iter_mut().rev().find(|e| {
                    matches!(
                        e,
                        FeedEntry::Tool {
                            name: n,
                            status: ToolStatus::Running,
                            ..
                        } if n == &name
                    )
                });
                if let Some(FeedEntry::Tool {
                    status, elapsed_ms, ..
                }) = updated
                {
                    *status = new_status;
                    *elapsed_ms = elapsed;
                } else {
                    state.feed_entries.push(FeedEntry::Tool {
                        name,
                        status: new_status,
                        elapsed_ms: elapsed,
                    });
                }
            }
            ParsedStderr::Fallback(model) => {
                TuiApp::push_notice(state, NoticeKind::Warn, format!("Model fallback → {model}"));
            }
            ParsedStderr::ContextCompressed(msg) => {
                TuiApp::push_notice(state, NoticeKind::Info, msg);
            }
            ParsedStderr::IterationLimit(msg) => {
                TuiApp::push_notice(state, NoticeKind::Warn, msg);
            }
            ParsedStderr::Other(msg) => {
                TuiApp::push_notice(state, NoticeKind::Info, msg);
            }
        },
        ChatEvent::Exit(_) | ChatEvent::Approval { .. } => {}
    }
}

fn finish_turn(app: &mut TuiApp, state: &mut ReplState, result: Result<u8, String>) {
    let elapsed = app
        .turn_started
        .take()
        .map(|s| s.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    app.active_tools.clear();
    let final_text = std::mem::take(&mut app.streaming_assistant);
    if !final_text.trim().is_empty() {
        state
            .feed_entries
            .push(FeedEntry::Assistant(final_text.trim_end().to_owned()));
    }
    match result {
        Ok(status) => {
            state.exit_code = status;
            state.unsaved = None;
            if let Some((input, output)) = app.turn_usage.take() {
                state.record_usage(input, output);
                if input + output > 0 {
                    TuiApp::push_notice(
                        state,
                        NoticeKind::Info,
                        format!("[{elapsed:.1}s · {input} in / {output} out]"),
                    );
                } else {
                    TuiApp::push_notice(state, NoticeKind::Info, format!("[{elapsed:.1}s]"));
                }
            } else {
                TuiApp::push_notice(state, NoticeKind::Info, format!("[{elapsed:.1}s]"));
            }
        }
        Err(error) => {
            TuiApp::push_notice(state, NoticeKind::Error, error);
            if let Some(q) = app.turn_query.take() {
                state.unsaved = Some(q);
            }
        }
    }
    if let Some(id) = app.turn_saved_session.take() {
        state.session = Some(id);
    }
    app.turn_query = None;
}

fn start_turn(
    home: Option<&Path>,
    app: &mut TuiApp,
    state: &mut ReplState,
    query: String,
    active_handle: &mut Option<JoinHandle<()>>,
    turn_rx: &mut Option<mpsc::UnboundedReceiver<TurnMsg>>,
) {
    state.feed_entries.push(FeedEntry::User(query.clone()));
    app.scroll_from_bottom = 0;
    app.streaming_assistant.clear();
    app.active_tools.clear();
    app.turn_started = Some(Instant::now());
    app.turn_saved_session = None;
    app.turn_usage = None;
    app.turn_query = Some(query.clone());

    let turn_request =
        state
            .request
            .clone()
            .for_turn(&query, &state.scope, state.session.as_deref());
    let home_owned = home.map(Path::to_path_buf);
    let (tx, rx) = mpsc::unbounded_channel::<TurnMsg>();
    *turn_rx = Some(rx);

    let handle = tokio::spawn(async move {
        let tx_events = tx.clone();
        let tx_approve = tx.clone();
        let res = chat::execute(
            home_owned.as_deref(),
            turn_request,
            move |ev| tx_events.send(TurnMsg::Event(ev)).is_ok(),
            move |req| {
                let tx_a = tx_approve.clone();
                async move {
                    if let Some(decision) = approval::precheck_policy(&req) {
                        return decision;
                    }
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if tx_a
                        .send(TurnMsg::Approval {
                            request: req,
                            reply: reply_tx,
                        })
                        .is_err()
                    {
                        return ApprovalDecision::Unavailable;
                    }
                    reply_rx.await.unwrap_or(ApprovalDecision::Denied)
                }
            },
        )
        .await;
        let _ = tx.send(TurnMsg::Finished(res));
    });
    *active_handle = Some(handle);
}

/// Handle a keyboard event. Returns `Some(query)` when the user submits a prompt to send to the model.
async fn handle_key_event(
    app: &mut TuiApp,
    state: &mut ReplState,
    home: Option<&Path>,
    key: KeyEvent,
    active_handle: &mut Option<JoinHandle<()>>,
    turn_rx: &mut Option<mpsc::UnboundedReceiver<TurnMsg>>,
) -> Option<String> {
    // 1. Active modal consumes keys first (except Ctrl-C which cancels the turn).
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(handle) = active_handle.take() {
            handle.abort();
            *turn_rx = None;
            app.modal = None;
            app.turn_started = None;
            app.active_tools.clear();
            let partial = std::mem::take(&mut app.streaming_assistant);
            if !partial.trim().is_empty() {
                state
                    .feed_entries
                    .push(FeedEntry::Assistant(partial.trim_end().to_owned()));
            }
            if let Some(q) = app.turn_query.take() {
                state.unsaved = Some(q);
            }
            hermes_runtime::mcp::flush_cancellations().await;
            TuiApp::push_notice(
                state,
                NoticeKind::Warn,
                "interrupted (the turn is not saved; /retry sends it again)",
            );
            return None;
        }
        // When idle, Ctrl-C clears current composer text (or exits if already empty).
        if !app.input.is_empty() {
            app.input.clear();
            app.cursor = 0;
        }
        return None;
    }

    if app.modal.is_some() {
        handle_modal_key(app, state, key);
        return None;
    }

    // Ctrl-D exits when composer is empty and idle.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        if app.input.is_empty() && !app.is_busy() {
            app.should_exit = Some(UiOutcome::Exit);
        }
        return None;
    }

    // Feed scrolling keys (always active, even during streaming).
    match (key.code, key.modifiers) {
        (KeyCode::PageUp, _) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(10);
            return None;
        }
        (KeyCode::PageDown, _) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(10);
            return None;
        }
        (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(3);
            return None;
        }
        (KeyCode::Down, m) if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(3);
            return None;
        }
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(12);
            return None;
        }
        (KeyCode::End, m) if m.contains(KeyModifiers::CONTROL) => {
            app.scroll_from_bottom = 0;
            return None;
        }
        _ => {}
    }

    if app.is_busy() {
        return None;
    }

    // Slash palette navigation
    let palette = app.slash_palette();
    if !palette.is_empty() {
        match key.code {
            KeyCode::Tab => {
                let idx = app.palette_index.min(palette.len() - 1);
                app.input = palette[idx].name.to_owned();
                if matches!(
                    app.input.as_str(),
                    "/resume" | "/model" | "/ui" | "/compress"
                ) {
                    app.input.push(' ');
                }
                app.cursor = app.input.len();
                app.palette_index = 0;
                return None;
            }
            KeyCode::Up => {
                app.palette_index = app.palette_index.saturating_sub(1);
                return None;
            }
            KeyCode::Down => {
                if app.palette_index + 1 < palette.len() {
                    app.palette_index += 1;
                }
                return None;
            }
            _ => {}
        }
    }

    // Composer editing & submission
    match (key.code, key.modifiers) {
        (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) => {
            app.insert_char('\n');
        }
        (KeyCode::Enter, _) => {
            // Support trailing `\` continuation just like inline REPL.
            if app.cursor == app.input.len() && app.input.ends_with('\\') {
                app.backspace();
                app.insert_char('\n');
                return None;
            }
            let text = app.input.trim().to_owned();
            app.input.clear();
            app.cursor = 0;
            app.history_pos = None;
            if text.is_empty() {
                return None;
            }
            if let Some(cmd) = text.strip_prefix('/').filter(|_| !text.contains('\n')) {
                return handle_slash_command(app, state, home, cmd).await;
            }
            app.history.push(text.clone());
            return Some(text);
        }
        (KeyCode::Backspace, _) => app.backspace(),
        (KeyCode::Delete, _) => app.delete_char_forward(),
        (KeyCode::Left, _) => app.move_left(),
        (KeyCode::Right, _) => app.move_right(),
        (KeyCode::Home, _) => app.cursor = 0,
        (KeyCode::End, _) => app.cursor = app.input.len(),
        (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => app.cursor = 0,
        (KeyCode::Char('e'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.cursor = app.input.len();
        }
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => app.delete_word_back(),
        (KeyCode::Up, _) if !app.history.is_empty() => {
            let next_idx = match app.history_pos {
                None => {
                    app.draft_before_history = app.input.clone();
                    app.history.len() - 1
                }
                Some(i) => i.saturating_sub(1),
            };
            app.history_pos = Some(next_idx);
            app.input = app.history[next_idx].clone();
            app.cursor = app.input.len();
        }
        (KeyCode::Down, _) => {
            if let Some(i) = app.history_pos {
                if i + 1 < app.history.len() {
                    app.history_pos = Some(i + 1);
                    app.input = app.history[i + 1].clone();
                } else {
                    app.history_pos = None;
                    app.input = std::mem::take(&mut app.draft_before_history);
                }
                app.cursor = app.input.len();
            }
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            app.insert_char(c);
        }
        _ => {}
    }
    None
}

fn handle_modal_key(app: &mut TuiApp, state: &mut ReplState, key: KeyEvent) {
    let Some(mut modal) = app.modal.take() else {
        return;
    };
    match &mut modal {
        ActiveModal::Approval {
            request,
            reply,
            scroll,
            ..
        } => {
            let is_command = matches!(request.action, ApprovalAction::RunCommand);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                    app.modal = Some(modal);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(1);
                    app.modal = Some(modal);
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(tx) = reply.take() {
                        let _ = tx.send(ApprovalDecision::Approved);
                    }
                }
                KeyCode::Char('s') | KeyCode::Char('S') if is_command => {
                    let _ = approval::record_grant(request, Grant::Session);
                    if let Some(tx) = reply.take() {
                        let _ = tx.send(ApprovalDecision::Approved);
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    let grant = if is_command {
                        Grant::Always
                    } else {
                        Grant::Session
                    };
                    if let Some(err) = approval::record_grant(request, grant) {
                        TuiApp::push_notice(
                            state,
                            NoticeKind::Warn,
                            format!("Could not save command_allowlist: {err}"),
                        );
                    }
                    if let Some(tx) = reply.take() {
                        let _ = tx.send(ApprovalDecision::Approved);
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(tx) = reply.take() {
                        let _ = tx.send(ApprovalDecision::Denied);
                    }
                }
                _ => {
                    app.modal = Some(modal);
                }
            }
        }
        ActiveModal::Clarify {
            questions,
            current,
            cursor,
            selected,
            custom,
            answers,
            reply,
        } => {
            let q = &questions[*current];
            match key.code {
                KeyCode::Esc => {
                    if let Some(tx) = reply.take() {
                        let _ = tx.send(ApprovalDecision::Denied);
                    }
                }
                KeyCode::Up if !q.choices.is_empty() => {
                    *cursor = cursor.saturating_sub(1);
                    app.modal = Some(modal);
                }
                KeyCode::Down if !q.choices.is_empty() => {
                    if *cursor + 1 < q.choices.len() {
                        *cursor += 1;
                    }
                    app.modal = Some(modal);
                }
                KeyCode::Char(' ')
                    if q.multi_select && custom.is_empty() && !q.choices.is_empty() =>
                {
                    if !selected.remove(cursor) {
                        selected.insert(*cursor);
                    }
                    app.modal = Some(modal);
                }
                KeyCode::Backspace => {
                    custom.pop();
                    app.modal = Some(modal);
                }
                KeyCode::Enter => {
                    let answer = if !custom.trim().is_empty() {
                        approval::parse_clarify_answer(custom, &q.choices, q.multi_select)
                            .unwrap_or(Value::Null)
                    } else if q.multi_select {
                        if selected.is_empty() {
                            Value::Null
                        } else {
                            Value::Array(
                                selected
                                    .iter()
                                    .filter_map(|&i| q.choices.get(i))
                                    .map(|c| Value::String(c.clone()))
                                    .collect(),
                            )
                        }
                    } else if let Some(choice) = q.choices.get(*cursor) {
                        Value::String(choice.clone())
                    } else {
                        Value::Null
                    };
                    answers.insert(q.qid.clone(), answer);
                    *current += 1;
                    *cursor = 0;
                    selected.clear();
                    custom.clear();
                    if *current >= questions.len() {
                        let payload = json!({"answers": answers, "outcome": "submitted"});
                        if let Some(tx) = reply.take() {
                            let _ = tx.send(ApprovalDecision::Answered(payload.to_string()));
                        }
                    } else {
                        app.modal = Some(modal);
                    }
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    custom.push(c);
                    app.modal = Some(modal);
                }
                _ => {
                    app.modal = Some(modal);
                }
            }
        }
    }
}

async fn handle_slash_command(
    app: &mut TuiApp,
    state: &mut ReplState,
    home: Option<&Path>,
    command: &str,
) -> Option<String> {
    let (name, argument) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(n, r)| (n, r.trim()));
    match name {
        "exit" | "quit" => app.should_exit = Some(UiOutcome::Exit),
        "help" => TuiApp::push_notice(state, NoticeKind::Info, HELP_TEXT),
        "ui" if argument.is_empty() => {
            TuiApp::push_notice(
                state,
                NoticeKind::Info,
                "Current UI mode: tui (options: inline, tui, plain)",
            );
        }
        "ui" => match argument.to_ascii_lowercase().as_str() {
            "inline" | "stream" | "repl" => {
                app.should_exit = Some(UiOutcome::Switch(ActiveUi::Inline));
            }
            "plain" | "off" | "none" => {
                app.should_exit = Some(UiOutcome::Switch(ActiveUi::Plain));
            }
            "tui" | "fullscreen" | "full" => {
                TuiApp::push_notice(state, NoticeKind::Info, "Already in fullscreen TUI mode.");
            }
            other => {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    format!("Unknown UI mode '{other}'. Expected: inline, tui, or plain."),
                );
            }
        },
        "new" => {
            state.session = None;
            state.unsaved = None;
            state.feed_entries.clear();
            state.guard.lock().unwrap().reset();
            hermes_runtime::terminal::forget_session(&state.scope);
            TuiApp::push_notice(state, NoticeKind::Info, "Started a new conversation.");
        }
        "model" if argument.is_empty() => {
            TuiApp::push_notice(
                state,
                NoticeKind::Info,
                format!(
                    "Current model: {}",
                    state.request.model().unwrap_or("(not set)")
                ),
            );
        }
        "model" => {
            state.request.set_model(argument);
            state.guard.lock().unwrap().reset();
            TuiApp::push_notice(
                state,
                NoticeKind::Info,
                format!("Model for the next turns: {argument}"),
            );
        }
        "yolo" => {
            if let Some(policy) = hermes_runtime::approval_policy::installed() {
                let on = !policy.approves_all();
                policy.set_approve_all(on);
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    if on {
                        "yolo on: file changes and commands run without asking, except what approvals.deny names"
                    } else {
                        "yolo off: you will be asked again"
                    },
                );
            }
        }
        "sessions" => {
            let mut text = String::new();
            if let Some(home) = home {
                match hermes_runtime::storage::SessionStore::open_read_only(home)
                    .and_then(|s| s.recent(10, None))
                {
                    Ok(list) if !list.is_empty() => {
                        for s in list {
                            let preview =
                                s.title.or(s.preview).unwrap_or_default().replace('\n', " ");
                            text.push_str(&format!(
                                "{}  {:>4} msgs  {}\n",
                                s.id, s.message_count, preview
                            ));
                        }
                    }
                    _ => text.push_str("No saved sessions yet."),
                }
            }
            TuiApp::push_notice(state, NoticeKind::Info, text.trim_end());
        }
        "resume" if argument.is_empty() => {
            TuiApp::push_notice(state, NoticeKind::Warn, "Usage: /resume <session id>");
        }
        "resume" => {
            state.session = Some(argument.to_owned());
            state.unsaved = None;
            state.feed_entries.clear();
            state.guard.lock().unwrap().reset();
            hermes_runtime::terminal::forget_session(&state.scope);
            TuiApp::push_notice(
                state,
                NoticeKind::Info,
                format!("The next turn continues session {argument}"),
            );
        }
        "compress" | "compact" => {
            let Some(home) = home else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    "Cannot compress: profile root is unavailable.",
                );
                return None;
            };
            let Some(id) = state.session.as_deref() else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    "Nothing to compress yet: start a conversation first.",
                );
                return None;
            };
            let focus = (!argument.is_empty()).then_some(argument);
            match chat::compress_session(home, &state.request, id, focus, &state.guard).await {
                Ok(ManualCompress::Applied { archived }) => TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    format!(
                        "Compressed session {id}: archived {archived} message(s) into a summary."
                    ),
                ),
                Ok(ManualCompress::Unchanged) => TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    "Nothing to compress: conversation is short enough already.",
                ),
                Ok(ManualCompress::Skipped) => TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    "Compression skipped: summary failed or did not shorten the conversation.",
                ),
                Err(error) => {
                    TuiApp::push_notice(
                        state,
                        NoticeKind::Error,
                        format!("Compression failed: {error}"),
                    );
                }
            }
        }
        "retry" => match state.unsaved.take() {
            Some(previous) => return Some(previous),
            None => TuiApp::push_notice(
                state,
                NoticeKind::Info,
                "Nothing to retry: the last turn finished and is saved.",
            ),
        },
        other => TuiApp::push_notice(
            state,
            NoticeKind::Warn,
            format!("Unknown command /{other}; /help lists the commands."),
        ),
    }
    None
}

// ── Pure Ratatui Frame Renderer (directly unit-testable with TestBackend) ───────────

pub fn render_frame(frame: &mut Frame<'_>, app: &TuiApp, state: &ReplState) {
    let area = frame.area();
    let composer_lines = (app.input.lines().count().max(1) as u16).clamp(1, 5);
    let composer_height = composer_lines + 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, chunks[0], state);
    render_feed(frame, chunks[1], app, state);
    render_composer(frame, chunks[2], app);
    render_footer(frame, chunks[3], app);

    // Floating slash-command palette above the composer
    let palette = app.slash_palette();
    if !palette.is_empty() {
        render_palette(frame, chunks[1], &palette, app.palette_index);
    }

    // Centered modal for Approval or Clarify
    if let Some(modal) = &app.modal {
        render_modal(frame, area, modal);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &ReplState) {
    let model = state.request.model().unwrap_or("(unset)");
    let provider = state.request.provider_name();
    let sess = state
        .session
        .as_deref()
        .map(|s| s.chars().take(10).collect::<String>())
        .unwrap_or_else(|| "new".to_owned());
    let yolo = hermes_runtime::approval_policy::installed().is_some_and(|p| p.approves_all());

    let mut spans = vec![
        Span::styled(
            " HERMES ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            model,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({provider})"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(&state.profile_label, Style::default().fg(Color::Cyan)),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("sess:{sess}"), Style::default().fg(Color::DarkGray)),
    ];

    if yolo {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " YOLO ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let total = state.total_input_tokens + state.total_output_tokens;
    if total > 0 {
        let ctx_pct = state
            .request
            .context_length()
            .filter(|&w| w > 0)
            .map(|w| {
                let pct = ((state.last_turn_input_tokens as f64 / w as f64) * 100.0).min(100.0);
                format!(" · ctx {pct:.0}%")
            })
            .unwrap_or_default();
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!(
                "{} in / {} out{ctx_pct}",
                format_tokens_compact(state.total_input_tokens),
                format_tokens_compact(state.total_output_tokens)
            ),
            Style::default().fg(Color::Green),
        ));
    }

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    frame.render_widget(header, area);
}

fn render_feed(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, state: &ReplState) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let spinner_glyph = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];

    for entry in &state.feed_entries {
        match entry {
            FeedEntry::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled("▎ ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        "You",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                for l in text.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("▎ ", Style::default().fg(Color::Cyan)),
                        Span::raw(l.to_owned()),
                    ]));
                }
                lines.push(Line::from(""));
            }
            FeedEntry::Assistant(text) => {
                lines.push(Line::from(Span::styled(
                    "Hermes",
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(render_markdown_lines(text, state.color));
                lines.push(Line::from(""));
            }
            FeedEntry::Tool {
                name,
                status,
                elapsed_ms,
            } => {
                let (icon, icon_color, state_label) = match status {
                    ToolStatus::Running => (spinner_glyph, Color::Cyan, "running…".to_owned()),
                    ToolStatus::Succeeded => (
                        "✓",
                        Color::Green,
                        elapsed_ms
                            .map(format_duration_ms)
                            .unwrap_or_else(|| "ok".to_owned()),
                    ),
                    ToolStatus::Failed => (
                        "✗",
                        Color::Red,
                        elapsed_ms
                            .map(format_duration_ms)
                            .unwrap_or_else(|| "failed".to_owned()),
                    ),
                };
                lines.push(Line::from(vec![
                    Span::styled("  ╭─ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        icon,
                        Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" ({state_label})"),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(" ─╮", Style::default().fg(Color::DarkGray)),
                ]));
            }
            FeedEntry::Notice { kind, text } => {
                let (prefix, style) = match kind {
                    NoticeKind::Info => ("· ", Style::default().fg(Color::DarkGray)),
                    NoticeKind::Warn => ("⚠ ", Style::default().fg(Color::Yellow)),
                    NoticeKind::Error => ("✗ ", Style::default().fg(Color::Red)),
                };
                for (i, l) in text.lines().enumerate() {
                    lines.push(Line::from(Span::styled(
                        format!("{}{l}", if i == 0 { prefix } else { "  " }),
                        style,
                    )));
                }
            }
        }
    }

    if !app.streaming_assistant.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "Hermes ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(spinner_glyph, Style::default().fg(Color::Cyan)),
        ]));
        lines.extend(render_markdown_lines(&app.streaming_assistant, state.color));
    } else if app.is_busy() && app.active_tools.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(spinner_glyph, Style::default().fg(Color::Cyan)),
            Span::styled(" Thinking…", Style::default().fg(Color::DarkGray)),
        ]));
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    // Estimate wrapped line count so scroll-to-bottom stays accurate on long lines.
    let total_visual_lines: usize = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            w.max(1).div_ceil(inner_width)
        })
        .sum();
    let max_scroll = total_visual_lines.saturating_sub(inner_height);
    let scroll_y = max_scroll.saturating_sub(app.scroll_from_bottom) as u16;

    let title = if app.scroll_from_bottom > 0 {
        " Conversation [scrolled · Ctrl+End to jump ↓] "
    } else {
        " Conversation "
    };

    let feed = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(title, Style::default().fg(Color::Cyan))),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(feed, area);
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = if let Some(started) = app.turn_started {
        let glyph = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
        let secs = started.elapsed().as_secs_f64();
        Span::styled(
            format!(" {glyph} Running ({secs:.1}s) · Ctrl-C to stop "),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled(
            " › Message (/ for commands) ",
            Style::default().fg(Color::Cyan),
        )
    };

    let border_color = if app.is_busy() {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let display_text = if app.input.is_empty() && !app.is_busy() {
        Line::from(Span::styled(
            "Type a request or /help…",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(app.input.clone())
    };

    let composer = if app.input.contains('\n') {
        Paragraph::new(app.input.as_str())
    } else {
        Paragraph::new(display_text)
    }
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title),
    )
    .wrap(Wrap { trim: false });
    frame.render_widget(composer, area);

    // Position visible terminal cursor inside the composer when idle and no modal is active.
    if !app.is_busy() && app.modal.is_none() {
        let before = &app.input[..app.cursor];
        let row = before.matches('\n').count() as u16;
        let col = before
            .rsplit('\n')
            .next()
            .map(|s| s.chars().count())
            .unwrap_or(0) as u16;
        let cx = area.x + 1 + col.min(area.width.saturating_sub(3));
        let cy = area.y + 1 + row.min(area.height.saturating_sub(3));
        frame.set_cursor_position((cx, cy));
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let text = if app.modal.is_some() {
        " Modal active · follow on-screen keys · Ctrl-C cancel turn"
    } else if app.is_busy() {
        " Ctrl-C stop turn · PgUp/PgDn scroll conversation"
    } else {
        " Enter send · Alt+Enter newline · Tab complete / · PgUp/PgDn scroll · /ui inline · Ctrl-D quit"
    };
    let footer = Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, area);
}

fn render_palette(
    frame: &mut Frame<'_>,
    feed_area: Rect,
    palette: &[&'static crate::ui::SlashCommand],
    selected: usize,
) {
    let visible = palette.len().min(8) as u16;
    let height = visible + 2;
    let width = feed_area.width.min(62);
    let rect = Rect {
        x: feed_area.x + 1,
        y: feed_area.y + feed_area.height.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let lines: Vec<Line<'static>> = palette
        .iter()
        .take(visible as usize)
        .enumerate()
        .map(|(i, cmd)| {
            let is_sel = i == selected.min(palette.len() - 1);
            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(
                format!(" {:<18} {}", cmd.usage, cmd.description),
                style,
            ))
        })
        .collect();
    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Commands (Tab to complete) ",
                Style::default().fg(Color::Cyan),
            )),
    );
    frame.render_widget(popup, rect);
}

fn render_modal(frame: &mut Frame<'_>, area: Rect, modal: &ActiveModal) {
    let rect = centered_rect(82, 72, area);
    frame.render_widget(Clear, rect);

    match modal {
        ActiveModal::Approval {
            request,
            reason,
            scroll,
            ..
        } => {
            let action = match request.action {
                ApprovalAction::CreateFile => "Create file",
                ApprovalAction::ReplaceFile => "Replace file",
                ApprovalAction::RunCommand => "Run command",
                ApprovalAction::RunCode => "Run Python code",
                ApprovalAction::ManageSkills => "Change skills",
                ApprovalAction::Clarify => "Clarify",
            };
            let is_command = matches!(request.action, ApprovalAction::RunCommand);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Action: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        action,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        approval::sanitize_terminal_text(&request.target),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
            ];
            if let Some(why) = reason {
                lines.push(Line::from(vec![
                    Span::styled("Flagged: ", Style::default().fg(Color::Red)),
                    Span::styled(
                        approval::sanitize_terminal_text(why),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }
            lines.push(Line::from(""));
            let preview = approval::sanitize_terminal_text(&request.content);
            for l in preview.lines().take(200) {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::White),
                )));
            }
            lines.push(Line::from(""));
            let keys_hint = if is_command {
                "[y] Approve once   [s] Session   [a] Always (allowlist)   [n / Esc] Deny"
            } else {
                "[y] Approve once   [a] Allow in session   [n / Esc] Deny"
            };
            lines.push(Line::from(Span::styled(
                keys_hint,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));

            let widget = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(Span::styled(
                            " Approval Required (↑/↓ scroll) ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )),
                )
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0));
            frame.render_widget(widget, rect);
        }
        ActiveModal::Clarify {
            questions,
            current,
            cursor,
            selected,
            custom,
            ..
        } => {
            let q = &questions[*current];
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("Question {} of {}", current + 1, questions.len()),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    q.prompt.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];
            for (idx, choice) in q.choices.iter().enumerate() {
                let is_cur = idx == *cursor;
                let mark = if q.multi_select {
                    if selected.contains(&idx) {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                } else if is_cur {
                    "▸"
                } else {
                    " "
                };
                let style = if is_cur {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(
                    format!(" {mark} {}) {choice}", idx + 1),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Custom answer: ", Style::default().fg(Color::DarkGray)),
                Span::styled(custom.clone(), Style::default().fg(Color::Yellow)),
            ]));
            lines.push(Line::from(""));
            let hint = if q.multi_select {
                "↑/↓ move · Space toggle · type custom · Enter submit · Esc cancel"
            } else {
                "↑/↓ choose · type custom · Enter submit · Esc cancel"
            };
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::Cyan),
            )));

            let widget = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(Span::styled(
                            " Clarification ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(widget, rect);
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn sample_state() -> ReplState {
        let req = chat::prepare(
            None,
            chat::ChatArgs::for_test("http://127.0.0.1:9/v1", "gpt-5.4", "k"),
        )
        .unwrap();
        let mut st = ReplState::new(None, req, true);
        st.session = Some("sess-abcdef123456".into());
        st.record_usage(1200, 350);
        st.feed_entries
            .push(FeedEntry::User("Check Cargo.toml".into()));
        st.feed_entries.push(FeedEntry::Tool {
            name: "read_file".into(),
            status: ToolStatus::Succeeded,
            elapsed_ms: Some(18),
        });
        st.feed_entries.push(FeedEntry::Assistant(
            "## Result\nFound **ratatui** in `Cargo.toml`.".into(),
        ));
        st
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_header_feed_tool_cards_and_composer_in_test_backend() {
        let state = sample_state();
        let mut app = TuiApp::new();
        app.input = "/co".into();
        app.cursor = 3;

        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("HERMES"), "header missing HERMES:\n{text}");
        assert!(text.contains("gpt-5.4"), "header missing model:\n{text}");
        assert!(
            text.contains("1.2k in / 350 out"),
            "header missing usage:\n{text}"
        );
        assert!(
            text.contains("read_file (18ms)"),
            "missing tool card:\n{text}"
        );
        assert!(text.contains("Result"), "missing markdown heading:\n{text}");
        assert!(
            text.contains("Commands (Tab to complete)"),
            "missing slash palette:\n{text}"
        );
        assert!(
            text.contains("/compress"),
            "missing /compress in palette:\n{text}"
        );
    }

    #[test]
    fn renders_approval_and_clarify_modals_and_handles_keys() {
        let mut state = sample_state();
        let mut app = TuiApp::new();

        // 1. Approval modal
        let (tx, mut rx) = oneshot::channel();
        let req = ApprovalRequest {
            action: ApprovalAction::RunCommand,
            target: "/tmp/work".into(),
            content: "rm -rf build".into(),
            timeout_secs: 60,
        };
        open_modal(&mut app, req, tx);
        assert!(app.modal.is_some());

        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("Approval Required"),
            "missing approval modal:\n{text}"
        );
        assert!(
            text.contains("rm -rf build"),
            "missing command preview:\n{text}"
        );

        handle_modal_key(
            &mut app,
            &mut state,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert!(app.modal.is_none());
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Approved)));

        // 2. Clarify modal
        let (tx2, mut rx2) = oneshot::channel();
        let clarify_req = ApprovalRequest {
            action: ApprovalAction::Clarify,
            target: "".into(),
            content: json!([{
                "qid": "q1",
                "question": "Which UI mode?",
                "choices": ["Inline", "TUI"],
                "multi_select": false
            }])
            .to_string(),
            timeout_secs: 60,
        };
        open_modal(&mut app, clarify_req, tx2);
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let text2 = buffer_text(&terminal);
        assert!(
            text2.contains("Clarification"),
            "missing clarify modal:\n{text2}"
        );
        assert!(
            text2.contains("Which UI mode?"),
            "missing question:\n{text2}"
        );

        // Press Down -> selects "TUI", then Enter -> submits
        handle_modal_key(
            &mut app,
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        handle_modal_key(
            &mut app,
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(app.modal.is_none());
        match rx2.try_recv() {
            Ok(ApprovalDecision::Answered(json_str)) => {
                let parsed: Value = serde_json::from_str(&json_str).unwrap();
                assert_eq!(parsed["answers"]["q1"], "TUI");
                assert_eq!(parsed["outcome"], "submitted");
            }
            _ => panic!("expected ApprovalDecision::Answered"),
        }
    }
}
