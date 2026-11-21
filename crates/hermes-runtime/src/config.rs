//! Profile-scoped settings required by the first chat entry point.

use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct ChatSettings {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    openai_api_key: Option<String>,
    openai_base_url: Option<String>,
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

pub fn load_chat_settings(home: Option<&Path>) -> Result<ChatSettings, String> {
    let Some(home) = home else {
        return Ok(ChatSettings::default());
    };
    let config_path = home.join("config.yaml");
    let config: serde_yaml::Value = match std::fs::read_to_string(&config_path) {
        Ok(contents) => {
            serde_yaml::from_str(&contents).map_err(|_| "invalid profile config.yaml")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_yaml::Value::Null,
        Err(error) => return Err(format!("cannot read profile config: {error}")),
    };
    if !config.is_mapping() && !config.is_null() {
        return Err("profile config.yaml must be a mapping".into());
    }
    let mut settings = ChatSettings::default();
    if let Some(model) = config.get("model") {
        if let Some(name) = model.as_str() {
            settings.model = Some(name.to_owned());
        } else if model.is_mapping() {
            if let Some(provider) = model.get("provider").and_then(serde_yaml::Value::as_str) {
                if !matches!(provider, "" | "auto" | "custom" | "openai") {
                    return Err(format!("provider '{provider}' is not supported"));
                }
            }
            settings.model = model
                .get("default")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned);
            settings.base_url = model
                .get("base_url")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned);
            if settings.base_url.as_deref().is_none_or(str::is_empty)
                && model.get("provider").and_then(serde_yaml::Value::as_str) == Some("openai")
            {
                settings.base_url = Some("https://api.openai.com/v1".to_owned());
            }
        } else if !model.is_null() {
            return Err("invalid model settings in config.yaml".into());
        }
    }
    let env_path = home.join(".env");
    if env_path.exists() {
        let iter = dotenvy::from_path_iter(&env_path).map_err(|_| "cannot read profile .env")?;
        for entry in iter {
            let (key, value) = entry.map_err(|_| "invalid profile .env")?;
            match key.as_str() {
                "HERMES_API_KEY" => settings.api_key = Some(value),
                "OPENAI_API_KEY" => settings.openai_api_key = Some(value),
                "OPENAI_BASE_URL" => settings.openai_base_url = Some(value),
                _ => {}
            }
        }
    }
    Ok(settings)
}
