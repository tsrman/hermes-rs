//! Approved foreground commands on the local Linux and macOS CLI.

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
        let mut out: Vec<(String, String)> = Vec::new();
        for name in DEFAULT_ENV
            .iter()
            .copied()
            .chain(self.env_passthrough.iter().map(String::as_str))
        {
            if WITHHELD_ENV.contains(&name) || out.iter().any(|(seen, _)| seen == name) {
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
            .filter(|name| !WITHHELD_ENV.contains(name))
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
                out.retain(|(known, _)| known != name);
                out.push((name.to_owned(), scratch.display().to_string()));
            }
        }
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
pub(crate) const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

pub(crate) async fn execute<A, F>(
    workspace: &FileWorkspace<'_>,
    arguments: &str,
    approval_timeout: Option<u32>,
    background: Option<&str>,
    settings: &TerminalSettings,
    session: &str,
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
            approve,
        )
        .await
        .unwrap_or_else(|error| json!({"error": error, "exit_code": -1, "status": "error"}))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (
            workspace,
            arguments,
            approval_timeout,
            background,
            settings,
            session,
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shell {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::MetadataExt;
    use std::process::{ExitStatus, Stdio};
    use std::time::Duration;

    use rustix::process::{fchdir, kill_process_group, Pid, Signal};
    use serde::Deserialize;
    use tokio::io::{AsyncRead, AsyncReadExt};
    use tokio::process::{Child, Command};
    use tokio::time::timeout;

    use crate::file_tools::{directory::open_directory, request_approval, ApprovalAction};
    use crate::redact::redact_terminal_output;

    use super::*;

    /// Output kept for the model: the start (what the command announced) and the end (where
    /// errors and results are), with the middle omitted.
    const CWD_MARKER: &str = "__HERMES_CWD__";
    /// Private high file descriptors used to pass the persisted bash `export -p` snapshot into and
    /// out of the child process purely in memory (never touching disk). Closed before the user's
    /// command runs via `197<&- 198>&-`.
    const RESTORE_FD: libc::c_int = 197;
    const DUMP_FD: libc::c_int = 198;
    /// Maximum size of a captured shell environment snapshot (16 MiB, matching the reference's
    /// `_MAX_HANDOFF_BYTES`).
    const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arguments {
        command: String,
        workdir: Option<String>,
        timeout: Option<u32>,
        background: Option<bool>,
        pty: Option<bool>,
        notify_on_complete: Option<bool>,
        watch_patterns: Option<Vec<String>>,
    }

    /// `background` is the owner of processes started here, when `process_manage` is available.
    pub(super) async fn execute<A, F>(
        workspace: &FileWorkspace<'_>,
        arguments: &str,
        approval_timeout: u32,
        background: Option<&str>,
        settings: &TerminalSettings,
        session: &str,
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
        )
        .await?;
        // Remember where the command ended, when that is still inside the workspace.
        let ended = result
            .as_object_mut()
            .and_then(|map| map.remove("_cwd"))
            .and_then(|value| value.as_str().map(PathBuf::from));
        if let Some(ended) = ended {
            let inside = ended
                .canonicalize()
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
            tokio::try_join!(group.wait(), capture(receiver, &mut kept), snap_fut)
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

    /// The first and the last characters of what a command printed, 40% and 60% of the limit,
    /// with a notice between them when the middle was left out.
    pub(super) struct Kept {
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

    const MARKER_SLACK: usize = 8192;

    impl Default for Kept {
        fn default() -> Self {
            Self::with_limit(OutputLimits::default().max_bytes)
        }
    }

    impl Kept {
        pub(super) fn with_limit(max_chars: usize) -> Self {
            let head_limit = (max_chars as f64 * 0.4) as usize;
            Self {
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
        pub(super) fn push(&mut self, bytes: &[u8]) {
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

        pub(super) fn omitted(&self) -> usize {
            self.omitted + self.tail_chars.saturating_sub(self.tail_limit)
        }

        /// Cut the wrapper's final-directory line off the end and return the directory.
        pub(super) fn take_directory(&mut self) -> Option<String> {
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

        pub(super) fn render(&self) -> String {
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

    async fn capture(mut reader: impl AsyncRead + Unpin, kept: &mut Kept) -> io::Result<()> {
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            kept.push(&buffer[..count]);
        }
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
        let root = directory.path().canonicalize().unwrap();
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
            let root = directory.path().canonicalize().unwrap();
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
