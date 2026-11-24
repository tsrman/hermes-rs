use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hermes_runtime::config::{profile_home, set_config_value};

mod approval;
mod auth;
mod browser;
mod chat;
mod cron;
#[cfg(unix)]
mod owner;
#[cfg(target_os = "linux")]
mod profile;
mod repl;
mod sessions;
mod ui;

#[derive(Parser)]
struct Cli {
    #[arg(short = 'p', long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    chat: chat::ChatArgs,
}

#[derive(Subcommand)]
// The cron subcommand carries every job flag, so it is by far the largest variant.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Authenticate with OAuth providers and inspect stored credentials.
    Auth {
        #[command(subcommand)]
        command: Option<auth::Command>,
    },
    /// Connect to, inspect, or disconnect a live CDP browser session.
    Browser {
        #[command(subcommand)]
        command: Option<browser::Command>,
    },
    /// Inspect stored sessions in the selected profile.
    Sessions {
        #[command(subcommand)]
        command: sessions::Command,
    },
    /// Check a profile and prepare a copy for migration.
    #[cfg(target_os = "linux")]
    Profile {
        #[command(subcommand)]
        command: profile::Command,
    },
    /// Inspect scheduled jobs and preview schedules.
    Cron {
        #[command(subcommand)]
        command: cron::Command,
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
    let outcome = run().await;
    // Background processes belong to this run; none is left behind.
    hermes_runtime::process::kill_all();
    hermes_runtime::execute_code::kill_all_kernels();
    hermes_runtime::browser_connect::kill_spawned_browsers();
    match outcome {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<u8, String> {
    let Cli {
        profile,
        command,
        chat,
    } = Cli::parse();
    let home = profile_home(profile.as_deref())?;
    let home = home.as_deref();
    let command = command.unwrap_or(Command::Chat(chat));
    match command {
        Command::Auth { command } => {
            auth::run(home.ok_or("profile root is unavailable")?, command).await
        }
        Command::Browser { command } => browser::run(home, command).await,
        Command::Sessions { command } => {
            sessions::run(home.ok_or("profile root is unavailable")?, command)?;
            Ok(0)
        }
        #[cfg(target_os = "linux")]
        Command::Profile { command } => {
            profile::run(home.ok_or("profile root is unavailable")?, command)
        }
        Command::Cron { command } => cron::run(home, command).await,
        Command::Config {
            command: ConfigCommand::Set { key, value },
        } => {
            set_config_value(home.ok_or("profile root is unavailable")?, &key, &value)?;
            println!("Updated {key:?} in config.yaml");
            Ok(0)
        }
        Command::Chat(args) => {
            let events_json = args.events_json;
            let emit_json_err = |error: String| -> Result<u8, String> {
                if events_json {
                    chat::write_stdout_line(&chat::json_error(&error));
                    chat::write_stdout_line(&chat::json_done(1, "error"));
                    Ok(1)
                } else {
                    Err(error)
                }
            };
            let request = match chat::prepare(home, args) {
                Ok(request) => request,
                Err(error) => return emit_json_err(error),
            };
            approval::install(request.approvals().clone());
            if request.is_unattended(approval::available()) {
                if let Some(policy) = hermes_runtime::approval_policy::installed() {
                    policy.set_unattended(request.approvals().single_query_approves);
                }
            }
            if request.is_interactive() {
                return repl::run(home, request).await;
            }
            #[cfg(unix)]
            if let Some(home) = home {
                match owner::connect(home).await {
                    Ok(Some(connection)) => match owner::forward(connection, request).await {
                        Ok(code) => return Ok(code),
                        Err(error) => return emit_json_err(error),
                    },
                    Ok(None) => {}
                    Err(error) => return emit_json_err(error),
                }
            }
            let mut cancelled = false;
            let outcome = tokio::select! {
                result = chat::execute(home, request, |event| chat::display(event).is_ok(), approval::prompt) => {
                    match result {
                        Ok(code) => Ok(code),
                        Err(error) => emit_json_err(error),
                    }
                }
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| format!("signal handler failed: {error}"))?;
                    cancelled = true;
                    Ok(130)
                }
            };
            if cancelled {
                // The turn was dropped: tell the MCP servers its calls are cancelled.
                hermes_runtime::mcp::flush_cancellations().await;
                if events_json {
                    chat::write_stdout_line(&chat::json_cancelled(130));
                    chat::write_stdout_line(&chat::json_done(130, "cancelled"));
                } else {
                    eprintln!("interrupted");
                }
            }
            outcome
        }
        #[cfg(unix)]
        Command::Owner => {
            owner::serve(home.ok_or("profile root is unavailable")?).await?;
            Ok(0)
        }
    }
}
