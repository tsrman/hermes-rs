//! NVIDIA NIM provider support (`nvidia` / `nvidia-nim` / `nim` / `build-nvidia` / `nemotron`).
//!
//! Mirrors `plugins/model-providers/nvidia/__init__.py`: NVIDIA NIM enforces a stricter
//! `ToolMessage` schema than OpenAI and rejects `name` / `tool_name` on `role: "tool"` messages.

use serde_json::Value;

pub const NVIDIA_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";
pub const NVIDIA_KEY_ENV: &str = "NVIDIA_API_KEY";
pub const NVIDIA_BASE_URL_ENV: &str = "NVIDIA_BASE_URL";

pub const NVIDIA_ALIASES: &[&str] = &["nvidia", "nvidia-nim", "nim", "build-nvidia", "nemotron"];

pub fn is_nvidia_provider(name: &str) -> bool {
    let norm = name.trim().to_ascii_lowercase().replace('_', "-");
    NVIDIA_ALIASES.contains(&norm.as_str())
}

fn needs_strip(msg: &Value) -> bool {
    msg.as_object().is_some_and(|obj| {
        obj.get("role").and_then(Value::as_str) == Some("tool")
            && (obj.contains_key("name") || obj.contains_key("tool_name"))
    })
}

/// Copy-on-write message rewriting matching `NvidiaProviderProfile.prepare_messages`:
/// strips `"name"` and `"tool_name"` from `role == "tool"` messages while leaving all other
/// messages (including `user`/`assistant` `"name"` fields) untouched.
pub fn prepare_messages(messages: &[Value]) -> Vec<Value> {
    if !messages.iter().any(needs_strip) {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|msg| {
            if needs_strip(msg) {
                let mut cloned = msg.as_object().cloned().unwrap_or_default();
                cloned.remove("name");
                cloned.remove("tool_name");
                Value::Object(cloned)
            } else {
                msg.clone()
            }
        })
        .collect()
}

/// Rewrite the `"messages"` array of an OpenAI chat-completions request object in place.
pub fn rewrite_request_messages(request: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Array(messages)) = request.get("messages") {
        if messages.iter().any(needs_strip) {
            let rewritten = prepare_messages(messages);
            request.insert("messages".into(), Value::Array(rewritten));
        }
    }
}
