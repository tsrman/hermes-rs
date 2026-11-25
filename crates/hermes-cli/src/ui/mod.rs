//! Interactive console UI layer: enhanced inline streaming REPL, fullscreen `ratatui` TUI,
//! and automatic plain-text fallback for pipes, `-q`, `--events-json`, `TERM=dumb`, and `NO_COLOR`.

pub mod clipboard;
pub mod inline;
pub mod markdown;
pub mod tui;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hermes_runtime::config::{ResumeDisplaySettings, UiModeSetting};
use hermes_runtime::storage::{Message, SessionStore};
use hermes_runtime::turn::CompactionGuard;
use serde_json::Value;

use crate::approval;
use crate::chat::ChatRequest;

/// Resolved runtime UI mode for an interactive `hermes chat` session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveUi {
    /// Zero ANSI escapes, exact line-oriented output (pipes, `--ui plain`, `TERM=dumb`, `NO_COLOR` in auto).
    Plain,
    /// Enhanced inline streaming REPL (styled banner, Markdown + syntax highlighting, spinners, `/` completion).
    Inline,
    /// Fullscreen `ratatui` + `crossterm` TUI (message feed, tool cards, status bar, multiline input, modals).
    Tui,
}

impl ActiveUi {
    pub fn label(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Inline => "inline",
            Self::Tui => "tui",
        }
    }
}

/// Resolve the active UI mode from configured setting and terminal environment.
///
/// Safety rule: whenever stdin or stdout is not a TTY, or `TERM=dumb`, always fall back to
/// `ActiveUi::Plain` so pipes, scripts, and CI harnesses never receive raw-mode or ANSI control bytes.
pub fn resolve_ui_mode(
    configured: UiModeSetting,
    stdin_tty: bool,
    stdout_tty: bool,
    no_color: bool,
    term: Option<&str>,
) -> ActiveUi {
    if !stdin_tty || !stdout_tty {
        return ActiveUi::Plain;
    }
    if term.is_some_and(|t| t.trim().eq_ignore_ascii_case("dumb")) {
        return ActiveUi::Plain;
    }
    match configured {
        UiModeSetting::Plain => ActiveUi::Plain,
        UiModeSetting::Auto => {
            if no_color {
                ActiveUi::Plain
            } else {
                ActiveUi::Tui
            }
        }
        UiModeSetting::Inline => ActiveUi::Inline,
        UiModeSetting::Tui => ActiveUi::Tui,
    }
}

/// True when `NO_COLOR` is set in the process environment (per https://no-color.org/).
pub fn env_no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Metadata for a `/` interactive slash command, shared across `/help`, inline completion, and TUI palette.
#[derive(Clone, Copy, Debug)]
pub struct SlashCommand {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        usage: "/help",
        description: "show this list",
    },
    SlashCommand {
        name: "/new",
        usage: "/new",
        description: "start a new conversation (the old one stays saved)",
    },
    SlashCommand {
        name: "/reset",
        usage: "/reset",
        description: "alias for /new",
    },
    SlashCommand {
        name: "/clear",
        usage: "/clear",
        description: "clear the visible screen / conversation feed",
    },
    SlashCommand {
        name: "/undo",
        usage: "/undo",
        description: "remove the last exchange from the active session",
    },
    SlashCommand {
        name: "/skills",
        usage: "/skills [search <kw>]",
        description: "list or search installed skills (invoke any via /<skill>)",
    },
    SlashCommand {
        name: "/sessions",
        usage: "/sessions [id|num]",
        description: "list and select a recent session to resume",
    },
    SlashCommand {
        name: "/resume",
        usage: "/resume [id|num]",
        description: "pick or continue a saved session",
    },
    SlashCommand {
        name: "/model",
        usage: "/model [name]",
        description: "show or change the model for the next turns",
    },
    SlashCommand {
        name: "/compress",
        usage: "/compress [topic]",
        description: "compress the current session history now",
    },
    SlashCommand {
        name: "/compact",
        usage: "/compact [topic]",
        description: "alias for /compress",
    },
    SlashCommand {
        name: "/copy",
        usage: "/copy [last|code|all]",
        description: "copy the last reply, last code block, or full conversation to clipboard",
    },
    SlashCommand {
        name: "/yolo",
        usage: "/yolo",
        description: "toggle approving file changes and commands without asking",
    },
    SlashCommand {
        name: "/retry",
        usage: "/retry",
        description: "send the last turn again when it was interrupted or failed",
    },
    SlashCommand {
        name: "/ui",
        usage: "/ui [inline|tui|plain]",
        description: "show or switch the console interface mode",
    },
    SlashCommand {
        name: "/browser",
        usage: "/browser [connect|status|disconnect]",
        description: "connect, inspect, or disconnect a live CDP browser session",
    },
    SlashCommand {
        name: "/palette",
        usage: "/palette",
        description: "list commands and installed skills (Ctrl-P in TUI)",
    },
    SlashCommand {
        name: "/exit",
        usage: "/exit, /quit",
        description: "leave (Ctrl-D also works)",
    },
    SlashCommand {
        name: "/quit",
        usage: "/quit",
        description: "leave (alias for /exit)",
    },
];

pub const HELP_TEXT: &str = "\
Commands:
  /help                  show this list
  /new, /reset           start a new conversation (the old one stays saved)
  /clear                 clear the visible screen / conversation feed
  /undo                  roll back the last exchange from the active session
  /skills [search <kw>]  list or search installed skills (or run /<skill-name> [args])
  /sessions [id|num]     list and select a recent session to resume
  /resume [id|num]       pick or continue a saved session
  /model [name]          show or change the model for the next turns
  /compress [topic]      compress the current session history now
  /copy [last|code|all]  copy last reply, code block, or transcript to clipboard
  /yolo                  toggle approving file changes and commands without asking
  /retry                 send the last turn again when it was interrupted or failed
  /ui [inline|tui|plain] show or switch the console interface mode
  /browser [connect|status|disconnect] connect or manage a live CDP browser
  /palette               show commands & installed skills (Ctrl-P in TUI)
  /exit, /quit           leave (Ctrl-D also works)

Context references & shortcuts:
  @file:path[:a-b]       attach a file or line range (@folder:dir, @diff, @staged, @git:N, @url:...)
  /<skill-name> [args]   run an installed skill directly from <profile>/skills/
  !<command>             run a local shell command immediately without calling the model
A line ending in \\ continues on the next line. Ctrl-C stops a running turn.";

/// Extract text from conversation feed entries for `/copy [last|code|all]`.
pub fn extract_copy_payload(
    entries: &[FeedEntry],
    arg: &str,
) -> Result<(String, &'static str), String> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "" | "last" => entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                FeedEntry::Assistant(text) => {
                    let trimmed = text.trim();
                    (!trimmed.is_empty()).then(|| (trimmed.to_owned(), "last reply"))
                }
                _ => None,
            })
            .ok_or_else(|| "Nothing to copy yet: no assistant reply in this conversation.".into()),
        "code" => entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                FeedEntry::Assistant(text) => {
                    extract_last_fenced_code_block(text).map(|code| (code, "last code block"))
                }
                _ => None,
            })
            .ok_or_else(|| "No fenced code block found in assistant replies.".into()),
        "all" => {
            let mut blocks = Vec::new();
            for entry in entries {
                match entry {
                    FeedEntry::User(text) if !text.trim().is_empty() => {
                        blocks.push(format!("You:\n{}", text.trim()));
                    }
                    FeedEntry::Assistant(text) if !text.trim().is_empty() => {
                        blocks.push(format!("Hermes:\n{}", text.trim()));
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                Err("Nothing to copy yet: conversation is empty.".into())
            } else {
                Ok((blocks.join("\n\n"), "conversation"))
            }
        }
        other => Err(format!(
            "Unknown /copy target '{other}'. Expected: /copy, /copy last, /copy code, or /copy all."
        )),
    }
}

fn extract_last_fenced_code_block(markdown: &str) -> Option<String> {
    let mut in_code = false;
    let mut current: Vec<&str> = Vec::new();
    let mut last: Option<String> = None;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                in_code = false;
                let block = current.join("\n");
                if !block.is_empty() {
                    last = Some(block);
                }
                current.clear();
            } else {
                in_code = true;
                current.clear();
            }
        } else if in_code {
            current.push(line);
        }
    }
    if in_code && !current.is_empty() {
        last = Some(current.join("\n"));
    }
    last
}

/// Find the active `@...` token ending at `cursor` (ignoring emails like `user@host`).
pub fn active_at_token(input: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = input.get(..cursor)?;
    let start = before.rfind(char::is_whitespace).map_or(0, |idx| {
        idx + before[idx..].chars().next().map_or(1, char::len_utf8)
    });
    let token = &before[start..];
    if token.starts_with('@') {
        Some((start, token))
    } else {
        None
    }
}

/// Format `/skills [search <kw>]` output cleanly for both inline REPL and TUI.
pub fn format_skills_list(home: Option<&Path>, argument: &str) -> String {
    let skills = hermes_runtime::context_refs::list_skill_commands(home);
    if skills.is_empty() {
        return "No skills installed in this profile. Run `hsr skills sync` to download official bundled skills.".into();
    }
    let filter = argument
        .trim()
        .strip_prefix("search")
        .unwrap_or(argument)
        .trim()
        .to_ascii_lowercase();
    let matched: Vec<_> = skills
        .into_iter()
        .filter(|s| {
            filter.is_empty()
                || s.name.contains(&filter)
                || s.rel_path.to_ascii_lowercase().contains(&filter)
                || s.description.to_ascii_lowercase().contains(&filter)
        })
        .collect();
    if matched.is_empty() {
        return format!("No installed skills matching '{filter}'.");
    }
    let mut lines = vec![format!(
        "Installed skills ({}) — invoke with /<name> [prompt]:",
        matched.len()
    )];
    for s in matched {
        lines.push(format!("  /{:<20} {}", s.name, s.description));
    }
    lines.join("\n")
}

/// Execute `!<command>` directly in `root` and return `(is_error, display_text)`.
pub fn format_shell_escape(root: &Path, command: &str) -> (bool, String) {
    match hermes_runtime::context_refs::run_shell_escape(root, command) {
        Ok(out) => {
            let mut body = String::new();
            if !out.stdout.trim_end().is_empty() {
                body.push_str(out.stdout.trim_end());
            }
            if !out.stderr.trim_end().is_empty() {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(out.stderr.trim_end());
            }
            if body.is_empty() {
                body = format!("(exit {})", out.exit_code);
            } else if out.exit_code != 0 {
                body.push_str(&format!("\n(exit {})", out.exit_code));
            }
            (out.exit_code != 0, body)
        }
        Err(err) => (true, err),
    }
}

/// Return slash commands matching the typed prefix (e.g. `"/mo"` -> `[/model]`).
pub fn match_slash_commands(prefix: &str) -> Vec<&'static SlashCommand> {
    let trimmed = prefix.trim_start();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }
    let cmd_part = trimmed
        .split_once(char::is_whitespace)
        .map_or(trimmed, |(c, _)| c)
        .to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(&cmd_part))
        .collect()
}

/// Structured classification of `Event::Stderr` lines emitted by `chat::execute_with_guard`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedStderr {
    Session(String),
    Usage {
        input: u64,
        output: u64,
    },
    ContextUsage {
        tokens: u64,
        estimated: bool,
    },
    ToolStarted {
        name: String,
        detail: Option<String>,
    },
    ToolOutput {
        name: String,
        text: String,
    },
    ToolFinished {
        name: String,
        succeeded: bool,
        summary: Option<String>,
    },
    Reasoning(String),
    Fallback(String),
    ContextCompressed(String),
    IterationLimit(String),
    Other(String),
}

/// Append a raw live-output or reasoning chunk into a rolling tail buffer of at most `max_lines`
/// non-empty sanitized lines, preserving an in-progress partial line across chunk boundaries and
/// handling `\r` progress overwrites cleanly.
pub fn append_preview_lines(
    lines: &mut Vec<String>,
    partial: &mut String,
    chunk: &str,
    max_lines: usize,
) {
    let max_lines = max_lines.max(1);
    for ch in chunk.chars() {
        match ch {
            '\n' => {
                let cleaned = approval::sanitize_terminal_text(partial.trim());
                partial.clear();
                if !cleaned.is_empty() {
                    lines.push(cleaned);
                    if lines.len() > max_lines {
                        let excess = lines.len() - max_lines;
                        lines.drain(..excess);
                    }
                }
            }
            '\r' => {
                partial.clear();
            }
            c => {
                if partial.len() < 400 {
                    partial.push(c);
                }
            }
        }
    }
}

/// Snapshot the latest `max_lines` lines from `(lines, partial)` for live rendering.
pub fn preview_tail_lines(lines: &[String], partial: &str, max_lines: usize) -> Vec<String> {
    let mut out: Vec<String> = lines.to_vec();
    let trimmed = approval::sanitize_terminal_text(partial.trim());
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    if out.len() > max_lines {
        out.drain(..out.len() - max_lines);
    }
    out
}

/// Split an in-flight streaming assistant buffer that begins with an inline `<think>` / `<thought>`
/// / `<thinking>` / `<reasoning>` tag into `(active_reasoning_preview_lines, visible_answer)`.
pub fn split_streaming_think(raw: &str, max_lines: usize) -> (Vec<String>, &str) {
    let trimmed = raw.trim_start();
    for (open, close) in [
        ("<think>", "</think>"),
        ("<thought>", "</thought>"),
        ("<thinking>", "</thinking>"),
        ("<reasoning>", "</reasoning>"),
    ] {
        if let Some(after_open) = trimmed.strip_prefix(open) {
            if let Some((_inside, rest)) = after_open.split_once(close) {
                return (Vec::new(), rest.trim_start_matches(['\r', '\n', ' ']));
            }
            let mut lines = Vec::new();
            let mut partial = String::new();
            append_preview_lines(&mut lines, &mut partial, after_open, max_lines);
            return (preview_tail_lines(&lines, &partial, max_lines), "");
        }
    }
    (Vec::new(), raw)
}

fn unquote_debug_str(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.trim_matches('"').to_owned())
}

fn split_name_and_extra(raw: &str) -> (String, Option<String>) {
    match raw.split_once('\t') {
        Some((name, extra)) => {
            let extra = extra.trim();
            (
                unquote_debug_str(name),
                (!extra.is_empty()).then(|| extra.to_owned()),
            )
        }
        None => (unquote_debug_str(raw), None),
    }
}

pub fn parse_stderr_event(text: &str) -> ParsedStderr {
    if let Some(id) = text.strip_prefix("Session: ") {
        return ParsedStderr::Session(id.to_owned());
    }
    if let Some(counts) = text.strip_prefix("Usage: ") {
        if let Some((input, output)) = counts
            .split_once(' ')
            .and_then(|(i, o)| Some((i.parse::<u64>().ok()?, o.parse::<u64>().ok()?)))
        {
            return ParsedStderr::Usage { input, output };
        }
    }
    if let Some(ctx) = text.strip_prefix("Context: ") {
        if let Some((tokens, kind)) = ctx.split_once(' ') {
            if let Ok(tokens) = tokens.parse::<u64>() {
                return ParsedStderr::ContextUsage {
                    tokens,
                    estimated: kind.trim() == "est",
                };
            }
        }
    }
    if let Some(raw) = text.strip_prefix("Tool started: ") {
        let (name, detail) = split_name_and_extra(raw);
        return ParsedStderr::ToolStarted { name, detail };
    }
    if let Some(raw) = text.strip_prefix("Tool output: ") {
        if let Some((name_raw, chunk_raw)) = raw.split_once('\t') {
            let name = unquote_debug_str(name_raw);
            let chunk =
                serde_json::from_str::<String>(chunk_raw).unwrap_or_else(|_| chunk_raw.to_owned());
            return ParsedStderr::ToolOutput { name, text: chunk };
        }
    }
    if let Some(raw) = text.strip_prefix("Reasoning: ") {
        let chunk = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_owned());
        return ParsedStderr::Reasoning(chunk);
    }
    if let Some(raw) = text.strip_prefix("Tool completed: ") {
        let (name, summary) = split_name_and_extra(raw);
        return ParsedStderr::ToolFinished {
            name,
            succeeded: true,
            summary,
        };
    }
    if let Some(raw) = text.strip_prefix("Tool failed: ") {
        let (name, summary) = split_name_and_extra(raw);
        return ParsedStderr::ToolFinished {
            name,
            succeeded: false,
            summary,
        };
    }
    if let Some(raw) = text.strip_prefix("Model fallback: ") {
        return ParsedStderr::Fallback(unquote_debug_str(raw));
    }
    if text.starts_with("Context window exceeded:") || text.starts_with("Context compression ") {
        return ParsedStderr::ContextCompressed(text.to_owned());
    }
    if text.starts_with("Iteration limit reached") {
        return ParsedStderr::IterationLimit(text.to_owned());
    }
    ParsedStderr::Other(text.to_owned())
}

/// Shared conversation state preserved seamlessly across `/ui` switches.
pub struct ReplState {
    pub request: ChatRequest,
    pub default_model: Option<String>,
    pub session: Option<String>,
    pub unsaved: Option<String>,
    pub scope: String,
    pub guard: Mutex<CompactionGuard>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub last_turn_input_tokens: u64,
    /// Current model context window occupancy (tokens from the most recent API call or estimate).
    pub context_tokens: u64,
    /// True when `context_tokens` is estimated (`~`) rather than provider-reported `prompt_tokens`.
    pub context_estimated: bool,
    /// Number of context compressions performed in this session.
    pub compressions: u32,
    pub exit_code: u8,
    pub color: bool,
    pub profile_label: String,
    /// Conversation history snapshot kept in memory for seamless Inline ↔ TUI transitions.
    pub feed_entries: Vec<FeedEntry>,
    /// Shared prompt input history (loaded from `<home>/.hsr_history`, persisted across sessions
    /// and shared between Inline and TUI modes).
    pub prompt_history: Vec<String>,
}

/// Entry displayed in the TUI conversation feed (and recorded across turns).
#[derive(Clone, Debug)]
pub enum FeedEntry {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        detail: Option<String>,
        status: ToolStatus,
        summary: Option<String>,
        elapsed_ms: Option<u64>,
    },
    Notice {
        kind: NoticeKind,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeKind {
    Info,
    Warn,
    Error,
}

/// Return value from running a UI mode loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiOutcome {
    Exit,
    Switch(ActiveUi),
}

impl ReplState {
    pub fn new(home: Option<&Path>, request: ChatRequest, color: bool) -> Self {
        let session = request.resume().map(str::to_owned);
        let default_model = request.primary_model().map(str::to_owned);
        let profile_label = profile_display_name(home);
        let prompt_history = load_prompt_history(home);
        Self {
            request,
            default_model,
            session,
            unsaved: None,
            scope: format!("repl-{}", std::process::id()),
            guard: Mutex::new(CompactionGuard::new()),
            total_input_tokens: 0,
            total_output_tokens: 0,
            last_turn_input_tokens: 0,
            context_tokens: 0,
            context_estimated: false,
            compressions: 0,
            exit_code: 0,
            color,
            profile_label,
            feed_entries: Vec::new(),
            prompt_history,
        }
    }

    pub fn record_usage(&mut self, input: u64, output: u64) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(input);
        self.total_output_tokens = self.total_output_tokens.saturating_add(output);
        if input > 0 {
            self.last_turn_input_tokens = input;
            if self.context_tokens == 0 {
                self.context_tokens = input;
                self.context_estimated = false;
            }
        }
    }

    pub fn record_context(&mut self, tokens: u64, estimated: bool) {
        self.context_tokens = tokens;
        self.context_estimated = estimated;
    }

    pub fn reset_metrics(&mut self) {
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.last_turn_input_tokens = 0;
        self.context_tokens = 0;
        self.context_estimated = false;
        self.compressions = 0;
    }
}

/// Primary history file path under the selected profile directory.
pub fn history_path(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|home| home.join(".hsr_history"))
}

/// Decode a rustyline `#V2` escaped history line (`\n` -> newline, `\\` -> `\`).
fn unescape_history_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_history_line(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for c in entry.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// Load previous prompt history from `<home>/.hsr_history` (falling back to `.hermes_history`).
pub fn load_prompt_history(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    let primary = home.join(".hsr_history");
    let path = if primary.is_file() {
        primary
    } else {
        let legacy = home.join(".hermes_history");
        if legacy.is_file() {
            legacy
        } else {
            return Vec::new();
        }
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut lines = contents.lines();
    let v2 = lines.clone().next() == Some("#V2");
    if v2 {
        lines.next();
    }
    let mut history: Vec<String> = Vec::new();
    for raw in lines {
        if raw.trim().is_empty() {
            continue;
        }
        let item = if v2 {
            unescape_history_line(raw)
        } else {
            raw.to_owned()
        };
        if history.last().map(String::as_str) != Some(item.as_str()) {
            history.push(item);
        }
    }
    if history.len() > 1000 {
        history.drain(..history.len() - 1000);
    }
    history
}

/// Record a submitted entry into in-memory prompt history and persist it to `<home>/.hsr_history`.
pub fn record_prompt_history(home: Option<&Path>, history: &mut Vec<String>, entry: &str) {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return;
    }
    if history.last().map(String::as_str) == Some(trimmed) {
        return;
    }
    history.push(trimmed.to_owned());
    if history.len() > 1000 {
        history.drain(..history.len() - 1000);
    }
    let Some(path) = history_path(home) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let needs_header = !path.exists() || fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        if needs_header {
            let _ = writeln!(file, "#V2");
        }
        let _ = writeln!(file, "{}", escape_history_line(trimmed));
    }
}

/// One displayable turn entry for a resumed-session recap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecapEntry {
    User(String),
    Assistant { text: String, is_last: bool },
}

/// Summary and displayable dialogue entries produced when resuming a session.
#[derive(Clone, Debug)]
pub struct ResumeRecap {
    pub session_id: String,
    pub title: Option<String>,
    pub user_turns: usize,
    pub total_messages: usize,
    pub skipped_entries: usize,
    pub entries: Vec<RecapEntry>,
}

impl ResumeRecap {
    pub fn headline(&self) -> String {
        let title_part = self
            .title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| format!(" \"{t}\""))
            .unwrap_or_default();
        let noun = if self.user_turns == 1 {
            "user message"
        } else {
            "user messages"
        };
        format!(
            "Resumed session {}{} ({} {noun}, {} total messages)",
            self.session_id, title_part, self.user_turns, self.total_messages
        )
    }
}

fn content_display_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                let kind = part.get("type").and_then(Value::as_str)?;
                match kind {
                    "text" | "input_text" | "output_text" => {
                        part.get("text").and_then(Value::as_str).map(str::to_owned)
                    }
                    "image_url" | "input_image" | "image" => Some("[image]".to_owned()),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn tool_calls_summary(calls: &Value) -> Option<String> {
    let items = calls.as_array()?;
    if items.is_empty() {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    for call in items {
        let name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    let preview = if names.len() > 4 {
        format!("{}, ...", names[..4].join(", "))
    } else {
        names.join(", ")
    };
    let noun = if items.len() == 1 {
        "tool call"
    } else {
        "tool calls"
    };
    Some(format!("[{} {noun}: {preview}]", items.len()))
}

fn strip_reasoning_tags(text: &str) -> String {
    let mut result = text.to_owned();
    for (open, close) in [
        ("<think>", "</think>"),
        ("<thought>", "</thought>"),
        ("<thinking>", "</thinking>"),
        ("<reasoning>", "</reasoning>"),
    ] {
        while let Some(start) = result.find(open) {
            if let Some(end_rel) = result[start + open.len()..].find(close) {
                let end = start + open.len() + end_rel + close.len();
                result.replace_range(start..end, "");
            } else {
                break;
            }
        }
    }
    result.trim().to_owned()
}

fn extract_merged_tail_original(text: &str) -> Option<String> {
    const PRIOR_PREFIX: &str = "[PRIOR CONTEXT — for reference only; not a new message]";
    const PRIOR_END: &str = "[END OF PRIOR CONTEXT — COMPACTION SUMMARY BELOW]";
    if let Some(rest) = text.trim_start().strip_prefix(PRIOR_PREFIX) {
        if let Some((orig, _)) = rest.split_once(PRIOR_END) {
            let trimmed = orig.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    const SUMMARY_END: &str =
        "--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---";
    if let Some((_, tail)) = text.split_once(SUMMARY_END) {
        let trimmed = tail.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

pub fn collect_resume_recap(
    session_id: String,
    title: Option<String>,
    messages: &[Message],
    settings: &ResumeDisplaySettings,
) -> ResumeRecap {
    let user_turns = messages
        .iter()
        .filter(|m| {
            m.role == "user"
                && (!m.compressed_summary
                    || extract_merged_tail_original(&content_display_text(&m.content)).is_some())
        })
        .count();
    let total_messages = messages.len();
    if settings.minimal {
        return ResumeRecap {
            session_id,
            title,
            user_turns,
            total_messages,
            skipped_entries: 0,
            entries: Vec::new(),
        };
    }

    let mut raw_entries: Vec<(bool, String)> = Vec::new();
    let mut last_assistant: Option<(usize, String)> = None;

    for msg in messages {
        if msg.display_kind.as_deref() == Some("hidden") {
            continue;
        }
        if msg.role == "user" {
            let raw = content_display_text(&msg.content);
            let display = if msg.compressed_summary {
                let Some(orig) = extract_merged_tail_original(&raw) else {
                    continue;
                };
                orig
            } else {
                raw
            };
            let text = approval::sanitize_terminal_text(&display);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            raw_entries.push((true, truncate_chars(trimmed, settings.max_user_chars)));
        } else if msg.role == "assistant" {
            let raw = content_display_text(&msg.content);
            let display = if msg.compressed_summary {
                let Some(orig) = extract_merged_tail_original(&raw) else {
                    continue;
                };
                orig
            } else {
                raw
            };
            let stripped = strip_reasoning_tags(&display);
            let fallback_raw = display.trim();
            let effective = if stripped.is_empty() && !fallback_raw.is_empty() {
                fallback_raw
            } else {
                stripped.as_str()
            };
            let cleaned = approval::sanitize_terminal_text(effective);
            let tc_summary = msg.tool_calls.as_ref().and_then(tool_calls_summary);
            if cleaned.is_empty() && (settings.skip_tool_only || tc_summary.is_none()) {
                continue;
            }
            let mut short_parts = Vec::new();
            let mut full_parts = Vec::new();
            if !cleaned.is_empty() {
                full_parts.push(cleaned.clone());
                let lines: Vec<&str> = cleaned.lines().collect();
                let line_clamped = if lines.len() > settings.max_assistant_lines {
                    format!("{} ...", lines[..settings.max_assistant_lines].join("\n"))
                } else {
                    cleaned
                };
                short_parts.push(truncate_chars(&line_clamped, settings.max_assistant_chars));
            }
            if let Some(summary) = tc_summary {
                short_parts.push(summary.clone());
                full_parts.push(summary);
            }
            let idx = raw_entries.len();
            raw_entries.push((false, short_parts.join(" ")));
            last_assistant = Some((idx, full_parts.join(" ")));
        }
    }

    let cap = settings.exchanges.saturating_mul(2).max(2);
    let skipped = raw_entries.len().saturating_sub(cap);
    let mut entries: Vec<RecapEntry> = raw_entries[skipped..]
        .iter()
        .map(|(is_user, text)| {
            if *is_user {
                RecapEntry::User(text.clone())
            } else {
                RecapEntry::Assistant {
                    text: text.clone(),
                    is_last: false,
                }
            }
        })
        .collect();
    if let Some((orig_idx, full_text)) = last_assistant {
        if orig_idx >= skipped {
            let adj = orig_idx - skipped;
            if let Some(slot) = entries.get_mut(adj) {
                *slot = RecapEntry::Assistant {
                    text: full_text,
                    is_last: true,
                };
            }
        }
    }

    ResumeRecap {
        session_id,
        title,
        user_turns,
        total_messages,
        skipped_entries: skipped,
        entries,
    }
}

/// Resolve and load a saved session into `state` (updating `state.session`, restoring the saved
/// model when no explicit CLI override is active, and populating `state.feed_entries` with the
/// recent dialogue recap).
pub fn load_resumed_session(
    home: Option<&Path>,
    state: &mut ReplState,
    target: &str,
) -> Result<ResumeRecap, String> {
    let home = home.ok_or_else(|| "profile root is unavailable".to_owned())?;
    let store = SessionStore::open_read_only(home).map_err(|e| e.to_string())?;
    let resolved_id = store
        .resolve_resume_target(target)
        .map_err(|e| e.to_string())?;
    let transcript = store
        .resume_display_transcript(&resolved_id)
        .map_err(|e| e.to_string())?;
    let recap = collect_resume_recap(
        transcript.session.id.clone(),
        transcript.session.title.clone(),
        &transcript.messages,
        state.request.resume_display(),
    );

    state.session = Some(transcript.session.id.clone());
    state.unsaved = None;
    state.reset_metrics();
    let est_tokens =
        hermes_runtime::model_metadata::estimate_active_messages_tokens(&transcript.messages);
    if est_tokens > 0 {
        state.record_context(est_tokens, true);
    }
    state.guard.lock().unwrap().reset();
    hermes_runtime::terminal::forget_session(&state.scope);
    if !state.request.has_explicit_model_override() {
        if let Some(saved_model) = transcript
            .session
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
        {
            state
                .request
                .set_primary_model(Some(saved_model.to_owned()));
        }
    }

    state.feed_entries.clear();
    state.feed_entries.push(FeedEntry::Notice {
        kind: NoticeKind::Info,
        text: recap.headline(),
    });
    if recap.skipped_entries > 0 {
        state.feed_entries.push(FeedEntry::Notice {
            kind: NoticeKind::Info,
            text: format!("... {} earlier message(s) ...", recap.skipped_entries),
        });
    }
    for entry in &recap.entries {
        match entry {
            RecapEntry::User(text) => state.feed_entries.push(FeedEntry::User(text.clone())),
            RecapEntry::Assistant { text, .. } => {
                state.feed_entries.push(FeedEntry::Assistant(text.clone()))
            }
        }
    }
    Ok(recap)
}

pub fn profile_display_name(home: Option<&Path>) -> String {
    let Some(home) = home else {
        return "ephemeral".to_owned();
    };
    if home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|p| p == "profiles")
    {
        if let Some(name) = home.file_name().and_then(|n| n.to_str()) {
            return name.to_owned();
        }
    }
    "default".to_owned()
}

pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Compact token count matching upstream `cli.py::format_token_count_compact`
/// (e.g. `0`, `850`, `1.2K`, `12.4K`, `128K`, `1.05M`).
pub fn format_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let units = [(1_000_000_000u64, "B"), (1_000_000, "M"), (1_000, "K")];
    for (threshold, suffix) in units {
        if n >= threshold {
            let scaled = n as f64 / threshold as f64;
            let raw = if scaled < 10.0 {
                format!("{scaled:.2}")
            } else if scaled < 100.0 {
                format!("{scaled:.1}")
            } else {
                format!("{scaled:.0}")
            };
            let trimmed = if raw.contains('.') {
                raw.trim_end_matches('0').trim_end_matches('.')
            } else {
                raw.as_str()
            };
            return format!("{trimmed}{suffix}");
        }
    }
    n.to_string()
}

/// Format a context window size matching upstream `hermes_cli/banner.py::_format_context_length`
/// (e.g. `128000` -> `"128K"`, `1048576` -> `"1M"`, `1050000` -> `"1.1M"`).
pub fn format_context_length(tokens: u64) -> String {
    for (unit, div) in [("M", 1_000_000.0), ("K", 1_000.0)] {
        if (tokens as f64) >= div {
            let val = tokens as f64 / div;
            let rounded = val.round();
            if (val - rounded).abs() < 0.05 {
                return format!("{}{unit}", rounded as u64);
            }
            return format!("{val:.1}{unit}");
        }
    }
    tokens.to_string()
}

/// Graphical 10-cell context bar matching upstream `cli_status_bar_mixin.py::_build_context_bar`.
pub fn build_context_bar(percent: u8, width: usize) -> String {
    let safe = percent.min(100);
    let filled = (((safe as f64) / 100.0) * (width as f64))
        .round()
        .clamp(0.0, width as f64) as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

/// Short display name for a model ID matching upstream `cli_status_bar_mixin.py`.
pub fn short_model_label(model: &str) -> String {
    let base = model.rsplit('/').next().unwrap_or(model);
    let stripped = base.strip_suffix(".gguf").unwrap_or(base);
    if stripped.chars().count() > 26 {
        let head: String = stripped.chars().take(23).collect();
        format!("{head}...")
    } else {
        stripped.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_ui_mode_with_safe_plain_fallback() {
        // Non-TTY always falls back to Plain regardless of requested mode.
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Auto,
                false,
                true,
                false,
                Some("xterm-256color")
            ),
            ActiveUi::Plain
        );
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Tui,
                true,
                false,
                false,
                Some("xterm-256color")
            ),
            ActiveUi::Plain
        );
        assert_eq!(
            resolve_ui_mode(UiModeSetting::Inline, false, false, false, None),
            ActiveUi::Plain
        );
        // TERM=dumb always falls back to Plain.
        assert_eq!(
            resolve_ui_mode(UiModeSetting::Tui, true, true, false, Some("dumb")),
            ActiveUi::Plain
        );
        // NO_COLOR causes Auto to fall back to Plain.
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Auto,
                true,
                true,
                true,
                Some("xterm-256color")
            ),
            ActiveUi::Plain
        );
        // TTY + Auto + colors => Tui (default interactive experience).
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Auto,
                true,
                true,
                false,
                Some("xterm-256color")
            ),
            ActiveUi::Tui
        );
        // Explicit Inline on TTY => Inline.
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Inline,
                true,
                true,
                false,
                Some("xterm-256color")
            ),
            ActiveUi::Inline
        );
        // Explicit TUI on TTY => Tui.
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Tui,
                true,
                true,
                false,
                Some("xterm-256color")
            ),
            ActiveUi::Tui
        );
        // Explicit Plain on TTY => Plain.
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Plain,
                true,
                true,
                false,
                Some("xterm-256color")
            ),
            ActiveUi::Plain
        );
    }

    #[test]
    fn parses_stderr_events_accurately() {
        assert_eq!(
            parse_stderr_event("Session: s-123"),
            ParsedStderr::Session("s-123".into())
        );
        assert_eq!(
            parse_stderr_event("Usage: 120 45"),
            ParsedStderr::Usage {
                input: 120,
                output: 45
            }
        );
        assert_eq!(
            parse_stderr_event("Tool started: \"read_file\""),
            ParsedStderr::ToolStarted {
                name: "read_file".into(),
                detail: None,
            }
        );
        assert_eq!(
            parse_stderr_event("Tool started: \"terminal\"\tgit status -sb"),
            ParsedStderr::ToolStarted {
                name: "terminal".into(),
                detail: Some("git status -sb".into()),
            }
        );
        assert_eq!(
            parse_stderr_event("Tool completed: \"read_file\""),
            ParsedStderr::ToolFinished {
                name: "read_file".into(),
                succeeded: true,
                summary: None,
            }
        );
        assert_eq!(
            parse_stderr_event("Tool failed: \"terminal\"\texit 127 · command not found"),
            ParsedStderr::ToolFinished {
                name: "terminal".into(),
                succeeded: false,
                summary: Some("exit 127 · command not found".into()),
            }
        );
        assert_eq!(
            parse_stderr_event("Model fallback: \"gpt-4o\""),
            ParsedStderr::Fallback("gpt-4o".into())
        );
        assert_eq!(
            parse_stderr_event("Context: 12400 exact"),
            ParsedStderr::ContextUsage {
                tokens: 12400,
                estimated: false
            }
        );
        assert_eq!(
            parse_stderr_event("Context: 3200 est"),
            ParsedStderr::ContextUsage {
                tokens: 3200,
                estimated: true
            }
        );
    }

    #[test]
    fn context_bar_and_token_formatters_match_upstream() {
        assert_eq!(format_tokens_compact(0), "0");
        assert_eq!(format_tokens_compact(850), "850");
        assert_eq!(format_tokens_compact(1_200), "1.2K");
        assert_eq!(format_tokens_compact(12_400), "12.4K");
        assert_eq!(format_tokens_compact(128_000), "128K");
        assert_eq!(format_tokens_compact(1_050_000), "1.05M");
        assert_eq!(format_context_length(128_000), "128K");
        assert_eq!(format_context_length(200_000), "200K");
        assert_eq!(format_context_length(1_048_576), "1M");
        assert_eq!(format_context_length(1_050_000), "1.1M");
        assert_eq!(build_context_bar(0, 10), "[░░░░░░░░░░]");
        assert_eq!(build_context_bar(24, 10), "[██░░░░░░░░]");
        assert_eq!(build_context_bar(50, 10), "[█████░░░░░]");
        assert_eq!(build_context_bar(100, 10), "[██████████]");
        assert_eq!(
            short_model_label("anthropic/claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn matches_slash_commands_by_prefix() {
        let m = match_slash_commands("/co");
        let names: Vec<&str> = m.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/compress", "/compact", "/copy"]);
        assert!(match_slash_commands("hello").is_empty());
        assert_eq!(match_slash_commands("/ui").len(), 1);
    }

    #[test]
    fn resume_recap_keeps_merged_tail_and_reasoning_only_last_reply() {
        let messages = vec![
            Message {
                id: 1,
                role: "user".into(),
                content: Value::String("standalone summary".into()),
                tool_call_id: None,
                tool_calls: None,
                tool_name: None,
                timestamp: 1.0,
                finish_reason: None,
                active: true,
                compacted: false,
                display_kind: None,
                display_metadata: None,
                compressed_summary: true,
            },
            Message {
                id: 2,
                role: "user".into(),
                content: Value::String(
                    "[PRIOR CONTEXT — for reference only; not a new message]\n\
                     Original user prompt\n\n\
                     [END OF PRIOR CONTEXT — COMPACTION SUMMARY BELOW]\n\n\
                     Summary\n\n\
                     --- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---"
                        .into(),
                ),
                tool_call_id: None,
                tool_calls: None,
                tool_name: None,
                timestamp: 2.0,
                finish_reason: None,
                active: true,
                compacted: false,
                display_kind: None,
                display_metadata: None,
                compressed_summary: true,
            },
            Message {
                id: 3,
                role: "assistant".into(),
                content: Value::String("<think>final answer inside tags</think>".into()),
                tool_call_id: None,
                tool_calls: None,
                tool_name: None,
                timestamp: 3.0,
                finish_reason: Some("stop".into()),
                active: true,
                compacted: false,
                display_kind: None,
                display_metadata: None,
                compressed_summary: false,
            },
        ];
        let recap = collect_resume_recap(
            "sess-1".into(),
            None,
            &messages,
            &ResumeDisplaySettings::default(),
        );
        assert_eq!(recap.user_turns, 1);
        assert_eq!(
            recap.entries,
            vec![
                RecapEntry::User("Original user prompt".into()),
                RecapEntry::Assistant {
                    text: "<think>final answer inside tags</think>".into(),
                    is_last: true,
                },
            ]
        );
    }

    #[test]
    fn parses_reasoning_and_tool_output_and_keeps_three_preview_lines() {
        assert_eq!(
            parse_stderr_event("Reasoning: \"checking\\nfiles\""),
            ParsedStderr::Reasoning("checking\nfiles".into())
        );
        assert_eq!(
            parse_stderr_event("Tool output: \"terminal\"\t\"step 1\\nstep 2\\n\""),
            ParsedStderr::ToolOutput {
                name: "terminal".into(),
                text: "step 1\nstep 2\n".into(),
            }
        );

        let mut lines = Vec::new();
        let mut partial = String::new();
        append_preview_lines(&mut lines, &mut partial, "line 1\nline 2\n", 3);
        append_preview_lines(
            &mut lines,
            &mut partial,
            "old\rline 3\nline 4\nin progress",
            3,
        );
        assert_eq!(
            preview_tail_lines(&lines, &partial, 3),
            vec!["line 3", "line 4", "in progress"]
        );

        let (think_lines, visible) = split_streaming_think("<think>a\nb\nc\nd", 3);
        assert_eq!(think_lines, vec!["b", "c", "d"]);
        assert_eq!(visible, "");

        let (done_lines, answer) = split_streaming_think("<think>step 1</think>\nFinal answer", 3);
        assert!(done_lines.is_empty());
        assert_eq!(answer, "Final answer");
    }
}
