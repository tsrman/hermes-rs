//! Profile-scoped settings for the CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::browser_cdp::BrowserSettings;
use crate::mcp::McpServerConfig;
use crate::memory_tool::MemorySettings;
use crate::prompt::PromptSettings;
use crate::providers::profiles;
use crate::tool_policy::ToolPolicy;
use crate::web_tools::WebSettings;

pub use crate::compaction::{
    merge_summary_into_tail_row, plan_summary_placement, CompactionBlockReason, CompactionGuard,
    CompactionSettings, SummaryPlacement,
};

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

/// `security.redact_phone_numbers: false`, the only value that lets the assistant's reply keep its
/// phone numbers; any other value, or a missing key, keeps them masked, as the reference does.
/// The reference reads YAML 1.1, where `no`, `off` and `n` (in any of their usual cases) are the
/// boolean false; the parser here gives those words as strings, so they count when unquoted. A
/// quoted `'no'` cannot be told from them, and the reference would keep it masked.
pub fn keeps_phone_numbers(config: &serde_yaml::Value) -> bool {
    use serde_yaml::Value;
    match &config["security"]["redact_phone_numbers"] {
        Value::Bool(flag) => !flag,
        Value::String(word) => matches!(
            word.as_str(),
            "no" | "No" | "NO" | "off" | "Off" | "OFF" | "n" | "N"
        ),
        _ => false,
    }
}

/// Interactive console UI mode (`ui.mode` in `config.yaml` or `HERMES_UI`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UiModeSetting {
    #[default]
    Auto,
    Inline,
    Tui,
    Plain,
}

impl UiModeSetting {
    pub fn parse_lossy(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "inline" | "stream" | "repl" => Some(Self::Inline),
            "tui" | "fullscreen" | "full" => Some(Self::Tui),
            "plain" | "off" | "none" => Some(Self::Plain),
            _ => None,
        }
    }

    pub fn from_config(config: &serde_yaml::Value, secrets: &HashMap<String, String>) -> Self {
        let env_val = std::env::var("HERMES_UI")
            .ok()
            .or_else(|| secrets.get("HERMES_UI").cloned())
            .filter(|v| !v.trim().is_empty());
        if let Some(parsed) = env_val.as_deref().and_then(Self::parse_lossy) {
            return parsed;
        }
        config
            .get("ui")
            .and_then(|ui| ui.get("mode"))
            .and_then(serde_yaml::Value::as_str)
            .and_then(Self::parse_lossy)
            .unwrap_or(Self::Auto)
    }
}

pub struct ChatSettings {
    /// Interactive console UI mode (`ui.mode` / `HERMES_UI`).
    pub ui_mode: UiModeSetting,
    /// Whether OpenAI Chat Completions requests use SSE streaming (`display.streaming`, fallback
    /// `streaming.enabled`; default `true`).
    pub streaming: bool,
    /// `security.redact_phone_numbers: false`: the assistant's reply keeps its phone numbers.
    pub keep_phone_numbers: bool,
    /// `agent.stall_guards` (default `true`): identical-call loop notices/stubbing and
    /// continue-intent / degenerate-final re-prompts.
    pub stall_guards: bool,
    /// `agent.intent_ack_continuation` (default `"auto"`): governs degenerate-final re-prompting.
    pub intent_ack_continuation: crate::stall_guards::IntentAckContinuation,
    /// `agent.reasoning_effort`, `agent.reasoning_overrides` and `agent.text_verbosity`.
    pub reasoning: crate::reasoning::ReasoningSettings,
    pub model: Option<String>,
    pub provider: &'static str,
    /// Use the OpenAI Responses API instead of Chat Completions.
    pub responses: bool,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// `model.api_key` from config.yaml.
    pub config_api_key: Option<String>,
    /// `HERMES_REDACT_SECRETS` (environment or profile `.env`), else `security.redact_secrets`;
    /// on unless it says otherwise.
    pub redact_secrets: bool,
    /// `prefill_messages_file` (environment `HERMES_PREFILL_MESSAGES_FILE` first), as written.
    pub prefill_file: Option<String>,
    /// `model.provider` named an entry of `providers:`.
    pub named_custom: bool,
    /// `auxiliary.compression`: where the compression summary is made.
    pub compression_route: AuxiliaryRoute,
    /// `auxiliary.vision`: optional dedicated model route for `vision_analyze`.
    pub vision_route: AuxiliaryRoute,
    /// `delegation.model/provider/base_url/api_key/api_mode`: the route of subagents.
    pub delegation_route: AuxiliaryRoute,
    /// `model.default_headers`.
    pub default_headers: Vec<(String, String)>,
    /// `providers` / `custom_providers` entries that may carry an `extra_body`.
    pub custom_providers: Vec<CustomProvider>,
    /// `model_overrides`: context windows by provider and model.
    pub model_overrides: ModelOverrides,
    pub api_max_attempts: u32,
    pub max_turns: u32,
    pub fallbacks: Vec<FallbackSettings>,
    pub prompt: PromptSettings,
    pub compression: CompactionSettings,
    pub approval_timeout_secs: u32,
    pub terminal_local: bool,
    pub terminal_env: crate::terminal::TerminalSettings,
    pub approvals: crate::approval_policy::ApprovalSettings,
    pub delegation: crate::delegation::DelegationSettings,
    /// `clarify.timeout`, else `agent.clarify_timeout`, else 90; 0 or less waits without limit.
    pub clarify_timeout_secs: u32,
    pub mcp_servers: Vec<McpServerConfig>,
    /// `tool_budget.mcp_result_size_chars` (default 50,000, capped at 100,000): spillover threshold.
    pub mcp_result_size_chars: usize,
    pub web: WebSettings,
    pub browser: BrowserSettings,
    pub memory: MemorySettings,
    pub vision: crate::vision_tool::VisionSettings,
    pub image_gen: crate::image_gen::ImageGenSettings,
    pub tts: crate::tts_tool::TtsSettings,
    pub x_search: crate::x_search::XSearchSettings,
    pub tools: ToolPolicy,
    pub vertex: crate::providers::vertex::VertexSettings,
    pub bedrock: crate::providers::bedrock::BedrockSettings,
    openai_api_key: Option<String>,
    openai_base_url: Option<String>,
    /// The profile `.env` values, for the keys of named providers.
    secrets: HashMap<String, String>,
    anthropic_api_key: Option<String>,
    anthropic_base_url: Option<String>,
}

/// `auxiliary.<task>` as written: which model and endpoint a side task uses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuxiliaryRoute {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub key_env: Option<String>,
    pub api_mode: Option<String>,
    pub timeout_secs: Option<u64>,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

impl AuxiliaryRoute {
    /// Nothing is configured: the task runs on the main route.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn from_config(config: &serde_yaml::Value, task: &str) -> Self {
        Self::from_section(&config["auxiliary"][task])
    }

    fn from_section(section: &serde_yaml::Value) -> Self {
        let text = |field: &str, auto_is_unset: bool| {
            section
                .get(field)
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .filter(|value| !(auto_is_unset && value.eq_ignore_ascii_case("auto")))
                .map(str::to_owned)
        };
        Self {
            provider: text("provider", true),
            model: text("model", true),
            base_url: text("base_url", false),
            api_key: text("api_key", false),
            key_env: text("key_env", false).or_else(|| text("api_key_env", false)),
            api_mode: text("api_mode", false).map(|mode| canonical_api_mode(&mode)),
            timeout_secs: section
                .get("timeout")
                .and_then(serde_yaml::Value::as_f64)
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| seconds.ceil() as u64),
            extra_body: section
                .get("extra_body")
                .and_then(|body| serde_json::to_value(body).ok())
                .and_then(|body| match body {
                    serde_json::Value::Object(map) => Some(map),
                    _ => None,
                })
                .unwrap_or_default(),
        }
    }
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
            ui_mode: UiModeSetting::Auto,
            streaming: true,
            model: None,
            provider: "custom",
            responses: false,
            base_url: None,
            api_key: None,
            config_api_key: None,
            redact_secrets: true,
            keep_phone_numbers: false,
            stall_guards: true,
            intent_ack_continuation: crate::stall_guards::IntentAckContinuation::default(),
            reasoning: crate::reasoning::ReasoningSettings::default(),
            prefill_file: None,
            named_custom: false,
            compression_route: AuxiliaryRoute::default(),
            vision_route: AuxiliaryRoute::default(),
            delegation_route: AuxiliaryRoute::default(),
            default_headers: Vec::new(),
            custom_providers: Vec::new(),
            model_overrides: ModelOverrides::default(),
            api_max_attempts: 3,
            max_turns: 16,
            fallbacks: Vec::new(),
            prompt: PromptSettings::default(),
            compression: CompactionSettings::default(),
            approval_timeout_secs: 300,
            terminal_local: true,
            terminal_env: crate::terminal::TerminalSettings::default(),
            approvals: crate::approval_policy::ApprovalSettings::default(),
            delegation: crate::delegation::DelegationSettings::default(),
            clarify_timeout_secs: 90,
            mcp_servers: Vec::new(),
            mcp_result_size_chars: crate::mcp_media::DEFAULT_MCP_RESULT_SIZE_CHARS,
            web: WebSettings::default(),
            browser: BrowserSettings::default(),
            memory: MemorySettings::default(),
            vision: crate::vision_tool::VisionSettings::default(),
            image_gen: crate::image_gen::ImageGenSettings::default(),
            tts: crate::tts_tool::TtsSettings::default(),
            x_search: crate::x_search::XSearchSettings::default(),
            tools: ToolPolicy::allow_all(),
            vertex: crate::providers::vertex::VertexSettings::default(),
            bedrock: crate::providers::bedrock::BedrockSettings::default(),
            openai_api_key: None,
            openai_base_url: None,
            secrets: HashMap::new(),
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
        for var in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            if let Some(key) = scoped_key(
                var,
                "ANTHROPIC_BASE_URL",
                None,
                self.anthropic_base_url.as_ref(),
                &["anthropic.com"],
                base_url,
            ) {
                return Some(key);
            }
        }
        scoped_key(
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            self.anthropic_api_key.as_ref(),
            self.anthropic_base_url.as_ref(),
            &["anthropic.com"],
            base_url,
        )
    }

    pub fn nvidia_key_for(&self, base_url: &str) -> Option<String> {
        let profile_key = self.secrets.get(crate::providers::nvidia::NVIDIA_KEY_ENV);
        let profile_url = self
            .secrets
            .get(crate::providers::nvidia::NVIDIA_BASE_URL_ENV);
        scoped_key(
            crate::providers::nvidia::NVIDIA_KEY_ENV,
            crate::providers::nvidia::NVIDIA_BASE_URL_ENV,
            profile_key,
            profile_url,
            &["integrate.api.nvidia.com", "nvidia.com"],
            base_url,
        )
    }

    pub fn secrets(&self) -> &HashMap<String, String> {
        &self.secrets
    }

    /// An environment variable, else a profile `.env` value; empty values count as unset.
    pub fn secret(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .or_else(|| self.secrets.get(name).cloned())
            .filter(|value| !value.trim().is_empty())
    }

    /// The key of a named provider: only to its own host or to the endpoint override paired with it.
    fn profile_key_for(&self, profile: &profiles::Profile, base_url: &str) -> Option<String> {
        let lookup = |name: &str| {
            std::env::var(name)
                .ok()
                .or_else(|| self.secrets.get(name).cloned())
                .filter(|value| !value.trim().is_empty())
        };
        let key = profile.key_envs.iter().find_map(|name| lookup(name))?;
        let host = reqwest::Url::parse(&profile.base_url)
            .ok()?
            .host_str()?
            .to_owned();
        let target = reqwest::Url::parse(base_url).ok()?;
        let own_host = target.host_str().is_some_and(|candidate| {
            candidate == host
                || candidate
                    .strip_suffix(host.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.'))
        });
        let paired = profile
            .base_url_envs
            .iter()
            .find_map(|name| lookup(name))
            .is_some_and(|value| {
                value.trim().trim_end_matches('/') == base_url.trim().trim_end_matches('/')
            });
        (own_host || paired).then_some(key)
    }

    /// The vendor key allowed for `base_url`, chosen by provider name.
    /// Custom endpoints and named providers may run without a key (local servers): the reference
    /// sends the placeholder `no-key-required` there. Vendor providers need a real one.
    pub fn keyless_endpoint(provider: &str) -> bool {
        !matches!(
            provider,
            "openai" | "anthropic" | "nvidia" | "bedrock" | "vertex"
        ) && crate::providers::oauth::resolve_oauth_provider(provider).is_none()
            && profiles::lookup(provider).is_none()
    }

    pub fn vendor_key_for(&self, provider: &str, base_url: &str) -> Option<String> {
        match provider {
            "openai" => self.openai_key_for(base_url),
            "anthropic" => self.anthropic_key_for(base_url),
            "nvidia" => self.nvidia_key_for(base_url),
            other => match profiles::lookup(other) {
                Some(profile) => self.profile_key_for(profile, base_url),
                // Custom endpoints keep using an OpenAI key paired with their URL.
                None => self.openai_key_for(base_url),
            },
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

pub(crate) fn read_config(path: &Path) -> Result<serde_yaml::Value, String> {
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
    let mut config = config;
    if let Some(Ok(entries)) = path
        .parent()
        .map(|home| home.join(".env"))
        .filter(|env| env.exists())
        .map(|env| {
            dotenvy::from_path_iter(env).map(|iter| iter.flatten().collect::<HashMap<_, _>>())
        })
    {
        expand_config(&mut config, &entries);
    } else {
        expand_config(&mut config, &HashMap::new());
    }
    Ok(config)
}

pub fn load_chat_settings(home: Option<&Path>) -> Result<ChatSettings, String> {
    let terminal_env = std::env::var_os("TERMINAL_ENV");
    let Some(home) = home else {
        let ui_mode = UiModeSetting::from_config(&serde_yaml::Value::Null, &HashMap::new());
        return Ok(ChatSettings {
            ui_mode,
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
    settings.keep_phone_numbers = keeps_phone_numbers(&config);
    settings.stall_guards = crate::stall_guards::parse_stall_guards(&config);
    settings.intent_ack_continuation =
        crate::stall_guards::IntentAckContinuation::from_config(&config);
    settings.reasoning = crate::reasoning::ReasoningSettings::from_config(&config);
    settings.custom_providers = custom_providers(&config);
    settings.model_overrides = ModelOverrides::from_config(&config);
    let mut named_key_env = None;
    if let Some(model) = config.get("model") {
        if let Some(name) = model.as_str() {
            settings.model = Some(name.to_owned());
        } else if model.is_mapping() {
            let name = model.get("provider").and_then(serde_yaml::Value::as_str);
            let name = name.unwrap_or("");
            let resolved = resolve_provider(name).map(|found| (found, None));
            let Some(((provider, profile_responses), named)) = resolved.or_else(|| {
                // A key or name from `providers:`: the endpoint is the entry's.
                named_provider(&settings.custom_providers, name)
                    .map(|entry| (("custom", false), Some(entry.clone())))
            }) else {
                return Err(format!("provider '{name}' is not supported"));
            };
            if let Some(entry) = named.as_ref().filter(|entry| !entry.enabled) {
                return Err(format!(
                    "provider '{}' is disabled in config (providers.{}.enabled: false)",
                    name, entry.key
                ));
            }
            settings.provider = provider;
            settings.named_custom = named.is_some();
            settings.default_headers = model
                .get("default_headers")
                .and_then(serde_yaml::Value::as_mapping)
                .map(|headers| {
                    headers
                        .iter()
                        .filter_map(|(name, value)| Some((scalar_text(name)?, scalar_text(value)?)))
                        .collect()
                })
                .unwrap_or_default();
            let configured_mode = named
                .as_ref()
                .and_then(|entry| entry.api_mode.as_deref())
                .or_else(|| model.get("api_mode").and_then(serde_yaml::Value::as_str));
            settings.responses = provider != "anthropic"
                && match configured_mode {
                    Some("codex_responses") => true,
                    Some("chat_completions") => false,
                    _ => profile_responses,
                };
            if configured_mode == Some("anthropic_messages") && named.is_some() {
                settings.provider = "anthropic";
            }
            let text = |field: &str| {
                model
                    .get(field)
                    .and_then(serde_yaml::Value::as_str)
                    .map(str::to_owned)
            };
            settings.model = text("default")
                .or_else(|| named.as_ref().map(|entry| entry.model.clone()))
                .filter(|model| !model.is_empty());
            // The entry's endpoint and key win over `model.base_url` and `model.api_key`.
            settings.config_api_key = named
                .as_ref()
                .and_then(|entry| entry.api_key.clone())
                .or_else(|| text("api_key"))
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty());
            named_key_env = named.as_ref().and_then(|entry| entry.key_env.clone());
            settings.base_url = named
                .as_ref()
                .map(|entry| entry.base_url.clone())
                .or_else(|| text("base_url"));
            if settings.base_url.as_deref().is_none_or(str::is_empty) {
                settings.base_url = default_base_url(provider).map(str::to_owned);
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
    settings.secrets = secrets.clone();
    settings.ui_mode = UiModeSetting::from_config(&config, &secrets);
    settings.streaming = config
        .get("display")
        .and_then(|d| d.get("streaming"))
        .filter(|v| !v.is_null())
        .or_else(|| {
            config
                .get("streaming")
                .and_then(|s| s.get("enabled"))
                .filter(|v| !v.is_null())
        })
        .is_none_or(|v| truthy(v, true));
    let ephemeral_env = std::env::var("HERMES_EPHEMERAL_SYSTEM_PROMPT")
        .ok()
        .or_else(|| secrets.get("HERMES_EPHEMERAL_SYSTEM_PROMPT").cloned());
    settings.prompt = PromptSettings::from_config_with_env(&config, ephemeral_env.as_deref());
    let policy = std::env::var("HERMES_REDACT_SECRETS")
        .ok()
        .or_else(|| secrets.get("HERMES_REDACT_SECRETS").cloned())
        .or_else(|| match &config["security"]["redact_secrets"] {
            serde_yaml::Value::Null => None,
            serde_yaml::Value::Bool(flag) => Some(flag.to_string()),
            serde_yaml::Value::String(text) => Some(text.clone()),
            serde_yaml::Value::Number(number) => Some(number.to_string()),
            _ => Some(String::new()),
        });
    if let Some(raw) = policy {
        settings.redact_secrets = matches!(
            raw.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    settings.vertex = crate::providers::vertex::VertexSettings::from_config(&config);
    settings.bedrock = crate::providers::bedrock::BedrockSettings::from_config(&config);
    // A named provider's endpoint may be overridden from the environment or the profile `.env`.
    let configured_base_url = config["model"]
        .get("base_url")
        .and_then(serde_yaml::Value::as_str)
        .is_some_and(|url| !url.is_empty());
    if !configured_base_url {
        if settings.provider == "nvidia" {
            let overridden = std::env::var(crate::providers::nvidia::NVIDIA_BASE_URL_ENV)
                .ok()
                .or_else(|| {
                    secrets
                        .get(crate::providers::nvidia::NVIDIA_BASE_URL_ENV)
                        .cloned()
                })
                .filter(|value| !value.trim().is_empty());
            if overridden.is_some() {
                settings.base_url = overridden;
            }
        } else if settings.provider == "bedrock" {
            let overridden = std::env::var("BEDROCK_BASE_URL")
                .ok()
                .or_else(|| secrets.get("BEDROCK_BASE_URL").cloned())
                .filter(|value| !value.trim().is_empty());
            if overridden.is_some() {
                settings.base_url = overridden;
            } else {
                let region =
                    crate::providers::bedrock::resolve_bedrock_region(&settings.bedrock, &secrets);
                settings.base_url =
                    Some(crate::providers::bedrock::bedrock_runtime_base_url(&region));
            }
        } else if let Some(profile) = profiles::lookup(settings.provider) {
            let overridden = profile.base_url_envs.iter().find_map(|name| {
                std::env::var(name)
                    .ok()
                    .or_else(|| secrets.get(name).cloned())
                    .filter(|value| !value.trim().is_empty())
            });
            if overridden.is_some() {
                settings.base_url = overridden;
            }
        }
    }
    if settings.config_api_key.is_none() {
        settings.config_api_key = named_key_env
            .and_then(|name| {
                std::env::var(&name)
                    .ok()
                    .or_else(|| secrets.get(&name).cloned())
            })
            .filter(|key| !key.trim().is_empty());
    }
    settings.api_key = secrets.get("HERMES_API_KEY").cloned();
    settings.openai_api_key = secrets.get("OPENAI_API_KEY").cloned();
    settings.openai_base_url = secrets.get("OPENAI_BASE_URL").cloned();
    settings.anthropic_api_key = secrets
        .get("ANTHROPIC_API_KEY")
        .or_else(|| secrets.get("ANTHROPIC_TOKEN"))
        .or_else(|| secrets.get("CLAUDE_CODE_OAUTH_TOKEN"))
        .cloned();
    settings.anthropic_base_url = secrets.get("ANTHROPIC_BASE_URL").cloned();
    settings.prefill_file = [
        std::env::var("HERMES_PREFILL_MESSAGES_FILE").unwrap_or_default(),
        config["prefill_messages_file"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        config["agent"]["prefill_messages_file"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
    ]
    .into_iter()
    .map(|path| path.trim().to_owned())
    .find(|path| !path.is_empty());
    settings.compression_route = AuxiliaryRoute::from_config(&config, "compression");
    settings.vision_route = AuxiliaryRoute::from_config(&config, "vision");
    settings.delegation_route = AuxiliaryRoute {
        // A subagent route has no timeout or body fields of its own.
        timeout_secs: None,
        extra_body: serde_json::Map::new(),
        key_env: None,
        ..AuxiliaryRoute::from_section(&config["delegation"])
    };
    settings.mcp_servers = mcp_servers(&config, &secrets, Some(home));
    settings.mcp_result_size_chars = crate::mcp_media::parse_mcp_result_size_chars(&config);
    settings.web = WebSettings::from_config(&config, &secrets, Some(home));
    settings.browser = BrowserSettings::from_config(&config, &secrets);
    settings.memory = MemorySettings::from_config(&config);
    settings.vision = crate::vision_tool::VisionSettings::from_config(
        &config,
        settings.web.website_policy.clone(),
        settings.web.allow_private_urls,
    );
    settings.image_gen = crate::image_gen::ImageGenSettings::from_config(
        &config,
        &secrets,
        settings.web.allow_private_urls,
    );
    settings.tts = crate::tts_tool::TtsSettings::from_config(&config, &secrets);
    settings.x_search =
        crate::x_search::XSearchSettings::from_config(&config, &secrets, Some(home));
    settings.tools = ToolPolicy::from_config(&config);
    settings.clarify_timeout_secs = clarify_timeout(&config);
    settings.prompt.memory_tool = settings.tools.allows("memory");
    settings.terminal_env = crate::terminal::TerminalSettings::from_config(&config);
    settings.approvals =
        crate::approval_policy::ApprovalSettings::from_config(&config).with_profile_dir(home);
    settings.delegation = crate::delegation::DelegationSettings::from_config(&config, &secrets);
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
    let (provider, profile_responses) = resolve_provider(field("provider")?)?;
    // An empty name would mean `custom` in the primary model; a fallback must name its provider.
    let responses = match (provider, field("api_mode")) {
        ("anthropic", None | Some("anthropic_messages")) => false,
        ("anthropic", _) => return None,
        (_, None) => profile_responses,
        (_, Some("chat_completions")) => false,
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

pub fn default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("https://api.openai.com/v1"),
        "anthropic" => Some("https://api.anthropic.com/v1"),
        "nvidia" => Some(crate::providers::nvidia::NVIDIA_BASE_URL),
        "bedrock" => Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        other => crate::providers::oauth::default_oauth_base_url(other)
            .or_else(|| profiles::lookup(other).map(|profile| profile.base_url.as_str())),
    }
}

/// The canonical provider name and whether it speaks the Responses API by default.
pub fn resolve_provider(name: &str) -> Option<(&'static str, bool)> {
    let trimmed = name.trim();
    match trimmed {
        "" | "auto" | "custom" => Some(("custom", false)),
        "openai" => Some(("openai", false)),
        "anthropic" => Some(("anthropic", false)),
        other if crate::providers::nvidia::is_nvidia_provider(other) => Some(("nvidia", false)),
        other => {
            let norm = other.to_ascii_lowercase().replace('_', "-");
            if matches!(
                norm.as_str(),
                "bedrock" | "aws" | "aws-bedrock" | "amazon-bedrock" | "amazon"
            ) {
                return Some(("bedrock", false));
            }
            if matches!(norm.as_str(), "vertex" | "vertex-ai" | "google-vertex") {
                return Some(("vertex", false));
            }
            crate::providers::oauth::resolve_oauth_provider(other).or_else(|| {
                profiles::lookup(other)
                    .map(|profile| (profile.name.as_str(), profile.uses_responses()))
            })
        }
    }
}

/// `timeouts.mcp.tool_call`, else 300 (`_resolve_tool_timeout` and `resolve_timeout`): a number or a
/// numeric string, not a boolean; zero, a negative number and anything else invalid fall through to
/// the default; the longest value is one year.
fn mcp_tool_call_timeout(config: &serde_yaml::Value) -> f64 {
    let raw = config
        .get("timeouts")
        .and_then(|timeouts| timeouts.get("mcp"))
        .and_then(|mcp| mcp.get("tool_call"));
    let value = match raw {
        Some(serde_yaml::Value::Number(number)) => number.as_f64(),
        Some(serde_yaml::Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    value
        .filter(|value| !value.is_nan() && *value > 0.0)
        .map_or(crate::mcp::DEFAULT_TOOL_TIMEOUT_SECS, |value| {
            value.min(crate::mcp::MAX_TIMEOUT_SECS)
        })
}

/// Servers from `mcp_servers`: stdio (`command`) or Streamable HTTP (`url`).
fn mcp_servers(
    config: &serde_yaml::Value,
    secrets: &HashMap<String, String>,
    home: Option<&Path>,
) -> Vec<McpServerConfig> {
    let Some(entries) = config
        .get("mcp_servers")
        .and_then(serde_yaml::Value::as_mapping)
    else {
        return Vec::new();
    };
    let expand = |text: &str| expand_env(text, secrets);
    let default_tool_timeout = mcp_tool_call_timeout(config);
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
            let url = entry
                .get("url")
                .and_then(serde_yaml::Value::as_str)
                .map(&expand)
                .filter(|url| !url.is_empty());
            let command = match (
                &url,
                entry.get("command").and_then(serde_yaml::Value::as_str),
            ) {
                (_, Some(command)) => expand(command),
                (Some(_), None) => String::new(),
                (None, None) => return None,
            };
            let mut headers: std::collections::BTreeMap<String, String> = entry
                .get("headers")
                .and_then(serde_yaml::Value::as_mapping)
                .map(|headers| {
                    headers
                        .iter()
                        .filter_map(|(key, value)| {
                            Some((key.as_str()?.to_owned(), expand(value.as_str()?)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if url.is_some()
                && !headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("authorization"))
            {
                if let Some(token) =
                    home.and_then(|h| crate::providers::oauth::resolve_mcp_oauth_token(h, &name))
                {
                    headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
                }
            }
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
            let tool_timeout = entry
                .get("timeout")
                .and_then(serde_yaml::Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .map_or(default_tool_timeout, |value| {
                    value.min(crate::mcp::MAX_TIMEOUT_SECS)
                });
            let seconds = |key, default| {
                entry
                    .get(key)
                    .and_then(serde_yaml::Value::as_u64)
                    .filter(|value| *value > 0)
                    .unwrap_or(default)
            };
            let transport = entry
                .get("transport")
                .and_then(serde_yaml::Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty());
            Some(McpServerConfig {
                name,
                command,
                args,
                env,
                url,
                headers,
                transport,
                timeout_secs: tool_timeout,
                connect_timeout_secs: seconds("connect_timeout", 60),
                // `tools.resources: false` hides the resource tools of a server that offers them.
                resources: truthy(
                    entry
                        .get("tools")
                        .and_then(|tools| tools.get("resources"))
                        .unwrap_or(&serde_yaml::Value::Null),
                    true,
                ),
                // `tools.prompts: false` hides the prompt tools of a server that offers them.
                prompts: truthy(
                    entry
                        .get("tools")
                        .and_then(|tools| tools.get("prompts"))
                        .unwrap_or(&serde_yaml::Value::Null),
                    true,
                ),
            })
        })
        .collect()
}

/// Replace `${NAME}` and `${env:NAME}` from the profile `.env` or the process environment.
/// Unknown names, other secret sources (`${vault:X}`) and an empty name stay as written, so a
/// missing secret is visible in the failure.
fn expand_env(text: &str, secrets: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = after[..end].trim();
        let name = match inner.strip_prefix("env:") {
            Some(name) => name.trim(),
            None if is_other_source(inner) => "",
            None => inner,
        };
        let value = (!name.is_empty())
            .then(|| {
                secrets
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
            })
            .flatten();
        match value {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + 3 + end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `bitwarden:FOO`, `vault:path`: a source other than the environment.
fn is_other_source(reference: &str) -> bool {
    reference.split_once(':').is_some_and(|(source, _)| {
        source.starts_with(|c: char| c.is_ascii_lowercase())
            && source
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    })
}

/// Expand references in every string value of the config; keys are left alone.
fn expand_config(value: &mut serde_yaml::Value, secrets: &HashMap<String, String>) {
    match value {
        serde_yaml::Value::String(text) if text.contains("${") => {
            *text = expand_env(text, secrets);
        }
        serde_yaml::Value::Sequence(items) => {
            items
                .iter_mut()
                .for_each(|item| expand_config(item, secrets));
        }
        serde_yaml::Value::Mapping(map) => {
            map.iter_mut()
                .for_each(|(_, item)| expand_config(item, secrets));
        }
        serde_yaml::Value::Tagged(tagged) => expand_config(&mut tagged.value, secrets),
        _ => {}
    }
}

/// The tool policy of a platform for the profile at `home` (all tools without a profile).
pub fn load_tool_policy(home: Option<&Path>, platform: &str) -> Result<ToolPolicy, String> {
    let Some(home) = home else {
        return Ok(ToolPolicy::allow_all());
    };
    Ok(ToolPolicy::for_platform(
        &read_config(&home.join("config.yaml"))?,
        platform,
    ))
}

/// Seconds the CLI waits for an answer to a question; 0 means no limit.
fn clarify_timeout(config: &serde_yaml::Value) -> u32 {
    let raw = config
        .get("clarify")
        .and_then(|clarify| clarify.get("timeout"))
        .filter(|value| !value.is_null())
        .or_else(|| {
            config
                .get("agent")
                .and_then(|agent| agent.get("clarify_timeout"))
        });
    let seconds = match raw {
        None => 90,
        Some(value) => value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .unwrap_or(90),
    };
    u32::try_from(seconds.max(0)).unwrap_or(90)
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn parses_stdio_servers_and_expands_profile_variables() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "mcp_servers:\n  a:\n    command: run\n    args: ['--key=${TOKEN}', '${UNSET_VAR_X}']\n    env: {K: '${TOKEN}', N: 5}\n    timeout: 7\n  off:\n    command: x\n    enabled: false\n  remote:\n    url: https://example.test/mcp\n    headers: {Authorization: 'Bearer ${TOKEN}', N: 1}\n  empty: {}\n",
        )
        .unwrap();
        let secrets = HashMap::from([("TOKEN".to_owned(), "t0".to_owned())]);
        let servers = mcp_servers(&config, &secrets, None);
        assert_eq!(servers.len(), 2);
        let remote = &servers[1];
        assert_eq!(remote.url.as_deref(), Some("https://example.test/mcp"));
        assert_eq!(remote.headers["Authorization"], "Bearer t0");
        assert!(remote.command.is_empty() && !remote.headers.contains_key("N"));
        let server = &servers[0];
        assert_eq!(server.name, "a");
        assert_eq!(server.args, ["--key=t0", "${UNSET_VAR_X}"]);
        assert_eq!(server.env["K"], "t0");
        assert_eq!(server.env["N"], "5");
        assert_eq!(
            (server.timeout_secs, server.connect_timeout_secs),
            (7.0, 60)
        );
    }
}

#[cfg(test)]
mod clarify_timeout_tests {
    use super::*;

    #[test]
    fn the_clarify_timeout_prefers_the_legacy_key_then_the_agent_key() {
        let timeout = |yaml: &str| clarify_timeout(&serde_yaml::from_str(yaml).unwrap());
        assert_eq!(timeout("{}"), 90);
        assert_eq!(timeout("agent: {clarify_timeout: 30}"), 30);
        assert_eq!(
            timeout("clarify: {timeout: 5}\nagent: {clarify_timeout: 30}"),
            5
        );
        assert_eq!(timeout("agent: {clarify_timeout: 0}"), 0);
        assert_eq!(timeout("agent: {clarify_timeout: -4}"), 0);
        assert_eq!(timeout("agent: {clarify_timeout: 'soon'}"), 90);
        assert_eq!(timeout("agent: {clarify_timeout: '45'}"), 45);
    }
}

/// One endpoint declared under `providers:` (or the legacy `custom_providers:` list).
#[derive(Clone, Debug, PartialEq)]
pub struct CustomProvider {
    /// Names this entry answers to as `model.provider`: its key and its `name`, normalized.
    ids: Vec<String>,
    key: String,
    enabled: bool,
    base_url: String,
    model: String,
    catalog: Vec<String>,
    context_length: Option<usize>,
    model_context: Vec<(String, usize)>,
    api_key: Option<String>,
    key_env: Option<String>,
    api_mode: Option<String>,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    extra_headers: Vec<(String, String)>,
}

/// How the reference compares provider names: lower case, spaces as hyphens.
fn provider_id(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

fn canonical_api_mode(mode: &str) -> String {
    match mode.trim().to_lowercase().as_str() {
        "openai" | "openai_chat" | "openai-chat" | "chat-completions" | "chatcompletions" => {
            "chat_completions".into()
        }
        "responses" | "openai_responses" | "openai-responses" => "codex_responses".into(),
        "anthropic" | "anthropic-messages" | "messages" => "anthropic_messages".into(),
        _ => mode.trim().to_owned(),
    }
}

fn custom_providers(config: &serde_yaml::Value) -> Vec<CustomProvider> {
    let legacy = match config.get("custom_providers") {
        Some(serde_yaml::Value::Sequence(entries)) => entries.iter().collect(),
        _ => Vec::new(),
    };
    let current = match config.get("providers") {
        Some(serde_yaml::Value::Mapping(entries)) => entries
            .iter()
            .map(|(key, entry)| (key.as_str().map(str::to_owned), entry))
            .collect(),
        _ => Vec::new(),
    };
    let entries = legacy.into_iter().map(|entry| (None, entry)).chain(current);
    entries
        .filter_map(|(key, entry)| custom_provider(key.as_deref(), entry))
        .collect()
}

/// camelCase spellings the reference maps onto the snake_case keys of a provider entry when the
/// snake_case one is absent (`providers.<name>.baseUrl` and friends).
const CAMEL_ALIASES: [(&str, &str); 9] = [
    ("apiKey", "api_key"),
    ("baseUrl", "base_url"),
    ("apiMode", "api_mode"),
    ("keyEnv", "key_env"),
    ("apiKeyEnv", "key_env"),
    ("defaultModel", "default_model"),
    ("contextLength", "context_length"),
    ("rateLimitDelay", "rate_limit_delay"),
    ("sessionAffinityHeader", "session_affinity_header"),
];

fn with_snake_case_keys(entry: &serde_yaml::Value) -> serde_yaml::Value {
    let mut entry = entry.clone();
    if let Some(map) = entry.as_mapping_mut() {
        for (camel, snake) in CAMEL_ALIASES {
            if map.contains_key(camel) && !map.contains_key(snake) {
                if let Some(value) = map.get(camel).cloned() {
                    map.insert(snake.into(), value);
                }
            }
        }
    }
    entry
}

/// `is_provider_enabled`: absent means enabled; a string is enabled unless it is one of the false
/// words; anything else follows Python truthiness (so `0` and an empty value disable the entry).
fn provider_enabled(flag: Option<&serde_yaml::Value>) -> bool {
    use serde_yaml::Value;
    match flag {
        None => true,
        Some(Value::Bool(on)) => *on,
        Some(Value::String(word)) => !matches!(
            word.trim().to_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        ),
        Some(Value::Null) => false,
        Some(Value::Number(number)) => number.as_f64().is_none_or(|n| n != 0.0),
        Some(Value::Sequence(items)) => !items.is_empty(),
        Some(Value::Mapping(map)) => !map.is_empty(),
        Some(Value::Tagged(_)) => true,
    }
}

fn custom_provider(key: Option<&str>, entry: &serde_yaml::Value) -> Option<CustomProvider> {
    let entry = &with_snake_case_keys(entry);
    let text = |names: &[&str]| {
        names.iter().find_map(|name| {
            entry
                .get(*name)
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    };
    let base_url = text(&["base_url", "url", "api"])?;
    // A provider needs a name, from the entry or its key.
    let key = key.map(str::trim).filter(|key| !key.is_empty());
    let name = text(&["name"]).or(key)?;
    let enabled = provider_enabled(entry.get("enabled"));
    let extra_body = entry
        .get("extra_body")
        .and_then(|body| serde_json::to_value(body).ok())
        .and_then(|body| match body {
            serde_json::Value::Object(map) if !map.is_empty() => Some(map),
            _ => None,
        });
    let catalog = match entry.get("models") {
        Some(serde_yaml::Value::Mapping(models)) => models
            .keys()
            .filter_map(|name| name.as_str())
            .map(|name| name.trim().to_lowercase())
            .collect(),
        Some(serde_yaml::Value::Sequence(models)) => models
            .iter()
            .filter_map(|name| name.as_str())
            .map(|name| name.trim().to_lowercase())
            .collect(),
        _ => Vec::new(),
    };
    let mut ids = vec![provider_id(name)];
    if let Some(key) = key {
        ids.push(provider_id(key));
    }
    Some(CustomProvider {
        ids,
        key: key.unwrap_or(name).to_owned(),
        enabled,
        base_url: base_url.trim_end_matches('/').to_owned(),
        model: text(&["model", "default_model"]).unwrap_or("").to_owned(),
        catalog,
        context_length: entry.get("context_length").and_then(positive),
        model_context: match entry.get("models") {
            Some(serde_yaml::Value::Mapping(models)) => models
                .iter()
                .filter_map(|(name, settings)| {
                    Some((
                        name.as_str()?.trim().to_owned(),
                        positive(settings.get("context_length")?)?,
                    ))
                })
                .collect(),
            _ => Vec::new(),
        },
        api_key: text(&["api_key"]).map(str::to_owned),
        key_env: text(&["key_env", "api_key_env"]).map(str::to_owned),
        api_mode: text(&["api_mode", "transport"]).map(canonical_api_mode),
        extra_body,
        extra_headers: entry
            .get("extra_headers")
            .and_then(serde_yaml::Value::as_mapping)
            .map(|headers| {
                headers
                    .iter()
                    .filter_map(|(name, value)| {
                        let name = scalar_text(name)?;
                        Some((name, scalar_text(value)?))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A scalar as the reference's `str()` shows it; null and collections are dropped.
fn scalar_text(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(text) => Some(text.clone()),
        serde_yaml::Value::Bool(flag) => Some(if *flag { "True" } else { "False" }.into()),
        serde_yaml::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The entry a `model.provider` of `name` or `custom:name` selects.
fn named_provider<'a>(entries: &'a [CustomProvider], provider: &str) -> Option<&'a CustomProvider> {
    let id = provider_id(provider);
    let id = id.strip_prefix("custom:").unwrap_or(&id);
    entries
        .iter()
        .find(|entry| entry.ids.iter().any(|known| known == id))
}

fn positive(value: &serde_yaml::Value) -> Option<usize> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|window| *window > 0)
        .and_then(|window| usize::try_from(window).ok())
}

/// `model_overrides:` as far as the port uses it: `context_window` per provider and model, the
/// `_default` entries of a provider and the global one.
#[derive(Clone, Debug, Default)]
pub struct ModelOverrides {
    root: serde_yaml::Mapping,
}

impl ModelOverrides {
    fn from_config(config: &serde_yaml::Value) -> Self {
        Self {
            root: config
                .get("model_overrides")
                .and_then(serde_yaml::Value::as_mapping)
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn section(&self, provider: &str) -> Option<&serde_yaml::Mapping> {
        self.root.get(provider.trim())?.as_mapping()
    }

    /// The window an entry of exactly this provider and model sets (exact name first, then
    /// case-insensitively, never `_default`).
    pub fn explicit_window(&self, provider: &str, model: &str) -> Option<usize> {
        let model = model.trim();
        let section = self.section(provider)?;
        if model.is_empty() {
            return None;
        }
        let entry = section
            .get(model)
            .filter(|entry| entry.is_mapping())
            .or_else(|| {
                section.iter().find_map(|(name, entry)| {
                    let name = name.as_str()?;
                    (name != "_default"
                        && name.to_lowercase() == model.to_lowercase()
                        && entry.is_mapping())
                    .then_some(entry)
                })
            })?;
        window(entry)
    }

    /// The fill-gap window: the provider's `_default`, else the global one. The reference applies
    /// it only when its model catalog misses, which the port, having no catalog, always does.
    pub fn default_window(&self, provider: &str) -> Option<usize> {
        self.section(provider)
            .and_then(|section| section.get("_default"))
            .filter(|entry| entry.is_mapping())
            .or_else(|| self.root.get("_default").filter(|entry| entry.is_mapping()))
            .and_then(window)
    }
}

/// `int(entry["context_window"])` when positive.
fn window(entry: &serde_yaml::Value) -> Option<usize> {
    let raw = entry.get("context_window")?;
    let number = match raw {
        serde_yaml::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64))?,
        serde_yaml::Value::String(text) => text.trim().parse::<i64>().ok()?,
        serde_yaml::Value::Bool(flag) => i64::from(*flag),
        _ => return None,
    };
    usize::try_from(number).ok().filter(|value| *value > 0)
}

/// The context window an entry for `base_url` declares for `model`: a per-model value first,
/// then the entry's own.
pub fn custom_context_length(
    entries: &[CustomProvider],
    base_url: &str,
    model: &str,
) -> Option<usize> {
    let target = base_url.trim().trim_end_matches('/');
    let model = model.trim();
    let matching = || {
        entries
            .iter()
            .filter(move |entry| entry.enabled && entry.base_url == target)
    };
    matching()
        .find_map(|entry| {
            entry
                .model_context
                .iter()
                .find_map(|(name, window)| (*name == model).then_some(*window))
        })
        .or_else(|| matching().find_map(|entry| entry.context_length))
}

/// `extra_headers` of the first enabled entry for `base_url` that declares any.
pub fn custom_extra_headers(entries: &[CustomProvider], base_url: &str) -> Vec<(String, String)> {
    let target = base_url.trim().trim_end_matches('/');
    entries
        .iter()
        .filter(|entry| entry.enabled && entry.base_url == target)
        .find(|entry| !entry.extra_headers.is_empty())
        .map(|entry| entry.extra_headers.clone())
        .unwrap_or_default()
}

/// The `extra_body` for the `custom` provider at `base_url` running `model`: the first entry
/// that names this model (or lists it in its catalog), else the first entry without a model.
pub fn custom_extra_body(
    entries: &[CustomProvider],
    base_url: &str,
    model: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let target = base_url.trim().trim_end_matches('/');
    let model = model.trim().to_lowercase();
    let mut fallback = None;
    for entry in entries.iter().filter(|entry| entry.base_url == target) {
        let Some(extra_body) = entry.extra_body.as_ref().filter(|_| entry.enabled) else {
            continue;
        };
        if entry.model.is_empty() {
            fallback.get_or_insert(extra_body);
        } else if entry.catalog.contains(&model) || entry.model.to_lowercase() == model {
            return Some(extra_body.clone());
        }
    }
    fallback.cloned()
}

#[cfg(test)]
mod expansion_tests {
    use super::*;

    #[test]
    fn references_expand_like_the_reference() {
        let secrets = HashMap::from([("KEY".to_owned(), "v".to_owned())]);
        let cases = [
            ("a-${KEY}-b", "a-v-b"),
            ("${env:KEY}", "v"),
            ("${ env:KEY }", "v"),
            ("${ KEY }", "v"),
            ("${NOPE_X}", "${NOPE_X}"),
            ("${env:NOPE_X}", "${env:NOPE_X}"),
            ("${vault:KEY}", "${vault:KEY}"),
            ("${env:}", "${env:}"),
            ("${}", "${}"),
            ("${KEY", "${KEY"),
            ("$KEY", "$KEY"),
        ];
        for (input, expected) in cases {
            assert_eq!(expand_env(input, &secrets), expected, "{input}");
        }
        let mut config: serde_yaml::Value =
            serde_yaml::from_str("${KEY}: ${KEY}\nlist: ['${KEY}', 3]\n").unwrap();
        expand_config(&mut config, &secrets);
        assert_eq!(config["${KEY}"], "v");
        assert_eq!(config["list"][0], "v");
        assert_eq!(config["list"][1], 3);
    }
}

#[cfg(test)]
mod custom_provider_tests {
    use super::*;

    fn entries(yaml: &str) -> Vec<CustomProvider> {
        custom_providers(&serde_yaml::from_str(yaml).unwrap())
    }

    #[test]
    fn provider_entries_accept_camel_case_keys_and_python_truthiness_for_enabled() {
        let parse = |text: &str| -> Option<CustomProvider> {
            let entry: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
            custom_provider(Some("p"), &entry)
        };
        let camel = parse(
            "{baseUrl: 'https://x/v1', apiKey: k, apiMode: codex_responses, defaultModel: m, keyEnv: E}",
        )
        .unwrap();
        assert_eq!(camel.base_url, "https://x/v1");
        assert_eq!(camel.api_key.as_deref(), Some("k"));
        assert_eq!(camel.model, "m");
        assert_eq!(camel.key_env.as_deref(), Some("E"));
        assert!(camel.api_mode.is_some());
        // The snake_case key wins when both are present.
        let both = parse("{base_url: 'https://a/v1', baseUrl: 'https://b/v1'}").unwrap();
        assert_eq!(both.base_url, "https://a/v1");
        for (flag, want) in [
            ("true", true),
            ("false", false),
            ("'false'", false),
            ("'No'", false),
            ("'off'", false),
            ("'0'", false),
            ("'yes'", true),
            ("0", false),
            ("1", true),
            ("null", false),
            ("[]", false),
            ("[a]", true),
        ] {
            let entry = parse(&format!("{{base_url: 'https://x/v1', enabled: {flag}}}")).unwrap();
            assert_eq!(entry.enabled, want, "enabled: {flag}");
        }
        assert!(parse("{base_url: 'https://x/v1'}").unwrap().enabled);
    }

    #[test]
    fn only_custom_endpoints_may_run_without_a_key() {
        for custom in ["custom", "llm-api", "local"] {
            assert!(ChatSettings::keyless_endpoint(custom), "{custom}");
        }
        for vendor in ["openai", "anthropic", "novita"] {
            assert!(!ChatSettings::keyless_endpoint(vendor), "{vendor}");
        }
    }

    #[test]
    fn extra_body_follows_url_and_model() {
        let providers = entries(
            "providers:\n  a:\n    base_url: https://x/v1/\n    model: Qwen\n    extra_body: {k: 1}\n  b:\n    base_url: https://x/v1\n    extra_body: {k: 2}\n  off:\n    enabled: false\n    base_url: https://y/v1\n    extra_body: {k: 3}\n",
        );
        assert_eq!(providers.len(), 3);
        let k = |url: &str, model: &str| {
            custom_extra_body(&providers, url, model).map(|body| body["k"].clone())
        };
        assert_eq!(k("https://x/v1", "qwen"), Some(1.into()));
        assert_eq!(k("https://x/v1/", "other"), Some(2.into()));
        assert_eq!(k("https://y/v1", "m"), None);
        assert_eq!(k("https://z/v1", "m"), None);
    }
}

#[cfg(test)]
mod named_provider_tests {
    use super::*;

    /// Cases recorded from the reference's `resolve_runtime_provider`.
    #[test]
    fn named_providers_resolve_like_the_reference() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/named_providers.json")).unwrap();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let home = tempfile::tempdir().unwrap();
            std::fs::write(
                home.path().join("config.yaml"),
                case["config"].as_str().unwrap(),
            )
            .unwrap();
            std::fs::write(home.path().join(".env"), "MYK=from-env\n").unwrap();
            let expected = &case["expected"];
            let loaded = load_chat_settings(Some(home.path()));
            if expected.get("error").is_some() {
                assert!(loaded.is_err(), "{name}: the reference refuses this config");
                continue;
            }
            let settings = loaded.unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(
                settings.base_url.as_deref(),
                expected["base_url"].as_str(),
                "{name}"
            );
            assert_eq!(
                settings.config_api_key.as_deref(),
                expected["api_key"].as_str(),
                "{name}"
            );
            let mode = match (settings.provider, settings.responses) {
                ("anthropic", _) => "anthropic_messages",
                (_, true) => "codex_responses",
                _ => "chat_completions",
            };
            assert_eq!(mode, expected["api_mode"].as_str().unwrap(), "{name}");
        }
    }
}

#[cfg(test)]
mod auxiliary_tests {
    use super::*;

    #[test]
    fn the_compression_route_reads_its_fields() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "auxiliary:\n  compression:\n    provider: auto\n    model: summarizer\n    timeout: 30\n    extra_body: {summary_only: true}\n",
        )
        .unwrap();
        let route = AuxiliaryRoute::from_config(&config, "compression");
        assert_eq!(route.provider, None);
        assert_eq!(route.model.as_deref(), Some("summarizer"));
        assert_eq!(route.timeout_secs, Some(30));
        assert_eq!(route.extra_body["summary_only"], true);
        assert!(AuxiliaryRoute::from_config(&config, "vision").is_default());
    }
}

#[cfg(test)]
mod context_length_tests {
    use super::*;

    #[test]
    fn a_provider_entry_declares_the_window_per_model_or_for_all() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "providers:\n  a:\n    base_url: https://x/v1\n    context_length: 100000\n    models:\n      Big: {context_length: 900000}\n      plain: {}\n",
        )
        .unwrap();
        let entries = custom_providers(&config);
        assert_eq!(
            custom_context_length(&entries, "https://x/v1/", "Big"),
            Some(900_000)
        );
        // Model names are compared as written, so the entry-wide value applies.
        assert_eq!(
            custom_context_length(&entries, "https://x/v1", "big"),
            Some(100_000)
        );
        assert_eq!(
            custom_context_length(&entries, "https://x/v1", "plain"),
            Some(100_000)
        );
        assert_eq!(custom_context_length(&entries, "https://y/v1", "big"), None);
    }
}

/// The messages of a prefill file: a JSON array of message objects. A relative path is taken
/// from the profile directory. Problems are returned as warnings and give no messages.
pub fn load_prefill(path: &str, home: Option<&Path>) -> (Vec<serde_json::Value>, Option<String>) {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => {
            std::env::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest))
        }
        None => PathBuf::from(path),
    };
    let full = if expanded.is_absolute() {
        expanded
    } else {
        home.map_or(expanded.clone(), |home| home.join(&expanded))
    };
    if !full.exists() {
        return (
            Vec::new(),
            Some(format!(
                "Prefill messages file not found: {}",
                full.display()
            )),
        );
    }
    let loaded = std::fs::read_to_string(&full)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}'))
                .map_err(|error| error.to_string())
        });
    match loaded {
        Ok(serde_json::Value::Array(items)) => (
            items.into_iter().filter(|item| item.is_object()).collect(),
            None,
        ),
        Ok(_) => (
            Vec::new(),
            Some(format!(
                "Prefill messages file must contain a JSON array: {}",
                full.display()
            )),
        ),
        Err(error) => (
            Vec::new(),
            Some(format!(
                "Failed to load prefill messages from {}: {error}",
                full.display()
            )),
        ),
    }
}

#[cfg(test)]
mod model_override_tests {
    use super::*;

    #[test]
    fn windows_come_from_explicit_entries_then_defaults() {
        let config = serde_yaml::from_str(
            "model_overrides:\n  _default: {context_window: 50000}\n  custom:\n    _default: {context_window: 60000}\n    Big: {context_window: '900000'}\n    bad: {context_window: -1}\n    flag: {context_window: true}\n  openai:\n    gpt: {context_window: 1.0e5}\n",
        )
        .unwrap();
        let overrides = ModelOverrides::from_config(&config);
        assert_eq!(overrides.explicit_window("custom", "Big"), Some(900_000));
        assert_eq!(overrides.explicit_window("custom", "big"), Some(900_000));
        assert_eq!(overrides.explicit_window("custom", "bad"), None);
        assert_eq!(overrides.explicit_window("custom", "flag"), Some(1));
        assert_eq!(overrides.explicit_window("openai", "gpt"), Some(100_000));
        assert_eq!(overrides.explicit_window("anthropic", "gpt"), None);
        assert_eq!(overrides.default_window("custom"), Some(60_000));
        assert_eq!(overrides.default_window("openai"), Some(50_000));
    }
}

#[cfg(test)]
mod mcp_timeout_tests {
    use super::*;

    /// The recorded `_resolve_tool_timeout` answers for each configuration of the reference.
    #[test]
    fn the_tool_timeout_matches_the_reference_for_recorded_configurations() {
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_reference.json")).unwrap();
        let cases = corpus["calls"]["timeouts"].as_array().unwrap();
        assert!(cases.len() >= 15);
        for case in cases {
            let mut entry = serde_json::json!({"command": "run"});
            for (key, value) in case["server"].as_object().unwrap() {
                entry[key] = value.clone();
            }
            let mut config = serde_json::json!({"mcp_servers": {"s": entry}});
            if !case["timeouts"].is_null() {
                config["timeouts"] = case["timeouts"].clone();
            }
            let config: serde_yaml::Value = serde_yaml::to_value(&config).unwrap();
            let servers = mcp_servers(&config, &HashMap::new(), None);
            assert_eq!(
                servers[0].timeout_secs,
                case["resolved"].as_f64().unwrap(),
                "{case}"
            );
        }
    }
}
