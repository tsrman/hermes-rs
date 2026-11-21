//! `browser_cdp`: raw Chrome DevTools Protocol commands to the configured browser.
//!
//! Mirrors `tools/browser_cdp_tool.py`. The endpoint comes from `BROWSER_CDP_URL` or
//! `browser.cdp_url`; a live `/browser connect` and the supervisor that serves `frame_id` calls
//! are not ported. Every call passes the private-page guard first and every result is redacted,
//! except the spans the protocol declares to be opaque base64.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::browser_url_policy::{
    expression_targets_private_url, literal_targets_metadata, url_blocked, url_literals,
};
use crate::redact::{redact_sensitive_text, WALK_DEPTH_MARKER, WALK_MAX_DEPTH};

pub(crate) const BROWSER_CDP: &str = "browser_cdp";
const CDP_DOCS_URL: &str = "https://chromedevtools.github.io/devtools-protocol/";
const MAX_ERROR_CHARS: usize = 2048;
const ERROR_TRUNCATION_MARKER: &str = "… [truncated]";

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/browser_cdp.json"))
            .unwrap_or_default()
    })
}

/// Where the browser is and whether it may reach private networks.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BrowserSettings {
    /// `BROWSER_CDP_URL`, else `browser.cdp_url`.
    pub cdp_url: Option<String>,
    /// `browser.allow_private_urls`.
    pub allow_private_urls: bool,
}

impl BrowserSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        secrets: &HashMap<String, String>,
    ) -> Self {
        let browser = config.get("browser");
        let from_env = std::env::var("BROWSER_CDP_URL")
            .ok()
            .or_else(|| secrets.get("BROWSER_CDP_URL").cloned())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let from_config = browser
            .and_then(|browser| browser.get("cdp_url"))
            .and_then(serde_yaml::Value::as_str)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let allow_private_urls = match browser.and_then(|browser| browser.get("allow_private_urls"))
        {
            Some(serde_yaml::Value::String(text)) => {
                matches!(
                    text.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }
            Some(serde_yaml::Value::Bool(flag)) => *flag,
            Some(serde_yaml::Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
            Some(serde_yaml::Value::Sequence(items)) => !items.is_empty(),
            Some(serde_yaml::Value::Mapping(map)) => !map.is_empty(),
            _ => false,
        };
        Self {
            cdp_url: from_env.or(from_config),
            allow_private_urls,
        }
    }

    /// The reference offers the tool only when a static endpoint is configured.
    pub(crate) fn offers_cdp(&self) -> bool {
        self.cdp_url.is_some()
    }
}

// ── Redaction of results ─────────────────────────────────────────────────────────────────────────

/// Browser/target inspection and tab management that never reads a page body, cookies, DOM or
/// storage; stays usable on a private page.
const PRIVATE_PAGE_ALLOWED: [&str; 10] = [
    "Browser.getVersion",
    "Target.getTargets",
    "Target.attachToTarget",
    "Target.detachFromTarget",
    "Target.createTarget",
    "Target.activateTarget",
    "Target.closeTarget",
    "Page.navigate",
    "Page.reload",
    "Page.stopLoading",
];

type PathList = Vec<Vec<String>>;

fn paths(list: &[&[&str]]) -> PathList {
    list.iter()
        .map(|path| path.iter().map(|part| (*part).to_owned()).collect())
        .collect()
}

/// Result paths that are always opaque base64 (protocol-declared binary).
fn always_binary(method: &str) -> PathList {
    paths(match method {
        "Page.captureScreenshot" | "Page.printToPDF" => &[&["data"]],
        "Network.streamResourceContent" => &[&["bufferedData"]],
        "HeadlessExperimental.beginFrame" => &[&["screenshotData"]],
        "CacheStorage.requestCachedResponse" => &[&["response", "body"]],
        _ => &[],
    })
}

/// Result paths that are opaque base64 only beside a `base64Encoded` that is exactly `true`.
fn flagged_binary(method: &str) -> PathList {
    paths(match method {
        "Network.getResponseBody" | "Fetch.getResponseBody" => &[&["body"]],
        "IO.read" => &[&["data"]],
        "Network.getRequestPostData" => &[&["postData"]],
        _ => &[],
    })
}

/// Redact browser-originated result text; opaque bytes stay byte-identical. The exemptions come
/// only from the calling method's spec as exact result paths, and descend only into the matching
/// subtree, so `base64Encoded` is honoured solely as a sibling on the trusted carrier object.
pub(crate) fn redact_cdp_output(
    value: &Value,
    always: &[Vec<String>],
    flagged: &[Vec<String>],
    depth: usize,
) -> Value {
    if depth >= WALK_MAX_DEPTH {
        return Value::String(WALK_DEPTH_MARKER.to_owned());
    }
    match value {
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_cdp_output(item, &[], &[], depth + 1))
                .collect(),
        ),
        Value::Object(map) => {
            let base64_flagged = map.get("base64Encoded") == Some(&Value::Bool(true));
            let leaf = |list: &[Vec<String>], key: &str| {
                list.iter().any(|path| path.len() == 1 && path[0] == key)
            };
            let descend = |list: &[Vec<String>], key: &str| -> PathList {
                list.iter()
                    .filter(|path| path.len() > 1 && path[0] == key)
                    .map(|path| path[1..].to_vec())
                    .collect()
            };
            let mut redacted = Map::new();
            for (key, item) in map {
                let opaque = leaf(always, key) || (leaf(flagged, key) && base64_flagged);
                let out = if opaque && item.is_string() {
                    item.clone()
                } else {
                    redact_cdp_output(
                        item,
                        &descend(always, key),
                        &descend(flagged, key),
                        depth + 1,
                    )
                };
                redacted.insert(redact_sensitive_text(key), out);
            }
            Value::Object(redacted)
        }
        other => other.clone(),
    }
}

fn redact_method_result(method: &str, result: &Value) -> Value {
    redact_cdp_output(result, &always_binary(method), &flagged_binary(method), 0)
}

// ── Errors ───────────────────────────────────────────────────────────────────────────────────────

fn bound_error(message: &str) -> String {
    if message.chars().count() <= MAX_ERROR_CHARS {
        return message.to_owned();
    }
    let head: String = message.chars().take(MAX_ERROR_CHARS).collect();
    format!("{head}{ERROR_TRUNCATION_MARKER}")
}

fn tool_error(message: &str) -> Value {
    json!({"error": bound_error(message)})
}

fn tool_error_with(message: &str, extra: &[(&str, &str)]) -> Value {
    let mut out = Map::new();
    out.insert("error".into(), Value::String(bound_error(message)));
    for (key, value) in extra {
        out.insert((*key).into(), Value::String((*value).into()));
    }
    Value::Object(out)
}

fn blocked(message: &str, method: &str) -> Value {
    tool_error_with(message, &[("method", method), ("cdp_docs", CDP_DOCS_URL)])
}

fn docs_error(message: &str) -> Value {
    tool_error_with(message, &[("cdp_docs", CDP_DOCS_URL)])
}

/// Python's `repr` of a JSON value, for error text that quotes a protocol error object.
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => py_str_repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| format!("{}: {}", py_str_repr(key), py_repr(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

// ── Endpoint ─────────────────────────────────────────────────────────────────────────────────────

/// Normalize a CDP endpoint into a concrete websocket URL: full `ws://.../devtools/browser/...`
/// endpoints pass through; HTTP discovery roots and bare `ws://host:port` resolve through
/// `/json/version` (`webSocketDebuggerUrl`), falling back to the raw value.
pub(crate) async fn resolve_endpoint(cdp_url: &str) -> String {
    let raw = cdp_url.trim();
    if raw.is_empty() {
        return String::new();
    }
    let lowered = raw.to_lowercase();
    if lowered.contains("/devtools/browser/") {
        return raw.to_owned();
    }
    let mut discovery_url = raw.to_owned();
    if lowered.starts_with("ws://") || lowered.starts_with("wss://") {
        let after_colon = raw.splitn(3, ':').nth(2).unwrap_or("");
        let port = raw.trim_end_matches('/').rsplit(':').next().unwrap_or("");
        let bare = raw.matches(':').count() == 2
            && !port.is_empty()
            && port.chars().all(|c| c.is_ascii_digit())
            && !after_colon.contains('/');
        if !bare {
            return raw.to_owned();
        }
        let scheme = if lowered.starts_with("ws://") {
            "http://"
        } else {
            "https://"
        };
        discovery_url = format!("{scheme}{}", raw.split_once("://").map_or("", |(_, r)| r));
    }
    let version_url = if discovery_url.to_lowercase().ends_with("/json/version") {
        discovery_url
    } else {
        format!("{}/json/version", discovery_url.trim_end_matches('/'))
    };
    let mut client = reqwest::Client::builder().timeout(Duration::from_secs(10));
    if reqwest::Url::parse(&version_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
    {
        client = client.no_proxy();
    }
    let payload: Option<Value> = async {
        let response = client.build().ok()?.get(&version_url).send().await.ok()?;
        response.error_for_status().ok()?.json().await.ok()
    }
    .await;
    payload
        .as_ref()
        .and_then(|payload| payload.get("webSocketDebuggerUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map_or_else(|| raw.to_owned(), str::to_owned)
}

// ── Guard ────────────────────────────────────────────────────────────────────────────────────────

/// `_current_page_private_url`'s verdict when the target list cannot be read: refuse.
const PAGE_URL_UNVERIFIED: &str = "<unverified>";
/// Browser-internal pages carry no web origin a worker or browser-level call could borrow.
const ORIGINLESS_PAGE_PREFIXES: [&str; 7] = [
    "about:",
    "chrome:",
    "chrome-untrusted:",
    "chrome-search:",
    "devtools:",
    "edge:",
    "brave:",
];

fn text_field(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(other) if truthy(other) => other.to_string(),
        _ => String::new(),
    }
}

/// The private/internal URL a call outside the allow-list would reach (or
/// [`PAGE_URL_UNVERIFIED`]), else `None`. Fail-closed: read from `Target.getTargets` on the same
/// endpoint. Only a `page` target whose own URL is known passes on that URL alone; any other
/// call runs with some page's origin, so it is refused while ANY page target is private.
async fn current_page_private_url(endpoint: &str, target_id: Option<&str>) -> Option<String> {
    if endpoint.is_empty() {
        return Some(PAGE_URL_UNVERIFIED.to_owned());
    }
    let infos = match cdp_call(endpoint, "Target.getTargets", &json!({}), None, 5.0).await {
        Ok(result) => result
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(_) => return Some(PAGE_URL_UNVERIFIED.to_owned()),
    };
    let acted = target_id.and_then(|id| {
        infos
            .iter()
            .find(|info| info.get("targetId").and_then(Value::as_str) == Some(id))
    });
    let url = acted
        .map(|info| text_field(info, "url"))
        .unwrap_or_default();
    if !url.is_empty() && url_blocked(&url).await {
        return Some(url);
    }
    if !url.is_empty()
        && acted
            .and_then(|info| info.get("type"))
            .and_then(Value::as_str)
            == Some("page")
    {
        return None;
    }
    for info in &infos {
        if info.get("type").and_then(Value::as_str) != Some("page") {
            continue;
        }
        let page = text_field(info, "url");
        if !page.is_empty()
            && !ORIGINLESS_PAGE_PREFIXES
                .iter()
                .any(|prefix| page.starts_with(prefix))
            && url_blocked(&page).await
        {
            return Some(page);
        }
    }
    None
}

/// Methods whose URL parameters the browser never fetches: input events, blocklist and
/// interception patterns, dialog text, cookie scopes.
fn non_fetching(method: &str) -> bool {
    method.starts_with("Input.")
        || matches!(
            method,
            "Network.setBlockedURLs"
                | "Network.setRequestInterception"
                | "Fetch.enable"
                | "Page.handleJavaScriptDialog"
                | "Network.setCookie"
                | "Network.setCookies"
                | "Network.deleteCookies"
                | "Storage.setCookies"
        )
}

/// Every URL a call's params name: each `url` value whole, plus the `http(s)://` and
/// protocol-relative `//host` literals in every string at any depth.
fn param_url_literals(value: &Value, key: &str, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if key == "url" && !text.trim().is_empty() {
                out.push(text.trim().to_owned());
            }
            out.extend(url_literals(text, true));
        }
        Value::Object(map) => {
            for (child_key, child) in map {
                param_url_literals(child, child_key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                param_url_literals(item, key, out);
            }
        }
        _ => {}
    }
}

fn fetched_param_urls(method: &str, params: &Value) -> Vec<String> {
    if non_fetching(method) {
        return Vec::new();
    }
    let mut all = Vec::new();
    param_url_literals(params, "", &mut all);
    let mut seen = std::collections::HashSet::new();
    all.retain(|literal| seen.insert(literal.clone()));
    all
}

/// Apply the browser SSRF/private-page guard to a raw CDP call. A page whose URL cannot be
/// verified as public refuses page-reading methods.
async fn private_guard(
    method: &str,
    params: &Value,
    endpoint: &str,
    target_id: Option<&str>,
    allow_private_urls: bool,
) -> Option<Value> {
    // With a CDP override the backend is never trusted as local.
    let guard_active = !allow_private_urls;
    let method_guarded = matches!(
        method,
        "Page.navigate" | "Target.createTarget" | "Runtime.evaluate"
    );
    if guard_active {
        let literal = match method {
            "Page.navigate" => navigate_target(params).await,
            "Target.createTarget" => new_tab_target(params).await,
            "Runtime.evaluate" => {
                expression_targets_private_url(&text_field(params, "expression")).await
            }
            _ => None,
        };
        if let Some(literal) = literal {
            let message = match method {
                "Page.navigate" => format!(
                    "Blocked: CDP Page.navigate target is a private or internal address ({literal})."
                ),
                "Target.createTarget" => format!(
                    "Blocked: CDP Target.createTarget url is a private or internal address ({literal})."
                ),
                _ => format!(
                    "Blocked: CDP Runtime.evaluate expression targets a private or internal address ({literal})."
                ),
            };
            return Some(blocked(&message, method));
        }
    }
    // The cloud-metadata floor applies with the private guard on or off.
    let literals = fetched_param_urls(method, params);
    for literal in &literals {
        if literal_targets_metadata(literal).await {
            return Some(blocked(
                &format!("Blocked: CDP {method} targets a cloud metadata endpoint ({literal})."),
                method,
            ));
        }
    }
    if !guard_active {
        return None;
    }
    if !method_guarded {
        // A public page does not make an unlisted method's own URL params safe.
        for literal in &literals {
            let lowered = literal.to_lowercase();
            if (lowered.starts_with("http://") || lowered.starts_with("https://"))
                && url_blocked(literal).await
            {
                return Some(blocked(
                    &format!(
                        "Blocked: CDP {method} params target a private or internal address ({literal})."
                    ),
                    method,
                ));
            }
        }
    }
    if !PRIVATE_PAGE_ALLOWED.contains(&method) {
        match current_page_private_url(endpoint, target_id).await {
            Some(url) if url == PAGE_URL_UNVERIFIED => {
                return Some(blocked(
                    &format!(
                        "Blocked: could not read the browser's targets to verify that raw CDP method {} cannot reach a private or internal page.",
                        py_str_repr(method)
                    ),
                    method,
                ));
            }
            Some(url) => {
                return Some(blocked(
                    &format!(
                        "Blocked: page URL targets a private or internal address ({url}). Raw CDP method {} could expose private page content or state.",
                        py_str_repr(method)
                    ),
                    method,
                ));
            }
            None => {}
        }
    }
    None
}

async fn navigate_target(params: &Value) -> Option<String> {
    let target = text_field(params, "url").trim().to_owned();
    (!target.is_empty() && url_blocked(&target).await).then_some(target)
}

/// A blank tab reaches nothing; any other url is held to `Page.navigate`'s rule.
async fn new_tab_target(params: &Value) -> Option<String> {
    if text_field(params, "url").trim().to_lowercase() == "about:blank" {
        return None;
    }
    navigate_target(params).await
}

// ── Transport ────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum CdpError {
    Timeout(String),
    Runtime(String),
    WebSocket(String),
    Other(String),
}

fn parse_unbounded(text: &str) -> Option<Value> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let value = Value::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(value)
}

/// One CDP call. With `target_id`, `Target.attachToTarget(flatten=True)` multiplexes a page-level
/// session over the browser-level socket; without it `method` runs at browser level.
async fn cdp_call(
    ws_url: &str,
    method: &str,
    params: &Value,
    target_id: Option<&str>,
    timeout: f64,
) -> Result<Value, CdpError> {
    let limit = Duration::from_secs_f64(timeout);
    // No frame or message size limit: results such as DOM.getDocument can be large.
    let config = WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None);
    let connecting = tokio_tungstenite::connect_async_with_config(ws_url, Some(config), false);
    let (mut socket, _) = match tokio::time::timeout(limit, connecting).await {
        Err(_) => {
            return Err(CdpError::Timeout(
                "timed out during opening handshake".into(),
            ))
        }
        Ok(Err(tokio_tungstenite::tungstenite::Error::Io(error))) => {
            return Err(CdpError::Other(format!("OSError: {error}")))
        }
        Ok(Err(error)) => return Err(CdpError::WebSocket(error.to_string())),
        Ok(Ok(connected)) => connected,
    };
    let mut next_id: u64 = 1;

    let mut send = |request: Value, what: String| {
        let call_id = next_id;
        next_id += 1;
        let mut frame = Map::new();
        frame.insert("id".into(), json!(call_id));
        if let Value::Object(fields) = request {
            frame.extend(fields);
        }
        (call_id, what, Value::Object(frame))
    };

    let mut session_id: Option<String> = None;
    if let Some(target) = target_id {
        let (call_id, what, frame) = send(
            json!({"method": "Target.attachToTarget", "params": {"targetId": target, "flatten": true}}),
            format!("attaching to target {target}"),
        );
        let reply = round_trip(&mut socket, call_id, &what, frame, limit).await?;
        if let Some(error) = reply.get("error") {
            return Err(CdpError::Runtime(format!(
                "Target.attachToTarget failed: {}",
                py_repr(error)
            )));
        }
        session_id = reply
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if session_id.is_none() {
            return Err(CdpError::Runtime(
                "Target.attachToTarget did not return a sessionId".into(),
            ));
        }
    }
    let mut request = json!({"method": method, "params": params});
    if let Some(id) = &session_id {
        request["sessionId"] = json!(id);
    }
    let (call_id, what, frame) = send(request, format!("waiting for response to {method}"));
    let reply = round_trip(&mut socket, call_id, &what, frame, limit).await?;
    if let Some(error) = reply.get("error") {
        return Err(CdpError::Runtime(format!("CDP error: {}", py_repr(error))));
    }
    Ok(reply.get("result").cloned().unwrap_or_else(|| json!({})))
}

/// Send `frame` and wait for the response carrying its id, ignoring events and other responses.
async fn round_trip<S>(
    socket: &mut S,
    call_id: u64,
    what: &str,
    frame: Value,
    limit: Duration,
) -> Result<Value, CdpError>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    socket
        .send(Message::text(frame.to_string()))
        .await
        .map_err(|error| CdpError::WebSocket(error.to_string()))?;
    let deadline = Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CdpError::Timeout(format!("Timed out {what}")));
        }
        let next = match tokio::time::timeout(remaining, socket.next()).await {
            // The reference's `wait_for` expires without a message.
            Err(_) => return Err(CdpError::Timeout(String::new())),
            Ok(next) => next,
        };
        let text = match next {
            None | Some(Ok(Message::Close(_))) => {
                return Err(CdpError::WebSocket("connection closed".into()))
            }
            Some(Err(error)) => return Err(CdpError::WebSocket(error.to_string())),
            Some(Ok(Message::Text(text))) => text.to_string(),
            Some(Ok(Message::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
            Some(Ok(_)) => continue,
        };
        let Some(message) = parse_unbounded(&text) else {
            return Err(CdpError::Other("JSONDecodeError: invalid JSON".into()));
        };
        if message.get("id").and_then(Value::as_u64) == Some(call_id) {
            return Ok(message);
        }
    }
}

// ── The tool ─────────────────────────────────────────────────────────────────────────────────────

fn read_timeout(raw: Option<&Value>) -> f64 {
    let parsed = match raw {
        Some(Value::Number(number)) => number.as_f64().filter(|n| *n != 0.0),
        Some(Value::Bool(true)) => Some(1.0),
        Some(Value::String(text)) if !text.is_empty() => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed.map_or(30.0, |seconds| {
        if seconds.is_nan() {
            1.0
        } else {
            seconds.clamp(1.0, 300.0)
        }
    })
}

/// Run `browser_cdp` with the model's arguments (a JSON object).
pub(crate) async fn call(settings: &BrowserSettings, task_id: &str, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let method_value = args.get("method").cloned().unwrap_or_else(|| json!(""));
    let params_value = args.get("params").cloned().unwrap_or(Value::Null);
    let target_id = args
        .get("target_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let frame_id = args
        .get("frame_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let timeout_raw = args.get("timeout");
    let raw_url = settings.cdp_url.as_deref().unwrap_or("");
    let method = method_value.as_str().unwrap_or("");

    if let Some(frame_id) = frame_id {
        // An OOPIF's frame id is its iframe target id, so the guard judges that frame.
        let endpoint = resolve_endpoint(raw_url).await;
        let params = if params_value.is_object() {
            params_value.clone()
        } else {
            json!({})
        };
        if let Some(refusal) = private_guard(
            method,
            &params,
            &endpoint,
            Some(frame_id),
            settings.allow_private_urls,
        )
        .await
        {
            return refusal;
        }
        // The supervisor that serves frame routing is not ported: no task has one attached.
        return tool_error(&format!(
            "No CDP supervisor is attached for task={}. Call browser_exec or /browser connect first so the supervisor can attach; Page.getFrameTree lists the frame ids you can pass here.",
            py_str_repr(task_id)
        ));
    }

    if method.is_empty() {
        return docs_error("'method' is required (e.g. 'Target.getTargets')");
    }
    let endpoint = resolve_endpoint(raw_url).await;
    if endpoint.is_empty() {
        return docs_error("No CDP endpoint is available. Run '/browser connect' to attach to a running Chrome, Brave, Chromium, or Edge browser, or set 'browser.cdp_url' in config.yaml.");
    }
    if !endpoint.starts_with("ws://") && !endpoint.starts_with("wss://") {
        return tool_error(&format!(
            "CDP endpoint is not a WebSocket URL: {}. Expected ws://... or wss://... — the /browser connect resolver should have rewritten this. Check that a Chromium-family browser is actually listening on the debug port.",
            py_str_repr(&endpoint)
        ));
    }
    let call_params = if truthy(&params_value) {
        params_value.clone()
    } else {
        json!({})
    };
    if !call_params.is_object() {
        return tool_error(&format!(
            "'params' must be an object/dict, got {}",
            python_type_name(&call_params)
        ));
    }
    if let Some(refusal) = private_guard(
        method,
        &call_params,
        &endpoint,
        target_id,
        settings.allow_private_urls,
    )
    .await
    {
        return refusal;
    }
    let timeout = read_timeout(timeout_raw);
    match cdp_call(&endpoint, method, &call_params, target_id, timeout).await {
        Ok(result) => {
            let mut payload = Map::new();
            payload.insert("success".into(), json!(true));
            payload.insert("method".into(), json!(method));
            payload.insert("result".into(), redact_method_result(method, &result));
            if let Some(target) = target_id {
                payload.insert("target_id".into(), json!(target));
            }
            Value::Object(payload)
        }
        Err(CdpError::Timeout(detail)) => tool_error_with(
            &format!("CDP call timed out after {timeout:?}s: {detail}"),
            &[("method", method)],
        ),
        Err(CdpError::Runtime(detail)) => tool_error_with(&detail, &[("method", method)]),
        Err(CdpError::WebSocket(detail)) => tool_error_with(
            &format!(
                "WebSocket error talking to CDP at {endpoint}: {detail}. The browser may have disconnected — try '/browser connect' again."
            ),
            &[("method", method)],
        ),
        Err(CdpError::Other(detail)) => tool_error_with(
            &format!("Unexpected error: {detail}"),
            &[("method", method)],
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::net::TcpListener;

    use super::*;

    fn parse(text: &str) -> Value {
        parse_unbounded(text).expect("valid JSON")
    }

    fn fixture() -> Value {
        parse(include_str!("../tests/fixtures/browser_cdp_cases.json"))
    }

    /// A scripted CDP endpoint, as in `compat/create_browser_cdp_fixture.py`.
    async fn fake_browser(targets: Value, script: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://{}/devtools/browser/fixture",
            listener.local_addr().unwrap()
        );
        let frames = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&frames);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (targets, script, recorded) =
                    (targets.clone(), script.clone(), Arc::clone(&recorded));
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = socket.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let request = parse(&text);
                        let mut frame = Map::new();
                        for key in ["method", "params", "sessionId"] {
                            if let Some(value) = request.get(key) {
                                frame.insert(key.into(), value.clone());
                            }
                        }
                        recorded.lock().unwrap().push(Value::Object(frame));
                        let method = request["method"].as_str().unwrap_or("");
                        let reply = script.get(method).cloned().or_else(|| match method {
                            "Target.getTargets" => {
                                Some(json!({"result": {"targetInfos": targets}}))
                            }
                            "Target.attachToTarget" => Some(json!({"result": {"sessionId": "S1"}})),
                            _ => Some(json!({"result": {}})),
                        });
                        let reply = reply.unwrap();
                        if reply.get("hang") == Some(&json!(true)) {
                            continue;
                        }
                        let id = request["id"].as_u64().unwrap();
                        if reply.get("noise") == Some(&json!(true)) {
                            let event =
                                json!({"method": "Page.frameNavigated", "params": {"frame": {}}});
                            let stray = json!({"id": id + 100, "result": {"stray": true}});
                            for noise in [event, stray] {
                                let _ = socket.send(Message::text(noise.to_string())).await;
                            }
                        }
                        let mut body = Map::new();
                        body.insert("id".into(), json!(id));
                        if let Some(error) = reply.get("error") {
                            body.insert("error".into(), error.clone());
                        } else if reply.get("raw").is_none() {
                            body.insert(
                                "result".into(),
                                reply.get("result").cloned().unwrap_or_else(|| json!({})),
                            );
                        }
                        let _ = socket
                            .send(Message::text(Value::Object(body).to_string()))
                            .await;
                    }
                });
            }
        });
        (url, frames)
    }

    #[test]
    fn url_literals_match_the_reference() {
        for case in fixture()["url_literals"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            for (key, relative) in [("plain", false), ("relative", true)] {
                assert_eq!(
                    json!(url_literals(text, relative)),
                    case[key],
                    "{text:?} ({key})"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_metadata_floor_matches_the_reference() {
        for case in fixture()["metadata"].as_array().unwrap() {
            let literal = case["literal"].as_str().unwrap();
            assert_eq!(
                literal_targets_metadata(literal).await,
                case["blocked"].as_bool().unwrap(),
                "{literal:?}"
            );
        }
    }

    #[test]
    fn redaction_matches_the_reference() {
        for case in fixture()["redact"].as_array().unwrap() {
            let method = case["method"].as_str().unwrap();
            assert_eq!(
                redact_method_result(method, &case["value"]),
                case["result"],
                "{method} {}",
                case["value"]
                    .to_string()
                    .chars()
                    .take(80)
                    .collect::<String>()
            );
        }
    }

    #[tokio::test]
    async fn calls_match_the_reference() {
        let fixture = fixture();
        for case in fixture["calls"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (url, frames) = fake_browser(case["targets"].clone(), case["script"].clone()).await;
            let settings = BrowserSettings {
                cdp_url: Some(url),
                allow_private_urls: case["config"]["allow_private_urls"] == json!(true),
            };
            let result = call(&settings, "default", &case["arguments"].to_string()).await;
            assert_eq!(result, case["result"], "{name}: result");
            let seen = Value::Array(frames.lock().unwrap().clone());
            assert_eq!(seen, case["frames"], "{name}: frames");
        }
    }

    #[tokio::test]
    async fn endpoints_resolve_as_the_reference_does() {
        // A full browser endpoint and anything that is not a bare host:port pass through.
        for raw in [
            "ws://127.0.0.1:9/devtools/browser/abc",
            "ws://127.0.0.1:9/",
            "ws://host/path",
            "wss://host.test/devtools/page/1",
        ] {
            assert_eq!(resolve_endpoint(raw).await, raw);
        }
        assert_eq!(resolve_endpoint("  ").await, "");
        // A failed discovery falls back to the raw value.
        assert_eq!(
            resolve_endpoint("ws://127.0.0.1:9").await,
            "ws://127.0.0.1:9"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9").await,
            "http://127.0.0.1:9"
        );
    }

    #[test]
    fn settings_read_the_environment_before_the_config() {
        let config: serde_yaml::Value =
            serde_yaml::from_str("browser: {cdp_url: ' ws://cfg:1 ', allow_private_urls: 'On'}")
                .unwrap();
        let mut secrets = HashMap::new();
        let settings = BrowserSettings::from_config(&config, &secrets);
        assert_eq!(settings.cdp_url.as_deref(), Some("ws://cfg:1"));
        assert!(settings.allow_private_urls);
        secrets.insert("BROWSER_CDP_URL".into(), "ws://env:2".into());
        let settings = BrowserSettings::from_config(&config, &secrets);
        if std::env::var("BROWSER_CDP_URL").is_err() {
            assert_eq!(settings.cdp_url.as_deref(), Some("ws://env:2"));
        }
        assert!(
            !BrowserSettings::from_config(&serde_yaml::Value::Null, &HashMap::new())
                .allow_private_urls
        );
    }
}
