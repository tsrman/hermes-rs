use std::ffi::OsString;
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use clap::{Args, ValueEnum};
use hermes_runtime::config::{load_chat_settings, CompactionSettings, UiModeSetting};
use hermes_runtime::mcp::{McpClients, McpServerConfig};
use hermes_runtime::memory_tool::MemorySettings;
use hermes_runtime::prompt::PromptSettings;
use hermes_runtime::providers::profiles;
use hermes_runtime::providers::{
    AnthropicProvider, BedrockProvider, OpenAiProvider, Provider, ProviderError, ResponsesProvider,
};
use hermes_runtime::storage::SessionWriter;
use hermes_runtime::tool_policy::ToolPolicy;
use hermes_runtime::turn::{
    run_turn, ApprovalDecision, ApprovalRequest, CompactionGuard, CompactionOutcome,
    CompactionTrigger, ModelRoute, TurnError, TurnEvent, TurnOptions, TurnOutcome,
};
use hermes_runtime::web_tools::WebSettings;
use hermes_runtime::BrowserSettings;
use serde::{Deserialize, Serialize};
use tokio::task::block_in_place;
use tokio::time::{interval_at, Instant};

/// The route of the compression summary from `auxiliary.compression`, or `None` when the main
/// route does the work. Whatever the section leaves out is taken from the main route when the
/// endpoint is the same, and must be given otherwise.
fn summary_route(
    settings: &hermes_runtime::config::ChatSettings,
    main_url: &str,
    main_key: &str,
) -> Result<Option<Route>, String> {
    side_route(
        settings,
        &settings.compression_route,
        "auxiliary.compression",
        main_url,
        main_key,
        false,
    )
}

/// The route of a side task from its config section. With `inherit_key`, a section that names
/// only an endpoint (a direct `base_url`) keeps using the main route's key, as `delegation` does.
fn side_route(
    settings: &hermes_runtime::config::ChatSettings,
    aux: &hermes_runtime::config::AuxiliaryRoute,
    label: &str,
    main_url: &str,
    main_key: &str,
    inherit_key: bool,
) -> Result<Option<Route>, String> {
    use hermes_runtime::config::{default_base_url, resolve_provider};
    if aux.is_default() {
        return Ok(None);
    }
    let name = match (&aux.provider, &aux.base_url) {
        (Some(provider), _) => provider.as_str(),
        (None, Some(_)) => "custom",
        (None, None) => settings.provider,
    };
    let Some((provider, default_responses)) = resolve_provider(name) else {
        return Err(format!("{label}.provider '{name}' is not supported"));
    };
    let base_url = aux
        .base_url
        .clone()
        .or_else(|| (provider == settings.provider).then(|| main_url.to_owned()))
        .or_else(|| default_base_url(provider).map(str::to_owned))
        .ok_or_else(|| format!("{label} needs base_url for this provider"))?;
    let same_endpoint = provider == settings.provider
        && base_url.trim_end_matches('/') == main_url.trim_end_matches('/');
    let model = match (&aux.model, same_endpoint) {
        (Some(model), _) => Some(model.clone()),
        (None, true) => None,
        (None, false) => {
            return Err(format!(
                "{label}.model is required when it uses another endpoint"
            ))
        }
    };
    let key_from_env = aux
        .key_env
        .as_deref()
        .and_then(|name| settings.secret(name));
    let api_key = aux
        .api_key
        .clone()
        .or(key_from_env)
        .or_else(|| {
            (same_endpoint
                || (inherit_key && aux.provider.as_deref().is_none_or(|name| name == "custom")))
            .then(|| main_key.to_owned())
        })
        .or_else(|| settings.vendor_key_for(provider, &base_url))
        .unwrap_or_default();
    let responses = match aux.api_mode.as_deref() {
        Some("codex_responses") => true,
        Some("chat_completions") => false,
        _ if same_endpoint => settings.responses,
        _ => default_responses,
    };
    let mut extras = hermes_runtime::providers::Extras {
        body: aux.extra_body.clone(),
        headers: Vec::new(),
    };
    if same_endpoint {
        extras.headers = settings.default_headers.clone();
    }
    for (name, value) in
        hermes_runtime::config::custom_extra_headers(&settings.custom_providers, &base_url)
    {
        extras
            .headers
            .retain(|(known, _)| !known.eq_ignore_ascii_case(&name));
        extras.headers.push((name, value));
    }
    Ok(Some(Route {
        model,
        provider: provider.into(),
        responses,
        base_url,
        api_key,
        extras,
        reasoning: Default::default(),
        timeout_secs: aux.timeout_secs,
        openai_native_web_search: false,
        bedrock_region: (provider == "bedrock").then(|| {
            hermes_runtime::providers::bedrock::resolve_bedrock_region(
                &settings.bedrock,
                settings.secrets(),
            )
        }),
        bedrock_auth: (provider == "bedrock")
            .then(|| hermes_runtime::providers::bedrock::resolve_aws_auth(settings.secrets()).ok())
            .flatten(),
        bedrock_guardrail: (provider == "bedrock")
            .then(|| settings.bedrock.guardrail_config())
            .flatten(),
    }))
}

/// What to do when the provider is not set up yet.
const SETUP_HINT: &str = "Set it once with `hermes config set model.provider custom`, `hermes config set model.base_url <url>`, `hermes config set model.default <model>` and put the key in the profile .env as HERMES_API_KEY (or in config.yaml as model.api_key); --base-url, --model and --api-key do the same for one run.";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum UiModeArg {
    Auto,
    Inline,
    Tui,
    Plain,
}

impl From<UiModeArg> for UiModeSetting {
    fn from(arg: UiModeArg) -> Self {
        match arg {
            UiModeArg::Auto => Self::Auto,
            UiModeArg::Inline => Self::Inline,
            UiModeArg::Tui => Self::Tui,
            UiModeArg::Plain => Self::Plain,
        }
    }
}

#[derive(Args)]
pub struct ChatArgs {
    /// The request to run. Without it, an interactive conversation starts.
    #[arg(short, long)]
    query: Option<String>,
    #[arg(long)]
    oneshot: bool,
    /// Save a completed turn as a new session in the selected profile.
    #[arg(long)]
    save_session: bool,
    /// Continue a saved session and append a completed turn.
    #[arg(short = 'r', long, conflicts_with_all = ["save_session", "continue_last"])]
    resume: Option<String>,
    /// Continue the most recent CLI session.
    #[arg(short = 'c', long = "continue", conflicts_with = "save_session")]
    continue_last: bool,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: Option<u32>,
    #[arg(long, env = "HERMES_BASE_URL")]
    base_url: Option<String>,
    #[arg(long, env = "HERMES_MODEL")]
    model: Option<String>,
    #[arg(long, env = "HERMES_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
    /// Approve every file change and command without asking, except what `approvals.deny` names.
    #[arg(long)]
    yolo: bool,
    /// Append what the turn sent, received and ran to this file, one JSON object per line.
    #[arg(long, env = "HERMES_DEBUG_LOG", value_name = "FILE")]
    debug_log: Option<OsString>,
    /// Emit versioned JSON Lines events on stdout instead of plain text (requires --query).
    #[arg(long)]
    pub events_json: bool,
    /// Interactive console interface mode: auto, inline, tui, or plain.
    #[arg(long, value_enum, env = "HERMES_UI")]
    pub ui: Option<UiModeArg>,
}

#[cfg(test)]
impl ChatArgs {
    pub fn for_test(base_url: &str, model: &str, api_key: &str) -> Self {
        Self {
            query: None,
            oneshot: false,
            save_session: false,
            resume: None,
            continue_last: false,
            max_turns: None,
            base_url: Some(base_url.to_owned()),
            model: Some(model.to_owned()),
            api_key: Some(api_key.to_owned()),
            yolo: false,
            debug_log: None,
            events_json: false,
            ui: None,
        }
    }
}

pub const EVENTS_JSON_VERSION: u32 = 1;

pub fn json_event(kind: &str, mut fields: serde_json::Map<String, serde_json::Value>) -> String {
    let mut payload = serde_json::Map::new();
    payload.insert("version".into(), serde_json::json!(EVENTS_JSON_VERSION));
    payload.insert("event".into(), serde_json::json!(kind));
    payload.append(&mut fields);
    let mut line = serde_json::Value::Object(payload).to_string();
    line.push('\n');
    line
}

pub fn json_warning(message: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("message".into(), serde_json::json!(message));
    json_event("warning", fields)
}

pub fn json_error(message: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("message".into(), serde_json::json!(message));
    json_event("error", fields)
}

pub fn json_cancelled(exit_code: u8) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("exit_code".into(), serde_json::json!(exit_code));
    json_event("cancelled", fields)
}

pub fn json_done(exit_code: u8, status: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("exit_code".into(), serde_json::json!(exit_code));
    fields.insert("status".into(), serde_json::json!(status));
    json_event("done", fields)
}

pub fn write_stdout_line(line: &str) {
    let mut output = io::stdout().lock();
    let _ = output.write_all(line.as_bytes());
    let _ = output.flush();
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ChatRequest {
    #[serde(default)]
    events_json: bool,
    #[serde(default)]
    ui_mode: UiModeSetting,
    #[serde(default)]
    startup_warnings: Vec<String>,
    #[serde(default)]
    state_scope: Option<String>,
    /// Where the compression summary is made; the main route when unset.
    #[serde(default)]
    summary: Option<Route>,
    /// Where subagents run; the main route when unset.
    #[serde(default)]
    delegate: Option<Route>,
    /// Messages sent after the system prompt on every request.
    #[serde(default)]
    prefill: Vec<serde_json::Value>,
    root: OsString,
    query: String,
    resume: Option<String>,
    save_session: bool,
    max_turns: u32,
    attempts: u32,
    #[serde(default)]
    prompt: PromptSettings,
    #[serde(default)]
    compression: CompactionSettings,
    #[serde(default)]
    keep_phone_numbers: bool,
    #[serde(default = "default_stall_guards")]
    stall_guards: bool,
    #[serde(default)]
    intent_ack_continuation: hermes_runtime::stall_guards::IntentAckContinuation,
    #[serde(default)]
    debug_log: Option<OsString>,
    #[serde(default)]
    approvals: hermes_runtime::approval_policy::ApprovalSettings,
    #[serde(default)]
    delegation: hermes_runtime::delegation::DelegationSettings,
    #[serde(default)]
    oneshot: bool,
    /// `chat -q`: one question, then exit. An interactive turn is not one.
    #[serde(default)]
    single_query: bool,
    #[serde(default)]
    approval_timeout_secs: Option<u32>,
    #[serde(default)]
    terminal_approval_timeout_secs: Option<u32>,
    #[serde(default)]
    terminal_local: bool,
    #[serde(default)]
    terminal_env: hermes_runtime::terminal::TerminalSettings,
    #[serde(default = "default_clarify_timeout")]
    clarify_timeout_secs: u32,
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,
    #[serde(default = "default_mcp_result_size_chars")]
    mcp_result_size_chars: usize,
    #[serde(default)]
    web: WebSettings,
    #[serde(default)]
    browser: BrowserSettings,
    #[serde(default)]
    memory: MemorySettings,
    #[serde(default)]
    tools: ToolPolicy,
    model_override: Option<String>,
    primary: Route,
    fallbacks: Vec<Route>,
}

fn default_clarify_timeout() -> u32 {
    90
}

fn default_stall_guards() -> bool {
    true
}

fn default_mcp_result_size_chars() -> usize {
    hermes_runtime::mcp_media::DEFAULT_MCP_RESULT_SIZE_CHARS
}

#[derive(Clone, Deserialize, Serialize)]
struct Route {
    model: Option<String>,
    provider: String,
    #[serde(default)]
    responses: bool,
    base_url: String,
    api_key: String,
    #[serde(default)]
    extras: hermes_runtime::providers::Extras,
    #[serde(default)]
    reasoning: hermes_runtime::reasoning::ReasoningSettings,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    openai_native_web_search: bool,
    #[serde(default)]
    bedrock_region: Option<String>,
    #[serde(default)]
    bedrock_auth: Option<hermes_runtime::providers::bedrock::AwsAuth>,
    #[serde(default)]
    bedrock_guardrail: Option<serde_json::Value>,
}

impl Route {
    fn open(&self, model: String, attempts: u32) -> Result<ModelRoute, String> {
        self.open_with_session(model, attempts, None)
    }

    fn open_with_session(
        &self,
        model: String,
        attempts: u32,
        session_id: Option<&str>,
    ) -> Result<ModelRoute, String> {
        let provider_name = match self.provider.as_str() {
            "custom" => "custom",
            "openai" => "openai",
            "anthropic" => "anthropic",
            "nvidia" => "nvidia",
            "bedrock" => "bedrock",
            "vertex" => "vertex",
            other => {
                if let Some((oauth_name, _)) =
                    hermes_runtime::providers::oauth::resolve_oauth_provider(other)
                {
                    oauth_name
                } else if let Some(profile) = profiles::lookup(other) {
                    profile.name.as_str()
                } else {
                    return Err("unsupported provider".into());
                }
            }
        };
        let timeout = Duration::from_secs(self.timeout_secs.unwrap_or(120));
        let setup = |error| format!("provider setup failed: {error:?}");
        let provider = if provider_name == "bedrock" {
            let region = self
                .bedrock_region
                .as_deref()
                .unwrap_or(hermes_runtime::providers::bedrock::DEFAULT_BEDROCK_REGION);
            let auth = self.bedrock_auth.clone().unwrap_or_else(|| {
                hermes_runtime::providers::bedrock::AwsAuth::Bearer(self.api_key.clone())
            });
            if hermes_runtime::providers::bedrock::bedrock_openai_uses_mantle(&model) {
                let default_runtime =
                    hermes_runtime::providers::bedrock::bedrock_runtime_base_url(region);
                let mantle_url = if self.base_url.trim_end_matches('/')
                    == default_runtime.trim_end_matches('/')
                {
                    hermes_runtime::providers::bedrock::bedrock_openai_base_url(region)
                } else {
                    self.base_url.clone()
                };
                Provider::Responses(
                    ResponsesProvider::new(&mantle_url, &self.api_key, timeout)
                        .map_err(setup)?
                        .with_sigv4(region.to_owned(), auth)
                        .with_session_id(session_id.map(str::to_owned))
                        .with_extras(self.extras.clone())
                        .with_max_attempts(attempts),
                )
            } else {
                Provider::Bedrock(
                    BedrockProvider::new(&self.base_url, region, auth, timeout)
                        .map_err(setup)?
                        .with_guardrail(self.bedrock_guardrail.clone())
                        .with_extras(self.extras.clone())
                        .with_max_attempts(attempts),
                )
            }
        } else if provider_name == "anthropic" || provider_name == "minimax-oauth" {
            let is_oauth = hermes_runtime::providers::oauth::anthropic_route_is_oauth(
                &self.base_url,
                &self.api_key,
                Some(provider_name),
            );
            Provider::Anthropic(
                AnthropicProvider::new(&self.base_url, &self.api_key, timeout)
                    .map_err(setup)?
                    .with_bearer_auth(provider_name == "minimax-oauth")
                    .with_oauth_identity(is_oauth)
                    .with_extras(self.extras.clone())
                    .with_max_attempts(attempts),
            )
        } else if self.responses {
            let mut extras = self.extras.clone();
            let mut body = self
                .reasoning
                .responses_fields(&model, provider_name, &self.base_url);
            hermes_runtime::providers::merge_body_maps(&mut body, &extras.body);
            extras.body = body;
            Provider::Responses(
                ResponsesProvider::new(&self.base_url, &self.api_key, timeout)
                    .map_err(setup)?
                    .with_xai(hermes_runtime::reasoning::is_xai_responses(
                        provider_name,
                        &self.base_url,
                    ))
                    .with_openai_native_web_search(self.openai_native_web_search)
                    .with_session_id(session_id.map(str::to_owned))
                    .with_extras(extras)
                    .with_max_attempts(attempts),
            )
        } else {
            Provider::OpenAi(
                OpenAiProvider::new(&self.base_url, &self.api_key, timeout)
                    .map_err(setup)?
                    .with_strip_tool_message_names(provider_name == "nvidia")
                    .with_extras(self.extras.clone())
                    .with_max_attempts(attempts),
            )
        };
        Ok(ModelRoute {
            model,
            provider_name,
            provider,
        })
    }
}

#[derive(Deserialize, Serialize)]
pub enum Event {
    Stdout(String),
    Stderr(String),
    Approval { id: u64, request: ApprovalRequest },
    Exit(u8),
}

pub fn display(event: Event) -> io::Result<()> {
    match event {
        Event::Stdout(text) => {
            let mut output = io::stdout().lock();
            output.write_all(text.as_bytes())?;
            output.flush()
        }
        Event::Stderr(text) => writeln!(io::stderr().lock(), "{text}"),
        Event::Exit(_) | Event::Approval { .. } => Ok(()),
    }
}

pub fn prepare(home: Option<&Path>, mut args: ChatArgs) -> Result<ChatRequest, String> {
    if args.events_json && args.query.is_none() {
        return Err("--events-json requires --query".into());
    }
    let oneshot = args.oneshot || args.events_json;
    let mut startup_warnings = Vec::new();
    if args.continue_last {
        let home = home.ok_or("profile root is unavailable")?;
        let store = hermes_runtime::storage::SessionStore::open_read_only(home)
            .map_err(|_| "no saved session to continue".to_owned())?;
        let latest = store
            .recent(1, Some("cli"))
            .map_err(|error| error.to_string())?
            .pop()
            .ok_or("no saved session to continue")?;
        args.resume = Some(latest.id);
    }
    let mut settings = load_chat_settings(home)?;
    let mut bedrock_region = None;
    let mut bedrock_auth = None;
    let mut bedrock_guardrail = None;
    let (base_url, api_key) = if settings.provider == "vertex" {
        let explicit_key = args
            .api_key
            .clone()
            .or(settings.config_api_key.clone())
            .or(settings.api_key.clone())
            .filter(|v| !v.is_empty());
        let explicit_url = args
            .base_url
            .clone()
            .or_else(|| settings.base_url.clone())
            .filter(|v| !v.is_empty());
        match (explicit_url, explicit_key) {
            (Some(u), Some(k)) => (u, k),
            (u_opt, k_opt) => {
                let (token, v_url) = hermes_runtime::providers::vertex::resolve_vertex_route(
                    &settings.vertex,
                    settings.secrets(),
                )?;
                (u_opt.unwrap_or(v_url), k_opt.unwrap_or(token))
            }
        }
    } else if settings.provider == "bedrock" {
        let region = hermes_runtime::providers::bedrock::resolve_bedrock_region(
            &settings.bedrock,
            settings.secrets(),
        );
        let explicit_key = args
            .api_key
            .clone()
            .or(settings.config_api_key.clone())
            .or(settings.api_key.clone())
            .filter(|v| !v.is_empty());
        let auth = match explicit_key {
            Some(token) => hermes_runtime::providers::bedrock::AwsAuth::Bearer(token),
            None => hermes_runtime::providers::bedrock::resolve_aws_auth(settings.secrets())?,
        };
        let url = args
            .base_url
            .clone()
            .or_else(|| settings.base_url.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                hermes_runtime::providers::bedrock::bedrock_runtime_base_url(&region)
            });
        let key_repr = match &auth {
            hermes_runtime::providers::bedrock::AwsAuth::Bearer(t) => t.clone(),
            hermes_runtime::providers::bedrock::AwsAuth::SigV4 { .. } => "aws-sigv4".to_owned(),
        };
        bedrock_region = Some(region);
        bedrock_auth = Some(auth);
        bedrock_guardrail = settings.bedrock.guardrail_config();
        (url, key_repr)
    } else if hermes_runtime::providers::oauth::resolve_oauth_provider(settings.provider).is_some()
    {
        let explicit_key = args
            .api_key
            .clone()
            .or(settings.config_api_key.clone())
            .or(settings.api_key.clone())
            .filter(|v| !v.is_empty());
        let explicit_url = args.base_url.clone().filter(|v| !v.is_empty());
        match explicit_key {
            Some(k) => {
                let u = explicit_url
                    .or_else(|| settings.base_url.clone())
                    .filter(|v| !v.is_empty())
                    .ok_or(format!("model base URL is required. {SETUP_HINT}"))?;
                (u, k)
            }
            None => {
                let resolved = hermes_runtime::providers::oauth::resolve_oauth_credentials(
                    home,
                    settings.provider,
                    settings.secrets(),
                )?;
                let u = explicit_url.unwrap_or(resolved.base_url);
                (u, resolved.access_token)
            }
        }
    } else {
        let base_url = args
            .base_url
            .or_else(|| settings.base_url.clone())
            .filter(|value| !value.is_empty())
            .ok_or(format!("model base URL is required. {SETUP_HINT}"))?;
        let vendor_api_key = settings.vendor_key_for(settings.provider, &base_url);
        let api_key = args
            .api_key
            .or(settings.config_api_key.clone())
            .or(settings.api_key.clone())
            .or(vendor_api_key)
            .filter(|value| !value.is_empty())
            // A custom endpoint may be a local server that wants no key; the reference sends this
            // placeholder there, and the endpoint's own 401 is the answer if it does want one.
            .or_else(|| {
                hermes_runtime::config::ChatSettings::keyless_endpoint(settings.provider)
                    .then(|| "no-key-required".to_owned())
            })
            .ok_or(format!("API key is required. {SETUP_HINT}"))?;
        (base_url, api_key)
    };
    if let Some(model) = args.model.as_deref().or(settings.model.as_deref()) {
        // The reference's order after `model.context_length`: an explicit `model_overrides`
        // entry, then the matching `providers` entry, then a `_default` override.
        let overrides = &settings.model_overrides;
        let window = overrides
            .explicit_window(settings.provider, model)
            .or_else(|| {
                hermes_runtime::config::custom_context_length(
                    &settings.custom_providers,
                    &base_url,
                    model,
                )
            })
            .or_else(|| overrides.default_window(settings.provider));
        if let Some(window) = window {
            settings.compression.set_context_length_if_unset(window);
        }
    }
    if let Some(model) = args.model.as_deref().or(settings.model.as_deref()) {
        settings.compression.set_route(model, settings.provider);
    }
    settings.terminal_env.scratch = home.and_then(hermes_runtime::scratch::directory);
    hermes_runtime::redact::set_enabled(settings.redact_secrets);
    if !settings.redact_secrets {
        let message = "secret redaction is off (security.redact_secrets or HERMES_REDACT_SECRETS); credentials in tool output reach the model unmasked.";
        if args.events_json {
            startup_warnings.push(message.to_owned());
        } else {
            eprintln!("Warning: {message}");
        }
    }
    let mut extras = hermes_runtime::providers::Extras::default();
    // A named provider's own request fields (what its hooks add with no reasoning setting).
    if let Some(profile) = hermes_runtime::providers::profiles::lookup(settings.provider) {
        extras.body = profile.default_body();
    }
    if let Some(model) = args.model.as_deref().or(settings.model.as_deref()) {
        if settings.provider == "custom" || settings.named_custom {
            // The reference's `custom` profile asks for `reasoning_effort` (medium unless
            // `agent.reasoning_effort` or `agent.reasoning_overrides` say otherwise) on
            // chat completions; the endpoint's own `extra_body` wins over it.
            if !settings.responses {
                extras
                    .body
                    .extend(settings.reasoning.custom_fields(model, &base_url));
            }
            let mut body = extras.body;
            if let Some(own) = hermes_runtime::config::custom_extra_body(
                &settings.custom_providers,
                &base_url,
                model,
            ) {
                hermes_runtime::providers::merge_body_maps(&mut body, &own);
            }
            extras.body = body;
        }
    }
    // `model.default_headers` first; the endpoint's own `extra_headers` win over them.
    extras.headers = settings.default_headers.clone();
    for (name, value) in
        hermes_runtime::config::custom_extra_headers(&settings.custom_providers, &base_url)
    {
        extras
            .headers
            .retain(|(known, _)| !known.eq_ignore_ascii_case(&name));
        extras.headers.push((name, value));
    }
    let summary = summary_route(&settings, &base_url, &api_key)?;
    let delegate = side_route(
        &settings,
        &settings.delegation_route,
        "delegation",
        &base_url,
        &api_key,
        true,
    )?;
    let prefill = match settings.prefill_file.as_deref() {
        Some(path) => {
            let (messages, warning) = hermes_runtime::config::load_prefill(path, home);
            if let Some(warning) = warning {
                if args.events_json {
                    startup_warnings.push(warning);
                } else {
                    eprintln!("Warning: {warning}");
                }
            }
            messages
        }
        None => Vec::new(),
    };
    Ok(ChatRequest {
        events_json: args.events_json,
        ui_mode: args.ui.map(UiModeSetting::from).unwrap_or(settings.ui_mode),
        startup_warnings,
        state_scope: None,
        prefill,
        summary,
        delegate,
        root: std::env::current_dir()
            .map_err(|error| error.to_string())?
            .into_os_string(),
        single_query: args.query.is_some(),
        query: args.query.unwrap_or_default(),
        resume: args.resume,
        save_session: args.save_session,
        max_turns: args.max_turns.unwrap_or(settings.max_turns),
        attempts: settings.api_max_attempts,
        prompt: settings.prompt,
        compression: settings.compression,
        keep_phone_numbers: settings.keep_phone_numbers,
        stall_guards: settings.stall_guards,
        intent_ack_continuation: settings.intent_ack_continuation,
        debug_log: args.debug_log.clone(),
        delegation: settings.delegation.clone(),
        oneshot,
        approvals: {
            let mut approvals = settings.approvals.clone();
            approvals.approve_all |= args.yolo;
            approvals
        },
        approval_timeout_secs: ((!oneshot && crate::approval::available())
            || settings.approvals.answers_without_a_person()
            || args.yolo)
            .then_some(settings.approval_timeout_secs),
        terminal_approval_timeout_secs: Some(settings.approval_timeout_secs),
        terminal_local: settings.terminal_local,
        terminal_env: settings.terminal_env,
        clarify_timeout_secs: settings.clarify_timeout_secs,
        mcp_servers: settings.mcp_servers,
        mcp_result_size_chars: settings.mcp_result_size_chars,
        primary: Route {
            model: settings.model,
            provider: settings.provider.into(),
            responses: settings.responses,
            extras,
            reasoning: settings.reasoning.clone(),
            timeout_secs: None,
            openai_native_web_search: settings.web.search
                == hermes_runtime::web_tools::Backend::OpenaiNative,
            base_url,
            api_key,
            bedrock_region,
            bedrock_auth,
            bedrock_guardrail,
        },
        fallbacks: settings
            .fallbacks
            .into_iter()
            .map(|config| Route {
                model: Some(config.model),
                provider: config.provider.into(),
                responses: config.responses,
                base_url: config.base_url,
                api_key: config.api_key.unwrap_or_default(),
                extras: Default::default(),
                reasoning: settings.reasoning.clone(),
                timeout_secs: None,
                openai_native_web_search: settings.web.search
                    == hermes_runtime::web_tools::Backend::OpenaiNative,
                bedrock_region: None,
                bedrock_auth: None,
                bedrock_guardrail: None,
            })
            .collect(),
        web: settings.web,
        browser: settings.browser,
        memory: settings.memory,
        tools: settings.tools,
        model_override: args.model,
    })
}

pub async fn execute<A, F>(
    home: Option<&Path>,
    request: ChatRequest,
    emit: impl FnMut(Event) -> bool,
    approve: A,
) -> Result<u8, String>
where
    A: FnMut(ApprovalRequest) -> F + Send,
    F: Future<Output = ApprovalDecision> + Send + 'static,
{
    execute_with_guard(home, request, None, emit, approve).await
}

pub async fn execute_with_guard<A, F>(
    home: Option<&Path>,
    request: ChatRequest,
    guard: Option<&std::sync::Mutex<CompactionGuard>>,
    mut emit: impl FnMut(Event) -> bool,
    approve: A,
) -> Result<u8, String>
where
    A: FnMut(ApprovalRequest) -> F + Send,
    F: Future<Output = ApprovalDecision> + Send + 'static,
{
    let root = Path::new(&request.root);
    if !root.is_absolute()
        || request.max_turns == 0
        || (request.save_session && request.resume.is_some())
    {
        return Err("invalid chat request".into());
    }
    let mut writer = if let Some(id) = request.resume.as_deref() {
        let home = home.ok_or("profile root is unavailable")?;
        Some(
            block_in_place(|| SessionWriter::prepare_resume(home, id))
                .map_err(|error| error.to_string())?,
        )
    } else if request.save_session {
        let home = home.ok_or("profile root is unavailable")?;
        Some(block_in_place(|| SessionWriter::prepare(home)).map_err(|error| error.to_string())?)
    } else {
        None
    };
    let model = request
        .model_override
        .or_else(|| {
            writer
                .as_ref()
                .and_then(SessionWriter::saved_model)
                .map(str::to_owned)
        })
        .or_else(|| request.primary.model.clone())
        .filter(|value| !value.is_empty())
        .ok_or("model name is required")?;
    let mut history = writer
        .as_mut()
        .map_or_else(Default::default, SessionWriter::take_history);
    let cache_scope = writer
        .as_ref()
        .and_then(SessionWriter::session_id)
        .or(request.state_scope.as_deref());
    let primary =
        request
            .primary
            .open_with_session(model.clone(), request.attempts, cache_scope)?;
    let summarizer = match &request.summary {
        Some(route) => Some(route.open(route.model.clone().unwrap_or(model), request.attempts)?),
        None => None,
    };
    let json_mode = request.events_json;
    for warning in &request.startup_warnings {
        if json_mode {
            emit(Event::Stdout(json_warning(warning)));
        }
    }
    let (clients, warnings) = McpClients::start_with_options(
        &request.mcp_servers,
        home.map(Path::to_path_buf),
        request.mcp_result_size_chars,
    )
    .await;
    for warning in warnings {
        if json_mode {
            emit(Event::Stdout(json_warning(&warning)));
        } else {
            emit(Event::Stderr(warning));
        }
    }
    let mcp = tokio::sync::Mutex::new(clients);
    let journal = match &request.debug_log {
        Some(path) => Some(hermes_runtime::journal::Journal::open(Path::new(path))?),
        None => None,
    };
    let delegate_route = match &request.delegate {
        Some(route) => Some(route.open(
            route.model.clone().unwrap_or_else(|| primary.model.clone()),
            request.attempts,
        )?),
        None => None,
    };
    let delegation =
        hermes_runtime::delegation::Delegation::new(request.delegation.clone(), request.oneshot)
            .with_route(delegate_route);
    let options = TurnOptions {
        single_query: request.single_query,
        journal: journal.as_ref(),
        delegation: Some(&delegation),
        system_prompt_override: None,
        child: None,
        prefill: (!request.prefill.is_empty()).then_some(request.prefill.as_slice()),
        state_scope: request.state_scope.as_deref(),
        mcp: Some(&mcp),
        web: Some(&request.web),
        browser: Some(&request.browser),
        memory: Some(&request.memory),
        tools: &request.tools,
        root,
        profile: home,
        session_id: writer.as_ref().and_then(SessionWriter::session_id),
        max_turns: request.max_turns,
        approval_timeout_secs: request.approval_timeout_secs,
        terminal_approval_timeout_secs: request.terminal_approval_timeout_secs,
        terminal_local: request.terminal_local,
        compression: Some(&request.compression),
        keep_phone_numbers: request.keep_phone_numbers,
        stall_guards: request.stall_guards,
        intent_ack_continuation: &request.intent_ack_continuation,
        summarizer: summarizer.as_ref(),
        compaction_guard: guard,
        terminal_env: Some(&request.terminal_env),
        clarify_timeout_secs: request.clarify_timeout_secs,
    };
    block_in_place(|| history.capture_prompt(&options, &request.prompt, &primary));
    let fallbacks = request
        .fallbacks
        .iter()
        .filter_map(|route| {
            route
                .open_with_session(route.model.clone()?, request.attempts, cache_scope)
                .ok()
        })
        .collect::<Vec<_>>();
    let mut receiving = true;
    let outcome = {
        let turn = async {
            let mut approve = approve;
            let mut compaction = history
                .compact(
                    home,
                    root,
                    &request.prompt,
                    &request.compression,
                    &primary,
                    summarizer.as_ref(),
                    CompactionTrigger::Estimate {
                        query: &request.query,
                    },
                    guard,
                )
                .await;
            let mut retried = false;
            loop {
                if let CompactionOutcome::Skipped = compaction {
                    receiving = emit(if json_mode {
                        let mut fields = serde_json::Map::new();
                        fields.insert("reason".into(), serde_json::json!("skipped"));
                        Event::Stdout(json_event("context_compressed", fields))
                    } else {
                        Event::Stderr(
                            "Context compression skipped: no usable shorter context.".into(),
                        )
                    });
                }
                // Output or tool effects from a failed attempt cannot be replayed safely.
                let mut started = false;
                let result = run_turn(
                    &primary,
                    &fallbacks,
                    &request.query,
                    options,
                    history.clone(),
                    |event| {
                        started |= matches!(event, TurnEvent::Text(_) | TurnEvent::ToolStarted(_));
                        if receiving {
                            receiving = emit(if json_mode {
                                format_json_event(event)
                            } else {
                                format_event(event)
                            });
                        }
                    },
                    &mut approve,
                )
                .await;
                let overflow = matches!(
                    result,
                    Err(TurnError::Provider(ProviderError::ContextOverflow))
                );
                if !overflow || started || retried || history.is_compacted() {
                    break result;
                }
                retried = true;
                compaction = history
                    .compact(
                        home,
                        root,
                        &request.prompt,
                        &request.compression,
                        &primary,
                        summarizer.as_ref(),
                        CompactionTrigger::Overflow {
                            query: &request.query,
                        },
                        guard,
                    )
                    .await;
                if !matches!(compaction, CompactionOutcome::Applied) {
                    break result;
                }
                receiving = emit(if json_mode {
                    let mut fields = serde_json::Map::new();
                    fields.insert("reason".into(), serde_json::json!("overflow"));
                    Event::Stdout(json_event("context_compressed", fields))
                } else {
                    Event::Stderr(
                        "Context window exceeded: retrying with compressed history.".into(),
                    )
                });
            }
        };
        let period = Duration::from_secs(60);
        let mut refresh = interval_at(Instant::now() + period, period);
        tokio::pin!(turn);
        loop {
            tokio::select! {
                result = &mut turn => break result.map_err(|error| format!("turn failed: {error:?}"))?,
                _ = refresh.tick(), if request.resume.is_some() => {
                    if let Some(writer) = &writer {
                        block_in_place(|| writer.refresh()).map_err(|error| error.to_string())?;
                    }
                }
            }
        }
    };
    mcp.lock().await.shutdown().await;
    if !receiving || (!json_mode && !emit(Event::Stdout("\n".into()))) {
        return Err("chat output was interrupted".into());
    }
    match outcome {
        TurnOutcome::Completed(turn) => {
            let (input, output) = turn.usage();
            if json_mode {
                let mut fields = serde_json::Map::new();
                fields.insert("input_tokens".into(), serde_json::json!(input));
                fields.insert("output_tokens".into(), serde_json::json!(output));
                emit(Event::Stdout(json_event("usage", fields)));
            } else if request.state_scope.is_some() {
                emit(Event::Stderr(format!("Usage: {input} {output}")));
            }
            if let Some(mut writer) = writer {
                let id = block_in_place(|| writer.save(&turn, root))
                    .map_err(|error| error.to_string())?;
                if json_mode {
                    let mut fields = serde_json::Map::new();
                    fields.insert("id".into(), serde_json::json!(id));
                    emit(Event::Stdout(json_event("session", fields)));
                } else {
                    emit(Event::Stderr(format!("Session: {id}")));
                }
            }
            if json_mode {
                emit(Event::Stdout(json_done(0, "completed")));
            }
            Ok(0)
        }
        TurnOutcome::IterationLimit => {
            if json_mode {
                emit(Event::Stdout(json_done(1, "iteration_limit")));
            }
            Ok(1)
        }
    }
}

fn format_json_event(event: TurnEvent<'_>) -> Event {
    let mut fields = serde_json::Map::new();
    let line = match event {
        TurnEvent::Text(text) => {
            fields.insert("text".into(), serde_json::json!(text));
            json_event("text", fields)
        }
        TurnEvent::Fallback(model) => {
            fields.insert("model".into(), serde_json::json!(model));
            json_event("fallback", fields)
        }
        TurnEvent::ToolStarted(name) => {
            fields.insert("name".into(), serde_json::json!(name));
            json_event("tool_started", fields)
        }
        TurnEvent::ToolFinished { name, succeeded } => {
            fields.insert("name".into(), serde_json::json!(name));
            fields.insert("succeeded".into(), serde_json::json!(succeeded));
            json_event("tool_finished", fields)
        }
        TurnEvent::IterationLimit(limit) => {
            fields.insert("limit".into(), serde_json::json!(limit));
            json_event("iteration_limit", fields)
        }
        TurnEvent::ContextSqueezed => {
            fields.insert("reason".into(), serde_json::json!("midturn"));
            json_event("context_compressed", fields)
        }
    };
    Event::Stdout(line)
}

fn format_event(event: TurnEvent<'_>) -> Event {
    match event {
        TurnEvent::Text(text) => Event::Stdout(text.into()),
        TurnEvent::Fallback(model) => Event::Stderr(format!("Model fallback: {model:?}")),
        TurnEvent::ToolStarted(name) => Event::Stderr(format!("Tool started: {name:?}")),
        TurnEvent::ToolFinished { name, succeeded } => {
            let status = if succeeded { "completed" } else { "failed" };
            Event::Stderr(format!("Tool {status}: {name:?}"))
        }
        TurnEvent::IterationLimit(limit) => Event::Stderr(format!(
            "Iteration limit reached ({limit}). Requesting summary."
        )),
        TurnEvent::ContextSqueezed => Event::Stderr(
            "Context window exceeded: replacing earlier steps of this turn with a summary and retrying."
                .into(),
        ),
    }
}

/// Run one unattended turn and return the final text and the exit status. Nothing is asked of
/// a person: file writes and commands are not offered, and the turn is not saved as a session.
/// `activity` is called on every output event so that a caller can watch for a stalled turn.
pub async fn run_text(
    home: Option<&Path>,
    query: String,
    model: Option<String>,
    platform: &str,
    workdir: Option<&Path>,
    mut activity: impl FnMut(),
) -> Result<(String, u8), String> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let args = ChatArgs {
        query: Some(query),
        oneshot: true,
        save_session: false,
        resume: None,
        continue_last: false,
        max_turns: None,
        base_url: env("HERMES_BASE_URL"),
        model: model.or_else(|| env("HERMES_MODEL")),
        api_key: env("HERMES_API_KEY"),
        yolo: false,
        debug_log: None,
        events_json: false,
        ui: None,
    };
    let mut request = prepare(home, args)?;
    if let Some(dir) = workdir.filter(|dir| dir.is_absolute() && dir.is_dir()) {
        request.root = dir.as_os_str().to_os_string();
    }
    request.tools = hermes_runtime::config::load_tool_policy(home, platform)?;
    if platform == "cron" {
        // Loop prevention: cron runs cannot schedule other cron jobs unless explicitly opted in.
        let allow_scheduling = home.is_some_and(hermes_runtime::cron::allow_agent_scheduling);
        if !allow_scheduling {
            request.tools = request.tools.without_tools(&["cronjob_manage"]);
        }
        // No person answers a cron job: unflagged commands run, flagged ones follow `cron_mode`.
        crate::approval::install(request.approvals().clone());
        if let Some(policy) = hermes_runtime::approval_policy::installed() {
            policy.set_unattended(request.approvals().cron_approves);
        }
    }
    let mut text = String::new();
    let code = execute(
        home,
        request,
        |event| {
            activity();
            if let Event::Stdout(chunk) = event {
                text.push_str(&chunk);
            }
            true
        },
        |_| async { ApprovalDecision::Unavailable },
    )
    .await?;
    Ok((text, code))
}

impl ChatRequest {
    pub fn is_interactive(&self) -> bool {
        self.query.is_empty()
    }

    /// A single query that no person can answer: `--oneshot`, or no terminal to ask on.
    pub fn is_unattended(&self, terminal_available: bool) -> bool {
        !self.is_interactive() && (self.oneshot || !terminal_available)
    }

    /// The model the next turn uses: the `/model` choice, else the configured default.
    pub fn model(&self) -> Option<&str> {
        self.model_override
            .as_deref()
            .or(self.primary.model.as_deref())
    }

    pub fn set_model(&mut self, model: &str) {
        self.model_override = Some(model.to_owned());
    }

    pub fn resume(&self) -> Option<&str> {
        self.resume.as_deref()
    }

    /// The request for one turn of an interactive conversation: the first turn saves a new
    /// session, later ones continue it.
    pub fn for_turn(mut self, query: &str, scope: &str, session: Option<&str>) -> Self {
        self.query = query.to_owned();
        self.single_query = false;
        self.state_scope = Some(scope.to_owned());
        self.resume = session.map(str::to_owned);
        self.save_session = session.is_none();
        self
    }

    pub fn approvals(&self) -> &hermes_runtime::approval_policy::ApprovalSettings {
        &self.approvals
    }

    pub fn ui_mode(&self) -> UiModeSetting {
        self.ui_mode
    }

    pub fn provider_name(&self) -> &str {
        &self.primary.provider
    }

    pub fn context_length(&self) -> Option<u64> {
        self.compression.context_window()
    }

    #[cfg(unix)]
    pub fn events_json(&self) -> bool {
        self.events_json
    }
}

/// Outcome of an explicit `/compress [focus]` command in the REPL.
pub enum ManualCompress {
    Applied { archived: usize },
    Unchanged,
    Skipped,
}

/// Compress a saved session immediately in place (REPL `/compress [focus]`), clearing any active
/// cooldown/anti-thrash state in `guard`.
pub async fn compress_session(
    home: &Path,
    request: &ChatRequest,
    session_id: &str,
    focus: Option<&str>,
    guard: &std::sync::Mutex<CompactionGuard>,
) -> Result<ManualCompress, String> {
    let root = Path::new(&request.root);
    let mut writer = block_in_place(|| SessionWriter::prepare_resume(home, session_id))
        .map_err(|error| error.to_string())?;
    let model = request
        .model_override
        .clone()
        .or_else(|| writer.saved_model().map(str::to_owned))
        .or_else(|| request.primary.model.clone())
        .filter(|value| !value.is_empty())
        .ok_or("model name is required")?;
    let mut history = writer.take_history();
    let cache_scope = writer.session_id().or(request.state_scope.as_deref());
    let primary =
        request
            .primary
            .open_with_session(model.clone(), request.attempts, cache_scope)?;
    let summarizer = match &request.summary {
        Some(route) => Some(route.open(route.model.clone().unwrap_or(model), request.attempts)?),
        None => None,
    };
    let options = TurnOptions {
        single_query: false,
        journal: None,
        delegation: None,
        system_prompt_override: None,
        child: None,
        prefill: None,
        state_scope: request.state_scope.as_deref(),
        mcp: None,
        web: Some(&request.web),
        browser: Some(&request.browser),
        memory: Some(&request.memory),
        tools: &request.tools,
        root,
        profile: Some(home),
        session_id: writer.session_id(),
        max_turns: request.max_turns,
        approval_timeout_secs: request.approval_timeout_secs,
        terminal_approval_timeout_secs: request.terminal_approval_timeout_secs,
        terminal_local: request.terminal_local,
        compression: Some(&request.compression),
        keep_phone_numbers: request.keep_phone_numbers,
        stall_guards: request.stall_guards,
        intent_ack_continuation: &request.intent_ack_continuation,
        summarizer: summarizer.as_ref(),
        compaction_guard: Some(guard),
        terminal_env: Some(&request.terminal_env),
        clarify_timeout_secs: request.clarify_timeout_secs,
    };
    block_in_place(|| history.capture_prompt(&options, &request.prompt, &primary));
    match history
        .compact(
            Some(home),
            root,
            &request.prompt,
            &request.compression,
            &primary,
            summarizer.as_ref(),
            CompactionTrigger::Manual { focus },
            Some(guard),
        )
        .await
    {
        CompactionOutcome::Applied => {
            let archived = block_in_place(|| writer.save_compaction(&history))
                .map_err(|error| error.to_string())?;
            Ok(ManualCompress::Applied { archived })
        }
        CompactionOutcome::Unchanged => Ok(ManualCompress::Unchanged),
        CompactionOutcome::Skipped => Ok(ManualCompress::Skipped),
    }
}
