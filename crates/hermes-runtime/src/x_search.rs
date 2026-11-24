//! `x_search`: read-only X (Twitter) search backed by xAI's built-in `x_search` Responses API
//! tool. Mirrors `tools/x_search_tool.py`: `XAI_API_KEY` wins over `xai-oauth`, client-side date
//! and handle validation, retry on transient 429/5xx, and `degraded` reporting when active filters
//! produce zero citations.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub(crate) const X_SEARCH: &str = "x_search";
pub const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const DEFAULT_X_SEARCH_MODEL: &str = "grok-4.5";
const DEFAULT_TIMEOUT_SECS: u64 = 180;
const MIN_TIMEOUT_SECS: u64 = 30;
const DEFAULT_RETRIES: u32 = 2;
const MAX_HANDLES: usize = 10;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct XSearchSettings {
    pub api_key: Option<String>,
    /// Credential source (`"api_key"` or `"oauth"`).
    pub credential_source: String,
    pub base_url: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub timeout_seconds: u64,
    pub retries: u32,
}

impl Default for XSearchSettings {
    fn default() -> Self {
        Self {
            api_key: None,
            credential_source: String::new(),
            base_url: DEFAULT_XAI_BASE_URL.to_owned(),
            model: DEFAULT_X_SEARCH_MODEL.to_owned(),
            reasoning_effort: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECS,
            retries: DEFAULT_RETRIES,
        }
    }
}

impl XSearchSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        secrets: &HashMap<String, String>,
        profile: Option<&Path>,
    ) -> Self {
        let section = config.get("x_search");
        let get_str = |key: &str| {
            section
                .and_then(|s| s.get(key))
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let get_u64 = |key: &str| {
            section.and_then(|s| s.get(key)).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str()?.trim().parse::<u64>().ok())
            })
        };

        // Explicit `XAI_API_KEY` wins over `xai-oauth` (`prefer_api_key=True` in x_search_tool.py).
        let explicit_key = std::env::var("XAI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                secrets
                    .get("XAI_API_KEY")
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
            });
        let (api_key, credential_source) = if let Some(k) = explicit_key {
            (Some(k), "api_key".to_owned())
        } else if let Some(home) = profile {
            match crate::providers::oauth::resolve_oauth_credentials(
                Some(home),
                "xai-oauth",
                secrets,
            ) {
                Ok(creds) if !creds.access_token.trim().is_empty() => {
                    (Some(creds.access_token), "oauth".to_owned())
                }
                _ => (None, String::new()),
            }
        } else {
            (None, String::new())
        };

        let base_url = get_str("base_url")
            .or_else(|| {
                std::env::var("XAI_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_XAI_BASE_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let model = get_str("model").unwrap_or_else(|| DEFAULT_X_SEARCH_MODEL.to_owned());
        let reasoning_effort = get_str("reasoning_effort");
        let timeout_seconds = get_u64("timeout_seconds")
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .max(MIN_TIMEOUT_SECS);
        let retries = get_u64("retries").unwrap_or(DEFAULT_RETRIES as u64) as u32;

        Self {
            api_key,
            credential_source,
            base_url,
            model,
            reasoning_effort,
            timeout_seconds,
            retries,
        }
    }

    pub fn is_available(&self) -> bool {
        self.api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
    }
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/x_search.json"))
            .unwrap_or_default()
    })
}

fn validate_handles(raw: Option<&Value>, field: &str) -> Result<Vec<String>, String> {
    let Some(val) = raw.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let arr = val
        .as_array()
        .ok_or_else(|| format!("{field} must be an array of X handles"))?;
    let mut cleaned = Vec::new();
    for item in arr {
        let Some(s) = item.as_str() else {
            return Err(format!("{field} entries must be strings"));
        };
        let handle = s.trim().trim_start_matches('@').trim();
        if !handle.is_empty() {
            cleaned.push(handle.to_owned());
        }
    }
    if cleaned.len() > MAX_HANDLES {
        return Err(format!(
            "{field} accepts at most {MAX_HANDLES} handles, got {}",
            cleaned.len()
        ));
    }
    Ok(cleaned)
}

fn validate_date(raw: Option<&Value>, field: &str) -> Result<Option<NaiveDate>, String> {
    let Some(val) = raw.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let s = val
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{field} must be a YYYY-MM-DD date string"))?;
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| format!("{field} must be a valid YYYY-MM-DD date (got {s:?})"))
}

pub fn build_tool_def(args: &Value) -> Result<(Value, Vec<String>), String> {
    let allowed = validate_handles(args.get("allowed_x_handles"), "allowed_x_handles")?;
    let excluded = validate_handles(args.get("excluded_x_handles"), "excluded_x_handles")?;
    if !allowed.is_empty() && !excluded.is_empty() {
        return Err("allowed_x_handles and excluded_x_handles cannot both be set".into());
    }
    let from_date = validate_date(args.get("from_date"), "from_date")?;
    let to_date = validate_date(args.get("to_date"), "to_date")?;
    if let (Some(f), Some(t)) = (from_date, to_date) {
        if f > t {
            return Err(format!("from_date ({f}) must not be after to_date ({t})"));
        }
    }

    let mut tool_def = Map::new();
    tool_def.insert("type".into(), json!("x_search"));
    let mut active_filters = Vec::new();
    if !allowed.is_empty() {
        tool_def.insert("allowed_x_handles".into(), json!(allowed));
        active_filters.push("allowed_x_handles".to_owned());
    }
    if !excluded.is_empty() {
        tool_def.insert("excluded_x_handles".into(), json!(excluded));
        active_filters.push("excluded_x_handles".to_owned());
    }
    if let Some(f) = from_date {
        tool_def.insert("from_date".into(), json!(f.format("%Y-%m-%d").to_string()));
        active_filters.push("from_date".to_owned());
    }
    if let Some(t) = to_date {
        tool_def.insert("to_date".into(), json!(t.format("%Y-%m-%d").to_string()));
        active_filters.push("to_date".to_owned());
    }
    if args
        .get("enable_image_understanding")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        tool_def.insert("enable_image_understanding".into(), json!(true));
    }
    if args
        .get("enable_video_understanding")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        tool_def.insert("enable_video_understanding".into(), json!(true));
    }
    Ok((Value::Object(tool_def), active_filters))
}

fn extract_responses_result(body: &Value) -> (String, Vec<Value>, Vec<Value>) {
    let mut text = String::new();
    let mut citations = Vec::new();
    let mut inline_citations = Vec::new();

    if let Some(top_citations) = body.get("citations").and_then(Value::as_array) {
        citations.extend(top_citations.iter().cloned());
    }
    if let Some(output_text) = body.get("output_text").and_then(Value::as_str) {
        text.push_str(output_text);
    }
    if let Some(outputs) = body.get("output").and_then(Value::as_array) {
        for item in outputs {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for block in content {
                    if text.is_empty() {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                        }
                    }
                    if let Some(ann) = block.get("annotations").and_then(Value::as_array) {
                        for a in ann {
                            if a.get("type").and_then(Value::as_str) == Some("url_citation")
                                || a.get("url").is_some()
                            {
                                inline_citations.push(a.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    (text, citations, inline_citations)
}

pub(crate) async fn execute(arguments: &str, settings: &XSearchSettings) -> Value {
    let err_json = |msg: String| {
        json!({
            "success": false,
            "provider": "xai",
            "tool": "x_search",
            "error": msg
        })
    };
    let Some(api_key) = settings.api_key.as_deref().filter(|k| !k.trim().is_empty()) else {
        return err_json(
            "x_search requires XAI_API_KEY or xai-oauth credentials in auth.json.".into(),
        );
    };
    let args: Value = match serde_json::from_str(arguments) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return err_json("x_search expects a JSON object".into()),
    };
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if query.is_empty() {
        return err_json("query is required for x_search".into());
    }
    let (tool_def, active_filters) = match build_tool_def(&args) {
        Ok(pair) => pair,
        Err(msg) => return err_json(msg),
    };
    if let Some(effort) = settings.reasoning_effort.as_deref() {
        if !matches!(effort, "low" | "medium" | "high") {
            return err_json(format!(
                "x_search.reasoning_effort must be one of: low, medium, high (got {effort:?})"
            ));
        }
    }

    let mut payload = json!({
        "model": settings.model,
        "input": query,
        "tools": [tool_def],
    });
    if let Some(effort) = settings.reasoning_effort.as_deref() {
        payload["reasoning"] = json!({"effort": effort});
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(settings.timeout_seconds))
        .build()
    {
        Ok(c) => c,
        Err(e) => return err_json(format!("HTTP client error: {e}")),
    };
    let endpoint = format!("{}/responses", settings.base_url);
    let max_attempts = settings.retries.saturating_add(1);
    let mut last_error = String::new();
    let mut response_body: Option<Value> = None;

    for attempt in 0..max_attempts {
        match client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    match serde_json::from_str::<Value>(&body_text) {
                        Ok(parsed) => {
                            response_body = Some(parsed);
                            break;
                        }
                        Err(e) => {
                            return err_json(format!("Invalid JSON from xAI x_search: {e}"));
                        }
                    }
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                last_error = format!(
                    "xAI x_search HTTP {}: {}",
                    status.as_u16(),
                    body_text.chars().take(300).collect::<String>()
                );
                if retryable && attempt + 1 < max_attempts {
                    tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 + 1))).await;
                    continue;
                }
                return err_json(last_error);
            }
            Err(e) => {
                last_error = if e.is_timeout() {
                    format!(
                        "xAI x_search timed out after {} seconds",
                        settings.timeout_seconds
                    )
                } else {
                    format!("xAI x_search request failed: {e}")
                };
                if attempt + 1 < max_attempts {
                    tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 + 1))).await;
                    continue;
                }
                return err_json(last_error);
            }
        }
    }

    let Some(body) = response_body else {
        return err_json(if last_error.is_empty() {
            "x_search request did not return a response".into()
        } else {
            last_error
        });
    };
    let (answer, citations, inline_citations) = extract_responses_result(&body);
    let degraded =
        !active_filters.is_empty() && citations.is_empty() && inline_citations.is_empty();
    let degraded_reason = degraded.then(|| {
        format!(
            "no citations returned despite filters: {}",
            active_filters.join(", ")
        )
    });
    json!({
        "success": true,
        "provider": "xai",
        "credential_source": settings.credential_source,
        "tool": "x_search",
        "answer": answer,
        "citations": citations,
        "inline_citations": inline_citations,
        "degraded": degraded,
        "degraded_reason": degraded_reason,
    })
}
