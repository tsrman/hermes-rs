//! Interactive OAuth login flows (browser PKCE + RFC 8628 device code), status inspection, and
//! credential logout for `<home>/auth.json` (and Anthropic setup tokens in `<home>/.env`).
//!
//! Mirrors `hermes_cli/auth*.py` without adding new dependencies: uses `ring` for CSPRNG and
//! SHA-256 PKCE challenges, `tokio::net::TcpListener` for the loopback callback server, and
//! `reqwest` for token exchange and device polling.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::providers::oauth::{
    auth_store_path, base64url_encode, decode_jwt_claims, default_oauth_base_url,
    is_anthropic_oauth_token, jwt_is_expiring, load_auth_store, minimax_resolve_token_expiry_unix,
    now_unix_secs, resolve_oauth_credentials, resolve_oauth_provider, save_auth_store,
    validate_nous_inference_url, CODEX_OAUTH_CLIENT_ID, CODEX_OAUTH_TOKEN_URL,
    CODEX_REFRESH_SKEW_SECS, DEFAULT_CODEX_BASE_URL, DEFAULT_NOUS_CLIENT_ID,
    DEFAULT_NOUS_INFERENCE_URL, DEFAULT_NOUS_PORTAL_URL, DEFAULT_QWEN_BASE_URL,
    DEFAULT_XAI_OAUTH_BASE_URL, MINIMAX_OAUTH_CLIENT_ID, MINIMAX_OAUTH_CN_BASE,
    MINIMAX_OAUTH_CN_INFERENCE, MINIMAX_OAUTH_GLOBAL_BASE, MINIMAX_OAUTH_GLOBAL_INFERENCE,
    MINIMAX_REFRESH_SKEW_SECS, NOUS_REFRESH_SKEW_SECS, QWEN_OAUTH_CLIENT_ID, QWEN_OAUTH_TOKEN_URL,
    QWEN_REFRESH_SKEW_SECS, XAI_OAUTH_CLIENT_ID, XAI_REFRESH_SKEW_SECS,
};

pub const CODEX_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const CODEX_OAUTH_DEVICE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const CODEX_OAUTH_CALLBACK_PORT: u16 = 1455;
pub const QWEN_OAUTH_DEVICE_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/device/code";
pub const XAI_OIDC_DISCOVERY_URL: &str = "https://accounts.x.ai/.well-known/openid-configuration";

pub const SUPPORTED_LOGIN_PROVIDERS: &[&str] = &[
    "openai-codex",
    "nous",
    "xai-oauth",
    "qwen-oauth",
    "minimax-oauth",
    "anthropic",
];

#[derive(Clone, Debug)]
pub struct LoginOptions {
    pub provider: String,
    pub device_code: bool,
    pub no_browser: bool,
    pub port: Option<u16>,
    pub region: Option<String>,
    pub portal_url: Option<String>,
    pub timeout_secs: u64,
    pub token: Option<String>,
}

impl LoginOptions {
    pub fn for_provider(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            device_code: false,
            no_browser: false,
            port: None,
            region: None,
            portal_url: None,
            timeout_secs: 300,
            token: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum LoginPrompt {
    OpenUrl {
        url: String,
    },
    DeviceCode {
        verification_uri: String,
        verification_uri_complete: Option<String>,
        user_code: String,
    },
    Info(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginResult {
    pub provider: String,
    pub auth_path: PathBuf,
    pub base_url: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderAuthStatus {
    pub provider: String,
    pub authenticated: bool,
    pub source: String,
    pub has_refresh_token: bool,
    pub expires_at: Option<String>,
    pub expired: bool,
    pub base_url: Option<String>,
}

pub fn canonical_login_provider(raw: &str) -> Option<&'static str> {
    let lowered = raw.trim().to_ascii_lowercase().replace('_', "-");
    if lowered == "anthropic" || lowered == "claude" {
        return Some("anthropic");
    }
    resolve_oauth_provider(&lowered).map(|(p, _)| p)
}

fn generate_random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "SystemRandom failed generating OAuth nonce".to_owned())?;
    Ok(bytes)
}

pub fn generate_pkce_pair() -> Result<(String, String), String> {
    let raw = generate_random_bytes::<48>()?;
    let verifier = base64url_encode(&raw);
    let challenge = pkce_s256_challenge(&verifier);
    Ok((verifier, challenge))
}

pub fn pkce_s256_challenge(verifier: &str) -> String {
    let hash = digest(&SHA256, verifier.as_bytes());
    base64url_encode(hash.as_ref())
}

fn generate_state() -> Result<String, String> {
    let raw = generate_random_bytes::<16>()?;
    let mut out = String::with_capacity(32);
    for b in raw {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

/// Best-effort open a URL in the user's default browser without blocking or printing noise.
pub fn open_browser_best_effort(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

fn build_http_client(url: &str) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
    {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| format!("cannot build OAuth HTTP client: {e}"))
}

async fn post_form(url: &str, params: &[(&str, &str)]) -> Result<(u16, Value), String> {
    let client = build_http_client(url)?;
    let resp = client
        .post(url)
        .header("Accept", "application/json")
        .form(params)
        .send()
        .await
        .map_err(|e| format!("OAuth HTTP request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&text)
        .unwrap_or_else(|_| json!({"raw_body": text.chars().take(400).collect::<String>()}));
    Ok((status, parsed))
}

#[allow(clippy::too_many_arguments)]
async fn run_pkce_browser_flow<F>(
    authorize_url: &str,
    token_url: &str,
    client_id: &str,
    scope: &str,
    extra_authorize_params: &[(&str, &str)],
    bind_port: u16,
    no_browser: bool,
    timeout_secs: u64,
    on_prompt: &mut F,
) -> Result<Value, String>
where
    F: FnMut(LoginPrompt),
{
    let listener = TcpListener::bind(("127.0.0.1", bind_port))
        .await
        .map_err(|e| {
            format!(
                "Cannot bind local OAuth callback on 127.0.0.1:{bind_port} ({e}). Try `--port <port>` or `--device`."
            )
        })?;
    let actual_port = listener
        .local_addr()
        .map_err(|e| format!("Cannot read OAuth callback port: {e}"))?
        .port();
    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let (verifier, challenge) = generate_pkce_pair()?;
    let state = generate_state()?;

    let mut params: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", &redirect_uri),
        ("scope", scope),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ];
    params.extend_from_slice(extra_authorize_params);

    let full_url = reqwest::Url::parse_with_params(authorize_url, &params)
        .map_err(|e| format!("Invalid OAuth authorize URL '{authorize_url}': {e}"))?
        .to_string();

    on_prompt(LoginPrompt::OpenUrl {
        url: full_url.clone(),
    });
    if !no_browser {
        let _ = open_browser_best_effort(&full_url);
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(5));
    let code = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "Timed out after {timeout_secs}s waiting for browser OAuth callback."
            ));
        }
        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| {
                format!("Timed out after {timeout_secs}s waiting for browser OAuth callback.")
            })?
            .map_err(|e| format!("OAuth callback accept failed: {e}"))?;

        let mut buf = [0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .unwrap_or(Ok(0))
            .unwrap_or(0);
        if n == 0 {
            continue;
        }
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        if !path.starts_with("/auth/callback") {
            let _ = write_html_response(&mut stream, 404, "Not Found").await;
            continue;
        }
        let parsed = reqwest::Url::parse(&format!("http://localhost{path}"))
            .map_err(|e| format!("Malformed callback URL: {e}"))?;
        let mut got_code = None;
        let mut got_state = None;
        let mut got_error = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => got_code = Some(v.into_owned()),
                "state" => got_state = Some(v.into_owned()),
                "error" | "error_description" => {
                    got_error = Some(match got_error {
                        Some(prev) => format!("{prev}: {v}"),
                        None => v.into_owned(),
                    });
                }
                _ => {}
            }
        }
        if let Some(err) = got_error {
            let _ = write_html_response(
                &mut stream,
                400,
                "Authentication failed. You may close this tab and check your terminal.",
            )
            .await;
            return Err(format!("OAuth provider rejected login: {err}"));
        }
        if got_state.as_deref() != Some(state.as_str()) {
            let _ = write_html_response(
                &mut stream,
                400,
                "Invalid OAuth state parameter. Please restart `hsr auth login`.",
            )
            .await;
            return Err("OAuth callback state mismatch — refusing to exchange code.".into());
        }
        let Some(auth_code) = got_code.filter(|c| !c.is_empty()) else {
            let _ = write_html_response(&mut stream, 400, "Missing authorization code.").await;
            return Err("OAuth callback missing authorization code.".into());
        };
        let _ = write_html_response(
            &mut stream,
            200,
            "Authentication complete. You may close this tab and return to hsr.",
        )
        .await;
        break auth_code;
    };

    let (status, payload) = post_form(
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", client_id),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    if !(200..300).contains(&status) {
        let detail = payload["error_description"]
            .as_str()
            .or_else(|| payload["error"].as_str())
            .or_else(|| payload["raw_body"].as_str())
            .unwrap_or("token exchange failed");
        return Err(format!(
            "OAuth token exchange failed (HTTP {status}): {detail}"
        ));
    }
    Ok(payload)
}

async fn write_html_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    let reason = if status == 200 {
        "OK"
    } else if status == 404 {
        "Not Found"
    } else {
        "Bad Request"
    };
    let body = format!(
        "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h3>hsr auth</h3><p>{message}</p></body></html>"
    );
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

#[allow(clippy::too_many_arguments)]
async fn run_device_code_flow<F>(
    device_url: &str,
    token_url: &str,
    client_id: &str,
    scope: Option<&str>,
    use_pkce: bool,
    no_browser: bool,
    timeout_secs: u64,
    on_prompt: &mut F,
) -> Result<Value, String>
where
    F: FnMut(LoginPrompt),
{
    let pkce = if use_pkce {
        Some(generate_pkce_pair()?)
    } else {
        None
    };
    let mut form: Vec<(&str, &str)> = vec![("client_id", client_id)];
    if let Some(sc) = scope.filter(|s| !s.is_empty()) {
        form.push(("scope", sc));
    }
    if let Some((_, ref challenge)) = pkce {
        form.push(("code_challenge", challenge));
        form.push(("code_challenge_method", "S256"));
    }

    let (status, device_resp) = post_form(device_url, &form).await?;
    if !(200..300).contains(&status) {
        let detail = device_resp["error_description"]
            .as_str()
            .or_else(|| device_resp["error"].as_str())
            .or_else(|| device_resp["raw_body"].as_str())
            .unwrap_or("device authorization failed");
        return Err(format!(
            "OAuth device authorization request failed (HTTP {status}): {detail}"
        ));
    }

    let device_code = device_resp["device_code"]
        .as_str()
        .or_else(|| device_resp["device_auth_id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "OAuth device response missing device_code".to_owned())?
        .to_owned();
    let user_code = device_resp["user_code"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "OAuth device response missing user_code".to_owned())?
        .to_owned();
    let verification_uri = device_resp["verification_uri"]
        .as_str()
        .or_else(|| device_resp["verification_url"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("https://auth.openai.com/codex/device")
        .to_owned();
    let verification_uri_complete = device_resp["verification_uri_complete"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    on_prompt(LoginPrompt::DeviceCode {
        verification_uri: verification_uri.clone(),
        verification_uri_complete: verification_uri_complete.clone(),
        user_code: user_code.clone(),
    });
    if !no_browser {
        let target = verification_uri_complete
            .as_deref()
            .unwrap_or(&verification_uri);
        let _ = open_browser_best_effort(target);
    }

    let poll_override_ms = std::env::var("HERMES_OAUTH_POLL_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0);
    let mut interval_ms = poll_override_ms
        .unwrap_or_else(|| device_resp["interval"].as_u64().unwrap_or(5).clamp(1, 30) * 1000);
    let expires_in = device_resp["expires_in"].as_u64().unwrap_or(timeout_secs);
    let effective_timeout = timeout_secs.min(expires_in).max(5);
    let deadline = Instant::now() + Duration::from_secs(effective_timeout);

    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "OAuth device authorization timed out after {effective_timeout}s."
            ));
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;

        let mut poll_form: Vec<(&str, &str)> = vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", &device_code),
            ("client_id", client_id),
        ];
        if let Some((ref verifier, _)) = pkce {
            poll_form.push(("code_verifier", verifier));
        }

        let (poll_status, payload) = post_form(token_url, &poll_form).await?;
        if (200..300).contains(&poll_status)
            && payload["access_token"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty())
        {
            return Ok(payload);
        }
        let err = payload["error"].as_str().unwrap_or_default();
        match err {
            "authorization_pending" | "pending" => continue,
            "slow_down" => {
                interval_ms = poll_override_ms.unwrap_or(interval_ms.saturating_add(5000));
                continue;
            }
            "access_denied" => return Err("OAuth login was denied by the user.".into()),
            "expired_token" => {
                return Err("OAuth device code expired. Please run `hsr auth login` again.".into())
            }
            _ => {
                let detail = payload["error_description"]
                    .as_str()
                    .or_else(|| (!err.is_empty()).then_some(err))
                    .or_else(|| payload["raw_body"].as_str())
                    .unwrap_or("unexpected token polling error");
                return Err(format!(
                    "OAuth device login failed (HTTP {poll_status}): {detail}"
                ));
            }
        }
    }
}

fn jwt_expiry_iso(token: &str) -> Option<String> {
    let exp = decode_jwt_claims(token)
        .get("exp")
        .and_then(Value::as_i64)?;
    chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0).map(|dt| dt.to_rfc3339())
}

fn upsert_provider_state(
    home: &Path,
    provider: &str,
    state_value: Value,
) -> Result<PathBuf, String> {
    let mut store = load_auth_store(home);
    if !store.is_object() {
        store = json!({"version": 1, "providers": {}});
    }
    let providers = store
        .as_object_mut()
        .unwrap()
        .entry("providers")
        .or_insert_with(|| json!({}));
    if !providers.is_object() {
        *providers = json!({});
    }
    providers
        .as_object_mut()
        .unwrap()
        .insert(provider.to_owned(), state_value);
    save_auth_store(home, &store)?;
    Ok(auth_store_path(home))
}

fn save_anthropic_env_token(home: &Path, token: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("Anthropic token cannot be empty.".into());
    }
    if trimmed.contains(['\n', '\r']) {
        return Err("Anthropic token cannot contain newlines.".into());
    }
    std::fs::create_dir_all(home).map_err(|e| format!("cannot create profile directory: {e}"))?;
    let env_path = home.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let key_name = if is_anthropic_oauth_token(trimmed) {
        "ANTHROPIC_TOKEN"
    } else {
        "ANTHROPIC_API_KEY"
    };
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    let mut replaced = false;
    let prefix = format!("{key_name}=");
    for line in &mut lines {
        if line.trim_start().starts_with(&prefix) {
            *line = format!("{key_name}={trimmed}");
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(format!("{key_name}={trimmed}"));
    }
    let mut content = lines.join("\n");
    content.push('\n');

    let mut staged = tempfile::Builder::new()
        .prefix(".env_")
        .suffix(".tmp")
        .tempfile_in(home)
        .map_err(|e| format!("cannot stage .env: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    staged
        .write_all(content.as_bytes())
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|e| format!("cannot write .env: {e}"))?;
    staged
        .persist(&env_path)
        .map_err(|e| format!("cannot save .env: {}", e.error))?;
    Ok(env_path)
}

/// Run an interactive OAuth login flow and persist credentials in `<home>/auth.json` (or `.env`).
pub async fn login<F>(
    home: &Path,
    opts: LoginOptions,
    mut on_prompt: F,
) -> Result<LoginResult, String>
where
    F: FnMut(LoginPrompt),
{
    let Some(canonical) = canonical_login_provider(&opts.provider) else {
        return Err(format!(
            "Unsupported auth provider '{}'. Supported: {}",
            opts.provider,
            SUPPORTED_LOGIN_PROVIDERS.join(", ")
        ));
    };
    let now = now_unix_secs();

    match canonical {
        "anthropic" => {
            let token = opts
                .token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "Provide an Anthropic OAuth/setup token or API key via `--token` (or interactive prompt)."
                        .to_owned()
                })?;
            let path = save_anthropic_env_token(home, token)?;
            let kind = if is_anthropic_oauth_token(token) {
                "Anthropic OAuth token (ANTHROPIC_TOKEN)"
            } else {
                "Anthropic API key (ANTHROPIC_API_KEY)"
            };
            on_prompt(LoginPrompt::Info(format!(
                "Saved {kind} into {} (0600).",
                path.display()
            )));
            Ok(LoginResult {
                provider: "anthropic".into(),
                auth_path: path,
                base_url: Some("https://api.anthropic.com".into()),
                expires_at: None,
            })
        }
        "openai-codex" => {
            let token_url = std::env::var("HERMES_CODEX_TOKEN_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| CODEX_OAUTH_TOKEN_URL.to_owned());
            let payload = if opts.device_code {
                let device_url = std::env::var("HERMES_CODEX_DEVICE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| CODEX_OAUTH_DEVICE_URL.to_owned());
                run_device_code_flow(
                    &device_url,
                    &token_url,
                    CODEX_OAUTH_CLIENT_ID,
                    Some("openid profile email offline_access"),
                    false,
                    opts.no_browser,
                    opts.timeout_secs,
                    &mut on_prompt,
                )
                .await?
            } else {
                let auth_url = std::env::var("HERMES_CODEX_AUTHORIZE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| CODEX_OAUTH_AUTHORIZE_URL.to_owned());
                let port = opts.port.unwrap_or(CODEX_OAUTH_CALLBACK_PORT);
                run_pkce_browser_flow(
                    &auth_url,
                    &token_url,
                    CODEX_OAUTH_CLIENT_ID,
                    "openid profile email offline_access",
                    &[
                        ("id_token_add_organizations", "true"),
                        ("codex_cli_simplified_flow", "true"),
                    ],
                    port,
                    opts.no_browser,
                    opts.timeout_secs,
                    &mut on_prompt,
                )
                .await?
            };
            let at = payload["access_token"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "Codex OAuth response missing access_token".to_owned())?;
            let rt = payload["refresh_token"].as_str().unwrap_or_default().trim();
            let id_tok = payload["id_token"].as_str().unwrap_or_default().trim();
            let mut tokens = json!({"access_token": at});
            if !rt.is_empty() {
                tokens["refresh_token"] = json!(rt);
            }
            if !id_tok.is_empty() {
                tokens["id_token"] = json!(id_tok);
            }
            let expires_at = jwt_expiry_iso(at);
            let auth_path = upsert_provider_state(
                home,
                "openai-codex",
                json!({
                    "auth_mode": "oauth",
                    "tokens": tokens,
                    "last_refresh": chrono::DateTime::<chrono::Utc>::from_timestamp(now as i64, 0)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                }),
            )?;
            Ok(LoginResult {
                provider: "openai-codex".into(),
                auth_path,
                base_url: Some(DEFAULT_CODEX_BASE_URL.into()),
                expires_at,
            })
        }
        "nous" => {
            let portal_base = opts
                .portal_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    std::env::var("HERMES_NOUS_PORTAL_URL")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| DEFAULT_NOUS_PORTAL_URL.to_owned())
                .trim_end_matches('/')
                .to_owned();
            let device_url = format!("{portal_base}/api/oauth/device/code");
            let token_url = format!("{portal_base}/api/oauth/token");
            let payload = run_device_code_flow(
                &device_url,
                &token_url,
                DEFAULT_NOUS_CLIENT_ID,
                None,
                false,
                opts.no_browser,
                opts.timeout_secs,
                &mut on_prompt,
            )
            .await?;
            let at = payload["access_token"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "Nous OAuth response missing access_token".to_owned())?;
            let rt = payload["refresh_token"].as_str().unwrap_or_default().trim();
            let env_inf = std::env::var("NOUS_INFERENCE_BASE_URL").ok();
            let inference_base = payload["inference_base_url"]
                .as_str()
                .and_then(|u| validate_nous_inference_url(u, env_inf.as_deref()))
                .unwrap_or_else(|| DEFAULT_NOUS_INFERENCE_URL.to_owned());
            let expires_at = jwt_expiry_iso(at);
            let auth_path = upsert_provider_state(
                home,
                "nous",
                json!({
                    "access_token": at,
                    "refresh_token": rt,
                    "portal_base_url": portal_base,
                    "inference_base_url": inference_base,
                }),
            )?;
            Ok(LoginResult {
                provider: "nous".into(),
                auth_path,
                base_url: Some(inference_base),
                expires_at,
            })
        }
        "qwen-oauth" => {
            let device_url = std::env::var("HERMES_QWEN_DEVICE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| QWEN_OAUTH_DEVICE_URL.to_owned());
            let token_url = std::env::var("HERMES_QWEN_TOKEN_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| QWEN_OAUTH_TOKEN_URL.to_owned());
            let payload = run_device_code_flow(
                &device_url,
                &token_url,
                QWEN_OAUTH_CLIENT_ID,
                Some("openid profile email model.completion"),
                true,
                opts.no_browser,
                opts.timeout_secs,
                &mut on_prompt,
            )
            .await?;
            let at = payload["access_token"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "Qwen OAuth response missing access_token".to_owned())?;
            let rt = payload["refresh_token"].as_str().unwrap_or_default().trim();
            let exp_in = payload["expires_in"].as_i64().unwrap_or(6 * 3600).max(1);
            let expiry_ms = (now * 1000.0) as i64 + exp_in * 1000;
            let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(expiry_ms / 1000, 0)
                .map(|d| d.to_rfc3339());
            let auth_path = upsert_provider_state(
                home,
                "qwen-oauth",
                json!({
                    "access_token": at,
                    "refresh_token": rt,
                    "expiry_date": expiry_ms,
                }),
            )?;
            Ok(LoginResult {
                provider: "qwen-oauth".into(),
                auth_path,
                base_url: Some(DEFAULT_QWEN_BASE_URL.into()),
                expires_at,
            })
        }
        "minimax-oauth" => {
            let is_cn = opts
                .region
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case("cn"))
                || opts.provider.eq_ignore_ascii_case("minimax-oauth-cn");
            let portal_base = opts
                .portal_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    std::env::var("HERMES_MINIMAX_PORTAL_URL")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| {
                    if is_cn {
                        MINIMAX_OAUTH_CN_BASE.to_owned()
                    } else {
                        MINIMAX_OAUTH_GLOBAL_BASE.to_owned()
                    }
                })
                .trim_end_matches('/')
                .to_owned();
            let device_url = format!("{portal_base}/oauth/code");
            let token_url = format!("{portal_base}/oauth/token");
            let payload = run_device_code_flow(
                &device_url,
                &token_url,
                MINIMAX_OAUTH_CLIENT_ID,
                Some("group_id profile model.completion"),
                true,
                opts.no_browser,
                opts.timeout_secs,
                &mut on_prompt,
            )
            .await?;
            let at = payload["access_token"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "MiniMax OAuth response missing access_token".to_owned())?;
            let rt = payload["refresh_token"].as_str().unwrap_or_default().trim();
            let exp_unix = payload["expired_in"]
                .as_i64()
                .or_else(|| payload["expires_in"].as_i64())
                .map(|raw| minimax_resolve_token_expiry_unix(raw, now))
                .unwrap_or(now + 3600.0);
            let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp_unix as i64, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default();
            let inference_base = if is_cn {
                MINIMAX_OAUTH_CN_INFERENCE
            } else {
                MINIMAX_OAUTH_GLOBAL_INFERENCE
            };
            let auth_path = upsert_provider_state(
                home,
                "minimax-oauth",
                json!({
                    "access_token": at,
                    "refresh_token": rt,
                    "region": if is_cn { "cn" } else { "global" },
                    "portal_base_url": portal_base,
                    "inference_base_url": inference_base,
                    "expires_at": expires_at,
                }),
            )?;
            Ok(LoginResult {
                provider: "minimax-oauth".into(),
                auth_path,
                base_url: Some(inference_base.into()),
                expires_at: (!expires_at.is_empty()).then_some(expires_at),
            })
        }
        "xai-oauth" => {
            let discovery_url = std::env::var("HERMES_XAI_DISCOVERY_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| XAI_OIDC_DISCOVERY_URL.to_owned());
            let discovery: Value = if let (Ok(auth_u), Ok(tok_u)) = (
                std::env::var("HERMES_XAI_AUTHORIZE_URL"),
                std::env::var("HERMES_XAI_TOKEN_URL"),
            ) {
                json!({
                    "authorization_endpoint": auth_u,
                    "token_endpoint": tok_u,
                })
            } else {
                let client = build_http_client(&discovery_url)?;
                let resp = client
                    .get(&discovery_url)
                    .send()
                    .await
                    .map_err(|e| format!("xAI OIDC discovery request failed: {e}"))?;
                resp.error_for_status()
                    .map_err(|e| format!("xAI OIDC discovery failed: {e}"))?
                    .json::<Value>()
                    .await
                    .map_err(|e| format!("xAI OIDC discovery returned invalid JSON: {e}"))?
            };
            let token_url = discovery["token_endpoint"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "xAI OIDC discovery missing token_endpoint".to_owned())?
                .to_owned();
            let payload = if opts.device_code {
                let device_url = discovery["device_authorization_endpoint"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "xAI OIDC discovery does not advertise device_authorization_endpoint; omit `--device` to use browser PKCE."
                            .to_owned()
                    })?;
                run_device_code_flow(
                    device_url,
                    &token_url,
                    XAI_OAUTH_CLIENT_ID,
                    Some("openid profile email offline_access"),
                    false,
                    opts.no_browser,
                    opts.timeout_secs,
                    &mut on_prompt,
                )
                .await?
            } else {
                let auth_url = discovery["authorization_endpoint"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "xAI OIDC discovery missing authorization_endpoint".to_owned()
                    })?;
                let port = opts.port.unwrap_or(0);
                run_pkce_browser_flow(
                    auth_url,
                    &token_url,
                    XAI_OAUTH_CLIENT_ID,
                    "openid profile email offline_access",
                    &[],
                    port,
                    opts.no_browser,
                    opts.timeout_secs,
                    &mut on_prompt,
                )
                .await?
            };
            let at = payload["access_token"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "xAI OAuth response missing access_token".to_owned())?;
            let rt = payload["refresh_token"].as_str().unwrap_or_default().trim();
            let expires_at = jwt_expiry_iso(at);
            let auth_path = upsert_provider_state(
                home,
                "xai-oauth",
                json!({
                    "tokens": {
                        "access_token": at,
                        "refresh_token": rt,
                    },
                    "discovery": discovery,
                }),
            )?;
            Ok(LoginResult {
                provider: "xai-oauth".into(),
                auth_path,
                base_url: Some(DEFAULT_XAI_OAUTH_BASE_URL.into()),
                expires_at,
            })
        }
        other => Err(format!("Unsupported auth provider '{other}'")),
    }
}

/// Inspect stored OAuth credentials across `<home>/auth.json`, external CLI credential files, and
/// `<home>/.env` without ever returning secret token bytes.
pub fn status(home: &Path) -> Vec<ProviderAuthStatus> {
    let store = load_auth_store(home);
    let now = now_unix_secs();
    let mut out = Vec::new();

    for &provider in &[
        "openai-codex",
        "nous",
        "xai-oauth",
        "qwen-oauth",
        "minimax-oauth",
    ] {
        let state = store.pointer(&format!("/providers/{provider}"));
        let mut source = "none".to_owned();
        let mut at = String::new();
        let mut rt = String::new();
        let mut expires_at = None;
        let mut expired = false;
        let mut base_url = default_oauth_base_url(provider).map(str::to_owned);

        if let Some(st) = state {
            if let Some(tokens) = st.get("tokens").and_then(Value::as_object) {
                at = tokens
                    .get("access_token")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                rt = tokens
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
            } else {
                at = st
                    .get("access_token")
                    .or_else(|| st.get("agent_key"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                rt = st
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
            }
            if !at.is_empty() {
                source = "auth.json".into();
                if let Some(inf) = st["inference_base_url"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    base_url = Some(inf.to_owned());
                }
                match provider {
                    "qwen-oauth" => {
                        if let Some(exp_ms) = st["expiry_date"].as_i64() {
                            expires_at =
                                chrono::DateTime::<chrono::Utc>::from_timestamp(exp_ms / 1000, 0)
                                    .map(|d| d.to_rfc3339());
                            expired =
                                ((now + QWEN_REFRESH_SKEW_SECS as f64) * 1000.0) >= exp_ms as f64;
                        }
                    }
                    "minimax-oauth" => {
                        if let Some(ts) = st["expires_at"].as_str() {
                            expires_at = Some(ts.to_owned());
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                                expired = (dt.timestamp() as f64)
                                    <= now + MINIMAX_REFRESH_SKEW_SECS as f64;
                            }
                        }
                    }
                    "openai-codex" => {
                        expires_at = jwt_expiry_iso(&at);
                        expired = jwt_is_expiring(&at, CODEX_REFRESH_SKEW_SECS, now);
                    }
                    "nous" => {
                        expires_at = jwt_expiry_iso(&at);
                        expired = jwt_is_expiring(&at, NOUS_REFRESH_SKEW_SECS, now);
                    }
                    "xai-oauth" => {
                        expires_at = jwt_expiry_iso(&at);
                        expired = jwt_is_expiring(&at, XAI_REFRESH_SKEW_SECS, now);
                    }
                    _ => {}
                }
            }
        }

        // Check external CLI fallback for codex / qwen when not in auth.json
        if at.is_empty() && provider == "openai-codex" {
            let codex_path = std::env::var("CODEX_HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .map(|h| PathBuf::from(h).join(".codex"))
                })
                .map(|p| p.join("auth.json"));
            if let Some(p) = codex_path.filter(|p| p.is_file()) {
                if let Ok(raw) = std::fs::read_to_string(&p) {
                    if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                        if let Some(tok) =
                            val.pointer("/tokens/access_token").and_then(Value::as_str)
                        {
                            if !tok.trim().is_empty() {
                                at = tok.trim().to_owned();
                                rt = val
                                    .pointer("/tokens/refresh_token")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .trim()
                                    .to_owned();
                                source = "~/.codex/auth.json".into();
                                expires_at = jwt_expiry_iso(&at);
                                expired = jwt_is_expiring(&at, CODEX_REFRESH_SKEW_SECS, now);
                            }
                        }
                    }
                }
            }
        } else if at.is_empty() && provider == "qwen-oauth" {
            if let Ok(h) = std::env::var("HOME") {
                let p = PathBuf::from(h).join(".qwen/oauth_creds.json");
                if let Ok(raw) = std::fs::read_to_string(&p) {
                    if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                        if let Some(tok) = val["access_token"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                        {
                            at = tok.trim().to_owned();
                            rt = val["refresh_token"]
                                .as_str()
                                .unwrap_or_default()
                                .trim()
                                .to_owned();
                            source = "~/.qwen/oauth_creds.json".into();
                        }
                    }
                }
            }
        }

        out.push(ProviderAuthStatus {
            provider: provider.to_owned(),
            authenticated: !at.is_empty(),
            source,
            has_refresh_token: !rt.is_empty(),
            expires_at,
            expired,
            base_url,
        });
    }

    // Anthropic OAuth / API key status from env or profile `.env`
    let env_file = std::fs::read_to_string(home.join(".env")).unwrap_or_default();
    let has_env_token = |k: &str| {
        std::env::var(k).is_ok_and(|v| !v.trim().is_empty())
            || env_file.lines().any(|l| {
                l.trim_start()
                    .strip_prefix(&format!("{k}="))
                    .is_some_and(|v| !v.trim().is_empty())
            })
    };
    let (anth_auth, anth_src) =
        if has_env_token("ANTHROPIC_TOKEN") || has_env_token("CLAUDE_CODE_OAUTH_TOKEN") {
            (true, "env/.env (OAuth token)".to_owned())
        } else if has_env_token("ANTHROPIC_API_KEY") {
            (true, "env/.env (API key)".to_owned())
        } else {
            (false, "none".to_owned())
        };
    out.push(ProviderAuthStatus {
        provider: "anthropic".into(),
        authenticated: anth_auth,
        source: anth_src,
        has_refresh_token: false,
        expires_at: None,
        expired: false,
        base_url: Some("https://api.anthropic.com".into()),
    });

    out
}

/// Remove one OAuth provider (or all providers when `provider` is `None`) from `<home>/auth.json`.
pub fn logout(home: &Path, provider: Option<&str>) -> Result<Vec<String>, String> {
    let mut store = load_auth_store(home);
    let mut removed = Vec::new();
    let Some(providers) = store.get_mut("providers").and_then(Value::as_object_mut) else {
        return Ok(removed);
    };
    match provider {
        None => {
            removed = providers.keys().cloned().collect();
            removed.sort();
            providers.clear();
        }
        Some(raw) => {
            let canonical = canonical_login_provider(raw).ok_or_else(|| {
                format!(
                    "Unsupported auth provider '{raw}'. Supported: {}",
                    SUPPORTED_LOGIN_PROVIDERS.join(", ")
                )
            })?;
            if providers.remove(canonical).is_some() {
                removed.push(canonical.to_owned());
            }
        }
    }
    if !removed.is_empty() {
        save_auth_store(home, &store)?;
    }
    Ok(removed)
}

/// Explicitly resolve and refresh a stored OAuth provider credential.
pub fn refresh(home: &Path, provider: &str) -> Result<String, String> {
    let Some((canonical, _)) = resolve_oauth_provider(provider) else {
        return Err(format!("Provider '{provider}' does not use OAuth refresh."));
    };
    let resolved =
        resolve_oauth_credentials(Some(home), canonical, &std::collections::HashMap::new())?;
    Ok(format!(
        "Credentials ready for {} (endpoint: {}).",
        resolved.provider, resolved.base_url
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn fake_jwt(exp: i64) -> String {
        let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64url_encode(format!(r#"{{"sub":"u1","exp":{exp}}}"#).as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        // RFC 7636 Appendix B test vector:
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[tokio::test]
    async fn pkce_browser_flow_exchanges_code_and_persists_codex_tokens() {
        let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_port = mock_listener.local_addr().unwrap().port();
        let seen_verifier = Arc::new(Mutex::new(String::new()));
        let seen_clone = Arc::clone(&seen_verifier);
        let access_jwt = fake_jwt(now_unix_secs() as i64 + 7200);
        let jwt_for_server = access_jwt.clone();

        let token_server = tokio::spawn(async move {
            let (mut stream, _) = mock_listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let body = String::from_utf8_lossy(&buf[..n]);
            let form_part = body.split("\r\n\r\n").nth(1).unwrap_or("");
            for pair in form_part.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    if k == "code_verifier" {
                        *seen_clone.lock().unwrap() = v.to_owned();
                    }
                }
            }
            let resp_body = json!({
                "access_token": jwt_for_server,
                "refresh_token": "rt_codex_fresh",
                "id_token": "id_tok_1"
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let auth_url = format!("http://127.0.0.1:{mock_port}/authorize");
        let token_url = format!("http://127.0.0.1:{mock_port}/token");

        let client_handle = Arc::new(Mutex::new(None));
        let handle_ref = Arc::clone(&client_handle);
        let result = run_pkce_browser_flow(
            &auth_url,
            &token_url,
            CODEX_OAUTH_CLIENT_ID,
            "openid profile",
            &[],
            0,
            true,
            15,
            &mut |prompt| {
                if let LoginPrompt::OpenUrl { url } = prompt {
                    let parsed = reqwest::Url::parse(&url).unwrap();
                    let mut state = String::new();
                    let mut redirect = String::new();
                    for (k, v) in parsed.query_pairs() {
                        if k == "state" {
                            state = v.into_owned();
                        } else if k == "redirect_uri" {
                            redirect = v.into_owned();
                        }
                    }
                    let cb = format!("{redirect}?code=auth_code_123&state={state}");
                    let h = tokio::spawn(async move {
                        let client = reqwest::Client::builder().no_proxy().build().unwrap();
                        let res = client.get(&cb).send().await.unwrap();
                        assert_eq!(res.status().as_u16(), 200);
                    });
                    *handle_ref.lock().unwrap() = Some(h);
                }
            },
        )
        .await
        .unwrap();

        let handle = client_handle.lock().unwrap().take();
        if let Some(h) = handle {
            h.await.unwrap();
        }
        token_server.await.unwrap();

        assert_eq!(result["access_token"].as_str(), Some(access_jwt.as_str()));
        assert!(!seen_verifier.lock().unwrap().is_empty());

        let path = upsert_provider_state(
            dir.path(),
            "openai-codex",
            json!({"tokens": {"access_token": access_jwt, "refresh_token": "rt_codex_fresh"}}),
        )
        .unwrap();
        assert!(path.is_file());

        let statuses = status(dir.path());
        let codex = statuses
            .iter()
            .find(|s| s.provider == "openai-codex")
            .unwrap();
        assert!(codex.authenticated);
        assert!(codex.has_refresh_token);
        assert!(!codex.expired);

        let removed = logout(dir.path(), Some("codex")).unwrap();
        assert_eq!(removed, vec!["openai-codex".to_owned()]);
    }

    #[tokio::test]
    async fn device_code_flow_polls_pending_and_saves_nous_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let portal = format!("http://127.0.0.1:{port}");
        let jwt = fake_jwt(now_unix_secs() as i64 + 3600);
        let jwt_srv = jwt.clone();

        let server = tokio::spawn(async move {
            let mut poll_count = 0u32;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let (code, body) = if path == "/api/oauth/device/code" {
                    (
                        200,
                        json!({
                            "device_code": "dev_abc",
                            "user_code": "ABCD-1234",
                            "verification_uri": "https://portal.nousresearch.com/device",
                            "interval": 1,
                            "expires_in": 60
                        })
                        .to_string(),
                    )
                } else {
                    poll_count += 1;
                    if poll_count == 1 {
                        (400, json!({"error": "authorization_pending"}).to_string())
                    } else {
                        (
                            200,
                            json!({
                                "access_token": jwt_srv,
                                "refresh_token": "rt_nous_1",
                                "inference_base_url": DEFAULT_NOUS_INFERENCE_URL
                            })
                            .to_string(),
                        )
                    }
                };
                let reason = if code == 200 { "OK" } else { "Bad Request" };
                let resp = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                if poll_count >= 2 {
                    break;
                }
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let mut saw_code = String::new();
        let mut opts = LoginOptions::for_provider("nous");
        opts.no_browser = true;
        opts.portal_url = Some(portal);
        opts.timeout_secs = 20;

        let res = login(dir.path(), opts, |p| {
            if let LoginPrompt::DeviceCode { user_code, .. } = p {
                saw_code = user_code;
            }
        })
        .await
        .unwrap();
        server.await.unwrap();

        assert_eq!(saw_code, "ABCD-1234");
        assert_eq!(res.provider, "nous");
        assert_eq!(res.base_url.as_deref(), Some(DEFAULT_NOUS_INFERENCE_URL));
        let resolved =
            resolve_oauth_credentials(Some(dir.path()), "nous", &std::collections::HashMap::new())
                .unwrap();
        assert_eq!(resolved.access_token, jwt);
    }
}
