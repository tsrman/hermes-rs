//! Keyless free-tier web search/extract over public MCP/REST endpoints
//! (Exa, Parallel, Firecrawl, Keenable) with round-robin ring failover.
//!
//! Port of `plugins/web/keyless_mcp.py`, verified against
//! `crates/hermes-runtime/tests/fixtures/web_remainder_cases.json`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::web_tools::{client, entry, failure, read_body, search_ok, WebSettings};
use crate::web_vendors::{crawl_status, keenable_origin_error, origin_error, vendor_error};

pub const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
pub const PARALLEL_MCP_URL: &str = "https://search.parallel.ai/mcp";
pub const FIRECRAWL_API_URL: &str = "https://api.firecrawl.dev";
pub const KEENABLE_API_URL: &str = "https://api.keenable.ai";
const KEENABLE_TITLE: &str = "hermes-agent";
const TIMEOUT_SECS: u64 = 30;
const ALL_PAID_MSG: &str = "All keyless web providers are pinned to paid tiers.";
pub const KEYLESS_RING: [&str; 4] = ["exa", "parallel", "firecrawl", "keenable"];

const RATE_LIMIT_MARKERS: [&str; 7] = [
    "rate limit",
    "rate-limit",
    "ratelimit",
    "too many requests",
    "429",
    "quota exceeded",
    "slow down",
];

static VENDOR_REFUSAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:keyless\s+\w+\s+search\s+failed:\s*)?(?:http(?:\s+status)?|status(?:\s+code)?|client\s+error|error\s+code)\s*[:=']*\s*(?:40[123]|5\d\d)\b",
    )
    .expect("valid vendor refusal regex")
});

static ORIGIN_HTTP_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^HTTP \d{3} from origin(?::|$)").expect("valid origin http regex")
});

/// Random per-process Parallel rate-limit correlation id (never persisted).
static SESSION_ID: LazyLock<String> = LazyLock::new(|| {
    let mut buf = [0u8; 16];
    let rng = SystemRandom::new();
    let _ = rng.fill(&mut buf);
    let mut out = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
});

static RING_CURSOR: LazyLock<AtomicUsize> = LazyLock::new(|| {
    let seed = u8::from_str_radix(&SESSION_ID[..2], 16).unwrap_or(0) as usize;
    AtomicUsize::new(seed % KEYLESS_RING.len())
});

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeylessSettings {
    /// `web.keyless_fallback` (default `true`).
    #[serde(default = "default_true")]
    pub keyless_fallback: bool,
    /// True when `web.keyless_fallback: true` was explicitly set in config.yaml.
    #[serde(default)]
    pub keyless_explicit: bool,
    /// `web.provider_tier.<vendor>`: `"free"`, `"paid"`, or `"auto"`.
    #[serde(default)]
    pub provider_tier: BTreeMap<String, String>,
    /// True when config explicitly selected a web backend (`web.backend`/`search_backend`/`extract_backend`).
    #[serde(default)]
    pub explicit_backend: Option<String>,
    /// Test-only endpoint overrides.
    #[serde(default)]
    pub exa_mcp_url: Option<String>,
    #[serde(default)]
    pub parallel_mcp_url: Option<String>,
    #[serde(default)]
    pub firecrawl_keyless_url: Option<String>,
    #[serde(default)]
    pub keenable_keyless_url: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for KeylessSettings {
    fn default() -> Self {
        Self {
            keyless_fallback: true,
            keyless_explicit: false,
            provider_tier: BTreeMap::new(),
            explicit_backend: None,
            exa_mcp_url: None,
            parallel_mcp_url: None,
            firecrawl_keyless_url: None,
            keenable_keyless_url: None,
        }
    }
}

impl KeylessSettings {
    pub fn from_config(web: Option<&serde_yaml::Value>, explicit_backend: Option<String>) -> Self {
        let mut settings = Self {
            explicit_backend,
            ..Self::default()
        };
        let Some(web) = web.filter(|v| v.is_mapping()) else {
            return settings;
        };
        if let Some(val) = web.get("keyless_fallback") {
            let enabled = crate::config::truthy(val, true);
            settings.keyless_fallback = enabled;
            settings.keyless_explicit = enabled;
        }
        if let Some(tiers) = web
            .get("provider_tier")
            .and_then(serde_yaml::Value::as_mapping)
        {
            for (k, v) in tiers {
                let Some(name) = k.as_str().map(|s| s.trim().to_ascii_lowercase()) else {
                    continue;
                };
                let tier = v
                    .as_str()
                    .map(|s| s.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                let normalized = match tier.as_str() {
                    "free" => "free",
                    "paid" => "paid",
                    _ => "auto",
                };
                settings.provider_tier.insert(name, normalized.to_owned());
            }
        }
        settings
    }

    pub fn tier_for(&self, vendor: &str) -> &str {
        self.provider_tier
            .get(vendor)
            .map(String::as_str)
            .unwrap_or("auto")
    }

    /// Single chokepoint matching `plugins/web/keyless_mcp.py::use_keyless`.
    pub fn use_keyless(&self, vendor: &str, has_key: bool) -> bool {
        match self.tier_for(vendor) {
            "free" => true,
            "paid" => false,
            _ => !has_key && self.keyless_fallback,
        }
    }

    /// First keyless vendor available when `keyless_fallback: true` or a vendor is pinned `free`.
    pub fn explicit_keyless_backend(&self) -> Option<&'static str> {
        for vendor in KEYLESS_RING {
            if self.tier_for(vendor) == "free" {
                return Some(vendor);
            }
        }
        if self.keyless_explicit {
            return KEYLESS_RING
                .into_iter()
                .find(|v| self.tier_for(v) != "paid");
        }
        None
    }
}

pub fn is_origin_http_error(error: &str) -> bool {
    ORIGIN_HTTP_ERROR_RE.is_match(error)
}

pub fn is_rate_limitish(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    RATE_LIMIT_MARKERS.iter().any(|m| lower.contains(m))
}

pub fn is_search_failover_eligible(message: &str) -> bool {
    is_rate_limitish(message) || VENDOR_REFUSAL_RE.is_match(message)
}

fn vendor_hint(vendor: &str) -> (&'static str, &'static str, &'static str) {
    match vendor {
        "exa" => ("Exa", "EXA_API_KEY", "https://exa.ai"),
        "parallel" => ("Parallel", "PARALLEL_API_KEY", "https://parallel.ai"),
        "firecrawl" => ("Firecrawl", "FIRECRAWL_API_KEY", "https://firecrawl.dev"),
        "keenable" => ("Keenable", "KEENABLE_API_KEY", "https://keenable.ai"),
        _ => (
            "Web",
            "WEB_API_KEY",
            "https://hermes-agent.nousresearch.com",
        ),
    }
}

pub fn fail_msg(vendor: &str, kind: &str, exc: &str, other_backends: bool) -> String {
    let (label, env_key, site) = vendor_hint(vendor);
    let alt = if other_backends {
        " or another web backend via `hermes tools`"
    } else {
        ""
    };
    format!(
        "Keyless {label} {kind} failed: {exc}. Set {env_key} ({site}){alt} for reliable service."
    )
}

/// Parse first text content item from a JSON-RPC or SSE MCP `tools/call` response.
/// Splits SSE lines ONLY on `\r\n`, `\r`, `\n` so U+2028/U+2029 inside CJK text stay intact.
pub fn parse_mcp_body(body: &str) -> Result<String, String> {
    let from_payload = |payload: &str| -> Result<Option<String>, String> {
        let payload = payload.trim();
        if !payload.starts_with('{') {
            return Ok(None);
        }
        let Ok(data) = serde_json::from_str::<Value>(payload) else {
            return Ok(None);
        };
        if let Some(err) = data
            .get("error")
            .filter(|v| !v.is_null() && **v != Value::Bool(false))
        {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| err.to_string());
            return Err(msg);
        }
        let result = data.get("result");
        let texts: Vec<String> = result
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_object()?.get("text").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        if result
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            let non_empty: Vec<&str> = texts
                .iter()
                .map(String::as_str)
                .filter(|s| !s.is_empty())
                .collect();
            return Err(if non_empty.is_empty() {
                "MCP tool call failed".into()
            } else {
                non_empty.join(" ")
            });
        }
        Ok(Some(
            texts
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or_default(),
        ))
    };

    let mut candidates: Vec<&str> = Vec::new();
    let stripped = body.trim();
    if stripped.starts_with('{') {
        candidates.push(stripped);
    }
    for line in split_sse_lines(body) {
        if let Some(rest) = line.strip_prefix("data: ") {
            candidates.push(rest);
        }
    }

    let mut envelope_seen = false;
    for candidate in candidates {
        match from_payload(candidate)? {
            Some(text) if !text.is_empty() => return Ok(text),
            Some(_) => envelope_seen = true,
            None => {}
        }
    }
    if envelope_seen {
        Err("MCP response contained no text content".into())
    } else {
        Err("Unrecognized MCP response shape".into())
    }
}

/// Split strictly on `\r\n`, `\r`, or `\n` (never U+0085/U+2028/U+2029).
fn split_sse_lines(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&input[start..i]);
            i += if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                2
            } else {
                1
            };
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(&input[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    lines.push(&input[start..]);
    lines
}

async fn mcp_call(url: &str, tool: &str, arguments: Value) -> Result<String, String> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}
    });
    let http = client(Duration::from_secs(TIMEOUT_SECS))?;
    let response = http
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("User-Agent", "hermes-agent")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e.without_url()))?;
    let status = response.status().as_u16();
    let raw = read_body(response).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    if status >= 400 {
        let preview: String = text.chars().take(300).collect();
        return Err(format!("HTTP {status}: {preview}"));
    }
    parse_mcp_body(&text)
}

fn hit(url: &str, title: &str, description: &str, position: usize) -> Value {
    json!({"url": url, "title": title, "description": description, "position": position})
}

fn document(url: &str, title: &str, content: &str, source_url: Option<&str>) -> Value {
    json!({
        "url": url,
        "title": title,
        "content": content,
        "raw_content": content,
        "metadata": {"sourceURL": source_url.unwrap_or(url), "title": title}
    })
}

/// Parse Exa's `\n---\n`-separated `Title:/URL:/Published:/Author:/Highlights:` blocks.
pub fn parse_exa_search_text(text: &str, limit: usize) -> Vec<Value> {
    const LABELS: [&str; 5] = ["Title:", "URL:", "Highlights:", "Published:", "Author:"];
    let mut results = Vec::new();
    for block in text.split("\n---\n") {
        let mut title = String::new();
        let mut url = String::new();
        let mut highlight_lines: Vec<String> = Vec::new();
        let mut in_highlights = false;
        for line in block.lines() {
            let stripped = line.trim();
            if let Some(rest) = stripped.strip_prefix("Title:") {
                title = rest.trim().to_owned();
            } else if let Some(rest) = stripped.strip_prefix("URL:") {
                url = rest.trim().to_owned();
            } else if in_highlights
                && !stripped.is_empty()
                && !LABELS.iter().any(|l| stripped.starts_with(l))
            {
                highlight_lines.push(stripped.to_owned());
            }
            if LABELS.iter().any(|l| stripped.starts_with(l)) {
                in_highlights = stripped.starts_with("Highlights:");
            }
        }
        if !url.is_empty() {
            let pos = results.len() + 1;
            results.push(hit(&url, &title, &highlight_lines.join(" "), pos));
        }
        if limit > 0 && results.len() >= limit {
            break;
        }
    }
    results
}

async fn exa_search_keyless(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let url = settings
        .keyless
        .exa_mcp_url
        .as_deref()
        .unwrap_or(EXA_MCP_URL);
    match mcp_call(
        url,
        "web_search_exa",
        json!({"query": query, "numResults": limit.max(1)}),
    )
    .await
    {
        Ok(text) => search_ok(parse_exa_search_text(&text, limit)),
        Err(exc) => failure(fail_msg("exa", "search", &exc, true)),
    }
}

async fn exa_extract_keyless(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let endpoint = settings
        .keyless
        .exa_mcp_url
        .as_deref()
        .unwrap_or(EXA_MCP_URL);
    let mut pages = Vec::with_capacity(urls.len());
    for url in urls {
        match mcp_call(endpoint, "web_fetch_exa", json!({"urls": [url]})).await {
            Ok(text) => {
                let title = text
                    .lines()
                    .map(str::trim)
                    .find_map(|s| {
                        s.strip_prefix("# ")
                            .or_else(|| s.strip_prefix("Title:"))
                            .map(|rest| rest.trim().to_owned())
                    })
                    .unwrap_or_default();
                pages.push(document(url, &title, &text, None));
            }
            Err(exc) => {
                if let Some(origin) = origin_error(crawl_status(Some(&exc))) {
                    pages.push(entry(url, Some(&origin)));
                } else {
                    pages.push(entry(url, Some(&fail_msg("exa", "extract", &exc, true))));
                }
            }
        }
    }
    pages
}

async fn parallel_search_keyless(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let url = settings
        .keyless
        .parallel_mcp_url
        .as_deref()
        .unwrap_or(PARALLEL_MCP_URL);
    let text = match mcp_call(
        url,
        "web_search",
        json!({"objective": query, "search_queries": [query], "session_id": *SESSION_ID}),
    )
    .await
    {
        Ok(text) => text,
        Err(exc) => return failure(fail_msg("parallel", "search", &exc, true)),
    };
    let parsed: Value = match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => v,
        Ok(_) => {
            return failure(
                "Keyless Parallel search returned an unexpected payload: expected object",
            )
        }
        Err(exc) => {
            return failure(format!(
                "Keyless Parallel search returned an unexpected payload: {exc}"
            ))
        }
    };
    let iter = parsed["results"].as_array().into_iter().flatten();
    let rows: Vec<Value> = iter
        .take(if limit == 0 { usize::MAX } else { limit })
        .enumerate()
        .map(|(i, r)| {
            let excerpts = r["excerpts"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            hit(
                r["url"].as_str().unwrap_or(""),
                r["title"].as_str().unwrap_or(""),
                &excerpts,
                i + 1,
            )
        })
        .collect();
    search_ok(rows)
}

async fn parallel_extract_keyless(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let endpoint = settings
        .keyless
        .parallel_mcp_url
        .as_deref()
        .unwrap_or(PARALLEL_MCP_URL);
    let text = match mcp_call(
        endpoint,
        "web_fetch",
        json!({"urls": urls, "objective": "Full page content", "session_id": *SESSION_ID}),
    )
    .await
    {
        Ok(text) => text,
        Err(exc) => {
            let msg = fail_msg("parallel", "extract", &exc, true);
            return urls.iter().map(|u| entry(u, Some(&msg))).collect();
        }
    };
    let data: Value = match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => v,
        Ok(_) => {
            let msg = fail_msg("parallel", "extract", "expected JSON object", true);
            return urls.iter().map(|u| entry(u, Some(&msg))).collect();
        }
        Err(exc) => {
            let msg = fail_msg("parallel", "extract", &exc.to_string(), true);
            return urls.iter().map(|u| entry(u, Some(&msg))).collect();
        }
    };
    let mut results: Vec<Value> = data["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let full = r["full_content"].as_str().unwrap_or("");
            let content_field = r["content"].as_str().unwrap_or("");
            let content = if !full.is_empty() {
                full.to_owned()
            } else if !content_field.is_empty() {
                content_field.to_owned()
            } else {
                r["excerpts"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default()
            };
            document(
                r["url"].as_str().unwrap_or(""),
                r["title"].as_str().unwrap_or(""),
                &content,
                None,
            )
        })
        .collect();
    for err in data["errors"].as_array().into_iter().flatten() {
        results.push(vendor_error(
            err["url"].as_str().unwrap_or(""),
            err["http_status_code"].as_i64(),
            err["error_type"].as_str(),
            err["content"].as_str(),
        ));
    }
    let seen: std::collections::HashSet<String> = results
        .iter()
        .filter_map(|r| r["url"].as_str().map(str::to_owned))
        .collect();
    for u in urls {
        if !seen.contains(u) {
            results.push(entry(u, Some("no content returned")));
        }
    }
    results
}

async fn firecrawl_search_keyless(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let base = settings
        .keyless
        .firecrawl_keyless_url
        .as_deref()
        .unwrap_or(FIRECRAWL_API_URL)
        .trim_end_matches('/');
    let http = match client(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(http) => http,
        Err(exc) => return failure(fail_msg("firecrawl", "search", &exc, true)),
    };
    let response = match http
        .post(format!("{base}/v2/search"))
        .json(&json!({"query": query, "limit": limit.max(1)}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return failure(fail_msg(
                "firecrawl",
                "search",
                &e.without_url().to_string(),
                true,
            ))
        }
    };
    let status = response.status().as_u16();
    let raw = match read_body(response).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => return failure(fail_msg("firecrawl", "search", &e, true)),
    };
    if status >= 400 {
        let preview: String = raw.chars().take(300).collect();
        return failure(fail_msg(
            "firecrawl",
            "search",
            &format!("HTTP {status}: {preview}"),
            true,
        ));
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return failure(fail_msg(
            "firecrawl",
            "search",
            "invalid JSON response",
            true,
        ));
    };
    if parsed["success"].as_bool() == Some(false) {
        let msg = parsed["error"].as_str().unwrap_or("request failed");
        return failure(fail_msg("firecrawl", "search", msg, true));
    }
    let rows: Vec<Value> = parsed["data"]["web"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.is_object())
        .map(|item| {
            json!({
                "url": item["url"],
                "title": item["title"],
                "description": item["description"],
                "position": item["position"],
                "category": item["category"]
            })
        })
        .collect();
    search_ok(rows)
}

async fn firecrawl_extract_keyless(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let base = settings
        .keyless
        .firecrawl_keyless_url
        .as_deref()
        .unwrap_or(FIRECRAWL_API_URL)
        .trim_end_matches('/');
    let http = match client(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(http) => http,
        Err(exc) => {
            let msg = fail_msg("firecrawl", "extract", &exc, false);
            return urls.iter().map(|u| entry(u, Some(&msg))).collect();
        }
    };
    let mut pages = Vec::with_capacity(urls.len());
    for url in urls {
        let response = match http
            .post(format!("{base}/v2/scrape"))
            .json(&json!({"url": url, "formats": ["markdown"]}))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                pages.push(entry(
                    url,
                    Some(&fail_msg(
                        "firecrawl",
                        "extract",
                        &e.without_url().to_string(),
                        false,
                    )),
                ));
                continue;
            }
        };
        let status = response.status().as_u16();
        let raw = match read_body(response).await {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                pages.push(entry(
                    url,
                    Some(&fail_msg("firecrawl", "extract", &e, false)),
                ));
                continue;
            }
        };
        if status >= 400 {
            let preview: String = raw.chars().take(300).collect();
            pages.push(entry(
                url,
                Some(&fail_msg(
                    "firecrawl",
                    "extract",
                    &format!("HTTP {status}: {preview}"),
                    false,
                )),
            ));
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            pages.push(entry(
                url,
                Some(&fail_msg(
                    "firecrawl",
                    "extract",
                    "invalid JSON response",
                    false,
                )),
            ));
            continue;
        };
        let data = if parsed["data"].is_object() {
            &parsed["data"]
        } else {
            &parsed
        };
        let metadata = &data["metadata"];
        let final_url = metadata["url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(url);
        let title = metadata["title"].as_str().unwrap_or("");
        let content = data["markdown"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| data["html"].as_str())
            .unwrap_or("");
        let mut page = document(final_url, title, content, Some(url));
        let status_code = [&metadata["statusCode"], &metadata["status_code"]]
            .iter()
            .find_map(|v| v.as_i64());
        let reason = metadata["error"].as_str().unwrap_or("").trim();
        if let Some(origin) = origin_error(status_code) {
            let err_str = if reason.is_empty() {
                origin
            } else {
                format!("{origin}: {reason}")
            };
            page["error"] = json!(err_str);
        }
        pages.push(page);
    }
    pages
}

async fn keenable_public_request(
    settings: &WebSettings,
    method: reqwest::Method,
    path: &str,
    query: Option<&[(&str, &str)]>,
    body: Option<&Value>,
) -> Result<Value, String> {
    let base = settings
        .keyless
        .keenable_keyless_url
        .as_deref()
        .or(settings.keenable_base.as_deref())
        .unwrap_or(KEENABLE_API_URL)
        .trim_end_matches('/');
    let http = client(Duration::from_secs(TIMEOUT_SECS))?;
    let mut req = http
        .request(method.clone(), format!("{base}{path}"))
        .header("X-Keenable-Title", KEENABLE_TITLE);
    if let Some(q) = query {
        req = req.query(q);
    }
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").json(b);
    }
    let response = req
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e.without_url()))?;
    let status = response.status().as_u16();
    let raw = read_body(response).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    if status >= 400 {
        let trimmed = text.trim();
        if let Some(origin) = keenable_origin_error(status, trimmed) {
            return Err(origin);
        }
        let preview: String = trimmed.chars().take(300).collect();
        return Err(format!("HTTP {status}: {preview}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))
}

async fn keenable_search_keyless(settings: &WebSettings, query: &str, limit: usize) -> Value {
    match keenable_public_request(
        settings,
        reqwest::Method::POST,
        "/v1/search/public",
        None,
        Some(&json!({"query": query, "max_results": limit.max(1)})),
    )
    .await
    {
        Ok(data) => search_ok(
            data["results"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, r)| {
                    let snippet = r["snippet"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or_else(|| r["description"].as_str())
                        .unwrap_or("");
                    hit(
                        r["url"].as_str().unwrap_or(""),
                        r["title"].as_str().unwrap_or(""),
                        snippet,
                        i + 1,
                    )
                })
                .collect(),
        ),
        Err(exc) => failure(fail_msg("keenable", "search", &exc, true)),
    }
}

async fn keenable_extract_keyless(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let mut pages = Vec::with_capacity(urls.len());
    for url in urls {
        match keenable_public_request(
            settings,
            reqwest::Method::GET,
            "/v1/fetch/public",
            Some(&[("url", url.as_str())]),
            None,
        )
        .await
        {
            Ok(data) => {
                let final_url = data["url"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url);
                pages.push(document(
                    final_url,
                    data["title"].as_str().unwrap_or(""),
                    data["content"].as_str().unwrap_or(""),
                    Some(url),
                ));
            }
            Err(exc) => {
                if is_origin_http_error(&exc) {
                    pages.push(entry(url, Some(&exc)));
                } else {
                    pages.push(entry(
                        url,
                        Some(&fail_msg("keenable", "extract", &exc, false)),
                    ));
                }
            }
        }
    }
    pages
}

fn ring_order(settings: &WebSettings, name: &str) -> Vec<&'static str> {
    let pinned = settings.keyless.tier_for(name) == "free"
        || settings.keyless.explicit_backend.as_deref() == Some(name);
    let start = if pinned {
        KEYLESS_RING.iter().position(|&v| v == name).unwrap_or(0)
    } else {
        RING_CURSOR.fetch_add(1, Ordering::Relaxed) % KEYLESS_RING.len()
    };
    let mut ordered = Vec::with_capacity(KEYLESS_RING.len());
    for offset in 0..KEYLESS_RING.len() {
        let vendor = KEYLESS_RING[(start + offset) % KEYLESS_RING.len()];
        if settings.keyless.tier_for(vendor) != "paid" {
            ordered.push(vendor);
        }
    }
    ordered
}

async fn dispatch_search(settings: &WebSettings, vendor: &str, query: &str, limit: usize) -> Value {
    match vendor {
        "exa" => exa_search_keyless(settings, query, limit).await,
        "parallel" => parallel_search_keyless(settings, query, limit).await,
        "firecrawl" => firecrawl_search_keyless(settings, query, limit).await,
        "keenable" => keenable_search_keyless(settings, query, limit).await,
        _ => failure(format!("Unknown keyless vendor: {vendor}")),
    }
}

async fn dispatch_extract(settings: &WebSettings, vendor: &str, urls: &[String]) -> Vec<Value> {
    match vendor {
        "exa" => exa_extract_keyless(settings, urls).await,
        "parallel" => parallel_extract_keyless(settings, urls).await,
        "firecrawl" => firecrawl_extract_keyless(settings, urls).await,
        "keenable" => keenable_extract_keyless(settings, urls).await,
        _ => urls
            .iter()
            .map(|u| entry(u, Some(&format!("Unknown keyless vendor: {vendor}"))))
            .collect(),
    }
}

/// Search across the keyless ring with failover on rate limits and 401/402/403/5xx vendor refusals.
pub async fn search_with_failover(
    settings: &WebSettings,
    name: &str,
    query: &str,
    limit: usize,
) -> Value {
    let order = ring_order(settings, name);
    if order.is_empty() {
        return failure(ALL_PAID_MSG);
    }
    let mut last_result = failure(ALL_PAID_MSG);
    for &vendor in &order {
        let mut result = dispatch_search(settings, vendor, query, limit).await;
        let throttled = result["success"].as_bool() != Some(true)
            && is_search_failover_eligible(result["error"].as_str().unwrap_or(""));
        if !throttled {
            if result["success"].as_bool() == Some(true) && vendor != name {
                if let Some(data) = result.get_mut("data").and_then(Value::as_object_mut) {
                    data.insert("served_by".into(), json!(vendor));
                }
            }
            return result;
        }
        last_result = result;
    }
    let prev = last_result["error"].as_str().unwrap_or("");
    last_result["error"] = json!(format!(
        "{prev} (all keyless vendors unavailable: {})",
        order.join(", ")
    ));
    last_result
}

/// Extract across the keyless ring; advances only when EVERY URL fails with a rate-limit error
/// (never on origin HTTP errors like `HTTP 429 from origin`).
pub async fn extract_with_failover(
    settings: &WebSettings,
    name: &str,
    urls: &[String],
) -> Vec<Value> {
    let order = ring_order(settings, name);
    if order.is_empty() {
        return urls.iter().map(|u| entry(u, Some(ALL_PAID_MSG))).collect();
    }
    let mut last_results = Vec::new();
    for &vendor in &order {
        let results = dispatch_extract(settings, vendor, urls).await;
        let all_throttled = !results.is_empty()
            && results.iter().all(|r| {
                let err = r["error"].as_str().unwrap_or("");
                !err.is_empty() && !is_origin_http_error(err) && is_rate_limitish(err)
            });
        if !all_throttled {
            return results;
        }
        last_results = results;
    }
    last_results
}
