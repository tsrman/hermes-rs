//! Fullscreen Rust TUI built on `ratatui` + `crossterm`.
//!
//! Layout:
//! - Top status bar: Hermes title, profile, provider:model, session ID, YOLO badge, token/context meter
//! - Center scrollable conversation feed: Markdown-styled messages, framed tool cards, notices
//! - Floating `/` slash-command completion palette above the input box
//! - Multiline input composer with cursor, history, Alt+Enter/Shift+Enter newlines, live spinner
//! - Centered interactive modals for `approval` and `clarify`

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use hermes_runtime::storage::{SessionStore, SessionSummary};
use hermes_runtime::turn::{ApprovalAction, ApprovalDecision, ApprovalRequest};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Terminal;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::approval::{self, Grant};
use crate::chat::{self, Event as ChatEvent, ManualCompress};
use crate::ui::clipboard::{self, ClipboardRead};
use crate::ui::{
    active_at_token, format_shell_escape, format_skills_list, load_resumed_session,
    match_slash_commands, parse_stderr_event, record_prompt_history, ActiveUi, FeedEntry,
    NoticeKind, ParsedStderr, ReplState, ToolStatus, UiOutcome, HELP_TEXT,
};

pub(super) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

mod render;
mod selection;
#[cfg(test)]
mod tests;

pub use render::render_frame;
use selection::{
    copy_active_selection, ctrl_shortcut_letter, handle_mouse_event, is_copy_selection_key,
    is_paste_key, toggle_mouse_capture,
};

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

#[derive(Debug, Eq, PartialEq)]
pub(super) enum TuiAction {
    Turn(String),
    Compress {
        session_id: String,
        focus: Option<String>,
    },
    ShellEscape(String),
    Browser(String),
}

pub(super) enum BackgroundOutcome {
    Compress {
        session_id: String,
        result: Result<ManualCompress, String>,
    },
    ShellEscape {
        command: String,
        is_err: bool,
        output: String,
    },
    Browser {
        is_err: bool,
        message: String,
        cdp_update: Option<Option<String>>,
    },
}

pub(super) enum TurnMsg {
    Event(ChatEvent),
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    Finished(Result<u8, String>),
    BackgroundDone(BackgroundOutcome),
}

pub(super) struct ClarifyQuestion {
    pub(super) qid: String,
    pub(super) prompt: String,
    pub(super) choices: Vec<String>,
    pub(super) multi_select: bool,
}

pub(super) enum ActiveModal {
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

pub(super) struct PaletteEntry {
    pub(super) replacement: String,
    pub(super) usage: String,
    pub(super) description: String,
    pub(super) at_start: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct FeedPos {
    /// 0-based visual line index into `FeedSnapshot::lines`.
    pub(super) line: usize,
    /// 0-based column inside the feed inner rect.
    pub(super) col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FeedSelection {
    pub(super) anchor: FeedPos,
    pub(super) focus: FeedPos,
    pub(super) dragging: bool,
}

impl FeedSelection {
    pub(super) fn ordered(self) -> (FeedPos, FeedPos) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct WrappedFeedLine {
    pub(super) line: Line<'static>,
    pub(super) plain: String,
    /// True when this visual row is a soft word-wrap continuation of the previous row.
    pub(super) soft_wrapped: bool,
}

#[derive(Default)]
pub(super) struct FeedSnapshot {
    pub(super) inner_area: Rect,
    pub(super) scroll_y: usize,
    pub(super) lines: Vec<WrappedFeedLine>,
}

pub(super) const COPY_STATUS_TTL: Duration = Duration::from_secs(4);
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);

pub struct TuiApp {
    pub(super) input: String,
    /// Byte index of cursor in `input` (always on a UTF-8 char boundary).
    pub(super) cursor: usize,
    /// Optional selection anchor byte offset in `input`; when `Some(a)` with `a != cursor`,
    /// `min(a, cursor)..max(a, cursor)` is selected in the composer.
    pub(super) input_anchor: Option<usize>,
    pub(super) home: Option<PathBuf>,
    pub(super) root: PathBuf,
    pub(super) history_pos: Option<usize>,
    pub(super) draft_before_history: String,
    pub(super) palette_index: usize,
    pub(super) scroll_from_bottom: usize,
    /// Last rendered maximum scroll offset (updated in `render_feed`) so scrolling up clamps
    /// cleanly at the top and never builds up dead scroll debt when scrolling back down.
    pub(super) last_max_scroll: Cell<usize>,
    /// Last rendered wrapped feed geometry and text for exact mouse selection & copying.
    pub(super) feed_snapshot: RefCell<FeedSnapshot>,
    pub(super) selection: Option<FeedSelection>,
    pub(super) last_click: Option<(Instant, FeedPos, u8)>,
    /// Transient footer notification `(created_at, is_error, message)` for copy/mouse actions.
    pub(super) copy_status: Option<(Instant, bool, String)>,
    /// Whether mouse capture is currently enabled (toggleable via `Alt+M` or `/mouse`).
    pub(super) mouse_capture: bool,
    pub(super) spinner_tick: usize,
    pub(super) streaming_assistant: String,
    pub(super) streaming_reasoning_lines: Vec<String>,
    pub(super) streaming_reasoning_partial: String,
    pub(super) active_tools: HashMap<String, Instant>,
    pub(super) active_tool_output: HashMap<String, (Vec<String>, String)>,
    pub(super) turn_started: Option<Instant>,
    pub(super) background_status: Option<String>,
    pub(super) turn_saved_session: Option<String>,
    pub(super) turn_usage: Option<(u64, u64)>,
    pub(super) turn_query: Option<String>,
    pub(super) modal: Option<ActiveModal>,
    pub(super) should_exit: Option<UiOutcome>,
}

impl TuiApp {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            input_anchor: None,
            home: None,
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            history_pos: None,
            draft_before_history: String::new(),
            palette_index: 0,
            scroll_from_bottom: 0,
            last_max_scroll: Cell::new(0),
            feed_snapshot: RefCell::new(FeedSnapshot::default()),
            selection: None,
            last_click: None,
            copy_status: None,
            mouse_capture: true,
            spinner_tick: 0,
            streaming_assistant: String::new(),
            streaming_reasoning_lines: Vec::new(),
            streaming_reasoning_partial: String::new(),
            active_tools: HashMap::new(),
            active_tool_output: HashMap::new(),
            turn_started: None,
            background_status: None,
            turn_saved_session: None,
            turn_usage: None,
            turn_query: None,
            modal: None,
            should_exit: None,
        }
    }

    pub(super) fn set_copy_status(&mut self, is_err: bool, message: impl Into<String>) {
        self.copy_status = Some((Instant::now(), is_err, message.into()));
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn is_busy(&self) -> bool {
        self.turn_started.is_some() || self.background_status.is_some()
    }

    pub(super) fn scroll_feed_up(&mut self, lines: usize) {
        let max_scroll = self.last_max_scroll.get();
        self.scroll_from_bottom = self
            .scroll_from_bottom
            .saturating_add(lines)
            .min(max_scroll);
    }

    pub(super) fn scroll_feed_down(&mut self, lines: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(lines);
    }

    pub(super) fn input_selection(&self) -> Option<std::ops::Range<usize>> {
        let a = self.input_anchor?.min(self.input.len());
        let c = self.cursor.min(self.input.len());
        if a == c {
            None
        } else {
            Some(a.min(c)..a.max(c))
        }
    }

    pub(super) fn selected_input_text(&self) -> Option<&str> {
        let range = self.input_selection()?;
        self.input.get(range)
    }

    pub(super) fn clear_input_selection(&mut self) {
        self.input_anchor = None;
    }

    pub(super) fn select_all_input(&mut self) {
        if self.input.is_empty() {
            self.input_anchor = None;
            self.cursor = 0;
        } else {
            self.input_anchor = Some(0);
            self.cursor = self.input.len();
        }
    }

    pub(super) fn delete_input_selection(&mut self) -> bool {
        let Some(range) = self.input_selection() else {
            self.input_anchor = None;
            return false;
        };
        self.input.drain(range.clone());
        self.cursor = range.start;
        self.input_anchor = None;
        self.history_pos = None;
        self.palette_index = 0;
        true
    }

    pub(super) fn set_input_cursor(&mut self, pos: usize, extend: bool) {
        let clamped = pos.min(self.input.len());
        if extend {
            if self.input_anchor.is_none() {
                self.input_anchor = Some(self.cursor);
            }
            self.cursor = clamped;
            if self.input_anchor == Some(self.cursor) {
                self.input_anchor = None;
            }
        } else {
            self.input_anchor = None;
            self.cursor = clamped;
        }
    }

    pub(super) fn insert_char(&mut self, c: char) {
        self.delete_input_selection();
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.history_pos = None;
        self.palette_index = 0;
    }

    pub(super) fn backspace(&mut self) {
        if self.delete_input_selection() {
            return;
        }
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

    pub(super) fn prev_word_boundary(&self, from: usize) -> usize {
        let head = &self.input[..from.min(self.input.len())];
        let trimmed_end = head.trim_end().len();
        head[..trimmed_end]
            .rfind(char::is_whitespace)
            .and_then(|ws_idx| {
                let ws_char = head[ws_idx..].chars().next()?;
                Some(ws_idx + ws_char.len_utf8())
            })
            .unwrap_or(0)
    }

    pub(super) fn next_word_boundary(&self, from: usize) -> usize {
        let start = from.min(self.input.len());
        let tail = &self.input[start..];
        let mut iter = tail.char_indices().peekable();
        // Skip current non-whitespace word, then trailing whitespace (standard editor Ctrl+Right).
        let starts_ws = iter.peek().is_some_and(|(_, c)| c.is_whitespace());
        if starts_ws {
            while iter.peek().is_some_and(|(_, c)| c.is_whitespace()) {
                iter.next();
            }
            while iter.peek().is_some_and(|(_, c)| !c.is_whitespace()) {
                iter.next();
            }
        } else {
            while iter.peek().is_some_and(|(_, c)| !c.is_whitespace()) {
                iter.next();
            }
            while iter.peek().is_some_and(|(_, c)| c.is_whitespace()) {
                iter.next();
            }
        }
        iter.peek().map_or(self.input.len(), |(i, _)| start + *i)
    }

    pub(super) fn delete_word_back(&mut self) {
        if self.delete_input_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let new_pos = self.prev_word_boundary(self.cursor);
        self.input.drain(new_pos..self.cursor);
        self.cursor = new_pos;
        self.history_pos = None;
        self.palette_index = 0;
    }

    pub(super) fn delete_word_forward(&mut self) {
        if self.delete_input_selection() {
            return;
        }
        if self.cursor >= self.input.len() {
            return;
        }
        let end = self.next_word_boundary(self.cursor);
        self.input.drain(self.cursor..end);
        self.history_pos = None;
        self.palette_index = 0;
    }

    pub(super) fn delete_to_line_end(&mut self) {
        if self.delete_input_selection() {
            return;
        }
        if self.cursor >= self.input.len() {
            return;
        }
        let end = self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |rel| {
                if rel == 0 {
                    self.cursor + 1
                } else {
                    self.cursor + rel
                }
            });
        self.input.drain(self.cursor..end);
        self.history_pos = None;
        self.palette_index = 0;
    }

    pub(super) fn delete_char_forward(&mut self) {
        if self.delete_input_selection() {
            return;
        }
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
        self.palette_index = 0;
    }

    pub(super) fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.input_selection() {
                self.cursor = range.start;
                self.input_anchor = None;
                return;
            }
        }
        if self.cursor > 0 {
            let prev = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.set_input_cursor(prev, extend);
        } else if !extend {
            self.input_anchor = None;
        }
    }

    pub(super) fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.input_selection() {
                self.cursor = range.end;
                self.input_anchor = None;
                return;
            }
        }
        if self.cursor < self.input.len() {
            if let Some(c) = self.input[self.cursor..].chars().next() {
                self.set_input_cursor(self.cursor + c.len_utf8(), extend);
            }
        } else if !extend {
            self.input_anchor = None;
        }
    }

    pub(super) fn move_word_left(&mut self, extend: bool) {
        let next = self.prev_word_boundary(self.cursor);
        self.set_input_cursor(next, extend);
    }

    pub(super) fn move_word_right(&mut self, extend: bool) {
        let next = self.next_word_boundary(self.cursor);
        self.set_input_cursor(next, extend);
    }

    pub(super) fn move_home(&mut self, extend: bool) {
        self.set_input_cursor(0, extend);
    }

    pub(super) fn move_end(&mut self, extend: bool) {
        let len = self.input.len();
        self.set_input_cursor(len, extend);
    }

    pub(super) fn move_line_up(&mut self) {
        self.clear_input_selection();
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

    pub(super) fn move_line_down(&mut self) {
        self.clear_input_selection();
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

    pub(super) fn insert_pasted(&mut self, raw: &str) {
        self.delete_input_selection();
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.history_pos = None;
        self.palette_index = 0;
    }

    pub(super) fn completion_palette(&self) -> Option<(&'static str, Vec<PaletteEntry>)> {
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
            for skill in hermes_runtime::context_refs::cached_skill_commands_nonblocking(
                self.home.as_deref(),
            ) {
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
    pub(super) fn slash_palette(&self) -> Vec<&'static crate::ui::SlashCommand> {
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

    pub(super) fn push_notice(state: &mut ReplState, kind: NoticeKind, text: impl Into<String>) {
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
    hermes_runtime::context_refs::prewarm_skill_commands(home);
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

    let mut applied_mouse_capture = app.mouse_capture;
    let mut dirty = true;
    let outcome = loop {
        if app.mouse_capture != applied_mouse_capture {
            let mut stdout = io::stdout();
            if app.mouse_capture {
                let _ = execute!(stdout, EnableMouseCapture);
            } else {
                let _ = execute!(stdout, DisableMouseCapture);
            }
            applied_mouse_capture = app.mouse_capture;
        }
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
                    Some(TurnMsg::BackgroundDone(outcome)) => {
                        active_handle = None;
                        turn_rx = None;
                        finish_background(&mut app, state, home, outcome);
                    }
                    None => {
                        active_handle = None;
                        turn_rx = None;
                        app.background_status = None;
                    }
                }
            }
            maybe_ev = ev_rx.recv() => {
                match maybe_ev {
                    Some(TermEvent::Key(key)) => {
                        dirty = true;
                        if let Some(action) = handle_key_event(
                            &mut app,
                            state,
                            home,
                            key,
                            &mut active_handle,
                            &mut turn_rx,
                        ).await {
                            match action {
                                TuiAction::Turn(query) => {
                                    start_turn(
                                        home,
                                        &mut app,
                                        state,
                                        query,
                                        &mut active_handle,
                                        &mut turn_rx,
                                    );
                                }
                                other => {
                                    start_background_action(
                                        home,
                                        &mut app,
                                        state,
                                        other,
                                        &mut active_handle,
                                        &mut turn_rx,
                                    );
                                }
                            }
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
                        app.clear_selection();
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

pub(super) fn open_modal(
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

pub(super) fn parse_clarify_questions(content: &str) -> Vec<ClarifyQuestion> {
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

pub(super) fn handle_turn_event(app: &mut TuiApp, state: &mut ReplState, ev: ChatEvent) {
    match ev {
        ChatEvent::Stdout(chunk) => {
            if !chunk.trim().is_empty() {
                app.streaming_reasoning_lines.clear();
                app.streaming_reasoning_partial.clear();
            }
            if app.streaming_assistant.is_empty() {
                let trimmed = chunk.trim_start_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    return;
                }
                app.streaming_assistant.push_str(trimmed);
            } else {
                app.streaming_assistant.push_str(&chunk);
            }
        }
        ChatEvent::Stderr(text) => match parse_stderr_event(&text) {
            ParsedStderr::Session(id) => app.turn_saved_session = Some(id),
            ParsedStderr::Usage { input, output } => app.turn_usage = Some((input, output)),
            ParsedStderr::ContextUsage { tokens, estimated } => {
                state.record_context(tokens, estimated);
            }
            ParsedStderr::Reasoning(chunk) => {
                crate::ui::append_preview_lines(
                    &mut app.streaming_reasoning_lines,
                    &mut app.streaming_reasoning_partial,
                    &chunk,
                    3,
                );
            }
            ParsedStderr::ToolStarted { name, detail } => {
                app.streaming_reasoning_lines.clear();
                app.streaming_reasoning_partial.clear();
                let flushed = std::mem::take(&mut app.streaming_assistant);
                let cleaned = flushed.trim_start_matches(['\r', '\n']).trim_end();
                if !cleaned.is_empty() {
                    state
                        .feed_entries
                        .push(FeedEntry::Assistant(cleaned.to_owned()));
                }
                app.active_tools.insert(name.clone(), Instant::now());
                app.active_tool_output.remove(&name);
                state.feed_entries.push(FeedEntry::Tool {
                    name,
                    detail,
                    status: ToolStatus::Running,
                    summary: None,
                    elapsed_ms: None,
                });
            }
            ParsedStderr::ToolOutput { name, text } => {
                let entry = app
                    .active_tool_output
                    .entry(name)
                    .or_insert_with(|| (Vec::new(), String::new()));
                crate::ui::append_preview_lines(&mut entry.0, &mut entry.1, &text, 3);
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
                app.active_tool_output.remove(&name);
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

pub(super) fn finish_turn(app: &mut TuiApp, state: &mut ReplState, result: Result<u8, String>) {
    let elapsed = app
        .turn_started
        .take()
        .map(|s| s.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    app.active_tools.clear();
    app.active_tool_output.clear();
    app.streaming_reasoning_lines.clear();
    app.streaming_reasoning_partial.clear();
    let final_text = std::mem::take(&mut app.streaming_assistant);
    let cleaned = final_text.trim_start_matches(['\r', '\n']).trim_end();
    if !cleaned.is_empty() {
        state
            .feed_entries
            .push(FeedEntry::Assistant(cleaned.to_owned()));
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
            if app.turn_saved_session.is_none() && status != 0 {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    "This turn ended before completion and was not saved to session history.",
                );
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
    app.streaming_reasoning_lines.clear();
    app.streaming_reasoning_partial.clear();
    app.active_tools.clear();
    app.active_tool_output.clear();
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
    let guard = Arc::clone(&state.guard);
    let mcp_pool = Arc::clone(&state.mcp_pool);
    let (tx, rx) = mpsc::unbounded_channel::<TurnMsg>();
    *turn_rx = Some(rx);

    let handle = tokio::spawn(async move {
        let tx_events = tx.clone();
        let tx_approve = tx.clone();
        let res = chat::execute_with_context(
            home_owned.as_deref(),
            turn_request,
            Some(&guard),
            Some(&mcp_pool),
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

fn start_background_action(
    home: Option<&Path>,
    app: &mut TuiApp,
    state: &ReplState,
    action: TuiAction,
    active_handle: &mut Option<JoinHandle<()>>,
    turn_rx: &mut Option<mpsc::UnboundedReceiver<TurnMsg>>,
) {
    let (tx, rx) = mpsc::unbounded_channel::<TurnMsg>();
    *turn_rx = Some(rx);
    app.scroll_from_bottom = 0;

    match action {
        TuiAction::Compress { session_id, focus } => {
            app.background_status = Some("Compressing session…".into());
            let home_owned = home.map(Path::to_path_buf);
            let request = state.request.clone();
            let guard = Arc::clone(&state.guard);
            let handle = tokio::spawn(async move {
                let result = match home_owned.as_deref() {
                    Some(home) => {
                        chat::compress_session(
                            home,
                            &request,
                            &session_id,
                            focus.as_deref(),
                            &guard,
                        )
                        .await
                    }
                    None => Err("Cannot compress: profile root is unavailable.".into()),
                };
                let _ = tx.send(TurnMsg::BackgroundDone(BackgroundOutcome::Compress {
                    session_id,
                    result,
                }));
            });
            *active_handle = Some(handle);
        }
        TuiAction::ShellEscape(command) => {
            app.background_status = Some(format!("Running !{command}…"));
            let root = app.root.clone();
            let handle = tokio::spawn(async move {
                let cmd_clone = command.clone();
                let (is_err, output) =
                    tokio::task::spawn_blocking(move || format_shell_escape(&root, &cmd_clone))
                        .await
                        .unwrap_or_else(|e| (true, format!("Command task failed: {e}")));
                let _ = tx.send(TurnMsg::BackgroundDone(BackgroundOutcome::ShellEscape {
                    command,
                    is_err,
                    output,
                }));
            });
            *active_handle = Some(handle);
        }
        TuiAction::Browser(argument) => {
            app.background_status = Some("Running /browser…".into());
            let home_owned = home.map(Path::to_path_buf);
            let scope = state.scope.clone();
            let configured = state.request.browser_cdp_url().map(str::to_owned);
            let handle = tokio::spawn(async move {
                let (is_err, message, cdp_update) = crate::browser::execute_slash_browser(
                    home_owned.as_deref(),
                    &scope,
                    configured.as_deref(),
                    &argument,
                )
                .await;
                let _ = tx.send(TurnMsg::BackgroundDone(BackgroundOutcome::Browser {
                    is_err,
                    message,
                    cdp_update,
                }));
            });
            *active_handle = Some(handle);
        }
        TuiAction::Turn(_) => {}
    }
}

fn finish_background(
    app: &mut TuiApp,
    state: &mut ReplState,
    home: Option<&Path>,
    outcome: BackgroundOutcome,
) {
    app.background_status = None;
    app.scroll_from_bottom = 0;
    match outcome {
        BackgroundOutcome::Compress { session_id, result } => match result {
            Ok(ManualCompress::Applied { archived }) => {
                state.compressions = state.compressions.saturating_add(1);
                if let Some(home) = home {
                    if let Ok(transcript) = SessionStore::open_read_only(home)
                        .and_then(|s| s.resume_display_transcript(&session_id))
                    {
                        let est = hermes_runtime::model_metadata::estimate_active_messages_tokens(
                            &transcript.messages,
                        );
                        state.record_context(est, true);
                    }
                }
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    format!(
                        "Compressed session {session_id}: archived {archived} message(s) into a summary."
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
        },
        BackgroundOutcome::ShellEscape {
            command,
            is_err,
            output,
        } => {
            TuiApp::push_notice(
                state,
                if is_err {
                    NoticeKind::Error
                } else {
                    NoticeKind::Info
                },
                format!("$ {command}\n{output}"),
            );
        }
        BackgroundOutcome::Browser {
            is_err,
            message,
            cdp_update,
        } => {
            if let Some(new_url) = cdp_update {
                state.request.set_browser_cdp_url(new_url);
            }
            TuiApp::push_notice(
                state,
                if is_err {
                    NoticeKind::Error
                } else {
                    NoticeKind::Info
                },
                message,
            );
        }
    }
}

/// Handle a keyboard event. Returns `Some(TuiAction)` when the user submits a turn or background command.
async fn handle_key_event(
    app: &mut TuiApp,
    state: &mut ReplState,
    home: Option<&Path>,
    key: KeyEvent,
    active_handle: &mut Option<JoinHandle<()>>,
    turn_rx: &mut Option<mpsc::UnboundedReceiver<TurnMsg>>,
) -> Option<TuiAction> {
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }

    // 0. If feed text is selected, Ctrl+C / Ctrl+Shift+C / Cmd+C / Ctrl+Insert copies it
    // instead of cancelling the active turn or wiping the composer, and Esc clears it.
    if app.modal.is_none() && app.selection.is_some() {
        if is_copy_selection_key(key) {
            copy_active_selection(app, true);
            return None;
        }
        if key.code == KeyCode::Esc {
            app.clear_selection();
            return None;
        }
    }

    let ctrl_letter = ctrl_shortcut_letter(key);

    // 0b. If composer text is selected (e.g. after Ctrl+A or Shift+Arrows), Ctrl+C / Ctrl+Insert
    // copies it to clipboard (with OSC 52 fallback over SSH/PuTTY) instead of wiping the input.
    if app.modal.is_none() && !app.is_busy() && app.input_selection().is_some() {
        if is_copy_selection_key(key) {
            if let Some(selected) = app.selected_input_text().map(str::to_owned) {
                match clipboard::write_text(&selected) {
                    Ok(()) => app.set_copy_status(
                        false,
                        format!("Copied {} char(s) from input", selected.chars().count()),
                    ),
                    Err(msg) => app.set_copy_status(true, msg),
                }
            }
            return None;
        }
        if key.code == KeyCode::Esc {
            app.clear_input_selection();
            return None;
        }
    }

    // Toggle mouse capture (Alt+M) for users who prefer native terminal mouse selection.
    if app.modal.is_none()
        && key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('m' | 'M' | 'ь' | 'Ь'))
    {
        toggle_mouse_capture(app);
        return None;
    }

    // 1. Active modal consumes keys first (except Ctrl-C which cancels the turn).
    if ctrl_letter == Some('c') {
        if let Some(handle) = active_handle.take() {
            handle.abort();
            *turn_rx = None;
            app.modal = None;
            if let Some(bg_label) = app.background_status.take() {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    format!("Cancelled ({})", bg_label.trim_end_matches('…')),
                );
                return None;
            }
            app.turn_started = None;
            app.active_tools.clear();
            app.active_tool_output.clear();
            app.streaming_reasoning_lines.clear();
            app.streaming_reasoning_partial.clear();
            let partial = std::mem::take(&mut app.streaming_assistant);
            let cleaned = partial.trim_start_matches(['\r', '\n']).trim_end();
            if !cleaned.is_empty() {
                state
                    .feed_entries
                    .push(FeedEntry::Assistant(cleaned.to_owned()));
            }
            let saved_id = app.turn_saved_session.take();
            if let Some(id) = saved_id.clone() {
                state.session = Some(id);
                state.unsaved = None;
            } else if let Some(q) = app.turn_query.take() {
                state.unsaved = Some(q);
            }
            app.turn_query = None;
            hermes_runtime::mcp::flush_cancellations().await;
            if saved_id.is_some() {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    "Turn was already saved to session history; cancelled post-turn cleanup.",
                );
            } else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    "interrupted (the turn is not saved; /retry sends it again)",
                );
            }
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
            app.clear_input_selection();
            app.history_pos = None;
        }
        return None;
    }

    if app.modal.is_some() {
        handle_modal_key(app, state, home, key);
        return None;
    }

    // Ctrl-D exits when composer is empty and idle.
    if ctrl_letter == Some('d') {
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
        _ if ctrl_letter == Some('u') => {
            app.scroll_feed_up(12);
            return None;
        }
        (KeyCode::End, m)
            if m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::SHIFT) =>
        {
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
                    app.clear_input_selection();
                } else {
                    app.input = item.replacement.clone();
                    if matches!(
                        app.input.as_str(),
                        "/resume" | "/model" | "/ui" | "/compress" | "/skills"
                    ) {
                        app.input.push(' ');
                    }
                    app.cursor = app.input.len();
                    app.clear_input_selection();
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
                    app.clear_input_selection();
                    app.palette_index = 0;
                    return None;
                }
                let chosen = item.replacement.clone();
                app.input.clear();
                app.cursor = 0;
                app.clear_input_selection();
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
        _ if ctrl_letter == Some('p') && app.input.is_empty() => {
            app.input = "/".into();
            app.cursor = 1;
            app.clear_input_selection();
            app.palette_index = 0;
        }
        (KeyCode::Enter, m)
            if m.contains(KeyModifiers::ALT)
                || m.contains(KeyModifiers::SHIFT)
                || m.contains(KeyModifiers::CONTROL) =>
        {
            app.insert_char('\n');
        }
        _ if ctrl_letter == Some('j') => {
            app.insert_char('\n');
        }
        (KeyCode::Enter, _) => {
            // Support trailing `\` continuation just like inline REPL.
            if app.input_selection().is_none()
                && app.cursor == app.input.len()
                && app.input.ends_with('\\')
            {
                app.backspace();
                app.insert_char('\n');
                return None;
            }
            let text = app.input.trim().to_owned();
            app.input.clear();
            app.cursor = 0;
            app.clear_input_selection();
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
                return Some(TuiAction::ShellEscape(shell_cmd.to_owned()));
            }
            if let Some(cmd) = text.strip_prefix('/').filter(|_| !text.contains('\n')) {
                return handle_slash_command(app, state, home, cmd).await;
            }
            return Some(TuiAction::Turn(text));
        }
        (KeyCode::Backspace, m) if m.contains(KeyModifiers::CONTROL) => app.delete_word_back(),
        (KeyCode::Backspace, _) => app.backspace(),
        _ if ctrl_letter == Some('h') => app.backspace(),
        (KeyCode::Delete, m) if m.contains(KeyModifiers::CONTROL) => app.delete_word_forward(),
        (KeyCode::Delete, _) => app.delete_char_forward(),
        (KeyCode::Left, m) if m.contains(KeyModifiers::CONTROL) => {
            app.move_word_left(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Right, m) if m.contains(KeyModifiers::CONTROL) => {
            app.move_word_right(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Left, m) => app.move_left(m.contains(KeyModifiers::SHIFT)),
        (KeyCode::Right, m) => app.move_right(m.contains(KeyModifiers::SHIFT)),
        (KeyCode::Home, m) => app.move_home(m.contains(KeyModifiers::SHIFT)),
        (KeyCode::End, m) => app.move_end(m.contains(KeyModifiers::SHIFT)),
        _ if ctrl_letter == Some('a') => {
            app.clear_selection();
            app.select_all_input();
        }
        _ if ctrl_letter == Some('x') => {
            if let Some(selected) = app.selected_input_text().map(str::to_owned) {
                match clipboard::write_text(&selected) {
                    Ok(()) => app.set_copy_status(
                        false,
                        format!("Cut {} char(s) to clipboard", selected.chars().count()),
                    ),
                    Err(msg) => app.set_copy_status(true, msg),
                }
                app.delete_input_selection();
            }
        }
        _ if ctrl_letter == Some('e') => app.move_end(false),
        _ if ctrl_letter == Some('w') => app.delete_word_back(),
        _ if ctrl_letter == Some('k') => app.delete_to_line_end(),
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
            app.clear_input_selection();
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
                app.clear_input_selection();
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
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *scroll = scroll.saturating_sub(1);
                app.modal = Some(modal);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *scroll = scroll.saturating_add(1);
                app.modal = Some(modal);
            }
            KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                if let Some(tx) = reply.take() {
                    let _ = tx.send(ApprovalDecision::Approved);
                }
            }
            KeyCode::Char('s' | 'S' | 'ы' | 'Ы') => {
                let _ = approval::record_grant(request, Grant::Session);
                if let Some(tx) = reply.take() {
                    let _ = tx.send(ApprovalDecision::Approved);
                }
            }
            KeyCode::Char('a' | 'A' | 'ф' | 'Ф') => {
                if let Some(err) = approval::record_grant(request, Grant::Always) {
                    TuiApp::push_notice(
                        state,
                        NoticeKind::Warn,
                        format!("Could not save permanent approval setting: {err}"),
                    );
                }
                if let Some(tx) = reply.take() {
                    let _ = tx.send(ApprovalDecision::Approved);
                }
            }
            KeyCode::Char('n' | 'N' | 'т' | 'Т') | KeyCode::Esc => {
                if let Some(tx) = reply.take() {
                    let _ = tx.send(ApprovalDecision::Denied);
                }
            }
            _ => {
                app.modal = Some(modal);
            }
        },
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

pub(super) fn open_session_picker(app: &mut TuiApp, state: &mut ReplState, home: Option<&Path>) {
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

pub(super) fn resume_tui_target(
    app: &mut TuiApp,
    state: &mut ReplState,
    home: Option<&Path>,
    target: &str,
) {
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
            app.clear_selection();
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
) -> Option<TuiAction> {
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
            app.clear_selection();
        }
        "copy" => match crate::ui::extract_copy_payload(&state.feed_entries, argument) {
            Ok((text, label)) => match clipboard::write_text(&text) {
                Ok(()) => {
                    let msg = format!(
                        "Copied {label} ({} chars) to clipboard.",
                        text.chars().count()
                    );
                    app.set_copy_status(false, format!("✓ {msg}"));
                    TuiApp::push_notice(state, NoticeKind::Info, msg);
                }
                Err(err) => {
                    app.set_copy_status(true, format!("Copy failed: {err}"));
                    TuiApp::push_notice(state, NoticeKind::Warn, format!("Copy failed: {err}"));
                }
            },
            Err(err) => TuiApp::push_notice(state, NoticeKind::Warn, err),
        },
        "mouse" => {
            toggle_mouse_capture(app);
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
            app.clear_selection();
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
            if home.is_none() {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Warn,
                    "Cannot compress: profile root is unavailable.",
                );
                return None;
            }
            let Some(id) = state.session.clone() else {
                TuiApp::push_notice(
                    state,
                    NoticeKind::Info,
                    "Nothing to compress yet: start a conversation first.",
                );
                return None;
            };
            let focus = (!argument.is_empty()).then(|| argument.to_owned());
            return Some(TuiAction::Compress {
                session_id: id,
                focus,
            });
        }
        "browser" => {
            return Some(TuiAction::Browser(argument.to_owned()));
        }
        "retry" => match state.unsaved.take() {
            Some(previous) => return Some(TuiAction::Turn(previous)),
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
                return Some(TuiAction::Turn(inv.prompt));
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
