//! Approved foreground commands on the local Linux and macOS CLI.

use std::future::Future;

use serde_json::{json, Value};

use crate::file_tools::{approval_error, ApprovalDecision, ApprovalRequest, FileWorkspace};

pub(crate) const TERMINAL: &str = "terminal";
pub(crate) const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

pub(crate) async fn execute<A, F>(
    workspace: &FileWorkspace<'_>,
    arguments: &str,
    approval_timeout: Option<u32>,
    background: Option<&str>,
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
        shell::execute(workspace, arguments, approval_timeout, background, approve)
            .await
            .unwrap_or_else(|error| json!({"error": error, "exit_code": -1, "status": "error"}))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (workspace, arguments, approval_timeout, background, approve);
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

    use super::*;

    const OUTPUT_LIMIT: usize = 32 * 1024;

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
        let workdir = args.workdir.as_deref().unwrap_or(".");
        let (root, path) = workspace.resolve(workdir)?;
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
        let (current_root, current_path) = workspace.resolve(workdir)?;
        let current = open_directory(&current_root, &current_path)?;
        if current_path != path || identity(&current)? != identity(&directory)? {
            return Err("The working directory changed while approval was pending".into());
        }
        if let Some(owner) = owner {
            let mut child = Command::new("/bin/sh");
            configure(&mut child, &args.command, directory);
            return crate::process::spawn(child, &args.command, target, owner);
        }
        run(
            &args.command,
            directory,
            Duration::from_secs(u64::from(seconds)),
        )
        .await
    }

    fn identity(directory: &File) -> Result<(u64, u64), String> {
        let metadata = directory.metadata().map_err(|error| error.to_string())?;
        Ok((metadata.dev(), metadata.ino()))
    }

    /// The sandbox every command starts in: a minimal environment, its own process group, and the
    /// approved directory as the working directory.
    fn configure(child: &mut Command, command: &str, directory: File) {
        child
            .args(["-c", command])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(
                "LANG",
                if cfg!(target_os = "macos") {
                    "en_US.UTF-8"
                } else {
                    "C.UTF-8"
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: fchdir is async-signal-safe and the closure owns the open
        // descriptor, so the child enters the approved directory before exec.
        unsafe { child.pre_exec(move || fchdir(&directory).map_err(io::Error::from)) };
    }

    async fn run(command: &str, directory: File, duration: Duration) -> Result<Value, String> {
        let mut child = Command::new("/bin/sh");
        configure(&mut child, command, directory);
        let child = child.spawn().map_err(|error| error.to_string())?;
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or("Missing process group ID")?;
        let mut group = ProcessGroup {
            child,
            pid: Some(pid),
        };
        let stdout = group.child.stdout.take().ok_or("Missing command stdout")?;
        let stderr = group.child.stderr.take().ok_or("Missing command stderr")?;
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let result = timeout(duration, async {
            tokio::try_join!(
                group.wait(),
                capture(stdout, &mut output),
                capture(stderr, &mut errors)
            )
        })
        .await;
        match result {
            Ok(Ok((status, stdout_truncated, stderr_truncated))) => {
                let stdout = String::from_utf8_lossy(&output);
                let stderr = String::from_utf8_lossy(&errors);
                Ok(
                    json!({"output": format!("{stdout}{stderr}"), "exit_code": status.code().unwrap_or(-1),
                           "status": "completed", "truncated": stdout_truncated || stderr_truncated}),
                )
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => {
                group.stop();
                let _ = group.child.wait().await;
                Ok(
                    json!({"error": "The command timed out", "exit_code": -1, "status": "timeout",
                           "output": format!("{}{}", String::from_utf8_lossy(&output), String::from_utf8_lossy(&errors))}),
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

    async fn capture(mut reader: impl AsyncRead + Unpin, bytes: &mut Vec<u8>) -> io::Result<bool> {
        let mut buffer = [0; 8192];
        let mut truncated = false;
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok(truncated);
            }
            let kept = count.min(OUTPUT_LIMIT.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..kept]);
            truncated |= kept != count;
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
}
