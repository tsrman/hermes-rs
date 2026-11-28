//! Approved foreground commands on the local Linux, macOS, and Windows CLI.

use crate::paths::PathExt;
use std::future::Future;

use serde_json::{json, Value};

use crate::file_tools::{approval_error, ApprovalDecision, ApprovalRequest, FileWorkspace};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

pub(crate) const TERMINAL: &str = "terminal";

/// Which of the user's environment variables a command sees. The toolchain variables below are
/// always passed (a build or a test needs them); `terminal.env_passthrough` adds more by name.
/// The profile's own provider credentials are never passed.
/// `tool_output.*` and `file_read_max_chars`: how much of a tool result the model sees.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OutputLimits {
    /// Characters of command output kept (40% from the start, 60% from the end).
    pub max_bytes: usize,
    /// The most lines one `read_file` call may return.
    pub max_lines: usize,
    /// Longer lines are cut with `... [truncated]`.
    pub max_line_length: usize,
    /// Characters one `read_file` call may return.
    pub file_read_max_chars: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            max_bytes: 50_000,
            max_lines: 2000,
            max_line_length: 2000,
            file_read_max_chars: 100_000,
        }
    }
}

impl OutputLimits {
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Self {
        let defaults = Self::default();
        // Python's `int(value) >= 1`, else the default: numbers truncate, numeric text parses.
        let positive = |value: &serde_yaml::Value, default: usize| {
            let parsed = match value {
                serde_yaml::Value::Number(number) => number.as_i64().or_else(|| {
                    number
                        .as_f64()
                        .filter(|f| f.is_finite())
                        .map(|f| f.trunc() as i64)
                }),
                serde_yaml::Value::String(text) => text.trim().parse::<i64>().ok(),
                serde_yaml::Value::Bool(flag) => Some(i64::from(*flag)),
                _ => None,
            };
            parsed
                .filter(|number| *number >= 1)
                .and_then(|number| usize::try_from(number).ok())
                .unwrap_or(default)
        };
        let section = &config["tool_output"];
        let read_chars = match &config["file_read_max_chars"] {
            serde_yaml::Value::Number(number) => number
                .as_f64()
                .filter(|chars| *chars > 0.0)
                .map(|chars| chars as usize),
            _ => None,
        };
        Self {
            max_bytes: positive(&section["max_bytes"], defaults.max_bytes),
            max_lines: positive(&section["max_lines"], defaults.max_lines),
            max_line_length: positive(&section["max_line_length"], defaults.max_line_length),
            file_read_max_chars: read_chars.unwrap_or(defaults.file_read_max_chars),
        }
    }
}

fn default_local_persistent() -> bool {
    true
}

/// Match the reference's `_tenv_bool("TERMINAL_LOCAL_PERSISTENT", "true")`:
/// unset defaults to `true`; when set, only `"true"`, `"1"`, or `"yes"` (ASCII case-insensitive)
/// enable persistence.
pub fn parse_local_persistent(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(value) => matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TerminalSettings {
    #[serde(default)]
    pub output: OutputLimits,
    /// `terminal.timeout` (or `TERMINAL_TIMEOUT`): the foreground timeout when a call gives none.
    #[serde(default)]
    pub default_timeout: Option<u32>,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// `code_execution.timeout`: seconds one `execute_code` call may run.
    #[serde(default)]
    pub code_timeout: Option<u64>,
    /// `code_execution.max_tool_calls`: tool calls one `execute_code` call may make.
    #[serde(default)]
    pub code_max_tool_calls: Option<usize>,
    /// The profile's scratch directory; commands get it as `TMPDIR`, `TMP` and `TEMP`.
    #[serde(default)]
    pub scratch: Option<std::path::PathBuf>,
    /// `terminal.shell_init_files`: sourced before the first command (see `terminal_init`).
    #[serde(default)]
    pub shell_init_files: Vec<String>,
    /// `terminal.auto_source_bashrc: false` (the reference's default is true).
    #[serde(default)]
    pub no_auto_source_bashrc: bool,
    /// `TERMINAL_LOCAL_PERSISTENT` (default `true`): persist exported shell variables across
    /// commands in the same conversation/session.
    #[serde(default = "default_local_persistent")]
    pub local_persistent: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            output: OutputLimits::default(),
            default_timeout: None,
            env_passthrough: Vec::new(),
            code_timeout: None,
            code_max_tool_calls: None,
            scratch: None,
            shell_init_files: Vec::new(),
            no_auto_source_bashrc: false,
            local_persistent: true,
        }
    }
}

const DEFAULT_ENV: [&str; 22] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "NVM_DIR",
    "PYENV_ROOT",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "JAVA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
];
#[cfg(windows)]
const WINDOWS_DEFAULT_ENV: [&str; 16] = [
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "CommonProgramFiles",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PROCESSOR_ARCHITECTURE",
];
/// Names that carry the profile's model credentials: refused even when listed.
const WITHHELD_ENV: [&str; 8] = [
    "HERMES_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENROUTER_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
];

impl TerminalSettings {
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Self {
        let names = config["terminal"]["env_passthrough"]
            .as_sequence()
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_yaml::Value::as_str)
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let section = &config["code_execution"];
        Self {
            output: OutputLimits::from_config(config),
            default_timeout: std::env::var("TERMINAL_TIMEOUT")
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .or_else(|| {
                    config["terminal"]["timeout"]
                        .as_u64()
                        .and_then(|seconds| u32::try_from(seconds).ok())
                })
                .filter(|seconds| (1..=600).contains(seconds)),
            env_passthrough: names,
            code_timeout: section["timeout"].as_u64().filter(|seconds| *seconds > 0),
            code_max_tool_calls: section["max_tool_calls"]
                .as_u64()
                .filter(|calls| *calls > 0)
                .map(|calls| calls as usize),
            scratch: None,
            shell_init_files: config["terminal"]["shell_init_files"]
                .as_sequence()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_yaml::Value::as_str)
                        .filter(|path| !path.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            no_auto_source_bashrc: !config["terminal"]
                .get("auto_source_bashrc")
                .is_none_or(crate::terminal_init::py_truthy),
            local_persistent: parse_local_persistent(
                std::env::var("TERMINAL_LOCAL_PERSISTENT").ok().as_deref(),
            ),
        }
    }

    /// The variables a command starts with.
    pub(crate) fn environment(&self) -> Vec<(String, String)> {
        let is_withheld = |name: &str| {
            WITHHELD_ENV
                .iter()
                .any(|blocked| blocked.eq_ignore_ascii_case(name))
        };
        let has_var = |out: &[(String, String)], name: &str| {
            if cfg!(windows) {
                out.iter().any(|(seen, _)| seen.eq_ignore_ascii_case(name))
            } else {
                out.iter().any(|(seen, _)| seen == name)
            }
        };
        let mut out: Vec<(String, String)> = Vec::new();
        let base_env = DEFAULT_ENV.iter().copied();
        #[cfg(windows)]
        let base_env = base_env.chain(WINDOWS_DEFAULT_ENV.iter().copied());
        for name in base_env.chain(self.env_passthrough.iter().map(String::as_str)) {
            if is_withheld(name) || has_var(&out, name) {
                continue;
            }
            if let Ok(value) = std::env::var(name) {
                out.push((name.to_owned(), value));
            }
        }
        let names: Vec<String> = DEFAULT_ENV
            .iter()
            .copied()
            .chain(self.env_passthrough.iter().map(String::as_str))
            .filter(|name| !is_withheld(name))
            .map(str::to_owned)
            .collect();
        let mut out = crate::terminal_init::with_init_files(
            out,
            &self.shell_init_files,
            !self.no_auto_source_bashrc,
            &names,
        );
        if let Some(scratch) = &self.scratch {
            for name in ["TMPDIR", "TMP", "TEMP"] {
                if cfg!(windows) {
                    out.retain(|(known, _)| !known.eq_ignore_ascii_case(name));
                } else {
                    out.retain(|(known, _)| known != name);
                }
                out.push((name.to_owned(), scratch.display().to_string()));
            }
        }
        #[cfg(not(windows))]
        {
            if !out.iter().any(|(name, _)| name == "PATH") {
                out.push(("PATH".into(), "/usr/bin:/bin".into()));
            }
            if !out.iter().any(|(name, _)| name == "LANG") {
                out.push((
                    "LANG".into(),
                    if cfg!(target_os = "macos") {
                        "en_US.UTF-8"
                    } else {
                        "C.UTF-8"
                    }
                    .into(),
                ));
            }
        }
        out
    }
}

/// Where each conversation's commands left their working directory.
static WORKING_DIRECTORIES: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Persisted bash `export -p` snapshot per conversation/session (stored purely in memory so
/// exported credentials never touch disk).
static SESSION_SNAPSHOTS: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn tracked_directory(session: &str) -> Option<PathBuf> {
    WORKING_DIRECTORIES.lock().ok()?.get(session).cloned()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn session_snapshot(session: &str) -> Option<Vec<u8>> {
    SESSION_SNAPSHOTS.lock().ok()?.get(session).cloned()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn store_session_snapshot(session: &str, snapshot: Vec<u8>) {
    if let Ok(mut map) = SESSION_SNAPSHOTS.lock() {
        map.insert(session.to_owned(), snapshot);
    }
}

/// Forget where a finished conversation (a subagent or reset REPL) left its working directory
/// and persisted shell environment.
pub fn forget_session(session: &str) {
    track_directory(session, None);
    if let Ok(mut map) = SESSION_SNAPSHOTS.lock() {
        map.remove(session);
    }
}

/// Where a conversation's commands run now: the directory they last ended in, if it is still
/// inside the workspace, else the workspace root.
pub(crate) fn session_directory(session: &str, root: &Path) -> PathBuf {
    tracked_directory(session)
        .filter(|path| path.starts_with(root) && path.is_dir())
        .unwrap_or_else(|| root.to_path_buf())
}

fn track_directory(session: &str, path: Option<PathBuf>) {
    if let Ok(mut map) = WORKING_DIRECTORIES.lock() {
        match path {
            Some(path) => map.insert(session.to_owned(), path),
            None => map.remove(session),
        };
    }
}
pub(crate) const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
));

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute<A, F>(
    workspace: &FileWorkspace<'_>,
    arguments: &str,
    approval_timeout: Option<u32>,
    background: Option<&str>,
    settings: &TerminalSettings,
    session: &str,
    policy: Option<&crate::approval_policy::ApprovalPolicy>,
    live_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    approve: &mut A,
) -> Value
where
    A: FnMut(ApprovalRequest) -> F,
    F: Future<Output = ApprovalDecision>,
{
    let Some(approval_timeout) = approval_timeout.filter(|_| SUPPORTED) else {
        return approval_error(ApprovalDecision::Unavailable);
    };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        shell::execute(
            workspace,
            arguments,
            approval_timeout,
            background,
            settings,
            session,
            policy,
            live_output,
            approve,
        )
        .await
        .unwrap_or_else(|error| json!({"error": error, "exit_code": -1, "status": "error"}))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = background;
        windows_shell::execute(
            windows_shell::ExecContext {
                workspace,
                arguments,
                approval_timeout,
                settings,
                session,
                policy,
                live_output,
            },
            approve,
        )
        .await
        .unwrap_or_else(|error| json!({"error": error, "exit_code": -1, "status": "error"}))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (
            workspace,
            arguments,
            approval_timeout,
            background,
            settings,
            session,
            policy,
            live_output,
            approve,
        );
        approval_error(ApprovalDecision::Unavailable)
    }
}

/// `background` is offered only where `process_manage` is, so that a started process can be
/// managed.
pub(crate) fn schema(background: bool) -> Value {
    let mut properties = json!({
        "command": {"type": "string"}, "workdir": {"type": "string"},
        "timeout": {"type": "integer", "minimum": 1, "maximum": 600, "default": 180}
    });
    if background {
        properties["background"] = json!({"type": "boolean", "default": false,
            "description": "Run the command in the background and return a session_id at once (no timeout). Manage it with process_manage."});
        properties["pty"] = json!({"type": "boolean", "default": false,
            "description": "Allocate a pseudo-terminal (PTY) for a background process so process_manage write/submit/close can send interactive stdin."});
        properties["notify_on_complete"] = json!({"type": "boolean", "default": false,
            "description": "Queue a completion notification into the conversation when this background process exits."});
        properties["watch_patterns"] = json!({"type": "array", "items": {"type": "string"},
            "description": "Substring patterns in background output that trigger rate-limited notifications (falling back to notify_on_complete if noisy)."});
    }
    json!({"type": "function", "function": {
        "name": TERMINAL,
        "description": "Run a local shell command with the user's permissions after approval. Set workdir to a directory in the workspace. The child starts with a minimal environment.",
        "parameters": {"type": "object", "properties": properties,
            "required": ["command"], "additionalProperties": false}
    }})
}

/// Output marker appended by the Unix shell wrapper to detect directory changes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const CWD_MARKER: &str = "__HERMES_CWD__";
const MARKER_SLACK: usize = 8192;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    command: String,
    workdir: Option<String>,
    timeout: Option<u32>,
    background: Option<bool>,
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    pty: Option<bool>,
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    notify_on_complete: Option<bool>,
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    watch_patterns: Option<Vec<String>>,
}

/// The first and the last characters of what a command printed, 40% and 60% of the limit,
/// with a notice between them when the middle was left out.
struct Kept {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    max_chars: usize,
    head_limit: usize,
    tail_limit: usize,
    head: Vec<u8>,
    head_chars: usize,
    head_full: bool,
    tail: std::collections::VecDeque<u8>,
    tail_chars: usize,
    omitted: usize,
    total: usize,
}

impl Default for Kept {
    fn default() -> Self {
        Self::with_limit(OutputLimits::default().max_bytes)
    }
}

impl Kept {
    fn with_limit(max_chars: usize) -> Self {
        let head_limit = (max_chars as f64 * 0.4) as usize;
        Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            max_chars,
            head_limit,
            tail_limit: max_chars - head_limit,
            head: Vec::new(),
            head_chars: 0,
            head_full: false,
            tail: std::collections::VecDeque::new(),
            tail_chars: 0,
            omitted: 0,
            total: 0,
        }
    }

    /// Characters are counted by their first byte, so a character split across two reads
    /// is still one.
    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let starts = byte & 0xC0 != 0x80;
            self.total += usize::from(starts);
            if !self.head_full {
                if starts && self.head_chars == self.head_limit {
                    self.head_full = true;
                } else {
                    self.head.push(byte);
                    self.head_chars += usize::from(starts);
                    continue;
                }
            }
            self.tail.push_back(byte);
            self.tail_chars += usize::from(starts);
            // The tail keeps some slack so the wrapper's final-directory line, which is not
            // output, can be cut off without having eaten real output.
            if self.tail_chars > self.tail_limit + MARKER_SLACK {
                self.tail.pop_front();
                while self.tail.front().is_some_and(|next| next & 0xC0 == 0x80) {
                    self.tail.pop_front();
                }
                self.tail_chars -= 1;
                self.omitted += 1;
            }
        }
    }

    fn omitted(&self) -> usize {
        self.omitted + self.tail_chars.saturating_sub(self.tail_limit)
    }

    /// Cut the wrapper's final-directory line off the end and return the directory.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn take_directory(&mut self) -> Option<String> {
        let needle = format!("\n{CWD_MARKER}").into_bytes();
        let at = |bytes: &[u8]| {
            bytes
                .windows(needle.len())
                .rposition(|window| window == needle)
        };
        if self.omitted == 0 {
            // Everything is still here: rebuild from the text without the line.
            let mut all = self.head.clone();
            all.extend(self.tail.iter());
            let index = at(&all)?;
            let after = &all[index + needle.len()..];
            let line = after.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
            let path = String::from_utf8_lossy(line).into_owned();
            let mut fresh = Self::with_limit(self.max_chars);
            fresh.push(&all[..index]);
            *self = fresh;
            return (!path.is_empty()).then_some(path);
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let index = at(&tail)?;
        let after = &tail[index + needle.len()..];
        let line = after.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
        let path = String::from_utf8_lossy(line).into_owned();
        let removed = tail[index..]
            .iter()
            .filter(|byte| **byte & 0xC0 != 0x80)
            .count();
        self.tail.truncate(index);
        self.tail_chars -= removed;
        self.total -= removed;
        (!path.is_empty()).then_some(path)
    }

    fn render(&self) -> String {
        // Characters beyond the limit (the slack) are omitted too.
        let mut skip = 0;
        let mut dropped = 0;
        let bytes: Vec<u8> = self.tail.iter().copied().collect();
        while self.tail_chars - dropped > self.tail_limit && skip < bytes.len() {
            skip += 1;
            while bytes.get(skip).is_some_and(|next| next & 0xC0 == 0x80) {
                skip += 1;
            }
            dropped += 1;
        }
        let omitted = self.omitted + dropped;
        let mut out = self.head.clone();
        if omitted > 0 {
            out.extend_from_slice(
                format!(
                    "\n\n... [OUTPUT TRUNCATED - {} chars omitted out of {} total] ...\n\n",
                    thousands(omitted),
                    thousands(self.total)
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(&bytes[skip..]);
        String::from_utf8_lossy(&out).into_owned()
    }
}

fn thousands(count: usize) -> String {
    let digits = count.to_string();
    let mut grouped = String::new();
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shell {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::MetadataExt;
    use std::process::{ExitStatus, Stdio};
    use std::time::Duration;

    use rustix::process::{fchdir, kill_process_group, Pid, Signal};
    use tokio::io::{AsyncRead, AsyncReadExt};
    use tokio::process::{Child, Command};
    use tokio::time::timeout;

    use crate::file_tools::{directory::open_directory, request_approval, ApprovalAction};
    use crate::redact::redact_terminal_output;

    pub(super) use super::Kept;
    use super::*;

    /// Private high file descriptors used to pass the persisted bash `export -p` snapshot into and
    /// out of the child process purely in memory (never touching disk). Closed before the user's
    /// command runs via `197<&- 198>&-`.
    const RESTORE_FD: libc::c_int = 197;
    const DUMP_FD: libc::c_int = 198;
    /// Maximum size of a captured shell environment snapshot (16 MiB, matching the reference's
    /// `_MAX_HANDOFF_BYTES`).
    const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

    /// `background` is the owner of processes started here, when `process_manage` is available.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute<A, F>(
        workspace: &FileWorkspace<'_>,
        arguments: &str,
        approval_timeout: u32,
        background: Option<&str>,
        settings: &TerminalSettings,
        session: &str,
        policy: Option<&crate::approval_policy::ApprovalPolicy>,
        live_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        approve: &mut A,
    ) -> Result<Value, String>
    where
        A: FnMut(ApprovalRequest) -> F,
        F: Future<Output = ApprovalDecision>,
    {
        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "terminal expects a command and optional workdir and timeout")?;
        if args.command.trim().is_empty()
            || args.command.len() > 16 * 1024
            || args.command.contains('\0')
        {
            return Err("The command must be nonempty and no larger than 16 KiB".into());
        }
        let owner = if args.background == Some(true) {
            Some(background.ok_or(
                "Background processes need process_manage, which this session does not offer",
            )?)
        } else {
            None
        };
        let seconds = args
            .timeout
            .unwrap_or(settings.default_timeout.unwrap_or(180));
        if !(1..=600).contains(&seconds) {
            return Err("terminal timeout must be between 1 and 600 seconds".into());
        }
        // Without `workdir` a command starts where the last one in this conversation ended.
        let remembered = args
            .workdir
            .is_none()
            .then(|| tracked_directory(session))
            .flatten()
            .and_then(|path| {
                let root = workspace.resolve(".").ok()?.0;
                path.strip_prefix(&root).ok().map(|relative| {
                    if relative.as_os_str().is_empty() {
                        ".".to_owned()
                    } else {
                        relative.display().to_string()
                    }
                })
            });
        let workdir = args
            .workdir
            .as_deref()
            .or(remembered.as_deref())
            .unwrap_or(".");
        let (root, path) = match workspace.resolve(workdir) {
            Ok(found) => found,
            Err(_) if args.workdir.is_none() => {
                // The remembered directory is gone; start from the workspace root again.
                track_directory(session, None);
                workspace.resolve(".")?
            }
            Err(error) => return Err(error),
        };
        let directory = open_directory(&root, &path)?;
        let target = path
            .to_str()
            .ok_or("The working directory must be UTF-8 to display it for approval")?;
        let decision = request_approval(
            ApprovalRequest {
                target: target.to_owned(),
                content: args.command.clone(),
                action: ApprovalAction::RunCommand,
                timeout_secs: approval_timeout,
            },
            policy,
            approve,
        )
        .await;
        if !matches!(decision, ApprovalDecision::Approved) {
            return Ok(approval_error(decision));
        }
        let (current_root, current_path) = workspace.resolve(
            path.strip_prefix(&root)
                .ok()
                .and_then(|relative| relative.to_str())
                .filter(|relative| !relative.is_empty())
                .unwrap_or("."),
        )?;
        let current = open_directory(&current_root, &current_path)?;
        if current_path != path || identity(&current)? != identity(&directory)? {
            return Err("The working directory changed while approval was pending".into());
        }
        if let Some(owner) = owner {
            let pty = args.pty == Some(true);
            let prior = settings
                .local_persistent
                .then(|| session_snapshot(session))
                .flatten();
            let restore_read = match prior {
                Some(bytes) => Some(pipe_with_bytes(bytes).map_err(|error| error.to_string())?),
                None => None,
            };
            let (child, _) = build_command(
                &args.command,
                directory,
                settings,
                false,
                restore_read,
                None,
                pty,
            );
            let opts = crate::process::SpawnOptions {
                pty,
                notify_on_complete: args.notify_on_complete == Some(true),
                watch_patterns: args
                    .watch_patterns
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect(),
            };
            return crate::process::spawn(child, &args.command, target, owner, opts);
        }
        let mut result = run(
            &args.command,
            directory,
            Duration::from_secs(u64::from(seconds)),
            settings,
            session,
            live_output,
        )
        .await?;
        // Remember where the command ended, when that is still inside the workspace.
        let ended = result
            .as_object_mut()
            .and_then(|map| map.remove("_cwd"))
            .and_then(|value| value.as_str().map(PathBuf::from));
        if let Some(ended) = ended {
            let inside = ended
                .canonical()
                .ok()
                .filter(|path| path.starts_with(&root))
                .filter(|path| {
                    path.strip_prefix(&root)
                        .ok()
                        .and_then(|relative| relative.to_str())
                        .is_some_and(|relative| {
                            workspace
                                .resolve(if relative.is_empty() { "." } else { relative })
                                .is_ok()
                        })
                });
            result["cwd"] = json!(inside.as_ref().unwrap_or(&root).display().to_string());
            if inside.is_none() {
                result["note"] = json!("The command ended outside the workspace; the next command starts in the workspace root.");
            }
            if args.workdir.is_none() {
                track_directory(session, inside);
            }
        }
        Ok(result)
    }

    fn identity(directory: &File) -> Result<(u64, u64), String> {
        let metadata = directory.metadata().map_err(|error| error.to_string())?;
        Ok((metadata.dev(), metadata.ino()))
    }

    fn pipe_with_bytes(data: Vec<u8>) -> io::Result<OwnedFd> {
        let (reader, mut writer) = std::io::pipe()?;
        if data.len() <= 4096 {
            let _ = writer.write_all(&data);
            drop(writer);
        } else {
            std::thread::spawn(move || {
                let _ = writer.write_all(&data);
            });
        }
        Ok(OwnedFd::from(reader))
    }

    /// Install `restore` on `RESTORE_FD` and `dump` on `DUMP_FD` inside `pre_exec`, avoiding
    /// fd-number collisions and clearing `FD_CLOEXEC` only on the target descriptors.
    unsafe fn install_child_fds(restore: Option<RawFd>, dump: Option<RawFd>) -> io::Result<()> {
        let mut restore = restore;
        if let (Some(r), Some(_)) = (restore, dump) {
            if r == DUMP_FD {
                let moved = libc::dup(r);
                if moved < 0 {
                    return Err(io::Error::last_os_error());
                }
                let _ = libc::fcntl(moved, libc::F_SETFD, libc::FD_CLOEXEC);
                restore = Some(moved);
            }
        }
        if let Some(d) = dump {
            if d == DUMP_FD {
                if libc::fcntl(d, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(d, DUMP_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if let Some(r) = restore {
            if r == RESTORE_FD {
                if libc::fcntl(r, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(r, RESTORE_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn bash_wrapper_script(
        command: &str,
        has_restore: bool,
        capture_snapshot: bool,
        report_directory: bool,
    ) -> String {
        let escaped = command.replace('\'', "'\\''");
        let mut parts = Vec::new();
        if has_restore {
            parts.push(format!(
                "IFS= builtin read -r -d '' __hermes_snap <&{RESTORE_FD} || :"
            ));
            parts.push("builtin eval \"${__hermes_snap-}\" >/dev/null 2>&1 || true".to_owned());
            parts.push("builtin unset __hermes_snap".to_owned());
        }
        parts.push(
            "export AI_AGENT=\"${AI_AGENT:-hermes-agent}\" HERMES_AGENT=\"${HERMES_AGENT:-true}\""
                .to_owned(),
        );
        parts.push("export GIT_PAGER=\"${GIT_PAGER:-cat}\" PAGER=\"${PAGER:-cat}\"".to_owned());
        let mut closes = String::new();
        if has_restore {
            closes.push_str(&format!(" {RESTORE_FD}<&-"));
        }
        if capture_snapshot {
            closes.push_str(&format!(" {DUMP_FD}>&-"));
        }
        parts.push(format!("eval '{escaped}'{closes}"));
        parts.push("__hermes_ec=$?".to_owned());
        parts.push("builtin set +e; builtin set +u".to_owned());
        if capture_snapshot {
            // Unset per-session/harness/path-state vars in a subshell before `export -p` (matching
            // `base_session_env._export_dump_excluding_session_vars`) and append a NUL byte so the
            // parent only commits complete dumps (never partial output or an early `exit N`).
            parts.push(format!(
                "{{ ( builtin unset ${{!HERMES_SESSION_*}} ${{!HERMES_CRON_AUTO_DELIVER_*}} \
                 AI_AGENT HERMES_AGENT HERMES_DELEGATED_CHILD_CONTEXT HERMES_CRON_SESSION \
                 ${{!HERMES_RPC_*}} HERMES_KERNEL_DIR HERMES_UI_SESSION_ID \
                 PWD OLDPWD SHLVL _ 2>/dev/null; \
                 builtin export -n SHELLOPTS BASHOPTS 2>/dev/null; \
                 builtin export -p; builtin printf '\\0'; ) >&{DUMP_FD} 2>/dev/null || true; }}"
            ));
        }
        if report_directory {
            parts.push(format!(
                "builtin printf '\\n{CWD_MARKER}%s\\n' \"$(pwd -P)\""
            ));
        }
        parts.push("exit $__hermes_ec".to_owned());
        parts.join("\n")
    }

    /// Build the child command: bash with in-memory snapshot pipes when available, else `/bin/sh`.
    /// Returns `(command, using_bash_snapshot)`.
    fn build_command(
        command: &str,
        directory: File,
        settings: &TerminalSettings,
        report_directory: bool,
        restore_read: Option<OwnedFd>,
        dump_write: Option<OwnedFd>,
        pty: bool,
    ) -> (Command, bool) {
        let bash = crate::terminal_init::find_bash();
        let using_bash = bash.is_some();
        let (program, script, restore_fd, dump_fd) = match bash {
            Some(path) => {
                let script = bash_wrapper_script(
                    command,
                    restore_read.is_some(),
                    dump_write.is_some(),
                    report_directory,
                );
                (path, script, restore_read, dump_write)
            }
            None => {
                let script = if report_directory {
                    format!(
                        "trap 'status=$?; printf \"\\n{CWD_MARKER}%s\\n\" \"$(pwd -P)\"; exit $status' EXIT\n{command}"
                    )
                } else {
                    command.to_owned()
                };
                (PathBuf::from("/bin/sh"), script, None, None)
            }
        };
        let mut child = Command::new(program);
        child
            .args(["-c", &script])
            .env_clear()
            .envs(settings.environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if !pty {
            child.process_group(0);
        }
        // SAFETY: fchdir, dup, dup2, and fcntl are async-signal-safe; the closure owns the open
        // descriptors so the child enters the approved directory and inherits the snapshot pipes
        // before exec.
        unsafe {
            child.pre_exec(move || {
                fchdir(&directory).map_err(io::Error::from)?;
                install_child_fds(
                    restore_fd.as_ref().map(AsRawFd::as_raw_fd),
                    dump_fd.as_ref().map(AsRawFd::as_raw_fd),
                )
            })
        };
        (child, using_bash)
    }

    async fn read_snapshot(mut reader: tokio::net::unix::pipe::Receiver) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() < MAX_SNAPSHOT_BYTES {
                let take = n.min(MAX_SNAPSHOT_BYTES - out.len());
                out.extend_from_slice(&buf[..take]);
            }
        }
    }

    async fn run(
        command: &str,
        directory: File,
        duration: Duration,
        settings: &TerminalSettings,
        session: &str,
        live_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Value, String> {
        // One pipe carries both streams, so output keeps the order the command wrote it in.
        let (reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
        let errors = writer.try_clone().map_err(|error| error.to_string())?;
        let restore_read = if settings.local_persistent {
            match session_snapshot(session) {
                Some(bytes) => Some(pipe_with_bytes(bytes).map_err(|error| error.to_string())?),
                None => None,
            }
        } else {
            None
        };
        let (snap_rx, dump_write) = if settings.local_persistent {
            let (s_read, s_write) = std::io::pipe().map_err(|error| error.to_string())?;
            let rx = tokio::net::unix::pipe::Receiver::from_file(File::from(OwnedFd::from(s_read)))
                .map_err(|error| error.to_string())?;
            (Some(rx), Some(OwnedFd::from(s_write)))
        } else {
            (None, None)
        };
        let (mut child, using_bash) = build_command(
            command,
            directory,
            settings,
            true,
            restore_read,
            dump_write,
            false,
        );
        child
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(errors));
        let spawned = child.spawn();
        drop(child);
        let child = spawned.map_err(|error| error.to_string())?;
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or("Missing process group ID")?;
        let mut group = ProcessGroup {
            child,
            pid: Some(pid),
        };
        let receiver =
            tokio::net::unix::pipe::Receiver::from_file(File::from(OwnedFd::from(reader)))
                .map_err(|error| error.to_string())?;
        let mut kept = Kept::with_limit(settings.output.max_bytes);
        let result = timeout(duration, async {
            let snap_fut = async {
                match snap_rx {
                    Some(rx) if using_bash => read_snapshot(rx).await.map(Some),
                    _ => Ok(None),
                }
            };
            tokio::try_join!(
                group.wait(),
                capture(receiver, &mut kept, command, live_output.as_ref()),
                snap_fut
            )
        })
        .await;
        match result {
            Ok(Ok((status, (), captured_snapshot))) => {
                if let Some(mut snap) = captured_snapshot {
                    // Only commit dumps closed by the trailing NUL sentinel (an early `exit N`
                    // skips the dump and preserves the prior session snapshot).
                    if snap.last() == Some(&0) {
                        snap.pop();
                        store_session_snapshot(session, snap);
                    }
                }
                let ended = kept.take_directory();
                let text = kept.render();
                let text = redact_terminal_output(&text, Some(command), &[]);
                let mut result = json!({"output": text, "exit_code": status.code().unwrap_or(-1),
                           "status": "completed", "truncated": kept.omitted() > 0});
                if let Some(ended) = ended {
                    result["_cwd"] = json!(ended);
                }
                Ok(result)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => {
                group.stop();
                let _ = group.child.wait().await;
                kept.take_directory();
                let text = kept.render();
                Ok(
                    json!({"error": "The command timed out", "exit_code": -1, "status": "timeout",
                           "output": redact_terminal_output(&text, Some(command), &[])}),
                )
            }
        }
    }

    struct ProcessGroup {
        child: Child,
        pid: Option<Pid>,
    }

    impl ProcessGroup {
        fn stop(&mut self) {
            if let Some(pid) = self.pid.take() {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }

        async fn wait(&mut self) -> io::Result<ExitStatus> {
            let status = self.child.wait().await?;
            // A command can fork a child that still holds the output pipes.
            self.stop();
            Ok(status)
        }
    }

    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            self.stop();
        }
    }

    async fn capture(
        mut reader: impl AsyncRead + Unpin,
        kept: &mut Kept,
        command: &str,
        live_output: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> io::Result<()> {
        let mut buffer = [0; 8192];
        let mut last_emit = std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            kept.push(&buffer[..count]);
            if let Some(tx) = live_output {
                let now = std::time::Instant::now();
                if now.duration_since(last_emit) >= Duration::from_millis(60) {
                    let raw = String::from_utf8_lossy(&buffer[..count]);
                    let stripped = crate::session_search::strip_ansi(&raw);
                    let visible = match stripped.find(CWD_MARKER) {
                        Some(idx) => &stripped[..idx],
                        None => stripped.as_str(),
                    };
                    if !visible.trim().is_empty() {
                        let tail = if visible.len() > 1024 {
                            let mut start = visible.len() - 1024;
                            while !visible.is_char_boundary(start) {
                                start += 1;
                            }
                            &visible[start..]
                        } else {
                            visible
                        };
                        let safe = redact_terminal_output(tail, Some(command), &[]);
                        if !safe.is_empty() {
                            let _ = tx.send(safe);
                            last_emit = now;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_shell {
    use std::ffi::c_void;
    use std::fs;
    use std::io;
    use std::os::windows::fs::MetadataExt;
    use std::path::{Component, Path};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncReadExt};
    use tokio::process::Command;
    use tokio::time::timeout;

    use crate::file_tools::{request_approval, ApprovalAction};
    use crate::redact::redact_terminal_output;

    use super::*;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }

    const _: () = assert!(std::mem::size_of::<ThreadEntry32>() == 28);

    pub(super) struct ExecContext<'a> {
        pub(super) workspace: &'a FileWorkspace<'a>,
        pub(super) arguments: &'a str,
        pub(super) approval_timeout: u32,
        pub(super) settings: &'a TerminalSettings,
        pub(super) session: &'a str,
        pub(super) policy: Option<&'a crate::approval_policy::ApprovalPolicy>,
        pub(super) live_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<JobObjectExtendedLimitInformation>() == 144);

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            info_class: u32,
            info: *const c_void,
            info_len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
        fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn OpenThread(desired_access: u32, inherit_handle: i32, thread_id: u32) -> *mut c_void;
        fn ResumeThread(thread: *mut c_void) -> u32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    /// Resume the initial suspended thread(s) of a newly spawned child process after assigning it
    /// to its Job Object so no descendant can ever spawn outside the job.
    fn resume_process_threads(pid: u32) -> io::Result<()> {
        let invalid = (-1isize) as *mut c_void;
        // SAFETY: TH32CS_SNAPTHREAD enumerates threads system-wide; handle is closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot.is_null() || snapshot == invalid {
            return Err(io::Error::last_os_error());
        }
        let mut entry = ThreadEntry32 {
            dw_size: std::mem::size_of::<ThreadEntry32>() as u32,
            cnt_usage: 0,
            th32_thread_id: 0,
            th32_owner_process_id: 0,
            tp_base_pri: 0,
            tp_delta_pri: 0,
            dw_flags: 0,
        };
        let mut resumed_any = false;
        let mut last_err: Option<io::Error> = None;
        // SAFETY: `snapshot` is valid and `entry.dw_size` is initialized to `size_of::<ThreadEntry32>()`.
        let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
        while ok != 0 {
            if entry.th32_owner_process_id == pid {
                // SAFETY: `OpenThread` returns null on failure; valid handles are closed below.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32_thread_id) };
                if thread.is_null() {
                    last_err = Some(io::Error::last_os_error());
                } else {
                    // SAFETY: `thread` is an open handle with THREAD_SUSPEND_RESUME access.
                    let prev = unsafe { ResumeThread(thread) };
                    unsafe {
                        let _ = CloseHandle(thread);
                    }
                    if prev == u32::MAX {
                        last_err = Some(io::Error::last_os_error());
                    } else {
                        resumed_any = true;
                    }
                }
            }
            entry.dw_size = std::mem::size_of::<ThreadEntry32>() as u32;
            // SAFETY: `snapshot` and `entry` remain valid throughout enumeration.
            ok = unsafe { Thread32Next(snapshot, &mut entry) };
        }
        // SAFETY: `snapshot` is closed exactly once.
        unsafe {
            let _ = CloseHandle(snapshot);
        }
        if resumed_any {
            Ok(())
        } else {
            Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no suspended thread found for spawned process",
                )
            }))
        }
    }

    /// Win32 Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Terminates the
    /// entire child process tree (shell + all descendants) on completion, timeout, cancellation,
    /// or parent crash.
    struct JobObject {
        handle: *mut c_void,
    }

    // SAFETY: Win32 Job Object handles are thread-safe kernel handles; `CloseHandle` only runs
    // exclusively in `Drop` (`&mut self`).
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        fn create() -> io::Result<Self> {
            // SAFETY: null security attributes and null name create an anonymous job object.
            let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self { handle };
            let mut info = JobObjectExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` matches Win32 `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` for class 9.
            let ok = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        fn assign(&self, process_handle: *mut c_void) -> io::Result<()> {
            // SAFETY: `self.handle` is a valid job object and `process_handle` is a live child handle.
            let ok = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn terminate(&self) {
            if !self.handle.is_null() {
                // SAFETY: `self.handle` is an open job object owned exclusively by `self`.
                unsafe {
                    let _ = TerminateJobObject(self.handle, 1);
                }
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                self.terminate();
                // SAFETY: `self.handle` is closed exactly once on drop.
                unsafe {
                    let _ = CloseHandle(self.handle);
                }
                self.handle = std::ptr::null_mut();
            }
        }
    }

    fn verify_workspace_dir(root: &Path, path: &Path) -> Result<PathBuf, String> {
        let root_canon = root.canonical().map_err(|e| e.to_string())?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "The working directory is outside the workspace")?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err("The working directory must not traverse parent components".into());
            };
            current.push(name);
            let meta = fs::symlink_metadata(&current).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink()
                || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            {
                return Err("Refusing to run in a symlink or junction directory".into());
            }
            if !meta.is_dir() {
                return Err("The working directory is not a directory".into());
            }
        }
        let canon = current.canonical().map_err(|e| e.to_string())?;
        if !canon.starts_with(&root_canon) {
            return Err("The working directory is outside the workspace".into());
        }
        Ok(canon)
    }

    pub(super) async fn execute<A, F>(
        ctx: ExecContext<'_>,
        approve: &mut A,
    ) -> Result<Value, String>
    where
        A: FnMut(ApprovalRequest) -> F,
        F: Future<Output = ApprovalDecision>,
    {
        let ExecContext {
            workspace,
            arguments,
            approval_timeout,
            settings,
            session,
            policy,
            live_output,
        } = ctx;
        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "terminal expects a command and optional workdir and timeout")?;
        if args.command.trim().is_empty()
            || args.command.len() > 16 * 1024
            || args.command.contains('\0')
        {
            return Err("The command must be nonempty and no larger than 16 KiB".into());
        }
        if args.background == Some(true) {
            return Err(
                "Background processes need process_manage, which this session does not offer"
                    .into(),
            );
        }
        let seconds = args
            .timeout
            .unwrap_or(settings.default_timeout.unwrap_or(180));
        if !(1..=600).contains(&seconds) {
            return Err("terminal timeout must be between 1 and 600 seconds".into());
        }

        let remembered = args
            .workdir
            .is_none()
            .then(|| tracked_directory(session))
            .flatten()
            .and_then(|path| {
                let root = workspace.resolve(".").ok()?.0;
                path.strip_prefix(&root).ok().map(|relative| {
                    if relative.as_os_str().is_empty() {
                        ".".to_owned()
                    } else {
                        relative.display().to_string()
                    }
                })
            });
        let workdir = args
            .workdir
            .as_deref()
            .or(remembered.as_deref())
            .unwrap_or(".");
        let (root, path) = match workspace.resolve(workdir) {
            Ok(found) => found,
            Err(_) if args.workdir.is_none() => {
                track_directory(session, None);
                workspace.resolve(".")?
            }
            Err(error) => return Err(error),
        };
        let verified_before = verify_workspace_dir(&root, &path)?;
        let target = path
            .to_str()
            .ok_or("The working directory must be UTF-8 to display it for approval")?;

        let decision = request_approval(
            ApprovalRequest {
                target: target.to_owned(),
                content: args.command.clone(),
                action: ApprovalAction::RunCommand,
                timeout_secs: approval_timeout,
            },
            policy,
            approve,
        )
        .await;
        if !matches!(decision, ApprovalDecision::Approved) {
            return Ok(approval_error(decision));
        }

        let (current_root, current_path) = workspace.resolve(
            path.strip_prefix(&root)
                .ok()
                .and_then(|relative| relative.to_str())
                .filter(|relative| !relative.is_empty())
                .unwrap_or("."),
        )?;
        let verified_after = verify_workspace_dir(&current_root, &current_path)?;
        if verified_after != verified_before {
            return Err("The working directory changed while approval was pending".into());
        }

        run(
            &args.command,
            &verified_after,
            Duration::from_secs(u64::from(seconds)),
            settings,
            live_output,
        )
        .await
    }

    async fn read_stream(
        mut reader: impl AsyncRead + Unpin,
        kept: Arc<StdMutex<Kept>>,
        command: String,
        live_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> io::Result<()> {
        let mut buffer = [0u8; 8192];
        let mut last_emit = std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            if let Ok(mut guard) = kept.lock() {
                guard.push(&buffer[..count]);
            }
            if let Some(tx) = &live_output {
                let now = std::time::Instant::now();
                if now.duration_since(last_emit) >= Duration::from_millis(60) {
                    let raw = String::from_utf8_lossy(&buffer[..count]);
                    let stripped = crate::session_search::strip_ansi(&raw);
                    if !stripped.trim().is_empty() {
                        let tail = if stripped.len() > 1024 {
                            let mut start = stripped.len() - 1024;
                            while !stripped.is_char_boundary(start) {
                                start += 1;
                            }
                            &stripped[start..]
                        } else {
                            stripped.as_str()
                        };
                        let safe = redact_terminal_output(tail, Some(&command), &[]);
                        if !safe.is_empty() {
                            let _ = tx.send(safe);
                            last_emit = now;
                        }
                    }
                }
            }
        }
    }

    async fn run(
        command: &str,
        directory: &Path,
        duration: Duration,
        settings: &TerminalSettings,
        live_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Value, String> {
        let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let job = JobObject::create().map_err(|e| format!("Failed creating job object: {e}"))?;

        let mut cmd = Command::new(comspec);
        // Use `raw_arg` with `/S` so `cmd.exe` strips only the outer quotes and passes inner
        // quotes verbatim (standard `args()` applies MSVC `\"` escaping, which `cmd.exe` does
        // not unescape).
        cmd.raw_arg(format!("/D /S /C \"{command}\""))
            .current_dir(directory)
            .env_clear()
            .envs(settings.environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let mut job_warning: Option<String> = None;
        if let Some(raw_handle) = child.raw_handle() {
            if let Err(err) = job.assign(raw_handle) {
                job_warning = Some(format!(
                    "Warning: could not assign process to Windows Job Object ({err}); descendant cleanup on timeout is best-effort."
                ));
            }
        }
        if let Some(pid) = child.id() {
            if let Err(err) = resume_process_threads(pid) {
                job.terminate();
                let _ = child.kill().await;
                return Err(format!("Failed resuming spawned Windows command: {err}"));
            }
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let kept = Arc::new(StdMutex::new(Kept::with_limit(settings.output.max_bytes)));

        let out_kept = Arc::clone(&kept);
        let err_kept = Arc::clone(&kept);
        let cmd_out = command.to_owned();
        let cmd_err = command.to_owned();
        let live_out = live_output.clone();
        let live_err = live_output;

        let result = timeout(duration, async {
            let wait_fut = async {
                let status = child.wait().await?;
                // Clean up any stray background descendants holding stdout/stderr handles.
                job.terminate();
                Ok::<_, io::Error>(status)
            };
            let out_fut = async {
                match stdout {
                    Some(reader) => read_stream(reader, out_kept, cmd_out, live_out).await,
                    None => Ok(()),
                }
            };
            let err_fut = async {
                match stderr {
                    Some(reader) => read_stream(reader, err_kept, cmd_err, live_err).await,
                    None => Ok(()),
                }
            };
            tokio::try_join!(wait_fut, out_fut, err_fut)
        })
        .await;

        match result {
            Ok(Ok((status, (), ()))) => {
                let (text, omitted) = {
                    let guard = kept.lock().unwrap_or_else(|e| e.into_inner());
                    (guard.render(), guard.omitted())
                };
                let text = redact_terminal_output(&text, Some(command), &[]);
                let mut payload = json!({
                    "output": text,
                    "exit_code": status.code().unwrap_or(-1),
                    "status": "completed",
                    "truncated": omitted > 0,
                    "cwd": directory.display().to_string(),
                });
                if let Some(note) = job_warning {
                    payload["note"] = json!(note);
                }
                Ok(payload)
            }
            Ok(Err(error)) => {
                job.terminate();
                Err(error.to_string())
            }
            Err(_) => {
                job.terminate();
                let _ = child.kill().await;
                let _ = child.wait().await;
                let text = {
                    let guard = kept.lock().unwrap_or_else(|e| e.into_inner());
                    guard.render()
                };
                Ok(json!({
                    "error": "The command timed out",
                    "exit_code": -1,
                    "status": "timeout",
                    "output": redact_terminal_output(&text, Some(command), &[]),
                }))
            }
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    use super::*;

    async fn run_win(root: &Path, arguments: Value, decision: ApprovalDecision) -> Value {
        let workspace = FileWorkspace::new(root, None);
        let mut approve = move |_request: ApprovalRequest| {
            let decision = decision.clone();
            async move { decision }
        };
        execute(
            &workspace,
            &arguments.to_string(),
            Some(5),
            None,
            &TerminalSettings::default(),
            &format!("win-terminal-test-{}", root.display()),
            None,
            None,
            &mut approve,
        )
        .await
    }

    #[tokio::test]
    async fn approved_command_runs_and_denied_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let denied = run_win(
            dir.path(),
            json!({"command": "echo should-not-run > marker.txt"}),
            ApprovalDecision::Denied,
        )
        .await;
        assert_eq!(denied["status"], "approval_denied");
        assert!(!dir.path().join("marker.txt").exists());

        let ok = run_win(
            dir.path(),
            json!({"command": "echo windows-terminal-ok"}),
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(ok["status"], "completed", "{ok}");
        assert_eq!(ok["exit_code"], 0);
        assert!(ok["output"]
            .as_str()
            .unwrap_or("")
            .contains("windows-terminal-ok"));
    }

    #[tokio::test]
    async fn quoted_arguments_and_spaced_paths_reach_programs_intact() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("spaced dir");
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::write(spaced.join("sample.txt"), "hello world\nother line\n").unwrap();

        let found = run_win(
            dir.path(),
            json!({"command": "findstr /C:\"hello world\" \"spaced dir\\sample.txt\""}),
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(found["status"], "completed", "{found}");
        assert_eq!(found["exit_code"], 0, "{found}");
        let output = found["output"].as_str().unwrap_or("");
        assert!(output.contains("hello world"), "{found}");
        assert!(!output.contains("other line"), "{found}");

        let echoed = run_win(
            dir.path(),
            json!({"command": "echo \"quoted phrase\""}),
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(echoed["status"], "completed", "{echoed}");
        let out = echoed["output"].as_str().unwrap_or("");
        assert!(out.contains("\"quoted phrase\""), "{echoed}");
        assert!(
            !out.contains("\\\""),
            "quotes must not be escaped: {echoed}"
        );
    }

    #[tokio::test]
    async fn timeout_terminates_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let res = run_win(
            dir.path(),
            json!({"command": "ping -n 20 127.0.0.1 >nul", "timeout": 1}),
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(res["status"], "timeout", "{res}");
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    async fn run(
        root: &std::path::Path,
        arguments: Value,
        background: Option<&str>,
        decision: ApprovalDecision,
    ) -> Value {
        let workspace = FileWorkspace::new(root, None);
        let mut approve = move |_request: ApprovalRequest| {
            let decision = decision.clone();
            async move { decision }
        };
        execute(
            &workspace,
            &arguments.to_string(),
            Some(5),
            background,
            &TerminalSettings::default(),
            // One conversation per test directory, so tests running together do not share a
            // remembered working directory.
            &format!("terminal-test-{}", root.display()),
            None,
            None,
            &mut approve,
        )
        .await
    }

    #[derive(serde::Deserialize)]
    struct Truncation {
        text: String,
        max: usize,
        expected: String,
    }

    #[derive(serde::Deserialize)]
    struct LimitCase {
        value: serde_json::Value,
        expected: usize,
    }

    #[derive(serde::Deserialize)]
    struct Recorded {
        truncation: Vec<Truncation>,
        limits: Vec<LimitCase>,
    }

    fn recorded() -> Recorded {
        serde_json::from_str(include_str!("../tests/fixtures/truncation_cases.json")).unwrap()
    }

    #[test]
    fn kept_output_equals_the_reference_head_tail_truncation_for_any_chunking() {
        for case in recorded().truncation {
            for chunk in [1usize, 3, 7, 64, 100_000] {
                let mut kept = shell::Kept::with_limit(case.max);
                for piece in case.text.as_bytes().chunks(chunk) {
                    kept.push(piece);
                }
                assert_eq!(
                    kept.render(),
                    case.expected,
                    "max {} chunk {chunk}",
                    case.max
                );
            }
        }
    }

    #[test]
    fn limit_values_coerce_like_the_reference() {
        for case in recorded().limits {
            let config = serde_yaml::to_value(serde_json::json!({
                "tool_output": {"max_bytes": case.value}
            }))
            .unwrap();
            assert_eq!(
                OutputLimits::from_config(&config).max_bytes,
                case.expected,
                "{:?}",
                case.value
            );
        }
    }

    #[tokio::test]
    async fn a_background_command_runs_after_approval_and_is_managed_by_session_id() {
        let directory = tempfile::tempdir().unwrap();
        let started = run(
            directory.path(),
            json!({"command": "echo started in $(pwd | xargs basename); exit 2", "background": true}),
            Some("terminal-test"),
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(started["output"], "Background process started");
        assert_eq!(started["exit_code"], 0);
        assert!(started["hint"].as_str().unwrap().contains("SILENTLY"));
        let id = started["session_id"].as_str().unwrap();
        let waited = crate::process::call(
            "terminal-test",
            &json!({"action": "wait", "session_id": id, "timeout": 10}).to_string(),
        )
        .await;
        assert_eq!(waited["exit_code"], 2);
        let base = directory.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(waited["output"], format!("started in {base}\n"));
    }

    #[tokio::test]
    async fn a_background_command_needs_process_manage_and_approval() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let command = json!({"command": format!("touch {}", marker.display()), "background": true});
        let unmanaged = run(
            directory.path(),
            command.clone(),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert!(unmanaged["error"]
            .as_str()
            .unwrap()
            .contains("process_manage"));
        let denied = run(
            directory.path(),
            command,
            Some("terminal-test"),
            ApprovalDecision::Denied,
        )
        .await;
        assert!(denied.get("session_id").is_none(), "{denied}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn foreground_output_is_redacted_by_what_the_command_was() {
        let directory = tempfile::tempdir().unwrap();
        let key = "ghp_abcdefghijklmnop1234567890";
        let plain = run(
            directory.path(),
            json!({"command": format!("echo MAX_TOKENS=100 {key}")}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        // Ordinary output is source: assignments stay, a vendor key is masked.
        assert_eq!(plain["output"], "MAX_TOKENS=100 ghp_ab...7890\n");
        std::fs::write(
            directory.path().join(".env"),
            "SECRET_KEY=Zq8x4kLm2PvN7wRt9s\n",
        )
        .unwrap();
        let dump = run(
            directory.path(),
            json!({"command": "cat .env"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(dump["output"], "SECRET_KEY=***\n");
    }

    #[tokio::test]
    async fn commands_see_the_toolchain_environment_and_remember_the_directory() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("sub/deeper")).unwrap();
        let root = directory.path().canonical().unwrap();
        let env = run(
            directory.path(),
            json!({"command": "echo $HOME; echo $OPENAI_API_KEY; command -v sh"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        let text = env["output"].as_str().unwrap();
        assert!(
            text.starts_with(&std::env::var("HOME").unwrap_or_default()),
            "{text}"
        );
        assert_eq!(env["cwd"], json!(root.display().to_string()));
        // `cd` carries to the next command of the same conversation, which says where it ran.
        let moved = run(
            directory.path(),
            json!({"command": "cd sub/deeper && pwd"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(
            moved["cwd"],
            json!(root.join("sub/deeper").display().to_string())
        );
        let next = run(
            directory.path(),
            json!({"command": "pwd"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(
            next["output"],
            format!("{}\n", root.join("sub/deeper").display())
        );
        // An explicit workdir applies to that command only.
        let other = run(
            directory.path(),
            json!({"command": "pwd", "workdir": "sub"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(other["output"], format!("{}\n", root.join("sub").display()));
        let still = run(
            directory.path(),
            json!({"command": "pwd"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(
            still["output"],
            format!("{}\n", root.join("sub/deeper").display())
        );
        // Leaving the workspace is reported and forgotten.
        let outside = run(
            directory.path(),
            json!({"command": "cd / && pwd"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert!(outside["note"]
            .as_str()
            .unwrap()
            .contains("outside the workspace"));
        let back = run(
            directory.path(),
            json!({"command": "pwd"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        assert_eq!(back["output"], format!("{}\n", root.display()));
    }

    #[tokio::test]
    async fn long_output_keeps_its_start_and_its_end() {
        let directory = tempfile::tempdir().unwrap();
        let result = run(
            directory.path(),
            json!({"command": "i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done; echo the-end >&2; exit 3"}),
            None,
            ApprovalDecision::Approved,
        )
        .await;
        let text = result["output"].as_str().unwrap();
        assert_eq!(result["exit_code"], 3);
        assert_eq!(result["truncated"], true);
        assert!(text.starts_with("line-0\n"));
        assert!(text.contains("... [OUTPUT TRUNCATED - "));
        assert!(text.contains(" chars omitted out of "));
        assert!(
            text.trim_end().ends_with("the-end"),
            "{}",
            &text[text.len() - 40..]
        );
    }

    async fn run_with_settings(
        root: &std::path::Path,
        arguments: Value,
        background: Option<&str>,
        settings: &TerminalSettings,
        session: &str,
    ) -> Value {
        let workspace = FileWorkspace::new(root, None);
        let mut approve =
            move |_request: ApprovalRequest| async move { ApprovalDecision::Approved };
        execute(
            &workspace,
            &arguments.to_string(),
            Some(5),
            background,
            settings,
            session,
            None,
            None,
            &mut approve,
        )
        .await
    }

    #[derive(serde::Deserialize)]
    struct BoolCase {
        value: Option<String>,
        expected: bool,
    }

    #[derive(serde::Deserialize)]
    struct PersistentStep {
        command: String,
        output: String,
        returncode: i64,
        cwd: String,
    }

    #[derive(serde::Deserialize)]
    struct PersistentScenario {
        name: String,
        persistent: bool,
        steps: Vec<PersistentStep>,
    }

    #[derive(serde::Deserialize)]
    struct PersistentCorpus {
        bool_cases: Vec<BoolCase>,
        scenarios: Vec<PersistentScenario>,
    }

    #[test]
    fn terminal_local_persistent_bool_parsing_matches_reference() {
        let corpus: PersistentCorpus = serde_json::from_str(include_str!(
            "../tests/fixtures/local_persistent_cases.json"
        ))
        .unwrap();
        assert!(!corpus.bool_cases.is_empty());
        for case in corpus.bool_cases {
            assert_eq!(
                parse_local_persistent(case.value.as_deref()),
                case.expected,
                "value={:?}",
                case.value
            );
        }
    }

    #[tokio::test]
    async fn local_persistent_shell_matches_reference_corpus() {
        let corpus: PersistentCorpus = serde_json::from_str(include_str!(
            "../tests/fixtures/local_persistent_cases.json"
        ))
        .unwrap();
        assert!(!corpus.scenarios.is_empty());
        for scenario in corpus.scenarios {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonical().unwrap();
            let root_str = root.display().to_string();
            let session = format!("persistent-corpus-{}-{}", scenario.name, root_str);
            let settings = TerminalSettings {
                local_persistent: scenario.persistent,
                no_auto_source_bashrc: true,
                ..TerminalSettings::default()
            };
            for (idx, step) in scenario.steps.iter().enumerate() {
                let res = run_with_settings(
                    directory.path(),
                    json!({"command": step.command}),
                    None,
                    &settings,
                    &session,
                )
                .await;
                let got_output = res["output"]
                    .as_str()
                    .unwrap_or("")
                    .replace(&root_str, "<ROOT>");
                let got_code = res["exit_code"].as_i64().unwrap_or(-1);
                let got_cwd = res
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| session_directory(&session, &root).display().to_string())
                    .replace(&root_str, "<ROOT>");
                assert_eq!(
                    got_code, step.returncode,
                    "scenario={} step={} cmd={:?} res={res}",
                    scenario.name, idx, step.command
                );
                assert_eq!(
                    got_output, step.output,
                    "scenario={} step={} cmd={:?} res={res}",
                    scenario.name, idx, step.command
                );
                assert_eq!(
                    got_cwd, step.cwd,
                    "scenario={} step={} cmd={:?} res={res}",
                    scenario.name, idx, step.command
                );
            }
            forget_session(&session);
        }
    }

    #[tokio::test]
    async fn persistent_state_isolates_sessions_works_with_background_and_leaves_no_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("sub")).unwrap();
        let settings = TerminalSettings {
            scratch: Some(scratch.path().to_path_buf()),
            local_persistent: true,
            no_auto_source_bashrc: true,
            ..TerminalSettings::default()
        };
        let s1 = format!("iso-a-{}", directory.path().display());
        let s2 = format!("iso-b-{}", directory.path().display());

        // Large export (> 4 KiB) exercises the threaded restore pipe path too.
        let big = "Z".repeat(6000);
        let set = run_with_settings(
            directory.path(),
            json!({"command": format!("export SHARED_VAL=from_s1 BIG_VAL={big}")}),
            None,
            &settings,
            &s1,
        )
        .await;
        assert_eq!(set["exit_code"], 0);

        // Same session sees both exports, even with explicit workdir override, and PWD matches workdir.
        let check_sub = run_with_settings(
            directory.path(),
            json!({"command": "echo \"$SHARED_VAL|${#BIG_VAL}|$(basename \"$PWD\")\"", "workdir": "sub"}),
            None,
            &settings,
            &s1,
        )
        .await;
        assert_eq!(check_sub["output"], "from_s1|6000|sub\n");

        // Another session does NOT see s1's exports.
        let other = run_with_settings(
            directory.path(),
            json!({"command": "echo \"${SHARED_VAL:-none}\""}),
            None,
            &settings,
            &s2,
        )
        .await;
        assert_eq!(other["output"], "none\n");

        // Background process inherits s1's persisted environment.
        let bg = run_with_settings(
            directory.path(),
            json!({"command": "echo \"bg=$SHARED_VAL\"", "background": true}),
            Some(&s1),
            &settings,
            &s1,
        )
        .await;
        let bg_id = bg["session_id"].as_str().unwrap();
        let waited = crate::process::call(
            &s1,
            &json!({"action": "wait", "session_id": bg_id, "timeout": 10}).to_string(),
        )
        .await;
        assert_eq!(waited["output"], "bg=from_s1\n");

        // `set -e` + a readonly excluded var must not abort the dump subshell before `export -p`.
        let set_e_ro = run_with_settings(
            directory.path(),
            json!({"command": "set -e; export AFTER_RO=survived; readonly HERMES_SESSION_RO=1"}),
            None,
            &settings,
            &s1,
        )
        .await;
        assert_eq!(set_e_ro["exit_code"], 0);
        let check_ro = run_with_settings(
            directory.path(),
            json!({"command": "echo \"$AFTER_RO|${HERMES_SESSION_RO:-none}\""}),
            None,
            &settings,
            &s1,
        )
        .await;
        assert_eq!(check_ro["output"], "survived|none\n");

        // `forget_session` clears persisted exports.
        forget_session(&s1);
        let after_forget = run_with_settings(
            directory.path(),
            json!({"command": "echo \"${SHARED_VAL:-cleared}\""}),
            None,
            &settings,
            &s1,
        )
        .await;
        assert_eq!(after_forget["output"], "cleared\n");

        // Zero snapshot files written into scratch/TMPDIR.
        let leftover: Vec<_> = std::fs::read_dir(scratch.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftover.is_empty(),
            "unexpected scratch files: {leftover:?}"
        );
        forget_session(&s2);
    }
}
