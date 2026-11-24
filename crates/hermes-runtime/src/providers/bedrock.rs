//! AWS Bedrock provider support: pure-Rust AWS SigV4 signing, Bedrock Converse translation &
//! response normalization, and Bedrock Mantle OpenAI-Responses routing.
//!
//! Mirrors `agent/bedrock_adapter.py` and `hermes_cli/runtime_provider_backends.py` with zero
//! external AWS SDK dependency (uses `ring::hmac` + `ring::digest::SHA256`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use ring::{digest, hmac};
use serde_json::{json, Map, Value};

use super::openai::build_client;
use super::{Completion, ProviderError, ToolCall, Usage};

pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";
const EMPTY_TEXT_PLACEHOLDER: &str = "(empty)";

const BEDROCK_PROFILE_PREFIXES: &[&str] = &[
    "global", "us", "eu", "apac", "ap", "au", "jp", "ca", "sa", "me", "af",
];
const BEDROCK_OPENAI_PROFILE_PREFIXES: &[&str] = &[
    "global", "us", "eu", "apac", "ap", "au", "jp", "ca", "sa", "me", "af", "in",
];

pub const BEDROCK_OPENAI_RESPONSES_MODEL_IDS: &[&str] = &[
    "openai.gpt-6-astra",
    "openai.gpt-6.1-sol",
    "openai.gpt-6.1-terra",
    "openai.gpt-6-sol",
    "openai.gpt-6-luna",
    "openai.gpt-5.5",
    "openai.gpt-5.6-sol",
    "openai.gpt-5.6-terra",
    "openai.gpt-5.6-luna",
    "openai.gpt-5.4",
];

#[derive(Clone, Debug, Default)]
pub struct BedrockSettings {
    pub region: Option<String>,
    pub guardrail_id: Option<String>,
    pub guardrail_version: Option<String>,
    pub stream_processing_mode: Option<String>,
    pub trace: Option<String>,
}

impl BedrockSettings {
    pub fn from_config(config: &serde_yaml::Value) -> Self {
        let section = config.get("bedrock");
        let get_str = |obj: Option<&serde_yaml::Value>, key: &str| {
            obj.and_then(|s| s.get(key))
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let guardrail = section.and_then(|s| s.get("guardrail"));
        Self {
            region: get_str(section, "region"),
            guardrail_id: get_str(guardrail, "guardrail_identifier")
                .or_else(|| get_str(guardrail, "id"))
                .or_else(|| get_str(guardrail, "guardrailIdentifier")),
            guardrail_version: get_str(guardrail, "guardrail_version")
                .or_else(|| get_str(guardrail, "version"))
                .or_else(|| get_str(guardrail, "guardrailVersion")),
            stream_processing_mode: get_str(guardrail, "stream_processing_mode")
                .or_else(|| get_str(guardrail, "streamProcessingMode")),
            trace: get_str(guardrail, "trace"),
        }
    }

    pub fn guardrail_config(&self) -> Option<Value> {
        let id = self.guardrail_id.as_deref()?;
        let version = self.guardrail_version.as_deref().unwrap_or("DRAFT");
        let mut obj = Map::new();
        obj.insert("guardrailIdentifier".into(), json!(id));
        obj.insert("guardrailVersion".into(), json!(version));
        if let Some(mode) = self.stream_processing_mode.as_deref() {
            obj.insert("streamProcessingMode".into(), json!(mode));
        }
        if let Some(trace) = self.trace.as_deref() {
            obj.insert("trace".into(), json!(trace));
        }
        Some(Value::Object(obj))
    }
}

/// `(bare_id, profile_prefix)` for a Bedrock-hosted OpenAI GPT model, else `None`.
/// Matches `agent/bedrock_adapter.py::parse_bedrock_openai_model_id`.
pub fn parse_bedrock_openai_model_id(model_id: &str) -> Option<(String, String)> {
    let normalized = model_id.trim().to_ascii_lowercase();
    let (candidate, prefix) = if let Some((pref, rest)) = normalized.split_once('.') {
        if BEDROCK_OPENAI_PROFILE_PREFIXES.contains(&pref) && rest.starts_with("openai.") {
            (rest.to_owned(), pref.to_owned())
        } else {
            (normalized, String::new())
        }
    } else {
        (normalized, String::new())
    };
    BEDROCK_OPENAI_RESPONSES_MODEL_IDS
        .contains(&candidate.as_str())
        .then_some((candidate, prefix))
}

/// True when the model uses Bedrock Mantle's OpenAI Responses endpoint (bare in-Region ID).
pub fn bedrock_openai_uses_mantle(model_id: &str) -> bool {
    parse_bedrock_openai_model_id(model_id).is_some_and(|(_, prefix)| prefix.is_empty())
}

/// True for Claude on Bedrock (`anthropic.claude-*`, with optional regional/global prefix).
pub fn is_anthropic_bedrock_model(model_id: &str) -> bool {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.starts_with("anthropic.claude") {
        return true;
    }
    if let Some((prefix, rest)) = lower.split_once('.') {
        return BEDROCK_PROFILE_PREFIXES.contains(&prefix) && rest.starts_with("anthropic.claude");
    }
    false
}

pub fn bedrock_openai_base_url(region: &str) -> String {
    let region = if region.trim().is_empty() {
        DEFAULT_BEDROCK_REGION
    } else {
        region.trim()
    };
    format!("https://bedrock-mantle.{region}.api.aws/openai/v1")
}

pub fn bedrock_runtime_base_url(region: &str) -> String {
    let region = if region.trim().is_empty() {
        DEFAULT_BEDROCK_REGION
    } else {
        region.trim()
    };
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

// ── AWS Credentials + Pure-Rust SigV4 Signer ────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum AwsAuth {
    Bearer(String),
    SigV4 {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

/// Parse `~/.aws/credentials` INI file for `profile_name`.
pub fn load_aws_credentials_profile(path: &std::path::Path, profile_name: &str) -> Option<AwsAuth> {
    let raw = std::fs::read_to_string(path).ok()?;
    let target = format!("[{profile_name}]");
    let mut in_section = false;
    let mut ak = None;
    let mut sk = None;
    let mut st = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == target;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_owned();
            if val.is_empty() {
                continue;
            }
            match key.as_str() {
                "aws_access_key_id" => ak = Some(val),
                "aws_secret_access_key" => sk = Some(val),
                "aws_session_token" => st = Some(val),
                _ => {}
            }
        }
    }
    Some(AwsAuth::SigV4 {
        access_key_id: ak?,
        secret_access_key: sk?,
        session_token: st,
    })
}

pub fn resolve_bedrock_region(
    settings: &BedrockSettings,
    secrets: &HashMap<String, String>,
) -> String {
    let secret = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| secrets.get(name).cloned())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    settings
        .region
        .clone()
        .or_else(|| secret("AWS_REGION"))
        .or_else(|| secret("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| DEFAULT_BEDROCK_REGION.to_owned())
}

pub fn resolve_aws_auth(secrets: &HashMap<String, String>) -> Result<AwsAuth, String> {
    let secret = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| secrets.get(name).cloned())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    if let Some(bearer) = secret("AWS_BEARER_TOKEN_BEDROCK") {
        return Ok(AwsAuth::Bearer(bearer));
    }
    if let (Some(ak), Some(sk)) = (secret("AWS_ACCESS_KEY_ID"), secret("AWS_SECRET_ACCESS_KEY")) {
        return Ok(AwsAuth::SigV4 {
            access_key_id: ak,
            secret_access_key: sk,
            session_token: secret("AWS_SESSION_TOKEN"),
        });
    }
    let profile = secret("AWS_PROFILE").unwrap_or_else(|| "default".to_owned());
    let creds_path = secret("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".aws/credentials"))
        });
    if let Some(path) = creds_path {
        if let Some(auth) = load_aws_credentials_profile(&path, &profile) {
            return Ok(auth);
        }
    }
    Err(
        "No AWS credentials found for Bedrock. Set AWS_BEARER_TOKEN_BEDROCK, AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY, or configure ~/.aws/credentials."
            .into(),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let sum = digest::digest(&digest::SHA256, bytes);
    sum.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&k, data).as_ref().to_vec()
}

/// URI-encode a path segment according to AWS SigV4 rules (unreserved: A-Z a-z 0-9 - _ . ~).
pub fn sigv4_uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && byte == b'/');
        if unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Compute AWS Signature Version 4 headers (`authorization`, `x-amz-date`, `x-amz-content-sha256`,
/// and optional `x-amz-security-token`) for an HTTP request.
#[allow(clippy::too_many_arguments)]
pub fn sign_sigv4(
    method: &str,
    url: &reqwest::Url,
    body: &[u8],
    service: &str,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<(String, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(body);

    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };

    let mut canonical_headers_map = BTreeMap::new();
    canonical_headers_map.insert("host", host);
    canonical_headers_map.insert("x-amz-content-sha256", payload_hash.clone());
    canonical_headers_map.insert("x-amz-date", amz_date.clone());
    if let Some(tok) = session_token {
        canonical_headers_map.insert("x-amz-security-token", tok.to_owned());
    }

    let canonical_headers: String = canonical_headers_map
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers: String = canonical_headers_map
        .keys()
        .copied()
        .collect::<Vec<_>>()
        .join(";");

    // Canonical URI: decode once if already percent-encoded, then SigV4-encode (keeping '/').
    let raw_path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let canonical_uri = sigv4_uri_encode(raw_path, false);

    let mut query_pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (sigv4_uri_encode(&k, true), sigv4_uri_encode(&v, true)))
        .collect();
    query_pairs.sort();
    let canonical_querystring = query_pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_querystring}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature: String = hmac_sha256(&k_signing, string_to_sign.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    let mut headers = vec![
        ("authorization".into(), authorization),
        ("x-amz-date".into(), amz_date),
        ("x-amz-content-sha256".into(), payload_hash),
    ];
    if let Some(tok) = session_token {
        headers.push(("x-amz-security-token".into(), tok.to_owned()));
    }
    headers
}

// ── OpenAI <-> Bedrock Converse translation ─────────────────────────────────────────

fn safe_text(value: Option<&Value>) -> String {
    let raw = match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    if raw.trim().is_empty() {
        EMPTY_TEXT_PLACEHOLDER.to_owned()
    } else {
        raw
    }
}

/// Convert OpenAI tool definitions into Bedrock Converse `toolSpec` list.
/// Matches `agent/bedrock_adapter.py::convert_tools_to_converse`.
pub fn convert_tools_to_converse(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let func = tool.get("function").unwrap_or(&Value::Null);
            let name = func["name"].as_str().unwrap_or_default();
            let description = func["description"].as_str().unwrap_or_default();
            let schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            json!({
                "toolSpec": {
                    "name": name,
                    "description": description,
                    "inputSchema": {
                        "json": schema,
                    }
                }
            })
        })
        .collect()
}

fn convert_content_to_converse(content: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(parts)) = content else {
        return vec![json!({"text": safe_text(content)})];
    };
    let mut blocks = Vec::new();
    for part in parts {
        if let Some(s) = part.as_str() {
            blocks.push(json!({"text": safe_text(Some(&Value::String(s.to_owned())))}));
        } else if let Some(obj) = part.as_object() {
            match obj.get("type").and_then(Value::as_str) {
                Some("text") => {
                    blocks.push(json!({"text": safe_text(obj.get("text"))}));
                }
                Some("image_url") => {
                    let url = obj
                        .get("image_url")
                        .and_then(|u| u.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some((fmt, b64)) = parse_bedrock_image_data_url(url) {
                        blocks.push(json!({
                            "image": {
                                "format": fmt,
                                "source": {"bytes": b64}
                            }
                        }));
                    } else {
                        blocks.push(json!({"text": format!("[Image: {url}]")}));
                    }
                }
                _ => {}
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    blocks
}

fn parse_bedrock_image_data_url(url: &str) -> Option<(&'static str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(";base64,")?;
    if b64.is_empty() {
        return None;
    }
    let fmt = match meta.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    Some((fmt, b64))
}

fn parse_tool_args(args: Option<&Value>) -> Value {
    match args {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

/// Convert OpenAI-format messages to `(Option<system_blocks>, converse_messages)`.
/// Enforces strict user/assistant alternation, merging adjacent same-role turns and padding
/// leading/trailing assistant messages with `(empty)` user turns, matching
/// `agent/bedrock_adapter.py::convert_messages_to_converse`.
pub fn convert_messages_to_converse(messages: &[Value]) -> (Option<Vec<Value>>, Vec<Value>) {
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut converse_msgs: Vec<Value> = Vec::new();

    let append_turn = |msgs: &mut Vec<Value>, role: &str, blocks: Vec<Value>| {
        if let Some(last) = msgs.last_mut() {
            if last["role"].as_str() == Some(role) {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                    return;
                }
            }
        }
        msgs.push(json!({"role": role, "content": blocks}));
    };

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or_default();
        let content = msg.get("content");
        match role {
            "system" => match content {
                Some(Value::String(s)) if !s.trim().is_empty() => {
                    system_blocks.push(json!({"text": s}));
                }
                Some(Value::Array(parts)) => {
                    for part in parts {
                        let text = part.as_str().or_else(|| {
                            (part["type"].as_str() == Some("text"))
                                .then(|| part["text"].as_str())
                                .flatten()
                        });
                        if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
                            system_blocks.push(json!({"text": t}));
                        }
                    }
                }
                _ => {}
            },
            "tool" => {
                let result_text = match content {
                    Some(Value::String(s)) => safe_text(Some(&Value::String(s.clone()))),
                    Some(other) => safe_text(Some(&Value::String(other.to_string()))),
                    None => EMPTY_TEXT_PLACEHOLDER.to_owned(),
                };
                let tool_use_id = msg["tool_call_id"].as_str().unwrap_or_default();
                append_turn(
                    &mut converse_msgs,
                    "user",
                    vec![json!({
                        "toolResult": {
                            "toolUseId": tool_use_id,
                            "content": [{"text": result_text}],
                        }
                    })],
                );
            }
            "assistant" => {
                let mut blocks = Vec::new();
                match content {
                    Some(Value::String(s)) if !s.trim().is_empty() => {
                        blocks.push(json!({"text": s}));
                    }
                    Some(Value::Array(_)) => {
                        blocks.extend(convert_content_to_converse(content));
                    }
                    _ => {}
                }
                if let Some(tool_calls) = msg["tool_calls"].as_array() {
                    for tc in tool_calls {
                        let func = tc.get("function").unwrap_or(&Value::Null);
                        let id = tc["id"].as_str().unwrap_or_default();
                        let name = func["name"].as_str().unwrap_or_default();
                        let input = parse_tool_args(func.get("arguments"));
                        blocks.push(json!({
                            "toolUse": {
                                "toolUseId": id,
                                "name": name,
                                "input": input,
                            }
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
                }
                append_turn(&mut converse_msgs, "assistant", blocks);
            }
            "user" => {
                append_turn(
                    &mut converse_msgs,
                    "user",
                    convert_content_to_converse(content),
                );
            }
            _ => {}
        }
    }

    if let Some(first) = converse_msgs.first() {
        if first["role"].as_str() != Some("user") {
            converse_msgs.insert(
                0,
                json!({"role": "user", "content": [{"text": EMPTY_TEXT_PLACEHOLDER}]}),
            );
        }
    }
    if let Some(last) = converse_msgs.last() {
        if last["role"].as_str() != Some("user") {
            converse_msgs
                .push(json!({"role": "user", "content": [{"text": EMPTY_TEXT_PLACEHOLDER}]}));
        }
    }

    let system = (!system_blocks.is_empty()).then_some(system_blocks);
    (system, converse_msgs)
}

/// Normalize a Bedrock Converse JSON response into a runtime [`Completion`].
/// Matches `agent/bedrock_adapter.py::normalize_converse_response` (including adding
/// `cacheReadInputTokens` + `cacheWriteInputTokens` to `inputTokens`).
pub fn normalize_converse_response(response: &Value) -> Completion {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(blocks) = response
        .pointer("/output/message/content")
        .and_then(Value::as_array)
    {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                text_parts.push(text.to_owned());
            } else if let Some(tu) = block.get("toolUse").and_then(Value::as_object) {
                let id = tu
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = tu
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let input = tu.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }
    }

    let usage = response.get("usage").and_then(Value::as_object).map(|u| {
        let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let input_tokens =
            get("inputTokens") + get("cacheReadInputTokens") + get("cacheWriteInputTokens");
        let output_tokens = get("outputTokens");
        Usage {
            input_tokens,
            output_tokens,
        }
    });

    let stop_reason = response["stopReason"].as_str().unwrap_or("end_turn");
    let mut finish_reason = match stop_reason {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "content_filtered" | "guardrail_intervened" => "content_filter",
        _ => "stop",
    };
    if !tool_calls.is_empty() && finish_reason == "stop" {
        finish_reason = "tool_calls";
    }

    Completion {
        usage,
        content: text_parts.join("\n"),
        finish_reason: Some(finish_reason.to_owned()),
        tool_calls,
    }
}

/// Build the Bedrock Converse REST request body from an OpenAI chat-completions request.
pub fn translate_converse_request(
    request: &Value,
    guardrail_config: Option<&Value>,
) -> Result<(String, Map<String, Value>), ProviderError> {
    let model = request["model"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or(ProviderError::InvalidRequest)?
        .to_owned();
    let messages = request["messages"]
        .as_array()
        .ok_or(ProviderError::InvalidRequest)?;
    let (system, converse_messages) = convert_messages_to_converse(messages);

    let mut body = Map::new();
    body.insert("messages".into(), Value::Array(converse_messages));
    if let Some(sys) = system {
        body.insert("system".into(), Value::Array(sys));
    }
    let mut inference = Map::new();
    if let Some(max_tokens) = request
        .get("max_tokens")
        .or_else(|| request.get("max_completion_tokens"))
        .and_then(Value::as_u64)
    {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temp) = request.get("temperature").and_then(Value::as_f64) {
        inference.insert("temperature".into(), json!(temp));
    }
    if let Some(top_p) = request.get("top_p").and_then(Value::as_f64) {
        inference.insert("topP".into(), json!(top_p));
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Value::Object(inference));
    }
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            let converse_tools = convert_tools_to_converse(tools);
            body.insert("toolConfig".into(), json!({"tools": converse_tools}));
        }
    }
    if let Some(guardrail) = guardrail_config {
        body.insert("guardrailConfig".into(), guardrail.clone());
    }
    Ok((model, body))
}

// ── BedrockProvider (Converse REST API with SigV4 or Bearer) ────────────────────────

pub struct BedrockProvider {
    client: reqwest::Client,
    base_url: String,
    region: String,
    auth: AwsAuth,
    guardrail_config: Option<Value>,
    max_attempts: u32,
    extras: super::Extras,
}

impl BedrockProvider {
    pub fn new(
        base_url: &str,
        region: &str,
        auth: AwsAuth,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: build_client(timeout)?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            region: region.to_owned(),
            auth,
            guardrail_config: None,
            max_attempts: 3,
            extras: super::Extras::default(),
        })
    }

    pub fn with_guardrail(mut self, guardrail: Option<Value>) -> Self {
        self.guardrail_config = guardrail;
        self
    }

    pub fn with_extras(mut self, extras: super::Extras) -> Self {
        self.extras = extras;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub async fn complete<F>(
        &self,
        request: &Value,
        on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        self.complete_with(request, on_text, true).await
    }

    pub async fn complete_plain<F>(
        &self,
        request: &Value,
        on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        self.complete_with(request, on_text, false).await
    }

    async fn complete_with<F>(
        &self,
        request: &Value,
        mut on_text: F,
        with_extra_body: bool,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        let (model, mut body) =
            translate_converse_request(request, self.guardrail_config.as_ref())?;
        if with_extra_body {
            self.extras.merge_body(&mut body);
        }
        let encoded_model = sigv4_uri_encode(&model, true);
        let endpoint = format!("{}/model/{encoded_model}/converse", self.base_url);
        let url = reqwest::Url::parse(&endpoint).map_err(|_| ProviderError::InvalidRequest)?;
        let body_bytes =
            serde_json::to_vec(&Value::Object(body)).map_err(|_| ProviderError::InvalidRequest)?;

        let mut last_error = ProviderError::InvalidEvent;
        for attempt in 0..self.max_attempts {
            let mut builder = self
                .client
                .post(url.clone())
                .header("content-type", "application/json")
                .header("accept", "application/json");
            builder = self.extras.apply_headers(builder);
            match &self.auth {
                AwsAuth::Bearer(token) => {
                    builder = builder.bearer_auth(token);
                }
                AwsAuth::SigV4 {
                    access_key_id,
                    secret_access_key,
                    session_token,
                } => {
                    let secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let now = DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_default();
                    let signed = sign_sigv4(
                        "POST",
                        &url,
                        &body_bytes,
                        "bedrock",
                        &self.region,
                        access_key_id,
                        secret_access_key,
                        session_token.as_deref(),
                        now,
                    );
                    for (k, v) in signed {
                        builder = builder.header(k, v);
                    }
                }
            }
            match builder.body(body_bytes.clone()).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.map_err(ProviderError::Http)?;
                    let payload: Value =
                        serde_json::from_str(&text).map_err(|_| ProviderError::InvalidEvent)?;
                    let completion = normalize_converse_response(&payload);
                    if !completion.content.is_empty() {
                        on_text(&completion.content);
                    }
                    return Ok(completion);
                }
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let lower = text.to_ascii_lowercase();
                    if code == 400
                        && (lower.contains("too many tokens")
                            || lower.contains("input is too long")
                            || lower.contains("context length"))
                    {
                        return Err(ProviderError::ContextOverflow);
                    }
                    last_error = ProviderError::HttpStatus(code);
                    if !matches!(code, 429 | 500..=599) || attempt + 1 >= self.max_attempts {
                        return Err(last_error);
                    }
                }
                Err(err) => {
                    let retryable = err.is_connect() || err.is_timeout();
                    last_error = ProviderError::Http(err);
                    if !retryable || attempt + 1 >= self.max_attempts {
                        return Err(last_error);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200 * (1u64 << attempt.min(3)))).await;
        }
        Err(last_error)
    }
}
