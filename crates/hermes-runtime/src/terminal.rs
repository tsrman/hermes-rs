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
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TerminalSettings {
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
            env_passthrough: names,
            code_timeout: section["timeout"].as_u64().filter(|seconds| *seconds > 0),
            code_max_tool_calls: section["max_tool_calls"]
                .as_u64()
                .filter(|calls| *calls > 0)
                .map(|calls| calls as usize),
            scratch: None,
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

fn tracked_directory(session: &str) -> Option<PathBuf> {
    WORKING_DIRECTORIES.lock().ok()?.get(session).cloned()
}

/// Forget where a finished conversation (a subagent) left its working directory.
pub(crate) fn forget_session(session: &str) {
    track_directory(session, None);
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
    use std::io;
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
    const HEAD_LIMIT: usize = 16 * 1024;
    const TAIL_LIMIT: usize = 48 * 1024;
    const CWD_MARKER: &str = "__HERMES_CWD__";

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arguments {
        command: String,
        workdir: Option<String>,
        timeout: Option<u32>,
        background: Option<bool>,
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
        let seconds = args.timeout.unwrap_or(180);
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
            let mut child = Command::new("/bin/sh");
            configure(&mut child, &args.command, directory, settings, false);
            return crate::process::spawn(child, &args.command, target, owner);
        }
        let mut result = run(
            &args.command,
            directory,
            Duration::from_secs(u64::from(seconds)),
            settings,
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

    /// The sandbox every command starts in: a minimal environment, its own process group, and the
    /// approved directory as the working directory.
    fn configure(
        child: &mut Command,
        command: &str,
        directory: File,
        settings: &TerminalSettings,
        report_directory: bool,
    ) {
        // The trap reports the final directory even after `exit`, on a line of its own.
        let script = if report_directory {
            format!(
                "trap 'status=$?; printf \"\\n{CWD_MARKER}%s\\n\" \"$(pwd -P)\"; exit $status' EXIT\n{command}"
            )
        } else {
            command.to_owned()
        };
        child
            .args(["-c", &script])
            .env_clear()
            .envs(settings.environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: fchdir is async-signal-safe and the closure owns the open
        // descriptor, so the child enters the approved directory before exec.
        unsafe { child.pre_exec(move || fchdir(&directory).map_err(io::Error::from)) };
    }

    async fn run(
        command: &str,
        directory: File,
        duration: Duration,
        settings: &TerminalSettings,
    ) -> Result<Value, String> {
        // One pipe carries both streams, so output keeps the order the command wrote it in.
        let (reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
        let errors = writer.try_clone().map_err(|error| error.to_string())?;
        let mut child = Command::new("/bin/sh");
        configure(&mut child, command, directory, settings, true);
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
        let receiver = tokio::net::unix::pipe::Receiver::from_file(File::from(
            std::os::fd::OwnedFd::from(reader),
        ))
        .map_err(|error| error.to_string())?;
        let mut kept = Kept::default();
        let result = timeout(duration, async {
            tokio::try_join!(group.wait(), capture(receiver, &mut kept))
        })
        .await;
        match result {
            Ok(Ok((status, ()))) => {
                let (text, ended) = split_directory_marker(&kept.render());
                let text = redact_terminal_output(&text, Some(command), &[]);
                let mut result = json!({"output": text, "exit_code": status.code().unwrap_or(-1),
                           "status": "completed", "truncated": kept.omitted > 0});
                if let Some(ended) = ended {
                    result["_cwd"] = json!(ended);
                }
                Ok(result)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => {
                group.stop();
                let _ = group.child.wait().await;
                let (text, _) = split_directory_marker(&kept.render());
                Ok(
                    json!({"error": "The command timed out", "exit_code": -1, "status": "timeout",
                           "output": redact_terminal_output(&text, Some(command), &[])}),
                )
            }
        }
    }

    /// The first bytes and the last bytes of what a command printed.
    #[derive(Default)]
    struct Kept {
        head: Vec<u8>,
        tail: std::collections::VecDeque<u8>,
        omitted: usize,
    }

    impl Kept {
        fn push(&mut self, bytes: &[u8]) {
            let room = HEAD_LIMIT.saturating_sub(self.head.len());
            let (first, rest) = bytes.split_at(room.min(bytes.len()));
            self.head.extend_from_slice(first);
            self.tail.extend(rest);
            while self.tail.len() > TAIL_LIMIT {
                self.tail.pop_front();
                self.omitted += 1;
            }
        }

        fn render(&self) -> String {
            let mut bytes = self.head.clone();
            if self.omitted > 0 {
                bytes.extend_from_slice(
                    format!("\n… [{} bytes omitted] …\n", self.omitted).as_bytes(),
                );
            }
            bytes.extend(self.tail.iter());
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// Remove the final-directory line the wrapper prints, and return that directory.
    fn split_directory_marker(text: &str) -> (String, Option<String>) {
        let needle = format!("\n{CWD_MARKER}");
        match text.rfind(&needle) {
            Some(index) => {
                let after = &text[index + needle.len()..];
                let path = after.lines().next().unwrap_or("").to_owned();
                (text[..index].to_owned(), (!path.is_empty()).then_some(path))
            }
            None => (text.to_owned(), None),
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
        assert!(text.contains("bytes omitted"));
        assert!(
            text.trim_end().ends_with("the-end"),
            "{}",
            &text[text.len() - 40..]
        );
    }
}
