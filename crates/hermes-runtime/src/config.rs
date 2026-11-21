//! Profile-scoped settings for the CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod edit;
pub use edit::set_config_value;

pub struct ChatSettings {
    pub model: Option<String>,
    pub provider: &'static str,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_max_attempts: u32,
    pub max_turns: u32,
    pub fallbacks: Vec<FallbackSettings>,
    openai_api_key: Option<String>,
    openai_base_url: Option<String>,
}

pub struct FallbackSettings {
    pub model: String,
    pub provider: &'static str,
    pub base_url: String,
    pub api_key: Option<String>,
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            model: None,
            provider: "custom",
            base_url: None,
            api_key: None,
            api_max_attempts: 3,
            max_turns: 16,
            fallbacks: Vec::new(),
            openai_api_key: None,
            openai_base_url: None,
        }
    }
}

impl ChatSettings {
    pub fn openai_key_for(&self, base_url: &str) -> Option<String> {
        let key = std::env::var("OPENAI_API_KEY")
            .ok()
            .or_else(|| self.openai_api_key.clone())?;
        let paired = std::env::var("OPENAI_BASE_URL")
            .ok()
            .or_else(|| self.openai_base_url.clone());
        let url = reqwest::Url::parse(base_url).ok()?;
        let host = url.host_str()?;
        let allowed_host = ["openai.com", "openai.azure.com"].iter().any(|domain| {
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
}

pub fn profile_home(profile: Option<&str>) -> Result<Option<PathBuf>, String> {
    let configured_home = std::env::var_os("HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = configured_home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".hermes")));
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
    let Some(home) = home else {
        return Ok(ChatSettings::default());
    };
    let config = read_config(&home.join("config.yaml"))?;
    let mut settings = ChatSettings::default();
    if let Some(model) = config.get("model") {
        if let Some(name) = model.as_str() {
            settings.model = Some(name.to_owned());
        } else if model.is_mapping() {
            settings.provider = match model.get("provider").and_then(serde_yaml::Value::as_str) {
                None | Some("" | "auto" | "custom") => "custom",
                Some("openai") => "openai",
                Some(provider) => return Err(format!("provider '{provider}' is not supported")),
            };
            settings.model = model
                .get("default")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned);
            settings.base_url = model
                .get("base_url")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned);
            if settings.base_url.as_deref().is_none_or(str::is_empty)
                && settings.provider == "openai"
            {
                settings.base_url = Some("https://api.openai.com/v1".to_owned());
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
        _ => return None,
    };
    if field("api_mode").is_some_and(|mode| mode != "chat_completions") {
        return None;
    }
    let model = field("model")?;
    let base_url =
        field("base_url").or((provider == "openai").then_some("https://api.openai.com/v1"))?;
    let api_key = if let Some(key) = field("api_key") {
        Some(key.to_owned())
    } else if let Some(name) = field("key_env") {
        Some(
            std::env::var(name)
                .ok()
                .or_else(|| secrets.get(name).cloned())
                .filter(|key| !key.trim().is_empty())?,
        )
    } else if provider == "openai" {
        Some(
            settings
                .openai_key_for(base_url)
                .filter(|key| !key.trim().is_empty())?,
        )
    } else {
        None
    };
    Some(FallbackSettings {
        model: model.to_owned(),
        provider,
        base_url: base_url.to_owned(),
        api_key,
    })
}
