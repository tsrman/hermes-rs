//! `execute_code`: Python that calls tools programmatically, in a kernel that keeps its state.
//!
//! One `python3` process lives per conversation. Code arrives over a socket pair (the kernel's
//! file descriptor 3) as one cell at a time and runs in persistent globals; the `hermes_tools`
//! module it imports sends each tool call back over the same socket, and this side runs it through
//! the ordinary tool table, so approvals, the deny list, redaction and read-before-write apply to
//! every inner call. A cell that times out, or ends the interpreter, loses the kernel and its state.
//!
//! This is a security envelope, not an isolation jail: the script is ordinary Python running as
//! the user, in the conversation's working directory, with the same environment commands get
//! (never the model credentials). Its own `subprocess` or `os.system` calls do not pass through
//! the `terminal` rules, which is why every cell needs approval of its code.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::file_tools::{
    approval_error, request_approval, ApprovalAction, ApprovalDecision, ApprovalRequest,
};
use crate::redact::{redact_with, Mode};
use crate::tools::{BoxFuture, ToolContext, ToolEnv};

pub(crate) const EXECUTE_CODE: &str = "execute_code";

const MAX_STDOUT_BYTES: usize = 50_000;
const MAX_STDERR_BYTES: usize = 10_000;
/// What a cell may capture in memory before further output is dropped.
const CAPTURE_LIMIT: usize = 5_000_000;
pub(crate) const DEFAULT_TIMEOUT: u64 = 300;
pub(crate) const DEFAULT_MAX_TOOL_CALLS: usize = 50;

/// The tools a script may call, in the reference's order.
const SANDBOX_TOOLS: [&str; 7] = [
    "web_search",
    "web_extract",
    "read_file",
    "write_file",
    "search_files",
    "patch",
    "terminal",
];

/// The tools of `env` a script can call: those offered to the model that the sandbox allows.
pub(crate) fn sandbox_tools(env: &ToolEnv) -> Vec<&'static str> {
    SANDBOX_TOOLS
        .iter()
        .copied()
        .filter(|name| env.policy.allows(name) && !env.policy.is_deferred(name))
        .filter(|name| match *name {
            "web_search" => env
                .web
                .as_ref()
                .is_some_and(crate::web_tools::WebSettings::offers_search),
            "web_extract" => env
                .web
                .as_ref()
                .is_some_and(crate::web_tools::WebSettings::offers_extract),
            "read_file" | "search_files" => true,
            "write_file" | "patch" => env.write_files,
            "terminal" => env.terminal,
            _ => false,
        })
        .collect()
}

pub(crate) fn schema(tools: &[&str]) -> Value {
    #[derive(serde::Deserialize)]
    struct Docs {
        schema: Value,
        lines: Vec<(String, String)>,
    }
    let docs: Docs = serde_json::from_str(include_str!("schemas/execute_code.json"))
        .expect("valid execute_code.json");
    let mut function = docs.schema;
    if let Some(text) = function["description"].as_str() {
        let mut text = text.to_owned();
        for (name, line) in &docs.lines {
            if !tools.contains(&name.as_str()) {
                text = text.replacen(&format!("{line}\n"), "", 1);
            }
        }
        function["description"] = Value::String(text);
    }
    json!({"type": "function", "function": function})
}

/// The Python the kernel runs: the cell loop, and the `hermes_tools` module with the helpers and
/// one function per tool the script may call.
fn kernel_source(tools: &[&str]) -> String {
    const STUBS: [(&str, &str); 7] = [
        ("web_search", "def web_search(query, limit=5):\n    return _call('web_search', {'query': query, 'limit': limit})\n"),
        ("web_extract", "def web_extract(urls, char_limit=None):\n    return _call('web_extract', {'urls': urls, 'char_limit': char_limit})\n"),
        ("read_file", "def read_file(path, offset=1, limit=2000):\n    return _call('read_file', {'path': path, 'offset': offset, 'limit': limit})\n"),
        ("write_file", "def write_file(path, content):\n    return _call('write_file', {'path': path, 'content': content})\n"),
        ("search_files", "def search_files(pattern, target='content', path='.', file_glob=None, limit=50, offset=0, output_mode='content', context=0, order='discovery'):\n    return _call('search_files', {'pattern': pattern, 'target': target, 'path': path, 'file_glob': file_glob, 'limit': limit, 'offset': offset, 'output_mode': output_mode, 'context': context, 'order': order})\n"),
        ("patch", "def patch(path=None, old_string=None, new_string=None, replace_all=False, mode='replace'):\n    return _call('patch', {'path': path, 'old_string': old_string, 'new_string': new_string, 'replace_all': replace_all, 'mode': mode})\n"),
        ("terminal", "def terminal(command, timeout=None, workdir=None):\n    return _call('terminal', {'command': command, 'timeout': timeout, 'workdir': workdir})\n"),
    ];
    let stubs: String = STUBS
        .iter()
        .filter(|(name, _)| tools.contains(name))
        .map(|(_, source)| *source)
        .collect();
    let exports = ["json_parse", "shell_quote", "retry"]
        .iter()
        .chain(tools.iter())
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{KERNEL_HEAD}\n{stubs}\n_EXPORTS = [{exports}]\n{KERNEL_TAIL}")
}

const KERNEL_HEAD: &str = r#"
import contextlib, io, json, os, shlex, socket, sys, time, traceback, types

_sock = socket.socket(fileno=3)
_reader = _sock.makefile("r", encoding="utf-8", newline="\n")
_LIMIT = 5000000


class _Capture(io.StringIO):
    def write(self, text):
        if self.tell() < _LIMIT:
            return super().write(text)
        return len(text)


def _send(message):
    _sock.sendall((json.dumps(message, ensure_ascii=False) + "\n").encode("utf-8"))


def _call(tool, args):
    _send({"type": "call", "tool": tool, "args": args})
    line = _reader.readline()
    if not line:
        raise RuntimeError("the host closed the connection")
    return json.loads(line)["value"]


def json_parse(text):
    """Parse JSON tolerant of control characters and a UTF-8 BOM (strict=False)."""
    if isinstance(text, str) and text.startswith("﻿"):
        text = text[1:]
    return json.loads(text, strict=False)


def shell_quote(s):
    """Shell-escape a string for safe interpolation into commands."""
    return shlex.quote(s)


def retry(fn, max_attempts=3, delay=2):
    """Retry a function up to max_attempts times with exponential backoff."""
    last_err = None
    for attempt in range(max_attempts):
        try:
            return fn()
        except Exception as e:
            last_err = e
            if attempt < max_attempts - 1:
                time.sleep(delay * (2 ** attempt))
    raise last_err

"#;

const KERNEL_TAIL: &str = r#"
_module = types.ModuleType("hermes_tools")
for _name in _EXPORTS:
    setattr(_module, _name, globals()[_name])
sys.modules["hermes_tools"] = _module

GLOBALS = {"__name__": "__main__", "__builtins__": __builtins__}
_paths = set()

for _line in _reader:
    _message = json.loads(_line)
    if _message.get("type") != "cell":
        continue
    _cwd = _message.get("cwd")
    try:
        os.chdir(_cwd)
        if _cwd not in _paths:
            _paths.add(_cwd)
            sys.path.insert(0, _cwd)
    except Exception:
        pass
    _out, _err = _Capture(), _Capture()
    _status, _trace = "ok", ""
    try:
        with contextlib.redirect_stdout(_out), contextlib.redirect_stderr(_err):
            exec(compile(_message["code"], "<cell>", "exec"), GLOBALS)
    except SystemExit as _exit:
        _status, _trace = "exit", "SystemExit: " + repr(_exit.code)
    except BaseException:
        _status, _trace = "error", traceback.format_exc()
    _send({"type": "done", "status": _status, "stdout": _out.getvalue(),
           "stderr": _err.getvalue(), "traceback": _trace})
"#;

struct Kernel {
    child: Child,
    pgid: i32,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// What the kernel's processes wrote straight to the terminal's descriptors.
    raw: Arc<StdMutex<Vec<u8>>>,
    tools: Vec<&'static str>,
    cells: u64,
}

static KERNELS: LazyLock<Mutex<HashMap<String, Kernel>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static GROUPS: LazyLock<StdMutex<HashSet<i32>>> = LazyLock::new(|| StdMutex::new(HashSet::new()));

fn kill_group(pgid: i32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

/// Stop every kernel this process started. Called when the process exits.
pub fn kill_all_kernels() {
    if let Ok(groups) = GROUPS.lock() {
        for pgid in groups.iter() {
            kill_group(*pgid);
        }
    }
}

/// End the kernel of a finished conversation (a subagent's).
pub(crate) async fn end_session(session: &str) {
    if let Some(kernel) = KERNELS.lock().await.remove(session) {
        discard(kernel);
    }
}

fn discard(mut kernel: Kernel) {
    kill_group(kernel.pgid);
    if let Ok(mut groups) = GROUPS.lock() {
        groups.remove(&kernel.pgid);
    }
    let _ = kernel.child.start_kill();
}

/// The interpreter: the active virtual environment's, else `python3` on the command PATH.
fn interpreter(env: &[(String, String)]) -> Option<PathBuf> {
    let var = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    for name in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(prefix) = var(name) {
            let candidate = Path::new(&prefix).join("bin/python");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    var("PATH")?
        .split(':')
        .map(|dir| Path::new(dir).join("python3"))
        .find(|candidate| candidate.is_file())
}

async fn spawn(
    python: &Path,
    tools: &[&'static str],
    directory: &Path,
    env: Vec<(String, String)>,
) -> Result<Kernel, String> {
    let error = |e: std::io::Error| e.to_string();
    let (host, child_end) = std::os::unix::net::UnixStream::pair().map_err(error)?;
    let (reader, writer) = std::io::pipe().map_err(error)?;
    let errors = writer.try_clone().map_err(error)?;
    let mut command = Command::new(python);
    command
        .arg("-u")
        .arg("-c")
        .arg(kernel_source(tools))
        .env_clear()
        .envs(env)
        .env("PYTHONUNBUFFERED", "1")
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(errors))
        .process_group(0)
        .kill_on_drop(true);
    let raw_fd = child_end.as_raw_fd();
    // SAFETY: only async-signal-safe calls (dup2, fcntl) run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if raw_fd == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(raw_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    drop(command);
    drop(child_end);
    let child = spawned.map_err(error)?;
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("The kernel did not start")?;
    if let Ok(mut groups) = GROUPS.lock() {
        groups.insert(pgid);
    }
    host.set_nonblocking(true).map_err(error)?;
    let (read_half, write_half) = tokio::net::UnixStream::from_std(host)
        .map_err(error)?
        .into_split();
    let raw = Arc::new(StdMutex::new(Vec::new()));
    let sink = Arc::clone(&raw);
    let mut receiver =
        tokio::net::unix::pipe::Receiver::from_file(std::fs::File::from(OwnedFd::from(reader)))
            .map_err(error)?;
    tokio::spawn(async move {
        let mut buffer = [0u8; 8192];
        while let Ok(count) = receiver.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            if let Ok(mut bytes) = sink.lock() {
                if bytes.len() < CAPTURE_LIMIT {
                    bytes.extend_from_slice(&buffer[..count]);
                }
            }
        }
    });
    Ok(Kernel {
        child,
        pgid,
        reader: BufReader::new(read_half),
        writer: write_half,
        raw,
        tools: tools.to_vec(),
        cells: 0,
    })
}

enum Cell {
    Done(Value),
    TimedOut,
    Died,
}

/// Run one cell: feed it to the kernel and serve its tool calls until it reports back.
async fn run_cell(
    kernel: &mut Kernel,
    code: &str,
    directory: &Path,
    cx: &mut ToolContext<'_, '_>,
    deadline: Instant,
    max_calls: usize,
    calls: &mut usize,
) -> Cell {
    let cell = json!({"type": "cell", "code": code, "cwd": directory.display().to_string()});
    if kernel
        .writer
        .write_all(format!("{cell}\n").as_bytes())
        .await
        .is_err()
    {
        return Cell::Died;
    }
    loop {
        let mut line = String::new();
        let read =
            tokio::time::timeout_at(deadline.into(), kernel.reader.read_line(&mut line)).await;
        match read {
            Err(_) => return Cell::TimedOut,
            Ok(Err(_) | Ok(0)) => return Cell::Died,
            Ok(Ok(_)) => {}
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            return Cell::Died;
        };
        match message["type"].as_str() {
            Some("done") => return Cell::Done(message),
            Some("call") => {
                let name = message["tool"].as_str().unwrap_or("").to_owned();
                let value = if !kernel.tools.contains(&name.as_str()) {
                    json!({"error": format!(
                        "Tool '{name}' is not available in execute_code. Available: {}",
                        { let mut names = kernel.tools.clone(); names.sort_unstable(); names.join(", ") }
                    )})
                } else if *calls >= max_calls {
                    json!({"error": format!("Tool call limit reached ({max_calls}). No more tool calls allowed in this execution.")})
                } else {
                    // Parameters the sandbox does not take, and unset ones, are dropped.
                    let mut args = match &message["args"] {
                        Value::Object(map) => map.clone(),
                        _ => Map::new(),
                    };
                    args.retain(|_, value| !value.is_null());
                    if name == "terminal" {
                        for blocked in [
                            "background",
                            "pty",
                            "notify",
                            "notify_on_complete",
                            "watch_patterns",
                            "heartbeat",
                            "persist_on_release",
                        ] {
                            args.remove(blocked);
                        }
                    }
                    let text = Value::Object(args).to_string();
                    let outcome = match crate::tools::find(&name, cx.options.tools) {
                        Some(tool) => {
                            tokio::time::timeout_at(deadline.into(), tool.call(cx, &text)).await
                        }
                        None => Ok(json!({"error": format!("Unknown tool: {name}")})),
                    };
                    *calls += 1;
                    match outcome {
                        Ok(value) => value,
                        Err(_) => return Cell::TimedOut,
                    }
                };
                let reply = json!({"value": value});
                if kernel
                    .writer
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .is_err()
                {
                    return Cell::Died;
                }
            }
            _ => return Cell::Died,
        }
    }
}

/// ANSI escapes removed and credential shapes masked, as the reference cleans code output.
fn clean(text: &str) -> String {
    redact_with(
        &crate::session_search::strip_ansi(text),
        Mode {
            code_file: true,
            ..Mode::default()
        },
    )
}

/// The first 40% and the last 60% of `budget` bytes, with a notice for what is left out.
fn head_tail(text: &str, budget: usize) -> (String, Map<String, Value>) {
    let bytes = text.as_bytes();
    let total = bytes.len();
    let captured = total.min(budget);
    let mut meta = Map::new();
    meta.insert("stdout_truncated".into(), json!(total > captured));
    meta.insert("stdout_bytes_captured".into(), json!(captured));
    meta.insert("stdout_bytes_total".into(), json!(total));
    meta.insert("stdout_bytes_omitted".into(), json!(total - captured));
    if total <= budget {
        return (String::from_utf8_lossy(bytes).into_owned(), meta);
    }
    let head = budget * 2 / 5;
    let tail = budget - head;
    let shown = format!(
        "{}\n\n... [OUTPUT TRUNCATED - {} bytes omitted out of {} total] ...\n\n{}",
        String::from_utf8_lossy(&bytes[..head]),
        thousands(total - captured),
        thousands(total),
        String::from_utf8_lossy(&bytes[total - tail..])
    );
    meta.insert(
        "warning".into(),
        json!("execute_code stdout was truncated; the script did run, but only the captured head/tail output is included. Re-run only with narrower output if the omitted data is required."),
    );
    (shown, meta)
}

fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn with_stderr(stdout: &str, stderr: &str) -> String {
    format!("{stdout}\n--- stderr ---\n{stderr}")
}

pub(crate) fn call<'a>(
    cx: &'a mut ToolContext<'_, '_>,
    arguments: &'a str,
) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            code: Option<String>,
            #[serde(default)]
            reset: bool,
        }
        let args: Args = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(_) => {
                return json!({"error": "execute_code expects a 'code' string and an optional 'reset' boolean."})
            }
        };
        let code = args.code.unwrap_or_default();
        if code.trim().is_empty() {
            return json!({"error": "No code provided. execute_code requires a non-empty 'code' parameter containing Python source. To run shell commands, use terminal(command=...) instead."});
        }
        let Some(approval_timeout) = cx
            .options
            .approval_timeout_secs
            .filter(|_| cx.options.can_run_terminal())
        else {
            return approval_error(ApprovalDecision::Unavailable);
        };
        let session = cx.options.state_key().to_owned();
        let defaults = crate::terminal::TerminalSettings::default();
        let settings = cx.options.terminal_env.unwrap_or(&defaults);
        let root = cx
            .options
            .root
            .canonicalize()
            .unwrap_or_else(|_| cx.options.root.to_path_buf());
        let directory = crate::terminal::session_directory(&session, &root);
        let mut ask = |request: ApprovalRequest| (cx.approve)(request);
        let decision = request_approval(
            ApprovalRequest {
                target: directory.display().to_string(),
                content: code.clone(),
                action: ApprovalAction::RunCode,
                timeout_secs: approval_timeout,
            },
            &mut ask,
        )
        .await;
        if !matches!(decision, ApprovalDecision::Approved) {
            return approval_error(decision);
        }
        let tools = sandbox_tools(&ToolEnv::from_options(cx.options));
        let env = settings.environment();
        let Some(python) = interpreter(&env) else {
            return json!({"status": "error", "error": "No python3 was found on PATH (or in the active VIRTUAL_ENV / CONDA_PREFIX).", "tool_calls_made": 0, "duration_seconds": 0});
        };
        let started = Instant::now();
        let mut state_reset = args.reset;
        let existing = {
            let mut kernels = KERNELS.lock().await;
            match kernels.remove(&session) {
                Some(kernel) if args.reset || kernel.tools != tools => {
                    state_reset = true;
                    discard(kernel);
                    None
                }
                other => other,
            }
        };
        let reused = existing.is_some();
        let mut kernel = match existing {
            Some(kernel) => kernel,
            None => match spawn(&python, &tools, &directory, env).await {
                Ok(kernel) => kernel,
                Err(message) => {
                    return json!({"status": "error", "error": format!("The kernel could not start: {message}"), "tool_calls_made": 0, "duration_seconds": 0});
                }
            },
        };
        if let Ok(mut raw) = kernel.raw.lock() {
            raw.clear();
        }
        let timeout = Duration::from_secs(settings.code_timeout.unwrap_or(DEFAULT_TIMEOUT));
        let max_calls = settings
            .code_max_tool_calls
            .unwrap_or(DEFAULT_MAX_TOOL_CALLS);
        let mut calls = 0usize;
        let outcome = run_cell(
            &mut kernel,
            &code,
            &directory,
            cx,
            started + timeout,
            max_calls,
            &mut calls,
        )
        .await;
        // Let the reader catch up with what the processes wrote last.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let raw_text = kernel
            .raw
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        kernel.cells += 1;
        let cells = kernel.cells;
        let duration = (started.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        let mut result = Map::new();
        let info = |ended: bool| {
            let mut info = json!({"mode": "session", "reused": reused, "execution_count": cells, "state_reset": state_reset});
            if ended {
                info["ended"] = json!(true);
            }
            info
        };
        match outcome {
            Cell::Done(payload) => {
                let stdout = format!("{}{}", payload["stdout"].as_str().unwrap_or(""), raw_text);
                let (shown, meta) = head_tail(&clean(&stdout), MAX_STDOUT_BYTES);
                let stderr = clean(payload["stderr"].as_str().unwrap_or(""));
                let stderr = head_tail(&stderr, MAX_STDERR_BYTES).0;
                result.insert("status".into(), json!("success"));
                result.insert("output".into(), json!(shown));
                result.insert("exit_code".into(), json!(0));
                result.insert("tool_calls_made".into(), json!(calls));
                result.insert("duration_seconds".into(), json!(duration));
                let status = payload["status"].as_str().unwrap_or("ok");
                result.insert("kernel".into(), info(status == "exit"));
                result.extend(meta);
                match status {
                    "error" => {
                        let trace = clean(payload["traceback"].as_str().unwrap_or(""));
                        result.insert("status".into(), json!("error"));
                        result.insert("exit_code".into(), json!(1));
                        result.insert(
                            "error".into(),
                            json!(if trace.is_empty() {
                                "Cell raised an exception.".to_owned()
                            } else {
                                trace.clone()
                            }),
                        );
                        result.insert(
                            "output".into(),
                            json!(if stderr.is_empty() && trace.is_empty() {
                                shown
                            } else {
                                with_stderr(&shown, &format!("{stderr}{trace}"))
                            }),
                        );
                    }
                    "exit" => {
                        if !stderr.is_empty() {
                            result.insert("output".into(), json!(with_stderr(&shown, &stderr)));
                        }
                    }
                    _ => {
                        if !stderr.is_empty() {
                            result.insert("output".into(), json!(with_stderr(&shown, &stderr)));
                        }
                    }
                }
                if status == "exit" {
                    discard(kernel);
                } else {
                    KERNELS.lock().await.insert(session, kernel);
                }
            }
            Cell::TimedOut => {
                let message = format!(
                    "Cell timed out after {}s; the session kernel was killed and its state was lost. The next execute_code call starts a fresh kernel.",
                    timeout.as_secs()
                );
                let (shown, meta) = head_tail(&clean(&raw_text), MAX_STDOUT_BYTES);
                discard(kernel);
                result.insert("status".into(), json!("timeout"));
                result.insert(
                    "output".into(),
                    json!(if shown.is_empty() {
                        format!("⏰ {message}")
                    } else {
                        format!("{shown}\n\n⏰ {message}")
                    }),
                );
                result.insert("exit_code".into(), json!(-1));
                result.insert("tool_calls_made".into(), json!(calls));
                result.insert("duration_seconds".into(), json!(duration));
                result.insert("kernel".into(), info(false));
                result.extend(meta);
                result.insert("error".into(), json!(message));
            }
            Cell::Died => {
                let detail = clean(raw_text.trim());
                discard(kernel);
                result.insert("status".into(), json!("error"));
                result.insert("output".into(), json!(""));
                result.insert("exit_code".into(), json!(-1));
                result.insert("tool_calls_made".into(), json!(calls));
                result.insert("duration_seconds".into(), json!(duration));
                result.insert("kernel".into(), info(false));
                result.insert(
                    "error".into(),
                    json!(format!(
                        "The session kernel died while running the cell{}",
                        if detail.is_empty() {
                            ".".to_owned()
                        } else {
                            format!(": {detail}")
                        }
                    )),
                );
            }
        }
        Value::Object(result)
    })
}
