//! `text_to_speech`: convert text to audio via configured HTTP TTS providers (OpenAI,
//! ElevenLabs, DeepInfra, xAI, Mistral, MiniMax) or a `type: command` local synthesizer.
//!
//! Mirrors `tools/tts_tool.py`: per-provider character chunking without silent truncation,
//! `MEDIA:` injection guard on `output_path`, and `MEDIA:<path>` delivery tag.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub(crate) const TEXT_TO_SPEECH: &str = "text_to_speech";
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const CMD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TtsCommandProvider {
    pub command: Vec<String>,
    pub extension: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TtsSettings {
    pub provider: String,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub speed: Option<f64>,
    pub response_format: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub command_provider: Option<TtsCommandProvider>,
}

impl TtsSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        secrets: &HashMap<String, String>,
    ) -> Self {
        let section = config.get("tts");
        let get_str = |obj: Option<&serde_yaml::Value>, key: &str| {
            obj.and_then(|s| s.get(key))
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let get_env = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| secrets.get(name).filter(|s| !s.trim().is_empty()).cloned())
        };

        let explicit_provider = get_str(section, "provider").map(|p| p.to_ascii_lowercase());
        // Check custom `tts.providers.<name>` entry for `type: command` or custom endpoint.
        let custom_entry = explicit_provider.as_deref().and_then(|name| {
            section
                .and_then(|s| s.get("providers"))
                .and_then(|p| p.get(name))
        });
        let is_command = custom_entry
            .and_then(|e| e.get("type"))
            .and_then(serde_yaml::Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case("command"));

        let command_provider = if is_command {
            let cmd = custom_entry
                .and_then(|e| e.get("command"))
                .and_then(serde_yaml::Value::as_sequence)
                .map(|seq| {
                    seq.iter()
                        .filter_map(serde_yaml::Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !cmd.is_empty() {
                Some(TtsCommandProvider {
                    command: cmd,
                    extension: get_str(custom_entry, "extension"),
                })
            } else {
                None
            }
        } else {
            None
        };

        let provider = explicit_provider.unwrap_or_else(|| {
            if get_env("OPENAI_API_KEY").is_some() {
                "openai".to_owned()
            } else if get_env("ELEVENLABS_API_KEY").is_some() {
                "elevenlabs".to_owned()
            } else {
                String::new()
            }
        });

        let provider_section = section.and_then(|s| s.get(&provider));
        let api_key = get_str(custom_entry, "api_key")
            .or_else(|| get_str(provider_section, "api_key"))
            .or_else(|| match provider.as_str() {
                "openai" => get_env("OPENAI_API_KEY"),
                "elevenlabs" => get_env("ELEVENLABS_API_KEY"),
                "xai" => get_env("XAI_API_KEY"),
                "deepinfra" => get_env("DEEPINFRA_API_KEY"),
                "minimax" => get_env("MINIMAX_API_KEY"),
                "mistral" => get_env("MISTRAL_API_KEY"),
                "gemini" => get_env("GEMINI_API_KEY"),
                _ => get_str(custom_entry, "api_key_env").and_then(|env| get_env(&env)),
            });

        let base_url = get_str(custom_entry, "base_url")
            .or_else(|| get_str(provider_section, "base_url"))
            .or_else(|| get_str(section, "base_url"));
        let model = get_str(custom_entry, "model")
            .or_else(|| get_str(provider_section, "model"))
            .or_else(|| get_str(section, "model"));
        let voice = get_str(custom_entry, "voice")
            .or_else(|| get_str(provider_section, "voice"))
            .or_else(|| get_str(section, "voice"));
        let response_format = get_str(custom_entry, "response_format")
            .or_else(|| get_str(provider_section, "response_format"))
            .or_else(|| get_str(section, "response_format"));
        let speed = provider_section
            .and_then(|s| s.get("speed"))
            .or_else(|| section.and_then(|s| s.get("speed")))
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()));

        Self {
            provider,
            model,
            voice,
            speed,
            response_format,
            base_url,
            api_key,
            command_provider,
        }
    }

    pub fn is_available(&self) -> bool {
        if self.command_provider.is_some() {
            return true;
        }
        !self.provider.is_empty()
            && self
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty())
    }
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/text_to_speech.json"))
            .unwrap_or_default()
    })
}

pub fn max_chars_for_provider(provider: &str) -> usize {
    match provider {
        "xai" => 15_000,
        "minimax" => 10_000,
        "elevenlabs" => 5_000,
        _ => 4_096,
    }
}

/// Split long text into ordered chunks of at most `max_chars` characters, preferring sentence or
/// whitespace boundaries without silently truncating.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || max_chars == 0 {
        return Vec::new();
    }
    if trimmed.chars().count() <= max_chars {
        return vec![trimmed.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut remaining = trimmed;
    while !remaining.is_empty() {
        let total = remaining.chars().count();
        if total <= max_chars {
            chunks.push(remaining.trim().to_owned());
            break;
        }
        // Find byte offset of the `max_chars`-th character.
        let byte_limit = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
        let window = &remaining[..byte_limit];
        let split_at = window
            .rfind(['.', '!', '?', '\n'])
            .filter(|&i| i > byte_limit / 2)
            .map(|i| i + 1)
            .or_else(|| window.rfind(char::is_whitespace).filter(|&i| i > 0))
            .unwrap_or(byte_limit);
        let piece = remaining[..split_at].trim();
        if !piece.is_empty() {
            chunks.push(piece.to_owned());
        }
        remaining = remaining[split_at..].trim_start();
    }
    chunks
}

fn validate_elevenlabs_voice_id(voice_id: &str) -> bool {
    let len = voice_id.len();
    (8..=64).contains(&len) && voice_id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn resolve_output_path(
    raw: Option<&str>,
    ext: &str,
    root: &Path,
    profile: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(explicit) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        if explicit.contains("MEDIA:") {
            return Err("output_path must not contain a media directive (MEDIA:<path>)".into());
        }
        let candidate = {
            let p = Path::new(explicit);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        };
        let parent = candidate
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(root);
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create output directory: {e}"))?;
        let canon_parent = parent.canonicalize().map_err(|e| e.to_string())?;
        let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let in_root = canon_parent.starts_with(&canon_root);
        let in_profile = profile
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|p| canon_parent.starts_with(&p));
        if !in_root && !in_profile {
            return Err(format!(
                "output_path must reside inside the working directory or profile directory: {explicit}"
            ));
        }
        let filename = candidate
            .file_name()
            .ok_or("output_path must include a filename")?;
        return Ok(canon_parent.join(filename));
    }

    let dir = match profile {
        Some(p) => p.join("voice-memos"),
        None => root.join("voice-memos"),
    };
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create voice-memos directory: {e}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    Ok(dir.join(format!("tts_{stamp}.{ext}")))
}

fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "elevenlabs" => "https://api.elevenlabs.io",
        "xai" => "https://api.x.ai/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        "mistral" => "https://api.mistral.ai/v1",
        _ => "https://api.openai.com/v1",
    }
}

async fn synthesize_chunk_http(
    client: &reqwest::Client,
    settings: &TtsSettings,
    chunk: &str,
    voice: &str,
    speed: Option<f64>,
    instructions: Option<&str>,
    format: &str,
) -> Result<Vec<u8>, String> {
    let api_key = settings
        .api_key
        .as_deref()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "API key is not configured for TTS provider '{}'",
                settings.provider
            )
        })?;
    let base = settings
        .base_url
        .as_deref()
        .unwrap_or_else(|| default_base_url(&settings.provider))
        .trim_end_matches('/');

    if settings.provider == "elevenlabs" {
        if !validate_elevenlabs_voice_id(voice) {
            return Err(format!(
                "Invalid ElevenLabs voice_id '{voice}': must be 8-64 ASCII alphanumeric characters"
            ));
        }
        let model_id = settings
            .model
            .as_deref()
            .unwrap_or("eleven_multilingual_v2");
        let url = format!("{base}/v1/text-to-speech/{voice}");
        let mut payload = json!({
            "text": chunk,
            "model_id": model_id,
        });
        if let Some(spd) = speed {
            payload["voice_settings"] = json!({"speed": spd});
        }
        let resp = client
            .post(&url)
            .header("xi-api-key", api_key)
            .header("Accept", "audio/mpeg")
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("ElevenLabs TTS request failed: {e}"))?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "ElevenLabs TTS HTTP {code}: {}",
                err_body.chars().take(300).collect::<String>()
            ));
        }
        return resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("Failed to read ElevenLabs audio: {e}"));
    }

    // OpenAI-compatible `/audio/speech` (OpenAI, xAI, DeepInfra, Mistral, custom).
    let model = settings.model.as_deref().unwrap_or("gpt-4o-mini-tts");
    let url = format!("{base}/audio/speech");
    let mut payload = json!({
        "model": model,
        "input": chunk,
        "voice": voice,
        "response_format": format,
    });
    if let Some(spd) = speed {
        payload["speed"] = json!(spd);
    }
    if let Some(inst) = instructions.filter(|s| !s.trim().is_empty()) {
        payload["instructions"] = json!(inst);
    }
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("TTS request failed: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "TTS API HTTP {code}: {}",
            err_body.chars().take(300).collect::<String>()
        ));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("Failed to read TTS audio response: {e}"))
}

async fn synthesize_command(
    provider: &TtsCommandProvider,
    text: &str,
    dest: &Path,
) -> Result<usize, String> {
    let program = provider
        .command
        .first()
        .ok_or("TTS command provider has empty command")?;
    let dest_str = dest.display().to_string();
    let args: Vec<String> = provider
        .command
        .iter()
        .skip(1)
        .map(|arg| arg.replace("{output}", &dest_str).replace("{text}", text))
        .collect();
    let mut child = Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn TTS command '{program}': {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    let output = tokio::time::timeout(CMD_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "TTS command timed out after 60s".to_owned())?
        .map_err(|e| format!("TTS command failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "TTS command exited with {}: {}",
            output.status,
            stderr.chars().take(300).collect::<String>()
        ));
    }
    if !dest.is_file() && !output.stdout.is_empty() {
        fs::write(dest, &output.stdout).map_err(|e| format!("Failed to write TTS output: {e}"))?;
    }
    let meta =
        fs::metadata(dest).map_err(|e| format!("TTS command did not produce output file: {e}"))?;
    Ok(meta.len() as usize)
}

pub(crate) async fn execute(
    arguments: &str,
    settings: &TtsSettings,
    root: &Path,
    profile: Option<&Path>,
) -> Value {
    if !settings.is_available() {
        return json!({
            "success": false,
            "error": "text_to_speech is not configured. Set OPENAI_API_KEY / ELEVENLABS_API_KEY or configure tts.provider in config.yaml."
        });
    }
    let args: Value = match serde_json::from_str(arguments) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return json!({"success": false, "error": "text_to_speech expects a JSON object"}),
    };
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        return json!({"success": false, "error": "text is required for text_to_speech"});
    }
    let speed = match args.get("speed").and_then(Value::as_f64) {
        Some(s) if !(0.25..=4.0).contains(&s) => {
            return json!({"success": false, "error": "speed must be between 0.25 and 4.0"});
        }
        Some(s) => Some(s),
        None => settings.speed.map(|s| s.clamp(0.25, 4.0)),
    };
    let instructions = args.get("instructions").and_then(Value::as_str);
    let voice = args
        .get("voice")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(settings.voice.as_deref())
        .unwrap_or(if settings.provider == "elevenlabs" {
            "21m00Tcm4TlvDq8ikWAM"
        } else {
            "alloy"
        });
    let format = settings.response_format.as_deref().unwrap_or("mp3");
    let ext = settings
        .command_provider
        .as_ref()
        .and_then(|c| c.extension.as_deref())
        .unwrap_or(match format {
            "opus" => "ogg",
            other => other,
        });
    let dest = match resolve_output_path(
        args.get("output_path").and_then(Value::as_str),
        ext,
        root,
        profile,
    ) {
        Ok(p) => p,
        Err(err) => return json!({"success": false, "error": err}),
    };

    if let Some(cmd_provider) = &settings.command_provider {
        return match synthesize_command(cmd_provider, text, &dest).await {
            Ok(bytes) => {
                let path_str = dest.display().to_string();
                json!({
                    "success": true,
                    "provider": settings.provider,
                    "file_path": path_str,
                    "bytes": bytes,
                    "chunks": 1,
                    "media": format!("MEDIA:{path_str}")
                })
            }
            Err(err) => json!({"success": false, "error": err}),
        };
    }

    let chunks = chunk_text(text, max_chars_for_provider(&settings.provider));
    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return json!({"success": false, "error": format!("HTTP client error: {e}")}),
    };
    let mut combined = Vec::new();
    for chunk in &chunks {
        match synthesize_chunk_http(&client, settings, chunk, voice, speed, instructions, format)
            .await
        {
            Ok(bytes) => combined.extend_from_slice(&bytes),
            Err(err) => return json!({"success": false, "error": err}),
        }
    }
    if let Err(e) = fs::write(&dest, &combined) {
        return json!({"success": false, "error": format!("Failed to write audio file: {e}")});
    }
    let path_str = dest.display().to_string();
    json!({
        "success": true,
        "provider": settings.provider,
        "file_path": path_str,
        "bytes": combined.len(),
        "chunks": chunks.len(),
        "media": format!("MEDIA:{path_str}")
    })
}
