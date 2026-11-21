//! MCP client over stdio: newline-delimited JSON-RPC with local child processes.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use hermes_core::sse::SseDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 100_000;
const MAX_LIST_PAGES: usize = 20;
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
}

struct Connection {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Process group id, kept after the child is reaped so grandchildren can still be stopped.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    group: Option<u32>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Connection {
    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("server is closed")?;
        let mut line = message.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| format!("write failed: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("write failed: {error}"))
    }

    async fn read_message(&mut self) -> Result<Value, String> {
        loop {
            let mut line = String::new();
            let read = (&mut self.stdout)
                .take(MAX_LINE_BYTES)
                .read_line(&mut line)
                .await
                .map_err(|error| format!("read failed: {error}"))?;
            if read == 0 {
                return Err("server closed the connection".into());
            }
            if !line.ends_with('\n') {
                return Err("server message is too large".into());
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line).map_err(|_| "server sent invalid JSON".to_owned());
        }
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let wait = async {
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
                    return Err(text.to_owned());
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        timeout(limit, wait)
            .await
            .map_err(|_| format!("{method} timed out"))?
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
    ) -> Result<Value, String> {
        match self {
            Self::Stdio(connection) => connection.request(method, params, limit).await,
            Self::Http(connection) => connection.request(method, params, limit).await,
        }
    }

    async fn notify(&mut self, method: &str, limit: Duration) -> Result<(), String> {
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
                connection.stdin = None;
                let _ = timeout(Duration::from_secs(2), connection.child.wait()).await;
                connection.kill_group();
            }
            Self::Http(connection) => connection.end_session().await,
        }
    }
}

struct HttpConnection {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    session: Option<String>,
    next_id: u64,
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
            next_id: 0,
        })
    }

    async fn post(
        &mut self,
        message: &Value,
        limit: Duration,
    ) -> Result<reqwest::Response, String> {
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
        let response = timeout(limit, builder.send())
            .await
            .map_err(|_| "request timed out".to_owned())?
            .map_err(|error| format!("request failed: {}", error.without_url()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status.as_u16() == 404 && self.session.is_some() {
            return Err("session expired".into());
        }
        Err(format!("HTTP {}", status.as_u16()))
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let wait = async {
            let mut response = self.post(&message, limit).await?;
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
            if kind.starts_with("text/event-stream") {
                let mut decoder = SseDecoder::new();
                loop {
                    let chunk = response
                        .chunk()
                        .await
                        .map_err(|error| format!("read failed: {}", error.without_url()))?;
                    let Some(chunk) = chunk else {
                        return Err("stream ended without a response".into());
                    };
                    for data in decoder
                        .push(&chunk)
                        .map_err(|_| "server sent an invalid event stream".to_owned())?
                    {
                        let message: Value = serde_json::from_str(&data)
                            .map_err(|_| "server sent invalid JSON".to_owned())?;
                        if let Some(result) = self.handle(message, id, limit).await {
                            return result;
                        }
                    }
                }
            } else if kind.starts_with("application/json") {
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|error| format!("read failed: {}", error.without_url()))?
                {
                    if body.len() + chunk.len() > MAX_LINE_BYTES as usize {
                        return Err("server message is too large".into());
                    }
                    body.extend_from_slice(&chunk);
                }
                let parsed: Value = serde_json::from_slice(&body)
                    .map_err(|_| "server sent invalid JSON".to_owned())?;
                let batch = match parsed {
                    Value::Array(messages) => messages,
                    single => vec![single],
                };
                for message in batch {
                    if let Some(result) = self.handle(message, id, limit).await {
                        return result;
                    }
                }
                Err("response did not answer the request".into())
            } else {
                Err("server answered with an unsupported content type".into())
            }
        };
        let result = timeout(limit, wait)
            .await
            .map_err(|_| format!("{method} timed out"))?;
        if method == "initialize" {
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

    /// The outcome if `message` answers request `id`. Notifications are ignored and requests
    /// from the server are refused with a separate POST.
    async fn handle(
        &mut self,
        message: Value,
        id: u64,
        limit: Duration,
    ) -> Option<Result<Value, String>> {
        if message.get("method").is_some() {
            if let Some(request_id) = message.get("id") {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32601, "message": "Method not found"}
                });
                let _ = self.post(&reply, limit).await;
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
            return Some(Err(text.to_owned()));
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
        let _ = timeout(Duration::from_secs(2), request).await;
    }
}

struct Tool {
    exposed: String,
    server: usize,
    name: String,
    schema: Value,
}

struct Server {
    config: McpServerConfig,
    transport: Transport,
}

#[derive(Default)]
pub struct McpClients {
    servers: Vec<Server>,
    tools: Vec<Tool>,
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
        let connect_limit = Duration::from_secs(config.connect_timeout_secs);
        let request_limit = Duration::from_secs(config.timeout_secs);
        let handshake = async {
            transport
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "hermes", "version": env!("CARGO_PKG_VERSION")}
                    }),
                    connect_limit,
                )
                .await?;
            transport
                .notify("notifications/initialized", request_limit)
                .await?;
            let mut tools = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..MAX_LIST_PAGES {
                let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
                let page = transport
                    .request("tools/list", params, request_limit)
                    .await?;
                tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
                cursor = page["nextCursor"].as_str().map(str::to_owned);
                if cursor.is_none() {
                    break;
                }
            }
            Ok::<_, String>(tools)
        };
        // On error the transport drops here, which stops a stdio child's process group.
        let listed = handshake.await?;
        let index = self.servers.len();
        for tool in listed {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            let exposed = format!("mcp_{}_{}", sanitize(&config.name), sanitize(name));
            if self.tools.iter().any(|known| known.exposed == exposed) {
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
                name: name.to_owned(),
                schema,
            });
        }
        self.servers.push(Server {
            config: config.clone(),
            transport,
        });
        Ok(())
    }

    pub fn schemas(&self) -> Vec<Value> {
        self.tools.iter().map(|tool| tool.schema.clone()).collect()
    }

    pub fn owns(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.exposed == name)
    }

    /// Call a tool and return the result in the shape of a local tool output.
    pub async fn call(&mut self, name: &str, arguments: &str) -> Value {
        let Some(tool) = self.tools.iter().find(|tool| tool.exposed == name) else {
            return json!({"error": format!("Unknown tool: {name}")});
        };
        let arguments = match serde_json::from_str::<Value>(arguments) {
            Ok(value) if value.is_object() => value,
            Ok(_) | Err(_) if arguments.trim().is_empty() => json!({}),
            _ => return json!({"error": "Tool arguments must be a JSON object"}),
        };
        let server = &mut self.servers[tool.server];
        let limit = Duration::from_secs(server.config.timeout_secs);
        let result = server
            .transport
            .request(
                "tools/call",
                json!({"name": tool.name, "arguments": arguments}),
                limit,
            )
            .await;
        match result {
            Err(error) => json!({"error": format!("MCP call failed: {error}")}),
            Ok(result) => {
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
        stdin: Some(stdin),
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

    #[test]
    fn names_are_sanitized_and_truncation_respects_characters() {
        assert_eq!(sanitize("my-server.v2"), "my_server_v2");
        let mut text = "ааа".to_owned();
        truncate(&mut text, 3);
        assert_eq!(text, "а\n[truncated]");
    }
}
