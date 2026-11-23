//! OAuth credential store, JWT helpers, provider OAuth token resolution/refresh, Anthropic OAuth
//! identity transforms, and MCP OAuth token loading.
//!
//! Mirrors `hermes_cli/auth*.py` and `agent/anthropic_{credentials,adapter}.py` of the Python
//! reference using the shared `<home>/auth.json` layout (atomic 0600 writes, cross-process lock).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_REFRESH_SKEW_SECS: i64 = 120;

pub const DEFAULT_NOUS_PORTAL_URL: &str = "https://portal.nousresearch.com";
pub const DEFAULT_NOUS_INFERENCE_URL: &str = "https://inference-api.nousresearch.com/v1";
pub const DEFAULT_NOUS_WELCOME_URL: &str = "https://welcome-api.nousresearch.com/v1";
pub const DEFAULT_NOUS_CLIENT_ID: &str = "hermes-cli";
pub const NOUS_REFRESH_SKEW_SECS: i64 = 120;

pub const DEFAULT_XAI_OAUTH_BASE_URL: &str = "https://api.x.ai/v1";
pub const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const XAI_REFRESH_SKEW_SECS: i64 = 3600;

pub const DEFAULT_QWEN_BASE_URL: &str = "https://portal.qwen.ai/v1";
pub const QWEN_OAUTH_CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
pub const QWEN_OAUTH_TOKEN_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/token";
pub const QWEN_REFRESH_SKEW_SECS: i64 = 120;

pub const MINIMAX_OAUTH_CLIENT_ID: &str = "78257093-7e40-4613-99e0-527b14b39113";
pub const MINIMAX_OAUTH_GLOBAL_BASE: &str = "https://api.minimax.io";
pub const MINIMAX_OAUTH_CN_BASE: &str = "https://api.minimaxi.com";
pub const MINIMAX_OAUTH_GLOBAL_INFERENCE: &str = "https://api.minimax.io/anthropic";
pub const MINIMAX_OAUTH_CN_INFERENCE: &str = "https://api.minimaxi.com/anthropic";
pub const MINIMAX_REFRESH_SKEW_SECS: i64 = 60;

pub const CLAUDE_CODE_SYSTEM_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
pub const ANTHROPIC_OAUTH_ONLY_BETAS: &[&str] = &["claude-code-20250219", "oauth-2025-04-20"];
pub const ANTHROPIC_COMMON_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
];
pub const CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.74";

const ALLOWED_NOUS_INFERENCE_HOSTS: &[&str] = &[
    "inference-api.nousresearch.com",
    "welcome-api.nousresearch.com",
];

// ── Base64url + JWT helpers ─────────────────────────────────────────────────────────

pub fn base64url_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

pub fn base64_decode_flexible(input: &str) -> Option<Vec<u8>> {
    let cleaned: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return Some(Vec::new());
    }
    let val = |b: u8| -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    };
    let mut stripped = cleaned.as_slice();
    while stripped.last() == Some(&b'=') {
        stripped = &stripped[..stripped.len() - 1];
    }
    let mut out = Vec::with_capacity(stripped.len() * 3 / 4);
    let mut i = 0;
    while i < stripped.len() {
        let rem = stripped.len() - i;
        if rem == 1 {
            return None;
        }
        let a = val(stripped[i])?;
        let b = val(stripped[i + 1])?;
        let c = if rem > 2 { val(stripped[i + 2])? } else { 0 };
        let d = if rem > 3 { val(stripped[i + 3])? } else { 0 };
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((triple >> 16) & 0xff) as u8);
        if rem > 2 {
            out.push(((triple >> 8) & 0xff) as u8);
        }
        if rem > 3 {
            out.push((triple & 0xff) as u8);
        }
        i += 4;
    }
    Some(out)
}

/// Decode a JWT's payload claims without signature verification.
/// Returns `{}` on malformed tokens (matching `auth_constants.py::_decode_jwt_claims`).
pub fn decode_jwt_claims(token: &str) -> Map<String, Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Map::new();
    }
    let Some(bytes) = base64_decode_flexible(parts[1]) else {
        return Map::new();
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

pub fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn jwt_is_expiring(access_token: &str, skew_secs: i64, now_secs: f64) -> bool {
    let claims = decode_jwt_claims(access_token);
    claims
        .get("exp")
        .and_then(Value::as_f64)
        .is_some_and(|exp| exp <= now_secs + skew_secs.max(0) as f64)
}

/// Resolve MiniMax `expired_in` (either unix ms or TTL seconds) to unix seconds.
/// Matches `auth_minimax.py::_minimax_resolve_token_expiry_unix`.
pub fn minimax_resolve_token_expiry_unix(expired_in: i64, now_secs: f64) -> f64 {
    let now_ms = (now_secs * 1000.0) as i64;
    if expired_in > now_ms / 2 {
        expired_in as f64 / 1000.0
    } else {
        now_secs + expired_in.max(1) as f64
    }
}

/// Validate a Portal-returned Nous inference URL against the host allowlist.
/// Matches `auth_nous.py::_validate_nous_inference_url_from_network`.
pub fn validate_nous_inference_url(url: &str, env_override: Option<&str>) -> Option<String> {
    let cleaned = url.trim();
    if cleaned.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(cleaned).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?;
    let allowed = ALLOWED_NOUS_INFERENCE_HOSTS.contains(&host)
        || env_override
            .and_then(|ov| reqwest::Url::parse(ov.trim()).ok())
            .and_then(|u| u.host_str().map(|h| h == host))
            .unwrap_or(false);
    allowed.then(|| cleaned.trim_end_matches('/').to_owned())
}

// ── Anthropic OAuth identity helpers ────────────────────────────────────────────────

/// True for Anthropic OAuth/setup tokens (`sk-ant-oat*`, `eyJ*`, `cc-*`); false for Console API
/// keys (`sk-ant-api*`, `sk-ant-usr*`, `sk-ant-svc*`, `sk-ant-admin*`).
/// Matches `agent/anthropic_credentials.py::_is_oauth_token`.
pub fn is_anthropic_oauth_token(key: &str) -> bool {
    if key.is_empty()
        || key.starts_with("sk-ant-api")
        || key.starts_with("sk-ant-usr")
        || key.starts_with("sk-ant-svc")
        || key.starts_with("sk-ant-admin")
    {
        return false;
    }
    key.starts_with("sk-ant-") || key.starts_with("eyJ") || key.starts_with("cc-")
}

pub fn is_native_anthropic_host(base_url: &str) -> bool {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return true;
    }
    reqwest::Url::parse(trimmed)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.eq_ignore_ascii_case("api.anthropic.com"))
        })
        .unwrap_or(false)
}

/// Matches `agent/anthropic_credentials.py::anthropic_route_is_oauth`.
pub fn anthropic_route_is_oauth(base_url: &str, api_key: &str, provider: Option<&str>) -> bool {
    let is_anth = is_native_anthropic_host(base_url)
        || provider.is_some_and(|p| p.trim().eq_ignore_ascii_case("anthropic"));
    is_anth && is_anthropic_oauth_token(api_key)
}

fn normalize_to_mcp_wire(name: &str) -> String {
    if name.starts_with("mcp__") {
        name.to_owned()
    } else if let Some(rest) = name.strip_prefix("mcp_") {
        format!("mcp__{rest}")
    } else {
        format!("mcp__{name}")
    }
}

fn oauth_tool_alias(name: &str) -> Option<&'static str> {
    match name {
        "session_search" => Some("chat_history_lookup"),
        "memory" => Some("context_notes"),
        _ => None,
    }
}

/// Build the OAuth wire-name mapping for a tool set (`name -> mcp__...`) matching
/// `agent/anthropic_adapter.py::_oauth_wire_namer`.
pub fn anthropic_oauth_wire_names(tool_names: &[&str]) -> HashMap<String, String> {
    let claimed: HashSet<String> = tool_names
        .iter()
        .filter(|name| oauth_tool_alias(name).is_none())
        .map(|name| normalize_to_mcp_wire(name))
        .collect();
    let mut map = HashMap::new();
    for &name in tool_names {
        let candidate = match oauth_tool_alias(name) {
            Some(alias) if !claimed.contains(&format!("mcp__{alias}")) => alias,
            _ => name,
        };
        map.insert(name.to_owned(), normalize_to_mcp_wire(candidate));
    }
    map
}

/// Reverse map an OAuth wire tool name back to the Hermes tool name.
pub fn anthropic_oauth_reverse_tool_name(
    wire: &str,
    known_names: &HashMap<String, String>,
) -> String {
    for (orig, mapped) in known_names {
        if mapped == wire {
            return orig.clone();
        }
    }
    if let Some(stripped) = wire.strip_prefix("mcp__") {
        match stripped {
            "chat_history_lookup" => return "session_search".into(),
            "context_notes" => return "memory".into(),
            other => return other.to_owned(),
        }
    }
    wire.to_owned()
}

// ── Auth store (`<home>/auth.json`) ─────────────────────────────────────────────────

pub fn auth_store_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

pub fn load_auth_store(home: &Path) -> Value {
    let path = auth_store_path(home);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return json!({"version": 1, "providers": {}});
    };
    serde_json::from_str::<Value>(raw.strip_prefix('\u{feff}').unwrap_or(&raw))
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"version": 1, "providers": {}}))
}

pub fn save_auth_store(home: &Path, store: &Value) -> Result<(), String> {
    use std::io::Write;
    std::fs::create_dir_all(home)
        .map_err(|error| format!("cannot create profile directory: {error}"))?;
    let path = auth_store_path(home);
    let payload = serde_json::to_string_pretty(store)
        .map_err(|error| format!("cannot encode auth.json: {error}"))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".auth_")
        .suffix(".tmp")
        .tempfile_in(home)
        .map_err(|error| format!("cannot stage auth.json: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    staged
        .write_all(payload.as_bytes())
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|error| format!("cannot write auth.json: {error}"))?;
    staged
        .persist(&path)
        .map_err(|error| format!("cannot save auth.json: {}", error.error))?;
    Ok(())
}

// ── OAuth provider resolution + refresh ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ResolvedOAuth {
    pub provider: &'static str,
    pub access_token: String,
    pub base_url: String,
    pub responses: bool,
    pub anthropic: bool,
}

/// Canonical OAuth provider info `(canonical_name, uses_responses, is_anthropic)`.
pub fn resolve_oauth_provider(name: &str) -> Option<(&'static str, bool)> {
    match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "openai-codex" | "codex" | "chatgpt" => Some(("openai-codex", true)),
        "nous" | "nous-portal" | "portal" => Some(("nous", false)),
        "xai-oauth" | "grok-oauth" => Some(("xai-oauth", true)),
        "qwen-oauth" | "qwen-cli" => Some(("qwen-oauth", false)),
        "minimax-oauth" | "minimax-oauth-cn" => Some(("minimax-oauth", false)),
        _ => None,
    }
}

pub fn default_oauth_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "openai-codex" => Some(DEFAULT_CODEX_BASE_URL),
        "nous" => Some(DEFAULT_NOUS_INFERENCE_URL),
        "xai-oauth" => Some(DEFAULT_XAI_OAUTH_BASE_URL),
        "qwen-oauth" => Some(DEFAULT_QWEN_BASE_URL),
        "minimax-oauth" => Some(MINIMAX_OAUTH_GLOBAL_INFERENCE),
        _ => None,
    }
}

fn post_form_refresh_blocking(url: &str, params: &[(&str, &str)]) -> Result<Value, String> {
    let url = url.to_owned();
    let owned: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let run = move || -> Result<Value, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot build OAuth refresh runtime: {e}"))?;
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| format!("cannot build OAuth HTTP client: {e}"))?;
            let resp = client
                .post(&url)
                .header("Accept", "application/json")
                .form(&owned)
                .send()
                .await
                .map_err(|e| format!("OAuth refresh request failed: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(format!(
                    "OAuth refresh failed with HTTP {}: {}",
                    status.as_u16(),
                    text.chars().take(300).collect::<String>()
                ));
            }
            serde_json::from_str::<Value>(&text)
                .map_err(|e| format!("OAuth refresh returned invalid JSON: {e}"))
        })
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(run)
            .join()
            .map_err(|_| "OAuth refresh thread panicked".to_owned())?
    } else {
        run()
    }
}

fn provider_tokens_from_store(store: &Value, provider: &str) -> Option<(String, String, Value)> {
    if let Some(state) = store.pointer(&format!("/providers/{provider}")) {
        if let Some(tokens) = state.get("tokens").and_then(Value::as_object) {
            let at = tokens
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            let rt = tokens
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !at.is_empty() {
                return Some((at.to_owned(), rt.to_owned(), state.clone()));
            }
        }
        // Flat shape (Nous / MiniMax): access_token directly on provider state
        let at = state
            .get("access_token")
            .or_else(|| state.get("agent_key"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let rt = state
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !at.is_empty() {
            return Some((at.to_owned(), rt.to_owned(), state.clone()));
        }
    }
    // Fallback to credential_pool.<provider>
    if let Some(entries) = store
        .pointer(&format!("/credential_pool/{provider}"))
        .and_then(Value::as_array)
    {
        for entry in entries {
            let at = entry
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            let rt = entry
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !at.is_empty() {
                return Some((at.to_owned(), rt.to_owned(), entry.clone()));
            }
        }
    }
    None
}

fn update_provider_tokens(
    home: &Path,
    provider: &str,
    nested: bool,
    new_access: &str,
    new_refresh: &str,
    extra: Option<(&str, Value)>,
) {
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
    let state = providers
        .as_object_mut()
        .unwrap()
        .entry(provider)
        .or_insert_with(|| json!({}));
    if !state.is_object() {
        *state = json!({});
    }
    if nested {
        let tokens = state
            .as_object_mut()
            .unwrap()
            .entry("tokens")
            .or_insert_with(|| json!({}));
        if let Some(obj) = tokens.as_object_mut() {
            obj.insert("access_token".into(), json!(new_access));
            if !new_refresh.is_empty() {
                obj.insert("refresh_token".into(), json!(new_refresh));
            }
        }
    } else if let Some(obj) = state.as_object_mut() {
        obj.insert("access_token".into(), json!(new_access));
        if !new_refresh.is_empty() {
            obj.insert("refresh_token".into(), json!(new_refresh));
        }
    }
    if let Some((k, v)) = extra {
        if let Some(obj) = state.as_object_mut() {
            obj.insert(k.into(), v);
        }
    }
    let _ = save_auth_store(home, &store);
}

/// Resolve OAuth credentials for `provider` from `<home>/auth.json` (or external CLI files for
/// `openai-codex` / `qwen-oauth`), refreshing transparently if the token is expiring.
pub fn resolve_oauth_credentials(
    home: Option<&Path>,
    provider: &str,
    secrets: &HashMap<String, String>,
) -> Result<ResolvedOAuth, String> {
    let secret = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| secrets.get(name).cloned())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let now = now_unix_secs();

    match provider {
        "openai-codex" => {
            let store = home.map(load_auth_store).unwrap_or_else(|| json!({}));
            let (mut at, mut rt, _) = provider_tokens_from_store(&store, "openai-codex")
                .or_else(read_codex_cli_tokens)
                .ok_or_else(|| {
                    "No Codex OAuth credentials stored in auth.json. Authenticate with `hermes auth` or place tokens in auth.json.".to_owned()
                })?;
            if jwt_is_expiring(&at, CODEX_REFRESH_SKEW_SECS, now) && !rt.is_empty() {
                let token_url = secret("HERMES_CODEX_TOKEN_URL")
                    .unwrap_or_else(|| CODEX_OAUTH_TOKEN_URL.to_owned());
                let payload = post_form_refresh_blocking(
                    &token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", &rt),
                        ("client_id", CODEX_OAUTH_CLIENT_ID),
                    ],
                )?;
                if let Some(new_at) = payload["access_token"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    at = new_at.to_owned();
                    if let Some(new_rt) = payload["refresh_token"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        rt = new_rt.to_owned();
                    }
                    if let Some(h) = home {
                        update_provider_tokens(h, "openai-codex", true, &at, &rt, None);
                    }
                }
            }
            let base_url = secret("HERMES_CODEX_BASE_URL")
                .unwrap_or_else(|| DEFAULT_CODEX_BASE_URL.to_owned())
                .trim_end_matches('/')
                .to_owned();
            Ok(ResolvedOAuth {
                provider: "openai-codex",
                access_token: at,
                base_url,
                responses: true,
                anthropic: false,
            })
        }
        "nous" => {
            let store = home.map(load_auth_store).unwrap_or_else(|| json!({}));
            let (mut at, mut rt, state) =
                provider_tokens_from_store(&store, "nous").ok_or_else(|| {
                    "No Nous Portal OAuth credentials stored in auth.json.".to_owned()
                })?;
            let portal_base = state["portal_base_url"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_NOUS_PORTAL_URL)
                .trim_end_matches('/')
                .to_owned();
            let env_inf = secret("NOUS_INFERENCE_BASE_URL");
            let mut base_url = env_inf
                .clone()
                .or_else(|| {
                    state["inference_base_url"]
                        .as_str()
                        .and_then(|u| validate_nous_inference_url(u, env_inf.as_deref()))
                })
                .unwrap_or_else(|| DEFAULT_NOUS_INFERENCE_URL.to_owned());
            if jwt_is_expiring(&at, NOUS_REFRESH_SKEW_SECS, now) && !rt.is_empty() {
                let token_url = format!("{portal_base}/api/oauth/token");
                let payload = post_form_refresh_blocking(
                    &token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", &rt),
                        ("client_id", DEFAULT_NOUS_CLIENT_ID),
                    ],
                )?;
                if let Some(new_at) = payload["access_token"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    at = new_at.to_owned();
                    if let Some(new_rt) = payload["refresh_token"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        rt = new_rt.to_owned();
                    }
                    if let Some(net_url) = payload["inference_base_url"]
                        .as_str()
                        .and_then(|u| validate_nous_inference_url(u, env_inf.as_deref()))
                    {
                        if env_inf.is_none() {
                            base_url = net_url.clone();
                        }
                    }
                    if let Some(h) = home {
                        update_provider_tokens(
                            h,
                            "nous",
                            false,
                            &at,
                            &rt,
                            Some(("inference_base_url", json!(base_url))),
                        );
                    }
                }
            }
            Ok(ResolvedOAuth {
                provider: "nous",
                access_token: at,
                base_url,
                responses: false,
                anthropic: false,
            })
        }
        "xai-oauth" => {
            let store = home.map(load_auth_store).unwrap_or_else(|| json!({}));
            let (mut at, mut rt, state) = provider_tokens_from_store(&store, "xai-oauth")
                .ok_or_else(|| "No xAI OAuth credentials stored in auth.json.".to_owned())?;
            if jwt_is_expiring(&at, XAI_REFRESH_SKEW_SECS, now) && !rt.is_empty() {
                if let Some(token_url) = secret("HERMES_XAI_TOKEN_URL").or_else(|| {
                    state
                        .pointer("/discovery/token_endpoint")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }) {
                    let payload = post_form_refresh_blocking(
                        &token_url,
                        &[
                            ("grant_type", "refresh_token"),
                            ("refresh_token", &rt),
                            ("client_id", XAI_OAUTH_CLIENT_ID),
                        ],
                    )?;
                    if let Some(new_at) = payload["access_token"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        at = new_at.to_owned();
                        if let Some(new_rt) = payload["refresh_token"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            rt = new_rt.to_owned();
                        }
                        if let Some(h) = home {
                            update_provider_tokens(h, "xai-oauth", true, &at, &rt, None);
                        }
                    }
                }
            }
            let base_url = secret("HERMES_XAI_OAUTH_BASE_URL")
                .unwrap_or_else(|| DEFAULT_XAI_OAUTH_BASE_URL.to_owned())
                .trim_end_matches('/')
                .to_owned();
            Ok(ResolvedOAuth {
                provider: "xai-oauth",
                access_token: at,
                base_url,
                responses: true,
                anthropic: false,
            })
        }
        "qwen-oauth" => {
            let store = home.map(load_auth_store).unwrap_or_else(|| json!({}));
            let (mut at, mut rt, state) = provider_tokens_from_store(&store, "qwen-oauth")
                .or_else(read_qwen_cli_tokens)
                .ok_or_else(|| {
                    "No Qwen OAuth credentials found in auth.json or ~/.qwen/oauth_creds.json."
                        .to_owned()
                })?;
            let expiring = state["expiry_date"].as_i64().is_some_and(|exp_ms| {
                ((now + QWEN_REFRESH_SKEW_SECS as f64) * 1000.0) >= exp_ms as f64
            });
            if expiring && !rt.is_empty() {
                let token_url = secret("HERMES_QWEN_TOKEN_URL")
                    .unwrap_or_else(|| QWEN_OAUTH_TOKEN_URL.to_owned());
                let payload = post_form_refresh_blocking(
                    &token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", &rt),
                        ("client_id", QWEN_OAUTH_CLIENT_ID),
                    ],
                )?;
                if let Some(new_at) = payload["access_token"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    at = new_at.to_owned();
                    if let Some(new_rt) = payload["refresh_token"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        rt = new_rt.to_owned();
                    }
                    let exp_in = payload["expires_in"].as_i64().unwrap_or(6 * 3600).max(1);
                    let new_exp_ms = (now * 1000.0) as i64 + exp_in * 1000;
                    if let Some(h) = home {
                        update_provider_tokens(
                            h,
                            "qwen-oauth",
                            false,
                            &at,
                            &rt,
                            Some(("expiry_date", json!(new_exp_ms))),
                        );
                    }
                }
            }
            let base_url = secret("HERMES_QWEN_BASE_URL")
                .unwrap_or_else(|| DEFAULT_QWEN_BASE_URL.to_owned())
                .trim_end_matches('/')
                .to_owned();
            Ok(ResolvedOAuth {
                provider: "qwen-oauth",
                access_token: at,
                base_url,
                responses: false,
                anthropic: false,
            })
        }
        "minimax-oauth" => {
            let store = home.map(load_auth_store).unwrap_or_else(|| json!({}));
            let (mut at, mut rt, state) = provider_tokens_from_store(&store, "minimax-oauth")
                .ok_or_else(|| "No MiniMax OAuth credentials stored in auth.json.".to_owned())?;
            let is_cn = state["region"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case("cn"));
            let portal_base = state["portal_base_url"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(if is_cn {
                    MINIMAX_OAUTH_CN_BASE
                } else {
                    MINIMAX_OAUTH_GLOBAL_BASE
                })
                .trim_end_matches('/')
                .to_owned();
            let expiring = state["expires_at"]
                .as_str()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .is_some_and(|dt| {
                    (dt.timestamp() as f64) <= now + MINIMAX_REFRESH_SKEW_SECS as f64
                });
            if expiring && !rt.is_empty() {
                let token_url = format!("{portal_base}/oauth/token");
                let payload = post_form_refresh_blocking(
                    &token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", &rt),
                        ("client_id", MINIMAX_OAUTH_CLIENT_ID),
                    ],
                )?;
                if let Some(new_at) = payload["access_token"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    at = new_at.to_owned();
                    if let Some(new_rt) = payload["refresh_token"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        rt = new_rt.to_owned();
                    }
                    let exp_unix = payload["expired_in"]
                        .as_i64()
                        .map(|raw| minimax_resolve_token_expiry_unix(raw, now))
                        .unwrap_or(now + 3600.0);
                    let exp_iso =
                        chrono::DateTime::<chrono::Utc>::from_timestamp(exp_unix as i64, 0)
                            .map(|d| d.to_rfc3339())
                            .unwrap_or_default();
                    if let Some(h) = home {
                        update_provider_tokens(
                            h,
                            "minimax-oauth",
                            false,
                            &at,
                            &rt,
                            Some(("expires_at", json!(exp_iso))),
                        );
                    }
                }
            }
            let base_url = state["inference_base_url"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(if is_cn {
                    MINIMAX_OAUTH_CN_INFERENCE
                } else {
                    MINIMAX_OAUTH_GLOBAL_INFERENCE
                })
                .trim_end_matches('/')
                .to_owned();
            Ok(ResolvedOAuth {
                provider: "minimax-oauth",
                access_token: at,
                base_url,
                responses: false,
                anthropic: true,
            })
        }
        other => Err(format!("Unknown OAuth provider '{other}'")),
    }
}

fn read_codex_cli_tokens() -> Option<(String, String, Value)> {
    let codex_home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".codex"))
        })?;
    let path = codex_home.join("auth.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let data: Value = serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw)).ok()?;
    let tokens = data.get("tokens")?;
    let at = tokens["access_token"].as_str()?.trim();
    let rt = tokens["refresh_token"].as_str()?.trim();
    if at.is_empty() || rt.is_empty() {
        return None;
    }
    Some((at.to_owned(), rt.to_owned(), data))
}

fn read_qwen_cli_tokens() -> Option<(String, String, Value)> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".qwen/oauth_creds.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let data: Value = serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw)).ok()?;
    let at = data["access_token"].as_str()?.trim();
    let rt = data["refresh_token"].as_str().unwrap_or_default().trim();
    if at.is_empty() {
        return None;
    }
    Some((at.to_owned(), rt.to_owned(), data))
}

/// Resolve an MCP server's stored OAuth bearer token from `<home>/mcp_oauth/<server>.json` or
/// `<home>/auth.json` (`mcp_oauth.<server>`), refreshing via `refresh_token` + `token_endpoint`
/// when expired.
pub fn resolve_mcp_oauth_token(home: &Path, server_name: &str) -> Option<String> {
    let safe_name: String = server_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file_path = home.join("mcp_oauth").join(format!("{safe_name}.json"));
    let mut entry = std::fs::read_to_string(&file_path)
        .ok()
        .and_then(|raw| {
            serde_json::from_str::<Value>(raw.strip_prefix('\u{feff}').unwrap_or(&raw)).ok()
        })
        .or_else(|| {
            let store = load_auth_store(home);
            store.pointer(&format!("/mcp_oauth/{server_name}")).cloned()
        })?;
    let at = entry["access_token"].as_str()?.trim().to_owned();
    if at.is_empty() {
        return None;
    }
    let now = now_unix_secs();
    let expiring = entry["expires_at"]
        .as_f64()
        .is_some_and(|exp| exp <= now + 60.0)
        || jwt_is_expiring(&at, 60, now);
    if expiring {
        let rt = entry["refresh_token"].as_str().unwrap_or_default().trim();
        let token_url = entry["token_endpoint"].as_str().unwrap_or_default().trim();
        let client_id = entry["client_id"].as_str().unwrap_or_default().trim();
        if !rt.is_empty() && !token_url.is_empty() {
            let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", rt)];
            if !client_id.is_empty() {
                form.push(("client_id", client_id));
            }
            if let Ok(payload) = post_form_refresh_blocking(token_url, &form) {
                if let Some(new_at) = payload["access_token"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    entry["access_token"] = json!(new_at);
                    if let Some(new_rt) = payload["refresh_token"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        entry["refresh_token"] = json!(new_rt);
                    }
                    if let Some(exp_in) = payload["expires_in"].as_f64() {
                        entry["expires_at"] = json!(now + exp_in);
                    }
                    if file_path.is_file() {
                        let _ = std::fs::write(
                            &file_path,
                            serde_json::to_string_pretty(&entry).unwrap_or_default(),
                        );
                    }
                    return Some(new_at.to_owned());
                }
            }
        }
    }
    Some(at)
}
