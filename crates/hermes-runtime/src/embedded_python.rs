//! Cross-platform embedded RustPython session kernel for `execute_code`.
//!
//! Runs a persistent RustPython 3 interpreter on a dedicated OS thread per conversation with the
//! frozen Python standard library (`json`, `csv`, `re`, `math`, `statistics`, `decimal`,
//! `datetime`, `collections`, `itertools`, `zipfile`, `xml.etree.ElementTree`, ...) and no
//! external `python3` installation required. Tool calls (`read_file`, `write_file`, `patch`,
//! `search_files`, `web_search`, `web_extract`, `terminal`, `xlsx_read`, `xlsx_write`) bridge back
//! synchronously to the async Tokio host so approvals, redaction, and limits apply identically to
//! the Unix subprocess kernel.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};
use rustpython_vm::builtins::{PyDictRef, PyListRef, PyTupleRef, PyTypeRef};
use rustpython_vm::compiler::Mode;
use rustpython_vm::function::OptionalArg;
use rustpython_vm::signal::{user_signal_channel, UserSignal, UserSignalSender};
use rustpython_vm::{AsObject, Interpreter, PyObjectRef, PyResult, Settings, VirtualMachine};
use serde_json::{json, Value};
use tokio::sync::mpsc as tokio_mpsc;

enum HostCell {
    Run { code: String, cwd: String },
    Shutdown,
}

enum PyEvent {
    ToolCall { tool: String, args_json: String },
    Output(String),
    CellDone(Value),
}

struct AbortState {
    aborted: AtomicBool,
    in_cell: AtomicBool,
    exited: AtomicBool,
    nudge_started: AtomicBool,
    sleep_lock: Mutex<bool>,
    sleep_cv: Condvar,
}

impl AbortState {
    fn new() -> Self {
        Self {
            aborted: AtomicBool::new(false),
            in_cell: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            nudge_started: AtomicBool::new(false),
            sleep_lock: Mutex::new(false),
            sleep_cv: Condvar::new(),
        }
    }
}

struct ExitGuard(Arc<AbortState>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.in_cell.store(false, Ordering::Release);
        self.0.exited.store(true, Ordering::Release);
        BRIDGE.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

fn make_abort_signal(abort: Arc<AbortState>, tx: UserSignalSender) -> UserSignal {
    Box::new(move |vm: &VirtualMachine| {
        if abort.aborted.load(Ordering::Acquire) && abort.in_cell.load(Ordering::Acquire) {
            // Re-arm immediately so `except BaseException:` inside a tight loop cannot swallow
            // the timeout interruption and continue burning CPU.
            let _ = tx.send(make_abort_signal(Arc::clone(&abort), tx.clone()));
            return Err(vm.new_exception_msg(
                vm.ctx.exceptions.keyboard_interrupt.to_owned(),
                "execution timed out".into(),
            ));
        }
        Ok(())
    })
}

enum BridgeTransport {
    InProcess {
        event_tx: tokio_mpsc::UnboundedSender<PyEvent>,
        reply_rx: std_mpsc::Receiver<String>,
    },
    Stdio,
}

struct BridgeState {
    transport: BridgeTransport,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    abort: Arc<AbortState>,
}

static USE_SUBPROCESS_WORKER: AtomicBool = AtomicBool::new(false);

/// Opt in to running the embedded RustPython kernel inside a dedicated killable child process
/// (`<current_exe> __embedded-python`). Called by `hsr` at startup; library unit tests leave
/// this disabled so they test the in-process interpreter directly.
pub fn enable_subprocess_worker() {
    USE_SUBPROCESS_WORKER.store(true, Ordering::Relaxed);
}

pub(crate) fn use_subprocess_worker() -> bool {
    USE_SUBPROCESS_WORKER.load(Ordering::Relaxed)
}

fn write_stdio_line(payload: &Value) -> Result<(), String> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, payload).map_err(|e| e.to_string())?;
    out.write_all(b"\n").map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

thread_local! {
    static BRIDGE: RefCell<Option<BridgeState>> = const { RefCell::new(None) };
}

fn sandbox_environ(vm: &VirtualMachine) -> PyDictRef {
    let dict = vm.ctx.new_dict();
    BRIDGE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            for (k, v) in &state.env {
                #[cfg(windows)]
                {
                    let _ = dict.set_item(k.as_str(), vm.ctx.new_str(v.as_str()).into(), vm);
                }
                #[cfg(not(windows))]
                {
                    let key = vm.ctx.new_bytes(k.as_bytes().to_vec());
                    let val: PyObjectRef = vm.ctx.new_bytes(v.as_bytes().to_vec()).into();
                    let _ = dict.set_item(key.as_object(), val, vm);
                }
            }
        }
    });
    dict
}

fn current_session_cwd() -> PathBuf {
    BRIDGE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|s| s.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    })
}

fn resolve_in_session(raw: Option<String>) -> PathBuf {
    let base = current_session_cwd();
    match raw {
        Some(p) if !p.is_empty() => {
            let candidate = Path::new(&p);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                base.join(candidate)
            }
        }
        _ => base,
    }
}

fn sandbox_chdir(path: String, vm: &VirtualMachine) -> PyResult<()> {
    let target = resolve_in_session(Some(path));
    if !target.is_dir() {
        return Err(vm.new_os_error(format!("directory not found: {}", target.display())));
    }
    BRIDGE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.cwd = target;
        }
    });
    Ok(())
}

fn sandbox_listdir(path: OptionalArg<String>, vm: &VirtualMachine) -> PyResult<PyListRef> {
    let dir = resolve_in_session(path.into_option());
    let rd = std::fs::read_dir(&dir)
        .map_err(|e| vm.new_os_error(format!("listdir {}: {e}", dir.display())))?;
    let mut names: Vec<PyObjectRef> = Vec::new();
    for entry in rd.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            names.push(vm.ctx.new_str(name).into());
        }
    }
    Ok(vm.ctx.new_list(names))
}

fn sandbox_urandom(size: usize, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
    let mut buf = vec![0u8; size.min(1024 * 1024)];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| vm.new_os_error("CSPRNG failure"))?;
    Ok(buf)
}

fn sandbox_stat(path: String, follow_symlinks: bool, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
    let target = resolve_in_session(Some(path));
    let meta = if follow_symlinks {
        std::fs::metadata(&target)
    } else {
        std::fs::symlink_metadata(&target)
    }
    .map_err(|e| vm.new_os_error(format!("stat {}: {e}", target.display())))?;
    let mode: u32 = if meta.is_dir() {
        0o040_755
    } else if meta.is_symlink() {
        0o120_777
    } else {
        0o100_644
    };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let items: Vec<PyObjectRef> = vec![
        vm.ctx.new_int(mode).into(),
        vm.ctx.new_int(0).into(),
        vm.ctx.new_int(0).into(),
        vm.ctx.new_int(1).into(),
        vm.ctx.new_int(0).into(),
        vm.ctx.new_int(0).into(),
        vm.ctx.new_int(size).into(),
        vm.ctx.new_float(mtime).into(),
        vm.ctx.new_float(mtime).into(),
        vm.ctx.new_float(mtime).into(),
    ];
    Ok(vm.ctx.new_tuple(items))
}

fn sandbox_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn sandbox_exit(code: OptionalArg<i32>, vm: &VirtualMachine) -> PyResult<()> {
    let c = code.unwrap_or(0);
    let arg: PyObjectRef = vm.ctx.new_int(c).into();
    Err(vm.new_system_exit(vec![arg].into()))
}

#[cfg(not(windows))]
#[rustpython_vm::pymodule(name = "posix")]
mod _sandbox_os {
    use super::*;

    #[pyattr]
    const F_OK: i32 = 0;
    #[pyattr]
    const R_OK: i32 = 4;
    #[pyattr]
    const W_OK: i32 = 2;
    #[pyattr]
    const X_OK: i32 = 1;

    #[pyattr]
    fn environ(vm: &VirtualMachine) -> PyDictRef {
        sandbox_environ(vm)
    }

    #[pyattr]
    fn error(vm: &VirtualMachine) -> PyTypeRef {
        vm.ctx.exceptions.os_error.to_owned()
    }

    #[pyattr]
    fn _have_functions(vm: &VirtualMachine) -> PyListRef {
        vm.ctx.new_list(Vec::new())
    }

    #[pyfunction]
    fn getcwd() -> String {
        current_session_cwd().display().to_string()
    }

    #[pyfunction]
    fn chdir(path: String, vm: &VirtualMachine) -> PyResult<()> {
        sandbox_chdir(path, vm)
    }

    #[pyfunction]
    fn listdir(path: OptionalArg<String>, vm: &VirtualMachine) -> PyResult<PyListRef> {
        sandbox_listdir(path, vm)
    }

    #[pyfunction]
    fn stat(path: String, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        sandbox_stat(path, true, vm)
    }

    #[pyfunction]
    fn lstat(path: String, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        sandbox_stat(path, false, vm)
    }

    #[pyfunction]
    fn cpu_count() -> usize {
        sandbox_cpu_count()
    }

    #[pyfunction]
    fn getpid() -> u32 {
        std::process::id()
    }

    #[pyfunction]
    fn urandom(size: usize, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
        sandbox_urandom(size, vm)
    }

    #[pyfunction]
    fn _exit(code: OptionalArg<i32>, vm: &VirtualMachine) -> PyResult<()> {
        sandbox_exit(code, vm)
    }
}

#[cfg(windows)]
#[rustpython_vm::pymodule(name = "nt")]
mod _sandbox_os {
    use super::*;

    #[pyattr]
    const F_OK: i32 = 0;
    #[pyattr]
    const R_OK: i32 = 4;
    #[pyattr]
    const W_OK: i32 = 2;
    #[pyattr]
    const X_OK: i32 = 1;

    #[pyattr]
    fn environ(vm: &VirtualMachine) -> PyDictRef {
        sandbox_environ(vm)
    }

    #[pyattr]
    fn error(vm: &VirtualMachine) -> PyTypeRef {
        vm.ctx.exceptions.os_error.to_owned()
    }

    #[pyattr]
    fn _have_functions(vm: &VirtualMachine) -> PyListRef {
        vm.ctx.new_list(Vec::new())
    }

    #[pyfunction]
    fn getcwd() -> String {
        current_session_cwd().display().to_string()
    }

    #[pyfunction]
    fn chdir(path: String, vm: &VirtualMachine) -> PyResult<()> {
        sandbox_chdir(path, vm)
    }

    #[pyfunction]
    fn listdir(path: OptionalArg<String>, vm: &VirtualMachine) -> PyResult<PyListRef> {
        sandbox_listdir(path, vm)
    }

    #[pyfunction]
    fn stat(path: String, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        sandbox_stat(path, true, vm)
    }

    #[pyfunction]
    fn lstat(path: String, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        sandbox_stat(path, false, vm)
    }

    #[pyfunction]
    fn cpu_count() -> usize {
        sandbox_cpu_count()
    }

    #[pyfunction]
    fn getpid() -> u32 {
        std::process::id()
    }

    #[pyfunction]
    fn urandom(size: usize, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
        sandbox_urandom(size, vm)
    }

    #[pyfunction]
    fn _exit(code: OptionalArg<i32>, vm: &VirtualMachine) -> PyResult<()> {
        sandbox_exit(code, vm)
    }
}

/// Sandboxed `winreg` stub for Windows: Python's frozen `importlib._bootstrap_external`
/// unconditionally executes `import winreg` when `sys.platform == "win32"`, and catches
/// `OSError` from `OpenKey`/`QueryValue`. Exposing this stub lets the stdlib import machinery
/// bootstrap cleanly on Windows without granting sandboxed scripts access to the Windows Registry.
#[cfg_attr(not(windows), allow(dead_code))]
#[rustpython_vm::pymodule(name = "winreg")]
mod _sandbox_winreg {
    use super::*;

    #[pyattr]
    const HKEY_CLASSES_ROOT: u64 = 0x8000_0000;
    #[pyattr]
    const HKEY_CURRENT_USER: u64 = 0x8000_0001;
    #[pyattr]
    const HKEY_LOCAL_MACHINE: u64 = 0x8000_0002;
    #[pyattr]
    const HKEY_USERS: u64 = 0x8000_0003;
    #[pyattr]
    const HKEY_CURRENT_CONFIG: u64 = 0x8000_0005;
    #[pyattr]
    const KEY_READ: u32 = 0x20019;
    #[pyattr]
    const REG_SZ: u32 = 1;

    fn disabled(vm: &VirtualMachine) -> PyResult<PyObjectRef> {
        Err(vm.new_os_error("Windows registry access is disabled in the Python sandbox"))
    }

    #[pyfunction(name = "OpenKey")]
    fn open_key(
        _key: PyObjectRef,
        _sub_key: OptionalArg<PyObjectRef>,
        _reserved: OptionalArg<PyObjectRef>,
        _access: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        disabled(vm)
    }

    #[pyfunction(name = "OpenKeyEx")]
    fn open_key_ex(
        _key: PyObjectRef,
        _sub_key: OptionalArg<PyObjectRef>,
        _reserved: OptionalArg<PyObjectRef>,
        _access: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        disabled(vm)
    }

    #[pyfunction(name = "QueryValue")]
    fn query_value(
        _key: PyObjectRef,
        _sub_key: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        disabled(vm)
    }

    #[pyfunction(name = "QueryValueEx")]
    fn query_value_ex(
        _key: PyObjectRef,
        _value_name: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        disabled(vm)
    }

    #[pyfunction(name = "EnumKey")]
    fn enum_key(
        _key: PyObjectRef,
        _index: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<PyObjectRef> {
        disabled(vm)
    }
}

#[rustpython_vm::pymodule]
mod _hermes_bridge {
    use super::*;

    #[pyfunction]
    fn call_tool(tool: String, args_json: String, vm: &VirtualMachine) -> PyResult<String> {
        BRIDGE.with(|slot| {
            let borrow = slot.borrow();
            let Some(state) = borrow.as_ref() else {
                return Err(vm.new_runtime_error("hermes tool bridge is not initialized"));
            };
            if state.abort.aborted.load(Ordering::Acquire) {
                return Err(vm.new_exception_msg(
                    vm.ctx.exceptions.keyboard_interrupt.to_owned(),
                    "execution timed out".into(),
                ));
            }
            match &state.transport {
                BridgeTransport::InProcess { event_tx, reply_rx } => {
                    event_tx
                        .send(PyEvent::ToolCall { tool, args_json })
                        .map_err(|_| vm.new_runtime_error("host closed tool bridge"))?;
                    reply_rx
                        .recv()
                        .map_err(|_| vm.new_runtime_error("host closed tool reply channel"))
                }
                BridgeTransport::Stdio => {
                    let args: Value = serde_json::from_str(&args_json)
                        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
                    write_stdio_line(&json!({"type": "call", "tool": tool, "args": args}))
                        .map_err(|_| vm.new_runtime_error("host closed tool bridge"))?;
                    let mut line = String::new();
                    let n = std::io::stdin()
                        .read_line(&mut line)
                        .map_err(|_| vm.new_runtime_error("host closed tool reply channel"))?;
                    if n == 0 {
                        return Err(vm.new_runtime_error("host closed tool reply channel"));
                    }
                    Ok(line)
                }
            }
        })
    }

    #[pyfunction]
    fn emit_output(text: String) {
        if text.is_empty() {
            return;
        }
        BRIDGE.with(|slot| {
            let borrow = slot.borrow();
            let Some(state) = borrow.as_ref() else {
                return;
            };
            if state.abort.aborted.load(Ordering::Acquire) {
                return;
            }
            match &state.transport {
                BridgeTransport::InProcess { event_tx, .. } => {
                    let _ = event_tx.send(PyEvent::Output(text));
                }
                BridgeTransport::Stdio => {
                    let _ = write_stdio_line(&json!({"type": "output", "text": text}));
                }
            }
        });
    }

    #[pyfunction]
    fn sleep(object: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        let dur = object.try_into_value::<Duration>(vm).map_err(|e| {
            if e.class().is(vm.ctx.exceptions.value_error) {
                return vm.new_value_error("sleep length must be non-negative");
            }
            e
        })?;
        if dur.is_zero() {
            return vm.check_signals();
        }
        let abort = BRIDGE.with(|slot| slot.borrow().as_ref().map(|s| Arc::clone(&s.abort)));
        let Some(abort) = abort else {
            std::thread::sleep(dur);
            return Ok(());
        };
        let deadline = Instant::now() + dur;
        let mut guard = abort
            .sleep_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if *guard || abort.aborted.load(Ordering::Acquire) {
                return Err(vm.new_exception_msg(
                    vm.ctx.exceptions.keyboard_interrupt.to_owned(),
                    "execution timed out".into(),
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let (next, result) = abort
                .sleep_cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
            if result.timed_out() && !*guard && !abort.aborted.load(Ordering::Acquire) {
                return Ok(());
            }
        }
    }
}

fn bootstrap_source(tools: &[&str]) -> String {
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
        .map(|(_, src)| *src)
        .collect();
    let mut export_names: Vec<&str> = vec![
        "json_parse",
        "shell_quote",
        "retry",
        "xlsx_read",
        "xlsx_write",
        "docx_read",
        "docx_write",
        "pptx_read",
        "pptx_write",
    ];
    for t in tools {
        if !export_names.contains(t) {
            export_names.push(t);
        }
    }
    let exports = export_names
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"
import _hermes_bridge
import contextlib, io, json, os, shlex, sys, time, traceback, types

time.sleep = _hermes_bridge.sleep

class _StatResult(tuple):
    st_mode = property(lambda self: self[0])
    st_ino = property(lambda self: self[1])
    st_dev = property(lambda self: self[2])
    st_nlink = property(lambda self: self[3])
    st_uid = property(lambda self: self[4])
    st_gid = property(lambda self: self[5])
    st_size = property(lambda self: self[6])
    st_atime = property(lambda self: self[7])
    st_mtime = property(lambda self: self[8])
    st_ctime = property(lambda self: self[9])

_raw_stat = getattr(os, "stat", None)
if callable(_raw_stat):
    def _wrapped_stat(path, *args, **kwargs):
        res = _raw_stat(path)
        return res if hasattr(res, "st_size") else _StatResult(res)
    os.stat = _wrapped_stat
_raw_lstat = getattr(os, "lstat", None)
if callable(_raw_lstat):
    def _wrapped_lstat(path, *args, **kwargs):
        res = _raw_lstat(path)
        return res if hasattr(res, "st_size") else _StatResult(res)
    os.lstat = _wrapped_lstat
os.stat_result = _StatResult

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

def _call(tool, args):
    raw = _hermes_bridge.call_tool(tool, json.dumps(args, ensure_ascii=False))
    return json.loads(raw)["value"]

def json_parse(text):
    if isinstance(text, str) and text.startswith("\ufeff"):
        text = text[1:]
    return json.loads(text, strict=False)

def shell_quote(s):
    return shlex.quote(s)

def retry(fn, max_attempts=3, delay=2):
    last_err = None
    for attempt in range(max_attempts):
        try:
            return fn()
        except Exception as e:
            last_err = e
            if attempt < max_attempts - 1:
                time.sleep(delay * (2 ** attempt))
    raise last_err

{stubs}
_EXPORTS = [{exports}]
_module = types.ModuleType("hermes_tools")
for _name in _EXPORTS:
    setattr(_module, _name, globals()[_name])
sys.modules["hermes_tools"] = _module

GLOBALS = {{"__name__": "__main__", "__builtins__": __builtins__}}
_paths = set()

def _hermes_run_cell(code, cwd):
    try:
        if cwd:
            os.chdir(cwd)
            if cwd not in _paths:
                _paths.add(cwd)
                sys.path.insert(0, cwd)
    except Exception:
        pass
    _out, _err = _Capture(_hermes_bridge.emit_output), _Capture(_hermes_bridge.emit_output)
    _status, _trace = "ok", ""
    try:
        with contextlib.redirect_stdout(_out), contextlib.redirect_stderr(_err):
            exec(compile(code, "<cell>", "exec"), GLOBALS)
    except SystemExit as _exit:
        _status, _trace = "exit", "SystemExit: " + repr(_exit.code)
    except BaseException:
        _status, _trace = "error", traceback.format_exc()
    _out.flush_preview()
    _err.flush_preview()
    return json.dumps({{
        "type": "done",
        "status": _status,
        "stdout": _out.getvalue(),
        "stderr": _err.getvalue(),
        "traceback": _trace,
    }}, ensure_ascii=False)
"#
    )
}

pub(crate) struct EmbeddedKernel {
    cell_tx: std_mpsc::Sender<HostCell>,
    reply_tx: Mutex<Option<std_mpsc::Sender<String>>>,
    signal_tx: UserSignalSender,
    abort: Arc<AbortState>,
    event_rx: tokio_mpsc::UnboundedReceiver<PyEvent>,
    pub(crate) tools: Vec<&'static str>,
    pub(crate) cells: u64,
}

pub(crate) enum EmbeddedEvent {
    ToolCall { tool: String, args: Value },
    Output(String),
    Done(Value),
    TimedOut,
    Died,
}

impl EmbeddedKernel {
    pub(crate) fn spawn(
        tools: &[&'static str],
        env: Vec<(String, String)>,
    ) -> Result<Self, String> {
        let (cell_tx, cell_rx) = std_mpsc::channel::<HostCell>();
        let (reply_tx, reply_rx) = std_mpsc::channel::<String>();
        let (signal_tx, signal_rx) = user_signal_channel();
        let (event_tx, event_rx) = tokio_mpsc::unbounded_channel::<PyEvent>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();
        let abort = Arc::new(AbortState::new());
        let abort_worker = Arc::clone(&abort);
        let boot_src = bootstrap_source(tools);
        let tools_owned = tools.to_vec();

        std::thread::Builder::new()
            .name("hsr-rustpython".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let _exit_guard = ExitGuard(Arc::clone(&abort_worker));
                BRIDGE.with(|slot| {
                    *slot.borrow_mut() = Some(BridgeState {
                        transport: BridgeTransport::InProcess {
                            event_tx: event_tx.clone(),
                            reply_rx,
                        },
                        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                        env,
                        abort: Arc::clone(&abort_worker),
                    });
                });

                let builder = Interpreter::builder(Settings::default());
                let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
                let os_def = _sandbox_os::module_def(&builder.ctx);
                let bridge_def = _hermes_bridge::module_def(&builder.ctx);
                #[cfg(windows)]
                let winreg_def = _sandbox_winreg::module_def(&builder.ctx);
                let builder = builder
                    .add_native_modules(&stdlib_defs)
                    .add_native_module(os_def)
                    .add_native_module(bridge_def);
                #[cfg(windows)]
                let builder = builder.add_native_module(winreg_def);
                let interp = builder
                    .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
                    .init_hook(move |vm| {
                        vm.set_user_signal_channel(signal_rx);
                    })
                    .build();

                interp.enter(|vm| {
                    let scope = vm.new_scope_with_builtins();
                    let boot_code = match vm.compile(&boot_src, Mode::Exec, "<hermes-kernel>") {
                        Ok(c) => c,
                        Err(err) => {
                            let _ = ready_tx.send(Err(format!("kernel compile error: {err}")));
                            return;
                        }
                    };
                    if let Err(exc) = vm.run_code_obj(boot_code, scope.clone()) {
                        let mut msg = String::new();
                        let _ = vm.write_exception(&mut msg, &exc);
                        let _ = ready_tx.send(Err(format!("kernel init error: {msg}")));
                        return;
                    }
                    let runner = match scope.globals.get_item("_hermes_run_cell", vm) {
                        Ok(fn_obj) => fn_obj,
                        Err(_) => {
                            let _ = ready_tx.send(Err("kernel runner missing".into()));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));

                    while let Ok(msg) = cell_rx.recv() {
                        if abort_worker.aborted.load(Ordering::Acquire) {
                            break;
                        }
                        match msg {
                            HostCell::Shutdown => break,
                            HostCell::Run { code, cwd } => {
                                let py_code: PyObjectRef = vm.ctx.new_str(code).into();
                                let py_cwd: PyObjectRef = vm.ctx.new_str(cwd).into();
                                abort_worker.in_cell.store(true, Ordering::Release);
                                let call_result = runner.call((py_code, py_cwd), vm);
                                abort_worker.in_cell.store(false, Ordering::Release);
                                if abort_worker.aborted.load(Ordering::Acquire) {
                                    break;
                                }
                                let payload = match call_result {
                                    Ok(res) => match res.str(vm) {
                                        Ok(s) => serde_json::from_str::<Value>(
                                            &s.as_wtf8().to_string_lossy(),
                                        )
                                        .unwrap_or_else(|_| {
                                            json!({
                                                "type": "done",
                                                "status": "error",
                                                "stdout": "",
                                                "stderr": "",
                                                "traceback": "invalid JSON from embedded kernel"
                                            })
                                        }),
                                        Err(_) => json!({
                                            "type": "done",
                                            "status": "error",
                                            "stdout": "",
                                            "stderr": "",
                                            "traceback": "failed converting embedded kernel result"
                                        }),
                                    },
                                    Err(exc) => {
                                        let mut trace = String::new();
                                        let _ = vm.write_exception(&mut trace, &exc);
                                        json!({
                                            "type": "done",
                                            "status": "error",
                                            "stdout": "",
                                            "stderr": "",
                                            "traceback": trace
                                        })
                                    }
                                };
                                if event_tx.send(PyEvent::CellDone(payload)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            })
            .map_err(|e| format!("failed spawning embedded Python thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cell_tx,
                reply_tx: Mutex::new(Some(reply_tx)),
                signal_tx,
                abort,
                event_rx,
                tools: tools_owned,
                cells: 0,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("embedded Python thread exited during startup".into()),
        }
    }

    pub(crate) fn shutdown(&self) {
        self.abort.aborted.store(true, Ordering::Release);
        {
            let mut guard = self
                .abort
                .sleep_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = true;
            self.abort.sleep_cv.notify_all();
        }
        // Closing reply_tx immediately wakes any cell blocked inside `_hermes_bridge.call_tool`.
        if let Ok(mut tx) = self.reply_tx.lock() {
            tx.take();
        }
        let _ = self.cell_tx.send(HostCell::Shutdown);
        let _ = self.signal_tx.send(make_abort_signal(
            Arc::clone(&self.abort),
            self.signal_tx.clone(),
        ));
        // If another embedded VM thread cleared the process-wide EVAL_BREAKER bit before this
        // worker inspected its own signal queue, keep nudging briefly until this worker exits.
        if !self.abort.exited.load(Ordering::Acquire)
            && !self.abort.nudge_started.swap(true, Ordering::AcqRel)
        {
            let abort = Arc::clone(&self.abort);
            let signal_tx = self.signal_tx.clone();
            let _ = std::thread::Builder::new()
                .name("hsr-rustpython-abort".into())
                .spawn(move || {
                    for _ in 0..200 {
                        if abort.exited.load(Ordering::Acquire) {
                            break;
                        }
                        if signal_tx
                            .send(make_abort_signal(Arc::clone(&abort), signal_tx.clone()))
                            .is_err()
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
        }
    }

    pub(crate) fn start_cell(&self, code: &str, directory: &Path) -> bool {
        self.cell_tx
            .send(HostCell::Run {
                code: code.to_owned(),
                cwd: directory.display().to_string(),
            })
            .is_ok()
    }

    pub(crate) async fn next_event(&mut self, deadline: Instant) -> EmbeddedEvent {
        match tokio::time::timeout_at(deadline.into(), self.event_rx.recv()).await {
            Err(_) => EmbeddedEvent::TimedOut,
            Ok(None) => EmbeddedEvent::Died,
            Ok(Some(PyEvent::CellDone(payload))) => EmbeddedEvent::Done(payload),
            Ok(Some(PyEvent::Output(text))) => EmbeddedEvent::Output(text),
            Ok(Some(PyEvent::ToolCall { tool, args_json })) => {
                let args = serde_json::from_str::<Value>(&args_json)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
                EmbeddedEvent::ToolCall { tool, args }
            }
        }
    }

    pub(crate) fn send_tool_reply(&self, value: Value) -> bool {
        let reply = json!({"value": value}).to_string();
        self.reply_tx
            .lock()
            .ok()
            .and_then(|tx| tx.as_ref().map(|sender| sender.send(reply).is_ok()))
            .unwrap_or(false)
    }
}

impl Drop for EmbeddedKernel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Entry point for the dedicated killable child process (`hsr __embedded-python`).
///
/// Communicates with the host over stdin/stdout JSON lines using the same cell + tool-bridge
/// protocol as the in-process kernel, so timeouts terminate the OS process cleanly even inside
/// long-running builtins or tight loops.
///
/// Runs the RustPython VM on a dedicated 8 MiB stack thread because Windows MSVC binaries default
/// to a 1 MiB main thread stack, which overflows while initializing/compiling frozen stdlib code.
pub fn run_stdio_worker() -> Result<u8, String> {
    let handle = std::thread::Builder::new()
        .name("hsr-rustpython".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_stdio_worker_inner)
        .map_err(|e| format!("failed spawning embedded Python worker thread: {e}"))?;
    match handle.join() {
        Ok(res) => res,
        Err(_) => Err("embedded Python worker panicked".into()),
    }
}

fn run_stdio_worker_inner() -> Result<u8, String> {
    let mut init_line = String::new();
    let n = std::io::stdin()
        .read_line(&mut init_line)
        .map_err(|e| format!("failed reading init frame: {e}"))?;
    if n == 0 {
        return Ok(0);
    }
    #[derive(serde::Deserialize)]
    struct InitFrame {
        tools: Vec<String>,
        env: Vec<(String, String)>,
    }
    let init: InitFrame = serde_json::from_str(&init_line)
        .map_err(|e| format!("invalid embedded worker init frame: {e}"))?;
    let tool_refs: Vec<&str> = init.tools.iter().map(String::as_str).collect();
    let boot_src = bootstrap_source(&tool_refs);
    let abort = Arc::new(AbortState::new());
    let _exit_guard = ExitGuard(Arc::clone(&abort));

    BRIDGE.with(|slot| {
        *slot.borrow_mut() = Some(BridgeState {
            transport: BridgeTransport::Stdio,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: init.env,
            abort,
        });
    });

    let builder = Interpreter::builder(Settings::default());
    let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
    let os_def = _sandbox_os::module_def(&builder.ctx);
    let bridge_def = _hermes_bridge::module_def(&builder.ctx);
    #[cfg(windows)]
    let winreg_def = _sandbox_winreg::module_def(&builder.ctx);
    let builder = builder
        .add_native_modules(&stdlib_defs)
        .add_native_module(os_def)
        .add_native_module(bridge_def);
    #[cfg(windows)]
    let builder = builder.add_native_module(winreg_def);
    let interp = builder
        .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
        .build();

    let result: Result<(), String> = interp.enter(|vm| {
        let scope = vm.new_scope_with_builtins();
        let boot_code = match vm.compile(&boot_src, Mode::Exec, "<hermes-kernel>") {
            Ok(c) => c,
            Err(err) => {
                let msg = format!("kernel compile error: {err}");
                let _ = write_stdio_line(&json!({"type": "init_error", "error": msg}));
                return Err(msg);
            }
        };
        if let Err(exc) = vm.run_code_obj(boot_code, scope.clone()) {
            let mut msg = String::new();
            let _ = vm.write_exception(&mut msg, &exc);
            let err = format!("kernel init error: {msg}");
            let _ = write_stdio_line(&json!({"type": "init_error", "error": err}));
            return Err(err);
        }
        let runner = match scope.globals.get_item("_hermes_run_cell", vm) {
            Ok(fn_obj) => fn_obj,
            Err(_) => {
                let err = "kernel runner missing".to_owned();
                let _ = write_stdio_line(&json!({"type": "init_error", "error": err}));
                return Err(err);
            }
        };
        write_stdio_line(&json!({"type": "ready"}))?;

        loop {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                break;
            };
            match msg["type"].as_str() {
                Some("shutdown") => break,
                Some("cell") => {
                    let code = msg["code"].as_str().unwrap_or("");
                    let cwd = msg["cwd"].as_str().unwrap_or("");
                    let py_code: PyObjectRef = vm.ctx.new_str(code).into();
                    let py_cwd: PyObjectRef = vm.ctx.new_str(cwd).into();
                    let payload = match runner.call((py_code, py_cwd), vm) {
                        Ok(res) => match res.str(vm) {
                            Ok(s) => serde_json::from_str::<Value>(&s.as_wtf8().to_string_lossy())
                                .unwrap_or_else(|_| {
                                    json!({
                                        "type": "done",
                                        "status": "error",
                                        "stdout": "",
                                        "stderr": "",
                                        "traceback": "invalid JSON from embedded kernel"
                                    })
                                }),
                            Err(_) => json!({
                                "type": "done",
                                "status": "error",
                                "stdout": "",
                                "stderr": "",
                                "traceback": "failed converting embedded kernel result"
                            }),
                        },
                        Err(exc) => {
                            let mut trace = String::new();
                            let _ = vm.write_exception(&mut trace, &exc);
                            json!({
                                "type": "done",
                                "status": "error",
                                "stdout": "",
                                "stderr": "",
                                "traceback": trace
                            })
                        }
                    };
                    if write_stdio_line(&payload).is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    });

    result.map(|()| 0)
}

pub(crate) struct EmbeddedSubprocessKernel {
    pub(crate) child: tokio::process::Child,
    #[cfg(unix)]
    pub(crate) pgid: i32,
    reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    writer: tokio::process::ChildStdin,
    pub(crate) tools: Vec<&'static str>,
    pub(crate) cells: u64,
}

impl EmbeddedSubprocessKernel {
    pub(crate) async fn spawn(
        exe: &Path,
        tools: &[&'static str],
        directory: &Path,
        env: Vec<(String, String)>,
    ) -> Result<Self, String> {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::process::Command;

        let mut command = Command::new(exe);
        command
            .arg("__embedded-python")
            .env_clear()
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in [
            "PATH",
            "PATHEXT",
            "LD_LIBRARY_PATH",
            "SYSTEMROOT",
            "COMSPEC",
            "WINDIR",
            "TEMP",
            "TMP",
            "TMPDIR",
        ] {
            if let Some(val) = std::env::var_os(key) {
                command.env(key, val);
            }
        }
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

        let mut child = command.spawn().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let pgid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or("embedded Python worker did not start")?;
        let mut writer = child
            .stdin
            .take()
            .ok_or("embedded Python worker stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("embedded Python worker stdout unavailable")?;
        let mut reader = BufReader::new(stdout);

        let init = json!({"type": "init", "tools": tools, "env": env});
        writer
            .write_all(format!("{init}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        writer.flush().await.map_err(|e| e.to_string())?;

        let mut ready_line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut ready_line))
            .await
            .map_err(|_| "embedded Python worker timed out during startup".to_owned())?
            .map_err(|e| e.to_string())?;
        if read == 0 {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err("embedded Python worker exited during startup".into());
        }
        let msg: Value = serde_json::from_str(&ready_line)
            .map_err(|_| "invalid startup response from embedded Python worker".to_owned())?;
        if msg["type"].as_str() != Some("ready") {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let err = msg["error"]
                .as_str()
                .unwrap_or("embedded Python worker failed to initialize");
            return Err(err.to_owned());
        }

        Ok(Self {
            child,
            #[cfg(unix)]
            pgid,
            reader,
            writer,
            tools: tools.to_vec(),
            cells: 0,
        })
    }

    pub(crate) async fn start_cell(&mut self, code: &str, directory: &Path) -> bool {
        use tokio::io::AsyncWriteExt;
        let cell = json!({"type": "cell", "code": code, "cwd": directory.display().to_string()});
        self.writer
            .write_all(format!("{cell}\n").as_bytes())
            .await
            .is_ok()
            && self.writer.flush().await.is_ok()
    }

    pub(crate) async fn next_event(&mut self, deadline: Instant) -> EmbeddedEvent {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        match tokio::time::timeout_at(deadline.into(), self.reader.read_line(&mut line)).await {
            Err(_) => EmbeddedEvent::TimedOut,
            Ok(Err(_) | Ok(0)) => EmbeddedEvent::Died,
            Ok(Ok(_)) => {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    return EmbeddedEvent::Died;
                };
                match msg["type"].as_str() {
                    Some("done") => EmbeddedEvent::Done(msg),
                    Some("output") => {
                        EmbeddedEvent::Output(msg["text"].as_str().unwrap_or_default().to_owned())
                    }
                    Some("call") => EmbeddedEvent::ToolCall {
                        tool: msg["tool"].as_str().unwrap_or("").to_owned(),
                        args: msg["args"].clone(),
                    },
                    _ => EmbeddedEvent::Died,
                }
            }
        }
    }

    pub(crate) async fn send_tool_reply(&mut self, value: Value) -> bool {
        use tokio::io::AsyncWriteExt;
        let reply = json!({"value": value});
        self.writer
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .is_ok()
            && self.writer.flush().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn run_test_cell(
        kernel: &mut EmbeddedKernel,
        code: &str,
        dir: &Path,
        deadline: Instant,
    ) -> Value {
        assert!(kernel.start_cell(code, dir));
        loop {
            match kernel.next_event(deadline).await {
                EmbeddedEvent::Done(payload) => return payload,
                EmbeddedEvent::Output(_) => {}
                EmbeddedEvent::ToolCall { tool, args } => {
                    let val = match tool.as_str() {
                        "xlsx_write" => crate::excel_tools::xlsx_write(dir, None, &args),
                        "xlsx_read" => crate::excel_tools::xlsx_read(dir, None, &args),
                        "docx_write" => crate::office_tools::docx_write(dir, None, &args),
                        "docx_read" => crate::office_tools::docx_read(dir, None, &args),
                        "pptx_write" => crate::office_tools::pptx_write(dir, None, &args),
                        "pptx_read" => crate::office_tools::pptx_read(dir, None, &args),
                        other => json!({"error": format!("unexpected {other}")}),
                    };
                    assert!(kernel.send_tool_reply(val));
                }
                EmbeddedEvent::TimedOut => panic!("timed out"),
                EmbeddedEvent::Died => panic!("died"),
            }
        }
    }

    #[tokio::test]
    async fn embedded_rustpython_persists_state_uses_stdlib_and_bridges_xlsx() {
        let dir = tempfile::tempdir().unwrap();
        let mut kernel =
            EmbeddedKernel::spawn(&["read_file"], Vec::new()).expect("spawn embedded kernel");
        let deadline = Instant::now() + Duration::from_secs(30);

        // Cell 1: stdlib + xlsx_write + persistent variable
        let code1 = r#"
import json, statistics
from hermes_tools import xlsx_write, xlsx_read

vals = [10, 20, 30, 40]
avg = statistics.mean(vals)
res = xlsx_write("out.xlsx", {
    "Data": [
        ["Metric", "Value"],
        ["Mean", avg],
        ["Sum", "=SUM(B2:B2)"],
    ]
})
assert res["status"] == "ok", res
print(json.dumps({"avg": avg, "rows": res["rows_written"]}))
"#;
        let p1 = run_test_cell(&mut kernel, code1, dir.path(), deadline).await;
        assert_eq!(p1["status"], "ok", "{p1}");
        assert!(
            p1["stdout"].as_str().unwrap().contains("\"avg\": 25"),
            "{p1}"
        );

        // Cell 2: verifies `avg` persisted from Cell 1 and reads back `out.xlsx`
        let code2 = r#"
from hermes_tools import xlsx_read
wb = xlsx_read("out.xlsx")
print(avg, wb["sheets"]["Data"][1])
"#;
        let p2 = run_test_cell(&mut kernel, code2, dir.path(), deadline).await;
        assert_eq!(p2["status"], "ok", "{p2}");
        let out2 = p2["stdout"].as_str().unwrap();
        assert!(out2.contains("25") && out2.contains("Mean"), "{p2}");

        // Cell 3: bridges docx_write / docx_read, pptx_write / pptx_read, and os.path.getsize
        let code3 = r##"
import os
from hermes_tools import docx_write, docx_read, pptx_write, pptx_read

d_res = docx_write("brief.docx", "# Отчёт\n\n- **Итог**: готово\n", title="Бриф")
assert d_res["status"] == "ok", d_res
assert os.path.getsize("brief.docx") > 500
assert os.stat("brief.docx").st_size == os.path.getsize("brief.docx")
d_read = docx_read("brief.docx")
assert "Бриф" in d_read["paragraphs"], d_read

p_res = pptx_write("deck.pptx", [
    {"title": "План Q4", "subtitle": "Обзор"},
    {"title": "Цели", "bullets": ["Запуск", "Рост"], "notes": "Показать демо"},
])
assert p_res["status"] == "ok", p_res
assert os.path.getsize("deck.pptx") > 500
p_read = pptx_read("deck.pptx")
assert p_read["slide_count"] == 2, p_read
assert p_read["slides"][1]["notes"] == "Показать демо", p_read
print("OFFICE_OK")
"##;
        let p3 = run_test_cell(&mut kernel, code3, dir.path(), deadline).await;
        assert_eq!(p3["status"], "ok", "{p3}");
        assert!(p3["stdout"].as_str().unwrap().contains("OFFICE_OK"), "{p3}");
        kernel.shutdown();
    }

    async fn wait_for_exit(abort: &AbortState, max_wait: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < max_wait {
            if abort.exited.load(Ordering::Acquire) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        abort.exited.load(Ordering::Acquire)
    }

    #[tokio::test]
    async fn embedded_rustpython_shutdown_interrupts_infinite_loop_and_sleep() {
        let dir = tempfile::tempdir().unwrap();

        // 1. Tight CPU loop, even wrapped in `except BaseException: pass`.
        {
            let mut kernel = EmbeddedKernel::spawn(&[], Vec::new()).expect("spawn kernel");
            let abort = Arc::clone(&kernel.abort);
            assert!(kernel.start_cell(
                "while True:\n    try:\n        pass\n    except BaseException:\n        pass\n",
                dir.path()
            ));
            let event = kernel
                .next_event(Instant::now() + Duration::from_millis(120))
                .await;
            assert!(matches!(event, EmbeddedEvent::TimedOut));
            kernel.shutdown();
            assert!(
                wait_for_exit(&abort, Duration::from_secs(2)).await,
                "embedded worker thread kept burning CPU after timeout + shutdown"
            );
        }

        // 2. Long `time.sleep(...)` is interrupted immediately on shutdown.
        {
            let mut kernel = EmbeddedKernel::spawn(&[], Vec::new()).expect("spawn kernel");
            let abort = Arc::clone(&kernel.abort);
            assert!(kernel.start_cell("import time\ntime.sleep(60)\n", dir.path()));
            let event = kernel
                .next_event(Instant::now() + Duration::from_millis(120))
                .await;
            assert!(matches!(event, EmbeddedEvent::TimedOut));
            kernel.shutdown();
            assert!(
                wait_for_exit(&abort, Duration::from_secs(2)).await,
                "embedded worker thread remained blocked in time.sleep after shutdown"
            );
        }
    }

    #[test]
    fn sandbox_winreg_stub_imports_and_raises_os_error_on_open_key() {
        let handle = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let builder = Interpreter::builder(Settings::default());
                let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
                let os_def = _sandbox_os::module_def(&builder.ctx);
                let winreg_def = _sandbox_winreg::module_def(&builder.ctx);
                let interp = builder
                    .add_native_modules(&stdlib_defs)
                    .add_native_module(os_def)
                    .add_native_module(winreg_def)
                    .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
                    .build();
                interp.enter(|vm| {
                    let scope = vm.new_scope_with_builtins();
                    let code = vm
                        .compile(
                            concat!(
                                "import winreg\n",
                                "assert winreg.HKEY_CURRENT_USER > 0\n",
                                "assert winreg.HKEY_LOCAL_MACHINE > 0\n",
                                "raised = False\n",
                                "try:\n",
                                "    winreg.OpenKey(winreg.HKEY_CURRENT_USER, 'Software')\n",
                                "except OSError:\n",
                                "    raised = True\n",
                                "assert raised\n",
                            ),
                            Mode::Exec,
                            "<test-winreg>",
                        )
                        .expect("compile");
                    vm.run_code_obj(code, scope).expect("winreg stub behavior");
                });
            })
            .expect("spawn test thread");
        handle.join().expect("winreg test thread panicked");
    }
}
