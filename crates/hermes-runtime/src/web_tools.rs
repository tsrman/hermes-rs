//! `web_search` and `web_extract` over HTTP backends.
//!
//! Supported backends: Tavily (search and extract), SearXNG and Brave (search only). The
//! selection order follows `tools/web_tools.py`; a backend that this crate does not implement
//! is reported as such and never replaced silently.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::url_safety::is_safe_url;

pub(crate) const WEB_SEARCH: &str = "web_search";
pub(crate) const WEB_EXTRACT: &str = "web_extract";

const DEFAULT_CHAR_LIMIT: usize = 15_000;
const CHAR_LIMIT_FLOOR: usize = 2_000;
const CHAR_LIMIT_CEILING: usize = 500_000;
const SEARCH_LIMIT_CAP: usize = 20;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const BLOCKED_ERROR: &str = "Blocked: URL targets a private or internal network address";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub enum Backend {
    #[default]
    None,
    Tavily,
    Searxng,
    BraveFree,
    /// A backend of the Python agent that this crate does not implement.
    Unsupported(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WebSettings {
    pub search: Backend,
    pub extract: Backend,
    pub tavily_key: Option<String>,
    pub tavily_base: Option<String>,
    pub searxng_url: Option<String>,
    pub brave_key: Option<String>,
    pub char_limit: Option<usize>,
    pub allow_private_urls: bool,
}

impl WebSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        secrets: &HashMap<String, String>,
    ) -> Self {
        let env = |name: &str| {
            std::env::var(name)
                .ok()
                .or_else(|| secrets.get(name).cloned())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let web = config.get("web");
        let field = |key: &str| {
            web.and_then(|web| web.get(key))
                .and_then(serde_yaml::Value::as_str)
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
        };
        let shared = field("backend");
        let auto = || -> Option<String> {
            let candidates = [
                ("tavily", env("TAVILY_API_KEY").is_some()),
                ("perplexity", env("PERPLEXITY_API_KEY").is_some()),
                ("exa", env("EXA_API_KEY").is_some()),
                ("parallel", env("PARALLEL_API_KEY").is_some()),
                ("keenable", env("KEENABLE_API_KEY").is_some()),
                (
                    "firecrawl",
                    env("FIRECRAWL_API_KEY").is_some() || env("FIRECRAWL_API_URL").is_some(),
                ),
                ("searxng", env("SEARXNG_URL").is_some()),
                ("brave-free", env("BRAVE_SEARCH_API_KEY").is_some()),
            ];
            candidates
                .into_iter()
                .find_map(|(name, available)| available.then(|| name.to_owned()))
        };
        let resolve = |specific: Option<String>| -> Backend {
            let name = specific
                .or_else(|| shared.clone())
                .or_else(auto)
                .unwrap_or_default();
            match name.as_str() {
                "" => Backend::None,
                "tavily" => Backend::Tavily,
                "searxng" => Backend::Searxng,
                "brave-free" => Backend::BraveFree,
                // The managed subscription is served by Firecrawl.
                "nous" => Backend::Unsupported("firecrawl".into()),
                other => Backend::Unsupported(other.to_owned()),
            }
        };
        let allow_private = match std::env::var("HERMES_ALLOW_PRIVATE_URLS")
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("true" | "1" | "yes") => true,
            Some("false" | "0" | "no") => false,
            _ => ["security", "browser"].iter().any(|section| {
                config
                    .get(*section)
                    .and_then(|block| block.get("allow_private_urls"))
                    .is_some_and(|value| crate::config::truthy(value, false))
            }),
        };
        Self {
            search: resolve(field("search_backend")),
            extract: resolve(field("extract_backend")),
            tavily_key: env("TAVILY_API_KEY"),
            tavily_base: env("TAVILY_BASE_URL"),
            searxng_url: env("SEARXNG_URL"),
            brave_key: env("BRAVE_SEARCH_API_KEY"),
            char_limit: web
                .and_then(|web| web.get("extract_char_limit"))
                .and_then(serde_yaml::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok()),
            allow_private_urls: allow_private,
        }
    }

    pub(crate) fn offers_search(&self) -> bool {
        self.search != Backend::None
    }

    pub(crate) fn offers_extract(&self) -> bool {
        self.extract != Backend::None
    }
}

pub(crate) fn search_schema() -> Value {
    schema(include_str!("schemas/web_search.json"))
}

pub(crate) fn extract_schema() -> Value {
    schema(include_str!("schemas/web_extract.json"))
}

fn schema(text: &str) -> Value {
    json!({"type": "function", "function": serde_json::from_str::<Value>(text).unwrap_or_default()})
}

fn pretty(value: &Value) -> Value {
    // Results reach the model as the same indented JSON text the reference produces.
    Value::String(serde_json::to_string_pretty(value).unwrap_or_default())
}

fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .tls_built_in_native_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|error| error.without_url().to_string())
}

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err("response is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn failure(error: impl Into<String>) -> Value {
    json!({"success": false, "error": error.into()})
}

fn row(title: &str, url: &str, description: &str, position: usize) -> Value {
    json!({"title": title, "url": url, "description": description, "position": position})
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Run `web_search`. The result is the text the model receives.
pub async fn search(settings: &WebSettings, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let limit = match &args["limit"] {
        Value::Number(number) => number.as_i64().unwrap_or(5),
        Value::String(text) => text.trim().parse().unwrap_or(5),
        _ => 5,
    }
    .clamp(1, 100) as usize;
    let Some(query) = args["query"]
        .as_str()
        .filter(|query| !query.trim().is_empty())
    else {
        return json!({"error": "web_search needs a non-empty 'query' string.", "success": false})
            .to_string()
            .into();
    };
    let outcome = match &settings.search {
        Backend::Tavily => tavily_search(settings, query, limit).await,
        Backend::Searxng => searxng_search(settings, query, limit).await,
        Backend::BraveFree => brave_search(settings, query, limit).await,
        Backend::Unsupported(name) => failure(unsupported(name)),
        Backend::None => {
            failure("No web search provider configured. Run `hermes tools` to set one up.")
        }
    };
    pretty(&outcome)
}

fn unsupported(name: &str) -> String {
    format!(
        "The web backend '{name}' is not supported by the Rust CLI yet. Set web.backend to tavily, searxng (search only) or brave-free (search only)."
    )
}

fn search_ok(rows: Vec<Value>) -> Value {
    json!({"success": true, "data": {"web": rows}})
}

async fn tavily_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let payload = json!({
        "query": query,
        "max_results": limit.min(SEARCH_LIMIT_CAP),
        "include_raw_content": false,
        "include_images": false
    });
    match tavily_request(settings, "search", &payload).await {
        Ok(response) => search_ok(
            response["results"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, hit)| {
                    row(
                        hit["title"].as_str().unwrap_or(""),
                        hit["url"].as_str().unwrap_or(""),
                        hit["content"].as_str().unwrap_or(""),
                        index + 1,
                    )
                })
                .collect(),
        ),
        Err(message) => failure(message),
    }
}

/// POST to Tavily. A non-2xx answer returns the body so rate-limit and upgrade text reaches
/// the model. Without a key the request is keyless, as in the reference.
async fn tavily_request(
    settings: &WebSettings,
    endpoint: &str,
    payload: &Value,
) -> Result<Value, String> {
    let base = settings
        .tavily_base
        .as_deref()
        .unwrap_or("https://api.tavily.com")
        .trim_end_matches('/');
    let mut request = client(Duration::from_secs(60))
        .map_err(|error| format!("Tavily {endpoint} failed: {error}"))?
        .post(format!("{base}/{endpoint}"))
        .header("X-Client-Name", "hermes-agent")
        .json(payload);
    request = match settings.tavily_key.as_deref() {
        Some(key) => request.bearer_auth(key),
        None => request.header("X-Tavily-Access-Mode", "keyless"),
    };
    let response = request
        .send()
        .await
        .map_err(|error| format!("Tavily {endpoint} failed: {}", error.without_url()))?;
    let status = response.status();
    let body = read_body(response)
        .await
        .map_err(|error| format!("Tavily {endpoint} failed: {error}"))?;
    if status.as_u16() >= 400 {
        let text = String::from_utf8_lossy(&body).trim().to_owned();
        return Err(if text.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            text
        });
    }
    serde_json::from_slice(&body).map_err(|_| format!("Tavily {endpoint} failed: invalid response"))
}

/// GET JSON with the reference's error texts.
async fn get_json(
    label: &str,
    reach: &str,
    url: &str,
    query: &[(&str, String)],
    headers: &[(&str, &str)],
) -> Result<Value, Value> {
    let client = client(Duration::from_secs(15))
        .map_err(|e| failure(format!("Could not reach {reach}: {e}")))?;
    let mut request = client
        .get(url)
        .query(query)
        .header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| failure(format!("Could not reach {reach}: {}", error.without_url())))?;
    let status = response.status();
    if !status.is_success() {
        return Err(failure(format!(
            "{label} returned HTTP {}",
            status.as_u16()
        )));
    }
    let body = read_body(response)
        .await
        .map_err(|error| failure(format!("Could not reach {reach}: {error}")))?;
    serde_json::from_slice(&body)
        .map_err(|_| failure(format!("Could not parse {label} response as JSON")))
}

async fn searxng_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let Some(base) = settings
        .searxng_url
        .as_deref()
        .map(|url| url.trim_end_matches('/'))
    else {
        return failure("SEARXNG_URL is not set");
    };
    let reach = format!("SearXNG at {base}");
    let query = [
        ("q", query.to_owned()),
        ("format", "json".into()),
        ("pageno", "1".into()),
    ];
    let data = match get_json("SearXNG", &reach, &format!("{base}/search"), &query, &[]).await {
        Ok(data) => data,
        Err(failure) => return failure,
    };
    let mut results: Vec<&Value> = data["results"].as_array().into_iter().flatten().collect();
    let score = |hit: &Value| match &hit["score"] {
        Value::Number(number) => number.as_f64().unwrap_or(0.0),
        Value::String(text) => text.parse().unwrap_or(0.0),
        _ => 0.0,
    };
    // A stable sort keeps the engine's order for equal scores, like the reference.
    results.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    search_ok(titled_rows(results.into_iter().take(limit), "content"))
}

async fn brave_search(settings: &WebSettings, query: &str, limit: usize) -> Value {
    let Some(key) = settings.brave_key.as_deref() else {
        return failure("BRAVE_SEARCH_API_KEY is not set");
    };
    let query = [
        ("q", query.to_owned()),
        ("count", limit.clamp(1, 20).to_string()),
    ];
    let headers = [("X-Subscription-Token", key)];
    let data = match get_json(
        "Brave Search",
        "Brave Search",
        "https://api.search.brave.com/res/v1/web/search",
        &query,
        &headers,
    )
    .await
    {
        Ok(data) => data,
        Err(failure) => return failure,
    };
    let results = data["web"]["results"].as_array().into_iter().flatten();
    search_ok(titled_rows(results.take(limit), "description"))
}

fn titled_rows<'a>(results: impl Iterator<Item = &'a Value>, description: &str) -> Vec<Value> {
    results
        .enumerate()
        .map(|(index, hit)| {
            row(
                &text_of(&hit["title"]),
                &text_of(&hit["url"]),
                &text_of(&hit[description]),
                index + 1,
            )
        })
        .collect()
}

/// Prefixes of well-known credential formats. A URL carrying one is refused outright so a
/// secret is never sent to a third-party fetcher.
fn looks_like_secret(text: &str) -> bool {
    const PREFIXES: [&str; 22] = [
        "sk-",
        "sk_live_",
        "sk_test_",
        "pk_live_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "hf_",
        "AKIA",
        "ASIA",
        "AIza",
        "tvly-",
        "fc-",
        "pplx-",
    ];
    let bytes = text.as_bytes();
    PREFIXES.iter().any(|prefix| {
        text.match_indices(prefix).any(|(index, _)| {
            let boundary = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
            let tail = &text[index + prefix.len()..];
            let run = tail
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                .count();
            boundary && run >= 16
        })
    })
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&text[index + 1..index + 3], 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn entry(url: &str, error: Option<&str>) -> Value {
    json!({"url": url, "title": "", "content": "", "error": error})
}

/// Run `web_extract`.
pub async fn extract(settings: &WebSettings, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let items = args["urls"].as_array().cloned().unwrap_or_default();
    let char_limit = args["char_limit"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .or(settings.char_limit)
        .map_or(DEFAULT_CHAR_LIMIT, |value| {
            value.clamp(CHAR_LIMIT_FLOOR, CHAR_LIMIT_CEILING)
        });

    // Normalise the items and refuse the whole call when a URL carries a secret.
    let mut fixed: HashMap<usize, Value> = HashMap::new();
    let mut requested: Vec<(usize, String)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let url = match item {
            Value::String(text) => Some(text.as_str()),
            Value::Object(object) => object
                .get("url")
                .or_else(|| object.get("href"))
                .and_then(Value::as_str),
            _ => None,
        }
        .map(str::trim)
        .filter(|url| !url.is_empty());
        let Some(url) = url else {
            fixed.insert(
                index,
                entry(
                    "",
                    Some(&format!("Invalid URL item at index {index}: expected a URL string or an object with a string 'url' or 'href' field")),
                ),
            );
            continue;
        };
        if looks_like_secret(url) || looks_like_secret(&percent_decoded(url)) {
            return Value::String(
                json!({"success": false, "error": "Blocked: URL contains what appears to be an API key or token. Secrets must not be sent in URLs."})
                    .to_string(),
            );
        }
        requested.push((index, url.to_owned()));
    }

    let mut safe: Vec<(usize, String)> = Vec::new();
    for (index, url) in requested {
        if is_safe_url(&url, settings.allow_private_urls).await {
            safe.push((index, url));
        } else {
            fixed.insert(index, entry(&url, Some(BLOCKED_ERROR)));
        }
    }

    let mut fetched: Vec<Value> = Vec::new();
    if !safe.is_empty() {
        let urls: Vec<String> = safe.iter().map(|(_, url)| url.clone()).collect();
        fetched = match &settings.extract {
            Backend::Tavily => tavily_extract(settings, &urls).await,
            Backend::Searxng | Backend::BraveFree => {
                let name = if settings.extract == Backend::Searxng {
                    "SearXNG"
                } else {
                    "Brave Search (Free)"
                };
                return Value::String(
                    json!({"success": false, "error": format!("{name} is a search-only backend and cannot extract URL content. Set web.extract_backend to firecrawl, tavily, keenable, exa, or parallel.")})
                        .to_string(),
                );
            }
            Backend::Unsupported(name) => {
                return Value::String(
                    json!({"success": false, "error": unsupported(name)}).to_string(),
                );
            }
            Backend::None => {
                return Value::String(
                    json!({"success": false, "error": "No web extract provider configured. Set web.extract_backend to firecrawl, tavily, keenable, exa, or parallel."})
                        .to_string(),
                );
            }
        };
        // A page reported under a URL nobody asked for (a redirect target) is checked again.
        for page in &mut fetched {
            let url = page["url"].as_str().unwrap_or("").to_owned();
            if !url.is_empty()
                && !urls.contains(&url)
                && page["error"].is_null()
                && !is_safe_url(&url, settings.allow_private_urls).await
            {
                *page = json!({
                    "url": url, "title": page["title"], "content": "", "raw_content": "", "error": BLOCKED_ERROR
                });
            }
        }
    }

    // Rebuild the input order across invalid, blocked and fetched entries.
    let mut results = Vec::with_capacity(items.len());
    for index in 0..items.len() {
        if let Some(fixed) = fixed.remove(&index) {
            results.push(fixed);
            continue;
        }
        let position = safe.iter().position(|(safe_index, _)| *safe_index == index);
        let page = position.and_then(|position| fetched.get(position).cloned());
        results.push(page.unwrap_or_else(|| {
            entry(
                position.map_or("", |position| safe[position].1.as_str()),
                Some("Extract backend returned no result for this URL"),
            )
        }));
    }

    let trimmed: Vec<Value> = results
        .into_iter()
        .map(|page| {
            let content = page["content"].as_str().unwrap_or("");
            let content = truncate(&strip_base64_images(content), char_limit);
            json!({
                "url": page["url"].as_str().unwrap_or(""),
                "title": page["title"].as_str().unwrap_or(""),
                "content": content,
                "error": page["error"],
            })
        })
        .collect();
    if trimmed.is_empty() {
        return Value::String(
            json!({"error": "Content was inaccessible or not found"}).to_string(),
        );
    }
    pretty(&json!({"results": trimmed}))
}

async fn tavily_extract(settings: &WebSettings, urls: &[String]) -> Vec<Value> {
    let failed_all = |message: &str| -> Vec<Value> {
        urls.iter().map(|url| entry(url, Some(message))).collect()
    };
    let response = match tavily_request(
        settings,
        "extract",
        &json!({"urls": urls, "include_images": false}),
    )
    .await
    {
        Ok(response) => response,
        Err(message) => return failed_all(&message),
    };
    let mut pages: Vec<Value> = response["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|result| {
            let content = result["raw_content"]
                .as_str()
                .filter(|text| !text.is_empty())
                .or_else(|| result["content"].as_str())
                .unwrap_or("");
            json!({
                "url": result["url"].as_str().unwrap_or(""),
                "title": result["title"].as_str().unwrap_or(""),
                "content": content,
                "error": null
            })
        })
        .collect();
    for failed in response["failed_results"].as_array().into_iter().flatten() {
        pages.push(entry(
            failed["url"].as_str().unwrap_or(""),
            Some(failed["error"].as_str().unwrap_or("extraction failed")),
        ));
    }
    for url in response["failed_urls"].as_array().into_iter().flatten() {
        pages.push(entry(&text_of(url), Some("extraction failed")));
    }
    // Providers may omit failures or answer out of order. A page is matched to its request by
    // URL; a page reported under another URL (a redirect) fills the next request left without one.
    let mut used = vec![false; pages.len()];
    let mut slots: Vec<Option<usize>> = urls
        .iter()
        .map(|url| {
            let found = pages.iter().enumerate().position(|(index, page)| {
                !used[index] && page["url"].as_str() == Some(url.as_str())
            });
            if let Some(index) = found {
                used[index] = true;
            }
            found
        })
        .collect();
    for slot in slots.iter_mut().filter(|slot| slot.is_none()) {
        if let Some(index) = used.iter().position(|taken| !taken) {
            used[index] = true;
            *slot = Some(index);
        }
    }
    urls.iter()
        .zip(slots)
        .map(|(url, slot)| {
            slot.map_or_else(
                || entry(url, Some("Extract backend returned no result for this URL")),
                |index| pages[index].clone(),
            )
        })
        .collect()
}

/// Replace inline base64 images, which can cost hundreds of thousands of tokens.
fn strip_base64_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("data:image/") {
        let after = &rest[start..];
        let Some(marker) = after.find(";base64,") else {
            break;
        };
        if after[..marker].contains(char::is_whitespace) || marker > 40 {
            out.push_str(&rest[..start + 11]);
            rest = &rest[start + 11..];
            continue;
        }
        let payload = after[marker + 8..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
            .count();
        out.push_str(&rest[..start]);
        out.push_str("[IMAGE]");
        rest = &rest[start + marker + 8 + payload..];
    }
    out.push_str(rest);
    out
}

/// Pages over the budget become a head (75%) and tail (25%) window cut on line boundaries.
fn truncate(content: &str, limit: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= limit {
        return content.to_owned();
    }
    let head_budget = limit * 3 / 4;
    let tail_budget = limit - head_budget;
    let mut head: String = chars[..head_budget].iter().collect();
    let mut tail: String = chars[chars.len() - tail_budget..].iter().collect();
    if let Some(newline) = head
        .rfind('\n')
        .filter(|&n| n as f64 > head_budget as f64 * 0.5)
    {
        head.truncate(newline);
    }
    if let Some(newline) = tail
        .find('\n')
        .filter(|&n| (n as f64) < tail_budget as f64 * 0.5)
    {
        tail = tail[newline + 1..].to_owned();
    }
    format!(
        "{head}\n\n{rule} [TRUNCATED] {rule}\nShowing {} chars (head) + {} chars (tail) of {} total clean characters.\n{tail}",
        head.chars().count(),
        tail.chars().count(),
        chars.len(),
        rule = "─".repeat(8),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_secret_prefixed_urls_without_flagging_ordinary_text() {
        assert!(looks_like_secret(
            "https://x.test/?k=sk-abcdefghijklmnop1234"
        ));
        assert!(looks_like_secret(
            "https://x.test/?k=ghp_abcdefghijklmnop1234"
        ));
        assert!(looks_like_secret(&percent_decoded(
            "https://x.test/?k=sk%2Dabcdefghijklmnop1234"
        )));
        assert!(!looks_like_secret("https://x.test/sk-short"));
        assert!(!looks_like_secret(
            "https://x.test/task-abcdefghijklmnopqrstuvwxyz"
        ));
        assert!(!looks_like_secret("https://example.com/docs/page"));
    }

    #[test]
    fn strips_base64_images_and_keeps_real_links() {
        let text = "a ![pic](data:image/png;base64,AAAA+/==) b data:image/gif;base64,BBBB c ![x](https://x.test/i.png)";
        let cleaned = strip_base64_images(text);
        assert!(!cleaned.contains("base64"), "{cleaned}");
        assert!(cleaned.contains("https://x.test/i.png"));
    }

    #[test]
    fn truncates_with_a_head_and_tail_window() {
        let page: String = (0..3000).map(|n| format!("line {n}\n")).collect();
        let cut = truncate(&page, 2000);
        assert!(cut.starts_with("line 0\n"));
        assert!(cut.contains("[TRUNCATED]"));
        assert!(cut.trim_end().ends_with("line 2999"));
        assert!(cut.chars().count() < 2400);
        assert_eq!(truncate("short", 2000), "short");
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    fn load(yaml: &str, secrets: &[(&str, &str)]) -> WebSettings {
        let config = serde_yaml::from_str(yaml).unwrap();
        let secrets = secrets
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        WebSettings::from_config(&config, &secrets)
    }

    #[test]
    fn selection_follows_the_reference_precedence() {
        // No backend at all: the tools are not offered.
        let none = load("{}", &[]);
        assert!(!none.offers_search() && !none.offers_extract());
        // Autodetect: Tavily outranks SearXNG, which outranks Brave.
        let auto = load(
            "{}",
            &[
                ("SEARXNG_URL", "http://s"),
                ("BRAVE_SEARCH_API_KEY", "b"),
                ("TAVILY_API_KEY", "t"),
            ],
        );
        assert_eq!(
            (auto.search.clone(), auto.extract.clone()),
            (Backend::Tavily, Backend::Tavily)
        );
        let searx = load(
            "{}",
            &[("SEARXNG_URL", "http://s"), ("BRAVE_SEARCH_API_KEY", "b")],
        );
        assert_eq!(searx.search, Backend::Searxng);
        // A key for a backend that is not implemented still wins its slot, so it is never skipped silently.
        let exa = load("{}", &[("EXA_API_KEY", "e"), ("SEARXNG_URL", "http://s")]);
        assert_eq!(exa.search, Backend::Unsupported("exa".into()));
        // Per-capability pins beat the shared backend.
        let pinned = load(
            "web: {backend: tavily, search_backend: Brave-Free, extract_backend: firecrawl}",
            &[],
        );
        assert_eq!(pinned.search, Backend::BraveFree);
        assert_eq!(pinned.extract, Backend::Unsupported("firecrawl".into()));
        assert_eq!(
            load("web: {backend: nous}", &[]).search,
            Backend::Unsupported("firecrawl".into())
        );
    }

    #[test]
    fn private_url_opt_in_reads_the_security_and_legacy_browser_keys() {
        assert!(!load("{}", &[]).allow_private_urls);
        assert!(load("security: {allow_private_urls: true}", &[]).allow_private_urls);
        assert!(load("browser: {allow_private_urls: 'yes'}", &[]).allow_private_urls);
        assert_eq!(
            load("web: {extract_char_limit: 4000}", &[]).char_limit,
            Some(4000)
        );
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn schemas_equal_the_definitions_recorded_from_the_reference() {
        let recorded: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/default_tools.json")).unwrap();
        for (name, schema) in [
            (WEB_SEARCH, search_schema()),
            (WEB_EXTRACT, extract_schema()),
        ] {
            let reference = recorded
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .unwrap_or_else(|| panic!("{name} is not in the reference default tools"));
            assert_eq!(&schema, reference, "{name}");
        }
    }
}
