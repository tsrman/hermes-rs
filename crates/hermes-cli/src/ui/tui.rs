//! Fullscreen Rust TUI built on `ratatui` + `crossterm`.
//!
//! Layout:
//! - Top status bar: Hermes title, profile, provider:model, session ID, YOLO badge, token/context meter
//! - Center scrollable conversation feed: Markdown-styled messages, framed tool cards, notices
//! - Floating `/` slash-command completion palette above the input box
//! - Multiline input composer with cursor, history, Alt+Enter/Shift+Enter newlines, live spinner
//! - Centered interactive modals for `approval` and `clarify`

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use hermes_runtime::storage::{SessionStore, SessionSummary};
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
use crate::ui::clipboard::{self, ClipboardRead};
use crate::ui::markdown::render_markdown_lines;
use crate::ui::{
    active_at_token, build_context_bar, format_context_length, format_duration_ms,
    format_shell_escape, format_skills_list, format_tokens_compact, load_resumed_session,
    match_slash_commands, parse_stderr_event, record_prompt_history, short_model_label, ActiveUi,
    FeedEntry, NoticeKind, ParsedStderr, ReplState, ToolStatus, UiOutcome, HELP_TEXT,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// RAII guard that guarantees terminal raw mode, Kitty keyboard disambiguation (for Shift+Enter /
/// Ctrl+Enter), bracketed paste, mouse capture, and alternate screen are restored on drop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        // Enable Kitty keyboard protocol (CSI u) so Shift+Enter and Ctrl+Enter arrive distinctly
        // from plain Enter on terminals that support it (Kitty, Ghostty, WezTerm, Alacritty,
        // iTerm2, Konsole, Foot, Windows Terminal, VS Code). Unsupported terminals ignore it.
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
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
    SessionPicker {
        sessions: Vec<SessionSummary>,
        cursor: usize,
        scroll: usize,
    },
}

struct PaletteEntry {
    replacement: String,
    usage: String,
    description: String,
    at_start: Option<usize>,
}

pub struct TuiApp {
    input: String,
    /// Byte index of cursor in `input` (always on a UTF-8 char boundary).
    cursor: usize,
    home: Option<PathBuf>,
    root: PathBuf,
    history_pos: Option<usize>,
    draft_before_history: String,
    palette_index: usize,
    scroll_from_bottom: usize,
    /// Last rendered maximum scroll offset (updated in `render_feed`) so scrolling up clamps
    /// cleanly at the top and never builds up dead scroll debt when scrolling back down.
    last_max_scroll: Cell<usize>,
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
            home: None,
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            history_pos: None,
            draft_before_history: String::new(),
            palette_index: 0,
            scroll_from_bottom: 0,
            last_max_scroll: Cell::new(0),
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

    fn scroll_feed_up(&mut self, lines: usize) {
        let max_scroll = self.last_max_scroll.get();
        self.scroll_from_bottom = self
            .scroll_from_bottom
            .saturating_add(lines)
            .min(max_scroll);
    }

    fn scroll_feed_down(&mut self, lines: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(lines);
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.history_pos = None;
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
        self.history_pos = None;
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
        self.history_pos = None;
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
        self.history_pos = None;
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

    fn move_line_up(&mut self) {
        let before = &self.input[..self.cursor];
        let Some(cur_line_nl) = before.rfind('\n') else {
            return;
        };
        let col_chars = before[cur_line_nl + 1..].chars().count();
        let prev_line_start = before[..cur_line_nl].rfind('\n').map_or(0, |i| i + 1);
        let prev_line = &self.input[prev_line_start..cur_line_nl];
        let offset = prev_line
            .char_indices()
            .nth(col_chars)
            .map_or(prev_line.len(), |(i, _)| i);
        self.cursor = prev_line_start + offset;
    }

    fn move_line_down(&mut self) {
        let before = &self.input[..self.cursor];
        let cur_line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let col_chars = before[cur_line_start..].chars().count();
        let after = &self.input[self.cursor..];
        let Some(rel_nl) = after.find('\n') else {
            return;
        };
        let next_line_start = self.cursor + rel_nl + 1;
        let next_line_end = self.input[next_line_start..]
            .find('\n')
            .map_or(self.input.len(), |i| next_line_start + i);
        let next_line = &self.input[next_line_start..next_line_end];
        let offset = next_line
            .char_indices()
            .nth(col_chars)
            .map_or(next_line.len(), |(i, _)| i);
        self.cursor = next_line_start + offset;
    }

    fn insert_pasted(&mut self, raw: &str) {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.history_pos = None;
        self.palette_index = 0;
    }

    fn completion_palette(&self) -> Option<(&'static str, Vec<PaletteEntry>)> {
        if self.is_busy() || self.modal.is_some() || self.history_pos.is_some() {
            return None;
        }
        if let Some((at_start, token)) = active_at_token(&self.input, self.cursor) {
            let items: Vec<PaletteEntry> =
                hermes_runtime::context_refs::complete_at_references(&self.root, token, 8)
                    .into_iter()
                    .map(|c| PaletteEntry {
                        replacement: c.replacement,
                        usage: c.display,
                        description: c.detail,
                        at_start: Some(at_start),
                    })
                    .collect();
            if !items.is_empty() {
                return Some((" Context (@file / @folder / @diff) — Tab to insert ", items));
            }
        }
        if self.input.contains('\n') {
            return None;
        }
        if self.input.starts_with('/') && !self.input.contains(char::is_whitespace) {
            let mut items: Vec<PaletteEntry> = match_slash_commands(&self.input)
                .into_iter()
                .map(|c| PaletteEntry {
                    replacement: c.name.to_owned(),
                    usage: c.usage.to_owned(),
                    description: c.description.to_owned(),
                    at_start: None,
                })
                .collect();
            let needle = self.input[1..].to_ascii_lowercase();
            for skill in hermes_runtime::context_refs::list_skill_commands(self.home.as_deref()) {
                if needle.is_empty() || skill.name.starts_with(&needle) {
                    let slash_name = format!("/{}", skill.name);
                    if !items.iter().any(|p| p.replacement == slash_name) {
                        items.push(PaletteEntry {
                            replacement: slash_name.clone(),
                            usage: slash_name,
                            description: format!("[skill] {}", skill.description),
                            at_start: None,
                        });
                    }
                }
            }
            if !items.is_empty() {
                return Some((" Commands (Tab to complete) ", items));
            }
        }
        None
    }

    #[cfg(test)]
    fn slash_palette(&self) -> Vec<&'static crate::ui::SlashCommand> {
        if self.is_busy()
            || self.modal.is_some()
            || self.history_pos.is_some()
            || self.input.contains('\n')
        {
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
    app.home = home.map(Path::to_path_buf);
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
                    Some(TermEvent::Mouse(mouse)) if handle_mouse_event(&mut app, mouse) => {
                        dirty = true;
                    }
                    Some(TermEvent::Paste(pasted)) if !app.is_busy() && app.modal.is_none() => {
                        app.insert_pasted(&pasted);
                        dirty = true;
                    }
                    Some(TermEvent::Paste(pasted)) => {
                        if let Some(ActiveModal::Clarify { custom, .. }) = &mut app.modal {
                            let inline = pasted
                                .replace("\r\n", " ")
                                .replace(['\r', '\n'], " ");
                            custom.push_str(&inline);
                            dirty = true;
                        }
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
            ParsedStderr::ContextUsage { tokens, estimated } => {
                state.record_context(tokens, estimated);
            }
            ParsedStderr::ToolStarted { name, detail } => {
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
                    detail,
                    status: ToolStatus::Running,
                    summary: None,
                    elapsed_ms: None,
                });
            }
            ParsedStderr::ToolFinished {
                name,
                succeeded,
                summary,
            } => {
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
                    status,
                    summary: slot_summary,
                    elapsed_ms,
                    ..
                }) = updated
                {
                    *status = new_status;
                    *slot_summary = summary;
                    *elapsed_ms = elapsed;
                } else {
                    state.feed_entries.push(FeedEntry::Tool {
                        name,
                        detail: None,
                        status: new_status,
                        summary,
                        elapsed_ms: elapsed,
                    });
                }
            }
            ParsedStderr::Fallback(model) => {
                TuiApp::push_notice(state, NoticeKind::Warn, format!("Model fallback → {model}"));
            }
            ParsedStderr::ContextCompressed(msg) => {
                state.compressions = state.compressions.saturating_add(1);
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
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }

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
        // When idle, Ctrl-C closes a session picker modal or clears current composer text.
        if matches!(app.modal, Some(ActiveModal::SessionPicker { .. })) {
            app.modal = None;
            return None;
        }
        if !app.input.is_empty() {
            app.input.clear();
            app.cursor = 0;
            app.history_pos = None;
        }
        return None;
    }

    if app.modal.is_some() {
        handle_modal_key(app, state, home, key);
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
            app.scroll_feed_up(10);
            return None;
        }
        (KeyCode::PageDown, _) => {
            app.scroll_feed_down(10);
            return None;
        }
        (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) => {
            app.scroll_feed_up(3);
            return None;
        }
        (KeyCode::Down, m) if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) => {
            app.scroll_feed_down(3);
            return None;
        }
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.scroll_feed_up(12);
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

    // Slash & `@` context palette navigation
    if let Some((_title, palette)) = app.completion_palette() {
        match key.code {
            KeyCode::Tab => {
                let idx = app.palette_index.min(palette.len() - 1);
                let item = &palette[idx];
                if let Some(at_start) = item.at_start {
                    let suffix_space = !item.replacement.ends_with(':');
                    let mut replacement = item.replacement.clone();
                    if suffix_space {
                        replacement.push(' ');
                    }
                    app.input.replace_range(at_start..app.cursor, &replacement);
                    app.cursor = at_start + replacement.len();
                } else {
                    app.input = item.replacement.clone();
                    if matches!(
                        app.input.as_str(),
                        "/resume" | "/model" | "/ui" | "/compress" | "/skills"
                    ) {
                        app.input.push(' ');
                    }
                    app.cursor = app.input.len();
                }
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
            KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let idx = app.palette_index.min(palette.len() - 1);
                let item = &palette[idx];
                if let Some(at_start) = item.at_start {
                    let suffix_space = !item.replacement.ends_with(':');
                    let mut replacement = item.replacement.clone();
                    if suffix_space {
                        replacement.push(' ');
                    }
                    app.input.replace_range(at_start..app.cursor, &replacement);
                    app.cursor = at_start + replacement.len();
                    app.palette_index = 0;
                    return None;
                }
                let chosen = item.replacement.clone();
                app.input.clear();
                app.cursor = 0;
                app.history_pos = None;
                app.draft_before_history.clear();
                app.palette_index = 0;
                if !matches!(chosen.as_str(), "/exit" | "/quit") {
                    record_prompt_history(home, &mut state.prompt_history, &chosen);
                }
                if let Some(cmd) = chosen.strip_prefix('/') {
                    return handle_slash_command(app, state, home, cmd).await;
                }
                return None;
            }
            _ => {}
        }
    }

    // Composer editing & submission
    match (key.code, key.modifiers) {
        (KeyCode::Char('p'), m) if m.contains(KeyModifiers::CONTROL) && app.input.is_empty() => {
            app.input = "/".into();
            app.cursor = 1;
            app.palette_index = 0;
        }
        (KeyCode::Enter, m)
            if m.contains(KeyModifiers::ALT)
                || m.contains(KeyModifiers::SHIFT)
                || m.contains(KeyModifiers::CONTROL) =>
        {
            app.insert_char('\n');
        }
        (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => {
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
            app.draft_before_history.clear();
            if text.is_empty() {
                return None;
            }
            let is_exit = matches!(text.as_str(), "/exit" | "/quit");
            if !is_exit {
                record_prompt_history(home, &mut state.prompt_history, &text);
            }
            if let Some(shell_cmd) = text.strip_prefix('!').filter(|_| !text.contains('\n')) {
                let (is_err, output) = format_shell_escape(&app.root, shell_cmd);
                TuiApp::push_notice(
                    state,
                    if is_err {
                        NoticeKind::Error
                    } else {
                        NoticeKind::Info
                    },
                    format!("$ {shell_cmd}\n{output}"),
                );
                app.scroll_from_bottom = 0;
                return None;
            }
            if let Some(cmd) = text.strip_prefix('/').filter(|_| !text.contains('\n')) {
                return handle_slash_command(app, state, home, cmd).await;
            }
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
        _ if is_paste_key(key) => match clipboard::read_text() {
            ClipboardRead::Text(text) => app.insert_pasted(&text),
            ClipboardRead::Empty => {}
            ClipboardRead::Unavailable(msg) => {
                TuiApp::push_notice(state, NoticeKind::Warn, msg);
            }
        },
        (KeyCode::Up, _) if app.history_pos.is_none() && app.input[..app.cursor].contains('\n') => {
            app.move_line_up();
        }
        (KeyCode::Up, _) if !state.prompt_history.is_empty() => {
            let next_idx = match app.history_pos {
                None => {
                    app.draft_before_history = app.input.clone();
                    state.prompt_history.len() - 1
                }
                Some(i) => i.saturating_sub(1),
            };
            app.history_pos = Some(next_idx);
            app.input = state.prompt_history[next_idx].clone();
            app.cursor = app.input.len();
        }
        (KeyCode::Down, _) if app.history_pos.is_some() => {
            if let Some(i) = app.history_pos {
                if i + 1 < state.prompt_history.len() {
                    app.history_pos = Some(i + 1);
                    app.input = state.prompt_history[i + 1].clone();
                } else {
                    app.history_pos = None;
                    app.input = std::mem::take(&mut app.draft_before_history);
                }
                app.cursor = app.input.len();
            }
        }
        (KeyCode::Down, _) if app.input[app.cursor..].contains('\n') => {
            app.move_line_down();
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

/// True for key combinations that explicitly request clipboard paste when the
/// terminal emulator forwards the keypress directly instead of bracketed paste.
/// Covers `Ctrl+V`, `Ctrl+Shift+V`, Russian-layout `Ctrl+М`, macOS `Cmd+V`, and `Shift+Insert`.
fn is_paste_key(key: KeyEvent) -> bool {
    let m = key.modifiers;
    if m.contains(KeyModifiers::ALT) {
        return false;
    }
    match key.code {
        KeyCode::Char('v' | 'V' | 'м' | 'М') => {
            m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER)
        }
        KeyCode::Insert => m.contains(KeyModifiers::SHIFT),
        _ => false,
    }
}

/// Handle mouse wheel scrolling in the conversation feed and scrollable modals.
/// Returns `true` when state changed and the frame should be redrawn.
fn handle_mouse_event(app: &mut TuiApp, mouse: MouseEvent) -> bool {
    const WHEEL_LINES: usize = 3;
    match mouse.kind {
        MouseEventKind::ScrollUp => match &mut app.modal {
            Some(ActiveModal::Approval { scroll, .. }) => {
                let before = *scroll;
                *scroll = scroll.saturating_sub(WHEEL_LINES as u16);
                *scroll != before
            }
            Some(ActiveModal::SessionPicker { cursor, scroll, .. }) => {
                let before = *cursor;
                *cursor = cursor.saturating_sub(1);
                if *cursor < *scroll {
                    *scroll = *cursor;
                }
                *cursor != before
            }
            Some(ActiveModal::Clarify {
                questions,
                current,
                cursor,
                ..
            }) => {
                if questions
                    .get(*current)
                    .is_some_and(|q| !q.choices.is_empty())
                {
                    let before = *cursor;
                    *cursor = cursor.saturating_sub(1);
                    *cursor != before
                } else {
                    false
                }
            }
            None => {
                let before = app.scroll_from_bottom;
                app.scroll_feed_up(WHEEL_LINES);
                app.scroll_from_bottom != before
            }
        },
        MouseEventKind::ScrollDown => match &mut app.modal {
            Some(ActiveModal::Approval { scroll, .. }) => {
                let before = *scroll;
                *scroll = scroll.saturating_add(WHEEL_LINES as u16);
                *scroll != before
            }
            Some(ActiveModal::SessionPicker {
                sessions, cursor, ..
            }) => {
                let before = *cursor;
                if *cursor + 1 < sessions.len() {
                    *cursor += 1;
                }
                *cursor != before
            }
            Some(ActiveModal::Clarify {
                questions,
                current,
                cursor,
                ..
            }) => {
                if let Some(q) = questions.get(*current) {
                    let before = *cursor;
                    if *cursor + 1 < q.choices.len() {
                        *cursor += 1;
                    }
                    *cursor != before
                } else {
                    false
                }
            }
            None => {
                let before = app.scroll_from_bottom;
                app.scroll_feed_down(WHEEL_LINES);
                app.scroll_from_bottom != before
            }
        },
        _ => false,
    }
}

fn handle_modal_key(app: &mut TuiApp, state: &mut ReplState, home: Option<&Path>, key: KeyEvent) {
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
                _ if is_paste_key(key) => {
                    if let ClipboardRead::Text(text) = clipboard::read_text() {
                        let inline = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
                        custom.push_str(&inline);
                    }
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
        ActiveModal::SessionPicker {
            sessions,
            cursor,
            scroll,
        } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Up | KeyCode::Char('k') => {
                *cursor = cursor.saturating_sub(1);
                if *cursor < *scroll {
                    *scroll = *cursor;
                }
                app.modal = Some(modal);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *cursor + 1 < sessions.len() {
                    *cursor += 1;
                }
                app.modal = Some(modal);
            }
            KeyCode::PageUp => {
                *cursor = cursor.saturating_sub(8);
                if *cursor < *scroll {
                    *scroll = *cursor;
                }
                app.modal = Some(modal);
            }
            KeyCode::PageDown => {
                *cursor = (*cursor + 8).min(sessions.len().saturating_sub(1));
                app.modal = Some(modal);
            }
            KeyCode::Home => {
                *cursor = 0;
                *scroll = 0;
                app.modal = Some(modal);
            }
            KeyCode::End => {
                *cursor = sessions.len().saturating_sub(1);
                app.modal = Some(modal);
            }
            KeyCode::Enter => {
                if let Some(chosen) = sessions.get(*cursor) {
                    let id = chosen.id.clone();
                    resume_tui_target(app, state, home, &id);
                }
            }
            _ => {
                app.modal = Some(modal);
            }
        },
    }
}

fn open_session_picker(app: &mut TuiApp, state: &mut ReplState, home: Option<&Path>) {
    let Some(home) = home else {
        TuiApp::push_notice(
            state,
            NoticeKind::Warn,
            "No saved sessions yet: profile root is unavailable.",
        );
        return;
    };
    match SessionStore::open_read_only(home).and_then(|s| s.recent(20, None)) {
        Ok(sessions) if !sessions.is_empty() => {
            let cursor = state
                .session
                .as_deref()
                .and_then(|cur| sessions.iter().position(|s| s.id == cur))
                .unwrap_or(0);
            app.modal = Some(ActiveModal::SessionPicker {
                sessions,
                cursor,
                scroll: 0,
            });
        }
        Ok(_) => TuiApp::push_notice(state, NoticeKind::Info, "No saved sessions yet."),
        Err(_) => TuiApp::push_notice(state, NoticeKind::Info, "No saved sessions yet."),
    }
}

fn resume_tui_target(app: &mut TuiApp, state: &mut ReplState, home: Option<&Path>, target: &str) {
    let mut resolved_query = target.to_owned();
    if target.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(index) = target.parse::<usize>() {
            if let Some(home) = home {
                match SessionStore::open_read_only(home).and_then(|s| s.recent(20, None)) {
                    Ok(sessions) if index >= 1 && index <= sessions.len() => {
                        resolved_query = sessions[index - 1].id.clone();
                    }
                    Ok(sessions) => {
                        TuiApp::push_notice(
                            state,
                            NoticeKind::Warn,
                            format!(
                                "Session number {index} is out of range (1..={}).",
                                sessions.len()
                            ),
                        );
                        return;
                    }
                    Err(err) => {
                        TuiApp::push_notice(
                            state,
                            NoticeKind::Error,
                            format!("Could not read sessions: {err}"),
                        );
                        return;
                    }
                }
            }
        }
    }
    match load_resumed_session(home, state, &resolved_query) {
        Ok(_) => {
            app.scroll_from_bottom = 0;
        }
        Err(err) => {
            TuiApp::push_notice(
                state,
                NoticeKind::Error,
                format!("Could not resume session '{target}': {err}"),
            );
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
        "palette" => {
            app.input = "/".into();
            app.cursor = 1;
            app.palette_index = 0;
        }
        "clear" => {
            state.feed_entries.clear();
            app.scroll_from_bottom = 0;
        }
        "undo" => {
            let Some(home) = home else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    "Cannot undo: profile root is unavailable.",
                );
                return None;
            };
            let Some(id) = state.session.clone() else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    "Nothing to undo: no active saved session yet.",
                );
                return None;
            };
            match hermes_runtime::storage::undo_last_turn(home, &id) {
                Ok(0) => TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    format!("Nothing to undo in session {id}."),
                ),
                Ok(removed) => {
                    let _ = load_resumed_session(Some(home), state, &id);
                    app.scroll_from_bottom = 0;
                    TuiApp::push_notice(
                        state,
                        NoticeKind::Info,
                        format!(
                            "Undid last exchange in session {id} ({removed} message(s) removed)."
                        ),
                    );
                }
                Err(err) => {
                    TuiApp::push_notice(state, NoticeKind::Error, format!("Undo failed: {err}"))
                }
            }
        }
        "skills" => {
            TuiApp::push_notice(state, NoticeKind::Info, format_skills_list(home, argument));
        }
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
        "new" | "reset" => {
            state.session = None;
            state.unsaved = None;
            state.reset_metrics();
            state.feed_entries.clear();
            state.guard.lock().unwrap().reset();
            hermes_runtime::terminal::forget_session(&state.scope);
            if !state.request.has_explicit_model_override() {
                state.request.set_primary_model(state.default_model.clone());
            }
            app.scroll_from_bottom = 0;
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
        "sessions"
            if argument.is_empty()
                || matches!(
                    argument.to_ascii_lowercase().as_str(),
                    "list" | "ls" | "browse"
                ) =>
        {
            open_session_picker(app, state, home);
        }
        "sessions" => {
            resume_tui_target(app, state, home, argument);
        }
        "resume" if argument.is_empty() => {
            open_session_picker(app, state, home);
        }
        "resume" => {
            resume_tui_target(app, state, home, argument);
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
            let Some(id) = state.session.clone() else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    "Nothing to compress yet: start a conversation first.",
                );
                return None;
            };
            let focus = (!argument.is_empty()).then_some(argument);
            match chat::compress_session(home, &state.request, &id, focus, &state.guard).await {
                Ok(ManualCompress::Applied { archived }) => {
                    state.compressions = state.compressions.saturating_add(1);
                    if let Ok(transcript) = SessionStore::open_read_only(home)
                        .and_then(|s| s.resume_display_transcript(&id))
                    {
                        let est = hermes_runtime::model_metadata::estimate_active_messages_tokens(
                            &transcript.messages,
                        );
                        state.record_context(est, true);
                    }
                    TuiApp::push_notice(
                        state,
                        NoticeKind::Info,
                        format!(
                            "Compressed session {id}: archived {archived} message(s) into a summary."
                        ),
                    );
                }
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
        "browser" => {
            let (is_err, msg) = crate::browser::handle_slash_browser(home, state, argument).await;
            TuiApp::push_notice(
                state,
                if is_err {
                    NoticeKind::Error
                } else {
                    NoticeKind::Info
                },
                msg,
            );
        }
        "retry" => match state.unsaved.take() {
            Some(previous) => return Some(previous),
            None => TuiApp::push_notice(
                state,
                NoticeKind::Info,
                "Nothing to retry: the last turn finished and is saved.",
            ),
        },
        other => {
            let full = format!("/{command}");
            if let Some(inv) = hermes_runtime::context_refs::resolve_skill_invocation(home, &full) {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    format!("Running skill /{}...", inv.name),
                );
                return Some(inv.prompt);
            }
            TuiApp::push_notice(
                state,
                NoticeKind::Warn,
                format!("Unknown command /{other}; /help lists the commands."),
            );
        }
    }
    None
}

// ── Pure Ratatui Frame Renderer (directly unit-testable with TestBackend) ───────────

struct ComposerLayout {
    lines: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Deterministically wrap composer input on explicit newlines and `inner_width` columns so
/// box height, rendered rows, vertical scrolling, and visible cursor coordinates always agree.
fn wrap_composer_lines(input: &str, cursor: usize, inner_width: usize) -> ComposerLayout {
    let width = inner_width.max(1);
    let clamped_cursor = cursor.min(input.len());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current = String::new();
    let mut col: usize = 0;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = 0;
    let mut cursor_found = false;

    for (byte_idx, ch) in input.char_indices() {
        if !cursor_found && byte_idx >= clamped_cursor {
            cursor_row = lines.len();
            cursor_col = col;
            cursor_found = true;
        }
        if ch == '\n' {
            lines.push(Line::from(std::mem::take(&mut current)));
            col = 0;
            continue;
        }
        if col >= width {
            lines.push(Line::from(std::mem::take(&mut current)));
            col = 0;
            if byte_idx == clamped_cursor {
                cursor_row = lines.len();
                cursor_col = 0;
            }
        }
        current.push(ch);
        col += 1;
    }

    if !cursor_found {
        if col >= width {
            lines.push(Line::from(std::mem::take(&mut current)));
            col = 0;
        }
        cursor_row = lines.len();
        cursor_col = col;
    }
    lines.push(Line::from(current));

    ComposerLayout {
        lines,
        cursor_row,
        cursor_col,
    }
}

pub fn render_frame(frame: &mut Frame<'_>, app: &TuiApp, state: &ReplState) {
    let area = frame.area();
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let max_composer_lines = (area.height / 3).clamp(3, 8);
    let layout = wrap_composer_lines(&app.input, app.cursor, inner_width);
    let composer_lines = (layout.lines.len().max(1) as u16).clamp(1, max_composer_lines);
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
    render_composer(frame, chunks[2], app, layout);
    render_footer(frame, chunks[3], app);

    // Floating slash-command / `@` context palette above the composer
    if let Some((title, palette)) = app.completion_palette() {
        render_palette(frame, chunks[1], title, &palette, app.palette_index);
    }

    // Centered modal for Approval, Clarify, or SessionPicker
    if let Some(modal) = &app.modal {
        render_modal(frame, area, modal, state.session.as_deref());
    }
}

fn context_usage_style(percent: u8) -> Style {
    if percent >= 95 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if percent > 80 {
        Style::default().fg(Color::LightRed)
    } else if percent >= 50 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &ReplState) {
    let model_raw = state.request.model().unwrap_or("(unset)");
    let model = short_model_label(model_raw);
    let sess = state
        .session
        .as_deref()
        .map(|s| s.chars().take(10).collect::<String>())
        .unwrap_or_else(|| "new".to_owned());
    let yolo = hermes_runtime::approval_policy::installed().is_some_and(|p| p.approves_all());

    let (ctx_len, _is_fallback) = state.request.display_context_length();
    let ctx_used = state.context_tokens.min(ctx_len);
    let pct = ((ctx_used as f64 / ctx_len.max(1) as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;
    let mark = if state.context_estimated && ctx_used > 0 {
        "~"
    } else {
        ""
    };
    let ctx_style = context_usage_style(pct);
    let pct_label = format!("{mark}{pct}%");
    let wide = area.width >= 76;
    let narrow = area.width < 52;
    let sep = if wide { " │ " } else { " · " };

    let mut spans = vec![
        Span::styled(
            " HERMES ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("☤ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            model,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if wide {
        let ctx_detail = format!(
            "{mark}{}/{}",
            format_tokens_compact(ctx_used),
            format_context_length(ctx_len)
        );
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(ctx_detail, Style::default().fg(Color::Gray)));
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(build_context_bar(pct, 10), ctx_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pct_label, ctx_style));
    } else if !narrow {
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(build_context_bar(pct, 10), ctx_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pct_label, ctx_style));
    } else {
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(pct_label, ctx_style));
    }

    if state.compressions > 0 && !narrow {
        let comp_style = if state.compressions >= 10 {
            Style::default().fg(Color::LightRed)
        } else if state.compressions >= 5 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("🗜 {}", state.compressions),
            comp_style,
        ));
    }

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

    if wide {
        spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            &state.profile_label,
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("sess:{sess}"),
            Style::default().fg(Color::DarkGray),
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
                    Span::styled("│ ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        "You",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                for l in text.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(Color::Cyan)),
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
                detail,
                status,
                summary,
                elapsed_ms,
            } => {
                let (icon, icon_color) = match status {
                    ToolStatus::Running => (spinner_glyph, Color::Cyan),
                    ToolStatus::Succeeded => ("✓", Color::Green),
                    ToolStatus::Failed => ("✗", Color::Red),
                };
                let clean_detail = detail
                    .as_deref()
                    .map(approval::sanitize_terminal_text)
                    .filter(|s| !s.is_empty());
                let clean_summary = summary
                    .as_deref()
                    .map(approval::sanitize_terminal_text)
                    .filter(|s| !s.is_empty());
                // Split `exit <code> · <err>` so the header stays compact and any error message
                // sits cleanly on a secondary indented line underneath the card.
                let (inline_status, sub_note) = match (status, clean_summary) {
                    (ToolStatus::Running, _) => (Some("running…".to_owned()), None),
                    (ToolStatus::Failed, Some(s)) => {
                        if let Some((head, tail)) = s.split_once(" · ") {
                            (Some(head.to_owned()), Some(tail.to_owned()))
                        } else if s.starts_with("exit ") {
                            (Some(s), None)
                        } else {
                            (Some("failed".to_owned()), Some(s))
                        }
                    }
                    (ToolStatus::Failed, None) => (Some("failed".to_owned()), None),
                    (ToolStatus::Succeeded, Some(s)) => (Some(s), None),
                    (ToolStatus::Succeeded, None) => (None, None),
                };
                let dur = elapsed_ms.map(format_duration_ms);
                let meta = match (inline_status, dur) {
                    (Some(st), Some(d)) => format!("{st} · {d}"),
                    (Some(st), None) => st,
                    (None, Some(d)) => d,
                    (None, None) => "ok".to_owned(),
                };
                let mut card_spans = vec![
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
                ];
                if let Some(d) = clean_detail {
                    card_spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                    card_spans.push(Span::styled(d, Style::default().fg(Color::Gray)));
                }
                card_spans.push(Span::styled(
                    format!(" ({meta})"),
                    Style::default().fg(if matches!(status, ToolStatus::Failed) {
                        Color::LightRed
                    } else {
                        Color::DarkGray
                    }),
                ));
                card_spans.push(Span::styled(" ─╮", Style::default().fg(Color::DarkGray)));
                lines.push(Line::from(card_spans));
                if let Some(note) = sub_note {
                    lines.push(Line::from(vec![
                        Span::styled("     ↳ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(note, Style::default().fg(Color::Red)),
                    ]));
                }
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
    app.last_max_scroll.set(max_scroll);
    let clamped_from_bottom = app.scroll_from_bottom.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(clamped_from_bottom) as u16;

    let title = if clamped_from_bottom > 0 {
        " Conversation [scrolled · Wheel/PgDn or Ctrl+End ↓] "
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

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, layout: ComposerLayout) {
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

    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let total_rows = layout.lines.len().max(1);
    let scroll_y = if layout.cursor_row >= visible_rows {
        (layout.cursor_row + 1 - visible_rows).min(total_rows.saturating_sub(visible_rows))
    } else {
        0
    };

    let composer = if app.input.is_empty() && !app.is_busy() {
        Paragraph::new(Line::from(Span::styled(
            "Type a request or /help…",
            Style::default().fg(Color::DarkGray),
        )))
    } else {
        Paragraph::new(layout.lines).scroll((scroll_y as u16, 0))
    }
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title),
    );
    frame.render_widget(composer, area);

    // Position visible terminal cursor inside the composer when idle and no modal is active.
    if !app.is_busy() && app.modal.is_none() {
        let max_col = area.width.saturating_sub(3);
        let max_row = area.height.saturating_sub(3);
        let cx = area.x + 1 + (layout.cursor_col as u16).min(max_col);
        let cy = area.y + 1 + (layout.cursor_row.saturating_sub(scroll_y) as u16).min(max_row);
        frame.set_cursor_position((cx, cy));
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let text = match &app.modal {
        Some(ActiveModal::SessionPicker { .. }) => {
            " ↑/↓ select session · Enter resume · Esc cancel"
        }
        Some(_) => " Modal active · follow on-screen keys · Ctrl-C cancel turn",
        None if app.is_busy() => " Ctrl-C stop turn · PgUp/PgDn scroll conversation",
        None => {
            " Enter send · Shift+Enter newline · ↑/↓ history · Tab complete / · Wheel/PgUp scroll · Ctrl-D quit"
        }
    };
    let footer = Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray)));
    frame.render_widget(footer, area);
}

fn render_palette(
    frame: &mut Frame<'_>,
    feed_area: Rect,
    title: &str,
    palette: &[PaletteEntry],
    selected: usize,
) {
    let visible = palette.len().min(8) as u16;
    let height = visible + 2;
    let width = feed_area.width.min(68);
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
                format!(" {:<22} {}", cmd.usage, cmd.description),
                style,
            ))
        })
        .collect();
    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                title.to_owned(),
                Style::default().fg(Color::Cyan),
            )),
    );
    frame.render_widget(popup, rect);
}

fn render_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    modal: &ActiveModal,
    active_session: Option<&str>,
) {
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
        ActiveModal::SessionPicker {
            sessions,
            cursor,
            scroll,
        } => {
            let visible_rows = rect.height.saturating_sub(5).max(1) as usize;
            let eff_scroll = if *cursor < *scroll {
                *cursor
            } else if *cursor >= *scroll + visible_rows {
                *cursor + 1 - visible_rows
            } else {
                *scroll
            };
            let max_preview = rect.width.saturating_sub(36) as usize;
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "Select a session to resume ({} of {}):",
                    cursor.saturating_add(1).min(sessions.len()),
                    sessions.len()
                ),
                Style::default().fg(Color::DarkGray),
            ))];
            for (idx, sess) in sessions
                .iter()
                .enumerate()
                .skip(eff_scroll)
                .take(visible_rows)
            {
                let is_cur = idx == *cursor;
                let mark = if is_cur { "▸" } else { " " };
                let active_tag = if active_session == Some(sess.id.as_str()) {
                    "*"
                } else {
                    " "
                };
                let short_id: String = sess.id.chars().take(16).collect();
                let raw_preview = sess
                    .title
                    .as_deref()
                    .or(sess.preview.as_deref())
                    .unwrap_or("");
                let clean = approval::sanitize_terminal_text(raw_preview).replace('\n', " ");
                let preview: String = if max_preview > 3 && clean.chars().count() > max_preview {
                    let mut p: String = clean.chars().take(max_preview - 3).collect();
                    p.push_str("...");
                    p
                } else {
                    clean
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
                    format!(
                        " {mark}{active_tag}{:>2}. {:<16} {:>4}m  {}",
                        idx + 1,
                        short_id,
                        sess.message_count,
                        preview
                    ),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑/↓ or j/k select · Enter resume · Esc/q cancel",
                Style::default().fg(Color::Cyan),
            )));

            let widget = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(Span::styled(
                        " Sessions ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
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
            detail: Some("Cargo.toml".into()),
            status: ToolStatus::Succeeded,
            summary: Some("22 lines".into()),
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
            text.contains("1.2K/1.1M") && text.contains("[░░░░░░░░░░] 0%"),
            "header missing upstream-style context meter:\n{text}"
        );
        assert!(
            text.contains("read_file · Cargo.toml (22 lines · 18ms)"),
            "missing tool card with detail/status:\n{text}"
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
    fn status_bar_renders_upstream_context_bar_tiers_thresholds_and_estimates() {
        let mut state = sample_state();
        let app = TuiApp::new();

        // 50% of gpt-5.4 (1.05M) -> 525K/1.1M, [█████░░░░░] 50%, Yellow
        state.record_context(525_000, false);
        state.compressions = 2;
        let mut wide = Terminal::new(TestBackend::new(100, 20)).unwrap();
        wide.draw(|f| render_frame(f, &app, &state)).unwrap();
        let wide_text = buffer_text(&wide);
        assert!(
            wide_text.contains("525K/1.1M")
                && wide_text.contains("[█████░░░░░] 50%")
                && wide_text.contains("🗜 2"),
            "wide header mismatch:\n{wide_text}"
        );
        assert_eq!(context_usage_style(49).fg, Some(Color::Green));
        assert_eq!(context_usage_style(50).fg, Some(Color::Yellow));
        assert_eq!(context_usage_style(80).fg, Some(Color::Yellow));
        assert_eq!(context_usage_style(81).fg, Some(Color::LightRed));
        assert_eq!(context_usage_style(95).fg, Some(Color::Red));

        // Estimated context (~) shown with tilde prefix; medium width omits token ratio but keeps bar.
        state.record_context(840_000, true);
        let mut medium = Terminal::new(TestBackend::new(64, 20)).unwrap();
        medium.draw(|f| render_frame(f, &app, &state)).unwrap();
        let medium_text = buffer_text(&medium);
        assert!(
            medium_text.contains("[████████░░] ~80%"),
            "medium header mismatch:\n{medium_text}"
        );

        // Narrow width keeps compact percentage.
        let mut narrow = Terminal::new(TestBackend::new(44, 20)).unwrap();
        narrow.draw(|f| render_frame(f, &app, &state)).unwrap();
        let narrow_text = buffer_text(&narrow);
        assert!(
            narrow_text.contains("~80%"),
            "narrow header mismatch:\n{narrow_text}"
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
            None,
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
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        handle_modal_key(
            &mut app,
            &mut state,
            None,
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

    struct TempDirGuard(std::path::PathBuf);
    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let unique = format!(
                "hsr-tui-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn up_arrow_recalls_persisted_and_submitted_history_without_palette_hijack() {
        let dir = TempDirGuard::new("history");
        std::fs::write(
            dir.path().join(".hsr_history"),
            "#V2\nfirst saved prompt\n/model\nsecond saved prompt\n",
        )
        .unwrap();

        let req = chat::prepare(
            Some(dir.path()),
            chat::ChatArgs::for_test("http://127.0.0.1:9/v1", "gpt-5.4", "k"),
        )
        .unwrap();
        let mut state = ReplState::new(Some(dir.path()), req, true);
        let mut app = TuiApp::new();
        let mut handle = None;
        let mut rx = None;

        // Type draft, then walk up through persisted history (including a /command entry).
        app.input = "draft text".into();
        app.cursor = app.input.len();

        handle_key_event(
            &mut app,
            &mut state,
            Some(dir.path()),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "second saved prompt");

        // Second Up recalls `/model`; slash palette must stay hidden while browsing history.
        handle_key_event(
            &mut app,
            &mut state,
            Some(dir.path()),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "/model");
        assert!(app.slash_palette().is_empty());

        // Third Up must still continue walking history instead of moving slash palette!
        handle_key_event(
            &mut app,
            &mut state,
            Some(dir.path()),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "first saved prompt");

        // Key release events must be ignored.
        let release = KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        handle_key_event(
            &mut app,
            &mut state,
            Some(dir.path()),
            release,
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.input, "first saved prompt");

        // Walking back down restores the original draft.
        for _ in 0..3 {
            handle_key_event(
                &mut app,
                &mut state,
                Some(dir.path()),
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &mut handle,
                &mut rx,
            )
            .await;
        }
        assert_eq!(app.input, "draft text");
        assert!(app.history_pos.is_none());

        // Submitting a new prompt appends it to state.prompt_history and .hsr_history.
        let submitted = handle_key_event(
            &mut app,
            &mut state,
            Some(dir.path()),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(submitted.as_deref(), Some("draft text"));
        assert_eq!(
            state.prompt_history.last().map(String::as_str),
            Some("draft text")
        );
        let saved = std::fs::read_to_string(dir.path().join(".hsr_history")).unwrap();
        assert!(saved.contains("draft text"));
    }

    #[tokio::test]
    async fn sessions_modal_renders_and_navigates_with_keys() {
        let mut state = sample_state();
        let mut app = TuiApp::new();
        app.modal = Some(ActiveModal::SessionPicker {
            sessions: vec![
                SessionSummary {
                    id: "sess-newer-2222".into(),
                    source: "cli".into(),
                    model: Some("gpt-5.4".into()),
                    title: Some("Deploy checklist".into()),
                    started_at: 200.0,
                    last_activity_at: Some(210.0),
                    message_count: 6,
                    preview: Some("second session hello".into()),
                },
                SessionSummary {
                    id: "sess-older-1111".into(),
                    source: "cli".into(),
                    model: Some("gpt-5.4".into()),
                    title: None,
                    started_at: 100.0,
                    last_activity_at: Some(110.0),
                    message_count: 2,
                    preview: Some("first session hello".into()),
                },
            ],
            cursor: 0,
            scroll: 0,
        });

        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Sessions"), "missing modal title:\n{text}");
        assert!(
            text.contains("Deploy checklist") && text.contains("first session hello"),
            "missing session entries:\n{text}"
        );

        let mut handle = None;
        let mut rx = None;
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        match &app.modal {
            Some(ActiveModal::SessionPicker { cursor, .. }) => assert_eq!(*cursor, 1),
            _ => panic!("expected SessionPicker modal to remain open"),
        }

        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert!(app.modal.is_none());
    }

    #[test]
    fn mouse_wheel_scrolls_conversation_feed_with_clamping_and_modals() {
        let mut state = sample_state();
        for i in 0..30 {
            state
                .feed_entries
                .push(FeedEntry::User(format!("Message line {i}")));
        }
        let mut app = TuiApp::new();
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let max_scroll = app.last_max_scroll.get();
        assert!(
            max_scroll > 10,
            "expected scrollable feed, got {max_scroll}"
        );

        let wheel = |kind| MouseEvent {
            kind,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        // Scroll up moves away from bottom by 3 lines.
        assert!(handle_mouse_event(
            &mut app,
            wheel(MouseEventKind::ScrollUp)
        ));
        assert_eq!(app.scroll_from_bottom, 3);

        // Scrolling past top clamps cleanly at `max_scroll` (no overscroll debt).
        for _ in 0..100 {
            handle_mouse_event(&mut app, wheel(MouseEventKind::ScrollUp));
        }
        assert_eq!(app.scroll_from_bottom, max_scroll);
        assert!(!handle_mouse_event(
            &mut app,
            wheel(MouseEventKind::ScrollUp)
        ));

        // One notch down immediately moves back down by 3 lines.
        assert!(handle_mouse_event(
            &mut app,
            wheel(MouseEventKind::ScrollDown)
        ));
        assert_eq!(app.scroll_from_bottom, max_scroll.saturating_sub(3));

        // Mouse wheel also navigates SessionPicker modal.
        app.modal = Some(ActiveModal::SessionPicker {
            sessions: vec![
                SessionSummary {
                    id: "s1".into(),
                    source: "cli".into(),
                    model: None,
                    title: Some("One".into()),
                    started_at: 2.0,
                    last_activity_at: Some(2.0),
                    message_count: 1,
                    preview: None,
                },
                SessionSummary {
                    id: "s2".into(),
                    source: "cli".into(),
                    model: None,
                    title: Some("Two".into()),
                    started_at: 1.0,
                    last_activity_at: Some(1.0),
                    message_count: 1,
                    preview: None,
                },
            ],
            cursor: 0,
            scroll: 0,
        });
        assert!(handle_mouse_event(
            &mut app,
            wheel(MouseEventKind::ScrollDown)
        ));
        assert!(matches!(
            app.modal,
            Some(ActiveModal::SessionPicker { cursor: 1, .. })
        ));
        assert!(handle_mouse_event(
            &mut app,
            wheel(MouseEventKind::ScrollUp)
        ));
        assert!(matches!(
            app.modal,
            Some(ActiveModal::SessionPicker { cursor: 0, .. })
        ));
    }

    #[tokio::test]
    async fn multiline_composer_renders_wrapped_lines_pasted_newlines_and_scrolls_to_cursor() {
        let mut state = sample_state();
        let mut app = TuiApp::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();

        // 1. Long single line without `\n` wraps across multiple visible composer lines.
        let long_line = "AlphaBetaGammaDeltaEpsilonZetaEtaThetaIotaKappaLambdaMuNuXi";
        app.insert_pasted(long_line);
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let rendered = buffer_text(&terminal);
        assert!(
            rendered.contains("AlphaBeta") && rendered.contains("LambdaMuNuXi"),
            "wrapped multiline input was clipped:\n{rendered}"
        );

        // 2. Pasting CRLF / CR text normalizes to `\n` and renders every line visibly.
        app.input.clear();
        app.cursor = 0;
        app.insert_pasted("FirstLine\r\nSecondLine\rThirdLine");
        assert_eq!(app.input, "FirstLine\nSecondLine\nThirdLine");
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let rendered_paste = buffer_text(&terminal);
        assert!(
            rendered_paste.contains("FirstLine")
                && rendered_paste.contains("SecondLine")
                && rendered_paste.contains("ThirdLine"),
            "pasted multiline lines missing:\n{rendered_paste}"
        );

        // 3. Trailing newline (e.g. after Alt+Enter / Ctrl+J) expands box immediately and moves cursor to row + 1.
        app.input = "LineOne\n".into();
        app.cursor = app.input.len();
        let layout = wrap_composer_lines(&app.input, app.cursor, 38);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));

        // 4. Input taller than max composer rows scrolls down so the active last line stays visible.
        app.input = (1..=12)
            .map(|i| format!("RowNumber{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.cursor = app.input.len();
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let rendered_tall = buffer_text(&terminal);
        assert!(
            rendered_tall.contains("RowNumber12"),
            "composer failed to scroll to keep last line visible:\n{rendered_tall}"
        );

        // 5. Up/Down inside multiline input moves between lines before touching history.
        let mut handle = None;
        let mut rx = None;
        app.input = "LineA\nLineB".into();
        app.cursor = app.input.len();
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.cursor, 5); // end of "LineA"
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert_eq!(app.cursor, app.input.len());
    }

    #[tokio::test]
    async fn slash_palette_enter_executes_highlighted_command_and_tool_cards_show_status() {
        let mut state = sample_state();
        let mut app = TuiApp::new();
        let mut handle = None;
        let mut rx = None;

        // Typing `/se` matches `/sessions` uniquely; pressing Enter executes `/sessions`
        // instead of failing with "Unknown command /se".
        app.input = "/se".into();
        app.cursor = 3;
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert!(app.input.is_empty());
        assert!(
            !state.feed_entries.iter().any(
                |e| matches!(e, FeedEntry::Notice { text, .. } if text.contains("Unknown command"))
            ),
            "palette Enter should execute highlighted /sessions, not report Unknown command"
        );
        assert!(
            state
                .feed_entries
                .iter()
                .any(|e| matches!(e, FeedEntry::Notice { text, .. } if text.contains("No saved sessions"))),
            "expected /sessions output in feed"
        );

        // Typing `/` + Down selects `/new`, and Enter executes `/new` (clearing session).
        app.input = "/".into();
        app.cursor = 1;
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        handle_key_event(
            &mut app,
            &mut state,
            None,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut handle,
            &mut rx,
        )
        .await;
        assert!(state.session.is_none());

        // Tool cards show command/code detail, exit code, duration, and error line on failure.
        state.feed_entries.push(FeedEntry::Tool {
            name: "terminal".into(),
            detail: Some("cargo test -p hermes-cli".into()),
            status: ToolStatus::Failed,
            summary: Some("exit 101 · 1 failed".into()),
            elapsed_ms: Some(12_900),
        });
        let mut terminal = Terminal::new(TestBackend::new(96, 20)).unwrap();
        terminal.draw(|f| render_frame(f, &app, &state)).unwrap();
        let rendered = buffer_text(&terminal);
        assert!(
            rendered.contains("terminal · cargo test -p hermes-cli (exit 101 · 12.9s)"),
            "missing failed tool header with command & exit code:\n{rendered}"
        );
        assert!(
            rendered.contains("↳ 1 failed"),
            "missing indented failure detail line:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn ctrl_v_russian_layout_and_shift_insert_paste_from_clipboard() {
        let mut state = sample_state();
        let mut app = TuiApp::new();
        let mut handle = None;
        let mut rx = None;

        // Ctrl+V inserts multiline clipboard text normalized to `\n`.
        {
            let _clip =
                clipboard::override_for_test(ClipboardRead::Text("line1\r\nline2".to_owned()));
            handle_key_event(
                &mut app,
                &mut state,
                None,
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &mut handle,
                &mut rx,
            )
            .await;
            assert_eq!(app.input, "line1\nline2");
            assert_eq!(app.cursor, app.input.len());
        }

        // Russian keyboard layout Ctrl+М also pastes.
        {
            let _clip = clipboard::override_for_test(ClipboardRead::Text(" привет".to_owned()));
            handle_key_event(
                &mut app,
                &mut state,
                None,
                KeyEvent::new(KeyCode::Char('м'), KeyModifiers::CONTROL),
                &mut handle,
                &mut rx,
            )
            .await;
            assert_eq!(app.input, "line1\nline2 привет");
        }

        // Shift+Insert pastes as well.
        {
            let _clip = clipboard::override_for_test(ClipboardRead::Text("!".to_owned()));
            handle_key_event(
                &mut app,
                &mut state,
                None,
                KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
                &mut handle,
                &mut rx,
            )
            .await;
            assert_eq!(app.input, "line1\nline2 привет!");
        }

        // Unavailable clipboard reports a warning notice without modifying input.
        {
            let _clip = clipboard::override_for_test(ClipboardRead::Unavailable(
                "Clipboard unavailable".to_owned(),
            ));
            handle_key_event(
                &mut app,
                &mut state,
                None,
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &mut handle,
                &mut rx,
            )
            .await;
            assert_eq!(app.input, "line1\nline2 привет!");
            assert!(state.feed_entries.iter().any(|e| matches!(
                e,
                FeedEntry::Notice {
                    kind: NoticeKind::Warn,
                    text
                } if text.contains("Clipboard unavailable")
            )));
        }

        // Ctrl+V inside Clarify modal appends single-line text into custom input.
        {
            let (tx, _rx_reply) = oneshot::channel();
            app.modal = Some(ActiveModal::Clarify {
                questions: vec![ClarifyQuestion {
                    qid: "q1".into(),
                    prompt: "Details?".into(),
                    choices: vec![],
                    multi_select: false,
                }],
                current: 0,
                cursor: 0,
                selected: BTreeSet::new(),
                custom: String::new(),
                answers: Map::new(),
                reply: Some(tx),
            });
            let _clip = clipboard::override_for_test(ClipboardRead::Text("foo\r\nbar".to_owned()));
            handle_key_event(
                &mut app,
                &mut state,
                None,
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &mut handle,
                &mut rx,
            )
            .await;
            match &app.modal {
                Some(ActiveModal::Clarify { custom, .. }) => assert_eq!(custom, "foo bar"),
                _ => panic!("expected Clarify modal to stay open"),
            }
        }
    }
}
