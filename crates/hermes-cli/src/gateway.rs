//! `hsr gateway run`: local HTTP/SSE Runs API server compatible with Hermes's `platforms.api_server`
//! contract (used by AurumAI's `hermes-relay` and other programmatic orchestrators).
//!
//! Endpoints:
//! - `GET  /health`                  — unauthenticated liveness check (`200 {"status":"ok"}`)
//! - `POST /v1/runs`                 — queue & start an async run (`{input, session_id?, conversation_history?, instructions?}`)
//! - `GET  /v1/runs/{run_id}`        — current status, output, and token usage
//! - `GET  /v1/runs/{run_id}/events` — SSE stream with full replay from start + `: keepalive`
//! - `POST /v1/runs/{run_id}/stop`   — cooperatively stop an active run
//! - `POST /v1/runs/{run_id}/clarify`— answer a pending `clarify.request` (`409` if already settled)

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use clap::{Args, Subcommand};
use hermes_runtime::config::{load_chat_settings, ApiServerSettings};
use hermes_runtime::mcp::McpPool;
use hermes_runtime::turn::{
    ApprovalAction, ApprovalDecision, ApprovalRequest, TurnEvent, TurnHistory,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex as AsyncMutex, Notify};

use crate::chat::{self, GatewayTurnStatus};

const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_RETAINED_RUNS: usize = 128;
const SSE_KEEPALIVE_SECS: u64 = 15;

#[derive(Subcommand)]
pub enum Command {
    /// Start the local gateway HTTP Runs API (`platforms.api_server`).
    Run(RunArgs),
}

#[derive(Args, Default)]
pub struct RunArgs {
    /// Override bind host (defaults to `platforms.api_server.extra.host` or `127.0.0.1`).
    #[arg(long)]
    pub host: Option<String>,
    /// Override bind port (defaults to `platforms.api_server.extra.port` or `8642`).
    #[arg(long)]
    pub port: Option<u16>,
    /// Override Bearer API key (defaults to `platforms.api_server.extra.key`).
    #[arg(long, hide_env_values = true)]
    pub key: Option<String>,
}

#[derive(Clone)]
struct GatewayState {
    home: Option<PathBuf>,
    root: PathBuf,
    api_key: Option<String>,
    mcp_pool: Arc<McpPool>,
    runs: Arc<StdMutex<RunStore>>,
    sessions: Arc<StdMutex<HashMap<String, Arc<SessionEntry>>>>,
    counter: Arc<AtomicU64>,
}

struct RunStore {
    by_id: HashMap<String, Arc<RunEntry>>,
    order: VecDeque<String>,
}

impl RunStore {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, id: String, entry: Arc<RunEntry>) {
        self.by_id.insert(id.clone(), entry);
        self.order.push_back(id);
        while self.order.len() > MAX_RETAINED_RUNS {
            if let Some(oldest) = self.order.front() {
                let removable = self.by_id.get(oldest).is_some_and(|e| e.is_terminal());
                if removable {
                    if let Some(evicted) = self.order.pop_front() {
                        self.by_id.remove(&evicted);
                    }
                } else {
                    break;
                }
            }
        }
    }
}

struct SessionEntry {
    lock: AsyncMutex<()>,
    state: StdMutex<SessionTurnState>,
}

#[derive(Default)]
struct SessionTurnState {
    history: TurnHistory,
    db_session_id: Option<String>,
}

struct PendingClarify {
    prompt_id: String,
    generation: u64,
    tx: oneshot::Sender<String>,
}

struct RunInner {
    status: &'static str,
    output: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    events: Vec<Value>,
    pending_clarify: Option<PendingClarify>,
    clarify_generation: u64,
}

struct RunEntry {
    run_id: String,
    session_id: String,
    cancel: Arc<AtomicBool>,
    notify: Notify,
    inner: StdMutex<RunInner>,
}

impl RunEntry {
    fn new(run_id: String, session_id: String) -> Self {
        Self {
            run_id,
            session_id,
            cancel: Arc::new(AtomicBool::new(false)),
            notify: Notify::new(),
            inner: StdMutex::new(RunInner {
                status: "queued",
                output: None,
                input_tokens: 0,
                output_tokens: 0,
                events: Vec::new(),
                pending_clarify: None,
                clarify_generation: 0,
            }),
        }
    }

    fn is_terminal(&self) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        is_terminal_status(guard.status)
    }

    fn push_event(&self, event: Value) {
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.events.push(event);
        }
        self.notify.notify_waiters();
    }

    fn set_status(&self, status: &'static str) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !is_terminal_status(guard.status) {
            guard.status = status;
        }
        drop(guard);
        self.notify.notify_waiters();
    }

    fn finish(
        &self,
        status: &'static str,
        output: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
        terminal_event: Value,
    ) {
        let settled_prompt = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let settled = guard.pending_clarify.take().map(|p| p.prompt_id);
            if let Some(ref pid) = settled {
                guard.events.push(json!({
                    "event": "clarify.settled",
                    "run_id": self.run_id,
                    "prompt_id": pid,
                }));
            }
            guard.status = status;
            guard.output = output;
            guard.input_tokens = input_tokens;
            guard.output_tokens = output_tokens;
            guard.events.push(terminal_event);
            settled
        };
        let _ = settled_prompt;
        self.notify.notify_waiters();
    }

    fn snapshot_status(&self) -> Value {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "run_id": self.run_id,
            "session_id": self.session_id,
            "status": guard.status,
            "output": guard.output,
            "usage": {
                "input_tokens": guard.input_tokens,
                "output_tokens": guard.output_tokens,
            }
        })
    }

    fn request_stop(&self) -> &'static str {
        self.cancel.store(true, Ordering::SeqCst);
        let status = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pending) = guard.pending_clarify.take() {
                guard.events.push(json!({
                    "event": "clarify.settled",
                    "run_id": self.run_id,
                    "prompt_id": pending.prompt_id,
                }));
                drop(pending.tx);
            }
            if !is_terminal_status(guard.status) {
                guard.status = "stopping";
            }
            guard.status
        };
        self.notify.notify_waiters();
        status
    }

    fn submit_clarify(&self, request_id: &str, generation: u64, answer: String) -> bool {
        let tx = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let matches = guard
                .pending_clarify
                .as_ref()
                .is_some_and(|p| p.prompt_id == request_id && p.generation == generation);
            if !matches {
                return false;
            }
            let pending = guard.pending_clarify.take().unwrap();
            guard.events.push(json!({
                "event": "clarify.settled",
                "run_id": self.run_id,
                "prompt_id": pending.prompt_id,
            }));
            if guard.status == "waiting_for_approval" {
                guard.status = "running";
            }
            pending.tx
        };
        self.notify.notify_waiters();
        tx.send(answer).is_ok()
    }
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
}

fn is_terminal_event_name(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("run.completed" | "run.failed" | "run.cancelled" | "run.interrupted")
    )
}

pub async fn run(home: Option<&Path>, command: Command) -> Result<u8, String> {
    match command {
        Command::Run(args) => serve(home, args).await.map(|()| 0),
    }
}

pub async fn serve(home: Option<&Path>, args: RunArgs) -> Result<(), String> {
    let settings = load_chat_settings(home)?;
    let api: ApiServerSettings = settings.api_server.clone();
    let host = args.host.unwrap_or(api.host);
    let port = args.port.unwrap_or(api.port);
    let api_key = args.key.or(api.key);
    let root = std::env::current_dir().map_err(|e| format!("failed reading cwd: {e}"))?;

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("failed binding gateway API server on {addr}: {e}"))?;
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    eprintln!("Hermes gateway API server listening on http://{bound}");

    // Warm MCP servers in the background so `/health` answers promptly while connections initialize.
    let mcp_pool = Arc::new(McpPool::new());
    if !settings.mcp_servers.is_empty() {
        let pool = Arc::clone(&mcp_pool);
        let servers = settings.mcp_servers.clone();
        let home_owned = home.map(Path::to_path_buf);
        let limit = settings.mcp_result_size_chars;
        tokio::spawn(async move {
            let (_, warnings) = pool.acquire(&servers, home_owned, limit).await;
            for warning in warnings {
                eprintln!("MCP startup warning: {warning}");
            }
        });
    }

    serve_on_listener(
        listener,
        home.map(Path::to_path_buf),
        root,
        api_key,
        mcp_pool,
    )
    .await
}

pub(crate) async fn serve_on_listener(
    listener: TcpListener,
    home: Option<PathBuf>,
    root: PathBuf,
    api_key: Option<String>,
    mcp_pool: Arc<McpPool>,
) -> Result<(), String> {
    let state = GatewayState {
        home,
        root,
        api_key,
        mcp_pool: Arc::clone(&mcp_pool),
        runs: Arc::new(StdMutex::new(RunStore::new())),
        sessions: Arc::new(StdMutex::new(HashMap::new())),
        counter: Arc::new(AtomicU64::new(1)),
    };

    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);

    #[cfg(unix)]
    let outcome = loop {
        tokio::select! {
            res = &mut interrupt => break res.map_err(|e| e.to_string()),
            _ = terminate.recv() => break Ok(()),
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let st = state.clone();
                    tokio::spawn(async move {
                        let _ = handle_http_connection(stream, st).await;
                    });
                }
            }
        }
    };

    #[cfg(not(unix))]
    let outcome = loop {
        tokio::select! {
            res = &mut interrupt => break res.map_err(|e| e.to_string()),
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let st = state.clone();
                    tokio::spawn(async move {
                        let _ = handle_http_connection(stream, st).await;
                    });
                }
            }
        }
    };

    // Cancel any active runs and shut down pooled MCP connections cleanly.
    let active_runs: Vec<Arc<RunEntry>> = state
        .runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .by_id
        .values()
        .cloned()
        .collect();
    for run in active_runs {
        if !run.is_terminal() {
            run.request_stop();
        }
    }
    hermes_runtime::mcp::flush_cancellations().await;
    mcp_pool.shutdown().await;
    outcome
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_http_request(
    stream: &mut BufReader<TcpStream>,
) -> Result<Option<HttpRequest>, &'static str> {
    let mut request_line = String::new();
    let n = stream
        .read_line(&mut request_line)
        .await
        .map_err(|_| "read error")?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.split_whitespace();
    let method = parts.next().ok_or("bad request line")?.to_owned();
    let raw_target = parts.next().ok_or("bad request line")?;
    let path = raw_target
        .split('?')
        .next()
        .unwrap_or(raw_target)
        .to_owned();

    let mut headers = HashMap::new();
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let read = stream
            .read_line(&mut line)
            .await
            .map_err(|_| "read error")?;
        if read == 0 {
            return Err("unexpected eof in headers");
        }
        header_bytes += read;
        if header_bytes > MAX_HTTP_HEADER_BYTES {
            return Err("headers too large");
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }

    let mut body = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            let mut size_line = String::new();
            let read = stream
                .read_line(&mut size_line)
                .await
                .map_err(|_| "read error")?;
            if read == 0 {
                return Err("unexpected eof in chunked body");
            }
            let size_hex = size_line.trim().split(';').next().unwrap_or("");
            let chunk_len = usize::from_str_radix(size_hex, 16).map_err(|_| "bad chunk size")?;
            if chunk_len == 0 {
                // Consume trailing CRLF after last chunk.
                let mut trailer = String::new();
                let _ = stream.read_line(&mut trailer).await;
                break;
            }
            if body.len().saturating_add(chunk_len) > MAX_HTTP_BODY_BYTES {
                return Err("body too large");
            }
            let start = body.len();
            body.resize(start + chunk_len, 0);
            stream
                .read_exact(&mut body[start..])
                .await
                .map_err(|_| "read error")?;
            let mut crlf = [0u8; 2];
            stream
                .read_exact(&mut crlf)
                .await
                .map_err(|_| "read error")?;
        }
    } else if let Some(cl) = headers.get("content-length") {
        let len = cl.parse::<usize>().map_err(|_| "bad content-length")?;
        if len > MAX_HTTP_BODY_BYTES {
            return Err("body too large");
        }
        if len > 0 {
            body.resize(len, 0);
            stream
                .read_exact(&mut body)
                .await
                .map_err(|_| "read error")?;
        }
    }

    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

async fn write_json(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    payload: &Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

fn check_auth(req: &HttpRequest, expected_key: Option<&str>) -> bool {
    let Some(expected) = expected_key.filter(|k| !k.is_empty()) else {
        return true;
    };
    let Some(auth) = req.headers.get("authorization") else {
        return false;
    };
    let Some(token) = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    else {
        return false;
    };
    constant_time_eq(token.trim().as_bytes(), expected.as_bytes())
}

async fn handle_http_connection(stream: TcpStream, state: GatewayState) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let req = match read_http_request(&mut reader).await {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(msg) => {
            let mut raw = reader.into_inner();
            let code = if msg == "body too large" { 413 } else { 400 };
            return write_json(&mut raw, code, "Bad Request", &json!({"error": msg})).await;
        }
    };
    let mut raw = reader.into_inner();

    if req.method == "GET" && req.path == "/health" {
        return write_json(&mut raw, 200, "OK", &json!({"status": "ok"})).await;
    }

    if !check_auth(&req, state.api_key.as_deref()) {
        return write_json(
            &mut raw,
            401,
            "Unauthorized",
            &json!({"error": "unauthorized"}),
        )
        .await;
    }

    if req.method == "POST" && req.path == "/v1/runs" {
        return handle_create_run(&mut raw, &req.body, state).await;
    }

    if let Some(rest) = req.path.strip_prefix("/v1/runs/") {
        if let Some(run_id) = rest.strip_suffix("/events") {
            if req.method == "GET" && !run_id.is_empty() && !run_id.contains('/') {
                return handle_run_events(&mut raw, run_id, &state).await;
            }
        } else if let Some(run_id) = rest.strip_suffix("/stop") {
            if req.method == "POST" && !run_id.is_empty() && !run_id.contains('/') {
                return handle_run_stop(&mut raw, run_id, &state).await;
            }
        } else if let Some(run_id) = rest.strip_suffix("/clarify") {
            if req.method == "POST" && !run_id.is_empty() && !run_id.contains('/') {
                return handle_run_clarify(&mut raw, run_id, &req.body, &state).await;
            }
        } else if !rest.is_empty() && !rest.contains('/') && req.method == "GET" {
            return handle_run_status(&mut raw, rest, &state).await;
        }
    }

    write_json(&mut raw, 404, "Not Found", &json!({"error": "not found"})).await
}

#[derive(Deserialize)]
struct CreateRunBody {
    input: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    conversation_history: Option<Vec<Value>>,
    #[serde(default)]
    instructions: Option<String>,
}

async fn handle_create_run(
    stream: &mut TcpStream,
    body: &[u8],
    state: GatewayState,
) -> std::io::Result<()> {
    let parsed: CreateRunBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(err) => {
            return write_json(
                stream,
                400,
                "Bad Request",
                &json!({"error": format!("invalid JSON: {err}")}),
            )
            .await;
        }
    };
    if parsed.input.trim().is_empty() {
        return write_json(
            stream,
            400,
            "Bad Request",
            &json!({"error": "input must be non-empty"}),
        )
        .await;
    }

    let seq = state.counter.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let run_id = format!("run_{nanos:x}_{seq}");
    let session_id = parsed
        .session_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("sess_{nanos:x}_{seq}"));

    let entry = Arc::new(RunEntry::new(run_id.clone(), session_id.clone()));
    {
        let mut store = state.runs.lock().unwrap_or_else(|p| p.into_inner());
        store.insert(run_id.clone(), Arc::clone(&entry));
    }

    let session = {
        let mut sessions = state.sessions.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(sessions.entry(session_id.clone()).or_insert_with(|| {
            Arc::new(SessionEntry {
                lock: AsyncMutex::new(()),
                state: StdMutex::new(SessionTurnState::default()),
            })
        }))
    };

    let bg_state = state.clone();
    let bg_entry = Arc::clone(&entry);
    tokio::spawn(async move {
        execute_run(
            bg_state,
            session,
            bg_entry,
            parsed.input,
            parsed.conversation_history,
            parsed.instructions,
        )
        .await;
    });

    write_json(
        stream,
        200,
        "OK",
        &json!({
            "run_id": run_id,
            "session_id": session_id,
            "status": "queued",
        }),
    )
    .await
}

async fn handle_run_status(
    stream: &mut TcpStream,
    run_id: &str,
    state: &GatewayState,
) -> std::io::Result<()> {
    let entry = state
        .runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .by_id
        .get(run_id)
        .cloned();
    let Some(entry) = entry else {
        return write_json(stream, 404, "Not Found", &json!({"error": "run not found"})).await;
    };
    write_json(stream, 200, "OK", &entry.snapshot_status()).await
}

async fn handle_run_stop(
    stream: &mut TcpStream,
    run_id: &str,
    state: &GatewayState,
) -> std::io::Result<()> {
    let entry = state
        .runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .by_id
        .get(run_id)
        .cloned();
    let Some(entry) = entry else {
        return write_json(stream, 404, "Not Found", &json!({"error": "run not found"})).await;
    };
    let status = entry.request_stop();
    write_json(
        stream,
        200,
        "OK",
        &json!({"run_id": run_id, "status": status}),
    )
    .await
}

#[derive(Deserialize)]
struct ClarifyBody {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    prompt_id: Option<String>,
    execution_generation: u64,
    answer: String,
}

async fn handle_run_clarify(
    stream: &mut TcpStream,
    run_id: &str,
    body: &[u8],
    state: &GatewayState,
) -> std::io::Result<()> {
    let parsed: ClarifyBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(err) => {
            return write_json(
                stream,
                400,
                "Bad Request",
                &json!({"error": format!("invalid JSON: {err}")}),
            )
            .await;
        }
    };
    let Some(req_id) = parsed
        .request_id
        .or(parsed.prompt_id)
        .filter(|s| !s.is_empty())
    else {
        return write_json(
            stream,
            400,
            "Bad Request",
            &json!({"error": "request_id is required"}),
        )
        .await;
    };
    let entry = state
        .runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .by_id
        .get(run_id)
        .cloned();
    let Some(entry) = entry else {
        return write_json(stream, 404, "Not Found", &json!({"error": "run not found"})).await;
    };
    if entry.submit_clarify(&req_id, parsed.execution_generation, parsed.answer) {
        write_json(stream, 200, "OK", &json!({"status": "answered"})).await
    } else {
        write_json(
            stream,
            409,
            "Conflict",
            &json!({"error": "clarify request already settled"}),
        )
        .await
    }
}

async fn handle_run_events(
    stream: &mut TcpStream,
    run_id: &str,
    state: &GatewayState,
) -> std::io::Result<()> {
    let entry = state
        .runs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .by_id
        .get(run_id)
        .cloned();
    let Some(entry) = entry else {
        return write_json(stream, 404, "Not Found", &json!({"error": "run not found"})).await;
    };

    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    stream.write_all(headers.as_bytes()).await?;
    stream.flush().await?;

    let mut cursor = 0usize;
    loop {
        let notified = entry.notify.notified();
        let (batch, terminal) = {
            let guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
            let slice = if cursor < guard.events.len() {
                guard.events[cursor..].to_vec()
            } else {
                Vec::new()
            };
            cursor = guard.events.len();
            let done = is_terminal_status(guard.status);
            (slice, done)
        };
        let mut saw_terminal = false;
        for event in batch {
            if is_terminal_event_name(event["event"].as_str()) {
                saw_terminal = true;
            }
            let line = format!("data: {}\n\n", event);
            if stream.write_all(line.as_bytes()).await.is_err() {
                return Ok(());
            }
        }
        if stream.flush().await.is_err() {
            return Ok(());
        }
        if saw_terminal || terminal {
            return Ok(());
        }
        if tokio::time::timeout(Duration::from_secs(SSE_KEEPALIVE_SECS), notified)
            .await
            .is_err()
            && (stream.write_all(b": keepalive\n\n").await.is_err()
                || stream.flush().await.is_err())
        {
            return Ok(());
        }
    }
}

async fn execute_run(
    state: GatewayState,
    session: Arc<SessionEntry>,
    entry: Arc<RunEntry>,
    input: String,
    conversation_history: Option<Vec<Value>>,
    instructions: Option<String>,
) {
    // Serialize runs targeting the same session_id while allowing concurrent runs across sessions.
    let _session_guard = session.lock.lock().await;
    if entry.cancel.load(Ordering::SeqCst) {
        entry.finish(
            "cancelled",
            None,
            0,
            0,
            json!({"event": "run.cancelled", "run_id": entry.run_id, "status": "cancelled"}),
        );
        return;
    }

    entry.set_status("running");

    let request = match chat::prepare_gateway_request(
        state.home.as_deref(),
        &state.root,
        input,
        entry.session_id.clone(),
        instructions,
    ) {
        Ok(req) => req,
        Err(err) => {
            entry.finish(
                "failed",
                Some(err.clone()),
                0,
                0,
                json!({"event": "run.failed", "run_id": entry.run_id, "status": "failed", "error": err}),
            );
            return;
        }
    };

    let (mut history, mut db_session_id) = {
        let guard = session.state.lock().unwrap_or_else(|p| p.into_inner());
        (guard.history.clone(), guard.db_session_id.clone())
    };

    let event_entry = Arc::clone(&entry);
    let approve_entry = Arc::clone(&entry);
    let cancel_flag = Arc::clone(&entry.cancel);

    let turn_res = chat::run_gateway_turn(
        state.home.as_deref(),
        request,
        &mut history,
        &mut db_session_id,
        conversation_history,
        state.mcp_pool.as_ref(),
        cancel_flag.as_ref(),
        move |event| match event {
            TurnEvent::ToolStarted { name, call_id, .. } => {
                event_entry.push_event(json!({
                    "event": "tool.started",
                    "run_id": event_entry.run_id,
                    "tool": name,
                    "call_id": call_id,
                    "tool_call_id": call_id,
                }));
            }
            TurnEvent::ToolFinished {
                name,
                call_id,
                succeeded,
                ..
            } => {
                let kind = if succeeded {
                    "tool.completed"
                } else {
                    "tool.failed"
                };
                event_entry.push_event(json!({
                    "event": kind,
                    "run_id": event_entry.run_id,
                    "tool": name,
                    "call_id": call_id,
                    "tool_call_id": call_id,
                    "error": !succeeded,
                }));
            }
            _ => {}
        },
        move |req: ApprovalRequest| {
            let entry = Arc::clone(&approve_entry);
            async move { handle_gateway_approval(entry, req).await }
        },
    )
    .await;

    {
        let mut guard = session.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.history = history;
        guard.db_session_id = db_session_id;
    }

    match turn_res {
        Ok(res) => match res.status {
            GatewayTurnStatus::Completed => {
                if entry.cancel.load(Ordering::SeqCst) {
                    entry.finish(
                        "cancelled",
                        Some(res.output.clone()),
                        res.input_tokens,
                        res.output_tokens,
                        json!({
                            "event": "run.cancelled",
                            "run_id": entry.run_id,
                            "status": "cancelled",
                        }),
                    );
                } else {
                    entry.finish(
                        "completed",
                        Some(res.output.clone()),
                        res.input_tokens,
                        res.output_tokens,
                        json!({
                            "event": "run.completed",
                            "run_id": entry.run_id,
                            "status": "completed",
                            "output": res.output,
                            "usage": {
                                "input_tokens": res.input_tokens,
                                "output_tokens": res.output_tokens,
                            }
                        }),
                    );
                }
            }
            GatewayTurnStatus::Interrupted => {
                let status = if entry.cancel.load(Ordering::SeqCst) {
                    "cancelled"
                } else {
                    "interrupted"
                };
                let event_name = if status == "cancelled" {
                    "run.cancelled"
                } else {
                    "run.interrupted"
                };
                entry.finish(
                    status,
                    Some(res.output.clone()),
                    res.input_tokens,
                    res.output_tokens,
                    json!({
                        "event": event_name,
                        "run_id": entry.run_id,
                        "status": status,
                        "output": res.output,
                    }),
                );
            }
            GatewayTurnStatus::Failed(err) => {
                entry.finish(
                    "failed",
                    Some(err.clone()),
                    res.input_tokens,
                    res.output_tokens,
                    json!({
                        "event": "run.failed",
                        "run_id": entry.run_id,
                        "status": "failed",
                        "error": err,
                    }),
                );
            }
        },
        Err(err) => {
            entry.finish(
                "failed",
                Some(err.clone()),
                0,
                0,
                json!({
                    "event": "run.failed",
                    "run_id": entry.run_id,
                    "status": "failed",
                    "error": err,
                }),
            );
        }
    }
}

async fn handle_gateway_approval(entry: Arc<RunEntry>, req: ApprovalRequest) -> ApprovalDecision {
    if !matches!(req.action, ApprovalAction::Clarify) {
        // Non-clarify approvals are already handled by ApprovalPolicy (e.g. `approvals.mode: off`
        // approves non-hardline actions without calling this callback). Any remaining prompt has no
        // interactive terminal attached in gateway mode.
        return ApprovalDecision::Denied;
    }
    let questions: Vec<Value> = match serde_json::from_str(&req.content) {
        Ok(Value::Array(items)) if !items.is_empty() => items,
        _ => return ApprovalDecision::Unavailable,
    };

    let mut answers = serde_json::Map::new();
    let timeout_secs = req.timeout_secs;
    for (idx, q) in questions.iter().enumerate() {
        if entry.cancel.load(Ordering::SeqCst) {
            return ApprovalDecision::Denied;
        }
        let qid = q["qid"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("q{idx}"));
        let question_text = q["question"].as_str().unwrap_or("").trim().to_owned();
        if question_text.is_empty() {
            continue;
        }
        let choices = q.get("choices").cloned().filter(|v| v.is_array());
        let multi_select = q["multi_select"].as_bool().unwrap_or(false);

        let (tx, rx) = oneshot::channel::<String>();
        let (prompt_id, generation) = {
            let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.clarify_generation += 1;
            let gen = guard.clarify_generation;
            let pid = format!("clarify_{gen}_{idx}");
            guard.pending_clarify = Some(PendingClarify {
                prompt_id: pid.clone(),
                generation: gen,
                tx,
            });
            if !is_terminal_status(guard.status) {
                guard.status = "waiting_for_approval";
            }
            let mut ev = json!({
                "event": "clarify.request",
                "run_id": entry.run_id,
                "prompt_id": pid,
                "execution_generation": gen,
                "question": question_text,
                "multi_select": multi_select,
            });
            if let Some(ch) = choices {
                ev["choices"] = ch;
            }
            guard.events.push(ev);
            (pid, gen)
        };
        entry.notify.notify_waiters();

        let wait_result = if timeout_secs > 0 {
            match tokio::time::timeout(Duration::from_secs(u64::from(timeout_secs)), rx).await {
                Ok(Ok(ans)) => Ok(ans),
                Ok(Err(_)) => Err("cancelled"),
                Err(_) => Err("timed_out"),
            }
        } else {
            rx.await.map_err(|_| "cancelled")
        };

        match wait_result {
            Ok(ans) => {
                if ans.trim().eq_ignore_ascii_case("skip") {
                    answers.insert(qid, Value::Null);
                } else {
                    answers.insert(qid, Value::String(ans));
                }
            }
            Err(reason) => {
                {
                    let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
                    if guard
                        .pending_clarify
                        .as_ref()
                        .is_some_and(|p| p.prompt_id == prompt_id && p.generation == generation)
                    {
                        guard.pending_clarify = None;
                        guard.events.push(json!({
                            "event": "clarify.settled",
                            "run_id": entry.run_id,
                            "prompt_id": prompt_id,
                        }));
                    }
                    if guard.status == "waiting_for_approval" {
                        guard.status = "running";
                    }
                }
                entry.notify.notify_waiters();
                if reason == "timed_out" {
                    return ApprovalDecision::TimedOut;
                }
                return ApprovalDecision::Denied;
            }
        }
    }

    let reply = json!({
        "answers": answers,
        "outcome": "answered",
    });
    ApprovalDecision::Answered(reply.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    async fn http_call(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let body_bytes = body
            .map(|v| serde_json::to_vec(v).unwrap())
            .unwrap_or_default();
        let mut req =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        if let Some(tok) = token {
            req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
        }
        if body.is_some() {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        if !body_bytes.is_empty() {
            stream.write_all(&body_bytes).await.unwrap();
        }
        stream.flush().await.unwrap();

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, rest) = text.split_once("\r\n\r\n").unwrap();
        let status: u16 = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, rest.to_owned())
    }

    fn parse_sse_events(raw: &str) -> Vec<Value> {
        raw.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .collect()
    }

    struct TempDirGuard(PathBuf);
    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gateway_api_matches_aurumai_relay_contract_for_tools_replay_clarify_and_stop() {
        // 1. Mock OpenAI Chat Completions server handling 3 scenarios:
        //    - Run 1 (tool call + final answer + instructions dedup + conversation_history)
        //    - Run 2 (clarify question -> receives answer -> completes)
        //    - Run 3 (clarify question -> stopped via POST /v1/runs/{id}/stop)
        let llm_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        let recorded_reqs = Arc::new(StdMutex::new(Vec::<Value>::new()));
        let recorded_srv = Arc::clone(&recorded_reqs);
        let llm_step = Arc::new(AtomicUsize::new(0));
        let llm_step_srv = Arc::clone(&llm_step);

        let llm_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = llm_listener.accept().await else {
                    break;
                };
                let mut reader = BufReader::new(stream);
                let Ok(Some(req)) = read_http_request(&mut reader).await else {
                    continue;
                };
                let mut raw = reader.into_inner();
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                recorded_srv.lock().unwrap().push(body.clone());
                let idx = llm_step_srv.fetch_add(1, Ordering::SeqCst);
                let reply = match idx {
                    // Run 1, step 0: call read_file
                    0 => json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call_read_1",
                                    "type": "function",
                                    "function": {
                                        "name": "read_file",
                                        "arguments": "{\"path\":\"hello.txt\"}"
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                    }),
                    // Run 1, step 1: final text answer
                    1 => json!({
                        "choices": [{
                            "message": {"role": "assistant", "content": "File says hi!"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 6}
                    }),
                    // Run 2, step 0: ask clarify question
                    2 => json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call_clarify_1",
                                    "type": "function",
                                    "function": {
                                        "name": "clarify",
                                        "arguments": "{\"questions\":[{\"question\":\"Which format?\",\"choices\":[\"CSV\",\"JSON\"]}]}"
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 12, "completion_tokens": 8}
                    }),
                    // Run 2, step 1: echo user's clarify choice
                    3 => json!({
                        "choices": [{
                            "message": {"role": "assistant", "content": "Format chosen: JSON"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 18, "completion_tokens": 5}
                    }),
                    // Run 3, step 0: ask clarify question (will be stopped via /stop)
                    4 => json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call_clarify_stop",
                                    "type": "function",
                                    "function": {
                                        "name": "clarify",
                                        "arguments": "{\"questions\":[{\"question\":\"Wait forever?\"}]}"
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }]
                    }),
                    _ => json!({
                        "choices": [{
                            "message": {"role": "assistant", "content": "Stopped"},
                            "finish_reason": "stop"
                        }]
                    }),
                };
                let _ = write_json(&mut raw, 200, "OK", &reply).await;
            }
        });

        // 2. Profile + workspace matching AurumAI's config.yaml
        let workspace = TempDirGuard::new("hsr-gw-test");
        let profile = workspace.path().join(".hermes");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(workspace.path().join("hello.txt"), "hello aurum\n").unwrap();
        let cfg = json!({
            "model": {
                "provider": "custom",
                "base_url": format!("http://{llm_addr}/v1"),
                "default": "fixture-model",
                "api_key": "sess-token"
            },
            "display": {"streaming": false},
            "approvals": {"mode": "off"},
            "agent": {
                "clarify_timeout": 30,
                "system_prompt": "SANDBOX_NOTE_UNIQUE",
                "image_input_mode": "text"
            },
            "platform_toolsets": {"api_server": ["hermes-api-server", "clarify"]},
            "platforms": {
                "api_server": {
                    "enabled": true,
                    "extra": {"host": "127.0.0.1", "port": 8642, "key": "test-api-key"}
                }
            }
        });
        std::fs::write(profile.join("config.yaml"), cfg.to_string()).unwrap();

        let gw_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw_addr = gw_listener.local_addr().unwrap();
        let gw_home = profile.clone();
        let gw_root = hermes_runtime::paths::canonical(workspace.path()).unwrap();
        let gw_task = tokio::spawn(async move {
            let _ = serve_on_listener(
                gw_listener,
                Some(gw_home),
                gw_root,
                Some("test-api-key".to_owned()),
                Arc::new(McpPool::new()),
            )
            .await;
        });

        // 3. Health check without auth
        let (st, body) = http_call(gw_addr, "GET", "/health", None, None).await;
        assert_eq!(st, 200, "{body}");

        // 4. Unauthorized without Bearer key
        let (st, _) = http_call(
            gw_addr,
            "POST",
            "/v1/runs",
            None,
            Some(&json!({"input": "hi"})),
        )
        .await;
        assert_eq!(st, 401);

        // 5. Run 1: tool call + SSE stream + replay + conversation_history + instructions dedup
        let (st, body) = http_call(
            gw_addr,
            "POST",
            "/v1/runs",
            Some("test-api-key"),
            Some(&json!({
                "input": "Read hello.txt",
                "session_id": "sess-1",
                "conversation_history": [
                    {"role": "user", "content": "Prior question"},
                    {"role": "assistant", "content": "Prior answer"}
                ],
                "instructions": "SANDBOX_NOTE_UNIQUE"
            })),
        )
        .await;
        assert_eq!(st, 200, "{body}");
        let run1: Value = serde_json::from_str(&body).unwrap();
        let run1_id = run1["run_id"].as_str().unwrap().to_owned();

        let (st, sse1) = http_call(
            gw_addr,
            "GET",
            &format!("/v1/runs/{run1_id}/events"),
            Some("test-api-key"),
            None,
        )
        .await;
        assert_eq!(st, 200);
        let events1 = parse_sse_events(&sse1);
        assert!(
            events1.iter().any(|e| {
                e["event"] == "tool.started"
                    && e["tool"] == "read_file"
                    && e["call_id"] == "call_read_1"
            }),
            "{events1:?}"
        );
        assert!(
            events1.iter().any(|e| {
                e["event"] == "tool.completed"
                    && e["tool"] == "read_file"
                    && e["call_id"] == "call_read_1"
                    && e["error"] == false
            }),
            "{events1:?}"
        );
        assert!(
            events1.iter().any(|e| e["event"] == "run.completed"),
            "{events1:?}"
        );

        // Reattach to /events replays the full stream from start.
        let (st, sse1_replay) = http_call(
            gw_addr,
            "GET",
            &format!("/v1/runs/{run1_id}/events"),
            Some("test-api-key"),
            None,
        )
        .await;
        assert_eq!(st, 200);
        assert_eq!(parse_sse_events(&sse1_replay), events1);

        let (st, status1_raw) = http_call(
            gw_addr,
            "GET",
            &format!("/v1/runs/{run1_id}"),
            Some("test-api-key"),
            None,
        )
        .await;
        assert_eq!(st, 200);
        let status1: Value = serde_json::from_str(&status1_raw).unwrap();
        assert_eq!(status1["status"], "completed");
        assert_eq!(status1["output"], "File says hi!");
        assert_eq!(status1["usage"]["input_tokens"], 30);
        assert_eq!(status1["usage"]["output_tokens"], 11);

        // Verify conversation_history was seeded and SANDBOX_NOTE_UNIQUE was not duplicated.
        {
            let reqs = recorded_reqs.lock().unwrap();
            let first_msgs = reqs[0]["messages"].as_array().unwrap();
            assert!(
                first_msgs
                    .iter()
                    .any(|m| m["content"].as_str() == Some("Prior question")),
                "{first_msgs:?}"
            );
            let sys_text = first_msgs[0]["content"].as_str().unwrap();
            assert_eq!(
                sys_text.matches("SANDBOX_NOTE_UNIQUE").count(),
                1,
                "instructions matching agent.system_prompt must not be duplicated"
            );
        }

        // 6. Run 2: clarify request -> POST /clarify (200) -> duplicate POST /clarify (409) -> completed
        let (st, body) = http_call(
            gw_addr,
            "POST",
            "/v1/runs",
            Some("test-api-key"),
            Some(&json!({"input": "Ask me format", "session_id": "sess-2"})),
        )
        .await;
        assert_eq!(st, 200, "{body}");
        let run2_id = serde_json::from_str::<Value>(&body).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Poll status until waiting_for_approval, then read SSE prefix for prompt_id & generation.
        let mut req_stream = TcpStream::connect(gw_addr).await.unwrap();
        let req_str = format!(
            "GET /v1/runs/{run2_id}/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer test-api-key\r\nConnection: close\r\n\r\n"
        );
        req_stream.write_all(req_str.as_bytes()).await.unwrap();
        let mut sse_reader = BufReader::new(req_stream);
        // Skip HTTP headers
        loop {
            let mut l = String::new();
            sse_reader.read_line(&mut l).await.unwrap();
            if l == "\r\n" || l.is_empty() {
                break;
            }
        }
        let mut prompt_id = String::new();
        let mut saw_settled = false;
        let mut saw_completed = false;
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), sse_reader.read_line(&mut line))
                .await
                .expect("timed out waiting for SSE event")
                .unwrap();
            if n == 0 {
                break;
            }
            if let Some(data) = line.strip_prefix("data:") {
                let ev: Value = serde_json::from_str(data.trim()).unwrap();
                match ev["event"].as_str() {
                    Some("clarify.request") => {
                        prompt_id = ev["prompt_id"].as_str().unwrap().to_owned();
                        let gen = ev["execution_generation"].as_u64().unwrap();
                        assert_eq!(ev["question"], "Which format?");
                        let (cst, _) = http_call(
                            gw_addr,
                            "POST",
                            &format!("/v1/runs/{run2_id}/clarify"),
                            Some("test-api-key"),
                            Some(&json!({
                                "request_id": prompt_id,
                                "execution_generation": gen,
                                "answer": "JSON"
                            })),
                        )
                        .await;
                        assert_eq!(cst, 200);
                        // Second answer for same settled question must return 409 Conflict.
                        let (dup_st, _) = http_call(
                            gw_addr,
                            "POST",
                            &format!("/v1/runs/{run2_id}/clarify"),
                            Some("test-api-key"),
                            Some(&json!({
                                "request_id": prompt_id,
                                "execution_generation": gen,
                                "answer": "CSV"
                            })),
                        )
                        .await;
                        assert_eq!(dup_st, 409);
                    }
                    Some("clarify.settled") => {
                        assert_eq!(ev["prompt_id"], prompt_id);
                        saw_settled = true;
                    }
                    Some("run.completed") => {
                        assert_eq!(ev["output"], "Format chosen: JSON");
                        saw_completed = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_settled && saw_completed);

        // 7. Run 3: POST /stop cancels a run waiting on clarify and emits clarify.settled + terminal event.
        let (st, body) = http_call(
            gw_addr,
            "POST",
            "/v1/runs",
            Some("test-api-key"),
            Some(&json!({"input": "Block on question", "session_id": "sess-3"})),
        )
        .await;
        assert_eq!(st, 200, "{body}");
        let run3_id = serde_json::from_str::<Value>(&body).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Wait until waiting_for_approval
        for _ in 0..50 {
            let (_, sraw) = http_call(
                gw_addr,
                "GET",
                &format!("/v1/runs/{run3_id}"),
                Some("test-api-key"),
                None,
            )
            .await;
            let sv: Value = serde_json::from_str(&sraw).unwrap();
            if sv["status"] == "waiting_for_approval" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (st, stop_body) = http_call(
            gw_addr,
            "POST",
            &format!("/v1/runs/{run3_id}/stop"),
            Some("test-api-key"),
            Some(&json!({})),
        )
        .await;
        assert_eq!(st, 200, "{stop_body}");

        let (st, sse3) = http_call(
            gw_addr,
            "GET",
            &format!("/v1/runs/{run3_id}/events"),
            Some("test-api-key"),
            None,
        )
        .await;
        assert_eq!(st, 200);
        let events3 = parse_sse_events(&sse3);
        assert!(
            events3.iter().any(|e| e["event"] == "clarify.settled"),
            "{events3:?}"
        );
        assert!(
            events3.iter().any(|e| {
                matches!(
                    e["event"].as_str(),
                    Some("run.cancelled" | "run.interrupted")
                )
            }),
            "{events3:?}"
        );

        gw_task.abort();
        llm_task.abort();
    }
}
