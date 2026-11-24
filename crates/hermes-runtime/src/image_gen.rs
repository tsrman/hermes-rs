//! `image_generate`: image generation and editing via FAL.ai REST API.
//!
//! Mirrors `tools/image_generation_tool.py` and `tools/image_generation_catalog.py`:
//! translates unified `(prompt, aspect_ratio, image_url/reference_image_urls, seed)` into the
//! configured FAL model's native payload filtered to its `supports` / `edit_supports` whitelist,
//! downloads the generated image to `<profile>/cache/images` (or workspace), and returns a
//! `MEDIA:<path>` directive.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::file_tools::detect_image_mime;
use crate::url_safety;

pub(crate) const IMAGE_GENERATE: &str = "image_generate";
pub const DEFAULT_MODEL: &str = "fal-ai/flux-2/klein/9b";
pub const DEFAULT_ASPECT_RATIO: &str = "landscape";
pub const DEFAULT_FAL_BASE_URL: &str = "https://fal.run";
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ImageGenSettings {
    /// `FAL_KEY` from env / profile `.env`.
    pub fal_key: Option<String>,
    /// Configured FAL model ID (`image_gen.model`, default `fal-ai/flux-2/klein/9b`).
    pub model: String,
    /// Base URL for FAL REST calls (`image_gen.base_url`, default `https://fal.run`).
    pub base_url: String,
    /// Default aspect ratio (`landscape` | `square` | `portrait`).
    pub default_aspect_ratio: String,
    /// Allow private/loopback URLs (for tests / local gateways).
    pub allow_private_urls: bool,
}

impl ImageGenSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        secrets: &HashMap<String, String>,
        allow_private_urls: bool,
    ) -> Self {
        let section = config.get("image_gen");
        let get_str = |key: &str| {
            section
                .and_then(|s| s.get(key))
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let fal_key = std::env::var("FAL_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                secrets
                    .get("FAL_KEY")
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
            });
        let base_url = get_str("base_url")
            .or_else(|| {
                std::env::var("FAL_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_FAL_BASE_URL.to_owned());
        Self {
            fal_key,
            model: get_str("model").unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            base_url: base_url.trim_end_matches('/').to_owned(),
            default_aspect_ratio: get_str("aspect_ratio")
                .unwrap_or_else(|| DEFAULT_ASPECT_RATIO.to_owned()),
            allow_private_urls,
        }
    }

    pub fn is_available(&self) -> bool {
        self.fal_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
    }
}

#[derive(Clone, Copy)]
enum SizeStyle {
    ImageSizePreset,
    AspectRatio,
}

struct FalModelSpec {
    id: &'static str,
    style: SizeStyle,
    defaults: &'static [(&'static str, &'static str)],
    supports: &'static [&'static str],
    edit_endpoint: Option<&'static str>,
    edit_supports: &'static [&'static str],
    max_reference_images: usize,
}

const FAL_MODELS: &[FalModelSpec] = &[
    FalModelSpec {
        id: "fal-ai/flux-2/klein/9b",
        style: SizeStyle::ImageSizePreset,
        defaults: &[
            ("num_inference_steps", "4"),
            ("output_format", "\"png\""),
            ("enable_safety_checker", "false"),
        ],
        supports: &[
            "prompt",
            "image_size",
            "num_inference_steps",
            "seed",
            "output_format",
            "enable_safety_checker",
        ],
        edit_endpoint: Some("fal-ai/flux-2/klein/9b/edit"),
        edit_supports: &[
            "prompt",
            "image_urls",
            "num_inference_steps",
            "seed",
            "output_format",
            "enable_safety_checker",
        ],
        max_reference_images: 9,
    },
    FalModelSpec {
        id: "fal-ai/flux-2-pro",
        style: SizeStyle::ImageSizePreset,
        defaults: &[
            ("num_inference_steps", "50"),
            ("guidance_scale", "4.5"),
            ("num_images", "1"),
            ("output_format", "\"png\""),
            ("enable_safety_checker", "false"),
            ("safety_tolerance", "\"5\""),
            ("sync_mode", "true"),
        ],
        supports: &[
            "prompt",
            "image_size",
            "num_inference_steps",
            "guidance_scale",
            "num_images",
            "output_format",
            "enable_safety_checker",
            "safety_tolerance",
            "sync_mode",
            "seed",
        ],
        edit_endpoint: Some("fal-ai/flux-2-pro/edit"),
        edit_supports: &[
            "prompt",
            "image_urls",
            "num_inference_steps",
            "guidance_scale",
            "num_images",
            "output_format",
            "enable_safety_checker",
            "safety_tolerance",
            "sync_mode",
            "seed",
        ],
        max_reference_images: 9,
    },
    FalModelSpec {
        id: "fal-ai/z-image/turbo",
        style: SizeStyle::ImageSizePreset,
        defaults: &[
            ("num_inference_steps", "8"),
            ("num_images", "1"),
            ("output_format", "\"png\""),
            ("enable_safety_checker", "false"),
            ("enable_prompt_expansion", "false"),
        ],
        supports: &[
            "prompt",
            "image_size",
            "num_inference_steps",
            "num_images",
            "seed",
            "output_format",
            "enable_safety_checker",
            "enable_prompt_expansion",
        ],
        edit_endpoint: None,
        edit_supports: &[],
        max_reference_images: 0,
    },
    FalModelSpec {
        id: "fal-ai/nano-banana-pro",
        style: SizeStyle::AspectRatio,
        defaults: &[
            ("num_images", "1"),
            ("output_format", "\"png\""),
            ("safety_tolerance", "\"5\""),
            ("resolution", "\"1K\""),
        ],
        supports: &[
            "prompt",
            "aspect_ratio",
            "num_images",
            "output_format",
            "safety_tolerance",
            "resolution",
            "seed",
        ],
        edit_endpoint: Some("fal-ai/nano-banana-pro/edit"),
        edit_supports: &[
            "prompt",
            "image_urls",
            "aspect_ratio",
            "num_images",
            "output_format",
            "safety_tolerance",
            "resolution",
            "seed",
        ],
        max_reference_images: 8,
    },
];

fn lookup_model(model_id: &str) -> &'static FalModelSpec {
    let trimmed = model_id.trim();
    FAL_MODELS
        .iter()
        .find(|m| m.id == trimmed)
        .unwrap_or(&FAL_MODELS[0])
}

fn normalize_aspect_ratio(raw: Option<&str>, fallback: &str) -> &'static str {
    match raw.unwrap_or(fallback).trim().to_ascii_lowercase().as_str() {
        "square" | "1:1" => "square",
        "portrait" | "9:16" => "portrait",
        _ => "landscape",
    }
}

/// Build the FAL payload filtered strictly to the model's `supports` or `edit_supports` whitelist.
pub fn build_fal_payload(
    model_id: &str,
    prompt: &str,
    aspect_ratio: &str,
    edit_urls: &[String],
    seed: Option<i64>,
) -> Result<(String, Value), String> {
    let spec = lookup_model(model_id);
    let is_edit = !edit_urls.is_empty();
    let (endpoint, allowed): (&str, &[&str]) = if is_edit {
        let ep = spec.edit_endpoint.ok_or_else(|| {
            format!(
                "Model '{}' does not support image editing; choose an edit-capable model (e.g. fal-ai/flux-2/klein/9b).",
                spec.id
            )
        })?;
        if edit_urls.len() > spec.max_reference_images {
            return Err(format!(
                "Model '{}' accepts at most {} reference image(s), got {}",
                spec.id,
                spec.max_reference_images,
                edit_urls.len()
            ));
        }
        (ep, spec.edit_supports)
    } else {
        (spec.id, spec.supports)
    };

    let mut raw = Map::new();
    for (k, v_json) in spec.defaults {
        if let Ok(parsed) = serde_json::from_str::<Value>(v_json) {
            raw.insert((*k).to_owned(), parsed);
        }
    }
    raw.insert("prompt".into(), json!(prompt));
    if is_edit {
        raw.insert("image_urls".into(), json!(edit_urls));
    }
    match spec.style {
        SizeStyle::ImageSizePreset => {
            let preset = match aspect_ratio {
                "square" => "square_hd",
                "portrait" => "portrait_16_9",
                _ => "landscape_16_9",
            };
            raw.insert("image_size".into(), json!(preset));
        }
        SizeStyle::AspectRatio => {
            let ratio = match aspect_ratio {
                "square" => "1:1",
                "portrait" => "9:16",
                _ => "16:9",
            };
            raw.insert("aspect_ratio".into(), json!(ratio));
        }
    }
    if let Some(s) = seed {
        raw.insert("seed".into(), json!(s));
    }

    // Filter strictly to the model's whitelist so unsupported keys are never sent.
    let filtered: Map<String, Value> = raw
        .into_iter()
        .filter(|(k, _)| allowed.contains(&k.as_str()))
        .collect();
    Ok((endpoint.to_owned(), Value::Object(filtered)))
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/image_generate.json"))
            .unwrap_or_default()
    })
}

fn cache_dir(profile: Option<&Path>, root: &Path) -> PathBuf {
    match profile {
        Some(p) => p.join("cache").join("images"),
        None => root.join(".hermes_images"),
    }
}

fn ext_for_mime(mime: Option<&str>) -> &'static str {
    match mime {
        Some("image/jpeg") => "jpg",
        Some("image/webp") => "webp",
        Some("image/gif") => "gif",
        _ => "png",
    }
}

pub(crate) async fn execute(
    arguments: &str,
    settings: &ImageGenSettings,
    root: &Path,
    profile: Option<&Path>,
) -> Value {
    let Some(fal_key) = settings.fal_key.as_deref().filter(|k| !k.trim().is_empty()) else {
        return json!({
            "success": false,
            "error": "FAL_KEY is not set. Set FAL_KEY in your environment or profile .env to use image_generate."
        });
    };
    let args: Value = match serde_json::from_str(arguments) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return json!({"success": false, "error": "image_generate expects a JSON object"}),
    };
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if prompt.is_empty() {
        return json!({"success": false, "error": "prompt is required for image_generate"});
    }
    let aspect = normalize_aspect_ratio(
        args.get("aspect_ratio").and_then(Value::as_str),
        &settings.default_aspect_ratio,
    );
    let seed = args.get("seed").and_then(Value::as_i64);
    let mut edit_urls: Vec<String> = Vec::new();
    if let Some(u) = args
        .get("image_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        edit_urls.push(u.to_owned());
    }
    if let Some(arr) = args.get("reference_image_urls").and_then(Value::as_array) {
        for item in arr {
            if let Some(u) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                edit_urls.push(u.to_owned());
            }
        }
    }

    let (endpoint, payload) =
        match build_fal_payload(&settings.model, prompt, aspect, &edit_urls, seed) {
            Ok(pair) => pair,
            Err(err) => return json!({"success": false, "error": err}),
        };

    let url = format!("{}/{endpoint}", settings.base_url);
    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return json!({"success": false, "error": format!("HTTP client error: {e}")}),
    };
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Key {}", fal_key.trim()))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return json!({"success": false, "error": format!("FAL request failed: {e}")});
        }
    };
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return json!({
            "success": false,
            "error": format!("FAL API returned HTTP {}: {}", status.as_u16(), body_text.chars().take(400).collect::<String>())
        });
    }
    let body: Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => {
            return json!({"success": false, "error": format!("Invalid JSON from FAL API: {e}")});
        }
    };
    let image_url = body
        .pointer("/images/0/url")
        .or_else(|| body.pointer("/image/url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let Some(image_url) = image_url else {
        return json!({"success": false, "error": "FAL response contained no image URL"});
    };

    // Download generated image locally so CLI/platforms get a verified `MEDIA:<path>` file.
    let mut local_path: Option<String> = None;
    if url_safety::is_safe_url(&image_url, settings.allow_private_urls).await {
        if let Ok(parsed) = reqwest::Url::parse(&image_url) {
            if let Ok(img_resp) = client.get(parsed).send().await {
                if img_resp.status().is_success() {
                    if let Ok(bytes) = img_resp.bytes().await {
                        if !bytes.is_empty() && bytes.len() <= 25 * 1024 * 1024 {
                            let ext = ext_for_mime(detect_image_mime(&bytes));
                            let dir = cache_dir(profile, root);
                            if fs::create_dir_all(&dir).is_ok() {
                                let hash = digest(&SHA256, &bytes);
                                let hex: String = hash.as_ref()[..6]
                                    .iter()
                                    .map(|b| format!("{b:02x}"))
                                    .collect();
                                let dest = dir.join(format!("img_{hex}.{ext}"));
                                if fs::write(&dest, &bytes).is_ok() {
                                    local_path = Some(dest.display().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut out = Map::new();
    out.insert("success".into(), json!(true));
    out.insert("provider".into(), json!("fal"));
    out.insert("model".into(), json!(endpoint));
    out.insert("aspect_ratio".into(), json!(aspect));
    out.insert("image_url".into(), json!(image_url));
    if let Some(path) = local_path {
        out.insert("media".into(), json!(format!("MEDIA:{path}")));
        out.insert("file_path".into(), json!(path));
    }
    Value::Object(out)
}
