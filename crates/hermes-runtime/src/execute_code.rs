//! `execute_code`: Python that calls tools programmatically, in a kernel that keeps its state.
//!
//! Two backends share the exact same cell protocol, persistent session globals, approvals,
//! redaction, output limits, and `hermes_tools` API:
//! - On Unix when a system `python3` (or `VIRTUAL_ENV` / `CONDA_PREFIX`) is available: one
//!   `python3` subprocess lives per conversation over a socket pair (fd 3).
//! - On Windows, on Unix hosts without `python3`, or when `HERMES_PYTHON_ENGINE=embedded`: an
//!   in-process embedded **RustPython** interpreter with frozen stdlib runs on a dedicated session
//!   thread with zero external dependencies.
//!
//! Both backends also expose native pure-Rust Excel helpers (`xlsx_read`, `xlsx_write`) via
//! `from hermes_tools import xlsx_read, xlsx_write`.

use crate::paths::PathExt;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Map, Value};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
#[cfg(unix)]
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

#[cfg(not(target_os = "macos"))]
use crate::embedded_python::{
    use_subprocess_worker, EmbeddedEvent, EmbeddedKernel, EmbeddedSubprocessKernel,
};
use crate::excel_tools;
use crate::file_tools::{
    approval_error, request_approval, ApprovalAction, ApprovalDecision, ApprovalRequest,
};
use crate::office_tools;
use crate::redact::{redact_with, Mode};
use crate::tools::{BoxFuture, ToolContext, ToolEnv};

pub(crate) const EXECUTE_CODE: &str = "execute_code";

const MAX_STDOUT_BYTES: usize = 50_000;
const MAX_STDERR_BYTES: usize = 10_000;
/// What a cell may capture in memory before further output is dropped.
#[cfg(unix)]
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
        #[cfg(not(unix))]
        text.push_str(
            "\n\nBuilt-in RustPython + Office helpers (no system Python or Office required): `from hermes_tools import xlsx_read, xlsx_write, docx_read, docx_write, pptx_read, pptx_write`.\n- xlsx_read(path, sheet=None, max_rows=5000) / xlsx_write(path, sheets): read .xlsx/.xls/.xlsb/.ods sheets or write .xlsx workbooks (formulas like '=SUM(B2:B10)', bold headers, autofit).\n- docx_read(path, max_paragraphs=5000) / docx_write(path, content=None, title=None, subtitle=None, blocks=None, author=None): read .docx paragraphs & tables or create formatted Word .docx documents from Markdown (`content`) or structured `blocks` (heading, paragraph, bullets, numbered, table, quote, code, rule, page_break).\n- pptx_read(path, max_slides=200) / pptx_write(path, slides, title=None, author=None, theme='corporate'): read .pptx slides/tables/notes or create formatted 16:9 PowerPoint .pptx decks from `slides` dicts (title, subtitle, layout='title'|'content'|'section', bullets, body, table, left/right columns, notes; themes: 'corporate', 'dark', 'minimal').",
        );
        function["description"] = Value::String(text);
    }
    json!({"type": "function", "function": function})
}

/// The Python the Unix subprocess kernel runs: the cell loop, and the `hermes_tools` module with
/// helpers and one function per tool the script may call.
#[cfg(unix)]
fn kernel_source(tools: &[&str]) -> String {
    const STUBS: [(&str, &str); 13] = [
        ("web_search", "def web_search(query, limit=5):\n    return _call('web_search', {'query': query, 'limit': limit})\n"),
        ("web_extract", "def web_extract(urls, char_limit=None):\n    return _call('web_extract', {'urls': urls, 'char_limit': char_limit})\n"),
        ("read_file", "def read_file(path, offset=1, limit=2000):\n    return _call('read_file', {'path': path, 'offset': offset, 'limit': limit})\n"),
        ("write_file", "def write_file(path, content):\n    return _call('write_file', {'path': path, 'content': content})\n"),
        ("search_files", "def search_files(pattern, target='content', path='.', file_glob=None, limit=50, offset=0, output_mode='content', context=0, order='discovery'):\n    return _call('search_files', {'pattern': pattern, 'target': target, 'path': path, 'file_glob': file_glob, 'limit': limit, 'offset': offset, 'output_mode': output_mode, 'context': context, 'order': order})\n"),
        ("patch", "def patch(path=None, old_string=None, new_string=None, replace_all=False, mode='replace'):\n    return _call('patch', {'path': path, 'old_string': old_string, 'new_string': new_string, 'replace_all': replace_all, 'mode': mode})\n"),
        ("terminal", "def terminal(command, timeout=None, workdir=None):\n    return _call('terminal', {'command': command, 'timeout': timeout, 'workdir': workdir})\n"),
        ("xlsx_read", "def xlsx_read(path, sheet=None, max_rows=5000):\n    return _call('xlsx_read', {'path': path, 'sheet': sheet, 'max_rows': max_rows})\n"),
        ("xlsx_write", "def xlsx_write(path, sheets):\n    return _call('xlsx_write', {'path': path, 'sheets': sheets})\n"),
        ("docx_read", "def docx_read(path, max_paragraphs=5000):\n    return _call('docx_read', {'path': path, 'max_paragraphs': max_paragraphs})\n"),
        ("docx_write", "def docx_write(path, content=None, title=None, subtitle=None, blocks=None, author=None):\n    return _call('docx_write', {'path': path, 'content': content, 'title': title, 'subtitle': subtitle, 'blocks': blocks, 'author': author})\n"),
        ("pptx_read", "def pptx_read(path, max_slides=200):\n    return _call('pptx_read', {'path': path, 'max_slides': max_slides})\n"),
        ("pptx_write", "def pptx_write(path, slides, title=None, author=None, theme=None):\n    return _call('pptx_write', {'path': path, 'slides': slides, 'title': title, 'author': author, 'theme': theme})\n"),
    ];
    let stubs: String = STUBS
        .iter()
        .filter(|(name, _)| {
            matches!(
                *name,
                "xlsx_read"
                    | "xlsx_write"
                    | "docx_read"
                    | "docx_write"
                    | "pptx_read"
                    | "pptx_write"
            ) || tools.contains(name)
        })
        .map(|(_, source)| *source)
        .collect();
    let exports = [
        "json_parse",
        "shell_quote",
        "retry",
        "xlsx_read",
        "xlsx_write",
        "docx_read",
        "docx_write",
        "pptx_read",
        "pptx_write",
    ]
    .iter()
    .chain(tools.iter())
    .map(|name| format!("{name:?}"))
    .collect::<Vec<_>>()
    .join(", ");
    format!("{KERNEL_HEAD}\n{stubs}\n_EXPORTS = [{exports}]\n{KERNEL_TAIL}")
}

#[cfg(unix)]
const KERNEL_HEAD: &str = r#"
import contextlib, io, json, os, shlex, socket, sys, time, traceback, types

_sock = socket.socket(fileno=3)
_reader = _sock.makefile("r", encoding="utf-8", newline="\n")
_LIMIT = 5000000


class _Capture(io.StringIO):
    def __init__(self, emit=None):
        super().__init__()
        self._emit = emit
        self._buf = ""
        self._last = 0.0

    def write(self, text):
        if self._emit and text:
            self._buf += text
            now = time.monotonic()
            if "\n" in self._buf or "\r" in self._buf or (len(self._buf) >= 120 and now - self._last >= 0.15):
                chunk = self._buf[-512:] if len(self._buf) > 512 else self._buf
                self._buf = ""
                if now - self._last >= 0.08 or "\n" in chunk:
                    self._last = now
                    try:
                        self._emit(chunk)
                    except Exception:
                        pass
        if self.tell() < _LIMIT:
            return super().write(text)
        return len(text)

    def flush_preview(self):
        if self._emit and self._buf:
            chunk = self._buf[-512:] if len(self._buf) > 512 else self._buf
            self._buf = ""
            try:
                self._emit(chunk)
            except Exception:
                pass


def _send(message):
    _sock.sendall((json.dumps(message, ensure_ascii=False) + "\n").encode("utf-8"))


def _emit_output(text):
    _send({"type": "output", "text": text})


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

#[cfg(unix)]
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
    _out, _err = _Capture(_emit_output), _Capture(_emit_output)
    _status, _trace = "ok", ""
    try:
        with contextlib.redirect_stdout(_out), contextlib.redirect_stderr(_err):
            exec(compile(_message["code"], "<cell>", "exec"), GLOBALS)
    except SystemExit as _exit:
        _status, _trace = "exit", "SystemExit: " + repr(_exit.code)
    except BaseException:
        _status, _trace = "error", traceback.format_exc()
    _out.flush_preview()
    _err.flush_preview()
    _send({"type": "done", "status": _status, "stdout": _out.getvalue(),
           "stderr": _err.getvalue(), "traceback": _trace})
"#;

#[cfg(unix)]
struct UnixKernel {
    child: Child,
    pgid: i32,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// What the kernel's processes wrote straight to the terminal's descriptors.
    raw: Arc<StdMutex<Vec<u8>>>,
    tools: Vec<&'static str>,
    cells: u64,
}

enum SessionKernel {
    #[cfg(unix)]
    Unix(Box<UnixKernel>),
    #[cfg(not(target_os = "macos"))]
    Embedded(EmbeddedKernel),
    #[cfg(not(target_os = "macos"))]
    EmbeddedProcess(Box<EmbeddedSubprocessKernel>),
}

impl SessionKernel {
    fn tools(&self) -> &[&'static str] {
        match self {
            #[cfg(unix)]
            Self::Unix(k) => &k.tools,
            #[cfg(not(target_os = "macos"))]
            Self::Embedded(k) => &k.tools,
            #[cfg(not(target_os = "macos"))]
            Self::EmbeddedProcess(k) => &k.tools,
        }
    }

    fn increment_cells(&mut self) -> u64 {
        match self {
            #[cfg(unix)]
            Self::Unix(k) => {
                k.cells += 1;
                k.cells
            }
            #[cfg(not(target_os = "macos"))]
            Self::Embedded(k) => {
                k.cells += 1;
                k.cells
            }
            #[cfg(not(target_os = "macos"))]
            Self::EmbeddedProcess(k) => {
                k.cells += 1;
                k.cells
            }
        }
    }

    fn clear_raw(&self) {
        #[cfg(unix)]
        {
            #[allow(irrefutable_let_patterns)]
            if let Self::Unix(k) = self {
                if let Ok(mut raw) = k.raw.lock() {
                    raw.clear();
                }
            }
        }
    }

    async fn take_raw_text(&self) -> String {
        #[cfg(unix)]
        {
            #[allow(irrefutable_let_patterns)]
            if let Self::Unix(k) = self {
                // Let the reader catch up with what the processes wrote last.
                tokio::time::sleep(Duration::from_millis(30)).await;
                return k
                    .raw
                    .lock()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
            }
        }
        String::new()
    }
}

static KERNELS: LazyLock<Mutex<HashMap<String, SessionKernel>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(unix)]
static GROUPS: LazyLock<StdMutex<HashSet<i32>>> = LazyLock::new(|| StdMutex::new(HashSet::new()));

#[cfg(unix)]
fn kill_group(pgid: i32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

/// Stop every kernel this process started. Called when the process exits.
pub fn kill_all_kernels() {
    #[cfg(unix)]
    if let Ok(groups) = GROUPS.lock() {
        for pgid in groups.iter() {
            kill_group(*pgid);
        }
    }
}

/// End the kernel of a finished conversation (a subagent's).
pub(crate) async fn end_session(session: &str) {
    if let Some(kernel) = KERNELS.lock().await.remove(session) {
        discard_and_reap(kernel).await;
    }
}

async fn discard_and_reap(kernel: SessionKernel) {
    match kernel {
        #[cfg(unix)]
        SessionKernel::Unix(mut k) => {
            kill_group(k.pgid);
            if let Ok(mut groups) = GROUPS.lock() {
                groups.remove(&k.pgid);
            }
            let _ = k.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), k.child.wait()).await;
        }
        #[cfg(not(target_os = "macos"))]
        SessionKernel::Embedded(k) => {
            k.shutdown();
        }
        #[cfg(not(target_os = "macos"))]
        SessionKernel::EmbeddedProcess(mut k) => {
            #[cfg(unix)]
            {
                kill_group(k.pgid);
                if let Ok(mut groups) = GROUPS.lock() {
                    groups.remove(&k.pgid);
                }
            }
            let _ = k.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), k.child.wait()).await;
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn spawn_embedded(
    tools: &[&'static str],
    directory: &Path,
    env: Vec<(String, String)>,
) -> Result<SessionKernel, String> {
    if use_subprocess_worker() {
        if let Ok(exe) = std::env::current_exe() {
            let kernel = EmbeddedSubprocessKernel::spawn(&exe, tools, directory, env).await?;
            #[cfg(unix)]
            if let Ok(mut groups) = GROUPS.lock() {
                groups.insert(kernel.pgid);
            }
            return Ok(SessionKernel::EmbeddedProcess(Box::new(kernel)));
        }
    }
    EmbeddedKernel::spawn(tools, env).map(SessionKernel::Embedded)
}

/// The interpreter: the active virtual environment's, else `python3` on the command PATH.
#[cfg(unix)]
fn interpreter(env: &[(String, String)]) -> Option<PathBuf> {
    #[cfg(not(target_os = "macos"))]
    if std::env::var("HERMES_PYTHON_ENGINE")
        .ok()
        .is_some_and(|v| v.eq_ignore_ascii_case("embedded") || v.eq_ignore_ascii_case("rustpython"))
    {
        return None;
    }
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

#[cfg(unix)]
async fn spawn_unix(
    python: &Path,
    tools: &[&'static str],
    directory: &Path,
    env: Vec<(String, String)>,
) -> Result<UnixKernel, String> {
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
    Ok(UnixKernel {
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

#[allow(clippy::too_many_arguments)]
async fn dispatch_sandbox_tool(
    name: &str,
    raw_args: &Value,
    allowed_tools: &[&'static str],
    workspace_root: &Path,
    cx: &mut ToolContext<'_, '_>,
    deadline: Instant,
    max_calls: usize,
    calls: &mut usize,
) -> Result<Value, ()> {
    if *calls >= max_calls {
        return Ok(
            json!({"error": format!("Tool call limit reached ({max_calls}). No more tool calls allowed in this execution.")}),
        );
    }

    // Native pure-Rust Office (Excel/Word/PowerPoint) helpers available in every execute_code session.
    if name == "xlsx_read" {
        *calls += 1;
        return Ok(excel_tools::xlsx_read(workspace_root, raw_args));
    }
    if name == "docx_read" {
        *calls += 1;
        return Ok(office_tools::docx_read(workspace_root, raw_args));
    }
    if name == "pptx_read" {
        *calls += 1;
        return Ok(office_tools::pptx_read(workspace_root, raw_args));
    }
    if matches!(name, "xlsx_write" | "docx_write" | "pptx_write") {
        *calls += 1;
        if !cx.options.can_write_files() {
            return Ok(approval_error(ApprovalDecision::Unavailable));
        }
        let Some(timeout_secs) = cx.options.approval_timeout_secs else {
            return Ok(approval_error(ApprovalDecision::Unavailable));
        };
        let default_name = match name {
            "docx_write" => "document.docx",
            "pptx_write" => "presentation.pptx",
            _ => "workbook.xlsx",
        };
        let target = raw_args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(default_name)
            .to_owned();
        let action = if workspace_root.join(&target).exists() {
            ApprovalAction::ReplaceFile
        } else {
            ApprovalAction::CreateFile
        };
        let summary = raw_args.to_string();
        let mut ask = |request: ApprovalRequest| (cx.approve)(request);
        let decision = request_approval(
            ApprovalRequest {
                target,
                content: summary,
                action,
                timeout_secs,
            },
            cx.options.approval_policy,
            &mut ask,
        )
        .await;
        if !matches!(decision, ApprovalDecision::Approved) {
            return Ok(approval_error(decision));
        }
        return Ok(match name {
            "docx_write" => office_tools::docx_write(workspace_root, raw_args),
            "pptx_write" => office_tools::pptx_write(workspace_root, raw_args),
            _ => excel_tools::xlsx_write(workspace_root, raw_args),
        });
    }

    if !allowed_tools.contains(&name) {
        let mut names = allowed_tools.to_vec();
        names.extend([
            "xlsx_read",
            "xlsx_write",
            "docx_read",
            "docx_write",
            "pptx_read",
            "pptx_write",
        ]);
        names.sort_unstable();
        return Ok(json!({"error": format!(
            "Tool '{name}' is not available in execute_code. Available: {}",
            names.join(", ")
        )}));
    }

    // Parameters the sandbox does not take, and unset ones, are dropped.
    let mut args = match raw_args {
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
    let outcome = match crate::tools::find(name, cx.options.tools) {
        Some(tool) => tokio::time::timeout_at(deadline.into(), tool.call(cx, &text)).await,
        None => Ok(json!({"error": format!("Unknown tool: {name}")})),
    };
    *calls += 1;
    match outcome {
        Ok(value) => Ok(value),
        Err(_) => Err(()),
    }
}

/// Run one cell on a Unix subprocess kernel.
#[cfg(unix)]
async fn run_unix_cell(
    kernel: &mut UnixKernel,
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
            Some("output") => {
                if let Some(raw) = message["text"].as_str().filter(|s| !s.is_empty()) {
                    if let Some(tx) = &cx.tool_output {
                        let _ = tx.send(clean(raw));
                    }
                }
            }
            Some("call") => {
                let name = message["tool"].as_str().unwrap_or("").to_owned();
                let value = match dispatch_sandbox_tool(
                    &name,
                    &message["args"],
                    &kernel.tools,
                    directory,
                    cx,
                    deadline,
                    max_calls,
                    calls,
                )
                .await
                {
                    Ok(v) => v,
                    Err(()) => return Cell::TimedOut,
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

/// Run one cell on the embedded RustPython kernel.
#[cfg(not(target_os = "macos"))]
async fn run_embedded_cell(
    kernel: &mut EmbeddedKernel,
    code: &str,
    directory: &Path,
    cx: &mut ToolContext<'_, '_>,
    deadline: Instant,
    max_calls: usize,
    calls: &mut usize,
) -> Cell {
    if !kernel.start_cell(code, directory) {
        return Cell::Died;
    }
    let allowed = kernel.tools.clone();
    loop {
        match kernel.next_event(deadline).await {
            EmbeddedEvent::Done(payload) => return Cell::Done(payload),
            EmbeddedEvent::Output(raw) => {
                if !raw.is_empty() {
                    if let Some(tx) = &cx.tool_output {
                        let _ = tx.send(clean(&raw));
                    }
                }
            }
            EmbeddedEvent::TimedOut => return Cell::TimedOut,
            EmbeddedEvent::Died => return Cell::Died,
            EmbeddedEvent::ToolCall { tool, args } => {
                let value = match dispatch_sandbox_tool(
                    &tool, &args, &allowed, directory, cx, deadline, max_calls, calls,
                )
                .await
                {
                    Ok(v) => v,
                    Err(()) => return Cell::TimedOut,
                };
                if !kernel.send_tool_reply(value) {
                    return Cell::Died;
                }
            }
        }
    }
}

/// Run one cell on the embedded RustPython subprocess kernel.
#[cfg(not(target_os = "macos"))]
async fn run_embedded_process_cell(
    kernel: &mut EmbeddedSubprocessKernel,
    code: &str,
    directory: &Path,
    cx: &mut ToolContext<'_, '_>,
    deadline: Instant,
    max_calls: usize,
    calls: &mut usize,
) -> Cell {
    if !kernel.start_cell(code, directory).await {
        return Cell::Died;
    }
    let allowed = kernel.tools.clone();
    loop {
        match kernel.next_event(deadline).await {
            EmbeddedEvent::Done(payload) => return Cell::Done(payload),
            EmbeddedEvent::Output(raw) => {
                if !raw.is_empty() {
                    if let Some(tx) = &cx.tool_output {
                        let _ = tx.send(clean(&raw));
                    }
                }
            }
            EmbeddedEvent::TimedOut => return Cell::TimedOut,
            EmbeddedEvent::Died => return Cell::Died,
            EmbeddedEvent::ToolCall { tool, args } => {
                let value = match dispatch_sandbox_tool(
                    &tool, &args, &allowed, directory, cx, deadline, max_calls, calls,
                )
                .await
                {
                    Ok(v) => v,
                    Err(()) => return Cell::TimedOut,
                };
                if !kernel.send_tool_reply(value).await {
                    return Cell::Died;
                }
            }
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
            .filter(|_| cx.options.can_execute_code())
        else {
            return approval_error(ApprovalDecision::Unavailable);
        };
        let session = cx.options.state_key().to_owned();
        let defaults = crate::terminal::TerminalSettings::default();
        let settings = cx.options.terminal_env.unwrap_or(&defaults);
        let root = cx
            .options
            .root
            .canonical()
            .unwrap_or_else(|_| cx.options.root.to_path_buf());
        let directory = crate::terminal::session_directory(&session, &root);
        let tools = sandbox_tools(&ToolEnv::from_options(cx.options));
        let env = settings.environment();
        let will_use_embedded = {
            #[cfg(not(unix))]
            {
                true
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                interpreter(&env).is_none()
            }
            #[cfg(target_os = "macos")]
            {
                false
            }
        };
        let skip_upfront_approval = will_use_embedded
            && match cx.options.approval_policy {
                Some(policy) => policy.allows_embedded_code(),
                None => crate::approval_policy::installed()
                    .is_none_or(|policy| policy.allows_embedded_code()),
            };
        if !skip_upfront_approval {
            let mut ask = |request: ApprovalRequest| (cx.approve)(request);
            let decision = request_approval(
                ApprovalRequest {
                    target: directory.display().to_string(),
                    content: code.clone(),
                    action: ApprovalAction::RunCode,
                    timeout_secs: approval_timeout,
                },
                cx.options.approval_policy,
                &mut ask,
            )
            .await;
            if !matches!(decision, ApprovalDecision::Approved) {
                return approval_error(decision);
            }
        }
        let started = Instant::now();
        let mut state_reset = args.reset;
        let existing = {
            let mut kernels = KERNELS.lock().await;
            match kernels.remove(&session) {
                Some(kernel) if args.reset || kernel.tools() != tools.as_slice() => {
                    state_reset = true;
                    drop(kernels);
                    discard_and_reap(kernel).await;
                    None
                }
                other => other,
            }
        };
        let reused = existing.is_some();
        let mut kernel = match existing {
            Some(kernel) => kernel,
            None => {
                #[cfg(unix)]
                {
                    if let Some(python) = interpreter(&env) {
                        match spawn_unix(&python, &tools, &directory, env).await {
                            Ok(k) => SessionKernel::Unix(Box::new(k)),
                            Err(message) => {
                                return json!({"status": "error", "error": format!("The kernel could not start: {message}"), "tool_calls_made": 0, "duration_seconds": 0});
                            }
                        }
                    } else {
                        #[cfg(not(target_os = "macos"))]
                        {
                            match spawn_embedded(&tools, &directory, env).await {
                                Ok(k) => k,
                                Err(message) => {
                                    return json!({"status": "error", "error": format!("The embedded Python kernel could not start: {message}"), "tool_calls_made": 0, "duration_seconds": 0});
                                }
                            }
                        }
                        #[cfg(target_os = "macos")]
                        {
                            return json!({"status": "error", "error": "No python3 was found on PATH (or in the active VIRTUAL_ENV / CONDA_PREFIX).", "tool_calls_made": 0, "duration_seconds": 0});
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    match spawn_embedded(&tools, &directory, env).await {
                        Ok(k) => k,
                        Err(message) => {
                            return json!({"status": "error", "error": format!("The embedded Python kernel could not start: {message}"), "tool_calls_made": 0, "duration_seconds": 0});
                        }
                    }
                }
            }
        };
        kernel.clear_raw();
        let timeout = Duration::from_secs(settings.code_timeout.unwrap_or(DEFAULT_TIMEOUT));
        let max_calls = settings
            .code_max_tool_calls
            .unwrap_or(DEFAULT_MAX_TOOL_CALLS);
        let mut calls = 0usize;
        let outcome = match &mut kernel {
            #[cfg(unix)]
            SessionKernel::Unix(k) => {
                run_unix_cell(
                    k,
                    &code,
                    &directory,
                    cx,
                    started + timeout,
                    max_calls,
                    &mut calls,
                )
                .await
            }
            #[cfg(not(target_os = "macos"))]
            SessionKernel::Embedded(k) => {
                run_embedded_cell(
                    k,
                    &code,
                    &directory,
                    cx,
                    started + timeout,
                    max_calls,
                    &mut calls,
                )
                .await
            }
            #[cfg(not(target_os = "macos"))]
            SessionKernel::EmbeddedProcess(k) => {
                run_embedded_process_cell(
                    k,
                    &code,
                    &directory,
                    cx,
                    started + timeout,
                    max_calls,
                    &mut calls,
                )
                .await
            }
        };
        let raw_text = kernel.take_raw_text().await;
        let cells = kernel.increment_cells();
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
                    discard_and_reap(kernel).await;
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
                discard_and_reap(kernel).await;
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
                discard_and_reap(kernel).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tools::FileWorkspace;
    use crate::memory_tool::MemoryTurn;
    use crate::providers::{OpenAiProvider, Provider};
    use crate::skills::SkillCatalog;
    use crate::stall_guards::IntentAckContinuation;
    use crate::todo_tool::TodoStore;
    use crate::tool_policy::ToolPolicy;
    use crate::turn::{ModelRoute, TurnOptions};
    use std::sync::{Arc, Mutex as StdMutex};

    fn test_turn_options<'a>(
        root: &'a Path,
        policy: &'a ToolPolicy,
        intent_ack: &'a IntentAckContinuation,
        approval_timeout_secs: Option<u32>,
    ) -> TurnOptions<'a> {
        TurnOptions {
            mcp: None,
            web: None,
            browser: None,
            memory: None,
            vision: None,
            vision_route: None,
            image_gen: None,
            tts: None,
            x_search: None,
            root,
            profile: None,
            session_id: None,
            single_query: false,
            max_turns: 4,
            approval_timeout_secs,
            terminal_approval_timeout_secs: approval_timeout_secs,
            terminal_local: false,
            compression: None,
            summarizer: None,
            compaction_guard: None,
            keep_phone_numbers: false,
            stall_guards: true,
            intent_ack_continuation: intent_ack,
            terminal_env: None,
            clarify_timeout_secs: 0,
            tools: policy,
            journal: None,
            delegation: None,
            system_prompt_override: None,
            ephemeral_system_prompt: None,
            child: None,
            delegate_depth: 0,
            cancel_flag: None,
            steer_queue: None,
            prefill: None,
            state_scope: None,
            approval_policy: None,
        }
    }

    #[tokio::test]
    async fn office_writes_in_execute_code_require_write_capability_and_approval() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillCatalog::default();
        let policy = ToolPolicy::default();
        let intent_ack = IntentAckContinuation::default();
        let route = ModelRoute {
            provider: Provider::OpenAi(
                OpenAiProvider::new("http://127.0.0.1:1/v1", "test-key", Duration::from_secs(5))
                    .unwrap(),
            ),
            provider_name: "openai",
            model: "test-model".into(),
        };

        let cases = [
            (
                "docx_write",
                "out/report.docx",
                json!({"path": "out/report.docx", "content": "hello"}),
            ),
            (
                "pptx_write",
                "out/deck.pptx",
                json!({"path": "out/deck.pptx", "slides": [{"title": "hi"}]}),
            ),
            (
                "xlsx_write",
                "out/book.xlsx",
                json!({"path": "out/book.xlsx", "sheets": {"S1": [["a", 1]]}}),
            ),
        ];

        for (tool_name, rel_path, payload) in cases {
            // 1. Without write capability (`approval_timeout_secs: None`), write is refused.
            {
                let options = test_turn_options(dir.path(), &policy, &intent_ack, None);
                let mut files = FileWorkspace::new(dir.path(), None);
                let mut memory_turn = MemoryTurn::default();
                let mut todo = TodoStore::default();
                let mut approve = |_: ApprovalRequest| -> BoxFuture<'_, ApprovalDecision> {
                    Box::pin(async { ApprovalDecision::Approved })
                };
                let mut cx = ToolContext {
                    options: &options,
                    skills: &skills,
                    files: &mut files,
                    memory_turn: &mut memory_turn,
                    todo: &mut todo,
                    approve: &mut approve,
                    route: &route,
                    fallbacks: &[],
                    tool_output: None,
                };
                let mut calls = 0;
                let res = dispatch_sandbox_tool(
                    tool_name,
                    &payload,
                    &SANDBOX_TOOLS,
                    dir.path(),
                    &mut cx,
                    Instant::now() + Duration::from_secs(10),
                    10,
                    &mut calls,
                )
                .await
                .unwrap();
                assert_eq!(res["status"], "approval_unavailable", "{tool_name}: {res}");
                assert!(!dir.path().join(rel_path).exists());
            }

            // 2. With write capability enabled, denied approval blocks creation.
            {
                let options = test_turn_options(dir.path(), &policy, &intent_ack, Some(5));
                let mut files = FileWorkspace::new(dir.path(), None);
                let mut memory_turn = MemoryTurn::default();
                let mut todo = TodoStore::default();
                let seen: Arc<StdMutex<Vec<ApprovalAction>>> = Arc::new(StdMutex::new(Vec::new()));
                let seen_cb = Arc::clone(&seen);
                let mut approve = move |req: ApprovalRequest| -> BoxFuture<'_, ApprovalDecision> {
                    seen_cb.lock().unwrap().push(req.action);
                    Box::pin(async { ApprovalDecision::Denied })
                };
                let mut cx = ToolContext {
                    options: &options,
                    skills: &skills,
                    files: &mut files,
                    memory_turn: &mut memory_turn,
                    todo: &mut todo,
                    approve: &mut approve,
                    route: &route,
                    fallbacks: &[],
                    tool_output: None,
                };
                let mut calls = 0;
                let res = dispatch_sandbox_tool(
                    tool_name,
                    &payload,
                    &SANDBOX_TOOLS,
                    dir.path(),
                    &mut cx,
                    Instant::now() + Duration::from_secs(10),
                    10,
                    &mut calls,
                )
                .await
                .unwrap();
                assert_eq!(res["status"], "approval_denied", "{tool_name}: {res}");
                assert_eq!(*seen.lock().unwrap(), vec![ApprovalAction::CreateFile]);
                assert!(!dir.path().join(rel_path).exists());
            }

            // 3. Approved first write requests CreateFile; overwriting requests ReplaceFile.
            {
                let options = test_turn_options(dir.path(), &policy, &intent_ack, Some(5));
                let mut files = FileWorkspace::new(dir.path(), None);
                let mut memory_turn = MemoryTurn::default();
                let mut todo = TodoStore::default();
                let seen: Arc<StdMutex<Vec<ApprovalAction>>> = Arc::new(StdMutex::new(Vec::new()));
                let seen_cb = Arc::clone(&seen);
                let mut approve = move |req: ApprovalRequest| -> BoxFuture<'_, ApprovalDecision> {
                    seen_cb.lock().unwrap().push(req.action);
                    Box::pin(async { ApprovalDecision::Approved })
                };
                let mut cx = ToolContext {
                    options: &options,
                    skills: &skills,
                    files: &mut files,
                    memory_turn: &mut memory_turn,
                    todo: &mut todo,
                    approve: &mut approve,
                    route: &route,
                    fallbacks: &[],
                    tool_output: None,
                };
                let mut calls = 0;
                let first = dispatch_sandbox_tool(
                    tool_name,
                    &payload,
                    &SANDBOX_TOOLS,
                    dir.path(),
                    &mut cx,
                    Instant::now() + Duration::from_secs(10),
                    10,
                    &mut calls,
                )
                .await
                .unwrap();
                assert_eq!(first["status"], "ok", "{tool_name}: {first}");
                assert!(dir.path().join(rel_path).exists());

                let second = dispatch_sandbox_tool(
                    tool_name,
                    &payload,
                    &SANDBOX_TOOLS,
                    dir.path(),
                    &mut cx,
                    Instant::now() + Duration::from_secs(10),
                    10,
                    &mut calls,
                )
                .await
                .unwrap();
                assert_eq!(second["status"], "ok", "{tool_name}: {second}");
                assert_eq!(
                    *seen.lock().unwrap(),
                    vec![ApprovalAction::CreateFile, ApprovalAction::ReplaceFile]
                );
            }
        }
    }
}
