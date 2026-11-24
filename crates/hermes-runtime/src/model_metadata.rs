//! Model context-window metadata and display token estimation matching
//! upstream `agent/model_metadata.py`.

use crate::storage::Message;

/// Fallback context window when neither config nor the known-model table specifies one.
pub const DEFAULT_FALLBACK_CONTEXT_LENGTH: u64 = 128_000;

/// Known model family context windows ported from upstream `agent/model_metadata.py::DEFAULT_CONTEXT_LENGTHS`.
/// Lookups perform longest-key-first case-insensitive substring matching so specific model IDs
/// beat family catch-alls (e.g. `gpt-5.4-mini` 400K beats `gpt-5.4` 1.05M, `deepseek-flash` 1M beats `deepseek` 128K).
const DEFAULT_CONTEXT_LENGTHS: &[(&str, u64)] = &[
    // Anthropic
    ("claude-fable-5", 1_000_000),
    ("claude-fable", 1_000_000),
    ("claude-mythos", 1_000_000),
    ("claude-opus-5", 1_000_000),
    ("claude-sonnet-5", 1_000_000),
    ("claude-haiku-5", 1_000_000),
    ("claude-opus-4-8", 1_000_000),
    ("claude-opus-4.8", 1_000_000),
    ("claude-opus-4-7", 1_000_000),
    ("claude-opus-4.7", 1_000_000),
    ("claude-opus-4-6", 1_000_000),
    ("claude-sonnet-4-6", 1_000_000),
    ("claude-opus-4.6", 1_000_000),
    ("claude-sonnet-4.6", 1_000_000),
    ("claude", 200_000),
    // OpenAI
    ("gpt-6.2-astra", 1_050_000),
    ("gpt-6.2-sol", 1_050_000),
    ("gpt-6-astra", 1_050_000),
    ("gpt-6.1-sol", 1_050_000),
    ("gpt-6.1-terra", 1_050_000),
    ("gpt-6-sol", 1_050_000),
    ("gpt-6-luna", 1_050_000),
    ("gpt-5.6-luna", 1_050_000),
    ("gpt-5.6-terra", 1_050_000),
    ("gpt-5.6-sol", 1_050_000),
    ("gpt-5.6", 1_050_000),
    ("gpt-5.5", 1_050_000),
    ("gpt-5.4-nano", 400_000),
    ("gpt-5.4-mini", 400_000),
    ("gpt-5.4", 1_050_000),
    ("gpt-5.3-codex-spark", 128_000),
    ("gpt-5.1-chat", 128_000),
    ("gpt-5", 400_000),
    ("gpt-4.1", 1_047_576),
    ("gpt-4", 128_000),
    ("o3", 200_000),
    ("o4-mini", 200_000),
    // Google / Gemma
    ("gemini", 1_048_576),
    ("gemma-4-31b", 256_000),
    ("gemma-4", 256_000),
    ("gemma4", 256_000),
    ("gemma-3", 131_072),
    ("gemma", 8_192),
    // DeepSeek
    ("deepseek-v4-pro", 1_000_000),
    ("deepseek-v4.1-flash", 1_000_000),
    ("deepseek-v4-flash", 1_000_000),
    ("deepseek-chat", 1_000_000),
    ("deepseek-reasoner", 1_000_000),
    ("deepseek-flash", 1_000_000),
    ("deepseek-ai/deepseek-v3.2", 65_536),
    ("deepseek", 128_000),
    // Meta / Muse / Inkling
    ("llama", 131_072),
    ("muse-spark-1.3", 1_048_576),
    ("muse-spark", 1_048_576),
    ("inkling", 1_048_576),
    // Qwen
    ("qwen3.8-max", 1_000_000),
    ("qwen3.8-flash", 1_000_000),
    ("qwen3.7-plus", 1_048_576),
    ("qwen3.6-plus", 1_048_576),
    ("qwen3-coder-plus", 1_000_000),
    ("qwen3-coder", 262_144),
    ("qwen3-max", 262_144),
    ("qwen", 131_072),
    // MiniMax
    ("minimax-m3", 1_000_000),
    ("minimax", 204_800),
    // GLM
    ("glm-5.3-flash:batch", 1_048_576),
    ("glm-5.3-flash:us", 1_048_576),
    ("glm-5.3-flashx", 1_048_576),
    ("glm-5.3-flash", 1_048_576),
    ("glm-5.3:batch", 1_048_576),
    ("glm-5.3:us", 1_048_576),
    ("glm-5.3", 1_048_576),
    ("glm-5.2:free", 256_000),
    ("glm-5.2", 1_048_576),
    ("glm-5.1", 204_800),
    ("glm-5v-turbo", 202_752),
    ("glm-5-turbo", 202_752),
    ("glm-5", 204_800),
    ("glm-4.7-flash", 202_752),
    ("glm-4.7", 204_800),
    ("glm-4.6v", 131_072),
    ("glm-4.6", 204_800),
    ("glm", 202_752),
    // xAI Grok
    ("grok-composer", 200_000),
    ("grok-build-latest", 500_000),
    ("grok-build", 256_000),
    ("grok-code-fast", 256_000),
    ("grok-2-vision", 8_192),
    ("grok-4-fast", 2_000_000),
    ("grok-4.20", 1_000_000),
    ("grok-4.7", 500_000),
    ("grok-4.6", 500_000),
    ("grok-4.5", 500_000),
    ("grok-4.3", 1_000_000),
    ("grok-4", 256_000),
    ("grok-3", 131_072),
    ("grok-2", 131_072),
    ("grok", 131_072),
    // Kimi
    ("kimi-k3", 1_048_576),
    ("kimi", 262_144),
    // Upstage Solar
    ("solar-open2", 262_144),
    ("solar-pro3", 131_072),
    ("solar-pro2", 65_536),
    ("solar-mini", 32_768),
    ("solar-", 524_288),
    // Hunyuan / Nemotron / MiMo / others
    ("hy4-preview", 1_048_576),
    ("hy3-preview", 262_144),
    ("hy3", 262_144),
    ("nemotron-3.5-lightning", 1_000_000),
    ("nemotron-3.5-super-vl", 1_000_000),
    ("nemotron", 131_072),
    ("mimo-v2.6-pro-ultraspeed", 1_048_576),
    ("mimo-v2.6-pro", 1_048_576),
    ("mimo-v2.6-flash", 1_048_576),
    ("mimo-v2-pro", 1_048_576),
    ("mimo-v2.5-pro", 1_048_576),
    ("mimo-v2.5", 1_048_576),
    ("mimo-v2-omni", 262_144),
    ("mimo-v2-flash", 262_144),
    ("trinity", 262_144),
];

/// Infer a known model's context window using longest-key-first case-insensitive matching.
pub fn infer_model_context_length(model: &str) -> Option<u64> {
    let lower = model.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let mut best: Option<(usize, u64)> = None;
    for &(key, tokens) in DEFAULT_CONTEXT_LENGTHS {
        if lower.contains(key) {
            let len = key.len();
            if best.is_none_or(|(best_len, _)| len > best_len) {
                best = Some((len, tokens));
            }
        }
    }
    best.map(|(_, tokens)| tokens)
}

/// Resolve the display context window for the status bar.
/// Returns `(tokens, is_fallback)`:
/// 1. Explicit configured `model.context_length` / custom provider window (not fallback)
/// 2. Known model table lookup (not fallback)
/// 3. `DEFAULT_FALLBACK_CONTEXT_LENGTH` (128K, `is_fallback = true`)
pub fn resolve_display_context_length(configured: Option<u64>, model: Option<&str>) -> (u64, bool) {
    if let Some(window) = configured.filter(|&w| w > 1) {
        return (window, false);
    }
    if let Some(window) = model.and_then(infer_model_context_length) {
        return (window, false);
    }
    (DEFAULT_FALLBACK_CONTEXT_LENGTH, true)
}

/// Estimate current active context tokens (~chars/4) from a resumed session's messages so the
/// status bar can display an approximate `~` reading immediately upon `/resume` before the first
/// live provider response arrives.
pub fn estimate_active_messages_tokens(messages: &[Message]) -> u64 {
    let mut bytes: usize = 0;
    for msg in messages {
        if !msg.active {
            continue;
        }
        bytes += msg.content.to_string().len();
        if let Some(calls) = &msg.tool_calls {
            bytes += calls.to_string().len();
        }
        bytes += 16; // role + framing overhead
    }
    (bytes / 4) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_key_wins_in_context_lookup() {
        assert_eq!(infer_model_context_length("gpt-5.4"), Some(1_050_000));
        assert_eq!(infer_model_context_length("gpt-5.4-mini"), Some(400_000));
        assert_eq!(
            infer_model_context_length("deepseek-flash-1017"),
            Some(1_000_000)
        );
        assert_eq!(infer_model_context_length("deepseek-v3-old"), Some(128_000));
        assert_eq!(
            infer_model_context_length("anthropic/claude-sonnet-4-6"),
            Some(1_000_000)
        );
        assert_eq!(
            infer_model_context_length("anthropic/claude-3.5-sonnet"),
            Some(200_000)
        );
        assert_eq!(infer_model_context_length("unknown-local-model"), None);
    }

    #[test]
    fn resolve_display_context_length_prefers_config_then_known_then_fallback() {
        assert_eq!(
            resolve_display_context_length(Some(64_000), Some("gpt-5.4")),
            (64_000, false)
        );
        assert_eq!(
            resolve_display_context_length(None, Some("gpt-5.4")),
            (1_050_000, false)
        );
        assert_eq!(
            resolve_display_context_length(None, Some("my-custom-llm")),
            (128_000, true)
        );
    }
}
