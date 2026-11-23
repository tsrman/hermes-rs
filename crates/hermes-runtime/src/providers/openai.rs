//! Streaming OpenAI-compatible chat completions.

use std::collections::BTreeMap;
use std::time::Duration;

use hermes_core::sse::SseDecoder;
use serde_json::{Map, Value};

pub use super::{Completion, ProviderError, ToolCall};

pub struct OpenAiProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    max_attempts: u32,
    extras: super::Extras,
    strip_tool_message_names: bool,
}

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, ProviderError> {
        Ok(Self {
            client: build_client(timeout)?,
            endpoint: endpoint_url(base_url, "/chat/completions")?,
            api_key: api_key.to_owned(),
            max_attempts: 3,
            extras: super::Extras::default(),
            strip_tool_message_names: false,
        })
    }

    /// Strip `name` and `tool_name` from `role: "tool"` messages (NVIDIA NIM strict schema).
    pub fn with_strip_tool_message_names(mut self, enabled: bool) -> Self {
        self.strip_tool_message_names = enabled;
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

    async fn open_stream(
        &self,
        request: &Map<String, Value>,
    ) -> Result<reqwest::Response, ProviderError> {
        open_stream(
            &self.client,
            &self.endpoint,
            request,
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
        .await
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

    /// A side request such as the compression summary: the configured `extra_body` belongs to
    /// the main conversation and is not sent.
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
        let mut request = request
            .as_object()
            .cloned()
            .ok_or(ProviderError::InvalidRequest)?;
        if self.strip_tool_message_names {
            super::nvidia::rewrite_request_messages(&mut request);
        }
        if with_extra_body {
            self.extras.merge_body(&mut request);
        }
        request.insert("stream".into(), Value::Bool(true));
        request
            .entry("stream_options")
            .or_insert_with(|| serde_json::json!({"include_usage": true}));
        let mut response = self.open_stream(&request).await?;

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
                if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
                    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                    completion.usage = Some(super::Usage {
                        input_tokens: count("prompt_tokens"),
                        output_tokens: count("completion_tokens"),
                    });
                }
                // A usage-only chunk may carry no choices at all.
                let none = Vec::new();
                let choices = match event.get("choices") {
                    Some(choices) => choices.as_array().ok_or(ProviderError::InvalidEvent)?,
                    None if event.get("usage").is_some() => &none,
                    None => return Err(ProviderError::InvalidEvent),
                };
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

/// POST a streaming request with retry on transient failures.
pub(super) async fn open_stream(
    client: &reqwest::Client,
    endpoint: &str,
    request: &Map<String, Value>,
    max_attempts: u32,
    authorize: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    let body = serde_json::to_vec(request).map_err(|_| ProviderError::InvalidRequest)?;
    open_stream_bytes(client, endpoint, &body, max_attempts, |builder, _| {
        authorize(builder)
    })
    .await
}

/// POST a pre-serialized JSON streaming request with retry on transient failures,
/// giving `authorize` access to the exact wire payload bytes (used for AWS SigV4).
pub(super) async fn open_stream_bytes(
    client: &reqwest::Client,
    endpoint: &str,
    body: &[u8],
    max_attempts: u32,
    authorize: impl Fn(reqwest::RequestBuilder, &[u8]) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 1;
    loop {
        let last_attempt = attempt == max_attempts;
        let backoff = (2_u64 << (attempt - 1).min(5)).min(60);
        let builder = authorize(
            client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_vec()),
            body,
        );
        let delay = match builder.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let delay = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map_or(backoff, |seconds| seconds.min(600));
                if response.status().is_success() {
                    return Ok(response);
                }
                let retryable = match status {
                    429 if !last_attempt => is_transient_rate_limit(response).await?,
                    400 | 413 => {
                        return Err(if is_context_overflow(response).await {
                            ProviderError::ContextOverflow
                        } else {
                            ProviderError::HttpStatus(status)
                        });
                    }
                    500 | 502 | 503 | 504 | 529 => !last_attempt,
                    _ => false,
                };
                if !retryable {
                    return Err(ProviderError::HttpStatus(status));
                }
                delay
            }
            Err(error)
                if !last_attempt
                    && (error.is_connect() || error.is_timeout() || error.is_request()) =>
            {
                backoff
            }
            Err(error) => return Err(ProviderError::Http(error.without_url())),
        };
        attempt += 1;
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

pub(super) fn build_client(timeout: Duration) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .use_rustls_tls()
        // Keep OS trust as the only source even if another dependency enables bundled roots.
        .tls_built_in_root_certs(false)
        .tls_built_in_native_certs(true)
        .timeout(timeout)
        .build()
        .map_err(ProviderError::Http)
}

pub(super) fn endpoint_url(base_url: &str, suffix: &str) -> Result<String, ProviderError> {
    let mut endpoint = reqwest::Url::parse(base_url).map_err(|_| ProviderError::InvalidRequest)?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderError::InvalidRequest);
    }
    endpoint.set_path(&format!(
        "{}{suffix}",
        endpoint.path().trim_end_matches('/')
    ));
    Ok(endpoint.to_string())
}

/// Whether a 400 or 413 error body reports a prompt larger than the context window.
async fn is_context_overflow(mut response: reqwest::Response) -> bool {
    const MAX_ERROR_BYTES: usize = 65_536;
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
            return false;
        }
        body.extend_from_slice(&chunk);
    }
    context_overflow_body(&body)
}

pub(super) fn context_overflow_body(body: &[u8]) -> bool {
    let Ok(error) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let code = error
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .pointer("/error/message")
        .or_else(|| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    code == "context_length_exceeded"
        || [
            "maximum context length",
            "context length",
            "context window",
            "prompt is too long",
            "too many tokens",
            "request_too_large",
        ]
        .iter()
        .any(|signal| message.contains(signal))
        || error.pointer("/error/type").and_then(Value::as_str) == Some("request_too_large")
}

pub(super) async fn is_transient_rate_limit(
    mut response: reqwest::Response,
) -> Result<bool, ProviderError> {
    const MAX_ERROR_BYTES: usize = 65_536;
    let has_reset_header = ["retry-after", "x-ratelimit-reset"].iter().any(|name| {
        response
            .headers()
            .get(*name)
            .is_some_and(|value| !value.is_empty())
    });
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ProviderError::Http)? {
        if body.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
            return Ok(false);
        }
        body.extend_from_slice(&chunk);
    }
    let Ok(error) = serde_json::from_slice::<Value>(&body) else {
        return Ok(false);
    };
    let code = error
        .pointer("/error/code")
        .or_else(|| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let billing_codes = [
        "insufficient_quota",
        "billing_not_active",
        "payment_required",
        "insufficient_credits",
        "no_usable_credits",
        "balance_depleted",
        "model_not_supported_on_free_tier",
        "member_spend_cap_exceeded",
        "terminal_quota_exhausted",
        "personal-team-blocked:spending-limit",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "organization_usage_limit_exceeded",
        "project_spend_limit_exceeded",
        "insufficient_credits_for_paid_model",
    ];
    if billing_codes.contains(&code.as_str()) {
        return Ok(false);
    }
    let message = error
        .pointer("/error/message")
        .or_else(|| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let rate_signals = [
        "rate limit",
        "rate_limit",
        "too many requests",
        "throttled",
        "requests per minute",
        "tokens per minute",
        "throttling",
    ];
    let billing_signals = [
        "insufficient credits",
        "insufficient_quota",
        "insufficient balance",
        "payment required",
        "billing hard limit",
        "exceeded your current quota",
        "daily limit",
        "daily quota",
        "tokens per day",
        "weekly usage limit",
        "weekly limit",
        "reached your session usage limit",
    ];
    let reset_words = [
        "try again",
        "retry",
        "resets at",
        "reset in",
        "resets in",
        "reset after",
        "available in",
        "wait",
        "requests remaining",
        "periodic",
        "window",
        "per minute",
        "per second",
    ];
    let reset_fields = ["resets_in_seconds", "resets_at", "reset_at", "retry_after"];
    let has_reset_field = [Some(&error), error.get("error")]
        .into_iter()
        .flatten()
        .any(|item| {
            reset_fields.iter().any(|name| {
                item.get(*name)
                    .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
            })
        });
    let has_reset = has_reset_header
        || has_reset_field
        || reset_words.iter().any(|word| message.contains(word));
    if !has_reset
        && billing_signals
            .iter()
            .any(|signal| message.contains(signal))
    {
        return Ok(false);
    }
    Ok(code == "rate_limit_exceeded"
        || has_reset
        || rate_signals.iter().any(|signal| message.contains(signal)))
}

#[cfg(test)]
mod tests {
    use super::context_overflow_body;

    #[test]
    fn recognises_context_overflow_errors_from_each_api() {
        let openai = br#"{"error":{"code":"context_length_exceeded","message":"x"}}"#;
        let anthropic = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;
        let too_large = br#"{"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#;
        let other = br#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#;
        assert!(context_overflow_body(openai));
        assert!(context_overflow_body(anthropic));
        assert!(context_overflow_body(too_large));
        assert!(!context_overflow_body(other));
        assert!(!context_overflow_body(b"not json"));
    }
}
