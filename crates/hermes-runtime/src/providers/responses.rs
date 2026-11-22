//! Streaming OpenAI Responses API.
//!
//! Like the Anthropic adapter, this translates the OpenAI chat-format request used
//! by the turn loop into a Responses request and normalises the stream back into
//! a [`Completion`].

use std::collections::BTreeMap;
use std::time::Duration;

use hermes_core::sse::SseDecoder;
use serde_json::{json, Map, Value};

use super::openai::{build_client, endpoint_url, open_stream};
use super::{Completion, ProviderError, ToolCall};

pub struct ResponsesProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    max_attempts: u32,
    extras: super::Extras,
}

impl ResponsesProvider {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ProviderError> {
        Ok(Self {
            client: build_client(timeout)?,
            endpoint: endpoint_url(base_url, "/responses")?,
            api_key: api_key.to_owned(),
            max_attempts: 3,
            extras: super::Extras::default(),
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
        let mut response = open_stream(
            &self.client,
            &self.endpoint,
            &request,
            self.max_attempts,
            |builder| {
                let builder = self.extras.apply_headers(builder);
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
            .map(|function| {
                json!({
                    "type": "function",
                    "name": function["name"],
                    "description": function.get("description").cloned().unwrap_or(json!("")),
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
