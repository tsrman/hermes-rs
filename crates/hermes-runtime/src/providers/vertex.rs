//! Google Vertex AI provider support (pure-Rust Service Account RS256 JWT + authorized_user ADC).
//!
//! Mirrors `agent/vertex_adapter.py`:
//! - Base URL: `global` -> `https://aiplatform.googleapis.com/v1beta1/projects/{project}/locations/global/endpoints/openapi`,
//!   other regions -> `https://{region}-aiplatform.googleapis.com/v1beta1/projects/{project}/locations/{region}/endpoints/openapi`.
//! - Setting precedence: `VERTEX_CREDENTIALS_PATH` > `GOOGLE_APPLICATION_CREDENTIALS` > gcloud ADC;
//!   `VERTEX_PROJECT_ID` > `vertex.project_id` > SA JSON `project_id`;
//!   `VERTEX_REGION` > `vertex.region` > `"global"`.
//! - Pure-Rust RS256 signing via `ring::signature::RsaKeyPair::from_pkcs8` (no Python `google-auth`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::{json, Value};

use super::oauth::{base64_decode_flexible, base64url_encode};

pub const DEFAULT_REGION: &str = "global";
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

#[derive(Clone, Debug, Default)]
pub struct VertexSettings {
    pub project_id: Option<String>,
    pub region: Option<String>,
    pub credentials_path: Option<String>,
}

impl VertexSettings {
    pub fn from_config(config: &serde_yaml::Value) -> Self {
        let section = config.get("vertex");
        let get_str = |key: &str| {
            section
                .and_then(|s| s.get(key))
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Self {
            project_id: get_str("project_id"),
            region: get_str("region"),
            credentials_path: get_str("credentials_path"),
        }
    }
}

/// OpenAI-compatible Vertex AI base URL matching `agent/vertex_adapter.py::build_vertex_base_url`.
pub fn build_vertex_base_url(project_id: &str, region: &str) -> String {
    let region = if region.trim().is_empty() {
        DEFAULT_REGION
    } else {
        region.trim()
    };
    let host = if region == "global" {
        "aiplatform.googleapis.com".to_owned()
    } else {
        format!("{region}-aiplatform.googleapis.com")
    };
    format!("https://{host}/v1beta1/projects/{project_id}/locations/{region}/endpoints/openapi")
}

/// Extract DER bytes from a PEM-encoded PKCS#8 `PRIVATE KEY` block.
pub fn decode_pem_private_key(pem: &str) -> Result<Vec<u8>, String> {
    let mut body = String::new();
    let mut inside = false;
    for line in pem.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN ") && trimmed.ends_with("PRIVATE KEY-----") {
            inside = true;
            continue;
        }
        if trimmed.starts_with("-----END ") && trimmed.ends_with("PRIVATE KEY-----") {
            break;
        }
        if inside {
            body.push_str(trimmed);
        }
    }
    if body.is_empty() {
        return Err("Service account JSON private_key is not a valid PEM private key".into());
    }
    base64_decode_flexible(&body)
        .ok_or_else(|| "Service account PEM private_key contains invalid base64".to_owned())
}

/// Build and RS256-sign a Google OAuth2 service-account JWT assertion.
pub fn build_service_account_jwt(
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    now_secs: u64,
) -> Result<String, String> {
    let der = decode_pem_private_key(private_key_pem)?;
    let key_pair = RsaKeyPair::from_pkcs8(&der)
        .or_else(|_| RsaKeyPair::from_der(&der))
        .map_err(|_| {
            "Service account private_key is not a valid PKCS#8 or PKCS#1 RSA key".to_owned()
        })?;
    let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = json!({
        "iss": client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": token_uri,
        "iat": now_secs,
        "exp": now_secs + 3600,
    });
    let payload = base64url_encode(claims.to_string().as_bytes());
    let signing_input = format!("{header}.{payload}");
    let rng = SystemRandom::new();
    let mut signature = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &RSA_PKCS1_SHA256,
            &rng,
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| "RS256 JWT signing failed".to_owned())?;
    let sig_b64 = base64url_encode(&signature);
    Ok(format!("{signing_input}.{sig_b64}"))
}

fn post_token_exchange_blocking(
    token_uri: &str,
    form: Vec<(String, String)>,
) -> Result<String, String> {
    let url = token_uri.to_owned();
    let run = move || -> Result<String, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot build Vertex OAuth runtime: {e}"))?;
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| format!("cannot build Vertex HTTP client: {e}"))?;
            let resp = client
                .post(&url)
                .form(&form)
                .send()
                .await
                .map_err(|e| format!("Vertex OAuth token exchange failed: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(format!(
                    "Vertex OAuth token exchange returned HTTP {}: {}",
                    status.as_u16(),
                    text.chars().take(300).collect::<String>()
                ));
            }
            let payload: Value = serde_json::from_str(&text)
                .map_err(|e| format!("Vertex OAuth returned invalid JSON: {e}"))?;
            payload["access_token"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "Vertex OAuth response missing access_token".to_owned())
        })
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(run)
            .join()
            .map_err(|_| "Vertex OAuth thread panicked".to_owned())?
    } else {
        run()
    }
}

/// Exchange either a `service_account` or `authorized_user` JSON credential document for an
/// OAuth2 access token, returning `(access_token, embedded_project_id)`.
pub fn mint_token_from_credentials_json(
    doc: &Value,
    token_uri_override: Option<&str>,
) -> Result<(String, Option<String>), String> {
    let cred_type = doc["type"].as_str().unwrap_or_default();
    let project_id = doc["project_id"]
        .as_str()
        .or_else(|| doc["quota_project_id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    match cred_type {
        "service_account" => {
            let email = doc["client_email"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("Vertex service account JSON missing client_email")?;
            let key = doc["private_key"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("Vertex service account JSON missing private_key")?;
            let token_uri = token_uri_override
                .or_else(|| doc["token_uri"].as_str().filter(|s| !s.trim().is_empty()))
                .unwrap_or(DEFAULT_TOKEN_URI);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let assertion = build_service_account_jwt(email, key, token_uri, now)?;
            let token = post_token_exchange_blocking(
                token_uri,
                vec![
                    (
                        "grant_type".into(),
                        "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
                    ),
                    ("assertion".into(), assertion),
                ],
            )?;
            Ok((token, project_id))
        }
        "authorized_user" => {
            let client_id = doc["client_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("Vertex authorized_user JSON missing client_id")?;
            let client_secret = doc["client_secret"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("Vertex authorized_user JSON missing client_secret")?;
            let refresh_token = doc["refresh_token"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("Vertex authorized_user JSON missing refresh_token")?;
            let token_uri = token_uri_override.unwrap_or(DEFAULT_TOKEN_URI);
            let token = post_token_exchange_blocking(
                token_uri,
                vec![
                    ("grant_type".into(), "refresh_token".into()),
                    ("client_id".into(), client_id.into()),
                    ("client_secret".into(), client_secret.into()),
                    ("refresh_token".into(), refresh_token.into()),
                ],
            )?;
            Ok((token, project_id))
        }
        other => Err(format!(
            "Unsupported Vertex AI credentials type '{other}' (expected 'service_account' or 'authorized_user')"
        )),
    }
}

fn resolve_credentials_path(
    settings: &VertexSettings,
    secrets: &HashMap<String, String>,
) -> Option<PathBuf> {
    let secret = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| secrets.get(name).cloned())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    for candidate in [
        secret("VERTEX_CREDENTIALS_PATH"),
        secret("GOOGLE_APPLICATION_CREDENTIALS"),
        settings.credentials_path.clone(),
    ]
    .into_iter()
    .flatten()
    {
        let p = PathBuf::from(&candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    // Standard gcloud ADC location.
    if let Ok(home) = std::env::var("HOME") {
        let adc = Path::new(&home).join(".config/gcloud/application_default_credentials.json");
        if adc.is_file() {
            return Some(adc);
        }
    }
    None
}

/// Resolve `(access_token, base_url)` for Vertex AI from config, env, and credentials JSON.
pub fn resolve_vertex_route(
    settings: &VertexSettings,
    secrets: &HashMap<String, String>,
) -> Result<(String, String), String> {
    let secret = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| secrets.get(name).cloned())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let region = secret("VERTEX_REGION")
        .or_else(|| settings.region.clone())
        .unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let project_override = secret("VERTEX_PROJECT_ID").or_else(|| settings.project_id.clone());
    let base_override = secret("VERTEX_BASE_URL");

    // Pre-minted bearer token escape hatch (e.g. CI / Workload Identity / testing).
    if let Some(pre_minted) = secret("VERTEX_ACCESS_TOKEN") {
        let base_url = match (base_override, project_override) {
            (Some(url), _) => url.trim_end_matches('/').to_owned(),
            (None, Some(proj)) => build_vertex_base_url(&proj, &region),
            (None, None) => {
                return Err(
                    "Vertex AI requires VERTEX_PROJECT_ID (or vertex.project_id in config.yaml) when using VERTEX_ACCESS_TOKEN."
                        .into(),
                );
            }
        };
        return Ok((pre_minted, base_url));
    }

    let path = resolve_credentials_path(settings, secrets).ok_or_else(|| {
        "No Google Cloud credentials found for Vertex AI. Set VERTEX_CREDENTIALS_PATH or GOOGLE_APPLICATION_CREDENTIALS to a service account JSON file, or run `gcloud auth application-default login`."
            .to_owned()
    })?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read Vertex credentials {}: {e}", path.display()))?;
    let doc: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Invalid Vertex credentials JSON {}: {e}", path.display()))?;
    let token_uri_override = secret("VERTEX_TOKEN_URI");
    let (token, embedded_project) =
        mint_token_from_credentials_json(&doc, token_uri_override.as_deref())?;
    let base_url = match (base_override, project_override.or(embedded_project)) {
        (Some(url), _) => url.trim_end_matches('/').to_owned(),
        (None, Some(proj)) => build_vertex_base_url(&proj, &region),
        (None, None) => {
            return Err(
                "Vertex AI project_id not found in credentials JSON, VERTEX_PROJECT_ID, or vertex.project_id."
                    .into(),
            );
        }
    };
    Ok((token, base_url))
}
