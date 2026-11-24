//! Interactive console UI layer: enhanced inline streaming REPL, fullscreen `ratatui` TUI,
//! and automatic plain-text fallback for pipes, `-q`, `--events-json`, `TERM=dumb`, and `NO_COLOR`.

pub mod inline;
pub mod markdown;
pub mod tui;

use std::path::Path;
use std::sync::Mutex;

use hermes_runtime::config::UiModeSetting;
use hermes_runtime::turn::CompactionGuard;

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
                ActiveUi::Inline
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
        name: "/sessions",
        usage: "/sessions",
        description: "list recent sessions",
    },
    SlashCommand {
        name: "/resume",
        usage: "/resume <id>",
        description: "continue another session",
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
  /new                   start a new conversation (the old one stays saved)
  /sessions              list recent sessions
  /resume <id>           continue another session
  /model [name]          show or change the model for the next turns
  /compress [topic]      compress the current session history now
  /yolo                  toggle approving file changes and commands without asking
  /retry                 send the last turn again when it was interrupted or failed
  /ui [inline|tui|plain] show or switch the console interface mode
  /exit, /quit           leave (Ctrl-D also works)
A line ending in \\ continues on the next line. Ctrl-C stops a running turn.";

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
    Usage { input: u64, output: u64 },
    ToolStarted(String),
    ToolFinished { name: String, succeeded: bool },
    Fallback(String),
    ContextCompressed(String),
    IterationLimit(String),
    Other(String),
}

fn unquote_debug_str(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.trim_matches('"').to_owned())
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
    if let Some(raw) = text.strip_prefix("Tool started: ") {
        return ParsedStderr::ToolStarted(unquote_debug_str(raw));
    }
    if let Some(raw) = text.strip_prefix("Tool completed: ") {
        return ParsedStderr::ToolFinished {
            name: unquote_debug_str(raw),
            succeeded: true,
        };
    }
    if let Some(raw) = text.strip_prefix("Tool failed: ") {
        return ParsedStderr::ToolFinished {
            name: unquote_debug_str(raw),
            succeeded: false,
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
    pub session: Option<String>,
    pub unsaved: Option<String>,
    pub scope: String,
    pub guard: Mutex<CompactionGuard>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub last_turn_input_tokens: u64,
    pub exit_code: u8,
    pub color: bool,
    pub profile_label: String,
    /// Conversation history snapshot kept in memory for seamless Inline ↔ TUI transitions.
    pub feed_entries: Vec<FeedEntry>,
}

/// Entry displayed in the TUI conversation feed (and recorded across turns).
#[derive(Clone, Debug)]
pub enum FeedEntry {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        status: ToolStatus,
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
        let profile_label = profile_display_name(home);
        Self {
            request,
            session,
            unsaved: None,
            scope: format!("repl-{}", std::process::id()),
            guard: Mutex::new(CompactionGuard::new()),
            total_input_tokens: 0,
            total_output_tokens: 0,
            last_turn_input_tokens: 0,
            exit_code: 0,
            color,
            profile_label,
            feed_entries: Vec::new(),
        }
    }

    pub fn record_usage(&mut self, input: u64, output: u64) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(input);
        self.total_output_tokens = self.total_output_tokens.saturating_add(output);
        if input > 0 {
            self.last_turn_input_tokens = input;
        }
    }
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

pub fn format_tokens_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
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
        // TTY + Auto + colors => Inline (preserves native scrollback & PTY contracts).
        assert_eq!(
            resolve_ui_mode(
                UiModeSetting::Auto,
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
            ParsedStderr::ToolStarted("read_file".into())
        );
        assert_eq!(
            parse_stderr_event("Tool completed: \"read_file\""),
            ParsedStderr::ToolFinished {
                name: "read_file".into(),
                succeeded: true
            }
        );
        assert_eq!(
            parse_stderr_event("Tool failed: \"terminal\""),
            ParsedStderr::ToolFinished {
                name: "terminal".into(),
                succeeded: false
            }
        );
        assert_eq!(
            parse_stderr_event("Model fallback: \"gpt-4o\""),
            ParsedStderr::Fallback("gpt-4o".into())
        );
    }

    #[test]
    fn matches_slash_commands_by_prefix() {
        let m = match_slash_commands("/co");
        let names: Vec<&str> = m.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/compress", "/compact"]);
        assert!(match_slash_commands("hello").is_empty());
        assert_eq!(match_slash_commands("/ui").len(), 1);
    }
}
