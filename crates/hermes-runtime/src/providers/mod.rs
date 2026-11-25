pub mod anthropic;
pub mod bedrock;
pub mod nvidia;
pub mod oauth;
pub mod oauth_login;
pub mod openai;
pub mod profiles;
pub mod responses;
pub mod vertex;

use hermes_core::sse::SseError;

#[derive(Debug)]
pub enum ProviderError {
    InvalidRequest,
    Http(reqwest::Error),
    HttpStatus(u16),
    InvalidEvent,
    InvalidSse(SseError),
    IncompleteStream,
    /// The request exceeds the model's context window.
    ContextOverflow,
}

impl ProviderError {
    pub(crate) fn allows_fallback(&self) -> bool {
        match self {
            Self::Http(error) => {
                error.is_connect() || error.is_timeout() || error.is_request() || error.is_body()
            }
            Self::HttpStatus(status) => matches!(status, 401..=404 | 429 | 500..=599),
            Self::IncompleteStream | Self::InvalidSse(SseError::IncompleteEvent) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// What a provider says a request cost, when it says.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Default)]
pub struct Completion {
    pub usage: Option<Usage>,
    pub content: String,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use openai::OpenAiProvider;
pub use responses::ResponsesProvider;

/// A configured model API backend. Requests use OpenAI chat format for every variant.
pub enum Provider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Responses(ResponsesProvider),
    Bedrock(BedrockProvider),
}

/// What the configured endpoint adds to every request: body fields and headers. Header values
/// may carry credentials, so they are never logged or journaled.
#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Extras {
    #[serde(default)]
    pub body: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

impl Extras {
    /// Merge the body fields into `request` (nested objects merge, other values replace).
    pub(crate) fn merge_body(&self, request: &mut serde_json::Map<String, serde_json::Value>) {
        merge(request, &self.body);
    }

    pub(crate) fn apply_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder
    }
}

/// Merge `extra` into `target`: nested objects merge, any other value replaces.
pub fn merge_body_maps(
    target: &mut serde_json::Map<String, serde_json::Value>,
    extra: &serde_json::Map<String, serde_json::Value>,
) {
    merge(target, extra);
}

pub(crate) fn merge(
    target: &mut serde_json::Map<String, serde_json::Value>,
    extra: &serde_json::Map<String, serde_json::Value>,
) {
    use serde_json::Value;
    for (key, value) in extra {
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(more)) => merge(existing, more),
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum StreamDelta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
}

impl Provider {
    pub async fn complete<F>(
        &self,
        request: &serde_json::Value,
        mut on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        self.complete_stream(request, |delta| {
            if let StreamDelta::Text(text) = delta {
                on_text(text);
            }
        })
        .await
    }

    pub async fn complete_stream<F>(
        &self,
        request: &serde_json::Value,
        mut on_delta: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>),
    {
        match self {
            Self::OpenAi(provider) => provider.complete_stream(request, on_delta).await,
            Self::Anthropic(provider) => provider.complete_stream(request, on_delta).await,
            Self::Responses(provider) => provider.complete_stream(request, on_delta).await,
            Self::Bedrock(provider) => {
                provider
                    .complete(request, |text| on_delta(StreamDelta::Text(text)))
                    .await
            }
        }
    }
}

impl Provider {
    /// Like `complete`, for side requests that must not carry the conversation's `extra_body`.
    pub async fn complete_plain<F>(
        &self,
        request: &serde_json::Value,
        on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        match self {
            Self::OpenAi(provider) => provider.complete_plain(request, on_text).await,
            Self::Anthropic(provider) => provider.complete_plain(request, on_text).await,
            Self::Responses(provider) => provider.complete_plain(request, on_text).await,
            Self::Bedrock(provider) => provider.complete_plain(request, on_text).await,
        }
    }
}

impl From<OpenAiProvider> for Provider {
    fn from(provider: OpenAiProvider) -> Self {
        Self::OpenAi(provider)
    }
}

impl From<AnthropicProvider> for Provider {
    fn from(provider: AnthropicProvider) -> Self {
        Self::Anthropic(provider)
    }
}

impl From<ResponsesProvider> for Provider {
    fn from(provider: ResponsesProvider) -> Self {
        Self::Responses(provider)
    }
}

impl From<BedrockProvider> for Provider {
    fn from(provider: BedrockProvider) -> Self {
        Self::Bedrock(provider)
    }
}
