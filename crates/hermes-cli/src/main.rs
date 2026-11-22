use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hermes_runtime::config::{profile_home, set_config_value};

mod approval;
mod chat;
mod cron;
#[cfg(unix)]
mod owner;
#[cfg(target_os = "linux")]
mod profile;
mod repl;
mod sessions;

#[derive(Parser)]
struct Cli {
    #[arg(short = 'p', long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
// The cron subcommand carries every job flag, so it is by far the largest variant.
#[allow(clippy::large_enum_variant)]
enum Command {
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
    match outcome {
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
            let request = chat::prepare(home, args)?;
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
                if let Some(connection) = owner::connect(home).await? {
                    return owner::forward(connection, request).await;
                }
            }
            tokio::select! {
                result = chat::execute(home, request, |event| chat::display(event).is_ok(), approval::prompt) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| format!("signal handler failed: {error}"))?;
                    // The turn was dropped: tell the MCP servers its calls are cancelled.
                    hermes_runtime::mcp::flush_cancellations().await;
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
