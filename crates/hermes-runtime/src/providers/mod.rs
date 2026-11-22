pub mod anthropic;
pub mod openai;
pub mod profiles;
pub mod responses;

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
pub use openai::OpenAiProvider;
pub use responses::ResponsesProvider;

/// A configured model API backend. Requests use OpenAI chat format for every variant.
pub enum Provider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Responses(ResponsesProvider),
}

impl Provider {
    pub async fn complete<F>(
        &self,
        request: &serde_json::Value,
        on_text: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(&str),
    {
        match self {
            Self::OpenAi(provider) => provider.complete(request, on_text).await,
            Self::Anthropic(provider) => provider.complete(request, on_text).await,
            Self::Responses(provider) => provider.complete(request, on_text).await,
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
            other => other.complete(request, on_text).await,
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
