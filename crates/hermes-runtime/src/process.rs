//! Background processes started by `terminal(background=true)` and the `process_manage` tool.
//!
//! Mirrors `tools/process_registry.py` for local pipe-mode and PTY processes:
//! - Rolling 200k-char output buffer per process (pipe or real POSIX PTY via `posix_openpt`)
//! - Interactive `write`, `submit`, and `close` stdin actions for PTY processes
//! - `notify_on_complete` completion queue and `watch_patterns` substring watcher with rate-limit
//!   strikes & lifetime-hit promotion to `notify_on_complete`
//! - Clean group termination on `kill` and CLI exit (`kill_all`)

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::redact::{redact_terminal_output, redact_with, Mode};
use crate::session_search::{host_zone, strip_ansi};

pub(crate) const PROCESS_MANAGE: &str = "process_manage";

/// The reference's rolling buffer, completion tail, retention and cap.
const MAX_OUTPUT_CHARS: usize = 200_000;
const COMPLETION_OUTPUT_CHARS: usize = 2000;
const FINISHED_TTL_SECONDS: f64 = 1800.0;
const MAX_PROCESSES: usize = 64;
const MIN_PREFIX_CHARS: usize = 4;
const TERM_GRACE: Duration = Duration::from_secs(2);
/// A grandchild that keeps the pipes open must not keep a finished process "running".
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(600);

const WATCH_MIN_INTERVAL_SECONDS: f64 = 30.0;
const WATCH_STRIKE_LIMIT: usize = 3;
const WATCH_LIFETIME_MAX_HITS: usize = 20;

const NON_INTERACTIVE: &str =
    "Pass input non-interactively instead (arguments, a here-doc, or `printf ... | cmd`).";
const NO_PTY_STARTED_REASON: &str = "terminal background=true without pty=true";

pub(crate) const SILENT_HINT: &str = "background=true without notify_on_complete=true means this process runs SILENTLY — you will not be told when it exits. If this is a bounded task (test suite, build, CI poller, deploy, anything with a defined end), you almost certainly wanted notify_on_complete=true so the system pings you on exit. Re-launch with notify_on_complete=true, or call process_manage(action='poll') / process_manage(action='wait') yourself to learn the outcome. Only ignore this hint for genuine long-lived processes that never exit (servers, watchers, daemons).";

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/process_manage.json"))
            .unwrap_or_default()
    })
}

#[derive(Clone, Default)]
pub(crate) struct SpawnOptions {
    pub pty: bool,
    pub notify_on_complete: bool,
    pub watch_patterns: Vec<String>,
}

struct ProcessNotification {
    owner: String,
    session_id: String,
    kind: &'static str,
    text: String,
}

static NOTIFICATIONS: LazyLock<Mutex<Vec<ProcessNotification>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Drain and clear pending background process notifications for `owner` (or all if `owner` is empty).
pub fn drain_notifications(owner: &str) -> Vec<String> {
    let mut queue = NOTIFICATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut drained = Vec::new();
    let mut remaining = Vec::with_capacity(queue.len());
    for item in queue.drain(..) {
        if owner.is_empty() || item.owner == owner {
            drained.push(item.text);
        } else {
            remaining.push(item);
        }
    }
    *queue = remaining;
    drained
}

fn push_notification(owner: &str, session_id: &str, kind: &'static str, text: String) {
    let mut queue = NOTIFICATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    queue.push(ProcessNotification {
        owner: owner.to_owned(),
        session_id: session_id.to_owned(),
        kind,
        text,
    });
}

fn clear_completion_notification(session_id: &str) {
    let mut queue = NOTIFICATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    queue.retain(|item| !(item.session_id == session_id && item.kind == "completion"));
}

struct State {
    output: String,
    chars: usize,
    total: usize,
    exited: bool,
    exit_code: Option<i32>,
    reason: &'static str,
    source: &'static str,
    exited_at: f64,
    notify_on_complete: bool,
    watch_patterns: Vec<String>,
    watch_disabled: bool,
    watch_hits: usize,
    watch_cooldown_until: f64,
    watch_strike_candidate: bool,
    watch_consecutive_strikes: usize,
    watch_suppressed: usize,
    completion_acknowledged: bool,
}

struct Session {
    id: String,
    command: String,
    cwd: String,
    pid: i32,
    owner: String,
    pty: bool,
    started: f64,
    started_at: Instant,
    stdin: Mutex<Option<File>>,
    state: Mutex<State>,
    changed: Notify,
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn append(&self, text: &str) {
        let mut pending: Vec<(&'static str, String)> = Vec::new();
        {
            let mut state = self.lock();
            if state.exited {
                return;
            }
            state.output.push_str(text);
            let added = text.chars().count();
            state.chars += added;
            state.total += added;
            if state.chars > MAX_OUTPUT_CHARS {
                let drop = state.chars - MAX_OUTPUT_CHARS;
                let cut = state
                    .output
                    .char_indices()
                    .nth(drop)
                    .map_or(state.output.len(), |(index, _)| index);
                state.output.drain(..cut);
                state.chars = MAX_OUTPUT_CHARS;
            }

            if !state.watch_disabled && !state.watch_patterns.is_empty() {
                let hits: Vec<(String, String)> = split_lines(text)
                    .into_iter()
                    .filter_map(|line| {
                        state
                            .watch_patterns
                            .iter()
                            .find(|pat| line.contains(pat.as_str()))
                            .map(|pat| (pat.clone(), line.trim_end().to_owned()))
                    })
                    .collect();

                if !hits.is_empty() {
                    let now = unix_now();
                    if state.watch_cooldown_until > 0.0 && now < state.watch_cooldown_until {
                        state.watch_suppressed += hits.len();
                        if !state.watch_strike_candidate {
                            state.watch_strike_candidate = true;
                            state.watch_consecutive_strikes += 1;
                            if state.watch_consecutive_strikes >= WATCH_STRIKE_LIMIT {
                                state.watch_disabled = true;
                                state.notify_on_complete = true;
                                let suppressed = state.watch_suppressed;
                                pending.push((
                                    "watch_disabled",
                                    format!(
                                        "[IMPORTANT: Watch patterns disabled for process {} (suppressed {suppressed} match(es)) — {WATCH_STRIKE_LIMIT} consecutive rate-limit windows triggered (min spacing {WATCH_MIN_INTERVAL_SECONDS:.0}s). Falling back to notify_on_complete semantics; you'll get one notification when the process exits.]",
                                        self.id
                                    ),
                                ));
                            }
                        }
                    } else {
                        if state.watch_cooldown_until > 0.0 && !state.watch_strike_candidate {
                            state.watch_consecutive_strikes = 0;
                        }
                        state.watch_strike_candidate = false;
                        state.watch_cooldown_until = now + WATCH_MIN_INTERVAL_SECONDS;
                        state.watch_hits += 1;
                        let suppressed = std::mem::take(&mut state.watch_suppressed);
                        let matched_pattern = hits[0].0.clone();
                        let mut lines: Vec<String> =
                            hits.into_iter().take(20).map(|(_, line)| line).collect();
                        let mut out = lines.join("\n");
                        lines.clear();
                        if out.chars().count() > 2000 {
                            out = out.chars().take(2000).collect::<String>() + "\n...(truncated)";
                        }
                        let clean_out =
                            redact_terminal_output(&strip_ansi(&out), Some(&self.command), &[]);
                        let clean_cmd = redact_with(
                            &self.command,
                            Mode {
                                code_file: true,
                                ..Mode::default()
                            },
                        );
                        let sup_note = if suppressed > 0 {
                            format!(" ({suppressed} suppressed during cooldown)")
                        } else {
                            String::new()
                        };
                        pending.push((
                            "watch_match",
                            format!(
                                "[IMPORTANT: Background process {} ({clean_cmd}) matched watch pattern '{matched_pattern}'{sup_note}:]\n{clean_out}",
                                self.id
                            ),
                        ));
                        if state.watch_hits >= WATCH_LIFETIME_MAX_HITS {
                            state.watch_disabled = true;
                            state.notify_on_complete = true;
                            pending.push((
                                "watch_disabled",
                                format!(
                                    "[IMPORTANT: Watch patterns disabled for process {} — reached the lifetime cap of {WATCH_LIFETIME_MAX_HITS} delivered matches. Falling back to notify_on_complete semantics; you'll get one notification when the process exits.]",
                                    self.id
                                ),
                            ));
                        }
                    }
                }
            }
        }
        for (kind, notice) in pending {
            push_notification(&self.owner, &self.id, kind, notice);
        }
    }

    /// Record an exit unless one is already recorded.
    fn mark_exited(&self, code: Option<i32>, reason: &'static str, source: &'static str) -> bool {
        let maybe_notice = {
            let mut state = self.lock();
            if state.exited {
                return false;
            }
            state.exited = true;
            state.exit_code = code;
            state.reason = reason;
            state.source = source;
            state.exited_at = unix_now();
            if state.notify_on_complete && !state.completion_acknowledged {
                let tail = redact_terminal_output(
                    &strip_ansi(tail_chars(&state.output, COMPLETION_OUTPUT_CHARS)),
                    Some(&self.command),
                    &[],
                );
                let cmd = redact_with(
                    &self.command,
                    Mode {
                        code_file: true,
                        ..Mode::default()
                    },
                );
                let code_text = code.map_or_else(|| "unknown".to_owned(), |c| c.to_string());
                Some(format!(
                    "[IMPORTANT: Background process {} ({cmd}) exited with status '{reason}' (exit code {code_text}). Output tail:]\n{tail}",
                    self.id
                ))
            } else {
                None
            }
        };
        if let Some(notice) = maybe_notice {
            push_notification(&self.owner, &self.id, "completion", notice);
        }
        self.changed.notify_waiters();
        true
    }
}

static SESSIONS: LazyLock<Mutex<Vec<Arc<Session>>>> = LazyLock::new(|| Mutex::new(Vec::new()));
#[cfg(not(target_os = "linux"))]
static PTSNAME_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn sessions() -> MutexGuard<'static, Vec<Arc<Session>>> {
    SESSIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// Python's `int(time.time() - started)`.
fn uptime(session: &Session) -> u64 {
    session.started_at.elapsed().as_secs()
}

fn tail_chars(text: &str, count: usize) -> &str {
    let total = text.chars().count();
    if total <= count {
        return text;
    }
    let start = text
        .char_indices()
        .nth(total - count)
        .map_or(text.len(), |(index, _)| index);
    &text[start..]
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
}

/// Drop expired finished sessions, then the oldest finished one when at the cap.
fn prune(list: &mut Vec<Arc<Session>>) {
    let now = unix_now();
    list.retain(|session| {
        let finished = session.lock().exited;
        !(finished && now - session.started > FINISHED_TTL_SECONDS)
    });
    if list.len() >= MAX_PROCESSES {
        let oldest = list
            .iter()
            .enumerate()
            .filter(|(_, session)| session.lock().exited)
            .min_by(|(_, a), (_, b)| a.started.total_cmp(&b.started))
            .map(|(index, _)| index);
        if let Some(index) = oldest {
            list.remove(index);
        }
    }
}

struct Utf8StreamDecoder {
    carry: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn new() -> Self {
        Self { carry: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8], session: &Session) {
        self.carry.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.carry) {
                Ok(text) => {
                    session.append(text);
                    self.carry.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        session.append(&String::from_utf8_lossy(&self.carry[..valid]));
                    }
                    match error.error_len() {
                        None => {
                            self.carry.drain(..valid);
                            break;
                        }
                        Some(bad) => {
                            session.append("\u{fffd}");
                            self.carry.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    fn finish(&mut self, session: &Session) {
        if !self.carry.is_empty() {
            session.append(&String::from_utf8_lossy(&self.carry));
            self.carry.clear();
        }
    }
}

/// Feed a pipe stream into the session's buffer, decoding UTF-8 across chunk boundaries.
async fn pump(session: Arc<Session>, mut reader: impl AsyncRead + Unpin) {
    let mut decoder = Utf8StreamDecoder::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        decoder.push(&chunk[..count], &session);
    }
    decoder.finish(&session);
}

fn pump_pty_blocking(session: Arc<Session>, mut reader: File) {
    let mut decoder = Utf8StreamDecoder::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => decoder.push(&chunk[..count], &session),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // EIO on Linux/macOS PTY master means the slave side has closed.
            Err(_) => break,
        }
    }
    decoder.finish(&session);
}

fn open_pty() -> Result<(OwnedFd, OwnedFd), String> {
    // SAFETY: Standard POSIX PTY allocation via `posix_openpt` + `grantpt` + `unlockpt` + `open`.
    unsafe {
        let master_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master_fd < 0 {
            return Err(format!(
                "posix_openpt failed: {}",
                io::Error::last_os_error()
            ));
        }
        let master = OwnedFd::from_raw_fd(master_fd);
        if libc::grantpt(master.as_raw_fd()) != 0 {
            return Err(format!("grantpt failed: {}", io::Error::last_os_error()));
        }
        if libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(format!("unlockpt failed: {}", io::Error::last_os_error()));
        }

        let slave_name = {
            #[cfg(target_os = "linux")]
            {
                let mut buf = [0 as libc::c_char; 256];
                if libc::ptsname_r(master.as_raw_fd(), buf.as_mut_ptr(), buf.len()) != 0 {
                    return Err(format!("ptsname_r failed: {}", io::Error::last_os_error()));
                }
                CStr::from_ptr(buf.as_ptr()).to_owned()
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _guard = PTSNAME_LOCK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let ptr = libc::ptsname(master.as_raw_fd());
                if ptr.is_null() {
                    return Err(format!("ptsname failed: {}", io::Error::last_os_error()));
                }
                CStr::from_ptr(ptr).to_owned()
            }
        };

        let slave_fd = libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if slave_fd < 0 {
            return Err(format!(
                "open slave pty failed: {}",
                io::Error::last_os_error()
            ));
        }
        let slave = OwnedFd::from_raw_fd(slave_fd);
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ as _, &ws);
        Ok((master, slave))
    }
}

/// Start `command` (already configured with its working directory and environment) as a tracked
/// background process.
pub(crate) fn spawn(
    mut command: Command,
    text: &str,
    cwd: &str,
    owner: &str,
    options: SpawnOptions,
) -> Result<Value, String> {
    let mut pipe_reader: Option<tokio::net::unix::pipe::Receiver> = None;
    let mut pty_reader: Option<File> = None;
    let mut stdin_writer: Option<File> = None;
    let mut using_pty = false;

    if options.pty {
        match open_pty() {
            Ok((master, slave)) => {
                let slave_out = slave
                    .try_clone()
                    .map_err(|error| format!("Failed to start PTY process: {error}"))?;
                let slave_err = slave
                    .try_clone()
                    .map_err(|error| format!("Failed to start PTY process: {error}"))?;
                let master_dup = master
                    .try_clone()
                    .map_err(|error| format!("Failed to start PTY process: {error}"))?;
                stdin_writer = Some(File::from(master_dup));
                pty_reader = Some(File::from(master));
                command
                    .stdin(Stdio::from(File::from(slave)))
                    .stdout(Stdio::from(File::from(slave_out)))
                    .stderr(Stdio::from(File::from(slave_err)))
                    .kill_on_drop(false);
                // SAFETY: `setsid` and `ioctl(0, TIOCSCTTY, 0)` are async-signal-safe; fd 0 is
                // already connected to the slave PTY in the child.
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        if libc::ioctl(0, libc::TIOCSCTTY as _, 0 as libc::c_int) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                using_pty = true;
            }
            Err(_) => {
                // Fall back to pipe mode cleanly if PTY allocation fails.
            }
        }
    }

    if !using_pty {
        // One pipe carries both streams, so output keeps the order the process wrote it in.
        let (reader, writer) = std::io::pipe()
            .map_err(|error| format!("Failed to start background process: {error}"))?;
        let errors = writer
            .try_clone()
            .map_err(|error| format!("Failed to start background process: {error}"))?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(errors))
            .process_group(0)
            .kill_on_drop(false);
        let receiver =
            tokio::net::unix::pipe::Receiver::from_file(File::from(OwnedFd::from(reader)))
                .map_err(|error| format!("Failed to start background process: {error}"))?;
        pipe_reader = Some(receiver);
    }

    let spawned = command.spawn();
    // The child holds the only slave/write ends now; end of output is its exit.
    drop(command);
    let mut child =
        spawned.map_err(|error| format!("Failed to start background process: {error}"))?;
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("Failed to start background process: missing process id")?;

    let notify_on_complete = options.notify_on_complete;
    let watch_patterns = options.watch_patterns;
    let session = Arc::new(Session {
        id: format!("proc_{}", random_hex()),
        command: text.to_owned(),
        cwd: cwd.to_owned(),
        pid,
        owner: owner.to_owned(),
        pty: using_pty,
        started: unix_now(),
        started_at: Instant::now(),
        stdin: Mutex::new(stdin_writer),
        state: Mutex::new(State {
            output: String::new(),
            chars: 0,
            total: 0,
            exited: false,
            exit_code: None,
            reason: "exited",
            source: "",
            exited_at: 0.0,
            notify_on_complete,
            watch_patterns: watch_patterns.clone(),
            watch_disabled: false,
            watch_hits: 0,
            watch_cooldown_until: 0.0,
            watch_strike_candidate: false,
            watch_consecutive_strikes: 0,
            watch_suppressed: 0,
            completion_acknowledged: false,
        }),
        changed: Notify::new(),
    });
    {
        let mut list = sessions();
        prune(&mut list);
        list.push(Arc::clone(&session));
    }

    let reader_task = if let Some(receiver) = pipe_reader {
        tokio::spawn(pump(Arc::clone(&session), receiver))
    } else if let Some(pty_file) = pty_reader {
        let s = Arc::clone(&session);
        tokio::task::spawn_blocking(move || pump_pty_blocking(s, pty_file))
    } else {
        unreachable!()
    };

    let waiting = Arc::clone(&session);
    tokio::spawn(async move {
        let status = child.wait().await;
        let deadline = tokio::time::Instant::now() + DRAIN_AFTER_EXIT;
        let mut reader = reader_task;
        if tokio::time::timeout_at(deadline, &mut reader)
            .await
            .is_err()
        {
            reader.abort();
        }
        waiting.mark_exited(status.ok().and_then(exit_code), "exited", "");
    });

    let mut result = Map::new();
    result.insert("output".into(), json!("Background process started"));
    result.insert("session_id".into(), json!(session.id));
    result.insert("pid".into(), json!(session.pid));
    result.insert("exit_code".into(), json!(0));
    result.insert("error".into(), Value::Null);
    if using_pty {
        result.insert("pty".into(), json!(true));
    }
    if notify_on_complete {
        result.insert("notify_on_complete".into(), json!(true));
    }
    if !watch_patterns.is_empty() {
        result.insert("watch_patterns".into(), json!(watch_patterns));
    }
    if !notify_on_complete && watch_patterns.is_empty() {
        result.insert("hint".into(), json!(SILENT_HINT));
    }
    Ok(Value::Object(result))
}

fn random_hex() -> String {
    let mut bytes = [0u8; 6];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes()[..6]);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn signal_group(session: &Session, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(session.pid) {
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

/// Kill every process that is still running (the CLI is ending).
pub fn kill_all() {
    let list: Vec<Arc<Session>> = sessions().clone();
    for session in list {
        if session.mark_exited(Some(-15), "killed", "kill_all") {
            signal_group(&session, rustix::process::Signal::KILL);
        }
    }
}

#[cfg(test)]
fn kill_sessions(ids: &[String]) {
    let list: Vec<Arc<Session>> = sessions().clone();
    for session in list {
        if ids.iter().any(|id| id == &session.id)
            && session.mark_exited(Some(-15), "killed", "test_cleanup")
        {
            signal_group(&session, rustix::process::Signal::KILL);
        }
    }
}

// ── Lookup ───────────────────────────────────────────────────────────────────────────────────────

fn find(session_id: &str) -> Option<Arc<Session>> {
    if session_id.is_empty() {
        return None;
    }
    let list = sessions();
    if let Some(exact) = list.iter().find(|session| session.id == session_id) {
        return Some(Arc::clone(exact));
    }
    let query = session_id.trim();
    let query = if query.starts_with("proc_") {
        query.to_owned()
    } else {
        format!("proc_{query}")
    };
    if query.len() - "proc_".len() < MIN_PREFIX_CHARS {
        return None;
    }
    let mut matches = list.iter().filter(|session| session.id.starts_with(&query));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Some(Arc::clone(only)),
        _ => None,
    }
}

/// A session that exists but belongs to another conversation is refused like a miss.
fn actionable(session_id: &str, caller: &str) -> Option<Arc<Session>> {
    let session = find(session_id)?;
    let allowed = caller.is_empty() || session.owner.is_empty() || caller == session.owner;
    allowed.then_some(session)
}

fn not_found(session_id: &str) -> Value {
    json!({"status": "not_found", "error": format!("No process with ID {session_id}")})
}

fn tool_error(message: &str) -> Value {
    json!({"error": message})
}

// ── Results ──────────────────────────────────────────────────────────────────────────────────────

fn status_head(session: &Session, exited: bool) -> Map<String, Value> {
    let mut head = Map::new();
    head.insert("session_id".into(), json!(session.id));
    head.insert("command".into(), json!(session.command));
    head.insert(
        "status".into(),
        json!(if exited { "exited" } else { "running" }),
    );
    head
}

fn exit_fields(state: &State) -> [(&'static str, Value); 3] {
    [
        ("exit_code", json!(state.exit_code)),
        ("completion_reason", json!(state.reason)),
        ("termination_source", json!(state.source)),
    ]
}

/// `output` sized to the completion tail, plus `output_cut` when trimmed.
fn completion_output(state: &State) -> Vec<(&'static str, Value)> {
    let cut = state.chars as i64 - COMPLETION_OUTPUT_CHARS as i64;
    let mut fields = vec![(
        "output",
        json!(strip_ansi(tail_chars(
            &state.output,
            COMPLETION_OUTPUT_CHARS
        ))),
    )];
    if cut > 0 {
        fields.push(("output_cut", json!(cut)));
    }
    fields
}

fn exit_snapshot(session: &Session, state: &State, status: &str) -> Value {
    let mut map = Map::new();
    map.insert("status".into(), json!(status));
    map.insert("command".into(), json!(session.command));
    for (key, value) in exit_fields(state) {
        map.insert(key.into(), value);
    }
    for (key, value) in completion_output(state) {
        map.insert(key.into(), value);
    }
    Value::Object(map)
}

fn poll(session: &Session, acknowledge: bool) -> Value {
    let mut state = session.lock();
    if state.exited && acknowledge {
        state.completion_acknowledged = true;
        clear_completion_notification(&session.id);
    }
    let mut result = status_head(session, state.exited);
    result.insert("pid".into(), json!(session.pid));
    result.insert("uptime_seconds".into(), json!(uptime(session)));
    result.insert(
        "output_preview".into(),
        json!(strip_ansi(tail_chars(&state.output, 1000))),
    );
    if state.exited {
        for (key, value) in exit_fields(&state) {
            result.insert(key.into(), value);
        }
    }
    Value::Object(result)
}

/// Python's `lines[offset:offset + limit]` over `splitlines()`.
fn read_log(session: &Session, offset: Option<i64>, limit: i64) -> Value {
    let state = session.lock();
    let full = strip_ansi(&state.output);
    let lines = split_lines(&full);
    let total = lines.len() as i64;
    let selected: Vec<&str> = if offset.is_none() && limit > 0 {
        let start = (total - limit).max(0);
        lines[start as usize..].to_vec()
    } else {
        let resolve = |index: i64| {
            if index < 0 {
                (total + index).max(0)
            } else {
                index.min(total)
            }
        };
        let begin = resolve(offset.unwrap_or(0));
        let end = resolve(offset.unwrap_or(0).saturating_add(limit));
        if begin < end {
            lines[begin as usize..end as usize].to_vec()
        } else {
            Vec::new()
        }
    };
    let mut result = status_head(session, state.exited);
    result.insert("output".into(), json!(selected.join("\n")));
    result.insert("total_lines".into(), json!(total));
    result.insert("showing".into(), json!(format!("{} lines", selected.len())));
    Value::Object(result)
}

/// Python's `str.splitlines()`: every Unicode line boundary, no empty tail.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            lines.push(&text[start..index]);
            let mut next = index + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, after)| *after == '\n') {
                chars.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn max_wait_timeout() -> i64 {
    std::env::var("TERMINAL_TIMEOUT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(180)
}

async fn wait(session: &Session, requested: Option<&Value>) -> Value {
    let max_timeout = max_wait_timeout();
    let requested_seconds = match requested {
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    };
    if let Some(Value::Number(number)) = requested {
        let seconds = number.as_f64().unwrap_or(0.0);
        if seconds <= 0.0 {
            let shown = number
                .as_i64()
                .map_or_else(|| seconds.to_string(), |whole| whole.to_string());
            return json!({"status": "error", "error": format!("timeout must be positive (got {shown})")});
        }
    }
    let mut effective = requested_seconds.unwrap_or(max_timeout as f64);
    let mut clamp_note = None;
    if requested_seconds.is_some_and(|seconds| seconds > max_timeout as f64) {
        effective = max_timeout as f64;
        clamp_note = Some(format!(
            "Requested wait of {}s was clamped to configured limit of {max_timeout}s",
            requested_seconds.map_or(0, |seconds| seconds as i64)
        ));
    }
    let deadline = Instant::now() + Duration::from_secs_f64(effective.max(0.0));
    loop {
        let notified = session.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let mut state = session.lock();
            if state.exited {
                state.completion_acknowledged = true;
                clear_completion_notification(&session.id);
                let mut result = exit_snapshot(session, &state, "exited");
                if let Some(note) = &clamp_note {
                    result["timeout_note"] = json!(note);
                }
                return result;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let _ = tokio::time::timeout(remaining.min(Duration::from_secs(1)), notified).await;
    }
    let (preview, notify_on_complete) = {
        let state = session.lock();
        (
            strip_ansi(tail_chars(&state.output, 1000)),
            state.notify_on_complete,
        )
    };
    let effective_text = if effective.fract() == 0.0 {
        format!("{}", effective as i64)
    } else {
        effective.to_string()
    };
    let mut note = format!(
        "Wait window of {effective_text}s elapsed — the process is still running. This is not an error."
    );
    note.push_str(&format!(" Uptime: {}s.", uptime(session)));
    if notify_on_complete {
        note.push_str(
            " notify_on_complete is set: you will be notified on exit — do more work instead of waiting again.",
        );
    } else {
        note.push_str(
            " Poll again later or use terminal(background=true, notify_on_complete=true) next time for automatic notification.",
        );
    }
    json!({
        "status": "timeout",
        "command": session.command,
        "output": preview,
        "process_running": true,
        "timeout_note": match clamp_note {
            Some(prefix) => format!("{prefix}. {note}"),
            None => note,
        },
    })
}

async fn kill(session: &Session) -> Value {
    {
        let state = session.lock();
        if state.exited {
            return exit_snapshot(session, &state, "already_exited");
        }
    }
    signal_group(session, rustix::process::Signal::TERM);
    wait_for_exit(session, TERM_GRACE).await;
    if !session.lock().exited {
        signal_group(session, rustix::process::Signal::KILL);
        wait_for_exit(session, TERM_GRACE).await;
    }
    let output = {
        let mut state = session.lock();
        let output = completion_output(&state);
        state.exited = true;
        state.exit_code = Some(-15);
        state.reason = "killed";
        state.source = "process.kill";
        if state.exited_at == 0.0 {
            state.exited_at = unix_now();
        }
        output
    };
    session.changed.notify_waiters();
    let mut result = Map::new();
    result.insert("status".into(), json!("killed"));
    result.insert("session_id".into(), json!(session.id));
    result.insert("completion_reason".into(), json!("killed"));
    result.insert("termination_source".into(), json!("process.kill"));
    for (key, value) in output {
        result.insert(key.into(), value);
    }
    Value::Object(result)
}

async fn wait_for_exit(session: &Session, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        let notified = session.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if session.lock().exited {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return;
        }
    }
}

fn write_stdin(session: &Session, data: &str) -> Value {
    if session.lock().exited {
        return already_exited_error(session);
    }
    let mut guard = session
        .stdin
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(file) = guard.as_mut() else {
        return no_stdin_error(session);
    };
    if let Err(err) = file.write_all(data.as_bytes()).and_then(|()| file.flush()) {
        return json!({"status": "error", "error": format!("Failed to write stdin: {err}")});
    }
    json!({"status": "ok", "bytes_written": data.len()})
}

fn submit_stdin(session: &Session, data: &str) -> Value {
    let payload = if session.pty {
        format!("{data}\r")
    } else {
        format!("{data}\n")
    };
    write_stdin(session, &payload)
}

fn close_stdin(session: &Session) -> Value {
    if session.lock().exited {
        return already_exited_error(session);
    }
    let mut guard = session
        .stdin
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(mut file) = guard.take() else {
        return no_stdin_error(session);
    };
    if session.pty {
        let _ = file.write_all(b"\x04").and_then(|()| file.flush());
        drop(file);
        json!({"status": "ok", "message": "EOF sent"})
    } else {
        drop(file);
        json!({"status": "ok", "message": "stdin closed"})
    }
}

fn already_exited_error(session: &Session) -> Value {
    if !session.pty {
        return json!({"status": "already_exited", "error": format!(
            "Process has already finished. It ran without stdin ({NO_PTY_STARTED_REASON}), so if it read stdin, it got end-of-file at once. {NON_INTERACTIVE}"
        )});
    }
    json!({"status": "already_exited", "error": "Process has already finished"})
}

fn no_stdin_error(session: &Session) -> Value {
    if !session.pty {
        return json!({"status": "error", "error": format!(
            "This process was started without stdin ({NO_PTY_STARTED_REASON}). Relaunch it with terminal(background=true, pty=true) to send input."
        )});
    }
    json!({"status": "error", "error": format!("Process stdin is already closed. {NON_INTERACTIVE}")})
}

// ── Redaction ────────────────────────────────────────────────────────────────────────────────────

/// Secrets are removed from output and command before the result reaches the model. Output is
/// redacted by what the command was (a read of `.env` masks every assignment; ordinary output
/// is treated as source), and the command itself as source.
fn redact_result(mut result: Value) -> Value {
    if let Value::Object(map) = &mut result {
        let command = map
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        for key in ["output", "output_preview"] {
            if let Some(Value::String(text)) = map.get_mut(key) {
                if !text.is_empty() {
                    *text = redact_terminal_output(text, Some(&command), &[]);
                }
            }
        }
        if let Some(Value::String(text)) = map.get_mut("command") {
            if !text.is_empty() {
                *text = redact_with(
                    text,
                    Mode {
                        code_file: true,
                        ..Mode::default()
                    },
                );
            }
        }
    }
    result
}

fn list(caller: &str) -> Value {
    let all: Vec<Arc<Session>> = {
        let mut list = sessions();
        prune(&mut list);
        list.clone()
    };
    let (mut finished, running): (Vec<_>, Vec<_>) =
        all.into_iter().partition(|session| session.lock().exited);
    finished.extend(running);
    let zone = host_zone();
    let entries: Vec<Value> = finished
        .iter()
        .filter(|session| caller.is_empty() || session.owner == caller)
        .map(|session| {
            let state = session.lock();
            let started = DateTime::<Utc>::from_timestamp(session.started as i64, 0)
                .map(|instant| {
                    instant
                        .with_timezone(&zone)
                        .format("%Y-%m-%dT%H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("session_id".into(), json!(session.id));
            entry.insert(
                "command".into(),
                json!(session.command.chars().take(200).collect::<String>()),
            );
            entry.insert("cwd".into(), json!(session.cwd));
            entry.insert("pid".into(), json!(session.pid));
            entry.insert("owner_task_id".into(), json!(session.owner));
            entry.insert("started_at".into(), json!(started));
            entry.insert("uptime_seconds".into(), json!(uptime(session)));
            entry.insert(
                "status".into(),
                json!(if state.exited { "exited" } else { "running" }),
            );
            entry.insert(
                "output_preview".into(),
                json!(tail_chars(&state.output, 200)),
            );
            if session.pty {
                entry.insert("pty".into(), json!(true));
            }
            if state.notify_on_complete {
                entry.insert("notify_on_complete".into(), json!(true));
            }
            if !state.watch_patterns.is_empty() && !state.watch_disabled {
                entry.insert("watch_patterns".into(), json!(state.watch_patterns));
                entry.insert("watch_hit".into(), json!(state.watch_hits > 0));
            }
            if state.exited {
                entry.insert("exit_code".into(), json!(state.exit_code));
                entry.insert("exited_at".into(), json!(state.exited_at));
                entry.insert("completion_reason".into(), json!(state.reason));
            }
            redact_result(Value::Object(entry))
        })
        .collect();
    json!({"processes": entries})
}

/// Run `process_manage` for the conversation `caller`.
pub(crate) async fn call(caller: &str, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let session_id = match args.get("session_id") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    };
    match action {
        "list" => list(caller),
        "handoff" => {
            if session_id.is_empty() {
                return tool_error("session_id is required for handoff");
            }
            tool_error("handoff is only available to a running subagent with a live parent; you are not one.")
        }
        "poll" | "log" | "wait" | "kill" | "write" | "submit" | "close" => {
            if session_id.is_empty() {
                return tool_error(&format!("session_id is required for {action}"));
            }
            let Some(session) = actionable(&session_id, caller) else {
                return not_found(&session_id);
            };
            match action {
                "poll" => {
                    let ack = args
                        .get("acknowledge")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    redact_result(poll(&session, ack))
                }
                "log" => {
                    let offset = args.get("offset").and_then(Value::as_i64);
                    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(200);
                    redact_result(read_log(&session, offset, limit))
                }
                "wait" => redact_result(wait(&session, args.get("timeout")).await),
                "kill" => redact_result(kill(&session).await),
                "write" => {
                    let data = args.get("data").and_then(Value::as_str).unwrap_or("");
                    write_stdin(&session, data)
                }
                "submit" => {
                    let data = args.get("data").and_then(Value::as_str).unwrap_or("");
                    submit_stdin(&session, data)
                }
                "close" => close_stdin(&session),
                _ => unreachable!(),
            }
        }
        other => tool_error(&format!(
            "Unknown process action: {other}. Use: list, poll, log, wait, kill, write, submit, close, handoff"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(value: &Value, ids: &[String]) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| {
                        let item = match key.as_str() {
                            "pid" | "uptime_seconds" => json!(0),
                            "started_at" => json!("<t>"),
                            "exited_at" => json!(0.0),
                            "cwd" => json!("<cwd>"),
                            _ => normalize(item, ids),
                        };
                        (key.clone(), item)
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|i| normalize(i, ids)).collect()),
            Value::String(text) => {
                let mut text = text.clone();
                for (index, id) in ids.iter().enumerate() {
                    text = text.replace(id, &format!("<S{index}>"));
                }
                let short = regex::Regex::new(r"No process with ID [0-9a-f]{3}$").unwrap();
                let text = short.replace(&text, "No process with ID <P3>").into_owned();
                let uptime = regex::Regex::new(r"Uptime: \d+s").unwrap();
                let text = uptime.replace_all(&text, "Uptime: <n>s").into_owned();
                let text = text.replace(
                    NO_PTY_STARTED_REASON,
                    "the PTY library (ptyprocess/pywinpty) is not installed",
                );
                let text = text.replace(
                    "It was started without stdin (the PTY library (ptyprocess/pywinpty) is not installed), so if it read stdin, it got end-of-file at once.",
                    "It ran without stdin (the PTY library (ptyprocess/pywinpty) is not installed), so if it read stdin, it got end-of-file at once.",
                );
                let text = text.replace(
                    "This process was started without stdin (the PTY library (ptyprocess/pywinpty) is not installed). Relaunch it with terminal(background=true, pty=true) to send input.",
                    &format!("This process has no stdin: the PTY library (ptyprocess/pywinpty) is not installed. Relaunching with pty=true would not help. {NON_INTERACTIVE}"),
                );
                Value::String(text)
            }
            other => other.clone(),
        }
    }

    /// `$0` is the first spawned id, `$0~4` its first four hex digits, `$0~p6` the same with the
    /// `proc_` prefix.
    fn substitute(value: &Value, ids: &[String]) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), substitute(item, ids)))
                    .collect(),
            ),
            Value::String(text) => {
                let pattern = regex::Regex::new(r"^\$(\d+)(?:~(p?)(\d+))?$").unwrap();
                let Some(found) = pattern.captures(text) else {
                    return value.clone();
                };
                let id = &ids[found[1].parse::<usize>().unwrap()];
                match found.get(3) {
                    None => json!(id),
                    Some(count) => {
                        let hex: String = id["proc_".len()..]
                            .chars()
                            .take(count.as_str().parse().unwrap())
                            .collect();
                        json!(format!(
                            "{}{hex}",
                            if &found[2] == "p" { "proc_" } else { "" }
                        ))
                    }
                }
            }
            other => other.clone(),
        }
    }

    #[tokio::test]
    async fn results_match_the_reference() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/process_cases.json")).unwrap();
        assert!(cases.len() >= 9);
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().to_str().unwrap();
        let mut spawned_ids: Vec<String> = Vec::new();
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let owner = case["owner"].as_str().unwrap();
            let mut ids: Vec<String> = Vec::new();
            for step in case["steps"].as_array().unwrap() {
                if let Some(text) = step["spawn"].as_str() {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", text]).current_dir(cwd);
                    let started =
                        spawn(command, text, cwd, owner, SpawnOptions::default()).unwrap();
                    let sid = started["session_id"].as_str().unwrap().to_owned();
                    spawned_ids.push(sid.clone());
                    ids.push(sid);
                    continue;
                }
                let caller = match step["caller"].as_str() {
                    Some(suffix) => format!("{owner}-{suffix}"),
                    None => owner.to_owned(),
                };
                let arguments = substitute(&step["call"], &ids).to_string();
                let result = normalize(&call(&caller, &arguments).await, &ids);
                let expected: Value = step["result"].clone();
                assert_eq!(result, expected, "{name}: {}", step["call"]);
            }
        }
        kill_sessions(&spawned_ids);
    }

    #[tokio::test]
    async fn output_is_kept_in_order_across_both_streams() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "for i in 1 2 3 4 5 6 7 8; do echo out$i; echo err$i >&2; done",
        ]);
        let started = spawn(command, "order", "/", "ordering", SpawnOptions::default()).unwrap();
        let id = started["session_id"].as_str().unwrap().to_owned();
        let waited = call(
            "ordering",
            &json!({"action": "wait", "session_id": id}).to_string(),
        )
        .await;
        let expected: String = (1..=8).map(|i| format!("out{i}\nerr{i}\n")).collect();
        assert_eq!(waited["output"], json!(expected));
    }

    #[tokio::test]
    async fn pty_supports_write_submit_close_and_notifications_and_watch_patterns() {
        // 1. Real Unix PTY interactive session (`read line; printf "got:%s\n" "$line"; cat`)
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "read line; printf 'got:%s\\n' \"$line\"; cat"]);
        let started = spawn(
            command,
            "pty-echo",
            "/",
            "pty-owner",
            SpawnOptions {
                pty: true,
                notify_on_complete: true,
                watch_patterns: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(started["pty"], json!(true));
        let id = started["session_id"].as_str().unwrap().to_owned();

        let sub = call(
            "pty-owner",
            &json!({"action": "submit", "session_id": id, "data": "hello"}).to_string(),
        )
        .await;
        assert_eq!(sub["status"], "ok", "{sub}");

        let wr = call(
            "pty-owner",
            &json!({"action": "write", "session_id": id, "data": "world\n"}).to_string(),
        )
        .await;
        assert_eq!(wr["status"], "ok", "{wr}");

        let cls = call(
            "pty-owner",
            &json!({"action": "close", "session_id": id}).to_string(),
        )
        .await;
        assert_eq!(cls["status"], "ok", "{cls}");

        let waited = call(
            "pty-owner",
            &json!({"action": "wait", "session_id": id, "timeout": 5}).to_string(),
        )
        .await;
        let out = waited["output"].as_str().unwrap_or("");
        assert!(
            out.contains("got:hello"),
            "missing PTY submit output: {out:?}"
        );
        assert!(out.contains("world"), "missing PTY write output: {out:?}");
        // `wait` acknowledged completion, so completion notice was suppressed.
        assert!(drain_notifications("pty-owner").is_empty());

        // 2. notify_on_complete queues notification on natural exit (and poll(acknowledge=true) clears it).
        let mut cmd2 = Command::new("/bin/sh");
        cmd2.args(["-c", "echo done-ok"]);
        let s2 = spawn(
            cmd2,
            "notify-job",
            "/",
            "notify-owner",
            SpawnOptions {
                pty: false,
                notify_on_complete: true,
                watch_patterns: Vec::new(),
            },
        )
        .unwrap();
        let id2 = s2["session_id"].as_str().unwrap().to_owned();
        // Wait for background reaper to mark exited.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if find(&id2).is_some_and(|s| s.lock().exited) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let notices = drain_notifications("notify-owner");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("done-ok"), "{notices:?}");

        // 3. watch_patterns matches and falls back to notify_on_complete after 3 strikes.
        let mut cmd3 = Command::new("/bin/sh");
        cmd3.args(["-c", "echo READY:first; echo READY:second"]);
        let s3 = spawn(
            cmd3,
            "watch-job",
            "/",
            "watch-owner",
            SpawnOptions {
                pty: false,
                notify_on_complete: false,
                watch_patterns: vec!["READY:".into()],
            },
        )
        .unwrap();
        let id3 = s3["session_id"].as_str().unwrap().to_owned();
        let _ = call(
            "watch-owner",
            &json!({"action": "wait", "session_id": id3, "timeout": 5}).to_string(),
        )
        .await;
        let w_notices = drain_notifications("watch-owner");
        assert!(!w_notices.is_empty(), "expected watch_match notice");
        assert!(w_notices[0].contains("matched watch pattern 'READY:'"));

        // Exercise rate-limit strikes -> watch_disabled promotion to notify_on_complete.
        let sess = Session {
            id: "proc_strike".into(),
            command: "noisy".into(),
            cwd: "/".into(),
            pid: 0,
            owner: "strike-owner".into(),
            pty: false,
            started: unix_now(),
            started_at: Instant::now(),
            stdin: Mutex::new(None),
            state: Mutex::new(State {
                output: String::new(),
                chars: 0,
                total: 0,
                exited: false,
                exit_code: None,
                reason: "exited",
                source: "",
                exited_at: 0.0,
                notify_on_complete: false,
                watch_patterns: vec!["HIT".into()],
                watch_disabled: false,
                watch_hits: 0,
                watch_cooldown_until: 0.0,
                watch_strike_candidate: false,
                watch_consecutive_strikes: 0,
                watch_suppressed: 0,
                completion_acknowledged: false,
            }),
            changed: Notify::new(),
        };
        sess.append("HIT 0\n"); // immediate match, starts 30s cooldown
        for strike in 1..=WATCH_STRIKE_LIMIT {
            sess.append(&format!("HIT {strike}\n"));
            sess.lock().watch_strike_candidate = false;
        }
        sess.lock().watch_strike_candidate = false;
        sess.append("HIT final\n");
        let st = sess.lock();
        assert!(st.watch_disabled, "expected watch_disabled after strikes");
        assert!(
            st.notify_on_complete,
            "expected notify_on_complete promotion"
        );
        drop(st);
        let strike_notices = drain_notifications("strike-owner");
        assert!(strike_notices
            .iter()
            .any(|n| n.contains("Watch patterns disabled")));
    }

    #[test]
    fn the_buffer_keeps_the_tail_by_characters() {
        let session = Session {
            id: "proc_x".into(),
            command: String::new(),
            cwd: String::new(),
            pid: 0,
            owner: String::new(),
            pty: false,
            started: 0.0,
            started_at: Instant::now(),
            stdin: Mutex::new(None),
            state: Mutex::new(State {
                output: String::new(),
                chars: 0,
                total: 0,
                exited: false,
                exit_code: None,
                reason: "exited",
                source: "",
                exited_at: 0.0,
                notify_on_complete: false,
                watch_patterns: Vec::new(),
                watch_disabled: false,
                watch_hits: 0,
                watch_cooldown_until: 0.0,
                watch_strike_candidate: false,
                watch_consecutive_strikes: 0,
                watch_suppressed: 0,
                completion_acknowledged: false,
            }),
            changed: Notify::new(),
        };
        session.append(&"é".repeat(MAX_OUTPUT_CHARS));
        session.append("tail");
        let state = session.lock();
        assert_eq!(state.chars, MAX_OUTPUT_CHARS);
        assert_eq!(state.total, MAX_OUTPUT_CHARS + 4);
        assert!(state.output.ends_with("étail"));
        assert_eq!(state.output.chars().count(), MAX_OUTPUT_CHARS);
    }
}
