use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hermes_runtime::config::{profile_home, set_config_value};

mod approval;
mod chat;
#[cfg(unix)]
mod owner;
mod sessions;

#[derive(Parser)]
struct Cli {
    #[arg(short = 'p', long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect stored sessions in the selected profile.
    Sessions {
        #[command(subcommand)]
        command: sessions::Command,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Chat(chat::ChatArgs),
    /// Run the local session owner for the selected profile.
    #[cfg(unix)]
    Owner,
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
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<u8, String> {
    let Cli { profile, command } = Cli::parse();
    let home = profile_home(profile.as_deref())?;
    let home = home.as_deref();
    match command {
        Command::Sessions { command } => {
            sessions::run(home.ok_or("profile root is unavailable")?, command)?;
            Ok(0)
        }
        Command::Config {
            command: ConfigCommand::Set { key, value },
        } => {
            set_config_value(home.ok_or("profile root is unavailable")?, &key, &value)?;
            println!("Updated {key:?} in config.yaml");
            Ok(0)
        }
        Command::Chat(args) => {
            let request = chat::prepare(home, args)?;
            #[cfg(unix)]
            if let Some(home) = home {
                if let Some(connection) = owner::connect(home).await? {
                    return owner::forward(connection, request).await;
                }
            }
            tokio::select! {
                result = chat::execute(home, request, |event| chat::display(event).is_ok(), approval::prompt) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| format!("signal handler failed: {error}"))?;
                    eprintln!("interrupted");
                    Ok(130)
                }
            }
        }
        #[cfg(unix)]
        Command::Owner => {
            owner::serve(home.ok_or("profile root is unavailable")?).await?;
            Ok(0)
        }
    }
}
