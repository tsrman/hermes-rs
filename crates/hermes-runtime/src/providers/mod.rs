pub mod anthropic;
pub mod openai;

use hermes_core::sse::SseError;

#[derive(Debug)]
pub enum ProviderError {
    InvalidRequest,
    Http(reqwest::Error),
    HttpStatus(u16),
    InvalidEvent,
    InvalidSse(SseError),
    IncompleteStream,
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

#[derive(Debug, Default)]
pub struct Completion {
    pub content: String,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;

/// A configured model API backend. Requests use OpenAI chat format for every variant.
pub enum Provider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
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
