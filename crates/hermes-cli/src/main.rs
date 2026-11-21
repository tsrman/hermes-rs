use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use hermes_runtime::config::{load_chat_settings, profile_home, set_config_value};
use hermes_runtime::providers::openai::OpenAiProvider;
use hermes_runtime::turn::{run_turn, ModelRoute, TurnEvent, TurnOutcome};

#[derive(Parser)]
struct Cli {
    #[arg(short = 'p', long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Chat {
        #[arg(short, long)]
        query: String,
        #[arg(long)]
        oneshot: bool,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        max_turns: Option<u32>,
        #[arg(long, env = "HERMES_BASE_URL")]
        base_url: Option<String>,
        #[arg(long, env = "HERMES_MODEL")]
        model: Option<String>,
        #[arg(long, env = "HERMES_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Set a value in the selected profile's config.yaml.
    Set {
        key: String,
        #[arg(allow_negative_numbers = true)]
        value: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, String> {
    let Cli { profile, command } = Cli::parse();
    let home = profile_home(profile.as_deref())?;
    let (query, max_turns, base_url, model, api_key) = match command {
        Command::Config {
            command: ConfigCommand::Set { key, value },
        } => {
            let home = home.as_deref().ok_or("profile root is unavailable")?;
            set_config_value(home, &key, &value)?;
            println!("Updated {key:?} in config.yaml");
            return Ok(ExitCode::SUCCESS);
        }
        Command::Chat {
            query,
            oneshot: _,
            max_turns,
            base_url,
            model,
            api_key,
        } => (query, max_turns, base_url, model, api_key),
    };
    let settings = load_chat_settings(home.as_deref())?;
    let base_url = base_url
        .or(settings.base_url.clone())
        .filter(|value| !value.is_empty())
        .ok_or("model base URL is required")?;
    let openai_api_key = settings.openai_key_for(&base_url);
    let model = model
        .or(settings.model)
        .filter(|value| !value.is_empty())
        .ok_or("model name is required")?;
    let api_key = api_key
        .or(settings.api_key)
        .or(openai_api_key)
        .filter(|value| !value.is_empty())
        .ok_or("API key is required")?;
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    let timeout = Duration::from_secs(120);
    let attempts = settings.api_max_attempts;
    let max_turns = max_turns.unwrap_or(settings.max_turns);
    let primary = ModelRoute {
        model,
        provider_name: settings.provider,
        provider: OpenAiProvider::new(&base_url, &api_key, timeout)
            .map_err(|error| format!("provider setup failed: {error:?}"))?
            .with_max_attempts(attempts),
    };
    let fallbacks: Vec<_> = settings
        .fallbacks
        .into_iter()
        .filter_map(|config| {
            let provider = OpenAiProvider::new(
                &config.base_url,
                config.api_key.as_deref().unwrap_or(""),
                timeout,
            )
            .ok()?;
            Some(ModelRoute {
                model: config.model,
                provider_name: config.provider,
                provider: provider.with_max_attempts(attempts),
            })
        })
        .collect();
    let turn = run_turn(
        &primary,
        &fallbacks,
        &query,
        &root,
        max_turns,
        |event| match event {
            TurnEvent::Text(text) => {
                print!("{text}");
                let _ = io::stdout().flush();
            }
            TurnEvent::Fallback(model) => eprintln!("Model fallback: {model:?}"),
            TurnEvent::ToolStarted(name) => eprintln!("Tool started: {name:?}"),
            TurnEvent::ToolFinished { name, succeeded } => {
                let status = if succeeded { "completed" } else { "failed" };
                eprintln!("Tool {status}: {name:?}");
            }
            TurnEvent::IterationLimit(limit) => {
                eprintln!("Iteration limit reached ({limit}). Requesting summary.")
            }
        },
    );
    let outcome = tokio::select! {
        result = turn => result.map_err(|error| format!("turn failed: {error:?}"))?,
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|error| format!("signal handler failed: {error}"))?;
            eprintln!("interrupted");
            return Ok(ExitCode::from(130));
        }
    };
    println!();
    Ok(match outcome {
        TurnOutcome::Completed => ExitCode::SUCCESS,
        TurnOutcome::IterationLimit => ExitCode::FAILURE,
    })
}
