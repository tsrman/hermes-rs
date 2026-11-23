//! Streaming OpenAI Responses API.
//!
//! Like the Anthropic adapter, this translates the OpenAI chat-format request used
//! by the turn loop into a Responses request and normalises the stream back into
//! a [`Completion`].

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use hermes_core::sse::SseDecoder;
use regex::Regex;
use ring::digest::{digest, SHA256};
use serde_json::{json, Map, Value};

use super::openai::{build_client, endpoint_url, open_stream};
use super::{Completion, ProviderError, ToolCall};

pub struct ResponsesProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    max_attempts: u32,
    extras: super::Extras,
    is_xai: bool,
    session_id: Option<String>,
    openai_native_web_search: bool,
}

impl ResponsesProvider {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ProviderError> {
        Ok(Self {
            client: build_client(timeout)?,
            endpoint: endpoint_url(base_url, "/responses")?,
            api_key: api_key.to_owned(),
            max_attempts: 3,
            extras: super::Extras::default(),
            is_xai: crate::reasoning::is_xai_responses("", base_url),
            session_id: None,
            openai_native_web_search: false,
        })
    }

    /// Body fields and headers the configured endpoint adds to every request.
    pub fn with_extras(mut self, extras: super::Extras) -> Self {
        self.extras = extras;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Mark whether this route targets xAI Responses (`provider: xai` or `api.x.ai`).
    pub fn with_xai(mut self, is_xai: bool) -> Self {
        self.is_xai = is_xai;
        self
    }

    /// Session identifier used to scope `prompt_cache_key` (and `x-grok-conv-id` on xAI).
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id.filter(|s| !s.is_empty());
        self
    }

    /// Swap a client-side `web_search` function 1:1 for the server-executed `{"type": "web_search"}`
    /// built-in tool when `web.search_backend: openai-native` is configured.
    pub fn with_openai_native_web_search(mut self, enabled: bool) -> Self {
        self.openai_native_web_search = enabled;
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

    /// A side request such as the compression summary: the configured body fields are not sent.
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

    /// Build the final wire body and optional `x-grok-conv-id` header for a Responses request.
    pub fn build_request_body(
        &self,
        request: &Value,
        with_extra_body: bool,
    ) -> Result<(Map<String, Value>, Option<String>), ProviderError> {
        let mut body = translate_request(request)?;
        if self.openai_native_web_search {
            if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
                let had_client_web_search = tools
                    .iter()
                    .any(|t| t.get("name").and_then(Value::as_str) == Some("web_search"));
                if had_client_web_search {
                    tools.retain(|t| t.get("name").and_then(Value::as_str) != Some("web_search"));
                    tools.push(json!({"type": "web_search"}));
                }
            }
        }
        let mut grok_conv_id = None;
        if with_extra_body {
            let scope = cache_scope_from_session_id(self.session_id.as_deref());
            let instructions = body
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or("");
            let response_tools = body
                .get("tools")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let derived = content_cache_key(instructions, response_tools, &scope)
                .or_else(|| (!scope.is_empty()).then(|| scope.clone()));
            if !self.is_xai {
                if let Some(key) = derived.as_ref() {
                    body.insert("prompt_cache_key".into(), Value::String(key.clone()));
                }
            }
            self.extras.merge_body(&mut body);
            if self.is_xai && self.session_id.is_some() {
                if !scope.is_empty() {
                    grok_conv_id = Some(scope);
                }
                if !body.contains_key("prompt_cache_key") {
                    if let Some(key) = derived {
                        body.insert("prompt_cache_key".into(), Value::String(key));
                    }
                }
            }
            bound_prompt_cache_key_field(&mut body);
        }
        Ok((body, grok_conv_id))
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
        let (request, grok_conv_id) = self.build_request_body(request, with_extra_body)?;
        let mut response = open_stream(
            &self.client,
            &self.endpoint,
            &request,
            self.max_attempts,
            |builder| {
                let mut builder = self.extras.apply_headers(builder);
                if let Some(conv_id) = grok_conv_id.as_deref() {
                    builder = builder.header("x-grok-conv-id", conv_id);
                }
                if self.api_key.is_empty() {
                    builder
                } else {
                    builder.bearer_auth(&self.api_key)
                }
            },
        )
        .await?;

        let mut decoder = SseDecoder::new();
        let mut completion = Completion::default();
        // Finished function calls keyed by output index, so order follows the model output.
        let mut calls = BTreeMap::<u64, ToolCall>::new();
        let mut done = false;
        while let Some(bytes) = response.chunk().await.map_err(ProviderError::Http)? {
            for data in decoder.push(&bytes).map_err(ProviderError::InvalidSse)? {
                let event: Value =
                    serde_json::from_str(&data).map_err(|_| ProviderError::InvalidEvent)?;
                match event.get("type").and_then(Value::as_str) {
                    Some("response.output_text.delta") => {
                        let text = event
                            .get("delta")
                            .and_then(Value::as_str)
                            .ok_or(ProviderError::InvalidEvent)?;
                        on_text(text);
                        completion.content.push_str(text);
                    }
                    Some("response.output_item.done") => {
                        let item = event.get("item").ok_or(ProviderError::InvalidEvent)?;
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            let index = event
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .ok_or(ProviderError::InvalidEvent)?;
                            let field = |name| {
                                item.get(name)
                                    .and_then(Value::as_str)
                                    .filter(|value| !value.is_empty())
                                    .map(str::to_owned)
                                    .ok_or(ProviderError::InvalidEvent)
                            };
                            calls.insert(
                                index,
                                ToolCall {
                                    id: field("call_id")?,
                                    name: field("name")?,
                                    arguments: item
                                        .get("arguments")
                                        .and_then(Value::as_str)
                                        .filter(|value| !value.is_empty())
                                        .unwrap_or("{}")
                                        .to_owned(),
                                },
                            );
                        }
                    }
                    Some("response.completed") => {
                        completion.finish_reason = Some(
                            if calls.is_empty() {
                                "stop"
                            } else {
                                "tool_calls"
                            }
                            .to_owned(),
                        );
                        done = true;
                    }
                    Some("response.incomplete") => {
                        let reason = event
                            .pointer("/response/incomplete_details/reason")
                            .and_then(Value::as_str);
                        completion.finish_reason = Some(
                            match reason {
                                Some("max_output_tokens") => "length",
                                _ => "incomplete",
                            }
                            .to_owned(),
                        );
                        done = true;
                    }
                    Some("response.failed" | "error") => {
                        let code = event
                            .pointer("/response/error/code")
                            .or_else(|| event.get("code"))
                            .and_then(Value::as_str);
                        // Server-side faults are transient; everything else is the caller's.
                        return Err(match code {
                            Some("server_error" | "overloaded_error") => {
                                ProviderError::IncompleteStream
                            }
                            _ => ProviderError::InvalidEvent,
                        });
                    }
                    _ => {}
                }
            }
            if done {
                break;
            }
        }
        if !done {
            decoder.finish().map_err(ProviderError::InvalidSse)?;
            return Err(ProviderError::IncompleteStream);
        }
        completion.tool_calls = calls.into_values().collect();
        Ok(completion)
    }
}

/// Convert an OpenAI chat-format request into a streaming Responses request.
pub(super) fn translate_request(request: &Value) -> Result<Map<String, Value>, ProviderError> {
    let request = request.as_object().ok_or(ProviderError::InvalidRequest)?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidRequest)?;
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        let content = message.get("content").and_then(Value::as_str);
        match message.get("role").and_then(Value::as_str) {
            Some("system") => instructions.push(content.ok_or(ProviderError::InvalidRequest)?),
            Some("user") => input.push(json!({
                "role": "user",
                "content": content.ok_or(ProviderError::InvalidRequest)?
            })),
            Some("assistant") => {
                if let Some(text) = content.filter(|text| !text.is_empty()) {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]
                    }));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = |pointer: &str| {
                        call.pointer(pointer)
                            .and_then(Value::as_str)
                            .ok_or(ProviderError::InvalidRequest)
                    };
                    input.push(json!({
                        "type": "function_call",
                        "call_id": text("/id")?,
                        "name": text("/function/name")?,
                        "arguments": text("/function/arguments")?
                    }));
                }
            }
            Some("tool") => input.push(json!({
                "type": "function_call_output",
                "call_id": message.get("tool_call_id").and_then(Value::as_str).ok_or(ProviderError::InvalidRequest)?,
                "output": content.ok_or(ProviderError::InvalidRequest)?
            })),
            _ => return Err(ProviderError::InvalidRequest),
        }
    }
    let mut out = Map::new();
    out.insert(
        "model".into(),
        request
            .get("model")
            .cloned()
            .ok_or(ProviderError::InvalidRequest)?,
    );
    out.insert("stream".into(), Value::Bool(true));
    // History lives in the profile database, not at the provider.
    out.insert("store".into(), Value::Bool(false));
    if !instructions.is_empty() {
        out.insert(
            "instructions".into(),
            Value::String(instructions.join("\n\n")),
        );
    }
    out.insert("input".into(), Value::Array(input));
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        let tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .filter(|function| {
                function
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.trim().is_empty())
            })
            .map(|function| {
                let strict = function
                    .get("strict")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                json!({
                    "type": "function",
                    "name": function["name"],
                    "description": function.get("description").cloned().unwrap_or(json!("")),
                    "strict": strict,
                    "parameters": function.get("parameters").cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}))
                })
            })
            .collect();
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }
    }
    Ok(out)
}

/// Normalize a physical session_id into a stable logical cache scope (`_cache_scope_from_session_id`):
/// strips the per-fire `_YYYYMMDD_HHMMSS` timestamp from `cron_<job_id>_<ts>`.
pub fn cache_scope_from_session_id(session_id: Option<&str>) -> String {
    static CRON_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(cron_.+)_\d{8}_\d{6}$").unwrap());
    let sid = session_id.unwrap_or("");
    if let Some(caps) = CRON_RE.captures(sid) {
        if let Some(prefix) = caps.get(1) {
            return prefix.as_str().to_owned();
        }
    }
    sid.to_owned()
}

/// Content-addressed prompt cache key (`_content_cache_key`):
/// `pck_<sha256[:24]>` over `"{scope_id}\0{instructions}\0{sorted_compact_tools_json}"`.
pub fn content_cache_key(instructions: &str, tools: &[Value], scope_id: &str) -> Option<String> {
    if instructions.is_empty() && tools.is_empty() {
        return None;
    }
    let tools_part = if tools.is_empty() {
        String::new()
    } else {
        let mut sorted: Vec<&Value> = tools.iter().filter(|t| t.is_object()).collect();
        sorted.sort_by(|a, b| tool_sort_key(a).cmp(tool_sort_key(b)));
        let canonical_list = Value::Array(sorted.into_iter().map(sort_json_value).collect());
        serde_json::to_string(&canonical_list).unwrap_or_default()
    };
    let content = format!("{scope_id}\0{instructions}\0{tools_part}");
    Some(format!("pck_{}", &sha256_hex(&content)[..24]))
}

/// Bound a prompt cache key to provider-safe length (`<= 64` chars) or drop blank values
/// (`_bounded_prompt_cache_key`).
pub fn bounded_prompt_cache_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= 64 {
        Some(trimmed.to_owned())
    } else {
        Some(format!("pck_{}", &sha256_hex(trimmed)[..24]))
    }
}

fn bound_prompt_cache_key_field(body: &mut Map<String, Value>) {
    let Some(raw) = body.get("prompt_cache_key") else {
        return;
    };
    let as_text = match raw {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    };
    match as_text.as_deref().and_then(bounded_prompt_cache_key) {
        Some(bounded) => {
            body.insert("prompt_cache_key".into(), Value::String(bounded));
        }
        None => {
            body.remove("prompt_cache_key");
        }
    }
}

fn sort_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_json_value(v)))
                .collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_json_value).collect()),
        other => other.clone(),
    }
}

fn tool_sort_key(v: &Value) -> &str {
    v.get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("type")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
}

fn sha256_hex(text: &str) -> String {
    let hash = digest(&SHA256, text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash.as_ref() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_history_to_response_items() {
        let request = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "calling", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "out"}
            ],
            "tools": [{"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}]
        });
        let out = translate_request(&request).unwrap();
        assert_eq!(out["instructions"], "sys");
        assert_eq!(out["store"], false);
        let input = out["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(
            input[3],
            json!({"type": "function_call_output", "call_id": "c1", "output": "out"})
        );
        assert_eq!(out["tools"][0]["name"], "read_file");
        assert!(out.get("messages").is_none());
    }
}
