//! Cron pre-run and `no_agent` script validation and execution.
//!
//! Mirrors `cron/job_args.py::_validate_cron_script_path`, `cron/scheduler_script.py`, and
//! `cron/scheduler_prompt.py::_parse_wake_gate`:
//! - scripts must stay inside `<home>/scripts/` (no absolute paths, `~`, drive letters, NUL bytes,
//!   or traversal/symlink escapes);
//! - `.sh`/`.bash` run via `bash`; everything else runs via the job's validated Python
//!   `interpreter` or `python3`;
//! - stdout/stderr are lossily UTF-8 decoded, secret-redacted, and bounded by timeout and cancel
//!   polling with process-group termination on POSIX.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use tokio::io::AsyncReadExt;

use super::manage::py_repr;

const DEFAULT_SCRIPT_TIMEOUT_SECS: u64 = 120;

static INTERPRETER_IMAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^pythonw?(?:\d+(?:\.\d+)*)?(?:\.exe)?$").expect("valid interpreter regex")
});

/// Resolve `<home>/scripts`.
pub fn scripts_dir(home: &Path) -> PathBuf {
    home.join("scripts")
}

/// Normalize a path lexically (without requiring it to exist on disk) so traversal checks work
/// even when the target file does not exist yet.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Create/edit-time script validation, matching `cron/job_args.py::_validate_cron_script_path`.
pub fn validate_script_path_for_create(home: &Path, script: &str) -> Result<(), String> {
    let raw = script.trim();
    if raw.is_empty() {
        return Ok(());
    }
    if raw.contains('\0') {
        return Err(format!(
            "Blocked: script path contains a NUL byte: {}",
            py_repr(script)
        ));
    }
    let dir = scripts_dir(home);
    let is_drive = raw.len() >= 2 && raw.as_bytes()[1] == b':';
    if raw.starts_with('/') || raw.starts_with('~') || is_drive {
        return Err(format!(
            "Script path must be relative to {}/. Got absolute or home-relative path: {}. Place scripts in {}/ and use just the filename.",
            dir.display(),
            py_repr(raw),
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create scripts directory: {error}"))?;
    let base = std::fs::canonicalize(&dir).unwrap_or_else(|_| normalize_lexical(&dir));
    let candidate = dir.join(raw);
    let resolved =
        std::fs::canonicalize(&candidate).unwrap_or_else(|_| normalize_lexical(&base.join(raw)));
    if !resolved.starts_with(&base) {
        return Err(format!(
            "Script path escapes the scripts directory via traversal: {}",
            py_repr(raw)
        ));
    }
    if !candidate.is_file() {
        return Err(format!(
            "Script file not found: {}. Create it in {}/ first.",
            candidate.display(),
            dir.display()
        ));
    }
    Ok(())
}

/// Run-time script path resolution, matching `cron/scheduler_script.py::_resolve_script_path`.
pub fn resolve_script_path(home: &Path, script_path: &str) -> Result<PathBuf, String> {
    if script_path.contains('\0') {
        return Err(format!(
            "Blocked: script path contains a NUL byte: {}",
            py_repr(script_path)
        ));
    }
    let dir = scripts_dir(home);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create scripts directory: {error}"))?;
    let base = std::fs::canonicalize(&dir).unwrap_or_else(|_| normalize_lexical(&dir));
    let expanded = expand_tilde(script_path);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        dir.join(expanded)
    };
    let resolved = std::fs::canonicalize(&candidate).unwrap_or_else(|_| {
        if candidate.is_absolute() {
            normalize_lexical(&candidate)
        } else {
            normalize_lexical(&base.join(&candidate))
        }
    });
    if !resolved.starts_with(&base) {
        return Err(format!(
            "Blocked: script path resolves outside the scripts directory ({}): {}",
            base.display(),
            py_repr(script_path)
        ));
    }
    if !resolved.exists() {
        return Err(format!(
            "Script not found: {}. Cron scripts are looked up only in this profile's folder ({}); if the job was copied from another profile, copy the script there too, or edit the job with `hermes cron edit`.",
            resolved.display(),
            base.display()
        ));
    }
    if !resolved.is_file() {
        return Err(format!("Script path is not a file: {}", resolved.display()));
    }
    Ok(resolved)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Validate a job's explicit Python `interpreter` at run time, matching
/// `cron/scheduler_script.py::_resolve_cron_interpreter`.
pub fn resolve_interpreter(interpreter: &str) -> Result<PathBuf, String> {
    let raw = interpreter.trim();
    let expanded = expand_tilde(raw);
    if !expanded.is_absolute() {
        return Err(format!(
            "Interpreter must be an absolute or ~-prefixed path (got {}). Bare names like 'python3' are not stable across PATH changes.",
            py_repr(raw)
        ));
    }
    let meta = std::fs::metadata(&expanded).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("Interpreter not found: {raw}")
        } else {
            format!(
                "Unable to resolve interpreter path {}: {error}",
                py_repr(raw)
            )
        }
    })?;
    if !meta.is_file() {
        return Err(format!(
            "Interpreter path is not a file: {}",
            expanded.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "Interpreter is not executable: {}",
                expanded.display()
            ));
        }
    }
    let canonical = std::fs::canonicalize(&expanded).unwrap_or_else(|_| expanded.clone());
    for candidate in [&expanded, &canonical] {
        let name = candidate
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if !INTERPRETER_IMAGE_RE.is_match(&name) || name.starts_with("pythonw") {
            return Err(format!(
                "Interpreter must be a Python executable (python, python3, python3.12, ...): {}",
                expanded.display()
            ));
        }
    }
    Ok(expanded)
}

/// Wake gate: returns `false` only if the last non-empty line is a JSON object with
/// `"wakeAgent": false` (boolean false, not 0). Matches `cron/scheduler_prompt.py::_parse_wake_gate`.
pub fn parse_wake_gate(script_output: &str) -> bool {
    let Some(last) = script_output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
    else {
        return true;
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(last) else {
        return true;
    };
    map.get("wakeAgent") != Some(&serde_json::Value::Bool(false))
}

/// Configured script timeout: `HERMES_CRON_SCRIPT_TIMEOUT`, default 120s.
pub fn script_timeout_secs() -> u64 {
    std::env::var("HERMES_CRON_SCRIPT_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_SECS)
}

fn find_bash() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(if cfg!(windows) { "bash.exe" } else { "bash" });
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let fallback = PathBuf::from("/bin/bash");
    fallback.is_file().then_some(fallback)
}

fn find_python() -> PathBuf {
    if let Ok(path) = std::env::var("PATH") {
        for name in ["python3", "python"] {
            let bin = if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_owned()
            };
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join(&bin);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from("python3")
}

fn script_argv(path: &Path, interpreter: Option<&str>) -> Result<(PathBuf, Vec<String>), String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if ext == "sh" || ext == "bash" {
        let Some(bash) = find_bash() else {
            return Err(format!(
                "Cannot run .sh/.bash script {}: bash not found on PATH.",
                py_repr(&path.file_name().unwrap_or_default().to_string_lossy())
            ));
        };
        return Ok((bash, vec![path.to_string_lossy().into_owned()]));
    }
    if let Some(interp) = interpreter.map(str::trim).filter(|s| !s.is_empty()) {
        let exe = resolve_interpreter(interp)?;
        return Ok((exe, vec![path.to_string_lossy().into_owned()]));
    }
    Ok((find_python(), vec![path.to_string_lossy().into_owned()]))
}

#[cfg(unix)]
fn kill_process_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let pgid = -(pid as libc::pid_t);
        unsafe {
            libc::kill(pgid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// Run a cron script asynchronously with timeout and cancel polling.
/// Returns `(success, output_or_error)` matching `cron/scheduler_script.py::_run_job_script`.
pub async fn run_job_script<F>(
    home: &Path,
    script_path: &str,
    workdir: Option<&Path>,
    interpreter: Option<&str>,
    timeout_secs: u64,
    mut cancel_reason: F,
) -> (bool, String)
where
    F: FnMut() -> Option<String>,
{
    let path = match resolve_script_path(home, script_path) {
        Ok(path) => path,
        Err(error) => return (false, error),
    };
    let (program, args) = match script_argv(&path, interpreter) {
        Ok(argv) => argv,
        Err(error) => return (false, error),
    };
    let cwd = workdir
        .filter(|dir| dir.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.parent().unwrap_or(home).to_path_buf());

    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Ensure the script knows which profile home it belongs to.
    command.env("HERMES_HOME", home);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (false, format!("Script execution failed: {error}")),
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe.take() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let status = loop {
        if let Some(reason) = cancel_reason() {
            kill_process_group(&mut child);
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return (false, format!("Script cancelled: {reason}"));
        }
        if Instant::now() >= deadline {
            kill_process_group(&mut child);
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return (
                false,
                format!("Script timed out after {timeout_secs}s: {}", path.display()),
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => {
                kill_process_group(&mut child);
                stdout_task.abort();
                stderr_task.abort();
                return (false, format!("Script execution failed: {error}"));
            }
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let stdout = crate::redact::redact_with(
        String::from_utf8_lossy(&stdout_bytes).trim(),
        crate::redact::Mode::default(),
    );
    let stderr = crate::redact::redact_with(
        String::from_utf8_lossy(&stderr_bytes).trim(),
        crate::redact::Mode::default(),
    );

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let mut parts = vec![format!("Script exited with code {code}")];
        if !stderr.is_empty() {
            parts.push(format!("stderr:\n{stderr}"));
        }
        if !stdout.is_empty() {
            parts.push(format!("stdout:\n{stdout}"));
        }
        return (false, parts.join("\n"));
    }
    (true, stdout)
}
