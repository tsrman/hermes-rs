//! Exa and Parallel, reached over their REST routes (the reference uses their SDKs).
//!
//! The requests and the texts the model receives follow a recording of the reference with the
//! SDK versions of its lock file (`compat/create_web_sdk_corpus.py`). Without a key the reference
//! falls back to a keyless ring of remote MCP endpoints; this port does not, and says so.

use std::time::Duration;

use serde_json::{json, Value};

use crate::url_safety::is_safe_url;
use crate::web_tools::{align_pages, client, entry, failure, read_body, search_ok, WebSettings};

const LIMIT_CAP: usize = 20;
const EXA_KEY_MISSING: &str =
    "EXA_API_KEY environment variable not set. Get your API key at https://exa.ai";
const PARALLEL_KEY_MISSING: &str =
    "PARALLEL_API_KEY environment variable not set. Get your API key at https://parallel.ai";
const KEYLESS: &str =
    "The keyless free tier of this backend is not available in the Rust CLI; set the API key.";

/// A result row with the URL first (the wire shape of Exa and Parallel results).
fn hit(url: &str, title: &str, description: &str, position: usize) -> Value {
    json!({"url": url, "title": title, "description": description, "position": position})
}

/// A successful extract document, as the reference builds it.
fn document(url: &str, title: &str, content: &str) -> Value {
    json!({
        "url": url, "title": title, "content": content, "raw_content": content,
        "metadata": {"sourceURL": url, "title": title}
    })
}

/// `HTTP <status> from origin[: reason]` when the target page answered with an error.
fn origin_error(status: Option<i64>) -> Option<String> {
    status
        .filter(|code| *code >= 400)
        .map(|code| format!("HTTP {code} from origin"))
}

/// A failed URL from a vendor's own error list: the origin status when it has one (the body stays
/// as content), otherwise the vendor's body or reason is the error text.
fn vendor_error(url: &str, status: Option<i64>, reason: Option<&str>, body: Option<&str>) -> Value {
    let (content, error) = match origin_error(status) {
        Some(origin) => (body.unwrap_or("").to_owned(), origin),
        None => (
            String::new(),
            body.filter(|text| !text.is_empty())
                .or(reason.filter(|text| !text.is_empty()))
                .unwrap_or("extraction failed")
                .to_owned(),
        ),
    };
    json!({"url": url, "title": "", "content": content, "error": error, "metadata": {"sourceURL": url}})
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn joined(value: &Value, separator: &str) -> String {
    value
        .as_array()
        .map(|items| items.iter().map(text).collect::<Vec<_>>().join(separator))
        .unwrap_or_default()
}

/// POST JSON and return the parsed body, or the status and raw body on an HTTP error.
enum Reply {
    Json(Value),
    Http(u16, String),
    Failed(String),
}

async fn post(
    url: String,
    header: (&str, &str),
    extra: &[(&str, &str)],
    payload: &Value,
    retries: u32,
) -> Reply {
    let http = match client(Duration::from_secs(60)) {
        Ok(http) => http,
        Err(error) => return Reply::Failed(error),
    };
    let mut attempt = 0;
    loop {
        let mut request = http.post(&url).header(header.0, header.1).json(payload);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Reply::Failed(error.without_url().to_string()),
        };
        let status = response.status().as_u16();
        let body = match read_body(response).await {
            Ok(body) => String::from_utf8_lossy(&body).into_owned(),
            Err(error) => return Reply::Failed(error),
        };
        if status >= 400 {
            // The Parallel SDK retries rate limits, timeouts, conflicts and server errors.
            if attempt < retries && matches!(status, 408 | 409 | 429) | (status >= 500) {
                tokio::time::sleep(Duration::from_millis(437 << attempt)).await;
                attempt += 1;
                continue;
            }
            return Reply::Http(status, body);
        }
        return match serde_json::from_str(&body) {
            Ok(value) => Reply::Json(value),
            Err(_) => Reply::Failed("invalid response".into()),
        };
    }
}

// ---- Exa ----

fn exa_url(settings: &WebSettings, route: &str) -> String {
    let base = settings.exa_base.as_deref().unwrap_or("https://api.exa.ai");
    format!("{}/{route}", base.trim_end_matches('/'))
}

/// `Request failed with status code <n>: <body>`, the SDK's text for an HTTP error.
fn exa_http_error(status: u16, body: &str) -> String {
    format!("Request failed with status code {status}: {body}")
}

pub(crate) async fn exa_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let Some(key) = settings.exa_key.as_deref() else {
        return failure(format!("{EXA_KEY_MISSING} ({KEYLESS})"));
    };
    let payload = json!({"query": query, "contents": {"highlights": true}, "numResults": limit});
    match post(
        exa_url(settings, "search"),
        ("x-api-key", key),
        &[("x-exa-integration", "hermes-agent")],
        &payload,
        0,
    )
    .await
    {
        Reply::Json(response) => search_ok(
            response["results"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, result)| {
                    hit(
                        text(&result["url"]),
                        text(&result["title"]),
                        &joined(&result["highlights"], " "),
                        index + 1,
                    )
                })
                .collect(),
        ),
        Reply::Http(status, body) => failure(exa_http_error(status, &body)),
        Reply::Failed(error) => failure(format!("Exa search failed: {error}")),
    }
}

pub(crate) async fn exa_extract(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let all = |message: &str| -> Vec<Value> {
        urls.iter().map(|url| entry(url, Some(message))).collect()
    };
    let Some(key) = settings.exa_key.as_deref() else {
        return all(&format!("{EXA_KEY_MISSING} ({KEYLESS})"));
    };
    let payload = json!({"urls": urls, "text": true});
    let response = match post(
        exa_url(settings, "contents"),
        ("x-api-key", key),
        &[("x-exa-integration", "hermes-agent")],
        &payload,
        0,
    )
    .await
    {
        Reply::Json(response) => response,
        Reply::Http(status, body) => return all(&exa_http_error(status, &body)),
        Reply::Failed(error) => return all(&format!("Exa extract failed: {error}")),
    };
    let mut pages: Vec<Value> = response["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|result| {
            document(
                text(&result["url"]),
                text(&result["title"]),
                text(&result["text"]),
            )
        })
        .collect();
    // A URL Exa could not fetch is absent from `results`; its reason is in `statuses`.
    for status in response["statuses"].as_array().into_iter().flatten() {
        if status["status"] != "error" {
            continue;
        }
        let tag = status["error"]["tag"].as_str();
        let code = status["error"]["httpStatusCode"]
            .as_i64()
            .filter(|code| *code != 0)
            .or_else(|| crawl_status(tag));
        pages.push(vendor_error(text(&status["id"]), code, tag, None));
    }
    align_pages(urls, pages)
}

/// The origin status an Exa crawl tag ends with (`CRAWL_HTTP_429` is 429, `CRAWL_NOT_FOUND` 404).
fn crawl_status(tag: Option<&str>) -> Option<i64> {
    let tag = tag?.trim_end();
    let word = tag
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()?;
    if word == "CRAWL_NOT_FOUND" {
        return Some(404);
    }
    let digits = word.strip_prefix("CRAWL_HTTP_")?;
    (digits.len() == 3 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

// ---- Parallel ----

fn parallel_url(settings: &WebSettings, route: &str) -> String {
    let base = settings
        .parallel_base
        .as_deref()
        .unwrap_or("https://api.parallel.ai");
    format!("{}/v1beta/{route}", base.trim_end_matches('/'))
}

/// Python's `repr` of a JSON value, which the SDK puts in its error text.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => {
            let quote = if string.contains('\'') && !string.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::from(quote);
            for c in string.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
            )
        }
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    python_repr(&Value::String(key.clone())),
                    python_repr(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `Error code: <n> - <body>`, the SDK's text for an HTTP error (a JSON body shows as a Python value).
fn parallel_http_error(status: u16, body: &str) -> String {
    let shown = serde_json::from_str::<Value>(body)
        .map(|value| python_repr(&value))
        .unwrap_or_else(|_| body.to_owned());
    format!("Error code: {status} - {shown}")
}

pub(crate) async fn parallel_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let Some(key) = settings.parallel_key.as_deref() else {
        return failure(format!("{PARALLEL_KEY_MISSING} ({KEYLESS})"));
    };
    let mode = settings
        .parallel_mode
        .as_deref()
        .map(str::to_lowercase)
        .filter(|mode| matches!(mode.as_str(), "fast" | "one-shot" | "agentic"))
        .unwrap_or_else(|| "agentic".into());
    let payload = json!({
        "search_queries": [query], "objective": query, "mode": mode,
        "max_results": limit.min(LIMIT_CAP)
    });
    match post(
        parallel_url(settings, "search"),
        ("x-api-key", key),
        &[],
        &payload,
        2,
    )
    .await
    {
        Reply::Json(response) => search_ok(
            response["results"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, result)| {
                    hit(
                        text(&result["url"]),
                        text(&result["title"]),
                        &joined(&result["excerpts"], " "),
                        index + 1,
                    )
                })
                .collect(),
        ),
        Reply::Http(status, body) => failure(format!(
            "Parallel search failed: {}",
            parallel_http_error(status, &body)
        )),
        Reply::Failed(error) => failure(format!("Parallel search failed: {error}")),
    }
}

pub(crate) async fn parallel_extract(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let all = |message: &str| -> Vec<Value> {
        urls.iter().map(|url| entry(url, Some(message))).collect()
    };
    let Some(key) = settings.parallel_key.as_deref() else {
        return all(&format!("{PARALLEL_KEY_MISSING} ({KEYLESS})"));
    };
    let payload = json!({"urls": urls, "full_content": true});
    let response = match post(
        parallel_url(settings, "extract"),
        ("x-api-key", key),
        &[],
        &payload,
        2,
    )
    .await
    {
        Reply::Json(response) => response,
        Reply::Http(status, body) => {
            return all(&format!(
                "Parallel extract failed: {}",
                parallel_http_error(status, &body)
            ))
        }
        Reply::Failed(error) => return all(&format!("Parallel extract failed: {error}")),
    };
    let mut pages: Vec<Value> = response["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|result| {
            let full = text(&result["full_content"]);
            let content = if full.is_empty() {
                joined(&result["excerpts"], "\n\n")
            } else {
                full.to_owned()
            };
            document(text(&result["url"]), text(&result["title"]), &content)
        })
        .collect();
    for error in response["errors"].as_array().into_iter().flatten() {
        pages.push(vendor_error(
            text(&error["url"]),
            error["http_status_code"].as_i64(),
            error["error_type"].as_str(),
            error["content"].as_str(),
        ));
    }
    align_pages(urls, pages)
}

// ---- Firecrawl (keyed cloud or self-hosted, the SDK path of the reference) ----

const FIRECRAWL_UNCONFIGURED: &str = "Web tools are not configured. Set FIRECRAWL_API_KEY for cloud Firecrawl or set FIRECRAWL_API_URL for a self-hosted Firecrawl instance.";
const FIRECRAWL_ORIGIN: &str = "python-sdk@4.53.2";
const SCRAPE_TIMEOUT: &str =
    "Scrape timed out after 60s \u{2014} page may be too large or unresponsive. Try browser_exec instead.";
const BLOCKED_REDIRECT: &str = "Blocked: URL targets a private or internal network address";

/// The text the SDK raises for a failed call (`action` is "search" or "scrape").
fn firecrawl_error(status: u16, body: &str, action: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body)
        .ok()
        .filter(Value::is_object);
    let (message, details) = match &parsed {
        Some(object) => (
            object
                .get("error")
                .map_or("No error message provided.".to_owned(), |v| {
                    crate::web_tools::text_of(v)
                }),
            object
                .get("details")
                .map_or("No additional error details provided.".to_owned(), |v| {
                    crate::web_tools::text_of(v)
                }),
        ),
        None if !body.trim().is_empty() => (
            format!(
                "Server returned non-JSON response: {}",
                body.chars().take(500).collect::<String>()
            ),
            format!("Full response status: {status}"),
        ),
        None => (
            format!("Server returned empty response with status {status}"),
            "No additional details available".to_owned(),
        ),
    };
    if let Some(code) = parsed.as_ref().and_then(|o| o["code"].as_str()) {
        if matches!(code, "SCRAPE_DNS_RESOLUTION_ERROR" | "SCRAPE_SSL_ERROR") {
            let mut text = format!("Failed to {action}. {message}");
            if details != "No additional error details provided." {
                text = format!("{text} - {details}");
            }
            return text;
        }
    }
    let tail = format!("{message} - {details}");
    match status {
        400 => format!("Bad Request: Failed to {action}. {tail}"),
        401 => format!("Unauthorized: Failed to {action}. {tail}"),
        402 => format!("Payment Required: Failed to {action}. {tail}"),
        403 if parsed.as_ref().and_then(|o| o["code"].as_str())
            == Some("THIRD_PARTY_DATA_TERMS_REQUIRED") =>
        {
            message
        }
        403 => format!("Website Not Supported: Failed to {action}. {tail}"),
        408 => format!("Request Timeout: Failed to {action} as the request timed out. {tail}"),
        429 => format!("Rate Limit Exceeded: Failed to {action}. {tail}"),
        500 => format!("Internal Server Error: Failed to {action}. {tail}"),
        other => format!("Unexpected error during {action}: Status code {other}. {tail}"),
    }
}

enum Fire {
    Json(Value),
    Failed(String),
    TimedOut,
}

async fn firecrawl_post(
    settings: &WebSettings,
    route: &str,
    action: &str,
    payload: &Value,
) -> Fire {
    let base = settings
        .firecrawl_url
        .as_deref()
        .unwrap_or("https://api.firecrawl.dev");
    let http = match client(Duration::from_secs(60)) {
        Ok(http) => http,
        Err(error) => return Fire::Failed(error),
    };
    // The SDK makes up to three attempts: a 502 answer or a failed connection waits 0.5 s, then 1 s.
    let mut attempt: u32 = 0;
    let (status, body) = loop {
        let mut request = http.post(format!("{base}{route}")).json(payload);
        if let Some(key) = settings.firecrawl_key.as_deref() {
            request = request.bearer_auth(key);
        }
        let backoff = Duration::from_millis(500 << attempt);
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) if attempt < 2 => {
                let _ = error;
                tokio::time::sleep(backoff).await;
                attempt += 1;
                continue;
            }
            Err(error) if error.is_timeout() => return Fire::TimedOut,
            Err(error) => return Fire::Failed(error.without_url().to_string()),
        };
        let status = response.status().as_u16();
        let body = match read_body(response).await {
            Ok(body) => String::from_utf8_lossy(&body).into_owned(),
            Err(error) => return Fire::Failed(error),
        };
        if status == 502 && attempt < 2 {
            tokio::time::sleep(backoff).await;
            attempt += 1;
            continue;
        }
        break (status, body);
    };
    let parsed = serde_json::from_str::<Value>(&body).ok();
    // The SDK treats `success: false` in a 200 answer like any other failure.
    match parsed {
        Some(value) if status < 400 && value["success"].as_bool().unwrap_or(false) => {
            Fire::Json(value)
        }
        _ => Fire::Failed(firecrawl_error(status, &body, action)),
    }
}

fn configured(settings: &WebSettings) -> bool {
    settings.firecrawl_key.is_some() || settings.firecrawl_url.is_some()
}

pub(crate) async fn firecrawl_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    if !configured(settings) {
        return failure(format!("{FIRECRAWL_UNCONFIGURED} ({KEYLESS})"));
    }
    let payload = json!({"query": query, "limit": limit, "origin": FIRECRAWL_ORIGIN});
    match firecrawl_post(settings, "/v2/search", "search", &payload).await {
        Fire::Json(response) => search_ok(
            response["data"]["web"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item.is_object())
                .map(|item| {
                    // The SDK's result model: these five fields, absent ones are null.
                    json!({
                        "url": item["url"], "title": item["title"], "description": item["description"],
                        "position": item["position"], "category": item["category"]
                    })
                })
                .collect(),
        ),
        Fire::Failed(error) => failure(format!("Firecrawl search failed: {error}")),
        Fire::TimedOut => failure("Firecrawl search failed: request timed out"),
    }
}

pub(crate) async fn firecrawl_extract(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    if !configured(settings) {
        let message = format!("{FIRECRAWL_UNCONFIGURED} ({KEYLESS})");
        return urls.iter().map(|url| entry(url, Some(&message))).collect();
    }
    let mut pages = Vec::new();
    for url in urls {
        pages.push(firecrawl_scrape(settings, url).await);
    }
    pages
}

fn scrape_failure(url: &str, title: &str, error: &str) -> Value {
    json!({"url": url, "title": title, "content": "", "raw_content": "", "error": error})
}

async fn firecrawl_scrape(settings: &WebSettings, url: &str) -> Value {
    let payload = json!({
        "url": url, "onlyMainContent": true, "skipTlsVerification": true, "removeBase64Images": true,
        "fastMode": false, "blockAds": true, "storeInCache": true, "maxAge": 14_400_000,
        "formats": ["markdown", "html"], "timeout": 60_000, "mobile": false, "origin": FIRECRAWL_ORIGIN
    });
    let response = match firecrawl_post(settings, "/v2/scrape", "scrape", &payload).await {
        Fire::Json(response) => response,
        Fire::Failed(error) => return scrape_failure(url, "", &error),
        Fire::TimedOut => return entry(url, Some(SCRAPE_TIMEOUT)),
    };
    let payload = if response["data"].is_object() {
        &response["data"]
    } else {
        &response
    };
    let metadata = &payload["metadata"];
    let title = metadata["title"].as_str().unwrap_or("");
    let final_url = metadata["url"]
        .as_str()
        .filter(|u| !u.is_empty())
        .unwrap_or(url);
    if !is_safe_url(final_url, settings.allow_private_urls).await {
        return scrape_failure(final_url, title, BLOCKED_REDIRECT);
    }
    let markdown = text(&payload["markdown"]);
    let html = text(&payload["html"]);
    let content = if !markdown.is_empty() {
        markdown
    } else if !html.is_empty() {
        html
    } else {
        ""
    };
    let status = [&metadata["statusCode"], &metadata["status_code"]]
        .iter()
        .find_map(|value| value.as_i64());
    let reason = text(&metadata["error"]);
    let error = origin_error(status).map(|origin| {
        if reason.is_empty() {
            origin
        } else {
            format!("{origin}: {reason}")
        }
    });
    json!({"url": final_url, "title": title, "content": content, "raw_content": content, "error": error})
}

// ---- Keenable ----

fn keenable_headers(request: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    request
        .header("X-Keenable-Title", "hermes-agent")
        .bearer_auth(key)
}

/// A response body for an error status, or `HTTP <code>` when it is empty.
fn status_detail(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        format!("HTTP {status}")
    } else {
        trimmed.to_owned()
    }
}

fn keenable_base(settings: &WebSettings) -> &str {
    settings
        .keenable_base
        .as_deref()
        .unwrap_or("https://api.keenable.ai")
        .trim_end_matches('/')
}

pub(crate) async fn keenable_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let Some(key) = settings.keenable_key.as_deref() else {
        return failure(format!("KEENABLE_API_KEY is not set. {KEYLESS}"));
    };
    let payload = json!({"query": query, "max_results": limit.clamp(1, LIMIT_CAP)});
    let http = match client(Duration::from_secs(30)) {
        Ok(http) => http,
        Err(error) => return failure(format!("Keenable search failed: {error}")),
    };
    let request = keenable_headers(
        http.post(format!("{}/v1/search", keenable_base(settings))),
        key,
    )
    .json(&payload);
    let (status, body) = match send_text(request).await {
        Ok(reply) => reply,
        Err(error) => return failure(format!("Keenable search failed: {error}")),
    };
    if status >= 400 {
        return failure(format!(
            "Keenable search failed: {}",
            status_detail(status, &body)
        ));
    }
    let Ok(data) = serde_json::from_str::<Value>(&body) else {
        return failure("Keenable search failed: Expecting value: line 1 column 1 (char 0)");
    };
    search_ok(
        data["results"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, result)| {
                let snippet = text(&result["snippet"]);
                let description = if snippet.is_empty() {
                    text(&result["description"])
                } else {
                    snippet
                };
                hit(
                    text(&result["url"]),
                    text(&result["title"]),
                    description,
                    index + 1,
                )
            })
            .collect(),
    )
}

async fn send_text(request: reqwest::RequestBuilder) -> Result<(u16, String), String> {
    let response = request
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;
    let status = response.status().as_u16();
    let body = read_body(response).await?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// `HTTP <status> from origin: <message>` when a fetch error describes the target's own answer
/// (not Keenable's rate limit, authentication or server errors).
fn keenable_origin_error(status: u16, body: &str) -> Option<String> {
    let data = serde_json::from_str::<Value>(body).ok()?;
    let message = data
        .as_object()?
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let lower = message.to_lowercase();
    let origin = ["the target server", "the page does not exist"]
        .iter()
        .any(|prefix| {
            lower
                .strip_prefix(prefix)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        });
    (origin && status >= 400).then(|| format!("HTTP {status} from origin: {message}"))
}

pub(crate) async fn keenable_extract(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let Some(key) = settings.keenable_key.as_deref() else {
        let message = format!("KEENABLE_API_KEY is not set. {KEYLESS}");
        return urls.iter().map(|url| entry(url, Some(&message))).collect();
    };
    let http = match client(Duration::from_secs(30)) {
        Ok(http) => http,
        Err(error) => {
            let message = format!("Keenable extract failed: {error}");
            return urls.iter().map(|url| entry(url, Some(&message))).collect();
        }
    };
    let mut pages = Vec::new();
    for url in urls {
        let request = keenable_headers(
            http.get(format!("{}/v1/fetch", keenable_base(settings))),
            key,
        )
        .query(&[("url", url.as_str())]);
        let failed =
            |detail: String| entry(url, Some(&format!("Keenable extract failed: {detail}")));
        let (status, body) = match send_text(request).await {
            Ok(reply) => reply,
            Err(error) => {
                pages.push(failed(error));
                continue;
            }
        };
        if status >= 400 {
            pages.push(match keenable_origin_error(status, &body) {
                Some(origin) => entry(url, Some(&origin)),
                None => failed(status_detail(status, &body)),
            });
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&body) else {
            pages.push(failed("Expecting value: line 1 column 1 (char 0)".into()));
            continue;
        };
        let final_url = Some(text(&data["url"]))
            .filter(|u| !u.is_empty())
            .unwrap_or(url);
        let mut page = document(final_url, text(&data["title"]), text(&data["content"]));
        page["metadata"]["sourceURL"] = json!(url);
        pages.push(page);
    }
    pages
}

// ---- xAI (search only, through the Responses API web_search tool) ----

fn xai_prompt(query: &str, limit: usize) -> String {
    format!(
        "Use the web_search tool to find current information for the query below, then respond with ONLY a single JSON object \u{2014} no prose, no markdown fences, no inline citation links \u{2014} matching this exact schema:\n\n{{\"results\": [{{\"title\": \"string\", \"url\": \"string\", \"description\": \"1-2 sentence summary\"}}]}}\n\nReturn at most {limit} results, ordered by relevance, with absolute https:// URLs. If no usable results exist, return {{\"results\": []}}.\n\nQuery: {query}"
    )
}

/// A row with the title first (the historical shape of the xAI, Brave and Tavily results).
fn title_hit(title: &str, url: &str, description: &str, position: usize) -> Value {
    json!({"title": title, "url": url, "description": description, "position": position})
}

/// Python's `str(value).strip()` for the JSON values a result row can hold.
fn py_str(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::String(string)) => string.trim().to_owned(),
        Some(other) => python_repr(other).trim().to_owned(),
    }
}

fn xai_rows_from_json(block: &str, limit: usize) -> Option<Vec<Value>> {
    let start = block.find('{');
    let end = block.rfind('}');
    let mut candidates = vec![block.to_owned()];
    if let (Some(start), Some(end)) = (start, end) {
        if end >= start && &block[start..=end] != block {
            candidates.push(block[start..=end].to_owned());
        }
    }
    for candidate in candidates {
        let Ok(parsed) = serde_json::from_str::<Value>(&candidate) else {
            continue;
        };
        let Some(results) = parsed.get("results").and_then(Value::as_array) else {
            continue;
        };
        let mut rows: Vec<Value> = Vec::new();
        for row in results.iter().take(limit) {
            let Some(object) = row.as_object() else {
                continue;
            };
            let url = py_str(object.get("url"));
            if !url.is_empty() {
                let position = rows.len() + 1;
                rows.push(title_hit(
                    &py_str(object.get("title")),
                    &url,
                    &py_str(object.get("description")),
                    position,
                ));
            }
        }
        if !rows.is_empty() {
            return Some(rows);
        }
    }
    None
}

fn xai_rows_from_annotations(annotations: &[&Value], joined: &str, limit: usize) -> Vec<Value> {
    let chars: Vec<char> = joined.chars().collect();
    let mut seen: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    for annotation in annotations {
        let url = if annotation["type"] == "url_citation" {
            py_str(annotation.get("url"))
        } else {
            String::new()
        };
        if url.is_empty() || seen.contains(&url) {
            continue;
        }
        seen.push(url.clone());
        let mut description = String::new();
        if let (Some(start), Some(end)) = (
            annotation["start_index"].as_i64(),
            annotation["end_index"].as_i64(),
        ) {
            if 0 <= start && start < end && end as usize <= chars.len() {
                let from = (start as usize).saturating_sub(200);
                description = chars[from..start as usize]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_owned();
            }
        }
        let position = rows.len() + 1;
        rows.push(title_hit("", &url, &description, position));
        if rows.len() >= limit {
            break;
        }
    }
    rows
}

fn xai_rows(response: &Value, limit: usize) -> Vec<Value> {
    let mut blocks: Vec<String> = Vec::new();
    let mut annotations: Vec<&Value> = Vec::new();
    for item in response["output"].as_array().into_iter().flatten() {
        if item["type"] != "message" {
            continue;
        }
        for chunk in item["content"].as_array().into_iter().flatten() {
            if chunk["type"] != "output_text" {
                continue;
            }
            if let Some(block) = chunk["text"]
                .as_str()
                .filter(|block| !block.trim().is_empty())
            {
                blocks.push(block.to_owned());
            }
            annotations.extend(
                chunk["annotations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|a| a.is_object()),
            );
        }
    }
    if let Some(rows) = blocks
        .iter()
        .find_map(|block| xai_rows_from_json(block, limit))
    {
        return rows;
    }
    if !annotations.is_empty() {
        let rows = xai_rows_from_annotations(&annotations, &blocks.join("\n"), limit);
        if !rows.is_empty() {
            return rows;
        }
    }
    response["citations"]
        .as_array()
        .into_iter()
        .flatten()
        .take(limit)
        .enumerate()
        .filter_map(|(index, url)| {
            url.as_str()
                .filter(|u| !u.trim().is_empty())
                .map(|u| title_hit("", u, "", index + 1))
        })
        .collect()
}

pub(crate) async fn xai_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let Some(key) = settings.xai_key.as_deref() else {
        return failure("No xAI credentials found. Set XAI_API_KEY (Grok OAuth sign-in is not available in the Rust CLI).");
    };
    let base = settings
        .xai_base
        .as_deref()
        .unwrap_or("https://api.x.ai/v1");
    let config = &settings.xai;
    if !config.allowed_domains.is_empty() && !config.excluded_domains.is_empty() {
        return failure("web.xai.allowed_domains and web.xai.excluded_domains cannot both be set (xAI restriction).");
    }
    let mut tool = json!({"type": "web_search"});
    if !config.allowed_domains.is_empty() {
        tool["filters"] = json!({"allowed_domains": config.allowed_domains});
    } else if !config.excluded_domains.is_empty() {
        tool["filters"] = json!({"excluded_domains": config.excluded_domains});
    }
    let payload = json!({
        "model": config.model.as_deref().unwrap_or("grok-build-0.1"),
        "input": [{"role": "user", "content": xai_prompt(query, limit)}],
        "tools": [tool],
        "include": ["no_inline_citations"]
    });
    let timeout = config
        .timeout_secs
        .filter(|secs| *secs > 0.0)
        .unwrap_or(90.0);
    let http = match client(Duration::from_secs_f64(timeout)) {
        Ok(http) => http,
        Err(error) => return failure(format!("Could not reach xAI: {error}")),
    };
    let request = http
        .post(format!("{base}/responses"))
        .bearer_auth(key)
        .header(
            "User-Agent",
            concat!("Hermes-Agent/", env!("CARGO_PKG_VERSION")),
        )
        .json(&payload);
    let (status, body) = match send_text(request).await {
        Ok(reply) => reply,
        Err(error) => return failure(format!("Could not reach xAI: {error}")),
    };
    if status >= 400 {
        let shown: String = body.chars().take(300).collect();
        return failure(
            format!("xAI web search returned HTTP {status}: {shown}")
                .trim_end()
                .to_owned(),
        );
    }
    let Ok(data) = serde_json::from_str::<Value>(&body) else {
        return failure("Could not parse xAI Responses API reply as JSON");
    };
    if let Some(error) = data.get("error").filter(|error| error.is_object()) {
        let message = ["message", "code"]
            .iter()
            .map(|key| text(&error[*key]))
            .find(|message| !message.is_empty())
            .unwrap_or("unknown error");
        return failure(format!("xAI returned an error: {message}"));
    }
    search_ok(xai_rows(&data, limit))
}
