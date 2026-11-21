use std::io::{self, Write};
use std::time::Duration;

use clap::{Parser, Subcommand};
use hermes_runtime::config::{load_chat_settings, profile_home};
use hermes_runtime::providers::openai::OpenAiProvider;
use hermes_runtime::turn::run_turn;

#[derive(Parser)]
struct Cli {
    #[arg(short = 'p', long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Chat {
        #[arg(short, long)]
        query: String,
        #[arg(long)]
        oneshot: bool,
        #[arg(long, env = "HERMES_BASE_URL")]
        base_url: Option<String>,
        #[arg(long, env = "HERMES_MODEL")]
        model: Option<String>,
        #[arg(long, env = "HERMES_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let Cli {
        profile,
        command:
            Command::Chat {
                query,
                oneshot: _,
                base_url,
                model,
                api_key,
            },
    } = Cli::parse();
    let home = profile_home(profile.as_deref())?;
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
    let provider = OpenAiProvider::new(&base_url, &api_key, Duration::from_secs(120))
        .map_err(|error| format!("provider setup failed: {error:?}"))?;
    let turn = run_turn(&provider, &model, &query, &root, |text| {
        print!("{text}");
        let _ = io::stdout().flush();
    });
    tokio::select! {
        result = turn => result.map_err(|error| format!("turn failed: {error:?}"))?,
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|error| format!("signal handler failed: {error}"))?;
            return Err("interrupted".into());
        }
    }
    println!();
    Ok(())
}
