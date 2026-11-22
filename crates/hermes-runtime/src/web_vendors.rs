//! Exa and Parallel, reached over their REST routes (the reference uses their SDKs).
//!
//! The requests and the texts the model receives follow a recording of the reference with the
//! SDK versions of its lock file (`compat/create_web_sdk_corpus.py`). Without a key the reference
//! falls back to a keyless ring of remote MCP endpoints; this port does not, and says so.

use std::time::Duration;

use serde_json::{json, Value};

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
        .map(|items| {
            items
                .iter()
                .map(|item| text(item))
                .collect::<Vec<_>>()
                .join(separator)
        })
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
