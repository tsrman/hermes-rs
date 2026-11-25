use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hermes_runtime::config::{profile_home, set_config_value};

mod approval;
mod auth;
mod browser;
mod chat;
mod cron;
mod owner;
#[cfg(target_os = "linux")]
mod profile;
mod repl;
mod sessions;
mod setup;
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
    /// Interactive setup wizard: initialize profile directories, provider, model, and secrets.
    Setup(setup::SetupArgs),
    /// Show or switch the default LLM provider and model.
    Model(setup::ModelArgs),
    /// Diagnose profile directories, config.yaml, credentials, database, and local owner.
    Doctor(setup::DoctorArgs),
    /// Download, sync, list, or opt-out of bundled Markdown skills in the selected profile.
    Skills {
        #[command(subcommand)]
        command: Option<setup::SkillsCommand>,
    },
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

/// Entrypoint for the standalone `hsr` CLI executable.
pub fn run_cli() -> ExitCode {
    #[cfg(not(target_os = "macos"))]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "__embedded-python")
    {
        return match hermes_runtime::embedded_python::run_stdio_worker() {
            Ok(code) => ExitCode::from(code),
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    #[cfg(not(target_os = "macos"))]
    hermes_runtime::embedded_python::enable_subprocess_worker();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("failed to start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async_main())
}

/// Handle internal background subcommands (`owner`, `__embedded-python`) when `hermes-cli` is
/// embedded inside a standalone GUI binary (`hsr-gui`). Returns `None` when the process should
/// proceed with normal GUI startup.
pub fn try_run_embedded_subcommand() -> Option<ExitCode> {
    let first = std::env::args_os().nth(1)?;
    #[cfg(not(target_os = "macos"))]
    if first == "__embedded-python" {
        return Some(match hermes_runtime::embedded_python::run_stdio_worker() {
            Ok(code) => ExitCode::from(code),
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        });
    }
    if first == "owner" {
        return Some(run_embedded_owner());
    }
    None
}

fn run_embedded_owner() -> ExitCode {
    #[cfg(not(target_os = "macos"))]
    hermes_runtime::embedded_python::enable_subprocess_worker();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("failed to start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let outcome: Result<(), String> = async {
            let home = profile_home(None)?;
            let home = home.as_deref().ok_or("profile root is unavailable")?;
            owner::serve(home).await
        }
        .await;
        hermes_runtime::process::kill_all();
        hermes_runtime::execute_code::kill_all_kernels();
        hermes_runtime::browser_connect::kill_spawned_browsers();
        match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        }
    })
}

async fn async_main() -> ExitCode {
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
        Command::Setup(args) => {
            setup::run_setup(home.ok_or("profile root is unavailable")?, args).await
        }
        Command::Model(args) => {
            setup::run_model(home.ok_or("profile root is unavailable")?, args).await
        }
        Command::Doctor(args) => {
            setup::run_doctor(home.ok_or("profile root is unavailable")?, args).await
        }
        Command::Skills { command } => {
            setup::run_skills(home.ok_or("profile root is unavailable")?, command).await
        }
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
        Command::Owner => {
            owner::serve(home.ok_or("profile root is unavailable")?).await?;
            Ok(0)
        }
    }
}
