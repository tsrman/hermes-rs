//! Profile-scoped settings for the CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::mcp::McpServerConfig;
use crate::prompt::PromptSettings;

pub use crate::compaction::CompactionSettings;

mod edit;
pub use edit::set_config_value;

pub(crate) fn truthy(value: &serde_yaml::Value, default: bool) -> bool {
    use serde_yaml::Value;
    match value {
        Value::Null => default,
        Value::Bool(value) => *value,
        Value::String(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::Mapping(value) => !value.is_empty(),
        Value::Sequence(value) => !value.is_empty(),
        Value::Tagged(tag) => truthy(&tag.value, default),
    }
}

pub struct ChatSettings {
    pub model: Option<String>,
    pub provider: &'static str,
    /// Use the OpenAI Responses API instead of Chat Completions.
    pub responses: bool,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_max_attempts: u32,
    pub max_turns: u32,
    pub fallbacks: Vec<FallbackSettings>,
    pub prompt: PromptSettings,
    pub compression: CompactionSettings,
    pub approval_timeout_secs: u32,
    pub terminal_local: bool,
    pub mcp_servers: Vec<McpServerConfig>,
    openai_api_key: Option<String>,
    openai_base_url: Option<String>,
    anthropic_api_key: Option<String>,
    anthropic_base_url: Option<String>,
}

pub struct FallbackSettings {
    pub model: String,
    pub provider: &'static str,
    pub responses: bool,
    pub base_url: String,
    pub api_key: Option<String>,
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            model: None,
            provider: "custom",
            responses: false,
            base_url: None,
            api_key: None,
            api_max_attempts: 3,
            max_turns: 16,
            fallbacks: Vec::new(),
            prompt: PromptSettings::default(),
            compression: CompactionSettings::default(),
            approval_timeout_secs: 300,
            terminal_local: true,
            mcp_servers: Vec::new(),
            openai_api_key: None,
            openai_base_url: None,
            anthropic_api_key: None,
            anthropic_base_url: None,
        }
    }
}

impl ChatSettings {
    pub fn openai_key_for(&self, base_url: &str) -> Option<String> {
        scoped_key(
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            self.openai_api_key.as_ref(),
            self.openai_base_url.as_ref(),
            &["openai.com", "openai.azure.com"],
            base_url,
        )
    }

    pub fn anthropic_key_for(&self, base_url: &str) -> Option<String> {
        scoped_key(
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            self.anthropic_api_key.as_ref(),
            self.anthropic_base_url.as_ref(),
            &["anthropic.com"],
            base_url,
        )
    }

    /// The vendor key allowed for `base_url`, chosen by provider name.
    pub fn vendor_key_for(&self, provider: &str, base_url: &str) -> Option<String> {
        match provider {
            "openai" => self.openai_key_for(base_url),
            "anthropic" => self.anthropic_key_for(base_url),
            // Custom endpoints keep using an OpenAI key paired with their URL.
            _ => self.openai_key_for(base_url),
        }
    }
}

/// A vendor key is sent only to the vendor's own hosts or to the URL it was paired with.
fn scoped_key(
    key_var: &str,
    url_var: &str,
    profile_key: Option<&String>,
    profile_url: Option<&String>,
    domains: &[&str],
    base_url: &str,
) -> Option<String> {
    let key = std::env::var(key_var)
        .ok()
        .or_else(|| profile_key.cloned())?;
    let paired = std::env::var(url_var).ok().or_else(|| profile_url.cloned());
    let url = reqwest::Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    let allowed_host = domains.iter().any(|domain| {
        host == *domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    });
    let paired = paired.is_some_and(|value| {
        value.trim().trim_end_matches('/') == base_url.trim().trim_end_matches('/')
    });
    (allowed_host || paired).then_some(key)
}

pub fn profile_home(profile: Option<&str>) -> Result<Option<PathBuf>, String> {
    let configured_home = std::env::var_os("HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    #[cfg(windows)]
    let default_home = std::env::home_dir;
    #[cfg(not(windows))]
    let default_home = || std::env::var_os("HOME").map(PathBuf::from);
    let home = configured_home.or_else(|| default_home().map(|home| home.join(".hermes")));
    let Some(home) = home else {
        return if profile.is_some() {
            Err("profile root is unavailable".into())
        } else {
            Ok(None)
        };
    };
    let is_named_home = home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|parent| parent == "profiles");
    let root = if is_named_home {
        home.parent()
            .and_then(Path::parent)
            .unwrap_or(&home)
            .to_path_buf()
    } else {
        home.clone()
    };
    let sticky = if profile.is_none() && !is_named_home {
        match std::fs::read_to_string(root.join("active_profile")) {
            Ok(value) => Some(value.trim().trim_start_matches('\u{feff}').to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("cannot read active profile: {error}")),
        }
    } else {
        None
    };
    let name = profile
        .or(sticky.as_deref())
        .filter(|name| !name.is_empty());
    let Some(name) = name else {
        return Ok(Some(home));
    };
    if name == "default" {
        return Ok(Some(root));
    }
    let valid = name.len() <= 64
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'_' | b'-'))
        });
    if !valid {
        return Err("invalid profile name".into());
    }
    let profile_home = root.join("profiles").join(name);
    let identity = ["config.yaml", ".env", "state.db"]
        .iter()
        .any(|name| profile_home.join(name).is_file());
    if !profile_home.is_dir()
        || !identity
        || root.join("profiles").join(".deleted").join(name).exists()
    {
        return Err(format!("profile '{name}' does not exist"));
    }
    Ok(Some(profile_home))
}

fn read_config(path: &Path) -> Result<serde_yaml::Value, String> {
    let config: serde_yaml::Value = match std::fs::read_to_string(path) {
        Ok(contents) => {
            serde_yaml::from_str(&contents).map_err(|_| "invalid profile config.yaml")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_yaml::Value::Null,
        Err(error) => return Err(format!("cannot read profile config: {error}")),
    };
    if !config.is_mapping() && !config.is_null() {
        return Err("profile config.yaml must be a mapping".into());
    }
    Ok(config)
}

pub fn load_chat_settings(home: Option<&Path>) -> Result<ChatSettings, String> {
    let terminal_env = std::env::var_os("TERMINAL_ENV");
    let Some(home) = home else {
        return Ok(ChatSettings {
            terminal_local: terminal_env
                .as_deref()
                .is_none_or(|backend| backend == "local"),
            ..ChatSettings::default()
        });
    };
    let config = read_config(&home.join("config.yaml"))?;
    let mut settings = ChatSettings {
        prompt: PromptSettings::from_config(&config),
        compression: CompactionSettings::from_config(&config),
        approval_timeout_secs: config["approvals"]["timeout"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(300),
        ..ChatSettings::default()
    };
    if let Some(model) = config.get("model") {
        if let Some(name) = model.as_str() {
            settings.model = Some(name.to_owned());
        } else if model.is_mapping() {
            settings.provider = match model.get("provider").and_then(serde_yaml::Value::as_str) {
                None | Some("" | "auto" | "custom") => "custom",
                Some("openai") => "openai",
                Some("anthropic") => "anthropic",
                Some(provider) => return Err(format!("provider '{provider}' is not supported")),
            };
            settings.responses = settings.provider != "anthropic"
                && model.get("api_mode").and_then(serde_yaml::Value::as_str)
                    == Some("codex_responses");
            settings.model = model
                .get("default")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned);
            settings.base_url = model
                .get("base_url")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned);
            if settings.base_url.as_deref().is_none_or(str::is_empty) {
                settings.base_url = default_base_url(settings.provider).map(str::to_owned);
            }
        } else if !model.is_null() {
            return Err("invalid model settings in config.yaml".into());
        }
    }
    let agent = config.get("agent");
    if let Some(attempts) = agent
        .and_then(|agent| agent.get("api_max_retries"))
        .and_then(serde_yaml::Value::as_i64)
        .and_then(|value| u32::try_from(value.max(1)).ok())
    {
        settings.api_max_attempts = attempts;
    }
    if let Some(max_turns) = agent
        .and_then(|agent| agent.get("max_turns"))
        .filter(|value| !value.is_null())
        .or_else(|| config.get("max_turns"))
        .and_then(serde_yaml::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
    {
        settings.max_turns = max_turns;
    }
    let mut secrets = HashMap::new();
    let env_path = home.join(".env");
    if env_path.exists() {
        let iter = dotenvy::from_path_iter(&env_path).map_err(|_| "cannot read profile .env")?;
        for entry in iter {
            let (key, value) = entry.map_err(|_| "invalid profile .env")?;
            secrets.insert(key, value);
        }
    }
    settings.api_key = secrets.get("HERMES_API_KEY").cloned();
    settings.openai_api_key = secrets.get("OPENAI_API_KEY").cloned();
    settings.openai_base_url = secrets.get("OPENAI_BASE_URL").cloned();
    settings.anthropic_api_key = secrets.get("ANTHROPIC_API_KEY").cloned();
    settings.anthropic_base_url = secrets.get("ANTHROPIC_BASE_URL").cloned();
    settings.mcp_servers = mcp_servers(&config, &secrets);
    settings.terminal_local = match &config["terminal"]["backend"] {
        serde_yaml::Value::Null => terminal_env.as_deref().map_or_else(
            || {
                secrets
                    .get("TERMINAL_ENV")
                    .is_none_or(|backend| backend == "local")
            },
            |backend| backend == "local",
        ),
        backend => backend.as_str() == Some("local"),
    };
    let fallback = config
        .get("fallback_providers")
        .filter(|entries| !entries.is_null())
        .or_else(|| config.get("fallback_model"));
    let entries = match fallback {
        Some(serde_yaml::Value::Sequence(entries)) => entries.as_slice(),
        Some(entry @ serde_yaml::Value::Mapping(_)) => std::slice::from_ref(entry),
        _ => &[],
    };
    settings.fallbacks = entries
        .iter()
        .filter_map(|entry| fallback_settings(entry, &settings, &secrets))
        .collect();
    Ok(settings)
}

fn fallback_settings(
    entry: &serde_yaml::Value,
    settings: &ChatSettings,
    secrets: &HashMap<String, String>,
) -> Option<FallbackSettings> {
    let field = |name| {
        entry
            .get(name)
            .and_then(serde_yaml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let provider = match field("provider")? {
        "custom" => "custom",
        "openai" => "openai",
        "anthropic" => "anthropic",
        _ => return None,
    };
    let responses = match (provider, field("api_mode")) {
        ("anthropic", None | Some("anthropic_messages")) => false,
        ("anthropic", _) => return None,
        (_, None | Some("chat_completions")) => false,
        (_, Some("codex_responses")) => true,
        _ => return None,
    };
    let model = field("model")?;
    let base_url = field("base_url").or(default_base_url(provider))?;
    let api_key = if let Some(key) = field("api_key") {
        Some(key.to_owned())
    } else if let Some(name) = field("key_env") {
        Some(
            std::env::var(name)
                .ok()
                .or_else(|| secrets.get(name).cloned())
                .filter(|key| !key.trim().is_empty())?,
        )
    } else if provider != "custom" {
        Some(
            settings
                .vendor_key_for(provider, base_url)
                .filter(|key| !key.trim().is_empty())?,
        )
    } else {
        None
    };
    Some(FallbackSettings {
        model: model.to_owned(),
        provider,
        responses,
        base_url: base_url.to_owned(),
        api_key,
    })
}

fn default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("https://api.openai.com/v1"),
        "anthropic" => Some("https://api.anthropic.com/v1"),
        _ => None,
    }
}

/// Stdio servers from `mcp_servers`. Entries without `command` (URL servers) are not supported yet.
fn mcp_servers(
    config: &serde_yaml::Value,
    secrets: &HashMap<String, String>,
) -> Vec<McpServerConfig> {
    let Some(entries) = config
        .get("mcp_servers")
        .and_then(serde_yaml::Value::as_mapping)
    else {
        return Vec::new();
    };
    let expand = |text: &str| expand_env(text, secrets);
    entries
        .iter()
        .filter_map(|(name, entry)| {
            let name = name.as_str()?.to_owned();
            if !truthy(
                entry.get("enabled").unwrap_or(&serde_yaml::Value::Null),
                true,
            ) {
                return None;
            }
            let command = expand(entry.get("command")?.as_str()?);
            let args = entry
                .get("args")
                .and_then(serde_yaml::Value::as_sequence)
                .map(|args| {
                    args.iter()
                        .filter_map(serde_yaml::Value::as_str)
                        .map(&expand)
                        .collect()
                })
                .unwrap_or_default();
            let env = entry
                .get("env")
                .and_then(serde_yaml::Value::as_mapping)
                .map(|env| {
                    env.iter()
                        .filter_map(|(key, value)| {
                            let value = match value {
                                serde_yaml::Value::String(text) => expand(text),
                                serde_yaml::Value::Number(number) => number.to_string(),
                                serde_yaml::Value::Bool(flag) => flag.to_string(),
                                _ => return None,
                            };
                            Some((key.as_str()?.to_owned(), value))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let seconds = |key, default| {
                entry
                    .get(key)
                    .and_then(serde_yaml::Value::as_u64)
                    .filter(|value| *value > 0)
                    .unwrap_or(default)
            };
            Some(McpServerConfig {
                name,
                command,
                args,
                env,
                timeout_secs: seconds("timeout", 120),
                connect_timeout_secs: seconds("connect_timeout", 60),
            })
        })
        .collect()
}

/// Replace `${NAME}` from the profile `.env` or the process environment.
/// Unknown names stay as written so a missing secret is visible in the failure.
fn expand_env(text: &str, secrets: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match secrets
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
                {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&rest[start..start + 3 + end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn parses_stdio_servers_and_expands_profile_variables() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "mcp_servers:\n  a:\n    command: run\n    args: ['--key=${TOKEN}', '${UNSET_VAR_X}']\n    env: {K: '${TOKEN}', N: 5}\n    timeout: 7\n  off:\n    command: x\n    enabled: false\n  remote:\n    url: https://example.test/mcp\n",
        )
        .unwrap();
        let secrets = HashMap::from([("TOKEN".to_owned(), "t0".to_owned())]);
        let servers = mcp_servers(&config, &secrets);
        assert_eq!(servers.len(), 1);
        let server = &servers[0];
        assert_eq!(server.name, "a");
        assert_eq!(server.args, ["--key=t0", "${UNSET_VAR_X}"]);
        assert_eq!(server.env["K"], "t0");
        assert_eq!(server.env["N"], "5");
        assert_eq!((server.timeout_secs, server.connect_timeout_secs), (7, 60));
    }
}
