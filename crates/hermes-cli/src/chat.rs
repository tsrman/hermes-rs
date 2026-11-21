use std::ffi::OsString;
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use clap::Args;
use hermes_runtime::config::{load_chat_settings, CompactionSettings};
use hermes_runtime::mcp::{McpClients, McpServerConfig};
use hermes_runtime::memory_tool::MemorySettings;
use hermes_runtime::prompt::PromptSettings;
use hermes_runtime::providers::{
    AnthropicProvider, OpenAiProvider, Provider, ProviderError, ResponsesProvider,
};
use hermes_runtime::storage::SessionWriter;
use hermes_runtime::tool_policy::ToolPolicy;
use hermes_runtime::turn::{
    run_turn, ApprovalDecision, ApprovalRequest, CompactionOutcome, CompactionTrigger, ModelRoute,
    TurnError, TurnEvent, TurnOptions, TurnOutcome,
};
use hermes_runtime::web_tools::WebSettings;
use serde::{Deserialize, Serialize};
use tokio::task::block_in_place;
use tokio::time::{interval_at, Instant};

#[derive(Args)]
pub struct ChatArgs {
    #[arg(short, long)]
    query: String,
    #[arg(long)]
    oneshot: bool,
    /// Save a completed turn as a new session in the selected profile.
    #[arg(long)]
    save_session: bool,
    /// Continue a saved session and append a completed turn.
    #[arg(short = 'r', long, conflicts_with = "save_session")]
    resume: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: Option<u32>,
    #[arg(long, env = "HERMES_BASE_URL")]
    base_url: Option<String>,
    #[arg(long, env = "HERMES_MODEL")]
    model: Option<String>,
    #[arg(long, env = "HERMES_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct ChatRequest {
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
    approval_timeout_secs: Option<u32>,
    #[serde(default)]
    terminal_local: bool,
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    web: WebSettings,
    #[serde(default)]
    memory: MemorySettings,
    #[serde(default)]
    tools: ToolPolicy,
    model_override: Option<String>,
    primary: Route,
    fallbacks: Vec<Route>,
}

#[derive(Deserialize, Serialize)]
struct Route {
    model: Option<String>,
    provider: String,
    #[serde(default)]
    responses: bool,
    base_url: String,
    api_key: String,
}

impl Route {
    fn open(&self, model: String, attempts: u32) -> Result<ModelRoute, String> {
        let provider_name = match self.provider.as_str() {
            "custom" => "custom",
            "openai" => "openai",
            "anthropic" => "anthropic",
            _ => return Err("unsupported provider".into()),
        };
        let timeout = Duration::from_secs(120);
        let setup = |error| format!("provider setup failed: {error:?}");
        let provider = if provider_name == "anthropic" {
            Provider::Anthropic(
                AnthropicProvider::new(&self.base_url, &self.api_key, timeout)
                    .map_err(setup)?
                    .with_max_attempts(attempts),
            )
        } else if self.responses {
            Provider::Responses(
                ResponsesProvider::new(&self.base_url, &self.api_key, timeout)
                    .map_err(setup)?
                    .with_max_attempts(attempts),
            )
        } else {
            Provider::OpenAi(
                OpenAiProvider::new(&self.base_url, &self.api_key, timeout)
                    .map_err(setup)?
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

pub fn prepare(home: Option<&Path>, args: ChatArgs) -> Result<ChatRequest, String> {
    let settings = load_chat_settings(home)?;
    let base_url = args
        .base_url
        .or_else(|| settings.base_url.clone())
        .filter(|value| !value.is_empty())
        .ok_or("model base URL is required")?;
    let vendor_api_key = settings.vendor_key_for(settings.provider, &base_url);
    let api_key = args
        .api_key
        .or(settings.api_key)
        .or(vendor_api_key)
        .filter(|value| !value.is_empty())
        .ok_or("API key is required")?;
    Ok(ChatRequest {
        root: std::env::current_dir()
            .map_err(|error| error.to_string())?
            .into_os_string(),
        query: args.query,
        resume: args.resume,
        save_session: args.save_session,
        max_turns: args.max_turns.unwrap_or(settings.max_turns),
        attempts: settings.api_max_attempts,
        prompt: settings.prompt,
        compression: settings.compression,
        approval_timeout_secs: (!args.oneshot && crate::approval::available())
            .then_some(settings.approval_timeout_secs),
        terminal_local: settings.terminal_local,
        mcp_servers: settings.mcp_servers,
        web: settings.web,
        memory: settings.memory,
        tools: settings.tools,
        model_override: args.model,
        primary: Route {
            model: settings.model,
            provider: settings.provider.into(),
            responses: settings.responses,
            base_url,
            api_key,
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
            })
            .collect(),
    })
}

pub async fn execute<A, F>(
    home: Option<&Path>,
    request: ChatRequest,
    mut emit: impl FnMut(Event) -> bool,
    approve: A,
) -> Result<u8, String>
where
    A: FnMut(ApprovalRequest) -> F,
    F: Future<Output = ApprovalDecision>,
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
    let primary = request.primary.open(model, request.attempts)?;
    let (clients, warnings) = McpClients::start(&request.mcp_servers).await;
    for warning in warnings {
        emit(Event::Stderr(warning));
    }
    let mcp = tokio::sync::Mutex::new(clients);
    let options = TurnOptions {
        mcp: Some(&mcp),
        web: Some(&request.web),
        memory: Some(&request.memory),
        tools: &request.tools,
        root,
        profile: home,
        max_turns: request.max_turns,
        approval_timeout_secs: request.approval_timeout_secs,
        terminal_local: request.terminal_local,
    };
    block_in_place(|| history.capture_prompt(&options, &request.prompt, &primary));
    let fallbacks = request
        .fallbacks
        .iter()
        .filter_map(|route| route.open(route.model.clone()?, request.attempts).ok())
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
                    CompactionTrigger::Estimate {
                        query: &request.query,
                    },
                )
                .await;
            let mut retried = false;
            loop {
                if let CompactionOutcome::Skipped = compaction {
                    receiving = emit(Event::Stderr(
                        "Context compression skipped: no usable shorter context.".into(),
                    ));
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
                            receiving = emit(format_event(event));
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
                        CompactionTrigger::Overflow,
                    )
                    .await;
                if !matches!(compaction, CompactionOutcome::Applied) {
                    break result;
                }
                receiving = emit(Event::Stderr(
                    "Context window exceeded: retrying with compressed history.".into(),
                ));
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
    if !receiving || !emit(Event::Stdout("\n".into())) {
        return Err("chat output was interrupted".into());
    }
    match outcome {
        TurnOutcome::Completed(turn) => {
            if let Some(mut writer) = writer {
                let id = block_in_place(|| writer.save(&turn, root))
                    .map_err(|error| error.to_string())?;
                emit(Event::Stderr(format!("Session: {id}")));
            }
            Ok(0)
        }
        TurnOutcome::IterationLimit => Ok(1),
    }
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
    }
}
