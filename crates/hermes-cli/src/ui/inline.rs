//! Enhanced inline streaming REPL (with automatic plain-text fallback).
//!
//! Preserves native terminal scrollback while adding:
//! - Styled startup banner (profile, provider/model, session, shortcuts)
//! - Tab completion & inline hints for `/` slash commands
//! - Streaming Markdown & syntax-highlighted code blocks
//! - Live braille spinners & timed tool badges (`✓ read_file (14ms)`, `✗ terminal`)
//! - Clean `/ui [inline|tui|plain]` mode switching

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Config, Context, Editor, Event as ReadlineEvent, EventContext,
    EventHandler, Helper, KeyEvent as ReadlineKeyEvent, RepeatCount,
};
use tokio::task::spawn_blocking;

use crate::approval;
use crate::chat::{self, Event, ManualCompress};
use crate::ui::markdown::StreamMarkdown;
use crate::ui::{
    active_at_token, format_context_length, format_duration_ms, format_shell_escape,
    format_skills_list, format_tokens_compact, load_resumed_session, match_slash_commands,
    parse_stderr_event, record_prompt_history, ActiveUi, FeedEntry, NoticeKind, ParsedStderr,
    RecapEntry, ReplState, ResumeRecap, ToolStatus, UiOutcome, HELP_TEXT, SLASH_COMMANDS,
};

const PROMPT: &str = "› ";
const CONTINUATION: &str = "… ";
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const FG_CYAN: &str = "\x1b[36m";
const FG_BRIGHT_CYAN: &str = "\x1b[96m";
const FG_GREEN: &str = "\x1b[32m";
const FG_RED: &str = "\x1b[31m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_GRAY: &str = "\x1b[90m";
const FG_MAGENTA: &str = "\x1b[95m";

#[derive(Clone)]
pub struct SlashHelper {
    styled: bool,
    home: Option<PathBuf>,
    root: PathBuf,
}

impl SlashHelper {
    pub fn new(styled: bool) -> Self {
        Self {
            styled,
            home: None,
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn with_context(mut self, home: Option<&Path>, root: &Path) -> Self {
        self.home = home.map(Path::to_path_buf);
        self.root = root.to_path_buf();
        self
    }
}

impl Helper for SlashHelper {}
impl Validator for SlashHelper {}

impl Completer for SlashHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Complete `@file:...`, `@folder:...`, `@diff`, `@staged`, `@git:3`, `@url:` anywhere in line
        if let Some((start, token)) = active_at_token(line, pos) {
            let candidates =
                hermes_runtime::context_refs::complete_at_references(&self.root, token, 15)
                    .into_iter()
                    .map(|c| Pair {
                        display: format!("{:<28} {}", c.display, c.detail),
                        replacement: c.replacement,
                    })
                    .collect();
            return Ok((start, candidates));
        }

        let before = &line[..pos];
        if !before.starts_with('/') {
            return Ok((0, Vec::new()));
        }
        // Complete sub-arguments for `/ui <mode>`
        if let Some(arg) = before.strip_prefix("/ui ") {
            let start = "/ui ".len();
            let prefix = arg.to_ascii_lowercase();
            let candidates = ["inline", "tui", "plain"]
                .into_iter()
                .filter(|m| m.starts_with(&prefix))
                .map(|m| Pair {
                    display: m.to_owned(),
                    replacement: m.to_owned(),
                })
                .collect();
            return Ok((start, candidates));
        }
        if let Some(arg) = before.strip_prefix("/browser ") {
            let start = "/browser ".len();
            let prefix = arg.to_ascii_lowercase();
            let candidates = ["connect", "status", "disconnect"]
                .into_iter()
                .filter(|m| m.starts_with(&prefix))
                .map(|m| Pair {
                    display: m.to_owned(),
                    replacement: m.to_owned(),
                })
                .collect();
            return Ok((start, candidates));
        }
        if before.contains(char::is_whitespace) {
            return Ok((pos, Vec::new()));
        }
        let mut matches: Vec<Pair> = match_slash_commands(before)
            .into_iter()
            .map(|c| Pair {
                display: format!("{:<14} {}", c.name, c.description),
                replacement: c.name.to_owned(),
            })
            .collect();
        let needle = before[1..].to_ascii_lowercase();
        for skill in hermes_runtime::context_refs::list_skill_commands(self.home.as_deref()) {
            if needle.is_empty() || skill.name.starts_with(&needle) {
                let slash_name = format!("/{}", skill.name);
                if !matches.iter().any(|p| p.replacement == slash_name) {
                    matches.push(Pair {
                        display: format!("{:<14} [skill] {}", slash_name, skill.description),
                        replacement: slash_name,
                    });
                }
            }
        }
        Ok((0, matches))
    }
}

impl Hinter for SlashHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if !self.styled || pos < line.len() || !line.starts_with('/') || line.len() < 2 {
            return None;
        }
        if let Some(arg) = line.strip_prefix("/ui ") {
            let prefix = arg.to_ascii_lowercase();
            return ["inline", "tui", "plain"]
                .into_iter()
                .find(|m| m.starts_with(&prefix) && *m != prefix)
                .map(|m| m[prefix.len()..].to_owned());
        }
        if let Some(arg) = line.strip_prefix("/browser ") {
            let prefix = arg.to_ascii_lowercase();
            return ["connect", "status", "disconnect"]
                .into_iter()
                .find(|m| m.starts_with(&prefix) && *m != prefix)
                .map(|m| m[prefix.len()..].to_owned());
        }
        if line.contains(char::is_whitespace) {
            return None;
        }
        let lower = line.to_ascii_lowercase();
        SLASH_COMMANDS
            .iter()
            .find(|c| c.name.starts_with(&lower) && c.name != lower)
            .map(|c| c.name[lower.len()..].to_owned())
    }
}

impl Highlighter for SlashHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        if !self.styled {
            return Cow::Borrowed(prompt);
        }
        // Keep the exact contiguous `"› "` bytes intact inside the ANSI wrapper so PTY callers
        // matching `"› "` continue to work identically.
        if prompt == PROMPT {
            Cow::Owned(format!("{BOLD}{FG_BRIGHT_CYAN}{PROMPT}{RESET}"))
        } else if prompt == CONTINUATION {
            Cow::Owned(format!("{DIM}{FG_CYAN}{CONTINUATION}{RESET}"))
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if self.styled {
            Cow::Owned(format!("{DIM}{FG_GRAY}{hint}{RESET}"))
        } else {
            Cow::Borrowed(hint)
        }
    }

    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.styled || !line.starts_with('/') {
            return Cow::Borrowed(line);
        }
        if let Some((cmd, rest)) = line.split_once(char::is_whitespace) {
            Cow::Owned(format!("{BOLD}{FG_CYAN}{cmd}{RESET} {rest}"))
        } else {
            Cow::Owned(format!("{BOLD}{FG_CYAN}{line}{RESET}"))
        }
    }

    fn highlight_char(&self, line: &str, _pos: usize, _forced: bool) -> bool {
        self.styled && line.starts_with('/')
    }
}

type Lines = Editor<SlashHelper, FileHistory>;

enum Input {
    Line(String),
    Interrupted,
    End,
}

struct ClipboardPasteHandler;

impl ConditionalEventHandler for ClipboardPasteHandler {
    fn handle(
        &self,
        _evt: &ReadlineEvent,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        match crate::ui::clipboard::read_text() {
            crate::ui::clipboard::ClipboardRead::Text(text) => {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                Some(Cmd::Insert(1, normalized))
            }
            _ => Some(Cmd::Noop),
        }
    }
}

fn bind_clipboard_paste(editor: &mut Lines) {
    editor.bind_sequence(
        ReadlineKeyEvent::ctrl('v'),
        EventHandler::Conditional(Box::new(ClipboardPasteHandler)),
    );
}

fn placeholder(styled: bool) -> Lines {
    let mut ed = Editor::with_config(Config::default()).expect("a line editor can be created");
    ed.set_helper(Some(SlashHelper::new(styled)));
    bind_clipboard_paste(&mut ed);
    ed
}

async fn read(editor: Lines, prompt: &'static str) -> (Lines, Result<String, ReadlineError>) {
    spawn_blocking(move || {
        let mut editor = editor;
        let line = editor.readline(prompt);
        (editor, line)
    })
    .await
    .expect("the line reader does not panic")
}

async fn next_input(editor: &mut Lines, styled: bool) -> Input {
    let mut text = String::new();
    let mut prompt = PROMPT;
    loop {
        let (returned, line) = read(std::mem::replace(editor, placeholder(styled)), prompt).await;
        *editor = returned;
        match line {
            Ok(line) => match line.strip_suffix('\\') {
                Some(head) => {
                    text.push_str(head);
                    text.push('\n');
                    prompt = CONTINUATION;
                }
                None => {
                    text.push_str(&line);
                    return Input::Line(text);
                }
            },
            Err(ReadlineError::Interrupted) => return Input::Interrupted,
            Err(_) => return Input::End,
        }
    }
}

// ── Live Braille Spinner (stderr) ───────────────────────────────────────────────────

struct SpinnerInner {
    active: bool,
    visible: bool,
    label: String,
    frame: usize,
}

#[derive(Clone)]
struct InlineSpinner {
    enabled: bool,
    inner: Arc<Mutex<SpinnerInner>>,
}

impl InlineSpinner {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            inner: Arc::new(Mutex::new(SpinnerInner {
                active: false,
                visible: false,
                label: String::new(),
                frame: 0,
            })),
        }
    }

    fn start(&self, label: impl Into<String>) {
        if !self.enabled {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        guard.active = true;
        guard.label = label.into();
    }

    fn pause_and_clear(&self) {
        if !self.enabled {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        guard.active = false;
        if guard.visible {
            let mut err = io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
            guard.visible = false;
        }
    }

    fn tick(&self) {
        if !self.enabled {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        if !guard.active {
            return;
        }
        let glyph = SPINNER_FRAMES[guard.frame % SPINNER_FRAMES.len()];
        guard.frame = guard.frame.wrapping_add(1);
        let mut err = io::stderr().lock();
        let _ = write!(
            err,
            "\r\x1b[2K{FG_CYAN}{glyph}{RESET} {DIM}{}{RESET}",
            guard.label
        );
        let _ = err.flush();
        guard.visible = true;
    }
}

/// Format the startup banner shown in Inline mode (pure function for testing).
pub fn format_banner(
    profile: &str,
    provider: &str,
    model: &str,
    session: Option<&str>,
    yolo: bool,
    color: bool,
) -> String {
    let sess = session
        .map(|s| {
            let short: String = s.chars().take(12).collect();
            format!("session {short}")
        })
        .unwrap_or_else(|| "new session".to_owned());
    let yolo_tag = if yolo { " · YOLO" } else { "" };
    if !color {
        return format!(
            "Hermes · {profile} · {provider}:{model} · {sess}{yolo_tag}\n\
             /help for commands · /ui tui for fullscreen · Ctrl-C stop · Ctrl-D exit"
        );
    }
    format!(
        "{FG_GRAY}┌─ {BOLD}{FG_BRIGHT_CYAN}Hermes{RESET}{FG_GRAY} ─────────────────────────────────────────────┐{RESET}\n\
         {FG_GRAY}│{RESET} {BOLD}{model}{RESET} {DIM}({provider}){RESET}  {FG_GRAY}·{RESET}  profile {FG_CYAN}{profile}{RESET}  {FG_GRAY}·{RESET}  {DIM}{sess}{RESET}{FG_YELLOW}{yolo_tag}{RESET}\n\
         {FG_GRAY}│{RESET} {DIM}/help commands · /ui tui fullscreen · Ctrl-C stop · Ctrl-D exit{RESET}\n\
         {FG_GRAY}└───────────────────────────────────────────────────────┘{RESET}"
    )
}

/// Run the inline streaming REPL (or plain mode when `mode == ActiveUi::Plain`).
pub async fn run(
    home: Option<&Path>,
    state: &mut ReplState,
    mode: ActiveUi,
    show_banner: bool,
    startup_recap: Option<ResumeRecap>,
) -> Result<UiOutcome, String> {
    let styled = mode == ActiveUi::Inline && state.color;
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();

    let mut editor: Lines = Editor::with_config(
        Config::builder()
            .auto_add_history(false)
            .history_ignore_dups(true)
            .map_err(|error| error.to_string())?
            .build(),
    )
    .map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    editor.set_helper(Some(SlashHelper::new(styled).with_context(home, &cwd)));
    bind_clipboard_paste(&mut editor);

    for entry in &state.prompt_history {
        let _ = editor.add_history_entry(entry.as_str());
    }

    if show_banner {
        if styled {
            let yolo =
                hermes_runtime::approval_policy::installed().is_some_and(|p| p.approves_all());
            let banner = format_banner(
                &state.profile_label,
                state.request.provider_name(),
                state.request.model().unwrap_or("(unset)"),
                state.session.as_deref(),
                yolo,
                true,
            );
            eprintln!("{banner}");
        } else if io::stdin().is_terminal() {
            eprintln!("hsr: type a request, /help for commands, Ctrl-D to leave");
        }
    }
    if let Some(recap) = startup_recap {
        print_resume_recap(&recap, styled, true);
    }

    let mut rerun: Option<String> = None;
    let mut pending_sessions: Vec<String> = Vec::new();
    let mut outcome = UiOutcome::Exit;

    loop {
        let line = match rerun.take() {
            Some(line) => line,
            None => match next_input(&mut editor, styled).await {
                Input::Line(line) => line.trim().to_owned(),
                Input::Interrupted => {
                    pending_sessions.clear();
                    continue;
                }
                Input::End => break,
            },
        };
        let line = line.as_str();
        if line.is_empty() {
            pending_sessions.clear();
            continue;
        }
        if !pending_sessions.is_empty() {
            let candidates = std::mem::take(&mut pending_sessions);
            if line.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(index) = line.parse::<usize>() {
                    if index >= 1 && index <= candidates.len() {
                        resume_inline_target(home, state, &candidates[index - 1], styled);
                        continue;
                    }
                    eprintln!(
                        "Session number {index} is out of range (1..={}).",
                        candidates.len()
                    );
                    continue;
                }
            }
        }
        if !matches!(line, "/exit" | "/quit") {
            let _ = editor.add_history_entry(line);
            record_prompt_history(home, &mut state.prompt_history, line);
        }
        if let Some(shell_cmd) = line.strip_prefix('!').filter(|_| !line.contains('\n')) {
            let (is_err, output) = format_shell_escape(&cwd, shell_cmd);
            if is_err {
                eprintln!("{output}");
            } else {
                println!("{output}");
            }
            continue;
        }
        let mut effective_turn_line: Option<String> = None;
        if let Some(command) = line.strip_prefix('/').filter(|_| !line.contains('\n')) {
            let (name, argument) = command
                .split_once(char::is_whitespace)
                .map_or((command, ""), |(name, rest)| (name, rest.trim()));
            match name {
                "exit" | "quit" => break,
                "help" => println!("{HELP_TEXT}"),
                "palette" => {
                    println!("{HELP_TEXT}\n\n{}", format_skills_list(home, ""));
                }
                "clear" => {
                    state.feed_entries.clear();
                    if styled {
                        print!("\x1b[2J\x1b[H");
                        let _ = io::stdout().flush();
                    }
                }
                "undo" => {
                    let Some(home) = home else {
                        eprintln!("Cannot undo: profile root is unavailable.");
                        continue;
                    };
                    let Some(id) = state.session.clone() else {
                        println!("Nothing to undo: no active saved session yet.");
                        continue;
                    };
                    match hermes_runtime::storage::undo_last_turn(home, &id) {
                        Ok(0) => println!("Nothing to undo in session {id}."),
                        Ok(removed) => {
                            let _ = load_resumed_session(Some(home), state, &id);
                            println!(
                                "Undid last exchange in session {id} ({removed} message(s) removed)."
                            );
                        }
                        Err(err) => eprintln!("Undo failed: {err}"),
                    }
                }
                "skills" => {
                    println!("{}", format_skills_list(home, argument));
                }
                "copy" => match crate::ui::extract_copy_payload(&state.feed_entries, argument) {
                    Ok((text, label)) => match crate::ui::clipboard::write_text(&text) {
                        Ok(()) => println!(
                            "Copied {label} ({} chars) to clipboard.",
                            text.chars().count()
                        ),
                        Err(err) => eprintln!("Copy failed: {err}"),
                    },
                    Err(err) => eprintln!("{err}"),
                },
                "ui" if argument.is_empty() => {
                    println!(
                        "Current UI mode: {} (options: inline, tui, plain)",
                        mode.label()
                    );
                }
                "ui" => match argument.to_ascii_lowercase().as_str() {
                    "tui" | "fullscreen" | "full" => {
                        if !tty {
                            eprintln!("Fullscreen TUI requires an interactive terminal; staying in plain mode.");
                        } else {
                            outcome = UiOutcome::Switch(ActiveUi::Tui);
                            break;
                        }
                    }
                    "inline" | "stream" | "repl" => {
                        if mode == ActiveUi::Inline {
                            println!("Already in inline mode.");
                        } else if !tty {
                            eprintln!("Inline mode requires an interactive terminal; staying in plain mode.");
                        } else {
                            outcome = UiOutcome::Switch(ActiveUi::Inline);
                            break;
                        }
                    }
                    "plain" | "off" | "none" => {
                        if mode == ActiveUi::Plain {
                            println!("Already in plain mode.");
                        } else {
                            outcome = UiOutcome::Switch(ActiveUi::Plain);
                            break;
                        }
                    }
                    other => {
                        eprintln!("Unknown UI mode '{other}'. Expected: inline, tui, or plain.");
                    }
                },
                "new" | "reset" => {
                    announce(&state.session, styled);
                    state.session = None;
                    state.unsaved = None;
                    state.reset_metrics();
                    state.feed_entries.clear();
                    state.guard.lock().unwrap().reset();
                    hermes_runtime::terminal::forget_session(&state.scope);
                    if !state.request.has_explicit_model_override() {
                        state.request.set_primary_model(state.default_model.clone());
                    }
                }
                "model" if argument.is_empty() => {
                    println!("{}", state.request.model().unwrap_or("(not set)"));
                }
                "model" => {
                    state.request.set_model(argument);
                    state.guard.lock().unwrap().reset();
                    println!("Model for the next turns: {argument}");
                }
                "compress" | "compact" => {
                    let Some(home) = home else {
                        eprintln!("Cannot compress: profile root is unavailable.");
                        continue;
                    };
                    let Some(id) = state.session.clone() else {
                        println!("Nothing to compress yet: start a conversation first.");
                        continue;
                    };
                    let focus = (!argument.is_empty()).then_some(argument);
                    match chat::compress_session(home, &state.request, &id, focus, &state.guard)
                        .await
                    {
                        Ok(ManualCompress::Applied { archived }) => {
                            state.compressions = state.compressions.saturating_add(1);
                            if let Ok(transcript) =
                                hermes_runtime::storage::SessionStore::open_read_only(home)
                                    .and_then(|s| s.resume_display_transcript(&id))
                            {
                                let est =
                                    hermes_runtime::model_metadata::estimate_active_messages_tokens(
                                        &transcript.messages,
                                    );
                                state.record_context(est, true);
                            }
                            println!("Compressed session {id}: archived {archived} message(s) into a summary.");
                        }
                        Ok(ManualCompress::Unchanged) => {
                            println!("Nothing to compress: conversation is short enough already.");
                        }
                        Ok(ManualCompress::Skipped) => {
                            eprintln!("Compression skipped: summary failed or did not shorten the conversation.");
                        }
                        Err(error) => eprintln!("Compression failed: {error}"),
                    }
                }
                "yolo" => {
                    if let Some(policy) = hermes_runtime::approval_policy::installed() {
                        let on = !policy.approves_all();
                        policy.set_approve_all(on);
                        println!(
                            "{}",
                            if on {
                                "yolo on: file changes and commands run without asking, except what approvals.deny names"
                            } else {
                                "yolo off: you will be asked again"
                            }
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
                    pending_sessions = list_sessions(home, state.session.as_deref(), styled);
                }
                "sessions" => {
                    resume_inline_target(home, state, argument, styled);
                }
                "resume" if argument.is_empty() => {
                    eprintln!("Usage: /resume <session id | number | title>");
                    pending_sessions = list_sessions(home, state.session.as_deref(), styled);
                }
                "resume" => {
                    resume_inline_target(home, state, argument, styled);
                }
                "browser" => {
                    let (is_err, msg) =
                        crate::browser::handle_slash_browser(home, state, argument).await;
                    if is_err {
                        eprintln!("{msg}");
                    } else {
                        println!("{msg}");
                    }
                }
                "retry" => match state.unsaved.take() {
                    Some(previous) => rerun = Some(previous),
                    None => eprintln!(
                        "Nothing to retry: the last turn finished and is saved. Ask again to add to it."
                    ),
                },
                other => {
                    if let Some(inv) =
                        hermes_runtime::context_refs::resolve_skill_invocation(home, line)
                    {
                        eprintln!("Running skill /{}...", inv.name);
                        effective_turn_line = Some(inv.prompt);
                    } else {
                        eprintln!("Unknown command /{other}; /help lists the commands.");
                    }
                }
            }
            if effective_turn_line.is_none() {
                continue;
            }
        }

        let turn_query = effective_turn_line.as_deref().unwrap_or(line);
        state.feed_entries.push(FeedEntry::User(line.to_owned()));
        let turn =
            state
                .request
                .clone()
                .for_turn(turn_query, &state.scope, state.session.as_deref());

        let mut saved = None;
        let mut usage = None;
        let mut context_usage: Option<(u64, bool)> = None;
        let mut turn_compressions: u32 = 0;
        let mut output_open = true;
        let mut md = StreamMarkdown::new(styled);
        let mut assistant_buf = String::new();
        let mut tool_starts: HashMap<String, Instant> = HashMap::new();
        let mut tool_details: HashMap<String, String> = HashMap::new();
        let mut has_printed_text = false;
        let started = Instant::now();

        let spinner = InlineSpinner::new(styled);
        spinner.start("Thinking…");
        let spinner_task = if styled {
            let sp = spinner.clone();
            Some(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(85));
                loop {
                    tick.tick().await;
                    sp.tick();
                }
            }))
        } else {
            None
        };

        let sp_for_events = spinner.clone();
        let sp_for_approve = spinner.clone();

        let turn_outcome = tokio::select! {
            result = chat::execute_with_guard(
                home,
                turn,
                Some(&state.guard),
                |event| {
                    match event {
                        Event::Stdout(chunk) => {
                            if styled && !has_printed_text {
                                let trimmed = chunk.trim_start_matches(['\r', '\n']);
                                if trimmed.is_empty() {
                                    return output_open;
                                }
                                sp_for_events.pause_and_clear();
                                has_printed_text = true;
                                assistant_buf.push_str(trimmed);
                                let rendered = md.push(trimmed);
                                if !rendered.is_empty() {
                                    output_open = write_stdout(&rendered).is_ok();
                                }
                                return output_open;
                            }
                            sp_for_events.pause_and_clear();
                            if chunk == "\n" && has_printed_text {
                                let tail = md.finish();
                                if !tail.is_empty() {
                                    let _ = write_stdout(&tail);
                                }
                            } else {
                                has_printed_text = true;
                                assistant_buf.push_str(&chunk);
                            }
                            let rendered = if chunk == "\n" {
                                "\n".to_owned()
                            } else {
                                md.push(&chunk)
                            };
                            if !rendered.is_empty() {
                                output_open = write_stdout(&rendered).is_ok();
                            }
                            output_open
                        }
                        Event::Stderr(text) => {
                            match parse_stderr_event(&text) {
                                ParsedStderr::Session(id) => {
                                    saved = Some(id);
                                    true
                                }
                                ParsedStderr::Usage { input, output } => {
                                    usage = Some((input, output));
                                    true
                                }
                                ParsedStderr::ContextUsage { tokens, estimated } => {
                                    context_usage = Some((tokens, estimated));
                                    true
                                }
                                ParsedStderr::ToolStarted { name, detail } => {
                                    tool_starts.insert(name.clone(), Instant::now());
                                    if let Some(d) = &detail {
                                        tool_details.insert(name.clone(), d.clone());
                                    } else {
                                        tool_details.remove(&name);
                                    }
                                    if styled {
                                        if has_printed_text {
                                            let tail = md.finish();
                                            if !tail.is_empty() {
                                                let _ = write_stdout(&tail);
                                            }
                                            let _ = write_stdout("\n");
                                            has_printed_text = false;
                                        }
                                        let label = match detail {
                                            Some(d) => format!(
                                                "Running {name} · {}…",
                                                approval::sanitize_terminal_text(&d)
                                            ),
                                            None => format!("Running {name}…"),
                                        };
                                        sp_for_events.start(label);
                                    } else {
                                        output_open = chat::display(Event::Stderr(text)).is_ok();
                                    }
                                    output_open
                                }
                                ParsedStderr::ToolOutput { name, text } => {
                                    if styled {
                                        if let Some(last) = text
                                            .lines()
                                            .rev()
                                            .map(str::trim)
                                            .find(|l| !l.is_empty())
                                        {
                                            let clean = approval::sanitize_terminal_text(last);
                                            let clip: String = clean.chars().take(72).collect();
                                            if !clip.is_empty() {
                                                sp_for_events.start(format!("{name} · {clip}"));
                                            }
                                        }
                                    }
                                    true
                                }
                                ParsedStderr::Reasoning(text) => {
                                    if styled && !has_printed_text {
                                        if let Some(last) = text
                                            .lines()
                                            .rev()
                                            .map(str::trim)
                                            .find(|l| !l.is_empty())
                                        {
                                            let clean = approval::sanitize_terminal_text(last);
                                            let clip: String = clean.chars().take(72).collect();
                                            if !clip.is_empty() {
                                                sp_for_events.start(format!("Thinking · {clip}"));
                                            }
                                        }
                                    }
                                    true
                                }
                                ParsedStderr::ToolFinished {
                                    name,
                                    succeeded,
                                    summary,
                                } => {
                                    let elapsed = tool_starts
                                        .remove(&name)
                                        .map(|t| t.elapsed().as_millis() as u64);
                                    let detail = tool_details.remove(&name);
                                    state.feed_entries.push(FeedEntry::Tool {
                                        name: name.clone(),
                                        detail: detail.clone(),
                                        status: if succeeded {
                                            ToolStatus::Succeeded
                                        } else {
                                            ToolStatus::Failed
                                        },
                                        summary: summary.clone(),
                                        elapsed_ms: elapsed,
                                    });
                                    if styled {
                                        sp_for_events.pause_and_clear();
                                        let detail_part = detail
                                            .as_deref()
                                            .map(|d| {
                                                format!(
                                                    " {DIM}· {}{RESET}",
                                                    approval::sanitize_terminal_text(d)
                                                )
                                            })
                                            .unwrap_or_default();
                                        let dur_part = elapsed.map(format_duration_ms);
                                        let sum_part = summary
                                            .as_deref()
                                            .map(approval::sanitize_terminal_text)
                                            .filter(|s| !s.is_empty());
                                        let meta = match (sum_part, dur_part) {
                                            (Some(s), Some(d)) => format!(" ({s} · {d})"),
                                            (Some(s), None) => format!(" ({s})"),
                                            (None, Some(d)) => format!(" ({d})"),
                                            (None, None) => String::new(),
                                        };
                                        if succeeded {
                                            eprintln!("  {FG_GREEN}✓{RESET} {BOLD}{name}{RESET}{detail_part}{DIM}{meta}{RESET}");
                                        } else {
                                            eprintln!("  {FG_RED}✗{RESET} {BOLD}{name}{RESET}{detail_part}{FG_RED}{meta}{RESET}");
                                        }
                                        sp_for_events.start("Thinking…");
                                    } else {
                                        output_open = chat::display(Event::Stderr(text)).is_ok();
                                    }
                                    output_open
                                }
                                ParsedStderr::Fallback(model) => {
                                    sp_for_events.pause_and_clear();
                                    if styled {
                                        eprintln!("  {FG_YELLOW}⚡{RESET} {DIM}Fallback → {model}{RESET}");
                                    } else {
                                        output_open = chat::display(Event::Stderr(text)).is_ok();
                                    }
                                    output_open
                                }
                                ParsedStderr::ContextCompressed(msg) => {
                                    turn_compressions = turn_compressions.saturating_add(1);
                                    sp_for_events.pause_and_clear();
                                    if styled {
                                        eprintln!("  {FG_CYAN}◈{RESET} {DIM}{msg}{RESET}");
                                    } else {
                                        output_open = chat::display(Event::Stderr(text)).is_ok();
                                    }
                                    output_open
                                }
                                ParsedStderr::IterationLimit(msg) => {
                                    sp_for_events.pause_and_clear();
                                    if styled {
                                        eprintln!("  {FG_YELLOW}⚠{RESET} {msg}");
                                    } else {
                                        output_open = chat::display(Event::Stderr(text)).is_ok();
                                    }
                                    output_open
                                }
                                ParsedStderr::Other(msg) => {
                                    sp_for_events.pause_and_clear();
                                    output_open = chat::display(Event::Stderr(msg)).is_ok();
                                    output_open
                                }
                            }
                        }
                        other => {
                            output_open = chat::display(other).is_ok();
                            output_open
                        }
                    }
                },
                move |req| {
                    let sp = sp_for_approve.clone();
                    async move {
                        sp.pause_and_clear();
                        approval::prompt(req).await
                    }
                },
            ) => Some(result),
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("signal handler failed: {error}"))?;
                None
            }
        };

        if let Some(handle) = spinner_task {
            handle.abort();
        }
        spinner.pause_and_clear();
        let tail = md.finish();
        if !tail.is_empty() {
            let _ = write_stdout(&tail);
        }
        if let Some((tokens, estimated)) = context_usage {
            state.record_context(tokens, estimated);
        }
        state.compressions = state.compressions.saturating_add(turn_compressions);

        match turn_outcome {
            None => {
                hermes_runtime::mcp::flush_cancellations().await;
                if styled {
                    eprintln!("\n{FG_YELLOW}interrupted{RESET} {DIM}(the turn is not saved; /retry sends it again){RESET}");
                } else {
                    eprintln!("\ninterrupted (the turn is not saved; /retry sends it again)");
                }
                state.unsaved = Some(line.to_owned());
            }
            Some(Ok(status)) => {
                state.exit_code = status;
                state.unsaved = None;
                let cleaned = assistant_buf.trim_start_matches(['\r', '\n']).trim_end();
                if !cleaned.is_empty() {
                    state
                        .feed_entries
                        .push(FeedEntry::Assistant(cleaned.to_owned()));
                }
                let seconds = started.elapsed().as_secs_f64();
                if let Some((input, output)) = usage {
                    state.record_usage(input, output);
                }
                let (ctx_len, _) = state.request.display_context_length();
                let ctx_used = state.context_tokens.min(ctx_len);
                let ctx_pct = ((ctx_used as f64 / ctx_len.max(1) as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8;
                let ctx_mark = if state.context_estimated && ctx_used > 0 {
                    "~"
                } else {
                    ""
                };
                let ctx_suffix = if ctx_used > 0 {
                    format!(
                        " · ctx {ctx_mark}{}/{} ({ctx_mark}{ctx_pct}%)",
                        format_tokens_compact(ctx_used),
                        format_context_length(ctx_len)
                    )
                } else {
                    String::new()
                };
                match usage {
                    Some((input, output)) if input + output > 0 => {
                        if styled {
                            eprintln!(
                                "{DIM}{FG_GRAY}[{seconds:.1}s · {input} in / {output} out{ctx_suffix}]{RESET}"
                            );
                        } else {
                            eprintln!("[{seconds:.1}s · {input} in / {output} out{ctx_suffix}]");
                        }
                    }
                    _ => {
                        if styled {
                            eprintln!("{DIM}{FG_GRAY}[{seconds:.1}s{ctx_suffix}]{RESET}");
                        } else {
                            eprintln!("[{seconds:.1}s{ctx_suffix}]");
                        }
                    }
                }
            }
            Some(Err(error)) => {
                if styled {
                    eprintln!("{FG_RED}Error:{RESET} {error}");
                } else {
                    eprintln!("{error}");
                }
                state.feed_entries.push(FeedEntry::Notice {
                    kind: NoticeKind::Error,
                    text: error,
                });
                state.unsaved = Some(line.to_owned());
                if !output_open {
                    break;
                }
            }
        }
        if let Some(id) = saved {
            state.session = Some(id);
        }
    }

    Ok(outcome)
}

fn write_stdout(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

pub fn announce(session: &Option<String>, styled: bool) {
    if let Some(id) = session {
        if styled {
            eprintln!(
                "{DIM}Session: {RESET}{FG_CYAN}{id}{RESET}  {DIM}(continue with: hsr chat --resume {id}){RESET}"
            );
        } else {
            eprintln!("Session: {id}  (continue with: hsr chat --resume {id})");
        }
    }
}

fn print_resume_recap(recap: &ResumeRecap, styled: bool, to_stderr: bool) {
    let emit = |line: &str| {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    let headline = recap.headline();
    if styled {
        emit(&format!("{DIM}{FG_CYAN}◈ {headline}{RESET}"));
    } else {
        emit(&headline);
    }
    if recap.skipped_entries > 0 {
        let skipped_msg = format!("... {} earlier message(s) ...", recap.skipped_entries);
        if styled {
            emit(&format!("{DIM}{FG_GRAY}  {skipped_msg}{RESET}"));
        } else {
            emit(&format!("  {skipped_msg}"));
        }
    }
    for entry in &recap.entries {
        match entry {
            RecapEntry::User(text) => {
                if styled {
                    emit(&format!("{BOLD}{FG_CYAN}You:{RESET} {text}"));
                } else {
                    emit(&format!("You: {text}"));
                }
            }
            RecapEntry::Assistant { text, .. } => {
                if styled {
                    emit(&format!("{BOLD}{FG_MAGENTA}Hermes:{RESET} {text}"));
                } else {
                    emit(&format!("Hermes: {text}"));
                }
            }
        }
    }
}

fn resume_inline_target(home: Option<&Path>, state: &mut ReplState, target: &str, styled: bool) {
    let mut resolved_query = target.to_owned();
    if target.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(index) = target.parse::<usize>() {
            if let Some(home) = home {
                match hermes_runtime::storage::SessionStore::open_read_only(home)
                    .and_then(|s| s.recent(20, None))
                {
                    Ok(sessions) if index >= 1 && index <= sessions.len() => {
                        resolved_query = sessions[index - 1].id.clone();
                    }
                    Ok(sessions) => {
                        eprintln!(
                            "Session number {index} is out of range (1..={}).",
                            sessions.len()
                        );
                        return;
                    }
                    Err(err) => {
                        eprintln!("Could not read sessions: {err}");
                        return;
                    }
                }
            }
        }
    }
    let previous = state.session.clone();
    match load_resumed_session(home, state, &resolved_query) {
        Ok(recap) => {
            if previous.as_deref() != Some(recap.session_id.as_str()) {
                announce(&previous, styled);
            }
            print_resume_recap(&recap, styled, false);
        }
        Err(err) => {
            eprintln!("Could not resume session '{target}': {err}");
        }
    }
}

fn list_sessions(home: Option<&Path>, active: Option<&str>, styled: bool) -> Vec<String> {
    let Some(home) = home else {
        println!("No saved sessions yet.");
        return Vec::new();
    };
    let listed = hermes_runtime::storage::SessionStore::open_read_only(home)
        .and_then(|store| store.recent(20, None));
    match listed {
        Ok(sessions) if sessions.is_empty() => {
            println!("No saved sessions yet.");
            Vec::new()
        }
        Ok(sessions) => {
            let mut ids = Vec::with_capacity(sessions.len());
            for (idx, session) in sessions.into_iter().enumerate() {
                let text = session.title.or(session.preview).unwrap_or_default();
                let preview = approval::sanitize_terminal_text(&text).replace('\n', " ");
                let marker = if active == Some(session.id.as_str()) {
                    " *"
                } else {
                    ""
                };
                if styled {
                    println!(
                        "{BOLD}{:>2}.{RESET} {FG_CYAN}{}{RESET}{FG_GREEN}{marker}{RESET}  {DIM}{:>4} msgs{RESET}  {}",
                        idx + 1,
                        session.id,
                        session.message_count,
                        preview
                    );
                } else {
                    println!(
                        "{:>2}. {}{marker}  {:>4} msgs  {}",
                        idx + 1,
                        session.id,
                        session.message_count,
                        preview
                    );
                }
                ids.push(session.id);
            }
            let hint = format!(
                "Type a number (1..={}) or /resume <id|num> to continue a session.",
                ids.len()
            );
            if styled {
                println!("{DIM}{hint}{RESET}");
            } else {
                println!("{hint}");
            }
            ids
        }
        Err(_) => {
            println!("No saved sessions yet.");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_helper_completes_commands_and_ui_modes() {
        let helper = SlashHelper::new(true);
        let history = rustyline::history::MemHistory::new();
        let ctx = Context::new(&history);

        let (start, pairs) = helper.complete("/mo", 3, &ctx).unwrap();
        assert_eq!(start, 0);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].replacement, "/model");

        let (start, pairs) = helper.complete("/ui t", 5, &ctx).unwrap();
        assert_eq!(start, 4);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].replacement, "tui");

        assert_eq!(helper.hint("/co", 3, &ctx).as_deref(), Some("mpress"));
        assert_eq!(helper.hint("/ui i", 5, &ctx).as_deref(), Some("nline"));
    }

    #[test]
    fn styled_prompt_preserves_contiguous_prompt_marker_bytes() {
        let helper = SlashHelper::new(true);
        let styled = helper.highlight_prompt(PROMPT, true);
        assert!(styled
            .as_bytes()
            .windows(PROMPT.len())
            .any(|w| w == PROMPT.as_bytes()));
    }

    #[test]
    fn banner_includes_profile_provider_model_and_yolo() {
        let plain = format_banner(
            "work",
            "openai",
            "gpt-5.4",
            Some("sess-123456"),
            true,
            false,
        );
        assert!(plain.contains("work"));
        assert!(plain.contains("openai:gpt-5.4"));
        assert!(plain.contains("YOLO"));
        assert!(!plain.contains('\x1b'));

        let colored = format_banner("default", "anthropic", "claude-sonnet-4", None, false, true);
        assert!(colored.contains("Hermes"));
        assert!(colored.contains("claude-sonnet-4"));
        assert!(colored.contains('\x1b'));
    }
}
