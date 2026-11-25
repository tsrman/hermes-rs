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
use std::sync::mpsc as std_mpsc;
use std::time::Instant;

use ring::rand::{SecureRandom, SystemRandom};
use rustpython_vm::builtins::{PyDictRef, PyListRef, PyTupleRef, PyTypeRef};
use rustpython_vm::compiler::Mode;
use rustpython_vm::function::OptionalArg;
use rustpython_vm::{AsObject, Interpreter, PyObjectRef, PyResult, Settings, VirtualMachine};
use serde_json::{json, Value};
use tokio::sync::mpsc as tokio_mpsc;

enum HostCell {
    Run { code: String, cwd: String },
    Shutdown,
}

enum PyEvent {
    ToolCall { tool: String, args_json: String },
    CellDone(Value),
}

struct BridgeState {
    event_tx: tokio_mpsc::UnboundedSender<PyEvent>,
    reply_rx: std_mpsc::Receiver<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
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
            state
                .event_tx
                .send(PyEvent::ToolCall { tool, args_json })
                .map_err(|_| vm.new_runtime_error("host closed tool bridge"))?;
            state
                .reply_rx
                .recv()
                .map_err(|_| vm.new_runtime_error("host closed tool reply channel"))
        })
    }
}

fn bootstrap_source(tools: &[&str]) -> String {
    const STUBS: [(&str, &str); 9] = [
        ("web_search", "def web_search(query, limit=5):\n    return _call('web_search', {'query': query, 'limit': limit})\n"),
        ("web_extract", "def web_extract(urls, char_limit=None):\n    return _call('web_extract', {'urls': urls, 'char_limit': char_limit})\n"),
        ("read_file", "def read_file(path, offset=1, limit=2000):\n    return _call('read_file', {'path': path, 'offset': offset, 'limit': limit})\n"),
        ("write_file", "def write_file(path, content):\n    return _call('write_file', {'path': path, 'content': content})\n"),
        ("search_files", "def search_files(pattern, target='content', path='.', file_glob=None, limit=50, offset=0, output_mode='content', context=0, order='discovery'):\n    return _call('search_files', {'pattern': pattern, 'target': target, 'path': path, 'file_glob': file_glob, 'limit': limit, 'offset': offset, 'output_mode': output_mode, 'context': context, 'order': order})\n"),
        ("patch", "def patch(path=None, old_string=None, new_string=None, replace_all=False, mode='replace'):\n    return _call('patch', {'path': path, 'old_string': old_string, 'new_string': new_string, 'replace_all': replace_all, 'mode': mode})\n"),
        ("terminal", "def terminal(command, timeout=None, workdir=None):\n    return _call('terminal', {'command': command, 'timeout': timeout, 'workdir': workdir})\n"),
        ("xlsx_read", "def xlsx_read(path, sheet=None, max_rows=5000):\n    return _call('xlsx_read', {'path': path, 'sheet': sheet, 'max_rows': max_rows})\n"),
        ("xlsx_write", "def xlsx_write(path, sheets):\n    return _call('xlsx_write', {'path': path, 'sheets': sheets})\n"),
    ];
    let stubs: String = STUBS
        .iter()
        .filter(|(name, _)| *name == "xlsx_read" || *name == "xlsx_write" || tools.contains(name))
        .map(|(_, src)| *src)
        .collect();
    let mut export_names: Vec<&str> = vec![
        "json_parse",
        "shell_quote",
        "retry",
        "xlsx_read",
        "xlsx_write",
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

_LIMIT = 5000000

class _Capture(io.StringIO):
    def write(self, text):
        if self.tell() < _LIMIT:
            return super().write(text)
        return len(text)

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
    _out, _err = _Capture(), _Capture()
    _status, _trace = "ok", ""
    try:
        with contextlib.redirect_stdout(_out), contextlib.redirect_stderr(_err):
            exec(compile(code, "<cell>", "exec"), GLOBALS)
    except SystemExit as _exit:
        _status, _trace = "exit", "SystemExit: " + repr(_exit.code)
    except BaseException:
        _status, _trace = "error", traceback.format_exc()
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
    reply_tx: std_mpsc::Sender<String>,
    event_rx: tokio_mpsc::UnboundedReceiver<PyEvent>,
    pub(crate) tools: Vec<&'static str>,
    pub(crate) cells: u64,
}

pub(crate) enum EmbeddedEvent {
    ToolCall { tool: String, args: Value },
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
        let (event_tx, event_rx) = tokio_mpsc::unbounded_channel::<PyEvent>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();
        let boot_src = bootstrap_source(tools);
        let tools_owned = tools.to_vec();

        std::thread::Builder::new()
            .name("hsr-rustpython".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                BRIDGE.with(|slot| {
                    *slot.borrow_mut() = Some(BridgeState {
                        event_tx: event_tx.clone(),
                        reply_rx,
                        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                        env,
                    });
                });

                let builder = Interpreter::builder(Settings::default());
                let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
                let os_def = _sandbox_os::module_def(&builder.ctx);
                let bridge_def = _hermes_bridge::module_def(&builder.ctx);
                let interp = builder
                    .add_native_modules(&stdlib_defs)
                    .add_native_module(os_def)
                    .add_native_module(bridge_def)
                    .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
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
                        match msg {
                            HostCell::Shutdown => break,
                            HostCell::Run { code, cwd } => {
                                let py_code: PyObjectRef = vm.ctx.new_str(code).into();
                                let py_cwd: PyObjectRef = vm.ctx.new_str(cwd).into();
                                let payload = match runner.call((py_code, py_cwd), vm) {
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

                BRIDGE.with(|slot| {
                    *slot.borrow_mut() = None;
                });
            })
            .map_err(|e| format!("failed spawning embedded Python thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cell_tx,
                reply_tx,
                event_rx,
                tools: tools_owned,
                cells: 0,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("embedded Python thread exited during startup".into()),
        }
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.cell_tx.send(HostCell::Shutdown);
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
            Ok(Some(PyEvent::ToolCall { tool, args_json })) => {
                let args = serde_json::from_str::<Value>(&args_json)
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
                EmbeddedEvent::ToolCall { tool, args }
            }
        }
    }

    pub(crate) fn send_tool_reply(&self, value: Value) -> bool {
        let reply = json!({"value": value}).to_string();
        self.reply_tx.send(reply).is_ok()
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
                EmbeddedEvent::ToolCall { tool, args } => {
                    let val = match tool.as_str() {
                        "xlsx_write" => crate::excel_tools::xlsx_write(dir, &args),
                        "xlsx_read" => crate::excel_tools::xlsx_read(dir, &args),
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
        kernel.shutdown();
    }
}
