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
}

impl AnthropicProvider {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ProviderError> {
        Ok(Self {
            client: build_client(timeout)?,
            endpoint: endpoint_url(base_url, "/messages")?,
            api_key: api_key.to_owned(),
            max_attempts: 3,
        })
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub async fn complete<F>(
        &self,
        request: &Value,
        mut on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        let request = translate_request(request)?;
        let mut response = open_stream(
            &self.client,
            &self.endpoint,
            &request,
            self.max_attempts,
            |builder| {
                builder
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", API_VERSION)
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
                            call.name = string(block, "name")?;
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
                    Some("message_delta") => {
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

/// Convert an OpenAI chat-format request into a streaming Messages request.
pub(super) fn translate_request(request: &Value) -> Result<Map<String, Value>, ProviderError> {
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
            Some("user") => converted.push(json!({
                "role": "user",
                "content": content.ok_or(ProviderError::InvalidRequest)?
            })),
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
