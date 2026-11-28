//! MCP client over stdio, Streamable HTTP, and legacy HTTP+SSE.
//!
//! Follows `tools/mcp_tool*.py` and the `mcp` SDK it calls (2.3.0), as recorded by
//! `compat/create_mcp_corpus.py` and `compat/create_mcp_remainder_corpus.py`:
//!
//! * a call that is abandoned (its turn is cancelled, or its time runs out) sends
//!   `notifications/cancelled` with `{requestId, reason: "caller cancelled"}`; `initialize` never does;
//! * a Streamable HTTP request that gets a 404 for a session the client holds fails with
//!   "Session terminated". The server is then initialized again and tools are listed again. A
//!   tool with `readOnlyHint: true` and the resource/prompt tools are retried once; any other
//!   tool is not, because it may have run, and the model gets an `outcome_uncertain` error instead;
//! * an event stream that ends before the answer is resumed with a GET and `Last-Event-ID`
//!   (the `retry` field or one second apart, two failed attempts in a row end the call);
//! * legacy HTTP+SSE (`transport: sse`) connects with GET, validates same-origin `event: endpoint`,
//!   and exchanges JSON-RPC requests via POST + SSE responses;
//! * a server that advertises `resources` (unless `tools.resources: false`) gets
//!   `mcp__<server>__list_resources` and `mcp__<server>__read_resource`; one that advertises
//!   `prompts` (unless `tools.prompts: false`) gets `mcp__<server>__list_prompts` and
//!   `mcp__<server>__get_prompt`;
//! * every tool is named `mcp__<server>__<tool>` (`mcp_prefixed_tool_name`), the 64-character
//!   limit of the providers is kept with a hash suffix, and a name two tools of one server share
//!   after sanitising is given to neither;
//! * a call answers with the text `_render_call_tool_result` makes (`json.dumps` separators,
//!   `structuredContent` + 2M hard cap, `_meta`, `isError`, `MEDIA:` image/audio cache,
//!   embedded binary document cache, disk spillover `<persisted-output>`), and is given 300 s
//!   unless `timeout` or `timeouts.mcp.tool_call` says otherwise.
//!
//! Not ported (deferred): OAuth (grouped with provider OAuth), sampling, elicitation,
//! event subscriptions, the circuit breaker, and stdio respawn.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use hermes_core::sse::SseDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::mcp_media::{
    cache_mcp_audio_block, cache_mcp_image_block, cache_mcp_resource_blob,
    maybe_persist_mcp_result, DEFAULT_MCP_RESULT_SIZE_CHARS, DEFAULT_PREVIEW_SIZE_CHARS,
};
use crate::session_search::py_json_dumps;

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
/// The longest time the reference hands to a platform wait (`MAX_SAFE_TIMEOUT_S`, one year).
pub const MAX_TIMEOUT_SECS: f64 = 31_536_000.0;
/// What a tool call may take when neither the server nor `timeouts.mcp.tool_call` says (`_DEFAULT_TOOL_TIMEOUT`).
pub const DEFAULT_TOOL_TIMEOUT_SECS: f64 = 300.0;
/// The reference follows `nextCursor` for at most this many pages of one list.
const MAX_LIST_PAGES: usize = 50;
/// What the SDK waits before it resumes a stream when the server sent no `retry`, and how many
/// resumptions in a row it tries.
const DEFAULT_RESUME_DELAY_MS: u64 = 1000;
const MAX_RESUME_ATTEMPTS: u32 = 2;
/// How long a reconnect after an expired session may take (the reference waits 15 s).
const RECONNECT_LIMIT: Duration = Duration::from_secs(15);
/// Bound on the courtesy writes that cancel a call or end a session.
const COURTESY_LIMIT: Duration = Duration::from_secs(2);
const CANCEL_REASON: &str = "caller cancelled";
/// Variables a server inherits from the parent; everything else must be configured.
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SystemRoot",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
];

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,
    /// Stdio server command. Ignored when `url` is set.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Streamable HTTP or legacy SSE endpoint.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Optional explicit transport (`"sse"` selects legacy HTTP+SSE instead of Streamable HTTP).
    #[serde(default)]
    pub transport: Option<String>,
    /// The time a tool call may take (the reference's `tool_timeout`), in seconds.
    pub timeout_secs: f64,
    pub connect_timeout_secs: u64,
    /// `tools.resources`: offer the resource tools of a server that advertises resources.
    #[serde(default = "enabled")]
    pub resources: bool,
    /// `tools.prompts`: offer the prompt tools of a server that advertises prompts.
    #[serde(default = "enabled")]
    pub prompts: bool,
}

impl McpServerConfig {
    /// Exact configuration equality (including `timeout_secs` bitwise equality) for pooling.
    pub fn same_config(&self, other: &Self) -> bool {
        self.name == other.name
            && self.command == other.command
            && self.args == other.args
            && self.env == other.env
            && self.url == other.url
            && self.headers == other.headers
            && self.transport == other.transport
            && self.timeout_secs.to_bits() == other.timeout_secs.to_bits()
            && self.connect_timeout_secs == other.connect_timeout_secs
            && self.resources == other.resources
            && self.prompts == other.prompts
    }
}

/// Why a request failed. The variant decides the text the model sees, as the reference's
/// `_mcp_call_failed_message` does with the type of the root exception.
#[derive(Clone, Debug, PartialEq)]
enum RpcError {
    /// An error answer from the server, or one the SDK makes up for a lost answer (`MCPError`).
    Server(String),
    /// The session the client holds is gone: the SDK's "Session terminated".
    SessionExpired,
    /// An HTTP status the SDK turns into an error answer.
    Status(u16),
    Timeout {
        method: String,
        elapsed: Duration,
        limit: Duration,
    },
    Other(String),
}

impl RpcError {
    /// The error text of a tool call that failed with this.
    fn call_failed(&self) -> String {
        match self {
            Self::Server(text) => format!("MCP call failed: MCPError: {text}"),
            Self::SessionExpired => "MCP call failed: MCPError: Session terminated".into(),
            Self::Status(404) => "MCP call failed: MCPError: Not Found".into(),
            Self::Status(_) => {
                "MCP call failed: MCPError: Server returned an error response".into()
            }
            Self::Timeout { elapsed, limit, .. } => format!(
                "MCP call failed: TimeoutError: MCP call timed out after {:.1}s (configured timeout: {:.1}s)",
                elapsed.as_secs_f64(),
                limit.as_secs_f64()
            ),
            Self::Other(text) => format!("MCP call failed: {text}"),
        }
    }
}

impl std::fmt::Display for RpcError {
    /// The short form, for the warnings about a server that does not start.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(text) | Self::Other(text) => formatter.write_str(text),
            Self::SessionExpired => formatter.write_str("Session terminated"),
            Self::Status(code) => write!(formatter, "HTTP {code}"),
            Self::Timeout { method, .. } => write!(formatter, "{method} timed out"),
        }
    }
}

/// Tasks that tell a server about a call that was dropped. A turn that is cancelled drops its
/// whole future, so nothing is left to wait for them: whoever ends the process (or closes the
/// servers) calls [`flush_cancellations`] first.
static PENDING: StdMutex<Vec<JoinHandle<()>>> = StdMutex::new(Vec::new());

/// Wait, briefly, until every cancellation that is on its way has been written.
pub async fn flush_cancellations() {
    let tasks = std::mem::take(&mut *PENDING.lock().unwrap_or_else(PoisonError::into_inner));
    for task in tasks {
        let _ = timeout(COURTESY_LIMIT, task).await;
    }
}

/// A way to write one notification to a server while a request waits for its answer.
#[derive(Clone)]
enum Notifier {
    Stdio(Arc<Mutex<Option<ChildStdin>>>),
    Http {
        client: reqwest::Client,
        url: String,
        /// The configured headers with the session and the protocol version.
        headers: reqwest::header::HeaderMap,
    },
}

fn cancelled_message(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": id, "reason": CANCEL_REASON}
    })
}

impl Notifier {
    /// Write the cancellation without waiting, where that is possible: a short line goes into the
    /// pipe of a stdio server at once (a write of at most a pipe's atomic size). This is what makes
    /// it reach a server whose connection is dropped right after the call.
    fn cancelled_now(&self, id: u64) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Self::Stdio(stdin) = self {
            use std::os::fd::AsFd;
            let Ok(guard) = stdin.try_lock() else {
                return false;
            };
            let Some(stdin) = guard.as_ref() else {
                return false;
            };
            let mut line = cancelled_message(id).to_string();
            line.push('\n');
            return rustix::io::write(stdin.as_fd(), line.as_bytes()).is_ok();
        }
        let _ = id;
        false
    }

    /// `notifications/cancelled` for request `id`. Errors are ignored: the call is over anyway.
    async fn cancelled(&self, id: u64) {
        let message = cancelled_message(id);
        match self {
            Self::Stdio(stdin) => {
                let write = async {
                    let mut guard = stdin.lock().await;
                    if let Some(stdin) = guard.as_mut() {
                        let mut line = message.to_string();
                        line.push('\n');
                        let _ = stdin.write_all(line.as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                };
                let _ = timeout(COURTESY_LIMIT, write).await;
            }
            Self::Http {
                client,
                url,
                headers,
            } => {
                let request = client
                    .post(url)
                    .headers(headers.clone())
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    )
                    .json(&message)
                    .send();
                let _ = timeout(COURTESY_LIMIT, request).await;
            }
        }
    }
}

/// Tells the server to stop working on a request that is dropped before it is answered. The
/// future of a cancelled turn is dropped, so the notification goes out from a task.
struct CancelGuard {
    notifier: Option<Notifier>,
    id: u64,
}

impl CancelGuard {
    fn new(notifier: Notifier, id: u64) -> Self {
        Self {
            notifier: Some(notifier),
            id,
        }
    }

    /// The request was answered or failed on its own: nothing to cancel.
    fn disarm(mut self) {
        self.notifier = None;
    }

    /// The request ran out of time: cancel it now and wait for the write.
    async fn fire(mut self) {
        if let Some(notifier) = self.notifier.take() {
            notifier.cancelled(self.id).await;
        }
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let Some(notifier) = self.notifier.take() else {
            return;
        };
        if notifier.cancelled_now(self.id) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let id = self.id;
        let task = runtime.spawn(async move { notifier.cancelled(id).await });
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|task| !task.is_finished());
        pending.push(task);
    }
}

struct Connection {
    child: Child,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    /// Process group id, kept after the child is reaped so grandchildren can still be stopped.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    group: Option<u32>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Connection {
    async fn send(&mut self, message: &Value) -> Result<(), RpcError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| RpcError::Other("server is closed".into()))?;
        let mut line = message.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| RpcError::Other(format!("write failed: {error}")))?;
        stdin
            .flush()
            .await
            .map_err(|error| RpcError::Other(format!("write failed: {error}")))
    }

    async fn read_message(&mut self) -> Result<Value, RpcError> {
        loop {
            let mut line = String::new();
            let read = (&mut self.stdout)
                .take(MAX_LINE_BYTES)
                .read_line(&mut line)
                .await
                .map_err(|error| RpcError::Other(format!("read failed: {error}")))?;
            if read == 0 {
                return Err(RpcError::Other("server closed the connection".into()));
            }
            if !line.ends_with('\n') {
                return Err(RpcError::Other("server message is too large".into()));
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line)
                .map_err(|_| RpcError::Other("server sent invalid JSON".into()));
        }
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let guard = (method != "initialize")
            .then(|| CancelGuard::new(Notifier::Stdio(Arc::clone(&self.stdin)), id));
        let started = Instant::now();
        let wait = async {
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await?;
            loop {
                let message = self.read_message().await?;
                if message.get("method").is_some() {
                    // Notifications are ignored. Requests from the server are refused.
                    if let Some(request_id) = message.get("id") {
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {"code": -32601, "message": "Method not found"}
                        });
                        self.send(&reply).await?;
                    }
                    continue;
                }
                if message.get("id") != Some(&json!(id)) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed");
                    return Err(RpcError::Server(text.to_owned()));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        finish(timeout(limit, wait).await, guard, method, started, limit).await
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn kill_group(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(pid) = self.group.and_then(|pid| i32::try_from(pid).ok()) {
            if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
        let _ = self.child.start_kill();
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// The outcome of a request that may have run out of time: a request that did is cancelled.
async fn finish(
    outcome: Result<Result<Value, RpcError>, tokio::time::error::Elapsed>,
    guard: Option<CancelGuard>,
    method: &str,
    started: Instant,
    limit: Duration,
) -> Result<Value, RpcError> {
    match outcome {
        Ok(result) => {
            if let Some(guard) = guard {
                guard.disarm();
            }
            result
        }
        Err(_) => {
            if let Some(guard) = guard {
                guard.fire().await;
            }
            Err(RpcError::Timeout {
                method: method.to_owned(),
                elapsed: started.elapsed(),
                limit,
            })
        }
    }
}

/// A server reached over stdio, Streamable HTTP, or legacy HTTP+SSE.
enum Transport {
    Stdio(Box<Connection>),
    Http(HttpConnection),
    Sse(SseConnection),
}

impl Transport {
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        match self {
            Self::Stdio(connection) => connection.request(method, params, limit).await,
            Self::Http(connection) => connection.request(method, params, limit).await,
            Self::Sse(connection) => connection.request(method, params, limit).await,
        }
    }

    async fn notify(&mut self, method: &str, limit: Duration) -> Result<(), RpcError> {
        let message = json!({"jsonrpc": "2.0", "method": method});
        match self {
            Self::Stdio(connection) => connection.send(&message).await,
            Self::Http(connection) => connection.post(&message, limit).await.map(|_| ()),
            Self::Sse(connection) => connection.post(&message, limit).await,
        }
    }

    /// Close stdin and give a process a moment to exit, or end the HTTP/SSE session.
    async fn close(&mut self) {
        match self {
            Self::Stdio(connection) => {
                flush_cancellations().await;
                *connection.stdin.lock().await = None;
                let _ = timeout(Duration::from_secs(2), connection.child.wait()).await;
                connection.kill_group();
            }
            Self::Http(connection) => {
                flush_cancellations().await;
                connection.end_session().await;
            }
            Self::Sse(connection) => {
                flush_cancellations().await;
                connection.close();
            }
        }
    }
}

struct HttpConnection {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    session: Option<String>,
    /// The session expired and has not been replaced yet.
    down: bool,
    next_id: u64,
}

/// What the event streams of one request have shown so far.
#[derive(Default)]
struct StreamState {
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
}

enum StreamEnd {
    /// The request was answered (or failed for good).
    Answer(Result<Value, RpcError>),
    /// The stream closed without an answer.
    Closed,
    /// Reading the stream failed.
    Broke,
}

fn build_http_client(
    config: &McpServerConfig,
    url: &str,
) -> Result<(reqwest::Url, reqwest::Client, reqwest::header::HeaderMap), String> {
    use reqwest::header::{HeaderName, HeaderValue};

    let parsed = reqwest::Url::parse(url).map_err(|_| "invalid server URL".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("server URL must use http or https".into());
    }
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &config.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        let mut value =
            HeaderValue::from_str(value).map_err(|_| format!("invalid value for header {name}"))?;
        // Keep credentials out of debug output.
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .tls_built_in_native_certs(true)
        // A redirect would forward the configured headers to another origin.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("cannot create HTTP client: {}", error.without_url()))?;
    Ok((parsed, client, headers))
}

impl HttpConnection {
    fn new(config: &McpServerConfig, url: &str) -> Result<Self, String> {
        let (_parsed, client, headers) = build_http_client(config, url)?;
        Ok(Self {
            client,
            url: url.to_owned(),
            headers,
            session: None,
            down: false,
            next_id: 0,
        })
    }

    /// Forget the session, so that the next `initialize` opens a new one.
    fn reset_session(&mut self) {
        self.down = true;
        self.session = None;
        self.headers.remove("mcp-protocol-version");
    }

    fn notifier(&self) -> Notifier {
        let mut headers = self.headers.clone();
        if let Some(value) = self
            .session
            .as_deref()
            .and_then(|session| reqwest::header::HeaderValue::from_str(session).ok())
        {
            headers.insert("mcp-session-id", value);
        }
        Notifier::Http {
            client: self.client.clone(),
            url: self.url.clone(),
            headers,
        }
    }

    /// A notification or an answer to the server, bounded by `limit`.
    async fn post(
        &mut self,
        message: &Value,
        limit: Duration,
    ) -> Result<reqwest::Response, RpcError> {
        timeout(limit, self.send_post(message))
            .await
            .map_err(|_| RpcError::Other("request timed out".into()))?
    }

    /// POST without a limit of its own; a request is bound by the limit of the whole exchange.
    async fn send_post(&self, message: &Value) -> Result<reqwest::Response, RpcError> {
        let mut builder = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(message);
        if let Some(session) = &self.session {
            builder = builder.header("Mcp-Session-Id", session);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| RpcError::Other(format!("request failed: {}", error.without_url())))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status.as_u16() == 404 && self.session.is_some() {
            return Err(RpcError::SessionExpired);
        }
        Err(RpcError::Status(status.as_u16()))
    }

    /// Open the stream of a request again after its stream ended: a GET that carries the id of
    /// the last event it saw.
    async fn resume(&self, last_event_id: &str) -> Option<reqwest::Response> {
        let mut builder = self
            .client
            .get(&self.url)
            .headers(self.headers.clone())
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("Last-Event-ID", last_event_id);
        if let Some(session) = &self.session {
            builder = builder.header("Mcp-Session-Id", session);
        }
        let response = builder.send().await.ok()?;
        let is_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
        (response.status().is_success() && is_stream).then_some(response)
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let guard = (method != "initialize").then(|| CancelGuard::new(self.notifier(), id));
        let started = Instant::now();
        let result = finish(
            timeout(limit, self.exchange(&message, method, id)).await,
            guard,
            method,
            started,
            limit,
        )
        .await;
        if method == "initialize" {
            self.down = self.down && result.is_err();
            if let Some(version) = result
                .as_ref()
                .ok()
                .and_then(|value| value["protocolVersion"].as_str())
                .and_then(|version| reqwest::header::HeaderValue::from_str(version).ok())
            {
                self.headers.insert("mcp-protocol-version", version);
            }
        }
        result
    }

    /// Post a request and read its answer from the JSON body or the event stream.
    async fn exchange(
        &mut self,
        message: &Value,
        method: &str,
        id: u64,
    ) -> Result<Value, RpcError> {
        let response = self.send_post(message).await?;
        if method == "initialize" {
            self.session = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
        }
        let kind = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if kind.starts_with("application/json") {
            return self.read_json(response, id).await;
        }
        if !kind.starts_with("text/event-stream") {
            return Err(RpcError::Other(
                "server answered with an unsupported content type".into(),
            ));
        }
        let mut state = StreamState::default();
        match self.read_stream(response, id, &mut state).await {
            StreamEnd::Answer(result) => return result,
            StreamEnd::Closed | StreamEnd::Broke => {}
        }
        // The stream ended before the answer. With an event id it can be resumed (as the SDK does).
        if state.last_event_id.is_none() {
            return Err(RpcError::Server(
                "SSE stream ended without a response".into(),
            ));
        }
        let mut attempt = 0;
        loop {
            if attempt >= MAX_RESUME_ATTEMPTS {
                return Err(RpcError::Server(
                    "SSE stream ended and reconnection attempts were exhausted".into(),
                ));
            }
            sleep(Duration::from_millis(
                state.retry_ms.unwrap_or(DEFAULT_RESUME_DELAY_MS),
            ))
            .await;
            let last = state.last_event_id.clone().unwrap_or_default();
            let Some(response) = self.resume(&last).await else {
                attempt += 1;
                continue;
            };
            match self.read_stream(response, id, &mut state).await {
                StreamEnd::Answer(result) => return result,
                // A stream that ends again starts the count anew; one that breaks does not.
                StreamEnd::Closed => attempt = 0,
                StreamEnd::Broke => attempt += 1,
            }
        }
    }

    async fn read_json(
        &mut self,
        mut response: reqwest::Response,
        id: u64,
    ) -> Result<Value, RpcError> {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| RpcError::Other(format!("read failed: {}", error.without_url())))?
        {
            if body.len() + chunk.len() > MAX_LINE_BYTES as usize {
                return Err(RpcError::Other("server message is too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        let parsed: Value = serde_json::from_slice(&body)
            .map_err(|_| RpcError::Other("server sent invalid JSON".into()))?;
        let batch = match parsed {
            Value::Array(messages) => messages,
            single => vec![single],
        };
        for message in batch {
            if let Some(result) = self.handle(message, id).await {
                return result;
            }
        }
        Err(RpcError::Other(
            "response did not answer the request".into(),
        ))
    }

    /// Read events until one answers request `id`, or the stream ends.
    async fn read_stream(
        &mut self,
        mut response: reqwest::Response,
        id: u64,
        state: &mut StreamState,
    ) -> StreamEnd {
        let mut decoder = SseDecoder::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return StreamEnd::Closed,
                Err(_) => return StreamEnd::Broke,
            };
            let events = match decoder.push(&chunk) {
                Ok(events) => events,
                Err(_) => {
                    return StreamEnd::Answer(Err(RpcError::Other(
                        "server sent an invalid event stream".into(),
                    )))
                }
            };
            if let Some(last) = decoder.last_event_id() {
                state.last_event_id = Some(last.to_owned());
            }
            if let Some(retry) = decoder.retry_ms() {
                state.retry_ms = Some(retry);
            }
            for data in events {
                // An event with an id and no data only primes the stream for resumption.
                if data.is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&data) else {
                    return StreamEnd::Answer(Err(RpcError::Other(
                        "server sent invalid JSON".into(),
                    )));
                };
                if let Some(result) = self.handle(message, id).await {
                    return StreamEnd::Answer(result);
                }
            }
        }
    }

    /// The outcome if `message` answers request `id`. Notifications are ignored and requests
    /// from the server are refused with a separate POST.
    async fn handle(&mut self, message: Value, id: u64) -> Option<Result<Value, RpcError>> {
        if message.get("method").is_some() {
            if let Some(request_id) = message.get("id") {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32601, "message": "Method not found"}
                });
                let _ = self.send_post(&reply).await;
            }
            return None;
        }
        if message.get("id") != Some(&json!(id)) {
            return None;
        }
        if let Some(error) = message.get("error") {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            return Some(Err(RpcError::Server(text.to_owned())));
        }
        Some(Ok(message.get("result").cloned().unwrap_or(Value::Null)))
    }

    async fn end_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let request = self
            .client
            .delete(&self.url)
            .headers(self.headers.clone())
            .header("Mcp-Session-Id", session)
            .send();
        // Servers may answer 405 when they do not allow clients to end sessions.
        let _ = timeout(COURTESY_LIMIT, request).await;
    }
}

type SsePending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// Legacy MCP HTTP+SSE connection (MCP 2024-11-05): one long-lived GET SSE stream receives
/// `event: endpoint` followed by `event: message` JSON-RPC replies; client requests and
/// notifications are POSTed to the advertised same-origin endpoint.
struct SseConnection {
    client: reqwest::Client,
    post_url: String,
    headers: reqwest::header::HeaderMap,
    pending: SsePending,
    reader: JoinHandle<()>,
    next_id: u64,
}

fn same_origin(base: &reqwest::Url, target: &reqwest::Url) -> bool {
    base.scheme() == target.scheme()
        && base.host_str() == target.host_str()
        && base.port_or_known_default() == target.port_or_known_default()
}

impl SseConnection {
    async fn connect(config: &McpServerConfig, url: &str) -> Result<Self, String> {
        let (parsed, client, headers) = build_http_client(config, url)?;
        let connect_limit = Duration::from_secs(config.connect_timeout_secs);
        let mut response = timeout(
            connect_limit,
            client
                .get(url)
                .headers(headers.clone())
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .send(),
        )
        .await
        .map_err(|_| "SSE connect timed out".to_owned())?
        .map_err(|error| format!("SSE connect failed: {}", error.without_url()))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let kind = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !kind.starts_with("text/event-stream") {
            return Err("SSE server answered with an unsupported content type".into());
        }

        let mut decoder = SseDecoder::new();
        let mut post_url: Option<String> = None;
        let mut buffered_messages: Vec<String> = Vec::new();

        let wait_endpoint = async {
            while post_url.is_none() {
                let chunk = response
                    .chunk()
                    .await
                    .map_err(|error| format!("SSE read failed: {}", error.without_url()))?
                    .ok_or_else(|| "SSE stream closed before endpoint event".to_owned())?;
                let events = decoder
                    .push_events(&chunk)
                    .map_err(|_| "server sent an invalid event stream".to_owned())?;
                for event in events {
                    match event.event.as_deref() {
                        Some("endpoint") if post_url.is_none() => {
                            let raw = event.data.trim();
                            if raw.is_empty() {
                                return Err("SSE endpoint event was empty".into());
                            }
                            let resolved = parsed
                                .join(raw)
                                .map_err(|_| "invalid SSE endpoint URL".to_owned())?;
                            if !same_origin(&parsed, &resolved) {
                                return Err(
                                    "SSE endpoint origin does not match server URL".to_owned()
                                );
                            }
                            post_url = Some(resolved.to_string());
                        }
                        None | Some("message") if !event.data.is_empty() => {
                            buffered_messages.push(event.data);
                        }
                        _ => {}
                    }
                }
            }
            Ok::<(), String>(())
        };
        timeout(connect_limit, wait_endpoint)
            .await
            .map_err(|_| "SSE endpoint handshake timed out".to_owned())??;

        let post_url = post_url.ok_or_else(|| "missing SSE endpoint".to_owned())?;
        let pending: SsePending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_client = client.clone();
        let reader_post_url = post_url.clone();
        let reader_headers = headers.clone();
        let reader = tokio::spawn(async move {
            for data in buffered_messages {
                dispatch_sse_message(
                    &data,
                    &reader_pending,
                    &reader_client,
                    &reader_post_url,
                    &reader_headers,
                )
                .await;
            }
            loop {
                let chunk = match response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) | Err(_) => break,
                };
                let Ok(events) = decoder.push_events(&chunk) else {
                    break;
                };
                for event in events {
                    if matches!(event.event.as_deref(), None | Some("message"))
                        && !event.data.is_empty()
                    {
                        dispatch_sse_message(
                            &event.data,
                            &reader_pending,
                            &reader_client,
                            &reader_post_url,
                            &reader_headers,
                        )
                        .await;
                    }
                }
            }
            let mut map = reader_pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(RpcError::Server(
                    "SSE stream ended without a response".into(),
                )));
            }
        });

        Ok(Self {
            client,
            post_url,
            headers,
            pending,
            reader,
            next_id: 0,
        })
    }

    fn notifier(&self) -> Notifier {
        Notifier::Http {
            client: self.client.clone(),
            url: self.post_url.clone(),
            headers: self.headers.clone(),
        }
    }

    async fn post(&mut self, message: &Value, limit: Duration) -> Result<(), RpcError> {
        let send = self
            .client
            .post(&self.post_url)
            .headers(self.headers.clone())
            .json(message)
            .send();
        let response = timeout(limit, send)
            .await
            .map_err(|_| RpcError::Other("request timed out".into()))?
            .map_err(|error| RpcError::Other(format!("request failed: {}", error.without_url())))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(RpcError::Status(response.status().as_u16()))
        }
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let guard = (method != "initialize").then(|| CancelGuard::new(self.notifier(), id));
        let started = Instant::now();
        let wait = async {
            let response = self
                .client
                .post(&self.post_url)
                .headers(self.headers.clone())
                .json(&message)
                .send()
                .await
                .map_err(|error| {
                    RpcError::Other(format!("request failed: {}", error.without_url()))
                })?;
            if !response.status().is_success() {
                return Err(RpcError::Status(response.status().as_u16()));
            }
            rx.await.unwrap_or_else(|_| {
                Err(RpcError::Server(
                    "SSE stream ended without a response".into(),
                ))
            })
        };
        let outcome = finish(timeout(limit, wait).await, guard, method, started, limit).await;
        if outcome.is_err() {
            self.pending.lock().await.remove(&id);
        }
        outcome
    }

    fn close(&mut self) {
        self.reader.abort();
    }
}

impl Drop for SseConnection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn dispatch_sse_message(
    data: &str,
    pending: &SsePending,
    client: &reqwest::Client,
    post_url: &str,
    headers: &reqwest::header::HeaderMap,
) {
    let Ok(message) = serde_json::from_str::<Value>(data) else {
        return;
    };
    if message.get("method").is_some() {
        if let Some(request_id) = message.get("id") {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "Method not found"}
            });
            let _ = timeout(
                COURTESY_LIMIT,
                client
                    .post(post_url)
                    .headers(headers.clone())
                    .json(&reply)
                    .send(),
            )
            .await;
        }
        return;
    }
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };
    let tx = pending.lock().await.remove(&id);
    if let Some(tx) = tx {
        let result = if let Some(error) = message.get("error") {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            Err(RpcError::Server(text.to_owned()))
        } else {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = tx.send(result);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Remote,
    ListResources,
    ReadResource,
    ListPrompts,
    GetPrompt,
}

struct Tool {
    exposed: String,
    server: usize,
    kind: Kind,
    /// The server's own name of a remote tool.
    name: String,
    schema: Value,
    /// `annotations.readOnlyHint`: the only tools that are replayed after a session expired.
    read_only: bool,
}

struct Server {
    config: McpServerConfig,
    transport: Mutex<Transport>,
}

/// Why a request through [`McpClients::request_recovering`] gave no result.
enum Failure {
    Rpc(RpcError),
    /// The session expired under a call that may have had effects; it was not replayed.
    Uncertain {
        reconnected: bool,
    },
    /// The session is gone and a new one could not be opened.
    Down,
}

pub struct McpClients {
    servers: Vec<Server>,
    tools: std::sync::RwLock<Vec<Tool>>,
    profile: Option<PathBuf>,
    mcp_result_size_chars: usize,
}

impl Default for McpClients {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            tools: std::sync::RwLock::new(Vec::new()),
            profile: None,
            mcp_result_size_chars: DEFAULT_MCP_RESULT_SIZE_CHARS,
        }
    }
}

/// What `initialize` and discovery learned about a server.
struct Discovered {
    resources: bool,
    prompts: bool,
    tools: Vec<Value>,
}

/// Initialize a server and list its tools.
async fn handshake(
    transport: &mut Transport,
    config: &McpServerConfig,
) -> Result<Discovered, String> {
    let connect_limit = Duration::from_secs(config.connect_timeout_secs);
    let init = transport
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "hermes", "version": env!("CARGO_PKG_VERSION")}
            }),
            connect_limit,
        )
        .await
        .map_err(|error| error.to_string())?;
    transport
        .notify("notifications/initialized", connect_limit)
        .await
        .map_err(|error| error.to_string())?;
    let tools = list_all(transport, "tools/list", "tools", connect_limit)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Discovered {
        // The family is served exactly when the capability is present (the reference's gate).
        resources: config.resources && init["capabilities"]["resources"].is_object(),
        prompts: config.prompts && init["capabilities"]["prompts"].is_object(),
        tools,
    })
}

/// Follow `nextCursor` through a paginated list.
async fn list_all(
    transport: &mut Transport,
    method: &str,
    key: &str,
    limit: Duration,
) -> Result<Vec<Value>, RpcError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
        let page = transport.request(method, params, limit).await?;
        items.extend(page[key].as_array().cloned().unwrap_or_default());
        cursor = page["nextCursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(items)
}

impl McpClients {
    /// Connect to every configured server. Failures are returned as warnings.
    pub async fn start(configs: &[McpServerConfig]) -> (Self, Vec<String>) {
        Self::start_with_options(configs, None, DEFAULT_MCP_RESULT_SIZE_CHARS).await
    }

    /// Connect to every configured server with a profile directory (for image/audio/document
    /// caches and oversized tool-result spillover) and spillover threshold.
    pub async fn start_with_options(
        configs: &[McpServerConfig],
        profile: Option<PathBuf>,
        mcp_result_size_chars: usize,
    ) -> (Self, Vec<String>) {
        let mut clients = Self {
            servers: Vec::new(),
            tools: std::sync::RwLock::new(Vec::new()),
            profile,
            mcp_result_size_chars,
        };
        let mut warnings = Vec::new();
        for config in configs {
            match clients.connect(config).await {
                Ok(()) => {}
                Err(error) => warnings.push(format!(
                    "MCP server {:?} is unavailable: {error}",
                    config.name
                )),
            }
        }
        (clients, warnings)
    }

    async fn connect(&mut self, config: &McpServerConfig) -> Result<(), String> {
        let mut transport = match config.url.as_deref().filter(|url| !url.is_empty()) {
            Some(url) if config.transport.as_deref() == Some("sse") => {
                Transport::Sse(SseConnection::connect(config, url).await?)
            }
            Some(url) => Transport::Http(HttpConnection::new(config, url)?),
            None => Transport::Stdio(Box::new(spawn_stdio(config)?)),
        };
        // On error the transport drops here, which stops a stdio child's process group.
        let discovered = handshake(&mut transport, config).await?;
        let index = self.servers.len();
        self.servers.push(Server {
            config: config.clone(),
            transport: Mutex::new(transport),
        });
        self.register(index, discovered);
        Ok(())
    }

    /// Replace the tools of server `index` with what discovery found.
    ///
    /// Names follow `_resolve_name_collisions`: a tool twice under one name is kept once, two
    /// tools that sanitise onto one name are both skipped, a resource tool never shadows a tool
    /// of the server, and a name another server holds stays with that server.
    fn register(&self, index: usize, discovered: Discovered) {
        let mut tools = self.tools.write().unwrap_or_else(PoisonError::into_inner);
        tools.retain(|tool| tool.server != index);
        let config_name = self.servers[index].config.name.clone();
        let mut natives: Vec<(String, &Value)> = Vec::new();
        for tool in &discovered.tools {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            if !natives.iter().any(|(seen, _)| seen == name) {
                natives.push((name.to_owned(), tool));
            }
        }
        let exposed_of = |tool: &str| mcp_prefixed_tool_name(&config_name, tool);
        let shared = |exposed: &str| {
            natives
                .iter()
                .filter(|(name, _)| exposed_of(name) == exposed)
                .count()
        };
        for (name, tool) in &natives {
            let exposed = exposed_of(name);
            if shared(&exposed) > 1 || tools.iter().any(|t| t.exposed == exposed) {
                continue;
            }
            let schema = json!({
                "type": "function",
                "function": {
                    "name": exposed,
                    "description": tool["description"].as_str().unwrap_or(""),
                    "parameters": if tool["inputSchema"].is_object() {
                        tool["inputSchema"].clone()
                    } else {
                        json!({"type": "object", "properties": {}})
                    }
                }
            });
            tools.push(Tool {
                exposed,
                server: index,
                kind: Kind::Remote,
                name: name.to_owned(),
                schema,
                read_only: tool["annotations"]["readOnlyHint"].as_bool() == Some(true),
            });
        }
        let mut utilities: Vec<(Kind, &'static str, String, Value)> = Vec::new();
        if discovered.resources {
            utilities.push((
                Kind::ListResources,
                "list_resources",
                format!("List available resources from MCP server '{config_name}'"),
                json!({"type": "object", "properties": {}}),
            ));
            utilities.push((
                Kind::ReadResource,
                "read_resource",
                format!("Read a resource by URI from MCP server '{config_name}'"),
                json!({
                    "type": "object",
                    "properties": {"uri": {"type": "string", "description": "URI of the resource to read"}},
                    "required": ["uri"]
                }),
            ));
        }
        if discovered.prompts {
            utilities.push((
                Kind::ListPrompts,
                "list_prompts",
                format!("List available prompts from MCP server '{config_name}'"),
                json!({"type": "object", "properties": {}}),
            ));
            utilities.push((
                Kind::GetPrompt,
                "get_prompt",
                format!("Get a prompt by name from MCP server '{config_name}'"),
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Name of the prompt to retrieve"},
                        "arguments": {
                            "type": "object",
                            "description": "Optional arguments to pass to the prompt",
                            "properties": {},
                            "additionalProperties": true
                        }
                    },
                    "required": ["name"]
                }),
            ));
        }
        // A tool of the server with the same name wins: the utilities are optional.
        for (kind, key, description, parameters) in utilities {
            let exposed = exposed_of(key);
            if shared(&exposed) > 0 || tools.iter().any(|t| t.exposed == exposed) {
                continue;
            }
            let schema = json!({
                "type": "function",
                "function": {"name": exposed, "description": description, "parameters": parameters}
            });
            tools.push(Tool {
                exposed,
                server: index,
                kind,
                name: key.to_owned(),
                schema,
                read_only: true,
            });
        }
    }

    pub fn schemas(&self) -> Vec<Value> {
        let tools = self.tools.read().unwrap_or_else(PoisonError::into_inner);
        tools.iter().map(|tool| tool.schema.clone()).collect()
    }

    pub fn owns(&self, name: &str) -> bool {
        let tools = self.tools.read().unwrap_or_else(PoisonError::into_inner);
        tools.iter().any(|tool| tool.exposed == name)
    }

    /// Initialize server `index` again after its session expired and list its tools again.
    async fn reconnect(&self, index: usize, transport: &mut Transport) -> bool {
        let Transport::Http(connection) = transport else {
            return false;
        };
        connection.reset_session();
        let config = self.servers[index].config.clone();
        let outcome = timeout(RECONNECT_LIMIT, handshake(transport, &config)).await;
        match outcome {
            Ok(Ok(discovered)) => {
                self.register(index, discovered);
                true
            }
            _ => false,
        }
    }

    /// One request to server `index`. When its session has expired the server is initialized
    /// again; the request is then repeated once if `replay` allows it.
    async fn request_recovering(
        &self,
        index: usize,
        method: &str,
        params: Value,
        replay: bool,
    ) -> Result<Value, Failure> {
        let limit = Duration::from_secs_f64(
            self.servers[index]
                .config
                .timeout_secs
                .clamp(0.001, MAX_TIMEOUT_SECS),
        );
        let mut transport = self.servers[index].transport.lock().await;
        if matches!(&*transport, Transport::Http(c) if c.down)
            && !self.reconnect(index, &mut transport).await
        {
            return Err(Failure::Down);
        }
        let first = transport.request(method, params.clone(), limit).await;
        if !matches!(first, Err(RpcError::SessionExpired)) {
            return first.map_err(Failure::Rpc);
        }
        let reconnected = self.reconnect(index, &mut transport).await;
        if !replay {
            return Err(Failure::Uncertain { reconnected });
        }
        if reconnected {
            // Only an answer counts: a failed repeat leaves the first failure as the error.
            if let Ok(value) = transport.request(method, params, limit).await {
                return Ok(value);
            }
        }
        Err(Failure::Rpc(RpcError::SessionExpired))
    }

    /// Call a tool and return the result in the shape of a local tool output.
    pub async fn call(&self, name: &str, arguments: &str) -> Value {
        self.call_with_id(name, arguments, "mcp_call").await
    }

    /// Call a tool with its `tool_call_id` (used to name spillover files for oversized results).
    pub async fn call_with_id(&self, name: &str, arguments: &str, tool_call_id: &str) -> Value {
        let lookup = {
            let tools = self.tools.read().unwrap_or_else(PoisonError::into_inner);
            tools
                .iter()
                .find(|tool| tool.exposed == name)
                .map(|tool| (tool.server, tool.kind, tool.name.clone(), tool.read_only))
        };
        let Some((index, kind, remote, read_only)) = lookup else {
            return tool_text(&json!({"error": format!("Unknown tool: {name}")}));
        };
        let arguments = match serde_json::from_str::<Value>(arguments) {
            Ok(value) if value.is_object() => value,
            Ok(_) | Err(_) if arguments.trim().is_empty() => json!({}),
            _ => return tool_text(&json!({"error": "Tool arguments must be a JSON object"})),
        };
        let rendered = match kind {
            Kind::Remote => {
                let params = json!({"name": remote, "arguments": arguments});
                match self
                    .request_recovering(index, "tools/call", params, read_only)
                    .await
                {
                    Err(failure) => return self.failure_text(index, failure),
                    Ok(result) => {
                        let server = self.servers[index].config.name.clone();
                        render_call_tool_result(&result, &server, self.profile.as_deref())
                    }
                }
            }
            Kind::ListResources => return self.list_resources(index, tool_call_id).await,
            Kind::ReadResource => {
                let uri = arguments["uri"].as_str().filter(|uri| !uri.is_empty());
                let Some(uri) = uri else {
                    return tool_text(&json!({"error": "Missing required parameter 'uri'"}));
                };
                let params = json!({"uri": uri});
                match self
                    .request_recovering(index, "resources/read", params, true)
                    .await
                {
                    Err(failure) => return self.failure_text(index, failure),
                    Ok(result) => {
                        py_json_dumps(&render_read_resource(&result, self.profile.as_deref()))
                    }
                }
            }
            Kind::ListPrompts => return self.list_prompts(index, tool_call_id).await,
            Kind::GetPrompt => {
                let prompt_name = arguments["name"].as_str().filter(|n| !n.is_empty());
                let Some(prompt_name) = prompt_name else {
                    return tool_text(&json!({"error": "Missing required parameter 'name'"}));
                };
                let mut params = Map::new();
                params.insert("name".into(), json!(prompt_name));
                if let Some(prompt_args) = arguments.get("arguments").filter(|v| v.is_object()) {
                    params.insert("arguments".into(), prompt_args.clone());
                }
                match self
                    .request_recovering(index, "prompts/get", Value::Object(params), true)
                    .await
                {
                    Err(failure) => return self.failure_text(index, failure),
                    Ok(result) => py_json_dumps(&render_get_prompt(&result)),
                }
            }
        };
        Value::String(self.maybe_spillover(&rendered, tool_call_id))
    }

    fn maybe_spillover(&self, rendered: &str, tool_call_id: &str) -> String {
        maybe_persist_mcp_result(
            rendered,
            tool_call_id,
            self.profile.as_deref(),
            self.mcp_result_size_chars,
            DEFAULT_PREVIEW_SIZE_CHARS,
        )
    }

    /// `resources/list`, all pages. The pages are not replayed as a whole: the first failure is the answer.
    async fn list_resources(&self, index: usize, tool_call_id: &str) -> Value {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
            match self
                .request_recovering(index, "resources/list", params, true)
                .await
            {
                Err(failure) => return self.failure_text(index, failure),
                Ok(page) => {
                    resources.extend(page["resources"].as_array().cloned().unwrap_or_default());
                    cursor = page["nextCursor"]
                        .as_str()
                        .filter(|cursor| !cursor.is_empty())
                        .map(str::to_owned);
                }
            }
            if cursor.is_none() {
                break;
            }
        }
        let rendered = py_json_dumps(&render_resource_list(&resources));
        Value::String(self.maybe_spillover(&rendered, tool_call_id))
    }

    /// `prompts/list`, all pages. The pages are not replayed as a whole: the first failure is the answer.
    async fn list_prompts(&self, index: usize, tool_call_id: &str) -> Value {
        let mut prompts = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
            match self
                .request_recovering(index, "prompts/list", params, true)
                .await
            {
                Err(failure) => return self.failure_text(index, failure),
                Ok(page) => {
                    prompts.extend(page["prompts"].as_array().cloned().unwrap_or_default());
                    cursor = page["nextCursor"]
                        .as_str()
                        .filter(|cursor| !cursor.is_empty())
                        .map(str::to_owned);
                }
            }
            if cursor.is_none() {
                break;
            }
        }
        let rendered = py_json_dumps(&render_prompt_list(&prompts));
        Value::String(self.maybe_spillover(&rendered, tool_call_id))
    }

    /// The error output of a failed request, as `tool_error` writes it.
    fn failure_text(&self, index: usize, failure: Failure) -> Value {
        let error = match failure {
            Failure::Rpc(error) => json!({"error": error.call_failed()}),
            Failure::Down => json!({"error": format!(
                "MCP server '{}' transport is down; reconnect requested. Do NOT retry this tool immediately — give it a few seconds to come back.",
                self.servers[index].config.name
            )}),
            Failure::Uncertain { reconnected } => json!({
                "error": format!(
                    "The MCP transport session to '{}' expired while this write-capable call was in flight, so the outcome is UNKNOWN — the operation may or may not have taken effect server-side. It was NOT automatically retried to avoid a duplicate side effect. The connection has {}. Verify whether the operation took effect (e.g. with a read-only tool) before re-invoking it.",
                    self.servers[index].config.name,
                    if reconnected { "been re-established" } else { "not recovered yet" }
                ),
                "outcome_uncertain": true,
                "server": self.servers[index].config.name,
            }),
        };
        tool_text(&error)
    }

    /// Check persistent stdio servers between turns and transparently reconnect any idle child
    /// process that exited unexpectedly. Busy servers (serving an in-flight call on another turn)
    /// and healthy HTTP/SSE servers are left untouched without blocking.
    pub async fn ensure_healthy(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for index in 0..self.servers.len() {
            let Ok(mut transport) = self.servers[index].transport.try_lock() else {
                // Currently busy executing a call for another turn — alive and in use.
                continue;
            };
            let dead = match &mut *transport {
                Transport::Stdio(conn) => !conn.is_alive(),
                Transport::Http(_) | Transport::Sse(_) => false,
            };
            if !dead {
                continue;
            }
            let config = self.servers[index].config.clone();
            match spawn_stdio(&config) {
                Ok(conn) => {
                    let mut fresh = Transport::Stdio(Box::new(conn));
                    match handshake(&mut fresh, &config).await {
                        Ok(discovered) => {
                            *transport = fresh;
                            drop(transport);
                            self.register(index, discovered);
                        }
                        Err(error) => {
                            self.tools
                                .write()
                                .unwrap_or_else(PoisonError::into_inner)
                                .retain(|tool| tool.server != index);
                            warnings.push(format!(
                                "MCP server {:?} exited and failed to reconnect: {error}",
                                config.name
                            ));
                        }
                    }
                }
                Err(error) => {
                    self.tools
                        .write()
                        .unwrap_or_else(PoisonError::into_inner)
                        .retain(|tool| tool.server != index);
                    warnings.push(format!(
                        "MCP server {:?} exited and failed to restart: {error}",
                        config.name
                    ));
                }
            }
        }
        warnings
    }

    /// Close stdin, give servers a moment to exit, then stop whatever remains.
    pub async fn shutdown(&self) {
        for server in &self.servers {
            server.transport.lock().await.close().await;
        }
        self.tools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

struct CachedMcp {
    configs: Vec<McpServerConfig>,
    profile: Option<PathBuf>,
    mcp_result_size_chars: usize,
    clients: Arc<McpClients>,
}

impl CachedMcp {
    fn matches(
        &self,
        configs: &[McpServerConfig],
        profile: Option<&Path>,
        mcp_result_size_chars: usize,
    ) -> bool {
        self.profile.as_deref() == profile
            && self.mcp_result_size_chars == mcp_result_size_chars
            && self.configs.len() == configs.len()
            && self
                .configs
                .iter()
                .zip(configs.iter())
                .all(|(a, b)| a.same_config(b))
    }
}

/// Persistent pool of [`McpClients`] shared across turns in an interactive CLI session or
/// local session owner daemon.
///
/// Reuses live server connections while config is unchanged without serializing turn startup or
/// calls across unrelated servers. When config changes while another turn is still using the
/// previous clients, the old clients are detached rather than force-closed so the in-flight turn
/// finishes cleanly and `Drop` stops the old servers once the last reference is released.
#[derive(Default)]
pub struct McpPool {
    cached: Mutex<Option<CachedMcp>>,
}

impl McpPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return warm MCP clients matching `(configs, profile, mcp_result_size_chars)`, starting or
    /// replacing connections only when needed.
    pub async fn acquire(
        &self,
        configs: &[McpServerConfig],
        profile: Option<PathBuf>,
        mcp_result_size_chars: usize,
    ) -> (Arc<McpClients>, Vec<String>) {
        if configs.is_empty() {
            let old = self.cached.lock().await.take();
            if let Some(old) = old {
                if let Ok(exclusive) = Arc::try_unwrap(old.clients) {
                    exclusive.shutdown().await;
                }
            }
            return (Arc::new(McpClients::default()), Vec::new());
        }

        // Hold the pool lock across miss-detection + connect so two simultaneous turns with a new
        // config do not double-spawn servers, while cache hits release the lock immediately before
        // calling `ensure_healthy()` (which uses non-blocking `try_lock()` per server).
        let mut guard = self.cached.lock().await;
        if let Some(entry) = guard.as_ref() {
            if entry.matches(configs, profile.as_deref(), mcp_result_size_chars) {
                let clients = Arc::clone(&entry.clients);
                drop(guard);
                let warnings = clients.ensure_healthy().await;
                return (clients, warnings);
            }
        }

        let old = guard.take();
        let (started, warnings) =
            McpClients::start_with_options(configs, profile.clone(), mcp_result_size_chars).await;
        let clients = Arc::new(started);
        *guard = Some(CachedMcp {
            configs: configs.to_vec(),
            profile,
            mcp_result_size_chars,
            clients: Arc::clone(&clients),
        });
        drop(guard);

        if let Some(old) = old {
            if let Ok(exclusive) = Arc::try_unwrap(old.clients) {
                exclusive.shutdown().await;
            }
        }
        (clients, warnings)
    }

    /// Shut down any cached MCP servers cleanly (closing stdio pipes and ending HTTP sessions).
    pub async fn shutdown(&self) {
        let old = self.cached.lock().await.take();
        if let Some(old) = old {
            old.clients.shutdown().await;
        }
    }
}

/// A tool's text in `content` blocks of the answer: those with `text` set and not empty.
fn text_blocks(result: &Value) -> Vec<&str> {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"].as_str() == Some("text"))
        .filter_map(|block| block["text"].as_str())
        .filter(|text| !text.is_empty())
        .collect()
}

/// `_render_call_tool_result(result, server, for_model=True)`: the JSON text a `tools/call`
/// answer becomes for the model.
fn render_call_tool_result(result: &Value, server: &str, profile: Option<&Path>) -> String {
    render_call_tool_result_with_cap(result, server, profile, HARD_RESULT_CAP_CHARS)
}

/// Public entry point with a configurable hard cap for reference-corpus testing.
pub fn render_call_tool_result_with_cap(
    result: &Value,
    server: &str,
    profile: Option<&Path>,
    max_chars: usize,
) -> String {
    if result["isError"].as_bool() == Some(true) {
        return error_result(result);
    }
    let (mut text, usable) = render_content_blocks(result, server, profile);
    text = truncate_head_tail(&text, max_chars, "MCP RESULT");
    let mut structured = capped_structured_content(result, max_chars);
    let meta = strip_reserved_meta_keys(&result["_meta"]);
    if let Some(value) = structured.clone().filter(|_| usable > 0) {
        if value.is_string()
            || text_blocks(result)
                .iter()
                .any(|text_block| text_is_value(text_block, &value))
        {
            structured = None;
        } else if let Some(collapsed) = collapse_sdk_wrapped_copy(result, &value, usable) {
            if let Some(list) = collapsed {
                text = truncate_head_tail(&list, max_chars, "MCP RESULT");
            }
            structured = None;
        }
    }
    if structured.is_none() && meta.is_none() {
        return py_json_dumps(&json!({"result": text}));
    }
    // Key order is part of the output: "result" leads when there is text, otherwise "_meta" precedes it.
    let mut payload = Map::new();
    if !text.is_empty() {
        payload.insert("result".into(), json!(text));
    }
    if let Some(structured) = structured {
        let key = if text.is_empty() {
            "result"
        } else {
            "structuredContent"
        };
        payload.insert(key.into(), structured);
    }
    if let Some(meta) = meta {
        payload.insert("_meta".into(), meta);
    }
    payload.entry("result").or_insert(json!(text));
    py_json_dumps(&Value::Object(payload))
}

/// `_capped_structured_content`: return `structuredContent` when it fits `max_chars`, or degrade
/// it to a head+tail truncated JSON string when it exceeds the hard cap.
fn capped_structured_content(result: &Value, max_chars: usize) -> Option<Value> {
    let structured = result
        .get("structuredContent")
        .filter(|value| value.is_object())?;
    let as_json = py_json_dumps(structured);
    if as_json.chars().count() > max_chars {
        Some(Value::String(truncate_head_tail(
            &as_json,
            max_chars,
            "MCP RESULT",
        )))
    } else {
        Some(structured.clone())
    }
}

/// An `isError` answer: `tool_error(_sanitize_error(<capped text>))`.
fn error_result(result: &Value) -> String {
    let text: String = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            block["text"]
                .as_str()
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    block["resource"]["text"]
                        .as_str()
                        .filter(|text| !text.is_empty())
                })
        })
        .map(strip_unicode_tags)
        .collect();
    let text = if text.is_empty() {
        "MCP tool returned an error".to_owned()
    } else {
        text
    };
    let text = truncate_head_tail(&text, HARD_RESULT_CAP_CHARS, "MCP RESULT");
    let text = strip_unicode_tags(&CREDENTIALS.replace_all(&text, "[REDACTED]"));
    py_json_dumps(&json!({"error": bound_error_text(&text)}))
}

/// `registry._bound_error_text`: an error body for the model stays within 2048 characters.
fn bound_error_text(text: &str) -> String {
    match text.char_indices().nth(MAX_TOOL_ERROR_CHARS) {
        None => text.to_owned(),
        Some((end, _)) => format!("{}… [truncated]", &text[..end]),
    }
}

/// `_render_content_blocks`: the text of the blocks and how many of them carry something real.
/// Images and audio are cached under `<profile>/cache/{images,audio}` as `MEDIA:<path>`;
/// embedded binary resources are cached under `<profile>/cache/documents`.
fn render_content_blocks(result: &Value, server: &str, profile: Option<&Path>) -> (String, usize) {
    let mut parts: Vec<String> = Vec::new();
    let mut usable = 0;
    for block in result["content"].as_array().into_iter().flatten() {
        let kind = block["type"].as_str().unwrap_or("");
        if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
            parts.push(strip_unicode_tags(text));
            if !text.trim().is_empty() {
                usable += 1;
            }
            continue;
        }
        let (rendered, counts_as_usable) = match kind {
            "text" | "" => (String::new(), false),
            "image" => (cache_mcp_image_block(block, profile), true),
            "audio" => (cache_mcp_audio_block(block, profile), true),
            "resource_link" => (render_resource_link(block, server), true),
            "resource" => (render_embedded_resource(&block["resource"], profile), true),
            other => (dropped_block_notice(block, other), false),
        };
        if !rendered.is_empty() {
            parts.push(rendered);
            if counts_as_usable {
                usable += 1;
            }
        }
    }
    (parts.join("\n"), usable)
}

/// `_render_mcp_resource_block` for a link: the uri exactly, and the tool that reads it.
fn render_resource_link(block: &Value, server: &str) -> String {
    let Some(uri) = block["uri"].as_str().filter(|uri| !uri.is_empty()) else {
        return String::new();
    };
    let name = strip_unicode_tags(block["name"].as_str().unwrap_or(""));
    let mime = strip_unicode_tags(block["mimeType"].as_str().unwrap_or(""));
    let mut details = format!("uri={uri}");
    if !name.is_empty() {
        details.push_str(&format!(", name={name}"));
    }
    if !mime.is_empty() {
        details.push_str(&format!(", mimeType={mime}"));
    }
    format!(
        "[MCP resource link: {details} — fetch it with {}]",
        mcp_prefixed_tool_name(server, "read_resource")
    )
}

/// An embedded resource: its text, or a cached binary document notice.
fn render_embedded_resource(resource: &Value, profile: Option<&Path>) -> String {
    if let Some(text) = resource["text"].as_str() {
        return strip_unicode_tags(text);
    }
    if let Some(blob) = resource["blob"].as_str() {
        let mime = strip_unicode_tags(resource["mimeType"].as_str().unwrap_or(""));
        let uri = strip_unicode_tags(resource["uri"].as_str().unwrap_or(""));
        return cache_mcp_resource_blob(blob, &uri, &mime, profile);
    }
    String::new()
}

/// `_render_mcp_dropped_block_notice`: an unsupported block is named, not silently lost.
fn dropped_block_notice(block: &Value, kind: &str) -> String {
    let mut details = vec![format!("type={}", strip_unicode_tags(kind))];
    if let Some(mime) = block["mimeType"].as_str().filter(|mime| !mime.is_empty()) {
        details.push(format!("mimeType={}", strip_unicode_tags(mime)));
    }
    let uri = block["uri"]
        .as_str()
        .or_else(|| block["resource"]["uri"].as_str())
        .filter(|uri| !uri.is_empty());
    if let Some(uri) = uri {
        details.push(format!("uri={uri}"));
    }
    if let Some(size) = block["size"].as_i64().or(block["sizeInBytes"].as_i64()) {
        details.push(format!("size={size}"));
    }
    if let Some(name) = block["name"].as_str().filter(|name| !name.is_empty()) {
        details.push(format!("name={}", strip_unicode_tags(name)));
    }
    format!(
        "[MCP content dropped: unsupported block ({})]",
        details.join(", ")
    )
}

/// `_collapse_sdk_wrapped_copy`: the Python SDK wraps a bare return value as `{"result": value}`
/// next to a `content` block that carries it. `None`: nothing to collapse. `Some(None)`: the copy
/// is in the text already. `Some(Some(text))`: a list whose items arrived as one block each is
/// sent once, as JSON.
fn collapse_sdk_wrapped_copy(
    result: &Value,
    structured: &Value,
    usable: usize,
) -> Option<Option<String>> {
    let object = structured.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let value = object.get("result")?;
    let texts = text_blocks(result);
    if texts.iter().any(|text| text_is_value(text, value)) {
        return Some(None);
    }
    let items = value.as_array()?;
    (texts.len() == items.len()
        && items.len() == usable
        && texts
            .iter()
            .zip(items)
            .all(|(text, item)| text_is_value(text, item)))
    .then(|| Some(py_json_dumps(value)))
}

/// `_text_is_value`: the text is the value itself (a string) or its JSON.
fn text_is_value(text: &str, value: &Value) -> bool {
    if value.as_str() == Some(text) {
        return true;
    }
    serde_json::from_str::<Value>(text).is_ok_and(|parsed| type_stable_equal(&parsed, value))
}

/// `==` without Python's bool/int coercion, and with 1 and 1.0 apart (they are different types there).
fn type_stable_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, item)| {
                    right
                        .get(key)
                        .is_some_and(|other| type_stable_equal(item, other))
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(a, b)| type_stable_equal(a, b))
        }
        (Value::Number(left), Value::Number(right)) => {
            left.is_f64() == right.is_f64() && left == right
        }
        _ => left == right,
    }
}

/// `_strip_reserved_meta_keys`: `_meta` without the keys of the protocol's own namespaces
/// (`modelcontextprotocol` or `mcp` as a label that is not the last one before the slash).
fn strip_reserved_meta_keys(meta: &Value) -> Option<Value> {
    let kept: Map<String, Value> = meta
        .as_object()?
        .iter()
        .filter(|(key, _)| !is_reserved_meta_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!kept.is_empty()).then_some(Value::Object(kept))
}

fn is_reserved_meta_key(key: &str) -> bool {
    let Some(slash) = key.find('/').filter(|slash| *slash > 0) else {
        return false;
    };
    let labels: Vec<&str> = key[..slash].split('.').collect();
    labels
        .iter()
        .enumerate()
        .any(|(i, label)| matches!(*label, "modelcontextprotocol" | "mcp") && i + 1 < labels.len())
}

/// `truncate_head_tail`: 40 percent of the budget from the start, the rest from the end, a notice between.
fn truncate_head_tail(text: &str, max_chars: usize, label: &str) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let head = (max_chars as f64 * 0.4) as usize;
    let tail = max_chars - head;
    let omitted = total - head - tail;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(total - tail).collect();
    format!(
        "{start}\n\n... [{label} TRUNCATED - {} chars omitted out of {} total] ...\n\n{end}",
        grouped(omitted),
        grouped(total)
    )
}

/// Python's `{:,}`.
fn grouped(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

static CREDENTIALS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(concat!(
        r#"(?i)(?:ghp_[A-Za-z0-9_]{1,255}|sk-[A-Za-z0-9_-](?:\.?[A-Za-z0-9_-]){0,254}|Bearer\s+\S+"#,
        r#"|(?:token|key|API_KEY|password|secret)=[^\s&,;"']{1,255})"#
    ))
    .expect("the credential pattern is valid")
});

/// `_MCP_HARD_RESULT_CAP_CHARS`: the text of one MCP result the reference keeps before it reports an error.
const HARD_RESULT_CAP_CHARS: usize = 2_000_000;
/// `registry._MAX_TOOL_ERROR_CHARS`.
const MAX_TOOL_ERROR_CHARS: usize = 2048;

/// `mcp_prefixed_tool_name`: `mcp__<server>__<tool>`, each part with everything outside
/// `[A-Za-z0-9_]` replaced by `_`. A name over the providers' 64 characters is cut to 55 and
/// ends with `_` and the first 8 hex digits of its SHA-256.
pub fn mcp_prefixed_tool_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.chars().count() <= 64 {
        return full;
    }
    let digest = sha256(full.as_bytes());
    let suffix: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let head: String = full.chars().take(64 - 9).collect();
    format!("{head}_{suffix}")
}

/// SHA-256 (FIPS 180-4), for the hash suffix of a long name.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// A payload as the reference writes it: `json.dumps(payload, ensure_ascii=False)`.
fn tool_text(payload: &Value) -> Value {
    Value::String(py_json_dumps(payload))
}

/// `strip_unicode_tags`: invisible Unicode tag characters carry hidden text, so they are
/// dropped, except in a valid emoji tag sequence (a black flag, tag letters, a cancel tag).
fn strip_unicode_tags(text: &str) -> String {
    let is_tag = |c: char| ('\u{E0000}'..='\u{E007F}').contains(&c);
    if !text.chars().any(is_tag) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1F3F4}' {
            let mut end = i + 1;
            while end < chars.len() && ('\u{E0020}'..='\u{E007E}').contains(&chars[end]) {
                end += 1;
            }
            if end > i + 1 && chars.get(end) == Some(&'\u{E007F}') {
                out.extend(&chars[i..=end]);
                i = end + 1;
                continue;
            }
        }
        if !is_tag(chars[i]) {
            out.push(chars[i]);
        }
        i += 1;
    }
    out
}

/// `_render_resource_list`: the fields of each resource in the order uri, name, description,
/// mimeType; the description and the type only when they are not empty. The uri is sent back
/// to `read_resource`, so it stays byte for byte.
fn render_resource_list(resources: &[Value]) -> Value {
    let entries: Vec<Value> = resources
        .iter()
        .map(|resource| {
            let mut entry = Map::new();
            if let Some(uri) = resource["uri"].as_str() {
                entry.insert("uri".into(), json!(uri));
            }
            if let Some(name) = resource["name"].as_str() {
                entry.insert("name".into(), json!(strip_unicode_tags(name)));
            }
            for (field, key) in [("description", "description"), ("mimeType", "mimeType")] {
                if let Some(text) = resource[field].as_str().filter(|text| !text.is_empty()) {
                    entry.insert(key.into(), json!(strip_unicode_tags(text)));
                }
            }
            Value::Object(entry)
        })
        .collect();
    json!({"resources": entries})
}

/// `_render_read_resource`: the text of each content block, one per line. Binary blobs are cached
/// into `<profile>/cache/documents/` when a profile is available.
fn render_read_resource(result: &Value, profile: Option<&Path>) -> Value {
    let mut parts = Vec::new();
    for block in result["contents"].as_array().into_iter().flatten() {
        if let Some(text) = block["text"].as_str() {
            parts.push(strip_unicode_tags(text));
        } else if let Some(blob) = block["blob"].as_str() {
            let mime = strip_unicode_tags(block["mimeType"].as_str().unwrap_or(""));
            let uri = strip_unicode_tags(block["uri"].as_str().unwrap_or(""));
            parts.push(cache_mcp_resource_blob(blob, &uri, &mime, profile));
        }
    }
    json!({"result": parts.join("\n")})
}

/// `_render_prompt_list`: prompt and argument names stay verbatim; prose descriptions are stripped.
fn render_prompt_list(prompts: &[Value]) -> Value {
    let entries: Vec<Value> = prompts
        .iter()
        .map(|prompt| {
            let mut entry = Map::new();
            if let Some(name) = prompt["name"].as_str() {
                entry.insert("name".into(), json!(name));
            }
            if let Some(desc) = prompt["description"].as_str().filter(|s| !s.is_empty()) {
                entry.insert("description".into(), json!(strip_unicode_tags(desc)));
            }
            if let Some(args) = prompt["arguments"].as_array().filter(|a| !a.is_empty()) {
                let rendered_args: Vec<Value> = args
                    .iter()
                    .map(|arg| {
                        let mut a = Map::new();
                        if let Some(name) = arg["name"].as_str() {
                            a.insert("name".into(), json!(name));
                        }
                        if let Some(desc) = arg["description"].as_str().filter(|s| !s.is_empty()) {
                            a.insert("description".into(), json!(strip_unicode_tags(desc)));
                        }
                        if let Some(req) = arg.get("required") {
                            if !req.is_null() {
                                a.insert("required".into(), req.clone());
                            }
                        }
                        Value::Object(a)
                    })
                    .collect();
                entry.insert("arguments".into(), Value::Array(rendered_args));
            }
            Value::Object(entry)
        })
        .collect();
    json!({"prompts": entries})
}

/// Public helper for reference-corpus testing.
pub fn render_prompt_list_json(prompts: &[Value]) -> String {
    py_json_dumps(&render_prompt_list(prompts))
}

/// `_render_get_prompt`: messages array followed by optional non-empty description.
fn render_get_prompt(result: &Value) -> Value {
    let messages: Vec<Value> = result["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|msg| {
            let mut entry = Map::new();
            if let Some(role) = msg["role"].as_str() {
                entry.insert("role".into(), json!(strip_unicode_tags(role)));
            }
            if let Some(content) = msg.get("content") {
                let text = if let Some(t) = content.get("text").and_then(Value::as_str) {
                    strip_unicode_tags(t)
                } else if let Some(s) = content.as_str() {
                    strip_unicode_tags(s)
                } else {
                    strip_unicode_tags(&py_json_dumps(content))
                };
                entry.insert("content".into(), json!(text));
            }
            Value::Object(entry)
        })
        .collect();
    let mut payload = Map::new();
    payload.insert("messages".into(), Value::Array(messages));
    if let Some(desc) = result["description"].as_str().filter(|s| !s.is_empty()) {
        payload.insert("description".into(), json!(strip_unicode_tags(desc)));
    }
    Value::Object(payload)
}

/// Public helper for reference-corpus testing.
pub fn render_get_prompt_json(result: &Value) -> String {
    py_json_dumps(&render_get_prompt(result))
}

fn spawn_stdio(config: &McpServerConfig) -> Result<Connection, String> {
    if config.command.is_empty() {
        return Err("server needs a command or a url".into());
    }
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs(&config.env);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start command: {error}"))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("cannot attach to command streams".into());
    };
    Ok(Connection {
        group: child.id(),
        child,
        stdin: Arc::new(Mutex::new(Some(stdin))),
        stdout: BufReader::new(stdout),
        next_id: 0,
    })
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/mcp_reference.json")).unwrap()
    }

    #[test]
    fn names_are_sanitized_and_head_tail_truncation_respects_characters() {
        assert_eq!(sanitize("my-server.v2"), "my_server_v2");
        let text = "аааааааааа";
        let cut = truncate_head_tail(text, 5, "MCP RESULT");
        assert!(cut.starts_with(
            "аа\n\n... [MCP RESULT TRUNCATED - 5 chars omitted out of 10 total] ...\n\nааа"
        ));
    }

    #[test]
    fn tag_characters_are_dropped_but_emoji_tag_sequences_stay() {
        assert_eq!(strip_unicode_tags("x\u{E0041}y"), "xy");
        let scotland = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}";
        assert_eq!(strip_unicode_tags(scotland), scotland);
        // A flag without its cancel tag is not a sequence.
        assert_eq!(
            strip_unicode_tags("\u{1F3F4}\u{E0067}z\u{E007F}"),
            "\u{1F3F4}z"
        );
        assert_eq!(strip_unicode_tags("plain"), "plain");
    }

    #[test]
    fn resource_output_matches_the_reference_for_recorded_answers() {
        let corpus = corpus();
        let cases = &corpus["resources"];
        let list = &cases["render_list_cases"];
        let rendered = render_resource_list(list["wire"].as_array().unwrap());
        assert_eq!(
            py_json_dumps(&rendered),
            list["output"].as_str().unwrap(),
            "list"
        );
        assert_eq!(
            py_json_dumps(&render_resource_list(&[])),
            cases["empty_list"].as_str().unwrap()
        );
        assert_eq!(
            py_json_dumps(&render_read_resource(&json!({"contents": []}), None)),
            cases["empty_read"].as_str().unwrap()
        );
        // Text blocks (also an empty one) join with newlines; a hidden tag is dropped.
        let read = &cases["render_read_cases"];
        let output = py_json_dumps(&render_read_resource(&read["wire"], None));
        let recorded = read["output"].as_str().unwrap();
        // Without a profile dir, binary blobs report document cache unavailable.
        let text = |full: &str| full.split("\\n[MCP").next().unwrap().to_owned();
        assert_eq!(text(&output), text(recorded));
        assert!(
            output.contains("3 bytes, application/octet-stream) but document cache unavailable")
        );
    }

    #[test]
    fn call_results_match_the_reference_for_recorded_answers() {
        let corpus = corpus();
        let results = corpus["calls"]["results"].as_object().unwrap();
        assert!(results.len() > 30);
        for (case, entry) in results {
            let output = render_call_tool_result(
                &serde_json::from_str(entry["wire"].as_str().unwrap()).unwrap(),
                "fx",
                None,
            );
            let recorded = entry["output"].as_str().unwrap();
            if case == "resource_blob" {
                // Without a profile dir, binary blobs report document cache unavailable.
                assert_eq!(
                    output,
                    r#"{"result": "[MCP embedded resource received (3 bytes, x/y) but document cache unavailable in this process]"}"#
                );
                continue;
            }
            assert_eq!(output, recorded, "{case}");
        }
    }

    #[test]
    fn answers_to_a_call_are_written_with_python_separators() {
        let block = json!({"content": [{"type": "text", "text": "x"}]});
        assert_eq!(
            render_call_tool_result(&block, "s", None),
            r#"{"result": "x"}"#
        );
        // An image with invalid magic bytes renders empty (matching upstream).
        let image =
            json!({"content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]});
        assert_eq!(
            render_call_tool_result(&image, "s", None),
            r#"{"result": ""}"#
        );
        // A block type nobody knows is named instead of vanishing.
        let odd = json!({"content": [{"type": "hologram", "mimeType": "x/y", "name": "n"}]});
        assert_eq!(
            render_call_tool_result(&odd, "s", None),
            r#"{"result": "[MCP content dropped: unsupported block (type=hologram, mimeType=x/y, name=n)]"}"#
        );
    }

    #[test]
    fn tool_names_match_the_reference_for_recorded_servers_and_tools() {
        let corpus = corpus();
        let names = corpus["calls"]["names"].as_array().unwrap();
        assert!(names
            .iter()
            .any(|name| name["name"].as_str().unwrap().len() == 64));
        for entry in names {
            assert_eq!(
                mcp_prefixed_tool_name(
                    entry["server"].as_str().unwrap(),
                    entry["tool"].as_str().unwrap()
                ),
                entry["name"].as_str().unwrap(),
                "{entry}"
            );
        }
    }

    #[test]
    fn sha256_matches_the_known_values() {
        let hex = |data: &[u8]| -> String {
            sha256(data)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        };
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Two blocks of input: the padding spills into a second block.
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn messages_the_model_sees_match_the_reference() {
        let corpus = corpus();
        let wire = &corpus["wire"];
        for (case, error) in [
            ("session_expired", RpcError::SessionExpired),
            (
                "drop",
                RpcError::Server("SSE stream ended without a response".into()),
            ),
            (
                "resume_fail",
                RpcError::Server(
                    "SSE stream ended and reconnection attempts were exhausted".into(),
                ),
            ),
        ] {
            let recorded = wire[case]["error"]["message"]
                .as_str()
                .or_else(|| wire[case]["outcome"]["error"]["message"].as_str())
                .unwrap();
            assert_eq!(error.call_failed(), recorded, "{case}");
        }
        assert_eq!(
            RpcError::Server("Resource not found: file:///missing".into()).call_failed(),
            corpus["resources"]["missing"]["message"].as_str().unwrap()
        );
        let template = corpus["messages"]["timeout_template"].as_str().unwrap();
        assert!(
            template.contains("MCP call timed out after")
                && template.contains("(configured timeout:")
        );
        let timeout = RpcError::Timeout {
            method: "tools/call".into(),
            elapsed: Duration::from_millis(1040),
            limit: Duration::from_secs(1),
        };
        assert_eq!(
            timeout.call_failed(),
            "MCP call failed: TimeoutError: MCP call timed out after 1.0s (configured timeout: 1.0s)"
        );
    }

    #[tokio::test]
    async fn mcp_pool_handles_empty_and_config_equality() {
        let pool = McpPool::new();
        let (clients, warnings) = pool.acquire(&[], None, DEFAULT_MCP_RESULT_SIZE_CHARS).await;
        assert!(warnings.is_empty());
        assert!(clients.schemas().is_empty());
        pool.shutdown().await;

        let a = McpServerConfig {
            name: "demo".into(),
            command: "python3".into(),
            args: vec!["server.py".into()],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            transport: None,
            timeout_secs: 300.0,
            connect_timeout_secs: 15,
            resources: true,
            prompts: true,
        };
        let mut b = a.clone();
        assert!(a.same_config(&b));
        b.timeout_secs = 60.0;
        assert!(!a.same_config(&b));
    }
}
