//! `vision_analyze`: load an image (local file, `data:` URL, or `http(s)` URL) and either attach
//! it natively to the main model via the multimodal tool-result envelope or analyze it via a
//! configured `auxiliary.vision` route.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::truthy;
use crate::file_tools::detect_image_mime;
use crate::providers::oauth::{base64_decode_flexible, base64_encode};
use crate::turn::ModelRoute;
use crate::url_safety;
use crate::website_policy::WebsitePolicy;

pub(crate) const VISION_ANALYZE: &str = "vision_analyze";
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration for `vision_analyze`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VisionSettings {
    /// Attach loaded images natively to the active conversation turn when no dedicated
    /// `auxiliary.vision` route is configured (default `true`, matching `tools/vision_tools.py`).
    pub native: bool,
    /// Allow private/loopback URLs when fetching remote images (`security.allow_private_urls`).
    pub allow_private_urls: bool,
    /// Optional website blocklist (`security.website_blocklist`).
    #[serde(default)]
    pub website_policy: WebsitePolicy,
}

impl Default for VisionSettings {
    fn default() -> Self {
        Self {
            native: true,
            allow_private_urls: false,
            website_policy: WebsitePolicy::default(),
        }
    }
}

impl VisionSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        website_policy: WebsitePolicy,
        allow_private_urls: bool,
    ) -> Self {
        let native = config
            .get("vision")
            .and_then(|v| v.get("native"))
            .is_none_or(|v| truthy(v, true));
        Self {
            native,
            allow_private_urls,
            website_policy,
        }
    }
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/vision_analyze.json"))
            .unwrap_or_default()
    })
}

pub(crate) struct LoadedImage {
    pub data_url: String,
    pub mime_type: &'static str,
    pub size_bytes: usize,
}

fn mime_from_data_url_header(meta: &str) -> Option<&'static str> {
    match meta.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn load_local_image(
    raw_path: &str,
    root: &Path,
    profile: Option<&Path>,
) -> Result<LoadedImage, String> {
    let trimmed = raw_path.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or("Cannot expand '~': HOME is not set")?;
        PathBuf::from(home).join(rest)
    } else {
        let p = Path::new(trimmed);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("Image file not found ({trimmed}): {e}"))?;
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let allowed_in_root = canonical.starts_with(&canon_root);
    let allowed_in_cache = profile
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| {
            canonical.starts_with(p.join("cache").join("images"))
                || canonical.starts_with(p.join("cache").join("documents"))
                || canonical.starts_with(p.join("cache").join("spillover"))
        });
    if !allowed_in_root && !allowed_in_cache {
        return Err(format!(
            "Refusing to read image outside working directory or profile cache: {trimmed}"
        ));
    }
    let meta = fs::metadata(&canonical).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(format!("Image path is not a regular file: {trimmed}"));
    }
    if meta.len() as usize > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image exceeds maximum supported size ({MAX_IMAGE_BYTES} bytes)"
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    fs::File::open(&canonical)
        .map_err(|e| e.to_string())?
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image exceeds maximum supported size ({MAX_IMAGE_BYTES} bytes)"
        ));
    }
    let mime_type = detect_image_mime(&bytes)
        .ok_or_else(|| format!("Unsupported or unrecognized image format in {trimmed}"))?;
    let b64 = base64_encode(&bytes);
    Ok(LoadedImage {
        data_url: format!("data:{mime_type};base64,{b64}"),
        mime_type,
        size_bytes: bytes.len(),
    })
}

async fn load_remote_image(url: &str, settings: &VisionSettings) -> Result<LoadedImage, String> {
    if let Some(block) = settings.website_policy.check_access(url) {
        return Err(format!("Blocked by website policy: {}", block.message));
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Invalid image URL: {e}"))?;
    if !url_safety::is_safe_url(url, settings.allow_private_urls).await {
        return Err("Blocked image URL by SSRF safety policy".into());
    }

    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;
    let mut current_url = parsed;
    let response = {
        let mut hops = 0;
        loop {
            let resp = client
                .get(current_url.clone())
                .send()
                .await
                .map_err(|e| format!("Failed to fetch image URL: {e}"))?;
            if resp.status().is_redirection() {
                if hops >= 5 {
                    return Err("Too many redirects while fetching image".into());
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or("Redirect missing Location header")?;
                let next = current_url
                    .join(loc)
                    .map_err(|e| format!("Invalid redirect URL: {e}"))?;
                if let Some(block) = settings.website_policy.check_access(next.as_str()) {
                    return Err(format!(
                        "Redirect blocked by website policy: {}",
                        block.message
                    ));
                }
                if !url_safety::is_safe_url(next.as_str(), settings.allow_private_urls).await {
                    return Err("Redirect blocked by SSRF safety policy".into());
                }
                current_url = next;
                hops += 1;
                continue;
            }
            break resp;
        }
    };
    if !response.status().is_success() {
        return Err(format!(
            "HTTP {} while fetching image from {url}",
            response.status().as_u16()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read image response body: {e}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "Remote image exceeds maximum supported size ({MAX_IMAGE_BYTES} bytes)"
        ));
    }
    let mime_type = detect_image_mime(&bytes).ok_or_else(|| {
        "Remote response is not a supported image (PNG, JPEG, GIF, or WebP)".to_owned()
    })?;
    let b64 = base64_encode(&bytes);
    Ok(LoadedImage {
        data_url: format!("data:{mime_type};base64,{b64}"),
        mime_type,
        size_bytes: bytes.len(),
    })
}

fn load_data_url(url: &str) -> Result<LoadedImage, String> {
    let rest = url.strip_prefix("data:").ok_or("Not a data: URL")?;
    let (meta, b64) = rest
        .split_once(";base64,")
        .ok_or("data: image URL must be base64-encoded (data:image/...;base64,...)")?;
    let declared = mime_from_data_url_header(meta)
        .ok_or_else(|| format!("Unsupported image MIME type in data URL: {meta}"))?;
    let bytes = base64_decode_flexible(b64).ok_or("Invalid base64 in image data URL")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image exceeds maximum supported size ({MAX_IMAGE_BYTES} bytes)"
        ));
    }
    let mime_type = detect_image_mime(&bytes).unwrap_or(declared);
    Ok(LoadedImage {
        data_url: format!("data:{mime_type};base64,{}", base64_encode(&bytes)),
        mime_type,
        size_bytes: bytes.len(),
    })
}

pub(crate) async fn load_image(
    source: &str,
    root: &Path,
    profile: Option<&Path>,
    settings: &VisionSettings,
) -> Result<LoadedImage, String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("image_url is required".into());
    }
    if trimmed.starts_with("data:") {
        load_data_url(trimmed)
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        load_remote_image(trimmed, settings).await
    } else {
        load_local_image(trimmed, root, profile)
    }
}

pub(crate) async fn execute(
    arguments: &str,
    root: &Path,
    profile: Option<&Path>,
    settings: &VisionSettings,
    auxiliary_route: Option<&ModelRoute>,
) -> Value {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return json!({"success": false, "error": "vision_analyze expects a JSON object"}),
    };
    let image_url = args
        .get("image_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if image_url.is_empty() {
        return json!({"success": false, "error": "image_url is required"});
    }
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if question.is_empty() {
        return json!({"success": false, "error": "question is required"});
    }
    let region_note = match args.get("region") {
        None | Some(Value::Null) => None,
        Some(Value::Array(arr)) if arr.len() == 4 => {
            let coords: Option<Vec<i64>> = arr.iter().map(Value::as_i64).collect();
            match coords {
                Some(c) if c[2] > c[0] && c[3] > c[1] && c[0] >= 0 && c[1] >= 0 => Some(format!(
                    "Focus specifically on region [{}, {}, {}, {}] in pixel coordinates.",
                    c[0], c[1], c[2], c[3]
                )),
                _ => {
                    return json!({
                        "success": false,
                        "error": "region must be [x1, y1, x2, y2] with 0 <= x1 < x2 and 0 <= y1 < y2"
                    });
                }
            }
        }
        Some(_) => {
            return json!({
                "success": false,
                "error": "region must be a 4-integer array [x1, y1, x2, y2]"
            });
        }
    };

    let loaded = match load_image(image_url, root, profile, settings).await {
        Ok(img) => img,
        Err(err) => return json!({"success": false, "error": err, "analysis": err}),
    };

    let prompt = match &region_note {
        Some(note) => format!("{question}\n\n{note}"),
        None => question.to_owned(),
    };

    // Route through `auxiliary.vision` when configured; otherwise attach natively to the active
    // conversation turn so the main model sees the pixels directly.
    if let Some(route) = auxiliary_route {
        let request = json!({
            "model": route.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt},
                    {"type": "image_url", "image_url": {"url": loaded.data_url}}
                ]
            }]
        });
        return match route.provider.complete(&request, |_| {}).await {
            Ok(completion) => json!({
                "success": true,
                "analysis": completion.content,
                "model_used": route.model,
                "mime_type": loaded.mime_type,
                "image_size_bytes": loaded.size_bytes
            }),
            Err(err) => {
                let msg = format!("Auxiliary vision call failed: {err:?}");
                json!({"success": false, "error": msg, "analysis": msg})
            }
        };
    }

    if !settings.native {
        return json!({
            "success": false,
            "error": "Native vision is disabled and no auxiliary.vision route is configured."
        });
    }

    let preview_url: String = image_url.chars().take(200).collect();
    json!({
        "success": true,
        "native_vision": true,
        "image_url": preview_url,
        "mime_type": loaded.mime_type,
        "size_bytes": loaded.size_bytes,
        "note": format!("Image loaded ({}, {} bytes). Inspect the attached image to answer: {}", loaded.mime_type, loaded.size_bytes, prompt),
        "_hermes_multimodal_images": [{
            "data_url": loaded.data_url,
            "media_type": loaded.mime_type,
            "label": prompt
        }]
    })
}
