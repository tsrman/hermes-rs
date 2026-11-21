//! Background processes started by `terminal(background=true)` and the `process_manage` tool.
//!
//! Mirrors `tools/process_registry.py` for local pipe-mode processes: a rolling output buffer per
//! process, results for poll/log/wait/kill, and ownership by the conversation that started the
//! process. Not ported: pseudo-terminals (so no stdin), completion notifications, checkpoints that
//! outlive the CLI, and subagent handoff. Every process still running when the CLI ends is killed.

use std::process::ExitStatus;
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

const NON_INTERACTIVE: &str =
    "Pass input non-interactively instead (arguments, a here-doc, or `printf ... | cmd`).";
const NO_PTY_REASON: &str = "this build cannot give background processes a pseudo-terminal";

pub(crate) const SILENT_HINT: &str = "background=true without notify_on_complete=true means this process runs SILENTLY — you will not be told when it exits. If this is a bounded task (test suite, build, CI poller, deploy, anything with a defined end), you almost certainly wanted notify_on_complete=true so the system pings you on exit. Re-launch with notify_on_complete=true, or call process_manage(action='poll') / process_manage(action='wait') yourself to learn the outcome. Only ignore this hint for genuine long-lived processes that never exit (servers, watchers, daemons).";

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/process_manage.json"))
            .unwrap_or_default()
    })
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
}

struct Session {
    id: String,
    command: String,
    cwd: String,
    pid: i32,
    owner: String,
    started: f64,
    started_at: Instant,
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
    }

    /// Record an exit unless one is already recorded.
    fn mark_exited(&self, code: Option<i32>, reason: &'static str, source: &'static str) -> bool {
        let mut state = self.lock();
        if state.exited {
            return false;
        }
        state.exited = true;
        state.exit_code = code;
        state.reason = reason;
        state.source = source;
        state.exited_at = unix_now();
        drop(state);
        self.changed.notify_waiters();
        true
    }
}

static SESSIONS: LazyLock<Mutex<Vec<Arc<Session>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

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

/// Feed a stream into the session's buffer, decoding UTF-8 across chunk boundaries.
async fn pump(session: Arc<Session>, mut reader: impl AsyncRead + Unpin) {
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        carry.extend_from_slice(&chunk[..count]);
        loop {
            match std::str::from_utf8(&carry) {
                Ok(text) => {
                    session.append(text);
                    carry.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        session.append(&String::from_utf8_lossy(&carry[..valid]));
                    }
                    match error.error_len() {
                        // A split multibyte character: keep it for the next read.
                        None => {
                            carry.drain(..valid);
                            break;
                        }
                        Some(bad) => {
                            session.append("\u{fffd}");
                            carry.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }
    if !carry.is_empty() {
        session.append(&String::from_utf8_lossy(&carry));
    }
}

/// Start `command` (already configured with its working directory and environment) as a tracked
/// background process.
pub(crate) fn spawn(
    mut command: Command,
    text: &str,
    cwd: &str,
    owner: &str,
) -> Result<Value, String> {
    use std::process::Stdio;
    // One pipe carries both streams, so output keeps the order the process wrote it in.
    let (reader, writer) =
        std::io::pipe().map_err(|error| format!("Failed to start background process: {error}"))?;
    let errors = writer
        .try_clone()
        .map_err(|error| format!("Failed to start background process: {error}"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(errors))
        .process_group(0)
        .kill_on_drop(false);
    let spawned = command.spawn();
    // The child holds the only write ends now; end of output is its exit.
    drop(command);
    let mut child =
        spawned.map_err(|error| format!("Failed to start background process: {error}"))?;
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("Failed to start background process: missing process id")?;
    let receiver = tokio::net::unix::pipe::Receiver::from_file(std::fs::File::from(
        std::os::fd::OwnedFd::from(reader),
    ))
    .map_err(|error| format!("Failed to start background process: {error}"))?;
    let session = Arc::new(Session {
        id: format!("proc_{}", random_hex()),
        command: text.to_owned(),
        cwd: cwd.to_owned(),
        pid,
        owner: owner.to_owned(),
        started: unix_now(),
        started_at: Instant::now(),
        state: Mutex::new(State {
            output: String::new(),
            chars: 0,
            total: 0,
            exited: false,
            exit_code: None,
            reason: "exited",
            source: "",
            exited_at: 0.0,
        }),
        changed: Notify::new(),
    });
    {
        let mut list = sessions();
        prune(&mut list);
        list.push(Arc::clone(&session));
    }
    let readers = vec![tokio::spawn(pump(Arc::clone(&session), receiver))];
    let waiting = Arc::clone(&session);
    tokio::spawn(async move {
        let status = child.wait().await;
        let deadline = tokio::time::Instant::now() + DRAIN_AFTER_EXIT;
        for mut reader in readers {
            if tokio::time::timeout_at(deadline, &mut reader)
                .await
                .is_err()
            {
                reader.abort();
            }
        }
        waiting.mark_exited(status.ok().and_then(exit_code), "exited", "");
    });
    Ok(json!({
        "output": "Background process started",
        "session_id": session.id,
        "pid": session.pid,
        "exit_code": 0,
        "error": null,
        "hint": SILENT_HINT,
    }))
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

fn poll(session: &Session) -> Value {
    let state = session.lock();
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
            let state = session.lock();
            if state.exited {
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
    let preview = strip_ansi(tail_chars(&session.lock().output, 1000));
    let effective_text = if effective.fract() == 0.0 {
        format!("{}", effective as i64)
    } else {
        effective.to_string()
    };
    let mut note = format!(
        "Wait window of {effective_text}s elapsed — the process is still running. This is not an error."
    );
    note.push_str(&format!(" Uptime: {}s.", uptime(session)));
    note.push_str(
        " Poll again later or use terminal(background=true, notify_on_complete=true) next time for automatic notification.",
    );
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

/// A process started here has no stdin (pipe mode reads from /dev/null).
fn stdin_error(session: &Session) -> Value {
    if session.lock().exited {
        return json!({"status": "already_exited", "error": format!(
            "Process has already finished. It ran without stdin ({NO_PTY_REASON}), so if it read stdin, it got end-of-file at once. {NON_INTERACTIVE}"
        )});
    }
    json!({"status": "error", "error": format!(
        "This process has no stdin: {NO_PTY_REASON}. Relaunching with pty=true would not help. {NON_INTERACTIVE}"
    )})
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
                "poll" => redact_result(poll(&session)),
                "log" => {
                    let offset = args.get("offset").and_then(Value::as_i64);
                    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(200);
                    redact_result(read_log(&session, offset, limit))
                }
                "wait" => redact_result(wait(&session, args.get("timeout")).await),
                "kill" => redact_result(kill(&session).await),
                _ => stdin_error(&session),
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
                Value::String(uptime.replace_all(&text, "Uptime: <n>s").into_owned())
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
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let owner = case["owner"].as_str().unwrap();
            let mut ids: Vec<String> = Vec::new();
            for step in case["steps"].as_array().unwrap() {
                if let Some(text) = step["spawn"].as_str() {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", text]).current_dir(cwd);
                    let started = spawn(command, text, cwd, owner).unwrap();
                    ids.push(started["session_id"].as_str().unwrap().to_owned());
                    continue;
                }
                let caller = match step["caller"].as_str() {
                    Some(suffix) => format!("{owner}-{suffix}"),
                    None => owner.to_owned(),
                };
                let arguments = substitute(&step["call"], &ids).to_string();
                let result = normalize(&call(&caller, &arguments).await, &ids);
                // The reference environment lacks a PTY library; this build says its own reason.
                let expected: Value = serde_json::from_str(&step["result"].to_string().replace(
                    "the PTY library (ptyprocess/pywinpty) is not installed",
                    NO_PTY_REASON,
                ))
                .unwrap();
                assert_eq!(result, expected, "{name}: {}", step["call"]);
            }
        }
        kill_all();
    }

    #[tokio::test]
    async fn output_is_kept_in_order_across_both_streams() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "for i in 1 2 3 4 5 6 7 8; do echo out$i; echo err$i >&2; done",
        ]);
        let started = spawn(command, "order", "/", "ordering").unwrap();
        let id = started["session_id"].as_str().unwrap().to_owned();
        let waited = call(
            "ordering",
            &json!({"action": "wait", "session_id": id}).to_string(),
        )
        .await;
        let expected: String = (1..=8).map(|i| format!("out{i}\nerr{i}\n")).collect();
        assert_eq!(waited["output"], json!(expected));
    }

    #[test]
    fn the_buffer_keeps_the_tail_by_characters() {
        let session = Session {
            id: "proc_x".into(),
            command: String::new(),
            cwd: String::new(),
            pid: 0,
            owner: String::new(),
            started: 0.0,
            started_at: Instant::now(),
            state: Mutex::new(State {
                output: String::new(),
                chars: 0,
                total: 0,
                exited: false,
                exit_code: None,
                reason: "exited",
                source: "",
                exited_at: 0.0,
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
