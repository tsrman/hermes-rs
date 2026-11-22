//! MCP client over stdio and Streamable HTTP.
//!
//! Follows `tools/mcp_tool*.py` and the `mcp` SDK it calls (2.3.0), as recorded by
//! `compat/create_mcp_corpus.py`:
//!
//! * a call that is abandoned (its turn is cancelled, or its time runs out) sends
//!   `notifications/cancelled` with `{requestId, reason: "caller cancelled"}`; `initialize` never does;
//! * a Streamable HTTP request that gets a 404 for a session the client holds fails with
//!   "Session terminated". The server is then initialized again and tools are listed again. A
//!   tool with `readOnlyHint: true` and the resource tools are retried once; any other tool is
//!   not, because it may have run, and the model gets an `outcome_uncertain` error instead;
//! * an event stream that ends before the answer is resumed with a GET and `Last-Event-ID`
//!   (the `retry` field or one second apart, two failed attempts in a row end the call);
//! * a server that advertises `resources` (and has not turned them off with `tools.resources`)
//!   gets `mcp_<server>_list_resources` and `mcp_<server>_read_resource`.
//!
//! Not ported: prompts and event utilities, OAuth, sampling, elicitation, the SSE transport,
//! the circuit breaker, stdio respawn, and saving binary resources to a document cache.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use hermes_core::sse::SseDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::session_search::py_json_dumps;

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 100_000;
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
    /// Streamable HTTP endpoint.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
    /// `tools.resources`: offer the resource tools of a server that advertises resources.
    #[serde(default = "enabled")]
    pub resources: bool,
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

    fn kill_group(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(pid) = self.group.and_then(|pid| i32::try_from(pid).ok()) {
            if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
        let _ = self.child.start_kill();
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

/// A server reached over stdio or Streamable HTTP.
enum Transport {
    Stdio(Connection),
    Http(HttpConnection),
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
        }
    }

    async fn notify(&mut self, method: &str, limit: Duration) -> Result<(), RpcError> {
        let message = json!({"jsonrpc": "2.0", "method": method});
        match self {
            Self::Stdio(connection) => connection.send(&message).await,
            Self::Http(connection) => connection.post(&message, limit).await.map(|_| ()),
        }
    }

    /// Close stdin and give a process a moment to exit, or end the HTTP session.
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

impl HttpConnection {
    fn new(config: &McpServerConfig, url: &str) -> Result<Self, String> {
        use reqwest::header::{HeaderName, HeaderValue};

        let parsed = reqwest::Url::parse(url).map_err(|_| "invalid server URL".to_owned())?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("server URL must use http or https".into());
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &config.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid header name {name:?}"))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| format!("invalid value for header {name}"))?;
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Remote,
    ListResources,
    ReadResource,
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
    transport: Transport,
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

#[derive(Default)]
pub struct McpClients {
    servers: Vec<Server>,
    tools: Vec<Tool>,
}

/// What `initialize` and discovery learned about a server.
struct Discovered {
    resources: bool,
    tools: Vec<Value>,
}

/// Initialize a server and list its tools.
async fn handshake(
    transport: &mut Transport,
    config: &McpServerConfig,
) -> Result<Discovered, String> {
    let connect_limit = Duration::from_secs(config.connect_timeout_secs);
    let request_limit = Duration::from_secs(config.timeout_secs);
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
        .notify("notifications/initialized", request_limit)
        .await
        .map_err(|error| error.to_string())?;
    let tools = list_all(transport, "tools/list", "tools", request_limit)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Discovered {
        // The family is served exactly when the capability is present (the reference's gate).
        resources: config.resources && init["capabilities"]["resources"].is_object(),
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
        let mut clients = Self::default();
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
            Some(url) => Transport::Http(HttpConnection::new(config, url)?),
            None => Transport::Stdio(spawn_stdio(config)?),
        };
        // On error the transport drops here, which stops a stdio child's process group.
        let discovered = handshake(&mut transport, config).await?;
        let index = self.servers.len();
        self.servers.push(Server {
            config: config.clone(),
            transport,
        });
        self.register(index, discovered);
        Ok(())
    }

    /// Replace the tools of server `index` with what discovery found.
    fn register(&mut self, index: usize, discovered: Discovered) {
        self.tools.retain(|tool| tool.server != index);
        let config_name = self.servers[index].config.name.clone();
        let prefix = format!("mcp_{}_", sanitize(&config_name));
        for tool in discovered.tools {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            let exposed = format!("{prefix}{}", sanitize(name));
            if self.owns(&exposed) {
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
            self.tools.push(Tool {
                exposed,
                server: index,
                kind: Kind::Remote,
                name: name.to_owned(),
                schema,
                read_only: tool["annotations"]["readOnlyHint"].as_bool() == Some(true),
            });
        }
        if !discovered.resources {
            return;
        }
        // A tool of the server with the same name wins: the utilities are optional.
        let utilities = [
            (
                Kind::ListResources,
                "list_resources",
                format!("List available resources from MCP server '{config_name}'"),
                json!({"type": "object", "properties": {}}),
            ),
            (
                Kind::ReadResource,
                "read_resource",
                format!("Read a resource by URI from MCP server '{config_name}'"),
                json!({
                    "type": "object",
                    "properties": {"uri": {"type": "string", "description": "URI of the resource to read"}},
                    "required": ["uri"]
                }),
            ),
        ];
        for (kind, key, description, parameters) in utilities {
            let exposed = format!("{prefix}{key}");
            if self.owns(&exposed) {
                continue;
            }
            let schema = json!({
                "type": "function",
                "function": {"name": exposed, "description": description, "parameters": parameters}
            });
            self.tools.push(Tool {
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
        self.tools.iter().map(|tool| tool.schema.clone()).collect()
    }

    pub fn owns(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.exposed == name)
    }

    /// Initialize server `index` again after its session expired and list its tools again.
    async fn reconnect(&mut self, index: usize) -> bool {
        let server = &mut self.servers[index];
        let Transport::Http(connection) = &mut server.transport else {
            return false;
        };
        connection.reset_session();
        let config = server.config.clone();
        let outcome = timeout(RECONNECT_LIMIT, handshake(&mut server.transport, &config)).await;
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
        &mut self,
        index: usize,
        method: &str,
        params: Value,
        replay: bool,
    ) -> Result<Value, Failure> {
        let limit = Duration::from_secs(self.servers[index].config.timeout_secs);
        if matches!(&self.servers[index].transport, Transport::Http(c) if c.down)
            && !self.reconnect(index).await
        {
            return Err(Failure::Down);
        }
        let first = self.servers[index]
            .transport
            .request(method, params.clone(), limit)
            .await;
        if !matches!(first, Err(RpcError::SessionExpired)) {
            return first.map_err(Failure::Rpc);
        }
        let reconnected = self.reconnect(index).await;
        if !replay {
            return Err(Failure::Uncertain { reconnected });
        }
        if reconnected {
            // Only an answer counts: a failed repeat leaves the first failure as the error.
            if let Ok(value) = self.servers[index]
                .transport
                .request(method, params, limit)
                .await
            {
                return Ok(value);
            }
        }
        Err(Failure::Rpc(RpcError::SessionExpired))
    }

    /// Call a tool and return the result in the shape of a local tool output.
    pub async fn call(&mut self, name: &str, arguments: &str) -> Value {
        let Some(tool) = self.tools.iter().find(|tool| tool.exposed == name) else {
            return json!({"error": format!("Unknown tool: {name}")});
        };
        let (index, kind, remote, read_only) =
            (tool.server, tool.kind, tool.name.clone(), tool.read_only);
        let arguments = match serde_json::from_str::<Value>(arguments) {
            Ok(value) if value.is_object() => value,
            Ok(_) | Err(_) if arguments.trim().is_empty() => json!({}),
            _ => return json!({"error": "Tool arguments must be a JSON object"}),
        };
        match kind {
            Kind::Remote => {
                let params = json!({"name": remote, "arguments": arguments});
                match self
                    .request_recovering(index, "tools/call", params, read_only)
                    .await
                {
                    Err(failure) => self.failure_text(index, failure, false),
                    Ok(result) => tool_result(&result),
                }
            }
            Kind::ListResources => self.list_resources(index).await,
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
                    Err(failure) => self.failure_text(index, failure, true),
                    Ok(result) => tool_text(&render_read_resource(&result)),
                }
            }
        }
    }

    /// `resources/list`, all pages. The pages are not replayed as a whole: the first failure is the answer.
    async fn list_resources(&mut self, index: usize) -> Value {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
            match self
                .request_recovering(index, "resources/list", params, true)
                .await
            {
                Err(failure) => return self.failure_text(index, failure, true),
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
        tool_text(&render_resource_list(&resources))
    }

    /// The error output of a failed request. A utility tool answers in the reference's JSON
    /// text, a remote tool with an object.
    fn failure_text(&self, index: usize, failure: Failure, as_text: bool) -> Value {
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
        if as_text {
            tool_text(&error)
        } else {
            error
        }
    }

    /// Close stdin, give servers a moment to exit, then stop whatever remains.
    pub async fn shutdown(&mut self) {
        for server in &mut self.servers {
            server.transport.close().await;
        }
        self.servers.clear();
        self.tools.clear();
    }
}

/// The output of a `tools/call` answer.
fn tool_result(result: &Value) -> Value {
    let mut parts = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("text") => parts.push(item["text"].as_str().unwrap_or("").to_owned()),
            Some(other) => parts.push(format!("[{other} content omitted]")),
            None => {}
        }
    }
    let mut text = parts.join("\n");
    if text.is_empty() && !result["structuredContent"].is_null() {
        text = result["structuredContent"].to_string();
    }
    truncate(&mut text, MAX_RESULT_BYTES);
    if result["isError"].as_bool() == Some(true) {
        json!({"error": text})
    } else {
        json!({"result": text})
    }
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

/// `_render_read_resource`: the text of each content block, one per line. A binary block is not
/// saved anywhere (the reference caches it as a document); the model is told its size.
fn render_read_resource(result: &Value) -> Value {
    let mut parts = Vec::new();
    for block in result["contents"].as_array().into_iter().flatten() {
        if let Some(text) = block["text"].as_str() {
            parts.push(strip_unicode_tags(text));
        } else if let Some(blob) = block["blob"].as_str() {
            let mime = strip_unicode_tags(block["mimeType"].as_str().unwrap_or(""));
            let uri = strip_unicode_tags(block["uri"].as_str().unwrap_or(""));
            parts.push(match decoded_len(blob) {
                Some(bytes) => format!(
                    "[MCP embedded resource received ({bytes} bytes, {}) but document cache unavailable in this process]",
                    if mime.is_empty() { "unknown type" } else { &mime }
                ),
                None => format!(
                    "[MCP embedded resource could not be decoded: {}]",
                    if mime.is_empty() { &uri } else { &mime }
                ),
            });
        }
    }
    json!({"result": parts.join("\n")})
}

/// The size of base64 data once decoded, or `None` when it cannot be decoded.
fn decoded_len(data: &str) -> Option<usize> {
    let symbols = data
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        .count();
    (symbols % 4 != 1).then_some(symbols * 3 / 4)
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

fn truncate(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/mcp_reference.json")).unwrap()
    }

    #[test]
    fn names_are_sanitized_and_truncation_respects_characters() {
        assert_eq!(sanitize("my-server.v2"), "my_server_v2");
        let mut text = "ааа".to_owned();
        truncate(&mut text, 3);
        assert_eq!(text, "а\n[truncated]");
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
            py_json_dumps(&render_read_resource(&json!({"contents": []}))),
            cases["empty_read"].as_str().unwrap()
        );
        // Text blocks (also an empty one) join with newlines; a hidden tag is dropped.
        let read = &cases["render_read_cases"];
        let output = py_json_dumps(&render_read_resource(&read["wire"]));
        let recorded = read["output"].as_str().unwrap();
        // The reference saves a binary block to a document cache, which is not ported.
        let text = |full: &str| full.split("\\n[MCP").next().unwrap().to_owned();
        assert_eq!(text(&output), text(recorded));
        assert!(
            output.contains("3 bytes, application/octet-stream) but document cache unavailable")
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
}
