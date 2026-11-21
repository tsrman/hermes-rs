//! Streaming OpenAI-compatible chat completions.

use std::collections::BTreeMap;
use std::time::Duration;

use hermes_core::sse::{SseDecoder, SseError};
use serde_json::Value;

#[derive(Debug)]
pub enum ProviderError {
    InvalidRequest,
    Http(reqwest::Error),
    HttpStatus(u16),
    InvalidEvent,
    InvalidSse(SseError),
    IncompleteStream,
}

#[derive(Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Default)]
pub struct Completion {
    pub content: String,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

pub struct OpenAiProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ProviderError> {
        let mut endpoint =
            reqwest::Url::parse(base_url).map_err(|_| ProviderError::InvalidRequest)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(ProviderError::InvalidRequest);
        }
        endpoint.set_path(&format!(
            "{}/chat/completions",
            endpoint.path().trim_end_matches('/')
        ));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(ProviderError::Http)?;
        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
            api_key: api_key.to_owned(),
        })
    }

    pub async fn complete<F>(
        &self,
        request: &Value,
        mut on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        let mut request = request
            .as_object()
            .cloned()
            .ok_or(ProviderError::InvalidRequest)?;
        request.insert("stream".into(), Value::Bool(true));
        let mut attempt = 0;
        let mut response = loop {
            let response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&request)
                .send()
                .await
                .map_err(ProviderError::Http)?;
            let status = response.status().as_u16();
            if !matches!(status, 500 | 502 | 503 | 504) || attempt == 2 {
                break response;
            }
            let delay = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|seconds| seconds.min(600))
                .unwrap_or(2_u64 << attempt);
            attempt += 1;
            tokio::time::sleep(Duration::from_secs(delay)).await;
        };
        if !response.status().is_success() {
            return Err(ProviderError::HttpStatus(response.status().as_u16()));
        }

        let mut decoder = SseDecoder::new();
        let mut completion = Completion::default();
        let mut calls = BTreeMap::<u64, ToolCall>::new();
        let mut done = false;
        while let Some(bytes) = response.chunk().await.map_err(ProviderError::Http)? {
            for data in decoder.push(&bytes).map_err(ProviderError::InvalidSse)? {
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let event: Value =
                    serde_json::from_str(&data).map_err(|_| ProviderError::InvalidEvent)?;
                let choices = event
                    .get("choices")
                    .and_then(Value::as_array)
                    .ok_or(ProviderError::InvalidEvent)?;
                for choice in choices
                    .iter()
                    .filter(|choice| choice.get("index").and_then(Value::as_u64) == Some(0))
                {
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        completion.finish_reason = Some(reason.to_owned());
                    }
                    let Some(delta) = choice.get("delta") else {
                        continue;
                    };
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        on_text(text);
                        completion.content.push_str(text);
                    }
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        for tool in tool_calls {
                            let index = tool
                                .get("index")
                                .and_then(Value::as_u64)
                                .ok_or(ProviderError::InvalidEvent)?;
                            let call = calls.entry(index).or_default();
                            if let Some(id) = tool.get("id").and_then(Value::as_str) {
                                call.id.push_str(id);
                            }
                            if let Some(name) =
                                tool.pointer("/function/name").and_then(Value::as_str)
                            {
                                call.name.push_str(name);
                            }
                            if let Some(args) =
                                tool.pointer("/function/arguments").and_then(Value::as_str)
                            {
                                call.arguments.push_str(args);
                            }
                        }
                    }
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
        completion.tool_calls = calls.into_values().collect();
        Ok(completion)
    }
}
