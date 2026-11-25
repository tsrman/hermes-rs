//! Pure in-app Setup / Settings state for `hsr-gui` (unit-testable headless).

use std::fs;
use std::path::Path;

use hermes_runtime::config::{
    ensure_profile_scaffold, load_chat_settings, set_config_value, set_env_value,
};
use hermes_runtime::skills_sync;

use crate::editor::TextBuffer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupField {
    BaseUrl,
    Model,
    ApiKey,
    WebApiKey,
}

#[derive(Clone, Copy, Debug)]
pub struct GuiProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub default_model: &'static str,
    pub key_env: Option<&'static str>,
    pub default_base_url: Option<&'static str>,
    pub is_oauth: bool,
    pub keyless: bool,
}

pub const GUI_PROVIDER_PRESETS: &[GuiProviderPreset] = &[
    GuiProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        default_model: "anthropic/claude-sonnet-4",
        key_env: Some("OPENROUTER_API_KEY"),
        default_base_url: None,
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "openai",
        label: "OpenAI",
        default_model: "gpt-4.1",
        key_env: Some("OPENAI_API_KEY"),
        default_base_url: None,
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "anthropic",
        label: "Anthropic",
        default_model: "claude-sonnet-4-20250514",
        key_env: Some("ANTHROPIC_API_KEY"),
        default_base_url: None,
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        default_model: "deepseek-chat",
        key_env: Some("DEEPSEEK_API_KEY"),
        default_base_url: None,
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "xai",
        label: "xAI Grok",
        default_model: "grok-4",
        key_env: Some("XAI_API_KEY"),
        default_base_url: None,
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "gemini",
        label: "Gemini",
        default_model: "gemini-2.5-pro",
        key_env: Some("GEMINI_API_KEY"),
        default_base_url: None,
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "ollama",
        label: "Ollama (local)",
        default_model: "qwen2.5-coder:14b",
        key_env: None,
        default_base_url: Some("http://127.0.0.1:11434/v1"),
        is_oauth: false,
        keyless: true,
    },
    GuiProviderPreset {
        id: "custom",
        label: "Custom URL",
        default_model: "gpt-4o-mini",
        key_env: Some("HERMES_API_KEY"),
        default_base_url: Some("http://127.0.0.1:8000/v1"),
        is_oauth: false,
        keyless: false,
    },
    GuiProviderPreset {
        id: "nous",
        label: "Nous OAuth",
        default_model: "hermes-4-70b",
        key_env: None,
        default_base_url: None,
        is_oauth: true,
        keyless: false,
    },
    GuiProviderPreset {
        id: "openai-codex",
        label: "Codex OAuth",
        default_model: "gpt-5.3-codex",
        key_env: None,
        default_base_url: None,
        is_oauth: true,
        keyless: false,
    },
];

pub const GUI_WEB_BACKENDS: &[(&str, Option<&str>)] = &[
    ("none", None),
    ("tavily", Some("TAVILY_API_KEY")),
    ("brave", Some("BRAVE_API_KEY")),
    ("exa", Some("EXA_API_KEY")),
    ("firecrawl", Some("FIRECRAWL_API_KEY")),
    ("searxng", Some("SEARXNG_URL")),
    ("ddgs", None),
];

#[derive(Clone, Debug)]
pub struct SetupDraft {
    pub open: bool,
    pub provider: String,
    pub base_url: TextBuffer,
    pub api_mode: String,
    pub model: TextBuffer,
    pub api_key: TextBuffer,
    pub has_existing_api_key: bool,
    pub web_backend: String,
    pub web_api_key: TextBuffer,
    pub memory_enabled: bool,
    pub installed_skills_count: usize,
    pub syncing_skills: bool,
    pub oauth_in_progress: bool,
    pub notice: Option<(bool, String)>,
}

impl SetupDraft {
    pub fn load_from_profile(profile: &Path) -> Self {
        let config_exists = profile.join("config.yaml").is_file();
        let loaded = load_chat_settings(Some(profile)).ok();
        let provider = loaded
            .as_ref()
            .map(|s| s.provider.to_owned())
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "openrouter".into());
        let preset = preset_for(&provider);

        let mut base_url = TextBuffer::new();
        if let Some(url) = loaded
            .as_ref()
            .and_then(|s| s.base_url.clone())
            .or_else(|| preset.and_then(|p| p.default_base_url.map(str::to_owned)))
        {
            base_url.insert_str(&url);
        }

        let mut model = TextBuffer::new();
        let configured_model = loaded.as_ref().and_then(|s| s.model.clone());
        let needs_initial_setup = !config_exists
            || configured_model
                .as_deref()
                .is_none_or(|m| m.trim().is_empty());
        if let Some(m) = configured_model.or_else(|| preset.map(|p| p.default_model.to_owned())) {
            model.insert_str(&m);
        }

        let has_existing_api_key = profile_has_key_for(profile, &provider);
        let installed_skills_count = skills_sync::list_installed_skills(profile).len();

        Self {
            open: needs_initial_setup,
            provider,
            base_url,
            api_mode: "openai".into(),
            model,
            api_key: TextBuffer::new(),
            has_existing_api_key,
            web_backend: "none".into(),
            web_api_key: TextBuffer::new(),
            memory_enabled: true,
            installed_skills_count,
            syncing_skills: false,
            oauth_in_progress: false,
            notice: if needs_initial_setup {
                Some((
                    false,
                    "Welcome! Configure your provider, model, and optional skills below.".into(),
                ))
            } else {
                None
            },
        }
    }

    pub fn select_provider(&mut self, profile: &Path, provider_id: &str) {
        self.provider = provider_id.to_owned();
        if let Some(preset) = preset_for(provider_id) {
            self.model.clear();
            self.model.insert_str(preset.default_model);
            self.base_url.clear();
            if let Some(url) = preset.default_base_url {
                self.base_url.insert_str(url);
            }
        }
        self.has_existing_api_key = profile_has_key_for(profile, provider_id);
        self.notice = None;
    }

    pub fn preset(&self) -> Option<&'static GuiProviderPreset> {
        preset_for(&self.provider)
    }

    pub fn key_env_name(&self) -> Option<&'static str> {
        self.preset()
            .and_then(|p| p.key_env)
            .or(Some("HERMES_API_KEY"))
    }

    pub fn web_env_name(&self) -> Option<&'static str> {
        GUI_WEB_BACKENDS
            .iter()
            .find(|(name, _)| *name == self.web_backend)
            .and_then(|(_, env)| *env)
    }

    pub fn buffer_mut(&mut self, field: SetupField) -> &mut TextBuffer {
        match field {
            SetupField::BaseUrl => &mut self.base_url,
            SetupField::Model => &mut self.model,
            SetupField::ApiKey => &mut self.api_key,
            SetupField::WebApiKey => &mut self.web_api_key,
        }
    }

    /// Initialize all 16 profile directories, starter files, `state.db`, `config.yaml`, and `.env`.
    pub fn save(&mut self, profile: &Path) -> Result<String, String> {
        let model = self.model.as_str().trim().to_owned();
        if model.is_empty() {
            return Err("Model name cannot be empty.".into());
        }
        let base_url = self.base_url.as_str().trim().to_owned();
        if (self.provider == "custom" || self.provider == "ollama") && base_url.is_empty() {
            return Err(format!("Provider '{}' requires a Base URL.", self.provider));
        }

        let scaffold = ensure_profile_scaffold(profile)?;
        set_config_value(profile, "model.provider", &self.provider)?;
        set_config_value(profile, "model.default", &model)?;
        if !base_url.is_empty() && (self.provider == "custom" || self.provider == "ollama") {
            set_config_value(profile, "model.base_url", &base_url)?;
        }
        if self.provider == "custom" {
            set_config_value(profile, "model.api_mode", &self.api_mode)?;
        }
        let mem = if self.memory_enabled { "true" } else { "false" };
        set_config_value(profile, "memory.memory_enabled", mem)?;
        set_config_value(profile, "memory.user_profile_enabled", mem)?;

        if self.web_backend != "none" {
            set_config_value(profile, "web.search_backend", &self.web_backend)?;
        }

        let key_raw = self.api_key.as_str().trim().to_owned();
        if !key_raw.is_empty() {
            if let Some(var) = self.key_env_name() {
                let target_var =
                    if self.provider == "anthropic" && key_raw.starts_with("sk-ant-oat") {
                        "ANTHROPIC_TOKEN"
                    } else {
                        var
                    };
                set_env_value(profile, target_var, &key_raw)?;
                self.has_existing_api_key = true;
            }
            self.api_key.clear();
        }

        let web_key_raw = self.web_api_key.as_str().trim().to_owned();
        if !web_key_raw.is_empty() {
            if let Some(var) = self.web_env_name() {
                set_env_value(profile, var, &web_key_raw)?;
            }
            self.web_api_key.clear();
        }

        self.installed_skills_count = skills_sync::list_installed_skills(profile).len();
        let mut msg = format!("Saved {}:{} to config.yaml", self.provider, model);
        if !scaffold.created_dirs.is_empty() || !scaffold.created_files.is_empty() {
            msg.push_str(&format!(
                " (initialized {} dirs, {} starter files)",
                scaffold.created_dirs.len(),
                scaffold.created_files.len()
            ));
        }
        self.notice = Some((false, msg.clone()));
        Ok(msg)
    }
}

fn preset_for(id: &str) -> Option<&'static GuiProviderPreset> {
    GUI_PROVIDER_PRESETS.iter().find(|p| p.id == id)
}

fn profile_has_key_for(profile: &Path, provider: &str) -> bool {
    let Some(env_var) = preset_for(provider).and_then(|p| p.key_env) else {
        return false;
    };
    if std::env::var(env_var).is_ok_and(|v| !v.trim().is_empty()) {
        return true;
    }
    let Ok(raw) = fs::read_to_string(profile.join(".env")) else {
        return false;
    };
    let prefix = format!("{env_var}=");
    raw.lines().any(|line| {
        let stripped = line.trim_start();
        let body = stripped.strip_prefix("export ").unwrap_or(stripped);
        body.strip_prefix(&prefix)
            .is_some_and(|val| !val.trim().trim_matches('"').is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_runtime::config::PROFILE_DIRECTORIES;

    #[test]
    fn setup_draft_initializes_profile_saves_secrets_and_clears_buffer() {
        let dir = std::env::temp_dir().join(format!("hsr-gui-setup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut draft = SetupDraft::load_from_profile(&dir);
        assert!(draft.open, "fresh profile should auto-open Setup");

        draft.select_provider(&dir, "custom");
        draft.base_url.clear();
        draft.base_url.insert_str("http://127.0.0.1:9999/v1");
        draft.model.clear();
        draft.model.insert_str("local-test-model");
        draft.api_key.insert_str("sk-top-secret-123");
        draft.web_backend = "tavily".into();
        draft.web_api_key.insert_str("tvly-secret-456");

        let msg = draft.save(&dir).expect("save setup");
        assert!(msg.contains("custom:local-test-model"));
        assert!(!msg.contains("sk-top-secret-123"));
        assert!(!msg.contains("tvly-secret-456"));
        assert!(draft.api_key.is_empty(), "plaintext secret cleared");
        assert!(draft.web_api_key.is_empty());
        assert!(draft.has_existing_api_key);

        for &(rel, _) in PROFILE_DIRECTORIES {
            assert!(dir.join(rel).is_dir(), "missing dir {rel}");
        }
        assert!(dir.join("state.db").is_file());
        assert!(dir.join("cron/jobs.json").is_file());
        assert!(dir.join("memories/MEMORY.md").is_file());
        let env_raw = fs::read_to_string(dir.join(".env")).unwrap();
        assert!(env_raw.contains("HERMES_API_KEY=sk-top-secret-123"));
        assert!(env_raw.contains("TAVILY_API_KEY=tvly-secret-456"));

        let reloaded = SetupDraft::load_from_profile(&dir);
        assert!(
            !reloaded.open,
            "configured profile should not auto-open Setup"
        );
        assert_eq!(reloaded.provider, "custom");
        assert_eq!(reloaded.model.as_str(), "local-test-model");
        assert!(reloaded.has_existing_api_key);

        let _ = fs::remove_dir_all(&dir);
    }
}
