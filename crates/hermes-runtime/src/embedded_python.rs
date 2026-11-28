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
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};
use rustpython_vm::builtins::{
    PyBaseExceptionRef, PyBytes, PyDictRef, PyListRef, PyStr, PyTupleRef, PyTypeRef,
};
use rustpython_vm::compiler::Mode;
use rustpython_vm::function::{ArgBytesLike, KwArgs, OptionalArg};
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

struct SandboxFd {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

const MAX_SANDBOX_FDS: usize = 256;
const FIRST_SANDBOX_FD: i32 = 100;

struct BridgeState {
    transport: BridgeTransport,
    workspace_root: PathBuf,
    cwd: PathBuf,
    temp_root: Option<tempfile::TempDir>,
    env: Vec<(String, String)>,
    abort: Arc<AbortState>,
    fds: HashMap<i32, SandboxFd>,
    next_fd: i32,
    sqlite: crate::embedded_sqlite::SqliteStore,
}

impl BridgeState {
    fn new(
        transport: BridgeTransport,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        abort: Arc<AbortState>,
    ) -> Self {
        Self {
            transport,
            workspace_root: cwd.clone(),
            cwd,
            temp_root: None,
            env,
            abort,
            fds: HashMap::new(),
            next_fd: FIRST_SANDBOX_FD,
            sqlite: crate::embedded_sqlite::SqliteStore::new(),
        }
    }

    fn ensure_temp_dir(&mut self) -> Option<PathBuf> {
        if self.temp_root.is_none() {
            self.temp_root = tempfile::Builder::new().prefix("hsr-py-").tempdir().ok();
        }
        self.temp_root.as_ref().map(|d| d.path().to_path_buf())
    }
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

fn current_temp_dir() -> PathBuf {
    BRIDGE
        .with(|slot| {
            slot.borrow_mut()
                .as_mut()
                .and_then(BridgeState::ensure_temp_dir)
        })
        .unwrap_or_else(std::env::temp_dir)
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

/// Extract a filesystem path from Python `str`, `bytes`, or `os.PathLike` (`__fspath__`).
fn extract_path_string(obj: PyObjectRef, vm: &VirtualMachine) -> PyResult<String> {
    if let Some(s) = obj.downcast_ref::<PyStr>() {
        return Ok(s.as_wtf8().to_string_lossy().into_owned());
    }
    if let Some(b) = obj.downcast_ref::<PyBytes>() {
        return Ok(String::from_utf8_lossy(b.as_bytes()).into_owned());
    }
    if let Ok(Some(method)) = vm.get_attribute_opt(&obj, "__fspath__") {
        let res = method.call((), vm)?;
        if let Some(s) = res.downcast_ref::<PyStr>() {
            return Ok(s.as_wtf8().to_string_lossy().into_owned());
        }
        if let Some(b) = res.downcast_ref::<PyBytes>() {
            return Ok(String::from_utf8_lossy(b.as_bytes()).into_owned());
        }
    }
    Err(vm.new_type_error("expected str, bytes or os.PathLike object"))
}

fn resolve_py_path(obj: PyObjectRef, vm: &VirtualMachine) -> PyResult<PathBuf> {
    let raw = extract_path_string(obj, vm)?;
    Ok(resolve_in_session(Some(raw)))
}

fn io_err_to_py(err: std::io::Error, context: &str, vm: &VirtualMachine) -> PyBaseExceptionRef {
    use std::io::ErrorKind;
    let msg = format!("{context}: {err}");
    let (exc_type, errno_val) = match err.kind() {
        ErrorKind::NotFound => (vm.ctx.exceptions.file_not_found_error.to_owned(), 2i32),
        ErrorKind::AlreadyExists => (vm.ctx.exceptions.file_exists_error.to_owned(), 17i32),
        ErrorKind::PermissionDenied => (vm.ctx.exceptions.permission_error.to_owned(), 13i32),
        ErrorKind::IsADirectory => (vm.ctx.exceptions.is_a_directory_error.to_owned(), 21i32),
        ErrorKind::NotADirectory => (vm.ctx.exceptions.not_a_directory_error.to_owned(), 20i32),
        _ => (
            vm.ctx.exceptions.os_error.to_owned(),
            err.raw_os_error().unwrap_or(5),
        ),
    };
    vm.new_exception(
        exc_type,
        vec![vm.ctx.new_int(errno_val).into(), vm.ctx.new_str(msg).into()],
    )
}

/// Canonicalize a path even when its final component(s) do not exist yet, preventing symlink or
/// `..` escape outside allowed roots.
fn normalize_for_scope_check(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        tail.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent.to_path_buf();
    }
    let mut resolved = std::fs::canonicalize(&existing)?;
    for part in tail.into_iter().rev() {
        let comp = Path::new(&part);
        if comp.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path traversal outside workspace",
            ));
        }
        resolved.push(part);
    }
    Ok(resolved)
}

/// Enforce that mutating filesystem operations stay inside the session workspace root or the
/// session's isolated sandbox temp directory.
fn ensure_mutation_allowed(target: &Path, vm: &VirtualMachine) -> PyResult<PathBuf> {
    let (workspace_root, temp_root) = BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        if let Some(state) = borrow.as_mut() {
            let ws = state.workspace_root.clone();
            let tmp = state.temp_root.as_ref().map(|d| d.path().to_path_buf());
            (ws, tmp)
        } else {
            (
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                None,
            )
        }
    });
    let canon_target = normalize_for_scope_check(target)
        .map_err(|e| io_err_to_py(e, &target.display().to_string(), vm))?;
    let ws_allowed = std::fs::canonicalize(&workspace_root)
        .map(|ws| canon_target.starts_with(&ws))
        .unwrap_or(false);
    let tmp_allowed = temp_root
        .as_deref()
        .and_then(|t| std::fs::canonicalize(t).ok())
        .is_some_and(|t| canon_target.starts_with(&t));
    if ws_allowed || tmp_allowed {
        Ok(canon_target)
    } else {
        Err(vm.new_exception(
            vm.ctx.exceptions.permission_error.to_owned(),
            vec![
                vm.ctx.new_int(13i32).into(),
                vm.ctx
                    .new_str(format!(
                        "write access outside workspace/temp directory is disabled: {}",
                        target.display()
                    ))
                    .into(),
            ],
        ))
    }
}

fn sandbox_chdir(path: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
    let target = resolve_py_path(path, vm)?;
    if !target.is_dir() {
        return Err(io_err_to_py(
            std::io::Error::new(std::io::ErrorKind::NotFound, "directory not found"),
            &target.display().to_string(),
            vm,
        ));
    }
    BRIDGE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.cwd = target;
        }
    });
    Ok(())
}

fn sandbox_listdir(path: OptionalArg<PyObjectRef>, vm: &VirtualMachine) -> PyResult<PyListRef> {
    let dir = match path.into_option() {
        Some(obj) if !vm.is_none(&obj) => resolve_py_path(obj, vm)?,
        _ => current_session_cwd(),
    };
    let rd = std::fs::read_dir(&dir)
        .map_err(|e| io_err_to_py(e, &format!("listdir {}", dir.display()), vm))?;
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

fn path_inode_hash(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut hasher);
    hasher.finish().max(1)
}

fn metadata_to_stat_tuple(
    meta: &std::fs::Metadata,
    path: Option<&Path>,
    vm: &VirtualMachine,
) -> PyTupleRef {
    let mode: u32 = if meta.is_dir() {
        0o040_755
    } else if meta.is_symlink() {
        0o120_777
    } else {
        0o100_644
    };
    let ino = path.map(path_inode_hash).unwrap_or(0);
    let dev: u64 = if ino > 0 { 1 } else { 0 };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let items: Vec<PyObjectRef> = vec![
        vm.ctx.new_int(mode).into(),
        vm.ctx.new_int(ino).into(),
        vm.ctx.new_int(dev).into(),
        vm.ctx.new_int(1).into(),
        vm.ctx.new_int(0).into(),
        vm.ctx.new_int(0).into(),
        vm.ctx.new_int(size).into(),
        vm.ctx.new_float(mtime).into(),
        vm.ctx.new_float(mtime).into(),
        vm.ctx.new_float(mtime).into(),
    ];
    vm.ctx.new_tuple(items)
}

fn sandbox_stat_obj(
    path: PyObjectRef,
    default_follow_symlinks: bool,
    kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<PyTupleRef> {
    if let Ok(fd) = path.clone().try_into_value::<i32>(vm) {
        return sandbox_fstat(fd, vm);
    }
    let follow_symlinks = kwargs
        .into_iter()
        .find(|(k, _)| k.as_str() == Ok("follow_symlinks"))
        .map(|(_, v)| v.try_to_bool(vm))
        .transpose()?
        .unwrap_or(default_follow_symlinks);
    let target = resolve_py_path(path, vm)?;
    let meta = if follow_symlinks {
        std::fs::metadata(&target)
    } else {
        std::fs::symlink_metadata(&target)
    }
    .map_err(|e| io_err_to_py(e, &format!("stat {}", target.display()), vm))?;
    Ok(metadata_to_stat_tuple(&meta, Some(&target), vm))
}

fn sandbox_fstat(fd: i32, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
    let (meta, path) = BRIDGE.with(|slot| {
        let borrow = slot.borrow();
        let state = borrow
            .as_ref()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        let entry = state
            .fds
            .get(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        let m = entry
            .file
            .metadata()
            .map_err(|e| io_err_to_py(e, "fstat", vm))?;
        Ok::<_, PyBaseExceptionRef>((m, entry.path.clone()))
    })?;
    Ok(metadata_to_stat_tuple(&meta, Some(&path), vm))
}

fn sandbox_dup(fd: i32, vm: &VirtualMachine) -> PyResult<i32> {
    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        if state.fds.len() >= MAX_SANDBOX_FDS {
            return Err(vm.new_os_error("too many open files in sandbox"));
        }
        let entry = state
            .fds
            .get(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        let cloned = entry
            .file
            .try_clone()
            .map_err(|e| io_err_to_py(e, "dup", vm))?;
        let path = entry.path.clone();
        let new_fd = state.next_fd;
        state.next_fd = state.next_fd.wrapping_add(1).max(FIRST_SANDBOX_FD);
        state.fds.insert(new_fd, SandboxFd { file: cloned, path });
        Ok(new_fd)
    })
}

fn sandbox_symlink(
    src: PyObjectRef,
    dst: PyObjectRef,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<()> {
    let src_raw = extract_path_string(src, vm)?;
    let dst_path = resolve_py_path(dst, vm)?;
    ensure_mutation_allowed(&dst_path, vm)?;
    // Ensure symlink target also resolves inside workspace/temp so symlinks cannot be used to
    // escape the sandbox.
    let target_check = if Path::new(&src_raw).is_absolute() {
        PathBuf::from(&src_raw)
    } else {
        dst_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&src_raw)
    };
    ensure_mutation_allowed(&target_check, vm)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&src_raw, &dst_path)
            .map_err(|e| io_err_to_py(e, &format!("symlink {}", dst_path.display()), vm))
    }
    #[cfg(windows)]
    {
        let res = if target_check.is_dir() {
            std::os::windows::fs::symlink_dir(&src_raw, &dst_path)
        } else {
            std::os::windows::fs::symlink_file(&src_raw, &dst_path)
        };
        res.map_err(|e| io_err_to_py(e, &format!("symlink {}", dst_path.display()), vm))
    }
}

const SANDBOX_O_RDONLY: i32 = 0;
const SANDBOX_O_WRONLY: i32 = 1;
const SANDBOX_O_RDWR: i32 = 2;
const SANDBOX_O_CREAT: i32 = 0o100;
const SANDBOX_O_EXCL: i32 = 0o200;
const SANDBOX_O_TRUNC: i32 = 0o1000;
const SANDBOX_O_APPEND: i32 = 0o2000;
const SANDBOX_O_DIRECTORY: i32 = 0o200000;
const SANDBOX_O_NOFOLLOW: i32 = 0o400000;
const SANDBOX_O_CLOEXEC: i32 = 0o2000000;
const SANDBOX_O_NOINHERIT: i32 = 0x80;
const SANDBOX_O_TEXT: i32 = 0x4000;
const SANDBOX_O_BINARY: i32 = 0x8000;

fn ensure_read_allowed(target: &Path, vm: &VirtualMachine) -> PyResult<()> {
    crate::file_tools::check_protected_read_path(target, None).map_err(|msg| {
        vm.new_exception(
            vm.ctx.exceptions.permission_error.to_owned(),
            vec![vm.ctx.new_int(13i32).into(), vm.ctx.new_str(msg).into()],
        )
    })
}

fn sandbox_open(
    path: PyObjectRef,
    flags: i32,
    _mode: OptionalArg<u32>,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<i32> {
    let target = resolve_py_path(path, vm)?;
    ensure_read_allowed(&target, vm)?;
    let acc = flags & 3;
    let read = acc == SANDBOX_O_RDONLY || acc == SANDBOX_O_RDWR;
    let write = acc == SANDBOX_O_WRONLY || acc == SANDBOX_O_RDWR;
    let create = (flags & SANDBOX_O_CREAT) != 0;
    let excl = (flags & SANDBOX_O_EXCL) != 0;
    let trunc = (flags & SANDBOX_O_TRUNC) != 0;
    let append = (flags & SANDBOX_O_APPEND) != 0;

    if write || create || trunc || append {
        ensure_mutation_allowed(&target, vm)?;
    }

    let mut opts = OpenOptions::new();
    opts.read(read);
    if append {
        opts.append(true);
    } else if write {
        opts.write(true);
    }
    if create && excl {
        opts.create_new(true);
    } else if create {
        opts.create(true);
    }
    if trunc {
        opts.truncate(true);
    }

    let file = opts
        .open(&target)
        .map_err(|e| io_err_to_py(e, &format!("open {}", target.display()), vm))?;

    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("sandbox state unavailable"))?;
        if state.fds.len() >= MAX_SANDBOX_FDS {
            return Err(vm.new_os_error("too many open files in sandbox"));
        }
        let fd = state.next_fd;
        state.next_fd = state.next_fd.wrapping_add(1).max(FIRST_SANDBOX_FD);
        state.fds.insert(fd, SandboxFd { file, path: target });
        Ok(fd)
    })
}

fn sandbox_close(fd: i32, vm: &VirtualMachine) -> PyResult<()> {
    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        state
            .fds
            .remove(&fd)
            .map(|_| ())
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))
    })
}

fn sandbox_read(fd: i32, n: usize, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
    let cap = n.min(16 * 1024 * 1024);
    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        let entry = state
            .fds
            .get_mut(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        let mut buf = vec![0u8; cap];
        let got = entry
            .file
            .read(&mut buf)
            .map_err(|e| io_err_to_py(e, "read", vm))?;
        buf.truncate(got);
        Ok(buf)
    })
}

fn sandbox_write(fd: i32, data: ArgBytesLike, vm: &VirtualMachine) -> PyResult<usize> {
    let bytes = data.borrow_buf();
    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        let entry = state
            .fds
            .get_mut(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        entry
            .file
            .write(&bytes)
            .map_err(|e| io_err_to_py(e, "write", vm))
    })
}

fn sandbox_lseek(fd: i32, pos: i64, how: i32, vm: &VirtualMachine) -> PyResult<u64> {
    let seek = match how {
        0 => {
            SeekFrom::Start(u64::try_from(pos).map_err(|_| vm.new_os_error("invalid seek offset"))?)
        }
        1 => SeekFrom::Current(pos),
        2 => SeekFrom::End(pos),
        _ => return Err(vm.new_os_error(format!("invalid whence: {how}"))),
    };
    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        let entry = state
            .fds
            .get_mut(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        entry
            .file
            .seek(seek)
            .map_err(|e| io_err_to_py(e, "lseek", vm))
    })
}

fn sandbox_ftruncate(fd: i32, length: u64, vm: &VirtualMachine) -> PyResult<()> {
    BRIDGE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow
            .as_mut()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        let entry = state
            .fds
            .get_mut(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        entry
            .file
            .set_len(length)
            .map_err(|e| io_err_to_py(e, "ftruncate", vm))
    })
}

fn sandbox_fsync(fd: i32, vm: &VirtualMachine) -> PyResult<()> {
    BRIDGE.with(|slot| {
        let borrow = slot.borrow();
        let state = borrow
            .as_ref()
            .ok_or_else(|| vm.new_os_error("bad file descriptor"))?;
        let entry = state
            .fds
            .get(&fd)
            .ok_or_else(|| vm.new_os_error(format!("bad file descriptor: {fd}")))?;
        entry
            .file
            .sync_all()
            .map_err(|e| io_err_to_py(e, "fsync", vm))
    })
}

fn sandbox_mkdir(
    path: PyObjectRef,
    _mode: OptionalArg<u32>,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<()> {
    let target = resolve_py_path(path, vm)?;
    ensure_mutation_allowed(&target, vm)?;
    std::fs::create_dir(&target)
        .map_err(|e| io_err_to_py(e, &format!("mkdir {}", target.display()), vm))
}

fn sandbox_rmdir(path: PyObjectRef, _kwargs: KwArgs, vm: &VirtualMachine) -> PyResult<()> {
    let target = resolve_py_path(path, vm)?;
    ensure_mutation_allowed(&target, vm)?;
    std::fs::remove_dir(&target)
        .map_err(|e| io_err_to_py(e, &format!("rmdir {}", target.display()), vm))
}

fn sandbox_remove(path: PyObjectRef, _kwargs: KwArgs, vm: &VirtualMachine) -> PyResult<()> {
    let target = resolve_py_path(path, vm)?;
    ensure_mutation_allowed(&target, vm)?;
    std::fs::remove_file(&target)
        .map_err(|e| io_err_to_py(e, &format!("remove {}", target.display()), vm))
}

fn sandbox_rename(
    src: PyObjectRef,
    dst: PyObjectRef,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<()> {
    let src_path = resolve_py_path(src, vm)?;
    let dst_path = resolve_py_path(dst, vm)?;
    ensure_mutation_allowed(&src_path, vm)?;
    ensure_mutation_allowed(&dst_path, vm)?;
    std::fs::rename(&src_path, &dst_path).map_err(|e| {
        io_err_to_py(
            e,
            &format!("rename {} -> {}", src_path.display(), dst_path.display()),
            vm,
        )
    })
}

fn sandbox_readlink(path: PyObjectRef, _kwargs: KwArgs, vm: &VirtualMachine) -> PyResult<String> {
    let target = resolve_py_path(path, vm)?;
    let linked = std::fs::read_link(&target)
        .map_err(|e| io_err_to_py(e, &format!("readlink {}", target.display()), vm))?;
    Ok(linked.display().to_string())
}

fn sandbox_access(
    path: PyObjectRef,
    mode: i32,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<bool> {
    let target = resolve_py_path(path, vm)?;
    let Ok(meta) = std::fs::metadata(&target) else {
        return Ok(false);
    };
    Ok(!((mode & 2) != 0 && meta.permissions().readonly()))
}

fn sandbox_chmod(
    path: PyObjectRef,
    mode: u32,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<()> {
    let target = resolve_py_path(path, vm)?;
    ensure_mutation_allowed(&target, vm)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
            .map_err(|e| io_err_to_py(e, &format!("chmod {}", target.display()), vm))?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        if !target.exists() {
            return Err(io_err_to_py(
                std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
                &target.display().to_string(),
                vm,
            ));
        }
    }
    Ok(())
}

fn sandbox_utime(
    path: PyObjectRef,
    _times: OptionalArg<PyObjectRef>,
    _kwargs: KwArgs,
    vm: &VirtualMachine,
) -> PyResult<()> {
    let target = resolve_py_path(path, vm)?;
    ensure_mutation_allowed(&target, vm)?;
    if !target.exists() {
        return Err(io_err_to_py(
            std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
            &target.display().to_string(),
            vm,
        ));
    }
    Ok(())
}

fn sandbox_scandir_raw(path: OptionalArg<PyObjectRef>, vm: &VirtualMachine) -> PyResult<PyListRef> {
    let dir = match path.into_option() {
        Some(obj) if !vm.is_none(&obj) => resolve_py_path(obj, vm)?,
        _ => current_session_cwd(),
    };
    let rd = std::fs::read_dir(&dir)
        .map_err(|e| io_err_to_py(e, &format!("scandir {}", dir.display()), vm))?;
    let mut rows: Vec<PyObjectRef> = Vec::new();
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let full = entry.path();
        let lmeta = std::fs::symlink_metadata(&full).ok();
        let meta = std::fs::metadata(&full).ok();
        let is_symlink = lmeta.as_ref().is_some_and(|m| m.is_symlink());
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let is_file = meta.as_ref().is_some_and(|m| m.is_file());
        let lstat_obj: PyObjectRef = lmeta
            .as_ref()
            .map(|m| metadata_to_stat_tuple(m, Some(&full), vm).into())
            .unwrap_or_else(|| vm.ctx.none());
        let stat_obj: PyObjectRef = meta
            .as_ref()
            .map(|m| metadata_to_stat_tuple(m, Some(&full), vm).into())
            .unwrap_or_else(|| lstat_obj.clone());
        let row = vm.ctx.new_tuple(vec![
            vm.ctx.new_str(name).into(),
            vm.ctx.new_str(full.display().to_string()).into(),
            vm.ctx.new_bool(is_dir).into(),
            vm.ctx.new_bool(is_file).into(),
            vm.ctx.new_bool(is_symlink).into(),
            stat_obj,
            lstat_obj,
        ]);
        rows.push(row.into());
    }
    Ok(vm.ctx.new_list(rows))
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

macro_rules! define_sandbox_os {
    ($mod_name:literal) => {
        #[rustpython_vm::pymodule(name = $mod_name)]
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
            const O_RDONLY: i32 = SANDBOX_O_RDONLY;
            #[pyattr]
            const O_WRONLY: i32 = SANDBOX_O_WRONLY;
            #[pyattr]
            const O_RDWR: i32 = SANDBOX_O_RDWR;
            #[pyattr]
            const O_CREAT: i32 = SANDBOX_O_CREAT;
            #[pyattr]
            const O_EXCL: i32 = SANDBOX_O_EXCL;
            #[pyattr]
            const O_TRUNC: i32 = SANDBOX_O_TRUNC;
            #[pyattr]
            const O_APPEND: i32 = SANDBOX_O_APPEND;
            #[pyattr]
            const O_DIRECTORY: i32 = SANDBOX_O_DIRECTORY;
            #[pyattr]
            const O_NOFOLLOW: i32 = SANDBOX_O_NOFOLLOW;
            #[pyattr]
            const O_CLOEXEC: i32 = SANDBOX_O_CLOEXEC;
            #[pyattr]
            const O_NOINHERIT: i32 = SANDBOX_O_NOINHERIT;
            #[pyattr]
            const O_TEXT: i32 = SANDBOX_O_TEXT;
            #[pyattr]
            const O_BINARY: i32 = SANDBOX_O_BINARY;

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
            fn chdir(path: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_chdir(path, vm)
            }

            #[pyfunction]
            fn listdir(path: OptionalArg<PyObjectRef>, vm: &VirtualMachine) -> PyResult<PyListRef> {
                sandbox_listdir(path, vm)
            }

            #[pyfunction]
            fn stat(
                path: PyObjectRef,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<PyTupleRef> {
                sandbox_stat_obj(path, true, kwargs, vm)
            }

            #[pyfunction]
            fn lstat(
                path: PyObjectRef,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<PyTupleRef> {
                sandbox_stat_obj(path, false, kwargs, vm)
            }

            #[pyfunction]
            fn fstat(fd: i32, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
                sandbox_fstat(fd, vm)
            }

            #[pyfunction]
            fn open(
                path: PyObjectRef,
                flags: i32,
                mode: OptionalArg<u32>,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<i32> {
                sandbox_open(path, flags, mode, kwargs, vm)
            }

            #[pyfunction]
            fn close(fd: i32, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_close(fd, vm)
            }

            #[pyfunction]
            fn read(fd: i32, n: usize, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
                sandbox_read(fd, n, vm)
            }

            #[pyfunction]
            fn write(fd: i32, data: ArgBytesLike, vm: &VirtualMachine) -> PyResult<usize> {
                sandbox_write(fd, data, vm)
            }

            #[pyfunction]
            fn lseek(fd: i32, pos: i64, how: i32, vm: &VirtualMachine) -> PyResult<u64> {
                sandbox_lseek(fd, pos, how, vm)
            }

            #[pyfunction]
            fn ftruncate(fd: i32, length: u64, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_ftruncate(fd, length, vm)
            }

            #[pyfunction]
            fn fsync(fd: i32, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_fsync(fd, vm)
            }

            #[pyfunction]
            fn isatty(_fd: i32) -> bool {
                false
            }

            #[pyfunction]
            fn mkdir(
                path: PyObjectRef,
                mode: OptionalArg<u32>,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<()> {
                sandbox_mkdir(path, mode, kwargs, vm)
            }

            #[pyfunction]
            fn rmdir(path: PyObjectRef, kwargs: KwArgs, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_rmdir(path, kwargs, vm)
            }

            #[pyfunction]
            fn remove(path: PyObjectRef, kwargs: KwArgs, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_remove(path, kwargs, vm)
            }

            #[pyfunction]
            fn unlink(path: PyObjectRef, kwargs: KwArgs, vm: &VirtualMachine) -> PyResult<()> {
                sandbox_remove(path, kwargs, vm)
            }

            #[pyfunction]
            fn rename(
                src: PyObjectRef,
                dst: PyObjectRef,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<()> {
                sandbox_rename(src, dst, kwargs, vm)
            }

            #[pyfunction]
            fn replace(
                src: PyObjectRef,
                dst: PyObjectRef,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<()> {
                sandbox_rename(src, dst, kwargs, vm)
            }

            #[pyfunction]
            fn readlink(
                path: PyObjectRef,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<String> {
                sandbox_readlink(path, kwargs, vm)
            }

            #[pyfunction]
            fn access(
                path: PyObjectRef,
                mode: i32,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<bool> {
                sandbox_access(path, mode, kwargs, vm)
            }

            #[pyfunction]
            fn chmod(
                path: PyObjectRef,
                mode: u32,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<()> {
                sandbox_chmod(path, mode, kwargs, vm)
            }

            #[pyfunction]
            fn utime(
                path: PyObjectRef,
                times: OptionalArg<PyObjectRef>,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<()> {
                sandbox_utime(path, times, kwargs, vm)
            }

            #[pyfunction]
            fn umask(_mask: u32) -> u32 {
                0o022
            }

            #[pyfunction]
            fn putenv(_key: PyObjectRef, _value: PyObjectRef) {}

            #[pyfunction]
            fn unsetenv(_key: PyObjectRef) {}

            #[pyfunction]
            fn strerror(code: i32) -> String {
                std::io::Error::from_raw_os_error(code).to_string()
            }

            #[pyfunction]
            fn dup(fd: i32, vm: &VirtualMachine) -> PyResult<i32> {
                sandbox_dup(fd, vm)
            }

            #[pyfunction]
            fn symlink(
                src: PyObjectRef,
                dst: PyObjectRef,
                kwargs: KwArgs,
                vm: &VirtualMachine,
            ) -> PyResult<()> {
                sandbox_symlink(src, dst, kwargs, vm)
            }

            #[pyfunction]
            fn get_terminal_size(_fd: OptionalArg<i32>, vm: &VirtualMachine) -> PyTupleRef {
                vm.ctx
                    .new_tuple(vec![vm.ctx.new_int(80).into(), vm.ctx.new_int(24).into()])
            }

            #[pyfunction]
            fn scandir(path: OptionalArg<PyObjectRef>, vm: &VirtualMachine) -> PyResult<PyListRef> {
                sandbox_scandir_raw(path, vm)
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
    };
}

#[cfg(not(windows))]
define_sandbox_os!("posix");

#[cfg(windows)]
define_sandbox_os!("nt");

/// Safe in-memory `_signal` stub so stdlib `signal`, `unittest`, and `unittest.mock` import and
/// register handlers cleanly without exposing OS-level process signalling.
#[rustpython_vm::pymodule(name = "_signal")]
mod _sandbox_signal {
    use super::*;

    #[pyattr]
    const SIG_DFL: i32 = 0;
    #[pyattr]
    const SIG_IGN: i32 = 1;
    #[pyattr]
    const SIGHUP: i32 = 1;
    #[pyattr]
    const SIGINT: i32 = 2;
    #[pyattr]
    const SIGQUIT: i32 = 3;
    #[pyattr]
    const SIGILL: i32 = 4;
    #[pyattr]
    const SIGABRT: i32 = 6;
    #[pyattr]
    const SIGFPE: i32 = 8;
    #[pyattr]
    const SIGKILL: i32 = 9;
    #[pyattr]
    const SIGUSR1: i32 = 10;
    #[pyattr]
    const SIGSEGV: i32 = 11;
    #[pyattr]
    const SIGUSR2: i32 = 12;
    #[pyattr]
    const SIGPIPE: i32 = 13;
    #[pyattr]
    const SIGALRM: i32 = 14;
    #[pyattr]
    const SIGTERM: i32 = 15;
    #[pyattr]
    const SIGCHLD: i32 = 17;
    #[pyattr]
    const SIGBREAK: i32 = 21;
    #[pyattr]
    const NSIG: i32 = 65;

    #[pyfunction]
    fn default_int_handler(
        _signum: OptionalArg<PyObjectRef>,
        _frame: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        Err(vm.new_exception_msg(vm.ctx.exceptions.keyboard_interrupt.to_owned(), "".into()))
    }

    #[pyfunction]
    fn valid_signals(vm: &VirtualMachine) -> PyResult<PyObjectRef> {
        let sigs: Vec<PyObjectRef> = [
            SIGHUP, SIGINT, SIGQUIT, SIGILL, SIGABRT, SIGFPE, SIGKILL, SIGUSR1, SIGSEGV, SIGUSR2,
            SIGPIPE, SIGALRM, SIGTERM, SIGCHLD, SIGBREAK,
        ]
        .into_iter()
        .map(|n| vm.ctx.new_int(n).into())
        .collect();
        Ok(vm.ctx.new_list(sigs).into())
    }

    #[pyfunction]
    fn getsignal(_signum: i32, vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.new_int(SIG_DFL).into()
    }

    #[pyfunction]
    fn signal(_signum: i32, _handler: PyObjectRef, vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.new_int(SIG_DFL).into()
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

fn set_host_workspace_root(path: &str) {
    if path.is_empty() {
        return;
    }
    let target = PathBuf::from(path);
    BRIDGE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.workspace_root = target.clone();
            if target.is_dir() {
                state.cwd = target;
            }
        }
    });
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
    fn get_temp_dir() -> String {
        current_temp_dir().display().to_string()
    }

    #[pyfunction]
    fn sqlite_open(path: String, read_only: bool, vm: &VirtualMachine) -> PyResult<i32> {
        let trimmed = path.trim();
        let is_memory = trimmed.is_empty() || trimmed == ":memory:";
        let target = if is_memory {
            None
        } else {
            let resolved = resolve_in_session(Some(trimmed.to_owned()));
            ensure_read_allowed(&resolved, vm)?;
            if !read_only {
                ensure_mutation_allowed(&resolved, vm)?;
            }
            Some(resolved)
        };
        BRIDGE.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let state = borrow
                .as_mut()
                .ok_or_else(|| vm.new_runtime_error("sandbox state unavailable"))?;
            let res = match target {
                None => state.sqlite.open_memory(),
                Some(ref p) => state.sqlite.open_file(p, read_only),
            };
            res.map_err(|e| vm.new_runtime_error(e))
        })
    }

    #[pyfunction]
    fn sqlite_exec(
        conn_id: i32,
        sql: String,
        params_json: String,
        vm: &VirtualMachine,
    ) -> PyResult<String> {
        BRIDGE.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let state = borrow
                .as_mut()
                .ok_or_else(|| vm.new_runtime_error("sandbox state unavailable"))?;
            let val = state
                .sqlite
                .execute(conn_id, &sql, &params_json)
                .map_err(|e| vm.new_runtime_error(e))?;
            serde_json::to_string(&val).map_err(|e| vm.new_runtime_error(e.to_string()))
        })
    }

    #[pyfunction]
    fn sqlite_exec_batch(conn_id: i32, sql: String, vm: &VirtualMachine) -> PyResult<()> {
        BRIDGE.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let state = borrow
                .as_mut()
                .ok_or_else(|| vm.new_runtime_error("sandbox state unavailable"))?;
            state
                .sqlite
                .execute_batch(conn_id, &sql)
                .map_err(|e| vm.new_runtime_error(e))
        })
    }

    #[pyfunction]
    fn sqlite_in_transaction(conn_id: i32, vm: &VirtualMachine) -> PyResult<bool> {
        BRIDGE.with(|slot| {
            let borrow = slot.borrow();
            let state = borrow
                .as_ref()
                .ok_or_else(|| vm.new_runtime_error("sandbox state unavailable"))?;
            state
                .sqlite
                .in_transaction(conn_id)
                .map_err(|e| vm.new_runtime_error(e))
        })
    }

    #[pyfunction]
    fn sqlite_close(conn_id: i32) {
        BRIDGE.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                state.sqlite.close(conn_id);
            }
        });
    }

    #[pyfunction]
    fn yaml_load_all(text: String, vm: &VirtualMachine) -> PyResult<String> {
        crate::embedded_sqlite::yaml_load_all_to_json(&text).map_err(|e| vm.new_value_error(e))
    }

    #[pyfunction]
    fn yaml_dump(json_text: String, vm: &VirtualMachine) -> PyResult<String> {
        crate::embedded_sqlite::json_to_yaml(&json_text).map_err(|e| vm.new_value_error(e))
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
    const STUBS: [(&str, &str); 15] = [
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
        ("pdf_read", "def pdf_read(path, max_chars=100000):\n    return _call('pdf_read', {'path': path, 'max_chars': max_chars})\n"),
        ("http_request", "def http_request(url, method='GET', headers=None, body=None, body_b64=None, timeout=30):\n    return _call('http_request', {'url': url, 'method': method, 'headers': headers, 'body': body, 'body_b64': body_b64, 'timeout': timeout})\n"),
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
                    | "pdf_read"
                    | "http_request"
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
        "pdf_read",
        "http_request",
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
    let terminal_enabled = if tools.contains(&"terminal") {
        "True"
    } else {
        "False"
    };
    let shims = SANDBOX_SHIMS_PY;

    format!(
        r#"
import _hermes_bridge
import _io, builtins, contextlib, io, json, os, shlex, sys, time, traceback, types

time.sleep = _hermes_bridge.sleep

_SANDBOX_TMP = _hermes_bridge.get_temp_dir()
for _tmp_var in ("TMPDIR", "TEMP", "TMP"):
    if not os.environ.get(_tmp_var):
        os.environ[_tmp_var] = _SANDBOX_TMP

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
    st_atime_ns = property(lambda self: int(self[7] * 1_000_000_000))
    st_mtime_ns = property(lambda self: int(self[8] * 1_000_000_000))
    st_ctime_ns = property(lambda self: int(self[9] * 1_000_000_000))

_raw_stat = getattr(os, "stat", None)
if callable(_raw_stat):
    def _wrapped_stat(path, *args, **kwargs):
        res = _raw_stat(path, **kwargs)
        return res if hasattr(res, "st_size") else _StatResult(res)
    os.stat = _wrapped_stat
_raw_lstat = getattr(os, "lstat", None)
if callable(_raw_lstat):
    def _wrapped_lstat(path, *args, **kwargs):
        res = _raw_lstat(path, **kwargs)
        return res if hasattr(res, "st_size") else _StatResult(res)
    os.lstat = _wrapped_lstat
_raw_fstat = getattr(os, "fstat", None)
if callable(_raw_fstat):
    def _wrapped_fstat(fd):
        res = _raw_fstat(fd)
        return res if hasattr(res, "st_size") else _StatResult(res)
    os.fstat = _wrapped_fstat
os.stat_result = _StatResult

class _DirEntry:
    def __init__(self, row):
        self.name = row[0]
        self.path = row[1]
        self._is_dir = bool(row[2])
        self._is_file = bool(row[3])
        self._is_symlink = bool(row[4])
        self._stat = _StatResult(row[5]) if row[5] is not None else None
        self._lstat = _StatResult(row[6]) if row[6] is not None else self._stat

    def __fspath__(self):
        return self.path

    def is_dir(self, *, follow_symlinks=True):
        if not follow_symlinks and self._is_symlink:
            return False
        return self._is_dir

    def is_file(self, *, follow_symlinks=True):
        if not follow_symlinks and self._is_symlink:
            return False
        return self._is_file

    def is_symlink(self):
        return self._is_symlink

    def is_junction(self):
        return False

    def stat(self, *, follow_symlinks=True):
        res = self._stat if follow_symlinks else self._lstat
        if res is None:
            return os.stat(self.path, follow_symlinks=follow_symlinks)
        return res

    def inode(self):
        st = self._lstat or self._stat
        return st.st_ino if st is not None else 0

    def __repr__(self):
        return f"<DirEntry {{self.name!r}}>"

class _ScandirIterator:
    def __init__(self, entries):
        self._iter = iter(entries)

    def __iter__(self):
        return self

    def __next__(self):
        return next(self._iter)

    def close(self):
        self._iter = iter(())

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()
        return False

_raw_scandir = getattr(os, "scandir", None)
if callable(_raw_scandir):
    def _wrapped_scandir(path=None):
        rows = _raw_scandir(path)
        return _ScandirIterator([_DirEntry(r) for r in rows])
    os.scandir = _wrapped_scandir
    os.DirEntry = _DirEntry
    _os_mod = sys.modules.get(os.name)
    if _os_mod is not None:
        _os_mod.scandir = _wrapped_scandir
    if hasattr(os, "walk") and hasattr(os.walk, "__globals__"):
        os.walk.__globals__["scandir"] = _wrapped_scandir

class _SandboxFileIO(io.RawIOBase):
    def __init__(self, file, mode="r", closefd=True, opener=None):
        super().__init__()
        self._closed = False
        self._closefd = bool(closefd)
        self._readable = False
        self._writable = False
        self._appending = False
        self._created = False
        self.mode = mode
        if not isinstance(mode, str):
            raise TypeError("invalid mode: " + repr(mode))
        creating = "x" in mode
        reading = "r" in mode
        writing = "w" in mode
        appending = "a" in mode
        updating = "+" in mode
        if sum((creating, reading, writing, appending)) != 1:
            raise ValueError("must have exactly one of create/read/write/append mode")
        self._readable = reading or updating
        self._writable = writing or appending or creating or updating
        self._appending = appending
        self._created = creating
        if isinstance(file, int) and not isinstance(file, bool):
            if file < 0:
                raise ValueError("negative file descriptor")
            self._fd = file
            self.name = file
        else:
            if not self._closefd:
                raise ValueError("Cannot use closefd=False with file name")
            path = os.fspath(file)
            self.name = path
            if self._readable and self._writable:
                flags = os.O_RDWR
            elif self._writable:
                flags = os.O_WRONLY
            else:
                flags = os.O_RDONLY
            if creating:
                flags |= os.O_CREAT | os.O_EXCL
            elif writing:
                flags |= os.O_CREAT | os.O_TRUNC
            elif appending:
                flags |= os.O_CREAT | os.O_APPEND
            if hasattr(os, "O_BINARY"):
                flags |= os.O_BINARY
            if opener is not None:
                fd = opener(path, flags)
                if not isinstance(fd, int) or fd < 0:
                    raise ValueError("opener returned invalid fd")
                self._fd = fd
            else:
                self._fd = os.open(path, flags, 0o666)

    @property
    def closefd(self):
        return self._closefd

    @property
    def closed(self):
        return self._closed

    def _check_open(self):
        if self._closed:
            raise ValueError("I/O operation on closed file.")

    def fileno(self):
        self._check_open()
        return self._fd

    def readable(self):
        self._check_open()
        return self._readable

    def writable(self):
        self._check_open()
        return self._writable

    def seekable(self):
        self._check_open()
        return True

    def isatty(self):
        self._check_open()
        return False

    def read(self, size=-1):
        self._check_open()
        if not self._readable:
            raise io.UnsupportedOperation("File not open for reading")
        if size is None or size < 0:
            return self.readall()
        if size == 0:
            return b""
        return os.read(self._fd, int(size))

    def readall(self):
        self._check_open()
        if not self._readable:
            raise io.UnsupportedOperation("File not open for reading")
        chunks = []
        while True:
            chunk = os.read(self._fd, 65536)
            if not chunk:
                break
            chunks.append(chunk)
        return b"".join(chunks)

    def readinto(self, b):
        self._check_open()
        if not self._readable:
            raise io.UnsupportedOperation("File not open for reading")
        mv = memoryview(b).cast("B")
        if len(mv) == 0:
            return 0
        data = os.read(self._fd, len(mv))
        n = len(data)
        mv[:n] = data
        return n

    def write(self, b):
        self._check_open()
        if not self._writable:
            raise io.UnsupportedOperation("File not open for writing")
        mv = memoryview(b).cast("B")
        return os.write(self._fd, mv.tobytes())

    def seek(self, offset, whence=0):
        self._check_open()
        return os.lseek(self._fd, int(offset), int(whence))

    def tell(self):
        self._check_open()
        return os.lseek(self._fd, 0, 1)

    def truncate(self, size=None):
        self._check_open()
        if not self._writable:
            raise io.UnsupportedOperation("File not open for writing")
        if size is None:
            size = self.tell()
        os.ftruncate(self._fd, int(size))
        return int(size)

    def close(self):
        if not self._closed:
            self._closed = True
            if self._closefd:
                try:
                    os.close(self._fd)
                except OSError:
                    pass
        super().close()

def _sandbox_open(file, mode="r", buffering=-1, encoding=None, errors=None, newline=None, closefd=True, opener=None):
    if not isinstance(mode, str):
        raise TypeError("invalid mode: " + repr(mode))
    modes = set(mode)
    if modes - set("axrwb+tU") or len(mode) > len(modes):
        raise ValueError("invalid mode: " + repr(mode))
    creating = "x" in modes
    reading = "r" in modes
    writing = "w" in modes
    appending = "a" in modes
    updating = "+" in modes
    text = "t" in modes
    binary = "b" in modes
    if "U" in modes:
        if creating or writing or appending or updating:
            raise ValueError("mode U cannot be combined with 'x', 'w', 'a', or '+'")
        reading = True
    if text and binary:
        raise ValueError("can't have text and binary mode at once")
    if sum((creating, reading, writing, appending)) > 1:
        raise ValueError("can't have read/write/append mode at once")
    if not (creating or reading or writing or appending):
        raise ValueError("must have exactly one of create/read/write/append mode")
    if binary and encoding is not None:
        raise ValueError("binary mode doesn't take an encoding argument")
    if binary and errors is not None:
        raise ValueError("binary mode doesn't take an errors argument")
    if binary and newline is not None:
        raise ValueError("binary mode doesn't take a newline argument")
    raw_mode = (
        ("x" if creating else "")
        + ("r" if reading else "")
        + ("w" if writing else "")
        + ("a" if appending else "")
        + ("+" if updating else "")
    )
    raw = _SandboxFileIO(file, raw_mode, closefd=closefd, opener=opener)
    try:
        line_buffering = False
        if buffering == 1 or (buffering < 0 and raw.isatty()):
            buffering = -1
            line_buffering = True
        if buffering < 0:
            buffering = io.DEFAULT_BUFFER_SIZE
        if buffering == 0:
            if not binary:
                raise ValueError("can't have unbuffered text I/O")
            return raw
        if updating:
            buf = io.BufferedRandom(raw, buffering)
        elif creating or writing or appending:
            buf = io.BufferedWriter(raw, buffering)
        else:
            buf = io.BufferedReader(raw, buffering)
        if binary:
            return buf
        if encoding is None:
            encoding = "utf-8"
        wrapper = io.TextIOWrapper(buf, encoding=encoding, errors=errors, newline=newline, line_buffering=line_buffering)
        wrapper.mode = mode
        return wrapper
    except BaseException:
        raw.close()
        raise

io.FileIO = _SandboxFileIO
_io.FileIO = _SandboxFileIO
io.open = _sandbox_open
_io.open = _sandbox_open
io.open_code = lambda path: _sandbox_open(path, "rb")
_io.open_code = io.open_code
builtins.open = _sandbox_open

try:
    import _frozen_importlib as _bs
    import importlib
    import importlib._bootstrap_external as _bse
    sys.dont_write_bytecode = True
    if getattr(_bse, "_bootstrap", None) is None:
        _bse._bootstrap = _bs
    if getattr(_bs, "_bootstrap_external", None) is None:
        _bs._bootstrap_external = _bse
    _bse._io = io
    if hasattr(_bse, "_os") and _bse._os is not None:
        _bse._os.stat = os.stat
        _bse._os.lstat = os.lstat
        _bse._os.fspath = os.fspath
    _bse._get_cached = lambda _path: None
    _ff_hook = _bse.FileFinder.path_hook((_bse.SourceFileLoader, _bse.SOURCE_SUFFIXES))
    sys.path_hooks.append(_ff_hook)
    if _bse.PathFinder not in sys.meta_path:
        sys.meta_path.append(_bse.PathFinder)
    sys.path_importer_cache.clear()
except Exception:
    pass

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

_BRIDGE_CALL_TOOL = _hermes_bridge.call_tool
_BRIDGE_EMIT_OUTPUT = _hermes_bridge.emit_output

def _call(tool, args, _fn=_BRIDGE_CALL_TOOL):
    raw = _fn(tool, json.dumps(args, ensure_ascii=False))
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
_TERMINAL_ENABLED = {terminal_enabled}
_module = types.ModuleType("hermes_tools")
for _name in _EXPORTS:
    setattr(_module, _name, globals()[_name])
sys.modules["hermes_tools"] = _module
{shims}
GLOBALS = {{"__name__": "__main__", "__builtins__": __builtins__}}
_paths = set()

def _hermes_run_cell(code, cwd, _emit=_BRIDGE_EMIT_OUTPUT):
    try:
        if cwd:
            os.chdir(cwd)
            if cwd not in _paths:
                _paths.add(cwd)
                sys.path.insert(0, cwd)
    except Exception:
        pass
    _out, _err = _Capture(_emit), _Capture(_emit)
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

# Hide internal bridge module from user code once closures have captured their handles.
sys.modules["_hermes_bridge"] = None
for _priv_name in ("_hermes_bridge", "_BRIDGE_CALL_TOOL", "_BRIDGE_EMIT_OUTPUT"):
    globals().pop(_priv_name, None)
"#
    )
}

const SANDBOX_SHIMS_PY: &str = r#"
import base64 as _b64

# --- os.get_terminal_size polish ---
class _TerminalSize(tuple):
    columns = property(lambda self: self[0])
    lines = property(lambda self: self[1])
    def __repr__(self):
        return f"os.terminal_size(columns={self[0]}, lines={self[1]})"

os.terminal_size = _TerminalSize
_raw_gts = getattr(os, "get_terminal_size", None)
def _wrapped_get_terminal_size(fd=1):
    res = _raw_gts(fd) if callable(_raw_gts) else (80, 24)
    return _TerminalSize((int(res[0]), int(res[1])))
os.get_terminal_size = _wrapped_get_terminal_size

# --- Safe in-memory _signal handler tracking (unlocks signal, unittest, unittest.mock) ---
try:
    import _signal as _sig_mod
    _SIG_HANDLERS = {_sig_mod.SIGINT: _sig_mod.default_int_handler}
    def _sandbox_getsignal(sig):
        return _SIG_HANDLERS.get(int(sig), _sig_mod.SIG_DFL)
    def _sandbox_signal(sig, handler):
        key = int(sig)
        prev = _SIG_HANDLERS.get(key, _sig_mod.SIG_DFL)
        _SIG_HANDLERS[key] = handler
        return prev
    _sig_mod.getsignal = _sandbox_getsignal
    _sig_mod.signal = _sandbox_signal
except Exception:
    pass

# --- Workspace-scoped sqlite3 backed by host rusqlite ---
def _install_sqlite3(_bridge=_hermes_bridge):
    _sq_open = _bridge.sqlite_open
    _sq_exec = _bridge.sqlite_exec
    _sq_batch = _bridge.sqlite_exec_batch
    _sq_in_tx = _bridge.sqlite_in_transaction
    _sq_close = _bridge.sqlite_close
    mod = types.ModuleType("sqlite3")
    mod.sqlite_version = "3.46.0"
    mod.sqlite_version_info = (3, 46, 0)
    mod.paramstyle = "qmark"
    mod.threadsafety = 1
    mod.apilevel = "2.0"
    mod.PARSE_DECLTYPES = 1
    mod.PARSE_COLNAMES = 2

    class Error(Exception): pass
    class Warning(Exception): pass
    class InterfaceError(Error): pass
    class DatabaseError(Error): pass
    class DataError(DatabaseError): pass
    class OperationalError(DatabaseError): pass
    class IntegrityError(DatabaseError): pass
    class InternalError(DatabaseError): pass
    class ProgrammingError(DatabaseError): pass
    class NotSupportedError(DatabaseError): pass

    def _map_sql_err(err):
        msg = str(err)
        low = msg.lower()
        if "unique" in low or "constraint" in low or "foreign key" in low or "not null" in low:
            return IntegrityError(msg)
        if "closed" in low or "parameter" in low:
            return ProgrammingError(msg)
        return OperationalError(msg)

    def _encode_val(v):
        if isinstance(v, (bytes, bytearray, memoryview)):
            return {"__hsr_bytes__": _b64.b64encode(bytes(v)).decode("ascii")}
        return v

    def _encode_params(params):
        if params is None:
            return "null"
        if isinstance(params, dict):
            return json.dumps({str(k): _encode_val(v) for k, v in params.items()}, ensure_ascii=False)
        return json.dumps([_encode_val(v) for v in params], ensure_ascii=False)

    def _decode_val(v):
        if isinstance(v, dict) and len(v) == 1 and "__hsr_bytes__" in v:
            return _b64.b64decode(v["__hsr_bytes__"])
        return v

    class Row:
        def __init__(self, cursor, values):
            self._cols = [d[0] for d in (cursor.description or ())]
            self._vals = tuple(values)
            self._index = {c.lower(): i for i, c in enumerate(self._cols)}
        def keys(self):
            return list(self._cols)
        def __len__(self):
            return len(self._vals)
        def __iter__(self):
            return iter(self._vals)
        def __getitem__(self, key):
            if isinstance(key, str):
                idx = self._index.get(key.lower())
                if idx is None:
                    raise IndexError(f"No item with that key: {key}")
                return self._vals[idx]
            return self._vals[key]
        def __eq__(self, other):
            if isinstance(other, Row):
                return self._vals == other._vals and self._cols == other._cols
            if isinstance(other, tuple):
                return self._vals == other
            return NotImplemented
        def __repr__(self):
            return f"<sqlite3.Row {self._vals!r}>"

    class Cursor:
        def __init__(self, conn):
            self.connection = conn
            self.row_factory = conn.row_factory
            self.description = None
            self.rowcount = -1
            self.lastrowid = None
            self._rows = []
            self._pos = 0
            self.arraysize = 1

        def _format_row(self, raw_row):
            vals = tuple(_decode_val(v) for v in raw_row)
            factory = self.row_factory or self.connection.row_factory
            return factory(self, vals) if factory is not None else vals

        def execute(self, sql, parameters=()):
            self.connection._check_open()
            try:
                raw = _sq_exec(self.connection._id, str(sql), _encode_params(parameters))
            except PermissionError:
                raise
            except Exception as e:
                raise _map_sql_err(e) from None
            res = json.loads(raw)
            cols = res.get("columns") or []
            self.description = tuple((c, None, None, None, None, None, None) for c in cols) if cols else None
            self._rows = [self._format_row(r) for r in (res.get("rows") or [])]
            self._pos = 0
            self.rowcount = res.get("rowcount", -1)
            self.lastrowid = res.get("lastrowid")
            return self

        def executemany(self, sql, seq_of_parameters):
            total = 0
            for params in seq_of_parameters:
                self.execute(sql, params)
                if self.rowcount >= 0:
                    total += self.rowcount
            self.rowcount = total
            return self

        def executescript(self, sql_script):
            self.connection._check_open()
            try:
                _sq_batch(self.connection._id, str(sql_script))
            except Exception as e:
                raise _map_sql_err(e) from None
            self.description = None
            self._rows = []
            self._pos = 0
            return self

        def fetchone(self):
            if self._pos >= len(self._rows):
                return None
            row = self._rows[self._pos]
            self._pos += 1
            return row

        def fetchmany(self, size=None):
            if size is None:
                size = self.arraysize
            end = self._pos + int(size)
            batch = self._rows[self._pos:end]
            self._pos = min(end, len(self._rows))
            return batch

        def fetchall(self):
            rest = self._rows[self._pos:]
            self._pos = len(self._rows)
            return rest

        def close(self):
            self._rows = []

        def __iter__(self):
            return self

        def __next__(self):
            row = self.fetchone()
            if row is None:
                raise StopIteration
            return row

    class Connection:
        def __init__(self, database, uri=False, **kwargs):
            raw_db = os.fspath(database) if not isinstance(database, str) else database
            read_only = False
            if uri and raw_db.startswith("file:"):
                stripped = raw_db[5:]
                if "?" in stripped:
                    path_part, query = stripped.split("?", 1)
                    if "mode=ro" in query:
                        read_only = True
                    raw_db = path_part
                else:
                    raw_db = stripped
            try:
                self._id = _sq_open(raw_db, read_only)
            except PermissionError:
                raise
            except Exception as e:
                raise _map_sql_err(e) from None
            self._closed = False
            self.row_factory = None
            self.isolation_level = kwargs.get("isolation_level", "")

        def _check_open(self):
            if self._closed:
                raise ProgrammingError("Cannot operate on a closed database.")

        @property
        def in_transaction(self):
            if self._closed:
                return False
            return bool(_sq_in_tx(self._id))

        def cursor(self, factory=Cursor):
            self._check_open()
            return factory(self)

        def execute(self, sql, parameters=()):
            return self.cursor().execute(sql, parameters)

        def executemany(self, sql, seq_of_parameters):
            return self.cursor().executemany(sql, seq_of_parameters)

        def executescript(self, sql_script):
            return self.cursor().executescript(sql_script)

        def commit(self):
            self._check_open()
            if self.in_transaction:
                _sq_batch(self._id, "COMMIT;")

        def rollback(self):
            self._check_open()
            if self.in_transaction:
                _sq_batch(self._id, "ROLLBACK;")

        def close(self):
            if not self._closed:
                self._closed = True
                _sq_close(self._id)

        def __enter__(self):
            self._check_open()
            return self

        def __exit__(self, exc_type, exc_val, exc_tb):
            if not self._closed:
                if exc_type is None:
                    self.commit()
                else:
                    self.rollback()
            return False

    def connect(database, *args, **kwargs):
        return Connection(database, **kwargs)

    for _k, _v in {
        "Error": Error, "Warning": Warning, "InterfaceError": InterfaceError,
        "DatabaseError": DatabaseError, "DataError": DataError,
        "OperationalError": OperationalError, "IntegrityError": IntegrityError,
        "InternalError": InternalError, "ProgrammingError": ProgrammingError,
        "NotSupportedError": NotSupportedError, "Row": Row, "Cursor": Cursor,
        "Connection": Connection, "connect": connect,
    }.items():
        setattr(mod, _k, _v)
    sys.modules["sqlite3"] = mod

_install_sqlite3()

# --- Safe PyYAML-compatible yaml module backed by serde_yaml ---
def _install_yaml(_bridge=_hermes_bridge):
    _yl_load_all = _bridge.yaml_load_all
    _yl_dump = _bridge.yaml_dump
    mod = types.ModuleType("yaml")
    class YAMLError(Exception): pass
    class SafeLoader: pass
    class FullLoader(SafeLoader): pass
    class BaseLoader(SafeLoader): pass
    class Loader(SafeLoader): pass
    class CSafeLoader(SafeLoader): pass
    class SafeDumper: pass
    class Dumper(SafeDumper): pass

    def _read_stream(stream):
        if hasattr(stream, "read"):
            data = stream.read()
        else:
            data = stream
        if isinstance(data, (bytes, bytearray)):
            return data.decode("utf-8")
        return "" if data is None else str(data)

    def safe_load_all(stream):
        text = _read_stream(stream)
        if not text.strip():
            return iter(())
        try:
            docs = json.loads(_yl_load_all(text))
        except Exception as e:
            raise YAMLError(str(e)) from None
        return iter(docs)

    def safe_load(stream):
        docs = list(safe_load_all(stream))
        if not docs:
            return None
        if len(docs) > 1:
            raise YAMLError("expected a single document in the stream, but found multiple")
        return docs[0]

    def load(stream, Loader=None):
        return safe_load(stream)

    def load_all(stream, Loader=None):
        return safe_load_all(stream)

    def safe_dump(data, stream=None, **kwargs):
        try:
            j = json.dumps(data, ensure_ascii=False)
            out = _yl_dump(j)
        except Exception as e:
            raise YAMLError(str(e)) from None
        if stream is not None:
            stream.write(out)
            return None
        return out

    def dump(data, stream=None, **kwargs):
        return safe_dump(data, stream=stream, **kwargs)

    for _k, _v in {
        "YAMLError": YAMLError, "SafeLoader": SafeLoader, "FullLoader": FullLoader,
        "BaseLoader": BaseLoader, "Loader": Loader, "CSafeLoader": CSafeLoader,
        "SafeDumper": SafeDumper, "Dumper": Dumper, "safe_load": safe_load,
        "safe_load_all": safe_load_all, "load": load, "load_all": load_all,
        "safe_dump": safe_dump, "dump": dump,
    }.items():
        setattr(mod, _k, _v)
    sys.modules["yaml"] = mod

_install_yaml()

# --- Policy-gated HTTP client backing urllib.request and requests ---
def _install_http_shims():
    import urllib
    import urllib.parse as _uparse

    class _CaseInsensitiveHeaders(dict):
        def __init__(self, data=None):
            super().__init__()
            self._lower = {}
            if data:
                for k, v in data.items():
                    self[k] = v
        def __setitem__(self, k, v):
            super().__setitem__(k, v)
            self._lower[str(k).lower()] = (k, v)
        def get(self, k, default=None):
            hit = self._lower.get(str(k).lower())
            return hit[1] if hit is not None else default
        def __getitem__(self, k):
            hit = self._lower.get(str(k).lower())
            if hit is None:
                raise KeyError(k)
            return hit[1]
        def __contains__(self, k):
            return str(k).lower() in self._lower
        def get_content_charset(self, failobj="utf-8"):
            ct = self.get("content-type", "")
            for part in ct.split(";")[1:]:
                if "=" in part:
                    nk, nv = part.split("=", 1)
                    if nk.strip().lower() == "charset":
                        return nv.strip().strip('"').strip("'")
            return failobj

    def _do_http(url, method="GET", headers=None, data=None, timeout=30):
        payload = {"url": str(url), "method": str(method).upper(), "timeout": timeout}
        if headers:
            payload["headers"] = {str(k): str(v) for k, v in dict(headers).items()}
        if data is not None:
            if isinstance(data, (bytes, bytearray, memoryview)):
                payload["body_b64"] = _b64.b64encode(bytes(data)).decode("ascii")
            else:
                payload["body"] = str(data)
        return _call("http_request", payload)

    # 1. urllib.request
    ur = types.ModuleType("urllib.request")
    try:
        import urllib.error as _uerr
        URLError = _uerr.URLError
        HTTPError = _uerr.HTTPError
    except Exception:
        class URLError(OSError):
            def __init__(self, reason):
                super().__init__(str(reason))
                self.reason = reason
        class HTTPError(URLError):
            def __init__(self, url, code, msg, hdrs, fp):
                super().__init__(f"HTTP Error {code}: {msg}")
                self.url = url
                self.code = code
                self.msg = msg
                self.hdrs = hdrs
                self.fp = fp
            def read(self, amt=None):
                return self.fp.read(amt) if self.fp is not None else b""

    class Request:
        def __init__(self, url, data=None, headers=None, origin_req_host=None, unverifiable=False, method=None):
            self.full_url = str(url)
            self.data = data
            self.headers = dict(headers or {})
            self.method = method
        def add_header(self, key, val):
            self.headers[key] = val
        def get_method(self):
            if self.method:
                return self.method.upper()
            return "POST" if self.data is not None else "GET"

    class _URLResponse:
        def __init__(self, status, url, headers, body_bytes):
            self.status = int(status)
            self.code = self.status
            self.url = url
            self.headers = _CaseInsensitiveHeaders(headers)
            self._buf = io.BytesIO(body_bytes)
        def geturl(self):
            return self.url
        def info(self):
            return self.headers
        def getcode(self):
            return self.status
        def read(self, amt=-1):
            return self._buf.read() if amt is None or amt < 0 else self._buf.read(amt)
        def readline(self, limit=-1):
            return self._buf.readline(limit)
        def readlines(self, hint=-1):
            return self._buf.readlines(hint)
        def close(self):
            self._buf.close()
        def __enter__(self):
            return self
        def __exit__(self, *args):
            self.close()
            return False

    def urlopen(url, data=None, timeout=30, **kwargs):
        if isinstance(url, Request):
            target = url.full_url
            method = url.get_method()
            headers = url.headers
            body = data if data is not None else url.data
        else:
            target = str(url)
            method = "POST" if data is not None else "GET"
            headers = {}
            body = data
        res = _do_http(target, method=method, headers=headers, data=body, timeout=timeout)
        if res.get("status") in ("approval_denied", "approval_unavailable"):
            raise PermissionError(res.get("error") or res.get("message") or "HTTP request approval denied")
        if "error" in res:
            raise URLError(res["error"])
        body_bytes = _b64.b64decode(res.get("body_b64", ""))
        status = int(res.get("status", 0))
        hdrs = _CaseInsensitiveHeaders(res.get("headers", {}))
        if status >= 400:
            raise HTTPError(res.get("url", target), status, f"HTTP {status}", hdrs, io.BytesIO(body_bytes))
        return _URLResponse(status, res.get("url", target), hdrs, body_bytes)

    def urlretrieve(url, filename=None, reporthook=None, data=None):
        with urlopen(url, data=data) as resp:
            content = resp.read()
            hdrs = resp.headers
        if filename is None:
            import tempfile as _tf
            fd, filename = _tf.mkstemp()
            os.close(fd)
        with open(filename, "wb") as f:
            f.write(content)
        return filename, hdrs

    class _DummyOpener:
        def open(self, fullurl, data=None, timeout=30):
            return urlopen(fullurl, data=data, timeout=timeout)

    ur.Request = Request
    ur.urlopen = urlopen
    ur.urlretrieve = urlretrieve
    ur.URLError = URLError
    ur.HTTPError = HTTPError
    ur.build_opener = lambda *handlers: _DummyOpener()
    ur.install_opener = lambda opener: None
    sys.modules["urllib.request"] = ur
    setattr(urllib, "request", ur)

    # 2. requests (policy-gated lightweight compatible shim)
    req_mod = types.ModuleType("requests")
    class RequestException(OSError):
        def __init__(self, *args, response=None):
            super().__init__(*args)
            self.response = response
    class ConnectionError(RequestException): pass
    class Timeout(RequestException): pass
    class ReqHTTPError(RequestException): pass

    class Response:
        def __init__(self, status, url, headers, content):
            self.status_code = int(status)
            self.ok = 200 <= self.status_code < 400
            self.url = url
            self.headers = _CaseInsensitiveHeaders(headers)
            self.content = content
            self.encoding = self.headers.get_content_charset("utf-8")
        @property
        def text(self):
            return self.content.decode(self.encoding or "utf-8", errors="replace")
        def json(self, **kwargs):
            return json.loads(self.text, **kwargs)
        def raise_for_status(self):
            if self.status_code >= 400:
                raise ReqHTTPError(f"{self.status_code} Error for url: {self.url}", response=self)
        def close(self):
            pass
        def __enter__(self):
            return self
        def __exit__(self, *args):
            return False

    def _req(method, url, params=None, data=None, json_body=None, headers=None, timeout=30, **kwargs):
        hdrs = dict(headers or {})
        if params:
            sep = "&" if "?" in str(url) else "?"
            url = f"{url}{sep}{_uparse.urlencode(params, doseq=True)}"
        body = data
        if json_body is not None:
            body = json.dumps(json_body, ensure_ascii=False).encode("utf-8")
            if not any(k.lower() == "content-type" for k in hdrs):
                hdrs["Content-Type"] = "application/json"
        elif isinstance(data, dict):
            body = _uparse.urlencode(data, doseq=True).encode("utf-8")
            if not any(k.lower() == "content-type" for k in hdrs):
                hdrs["Content-Type"] = "application/x-www-form-urlencoded"
        if isinstance(timeout, (tuple, list)) and timeout:
            timeout = timeout[-1]
        res = _do_http(url, method=method, headers=hdrs, data=body, timeout=timeout or 30)
        if res.get("status") in ("approval_denied", "approval_unavailable"):
            raise PermissionError(res.get("error") or res.get("message") or "HTTP request approval denied")
        if "error" in res:
            msg = str(res["error"])
            if "timed out" in msg.lower():
                raise Timeout(msg)
            raise ConnectionError(msg)
        content = _b64.b64decode(res.get("body_b64", ""))
        return Response(res.get("status", 0), res.get("url", str(url)), res.get("headers", {}), content)

    def request(method, url, **kwargs):
        json_body = kwargs.pop("json", None)
        return _req(method, url, json_body=json_body, **kwargs)

    class Session:
        def __init__(self):
            self.headers = _CaseInsensitiveHeaders()
        def request(self, method, url, headers=None, **kwargs):
            merged = dict(self.headers)
            if headers:
                merged.update(headers)
            json_body = kwargs.pop("json", None)
            return _req(method, url, headers=merged, json_body=json_body, **kwargs)
        def get(self, url, **kwargs): return self.request("GET", url, **kwargs)
        def post(self, url, **kwargs): return self.request("POST", url, **kwargs)
        def put(self, url, **kwargs): return self.request("PUT", url, **kwargs)
        def patch(self, url, **kwargs): return self.request("PATCH", url, **kwargs)
        def delete(self, url, **kwargs): return self.request("DELETE", url, **kwargs)
        def head(self, url, **kwargs): return self.request("HEAD", url, **kwargs)
        def close(self): pass
        def __enter__(self): return self
        def __exit__(self, *args): return False

    req_mod.RequestException = RequestException
    req_mod.ConnectionError = ConnectionError
    req_mod.Timeout = Timeout
    req_mod.HTTPError = ReqHTTPError
    req_mod.Response = Response
    req_mod.Session = Session
    req_mod.request = request
    req_mod.get = lambda url, **kw: request("GET", url, **kw)
    req_mod.post = lambda url, **kw: request("POST", url, **kw)
    req_mod.put = lambda url, **kw: request("PUT", url, **kw)
    req_mod.patch = lambda url, **kw: request("PATCH", url, **kw)
    req_mod.delete = lambda url, **kw: request("DELETE", url, **kw)
    req_mod.head = lambda url, **kw: request("HEAD", url, **kw)
    sys.modules["requests"] = req_mod

_install_http_shims()

# --- Approval-gated subprocess shim routed through hermes_tools.terminal ---
def _install_subprocess_shim():
    mod = types.ModuleType("subprocess")
    mod.PIPE = -1
    mod.STDOUT = -2
    mod.DEVNULL = -3

    class SubprocessError(Exception): pass
    class CalledProcessError(SubprocessError):
        def __init__(self, returncode, cmd, output=None, stderr=None):
            self.returncode = returncode
            self.cmd = cmd
            self.output = output
            self.stdout = output
            self.stderr = stderr
            super().__init__(f"Command {cmd!r} returned non-zero exit status {returncode}.")
    class TimeoutExpired(SubprocessError):
        def __init__(self, cmd, timeout, output=None, stderr=None):
            self.cmd = cmd
            self.timeout = timeout
            self.output = output
            self.stdout = output
            self.stderr = stderr
            super().__init__(f"Command {cmd!r} timed out after {timeout} seconds")

    class CompletedProcess:
        def __init__(self, args, returncode, stdout=None, stderr=None):
            self.args = args
            self.returncode = int(returncode)
            self.stdout = stdout
            self.stderr = stderr
        def check_returncode(self):
            if self.returncode:
                raise CalledProcessError(self.returncode, self.args, self.stdout, self.stderr)
        def __repr__(self):
            return f"CompletedProcess(args={self.args!r}, returncode={self.returncode!r}, stdout={self.stdout!r}, stderr={self.stderr!r})"

    def list2cmdline(seq):
        result = []
        for arg in seq:
            s = os.fspath(arg)
            needquote = (" " in s) or ("\t" in s) or (not s)
            if result:
                result.append(" ")
            if needquote:
                result.append('"')
            bs = []
            for c in s:
                if c == "\\":
                    bs.append(c)
                elif c == '"':
                    result.append("\\" * (len(bs) * 2))
                    bs = []
                    result.append('\\"')
                else:
                    if bs:
                        result.extend(bs)
                        bs = []
                    result.append(c)
            if bs:
                result.extend(bs)
            if needquote:
                result.extend(bs)
                result.append('"')
        return "".join(result)

    def _quote_cmd_arg(s):
        if "\r" in s or "\n" in s:
            raise ValueError("newlines are not supported in Windows subprocess argument lists")
        # Quote any argument containing whitespace or cmd.exe metacharacters so shell=False does
        # not let cmd.exe interpret &, |, <, >, ^, %, !, (, ) as command separators.
        meta = set(' \t&|<>^%!()')
        if not s or any(c in meta for c in s) or '"' in s:
            escaped = s.replace('"', '""').replace('%', '%%')
            return '"' + escaped + '"'
        return s

    def _format_cmd(args, shell):
        if isinstance(args, (str, bytes)):
            return args.decode("utf-8", errors="replace") if isinstance(args, bytes) else args
        if hasattr(args, "__fspath__"):
            return os.fspath(args)
        parts = [os.fspath(a) for a in args]
        if shell and len(parts) == 1:
            return parts[0]
        if os.name == "nt":
            if shell:
                return list2cmdline(parts)
            return " ".join(_quote_cmd_arg(p) for p in parts)
        return " ".join(shlex.quote(p) for p in parts)

    def run(args, *, stdin=None, input=None, stdout=None, stderr=None, capture_output=False,
            shell=False, cwd=None, timeout=None, check=False, encoding=None, errors=None,
            text=None, env=None, universal_newlines=None):
        if not _TERMINAL_ENABLED:
            raise PermissionError("subprocess is disabled because the terminal tool is not enabled in this session")
        if input is not None or stdin is not None or env is not None:
            raise NotImplementedError("sandbox subprocess does not support input, stdin, or env overrides")
        cmd = _format_cmd(args, shell)
        payload = {"command": cmd}
        if timeout is not None:
            payload["timeout"] = timeout
        if cwd is not None:
            payload["workdir"] = os.fspath(cwd)
        res = _call("terminal", payload)
        if not isinstance(res, dict):
            res = {"output": str(res), "exit_code": 0}
        if "error" in res and "exit_code" not in res:
            err = str(res["error"])
            low = err.lower()
            if "approval" in low or "denied" in low or "unavailable" in low or "not available" in low:
                raise PermissionError(err)
            if "timed out" in low or "timeout" in low:
                raise TimeoutExpired(args, timeout or 0, output=res.get("output"))
            raise OSError(err)
        raw_out = str(res.get("output", ""))
        code = int(res.get("exit_code", 0))
        want_text = bool(text or universal_newlines or encoding is not None or errors is not None)
        want_capture = bool(capture_output or stdout == mod.PIPE or stderr == mod.PIPE)
        if want_capture:
            out_val = raw_out if want_text else raw_out.encode(encoding or "utf-8", errors=errors or "replace")
            err_val = "" if want_text else b""
        else:
            out_val = None
            err_val = None
            if raw_out:
                sys.stdout.write(raw_out)
        cp = CompletedProcess(args, code, stdout=out_val, stderr=err_val)
        if check and code != 0:
            raise CalledProcessError(code, args, output=out_val, stderr=err_val)
        return cp

    def check_output(args, **kwargs):
        kwargs.setdefault("stdout", mod.PIPE)
        kwargs["check"] = True
        return run(args, **kwargs).stdout

    def check_call(args, **kwargs):
        kwargs["check"] = True
        return run(args, **kwargs).returncode

    def call(args, **kwargs):
        return run(args, **kwargs).returncode

    for _k, _v in {
        "SubprocessError": SubprocessError, "CalledProcessError": CalledProcessError,
        "TimeoutExpired": TimeoutExpired, "CompletedProcess": CompletedProcess,
        "list2cmdline": list2cmdline, "run": run, "check_output": check_output,
        "check_call": check_call, "call": call,
    }.items():
        setattr(mod, _k, _v)
    sys.modules["subprocess"] = mod

_install_subprocess_shim()

# --- Safe coroutine-only asyncio shim (no raw sockets; unlocks unittest.mock and asyncio.run) ---
def _install_asyncio_shim():
    import inspect as _inspect
    mod = types.ModuleType("asyncio")
    class CancelledError(BaseException): pass
    class InvalidStateError(Exception): pass

    def iscoroutinefunction(func):
        return _inspect.iscoroutinefunction(func)

    def iscoroutine(obj):
        return _inspect.iscoroutine(obj)

    def isfuture(obj):
        return False

    async def sleep(delay, result=None):
        if delay and delay > 0:
            time.sleep(delay)
        return result

    def _drive(coro):
        if not iscoroutine(coro):
            return coro
        try:
            coro.send(None)
        except StopIteration as stop:
            return stop.value
        raise RuntimeError("sandbox asyncio.run only supports synchronous/non-yielding coroutines and asyncio.sleep")

    def run(main, *, debug=None):
        return _drive(main)

    async def gather(*aws, return_exceptions=False):
        results = []
        for aw in aws:
            try:
                results.append(await aw)
            except Exception as e:
                if return_exceptions:
                    results.append(e)
                else:
                    raise
        return results

    def _no_net(*args, **kwargs):
        raise PermissionError("raw network sockets are disabled in the sandbox; use hermes_tools.http_request or requests")

    for _k, _v in {
        "CancelledError": CancelledError, "TimeoutError": TimeoutError,
        "InvalidStateError": InvalidStateError, "iscoroutinefunction": iscoroutinefunction,
        "iscoroutine": iscoroutine, "isfuture": isfuture, "sleep": sleep,
        "run": run, "gather": gather, "open_connection": _no_net, "start_server": _no_net,
    }.items():
        setattr(mod, _k, _v)
    sys.modules["asyncio"] = mod

_install_asyncio_shim()
"#;

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
                    *slot.borrow_mut() = Some(BridgeState::new(
                        BridgeTransport::InProcess {
                            event_tx: event_tx.clone(),
                            reply_rx,
                        },
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                        env,
                        Arc::clone(&abort_worker),
                    ));
                });

                let builder = Interpreter::builder(Settings::default());
                let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
                let os_def = _sandbox_os::module_def(&builder.ctx);
                let signal_def = _sandbox_signal::module_def(&builder.ctx);
                let bridge_def = _hermes_bridge::module_def(&builder.ctx);
                #[cfg(windows)]
                let winreg_def = _sandbox_winreg::module_def(&builder.ctx);
                let builder = builder
                    .add_native_modules(&stdlib_defs)
                    .add_native_module(os_def)
                    .add_native_module(signal_def)
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
                                set_host_workspace_root(&cwd);
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
        *slot.borrow_mut() = Some(BridgeState::new(
            BridgeTransport::Stdio,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            init.env,
            abort,
        ));
    });

    let builder = Interpreter::builder(Settings::default());
    let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
    let os_def = _sandbox_os::module_def(&builder.ctx);
    let signal_def = _sandbox_signal::module_def(&builder.ctx);
    let bridge_def = _hermes_bridge::module_def(&builder.ctx);
    #[cfg(windows)]
    let winreg_def = _sandbox_winreg::module_def(&builder.ctx);
    let builder = builder
        .add_native_modules(&stdlib_defs)
        .add_native_module(os_def)
        .add_native_module(signal_def)
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
                    set_host_workspace_root(cwd);
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
                        "pdf_read" => crate::execute_code::pdf_read_tool(dir, None, &args),
                        "http_request" => {
                            crate::execute_code::sandbox_http_request(&args, None, deadline)
                                .await
                                .unwrap_or_else(|()| json!({"error": "timed out"}))
                        }
                        "terminal" => {
                            let cmd = args["command"].as_str().unwrap_or("");
                            if cmd.contains("DENY_ME") {
                                json!({"error": "approval_denied: user rejected command"})
                            } else if cmd.contains("FAIL_ME") {
                                json!({"output": "boom\n", "exit_code": 2})
                            } else {
                                json!({"output": format!("ran:{cmd}\n"), "exit_code": 0})
                            }
                        }
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

    #[tokio::test]
    async fn embedded_rustpython_supports_stdlib_fs_io_and_enforces_workspace_write_scope() {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_target = outside_dir.path().join("escaped.txt");
        let mut kernel = EmbeddedKernel::spawn(&[], Vec::new()).expect("spawn embedded kernel");
        let deadline = Instant::now() + Duration::from_secs(30);

        let code = format!(
            r#"
import glob, json, os, pathlib, shutil, tarfile, tempfile, zipfile, zoneinfo

# 1. open() text + binary + append + seek/truncate
with open("notes.txt", "w", encoding="utf-8") as f:
    f.write("hello ")
with open("notes.txt", "a", encoding="utf-8") as f:
    f.write("rustpython")
assert open("notes.txt", "r", encoding="utf-8").read() == "hello rustpython"

# 2. pathlib, os.makedirs, os.scandir, os.walk, glob
p = pathlib.Path("nested/sub/data.json")
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(json.dumps({{"ok": True}}), encoding="utf-8")
assert json.loads(p.read_text(encoding="utf-8"))["ok"] is True
assert sorted(x.name for x in pathlib.Path("nested").rglob("*.json")) == ["data.json"]
walked = [(root, dirs, files) for root, dirs, files in os.walk("nested")]
assert any("data.json" in files for _, _, files in walked), walked
assert glob.glob("nested/**/*.json", recursive=True)

# 3. shutil copy/copytree/rmtree + os.replace
shutil.copy("notes.txt", "notes_copy.txt")
assert pathlib.Path("notes_copy.txt").read_text() == "hello rustpython"
os.replace("notes_copy.txt", "notes_moved.txt")
assert not os.path.exists("notes_copy.txt")
assert os.path.exists("notes_moved.txt")
shutil.copytree("nested", "nested_copy")
assert pathlib.Path("nested_copy/sub/data.json").exists()
shutil.rmtree("nested_copy")
assert not pathlib.Path("nested_copy").exists()

# 4. tempfile (NamedTemporaryFile + TemporaryDirectory) inside isolated temp_root
with tempfile.NamedTemporaryFile("w+", delete=True) as tf:
    tf.write("tmp-data")
    tf.seek(0)
    assert tf.read() == "tmp-data"
with tempfile.TemporaryDirectory() as td:
    tp = pathlib.Path(td) / "item.txt"
    tp.write_text("inside-temp")
    assert tp.read_text() == "inside-temp"

# 5. zipfile (DEFLATED) + tarfile
with zipfile.ZipFile("bundle.zip", "w", compression=zipfile.ZIP_DEFLATED) as zf:
    zf.writestr("a/b.txt", "zipped-content" * 20)
with zipfile.ZipFile("bundle.zip", "r") as zf:
    assert zf.read("a/b.txt") == b"zipped-content" * 20
with tarfile.open("bundle.tar.gz", "w:gz") as tf:
    tf.add("notes.txt")
with tarfile.open("bundle.tar.gz", "r:gz") as tf:
    assert "notes.txt" in tf.getnames()

# 6. Local .py module import from workspace via PathFinder / FileFinder
import importlib
pathlib.Path("local_helper_mod.py").write_text("VALUE = 42\ndef double(x):\n    return x * 2\n", encoding="utf-8")
importlib.invalidate_caches()
import local_helper_mod
assert local_helper_mod.double(local_helper_mod.VALUE) == 84

# 7. Write outside workspace/temp MUST raise PermissionError (even after os.chdir)
outside = {outside:?}
blocked = False
try:
    with open(outside, "w") as f:
        f.write("escape")
except PermissionError:
    blocked = True
assert blocked, "expected PermissionError when writing outside workspace"

# Also verify FileNotFoundError is raised properly
not_found = False
try:
    open("does_not_exist_12345.txt", "r")
except FileNotFoundError:
    not_found = True
assert not_found, "expected FileNotFoundError"

print("STDLIB_FS_OK")
"#,
            outside = outside_target.display().to_string()
        );

        let payload = run_test_cell(&mut kernel, &code, dir.path(), deadline).await;
        assert_eq!(payload["status"], "ok", "{payload}");
        assert!(
            payload["stdout"].as_str().unwrap().contains("STDLIB_FS_OK"),
            "{payload}"
        );
        assert!(
            !outside_target.exists(),
            "outside file must not have been created"
        );
        kernel.shutdown();
    }

    #[tokio::test]
    async fn embedded_rustpython_supports_signal_unittest_sqlite3_yaml_http_pdf_and_gated_subprocess(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_db = outside_dir.path().join("outside.db");
        let outside_attach = outside_dir.path().join("attached.db");
        let outside_vacuum = outside_dir.path().join("vacuumed.db");

        // Create a minimal valid PDF and synthetic secret files inside workspace for C1 testing.
        std::fs::write(dir.path().join(".env"), "API_KEY=synthetic-secret\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".ssh").join("id_rsa"), "synthetic-key\n").unwrap();
        let minimal_pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Count 1/Kids[3 0 R]>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents 4 0 R>>endobj\n4 0 obj<</Length 44>>stream\nBT /F1 12 Tf 10 100 Td (Hello Hermes PDF) Tj ET\nendstream\nendobj\nxref\n0 5\n0000000000 65535 f \n0000000009 00000 n \n0000000056 00000 n \n0000000111 00000 n \n0000000190 00000 n \ntrailer<</Size 5/Root 1 0 R>>\nstartxref\n283\n%%EOF\n";
        std::fs::write(dir.path().join("sample.pdf"), minimal_pdf).unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);

        // Part A: kernel WITHOUT terminal — subprocess MUST raise PermissionError.
        {
            let mut kernel = EmbeddedKernel::spawn(&[], Vec::new()).expect("spawn kernel");
            let code = format!(
                r#"
import os, signal, sqlite3, subprocess, unittest, unittest.mock, urllib.request, requests, yaml
from hermes_tools import pdf_read

# 1. signal + unittest + unittest.mock
prev = signal.signal(signal.SIGINT, signal.SIG_IGN)
assert signal.getsignal(signal.SIGINT) == signal.SIG_IGN
signal.signal(signal.SIGINT, prev)

class MathTests(unittest.TestCase):
    def test_add(self):
        self.assertEqual(2 + 2, 4)
    def test_mock(self):
        m = unittest.mock.MagicMock(return_value=99)
        self.assertEqual(m("x"), 99)

suite = unittest.defaultTestLoader.loadTestsFromTestCase(MathTests)
res = unittest.TextTestRunner(verbosity=0).run(suite)
assert res.wasSuccessful() and res.testsRun == 2

# 2. sqlite3 (:memory:, workspace file, Row, BLOB, IntegrityError, and outside PermissionError)
conn = sqlite3.connect(":memory:")
conn.row_factory = sqlite3.Row
conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT UNIQUE, raw BLOB)")
conn.execute("INSERT INTO items (name, raw) VALUES (?, ?)", ("alpha", b"\x00\x01hi"))
row = conn.execute("SELECT name, raw FROM items WHERE name = :n", {{"n": "alpha"}}).fetchone()
assert row["name"] == "alpha" and row["raw"] == b"\x00\x01hi"
integrity_hit = False
try:
    conn.execute("INSERT INTO items (name, raw) VALUES (?, ?)", ("alpha", b"x"))
except sqlite3.IntegrityError:
    integrity_hit = True
assert integrity_hit
conn.close()

with sqlite3.connect("local.db") as fconn:
    fconn.execute("CREATE TABLE kv (k TEXT, v INT)")
    fconn.execute("INSERT INTO kv VALUES (?, ?)", ("k1", 7))
with sqlite3.connect("local.db") as fconn2:
    assert fconn2.execute("SELECT v FROM kv WHERE k = 'k1'").fetchone() == (7,)

db_blocked = False
try:
    sqlite3.connect({outside_db:?})
except PermissionError:
    db_blocked = True
assert db_blocked, "sqlite3 outside workspace must raise PermissionError"

# B2 regression: _hermes_bridge is inaccessible to user code, and sqlite3 ATTACH / VACUUM INTO
# cannot create files outside workspace.
bridge_blocked = False
try:
    import _hermes_bridge
except ModuleNotFoundError:
    bridge_blocked = True
assert bridge_blocked, "user code must not be able to import _hermes_bridge"

mem = sqlite3.connect(":memory:")
mem.execute("CREATE TABLE t (x INT)")
mem.execute("INSERT INTO t VALUES (1)")
attach_blocked = False
try:
    mem.execute("ATTACH DATABASE ? AS ext", ({outside_attach:?},))
except sqlite3.Error:
    attach_blocked = True
assert attach_blocked, "sqlite3 ATTACH DATABASE must be blocked"

vacuum_blocked = False
try:
    mem.execute("VACUUM /* bypass */ INTO " + repr({outside_vacuum:?}))
except sqlite3.Error:
    vacuum_blocked = True
assert vacuum_blocked, "sqlite3 VACUUM INTO must be blocked"
assert mem.execute("SELECT x FROM t").fetchone() == (1,)
mem.close()

# 3. yaml (safe_load, safe_load_all, safe_dump, YAMLError)
parsed = yaml.safe_load("name: demo\ncount: 3\ntags:\n  - a\n  - b\n")
assert parsed == {{"name": "demo", "count": 3, "tags": ["a", "b"]}}, parsed
docs = list(yaml.safe_load_all("---\na: 1\n---\nb: 2\n"))
assert docs == [{{"a": 1}}, {{"b": 2}}], docs
dumped = yaml.safe_dump({{"hello": "world", "nums": [1, 2]}})
assert yaml.safe_load(dumped) == {{"hello": "world", "nums": [1, 2]}}

# 4. os polish (get_terminal_size, samefile, dup) + pdf_read
ts = os.get_terminal_size()
assert ts.columns == 80 and ts.lines == 24
with open("dup_test.txt", "w") as f:
    f.write("abc")
assert os.path.samefile("dup_test.txt", "./dup_test.txt")
fd1 = os.open("dup_test.txt", os.O_RDONLY)
fd2 = os.dup(fd1)
assert os.read(fd2, 3) == b"abc"
os.close(fd1)
os.close(fd2)

pdf_res = pdf_read("sample.pdf")
assert pdf_res.get("status") == "ok", pdf_res
assert "Hello Hermes PDF" in pdf_res["text"], pdf_res

# 5. urllib.request & requests SSRF guard blocks loopback and cloud metadata
for blocked_url in ("http://127.0.0.1:1/", "http://169.254.169.254/latest/meta-data"):
    hit_ur = False
    try:
        urllib.request.urlopen(blocked_url)
    except urllib.request.URLError as e:
        hit_ur = "Blocked" in str(e)
    assert hit_ur, f"urllib did not block {{blocked_url}}"

    hit_req = False
    try:
        requests.get(blocked_url)
    except requests.ConnectionError as e:
        hit_req = "Blocked" in str(e)
    assert hit_req, f"requests did not block {{blocked_url}}"

# 6. subprocess MUST raise PermissionError when terminal tool is not enabled
sub_blocked = False
try:
    subprocess.run(["echo", "hi"], capture_output=True, text=True)
except PermissionError:
    sub_blocked = True
assert sub_blocked, "subprocess without terminal must raise PermissionError"

# C1 regression: secret/credential files cannot be read via open() or sqlite3 even inside workspace
assert os.path.exists(".env")
env_read_blocked = False
try:
    open(".env", "r").read()
except PermissionError:
    env_read_blocked = True
assert env_read_blocked, "reading .env must raise PermissionError"

ssh_read_blocked = False
try:
    open(".ssh/id_rsa", "r").read()
except PermissionError:
    ssh_read_blocked = True
assert ssh_read_blocked, "reading .ssh/id_rsa must raise PermissionError"

print("ALL_6_PART_A_OK")
"#,
                outside_db = outside_db.display().to_string(),
                outside_attach = outside_attach.display().to_string(),
                outside_vacuum = outside_vacuum.display().to_string()
            );
            let payload = run_test_cell(&mut kernel, &code, dir.path(), deadline).await;
            assert_eq!(payload["status"], "ok", "{payload}");
            assert!(
                payload["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("ALL_6_PART_A_OK"),
                "{payload}"
            );
            assert!(!outside_db.exists());
            assert!(!outside_attach.exists());
            assert!(!outside_vacuum.exists());
            kernel.shutdown();
        }

        // Part B: kernel WITH terminal — subprocess routes through terminal + handles approval denial & check=True
        {
            let mut kernel =
                EmbeddedKernel::spawn(&["terminal"], Vec::new()).expect("spawn kernel");
            let code = r#"
import subprocess

cp = subprocess.run(["echo", "hello world"], capture_output=True, text=True, check=True)
assert cp.returncode == 0
assert "ran:" in cp.stdout and "hello world" in cp.stdout, cp.stdout

out = subprocess.check_output(["echo", "ok"], text=True)
assert "ok" in out

failed = False
try:
    subprocess.run("FAIL_ME", shell=True, check=True, capture_output=True, text=True)
except subprocess.CalledProcessError as e:
    failed = (e.returncode == 2)
assert failed

denied = False
try:
    subprocess.run("DENY_ME", shell=True)
except PermissionError:
    denied = True
assert denied

# H2 regression: unsupported input/stdin/env arguments raise NotImplementedError
for bad_kw in ({"input": "abc"}, {"stdin": subprocess.PIPE}, {"env": {"FOO": "1"}}):
    hit_ni = False
    try:
        subprocess.run(["echo", "hi"], **bad_kw)
    except NotImplementedError:
        hit_ni = True
    assert hit_ni, f"expected NotImplementedError for {bad_kw}"

print("ALL_6_PART_B_OK")
"#;
            let payload = run_test_cell(&mut kernel, code, dir.path(), deadline).await;
            assert_eq!(payload["status"], "ok", "{payload}");
            assert!(
                payload["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("ALL_6_PART_B_OK"),
                "{payload}"
            );
            kernel.shutdown();
        }
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
