//! Streaming Anthropic Messages API.
//!
//! The turn loop keeps its history in OpenAI chat format. This adapter translates
//! a chat-format request into a Messages request and normalises the stream back
//! into a [`Completion`] with OpenAI-style finish reasons.

use std::collections::BTreeMap;
use std::time::Duration;

use hermes_core::sse::SseDecoder;
use serde_json::{json, Map, Value};

use super::openai::{build_client, endpoint_url, open_stream};
use super::{Completion, ProviderError, ToolCall};

const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8192;

pub struct AnthropicProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    max_attempts: u32,
    extras: super::Extras,
    bearer_auth: bool,
    oauth_identity: bool,
}

impl AnthropicProvider {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ProviderError> {
        Ok(Self {
            client: build_client(timeout)?,
            endpoint: endpoint_url(base_url, "/messages")?,
            api_key: api_key.to_owned(),
            max_attempts: 3,
            extras: super::Extras::default(),
            bearer_auth: false,
            oauth_identity: false,
        })
    }

    /// Use `Authorization: Bearer <token>` instead of `x-api-key` (e.g. MiniMax OAuth).
    pub fn with_bearer_auth(mut self, enabled: bool) -> Self {
        self.bearer_auth = enabled;
        self
    }

    /// Enable Anthropic OAuth identity handling (Bearer auth, OAuth beta/CLI headers,
    /// Claude Code system prefix, and `mcp__` tool wire-name remapping).
    pub fn with_oauth_identity(mut self, enabled: bool) -> Self {
        self.oauth_identity = enabled;
        self
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

    async fn complete_with<F>(
        &self,
        request: &Value,
        mut on_text: F,
        with_extra_body: bool,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        let mut request = translate_request(request)?;
        if with_extra_body {
            self.extras.merge_body(&mut request);
        }
        let wire_map: std::collections::HashMap<String, String> = if self.oauth_identity {
            apply_anthropic_oauth_request_transforms(&mut request)
        } else {
            std::collections::HashMap::new()
        };
        let oauth_beta_header = if self.oauth_identity {
            let mut betas: Vec<&str> = super::oauth::ANTHROPIC_COMMON_BETAS.to_vec();
            betas.extend_from_slice(super::oauth::ANTHROPIC_OAUTH_ONLY_BETAS);
            Some(betas.join(","))
        } else {
            None
        };
        let oauth_user_agent = format!(
            "claude-code/{} (external, cli)",
            super::oauth::CLAUDE_CODE_VERSION_FALLBACK
        );
        let mut response = open_stream(
            &self.client,
            &self.endpoint,
            &request,
            self.max_attempts,
            |builder| {
                let mut builder = self
                    .extras
                    .apply_headers(builder)
                    .header("anthropic-version", API_VERSION);
                if self.bearer_auth || self.oauth_identity {
                    builder = builder.bearer_auth(&self.api_key);
                } else {
                    builder = builder.header("x-api-key", &self.api_key);
                }
                if let Some(beta) = oauth_beta_header.as_deref() {
                    builder = builder
                        .header("anthropic-beta", beta)
                        .header("user-agent", &oauth_user_agent)
                        .header("x-app", "cli");
                }
                builder
            },
        )
        .await?;

        let mut decoder = SseDecoder::new();
        let mut completion = Completion::default();
        let mut calls = BTreeMap::<u64, ToolCall>::new();
        let mut done = false;
        while let Some(bytes) = response.chunk().await.map_err(ProviderError::Http)? {
            for data in decoder.push(&bytes).map_err(ProviderError::InvalidSse)? {
                let event: Value =
                    serde_json::from_str(&data).map_err(|_| ProviderError::InvalidEvent)?;
                match event.get("type").and_then(Value::as_str) {
                    Some("content_block_start") => {
                        let block = event
                            .get("content_block")
                            .ok_or(ProviderError::InvalidEvent)?;
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let call = calls.entry(index(&event)?).or_default();
                            call.id = string(block, "id")?;
                            let raw_name = string(block, "name")?;
                            call.name = if self.oauth_identity {
                                super::oauth::anthropic_oauth_reverse_tool_name(
                                    &raw_name, &wire_map,
                                )
                            } else {
                                raw_name
                            };
                        }
                    }
                    Some("content_block_delta") => {
                        let delta = event.get("delta").ok_or(ProviderError::InvalidEvent)?;
                        match delta.get("type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                let text = delta
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .ok_or(ProviderError::InvalidEvent)?;
                                on_text(text);
                                completion.content.push_str(text);
                            }
                            Some("input_json_delta") => {
                                let part = delta
                                    .get("partial_json")
                                    .and_then(Value::as_str)
                                    .ok_or(ProviderError::InvalidEvent)?;
                                calls
                                    .get_mut(&index(&event)?)
                                    .ok_or(ProviderError::InvalidEvent)?
                                    .arguments
                                    .push_str(part);
                            }
                            // Thinking and signature deltas are not part of the replayed history.
                            _ => {}
                        }
                    }
                    Some("message_start") => {
                        // Cached prompt tokens are counted apart from `input_tokens`.
                        let input: u64 = [
                            "input_tokens",
                            "cache_creation_input_tokens",
                            "cache_read_input_tokens",
                        ]
                        .iter()
                        .filter_map(|key| {
                            event
                                .pointer(&format!("/message/usage/{key}"))
                                .and_then(Value::as_u64)
                        })
                        .sum();
                        let usage = completion.usage.get_or_insert_with(Default::default);
                        usage.input_tokens = input;
                    }
                    Some("message_delta") => {
                        if let Some(output) = event
                            .pointer("/usage/output_tokens")
                            .and_then(Value::as_u64)
                        {
                            let usage = completion.usage.get_or_insert_with(Default::default);
                            usage.output_tokens = output;
                        }
                        if let Some(reason) =
                            event.pointer("/delta/stop_reason").and_then(Value::as_str)
                        {
                            completion.finish_reason = Some(
                                match reason {
                                    "end_turn" | "stop_sequence" => "stop",
                                    "tool_use" => "tool_calls",
                                    "max_tokens" => "length",
                                    other => other,
                                }
                                .to_owned(),
                            );
                        }
                    }
                    Some("message_stop") => done = true,
                    Some("error") => {
                        // Overload and internal errors during a stream are transient.
                        return Err(match event.pointer("/error/type").and_then(Value::as_str) {
                            Some("overloaded_error" | "api_error") => {
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
        if completion.finish_reason.is_none() {
            return Err(ProviderError::IncompleteStream);
        }
        if calls
            .values()
            .any(|call| call.id.is_empty() || call.name.is_empty())
        {
            return Err(ProviderError::InvalidEvent);
        }
        completion.tool_calls = calls
            .into_values()
            .map(|mut call| {
                // A tool without parameters streams no JSON fragments at all.
                if call.arguments.is_empty() {
                    call.arguments.push_str("{}");
                }
                call
            })
            .collect();
        Ok(completion)
    }
}

fn index(event: &Value) -> Result<u64, ProviderError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .ok_or(ProviderError::InvalidEvent)
}

fn string(value: &Value, key: &str) -> Result<String, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ProviderError::InvalidEvent)
}

/// Apply Anthropic OAuth request transformations:
/// 1. Prepend the Claude Code system identity block.
/// 2. Remap tool names in `tools[]` and replayed `tool_use` blocks to OAuth wire names.
///
/// Returns the `original -> wire` map for reversing incoming response `tool_use` names.
fn apply_anthropic_oauth_request_transforms(
    request: &mut Map<String, Value>,
) -> std::collections::HashMap<String, String> {
    let prefix_block = json!({
        "type": "text",
        "text": super::oauth::CLAUDE_CODE_SYSTEM_PREFIX,
    });
    match request.get_mut("system") {
        Some(Value::Array(blocks)) => {
            let already_has = blocks
                .first()
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                == Some(super::oauth::CLAUDE_CODE_SYSTEM_PREFIX);
            if !already_has {
                blocks.insert(0, prefix_block);
            }
        }
        Some(Value::String(text)) => {
            let existing = text.clone();
            let mut blocks = vec![prefix_block];
            if !existing.is_empty() && existing != super::oauth::CLAUDE_CODE_SYSTEM_PREFIX {
                blocks.push(json!({"type": "text", "text": existing}));
            }
            request.insert("system".into(), Value::Array(blocks));
        }
        _ => {
            request.insert("system".into(), Value::Array(vec![prefix_block]));
        }
    }

    let mut all_names: Vec<String> = Vec::new();
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                all_names.push(name.to_owned());
            }
        }
    }
    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for msg in messages {
            if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(Value::as_str) {
                            all_names.push(name.to_owned());
                        }
                    }
                }
            }
        }
    }

    let name_refs: Vec<&str> = all_names.iter().map(String::as_str).collect();
    let wire_map = super::oauth::anthropic_oauth_wire_names(&name_refs);
    if !wire_map.is_empty() {
        if let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if let Some(name) = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|n| wire_map.get(n))
                    .cloned()
                {
                    if let Some(obj) = tool.as_object_mut() {
                        obj.insert("name".into(), Value::String(name));
                    }
                }
            }
        }
        if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
            for msg in messages {
                if let Some(blocks) = msg.get_mut("content").and_then(Value::as_array_mut) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            if let Some(name) = block
                                .get("name")
                                .and_then(Value::as_str)
                                .and_then(|n| wire_map.get(n))
                                .cloned()
                            {
                                if let Some(obj) = block.as_object_mut() {
                                    obj.insert("name".into(), Value::String(name));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    wire_map
}

/// Convert an OpenAI chat-format request into a streaming Messages request.
pub(super) fn translate_user_content(raw: &Value) -> Result<Value, ProviderError> {
    match raw {
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            blocks.push(json!({"type": "text", "text": t}));
                        }
                    }
                    Some("image_url") => {
                        let url = part
                            .pointer("/image_url/url")
                            .or_else(|| part.get("image_url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(rest) = url.strip_prefix("data:") {
                            if let Some((meta, b64)) = rest.split_once(";base64,") {
                                if !meta.is_empty() && !b64.is_empty() {
                                    blocks.push(json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": meta,
                                            "data": b64,
                                        }
                                    }));
                                }
                            }
                        } else if url.starts_with("http://") || url.starts_with("https://") {
                            blocks.push(json!({
                                "type": "image",
                                "source": {"type": "url", "url": url}
                            }));
                        }
                    }
                    Some("image") => blocks.push(part.clone()),
                    _ => {}
                }
            }
            if blocks.is_empty() {
                return Err(ProviderError::InvalidRequest);
            }
            Ok(Value::Array(blocks))
        }
        _ => Err(ProviderError::InvalidRequest),
    }
}

fn translate_request(request: &Value) -> Result<Map<String, Value>, ProviderError> {
    let request = request.as_object().ok_or(ProviderError::InvalidRequest)?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidRequest)?;
    let mut system = Vec::new();
    let mut converted: Vec<Value> = Vec::new();
    for message in messages {
        let content = message.get("content").and_then(Value::as_str);
        match message.get("role").and_then(Value::as_str) {
            Some("system") => system.push(content.ok_or(ProviderError::InvalidRequest)?),
            Some("user") => {
                let raw = message
                    .get("content")
                    .ok_or(ProviderError::InvalidRequest)?;
                let user_blocks = translate_user_content(raw)?;
                // Merge adjacent user messages (e.g. a native multimodal image block immediately
                // after a `tool_result` user turn) so Anthropic's role alternation stays valid.
                if let Some(last) = converted.last_mut().filter(|m| m["role"] == "user") {
                    match last.get_mut("content") {
                        Some(Value::Array(existing)) => match user_blocks {
                            Value::Array(extra) => existing.extend(extra),
                            Value::String(text) => {
                                existing.push(json!({"type": "text", "text": text}))
                            }
                            other => existing.push(other),
                        },
                        Some(Value::String(existing_text)) => {
                            let mut merged =
                                vec![json!({"type": "text", "text": existing_text.clone()})];
                            match user_blocks {
                                Value::Array(extra) => merged.extend(extra),
                                Value::String(text) => {
                                    merged.push(json!({"type": "text", "text": text}))
                                }
                                other => merged.push(other),
                            }
                            last["content"] = Value::Array(merged);
                        }
                        _ => {
                            converted.push(json!({"role": "user", "content": user_blocks}));
                        }
                    }
                } else {
                    converted.push(json!({"role": "user", "content": user_blocks}));
                }
            }
            Some("assistant") => {
                let mut blocks = Vec::new();
                if let Some(text) = content.filter(|text| !text.is_empty()) {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let arguments = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // Invalid or empty arguments cannot be sent as a JSON object input.
                    let input = serde_json::from_str::<Value>(arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.get("id").and_then(Value::as_str).ok_or(ProviderError::InvalidRequest)?,
                        "name": call.pointer("/function/name").and_then(Value::as_str).ok_or(ProviderError::InvalidRequest)?,
                        "input": input
                    }));
                }
                if blocks.is_empty() {
                    // The API rejects empty assistant content.
                    blocks.push(json!({"type": "text", "text": "(empty)"}));
                }
                converted.push(json!({"role": "assistant", "content": blocks}));
            }
            Some("tool") => {
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).ok_or(ProviderError::InvalidRequest)?,
                    "content": content.ok_or(ProviderError::InvalidRequest)?
                });
                // All results for one assistant turn must share a single user message.
                match converted.last_mut().and_then(|last| {
                    (last["role"] == "user")
                        .then(|| last["content"].as_array_mut())
                        .flatten()
                        .filter(|blocks| blocks.iter().all(|b| b["type"] == "tool_result"))
                }) {
                    Some(blocks) => blocks.push(result),
                    None => converted.push(json!({"role": "user", "content": [result]})),
                }
            }
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
    out.insert(
        "max_tokens".into(),
        request
            .get("max_tokens")
            .cloned()
            .unwrap_or_else(|| json!(DEFAULT_MAX_TOKENS)),
    );
    out.insert("stream".into(), Value::Bool(true));
    if !system.is_empty() {
        out.insert(
            "system".into(),
            json!([{
                "type": "text",
                "text": system.join("\n\n"),
                "cache_control": {"type": "ephemeral"}
            }]),
        );
    }
    out.insert("messages".into(), Value::Array(converted));
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        let tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .map(|function| {
                json!({
                    "name": function["name"],
                    "description": function.get("description").cloned().unwrap_or(json!("")),
                    "input_schema": function.get("parameters").cloned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_history_with_tool_results_grouped() {
        let request = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"x\"}"}},
                    {"id": "b", "type": "function", "function": {"name": "t", "arguments": ""}}
                ]},
                {"role": "tool", "tool_call_id": "a", "content": "one"},
                {"role": "tool", "tool_call_id": "b", "content": "two"}
            ],
            "tools": [{"type": "function", "function": {"name": "read_file", "description": "d", "parameters": {"type": "object"}}}]
        });
        let out = translate_request(&request).unwrap();
        assert_eq!(out["max_tokens"], 8192);
        assert_eq!(out["system"][0]["text"], "sys");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][0]["input"], json!({"path": "x"}));
        assert_eq!(messages[1]["content"][1]["input"], json!({}));
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out["tools"][0]["input_schema"], json!({"type": "object"}));
    }

    #[test]
    fn rejects_unknown_roles() {
        let request = json!({"model": "m", "messages": [{"role": "developer", "content": "x"}]});
        assert!(translate_request(&request).is_err());
    }
}
