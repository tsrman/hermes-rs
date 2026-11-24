//! `hsr auth`: interactive OAuth login (browser PKCE / device-code), status, refresh, and logout.

use std::io::{self, BufRead, Write};
use std::path::Path;

use clap::Subcommand;
use hermes_runtime::providers::oauth_login::{
    self, canonical_login_provider, LoginOptions, LoginPrompt, SUPPORTED_LOGIN_PROVIDERS,
};

#[derive(Subcommand)]
pub enum Command {
    /// Authenticate with an OAuth provider (openai-codex, nous, xai-oauth, qwen-oauth, minimax-oauth, anthropic).
    Login {
        /// Provider name (e.g. openai-codex, codex, nous, xai-oauth, qwen-oauth, minimax-oauth, anthropic).
        provider: String,
        /// Use headless RFC 8628 device-code login instead of a local browser callback.
        #[arg(long)]
        device: bool,
        /// Print the login URL without attempting to open a browser automatically.
        #[arg(long)]
        no_browser: bool,
        /// Local callback port for browser PKCE login (0 = automatic ephemeral port).
        #[arg(long)]
        port: Option<u16>,
        /// Region for MiniMax OAuth (`global` or `cn`).
        #[arg(long)]
        region: Option<String>,
        /// Custom portal base URL override (Nous or MiniMax).
        #[arg(long)]
        portal_url: Option<String>,
        /// Maximum seconds to wait for authorization completion.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Token or setup key (used for `anthropic` login).
        #[arg(long)]
        token: Option<String>,
    },
    /// Show OAuth credential status for all providers (never prints secret tokens).
    Status {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refresh stored OAuth credentials for a provider.
    Refresh {
        /// Provider name (e.g. openai-codex, nous, xai-oauth, qwen-oauth, minimax-oauth).
        provider: String,
    },
    /// Remove stored OAuth credentials from the profile's auth.json.
    Logout {
        /// Provider name to remove (omit with `--all` to clear all providers).
        provider: Option<String>,
        /// Remove all stored OAuth providers from auth.json.
        #[arg(long)]
        all: bool,
    },
}

pub async fn run(home: &Path, command: Option<Command>) -> Result<u8, String> {
    match command.unwrap_or(Command::Status { json: false }) {
        Command::Status { json: as_json } => {
            let items = oauth_login::status(home);
            if as_json {
                let text = serde_json::to_string_pretty(&items)
                    .map_err(|e| format!("failed to format auth status JSON: {e}"))?;
                println!("{text}");
                return Ok(0);
            }
            println!(
                "{:<16} {:<10} {:<24} EXPIRES",
                "PROVIDER", "STATUS", "SOURCE"
            );
            for item in &items {
                let state = if !item.authenticated {
                    "not set"
                } else if item.expired {
                    "expired"
                } else {
                    "active"
                };
                let exp = item.expires_at.as_deref().unwrap_or("-");
                println!(
                    "{:<16} {:<10} {:<24} {}",
                    item.provider, state, item.source, exp
                );
            }
            Ok(0)
        }
        Command::Login {
            provider,
            device,
            no_browser,
            port,
            region,
            portal_url,
            timeout,
            mut token,
        } => {
            let Some(canonical) = canonical_login_provider(&provider) else {
                return Err(format!(
                    "Unsupported auth provider '{provider}'. Supported: {}",
                    SUPPORTED_LOGIN_PROVIDERS.join(", ")
                ));
            };
            if canonical == "anthropic" && token.is_none() {
                print!("Enter Anthropic OAuth/setup token or API key: ");
                let _ = io::stdout().flush();
                let mut line = String::new();
                io::stdin()
                    .lock()
                    .read_line(&mut line)
                    .map_err(|e| format!("failed reading token from stdin: {e}"))?;
                let trimmed = line.trim().to_owned();
                if !trimmed.is_empty() {
                    token = Some(trimmed);
                }
            }
            let opts = LoginOptions {
                provider,
                device_code: device,
                no_browser,
                port,
                region,
                portal_url,
                timeout_secs: timeout,
                token,
            };
            let result = oauth_login::login(home, opts, |prompt| match prompt {
                LoginPrompt::OpenUrl { url } => {
                    println!("Open this URL in your browser to sign in:\n  {url}\nWaiting for callback...");
                }
                LoginPrompt::DeviceCode {
                    verification_uri,
                    verification_uri_complete,
                    user_code,
                } => {
                    println!("Open: {verification_uri}");
                    println!("Code: {user_code}");
                    if let Some(full) = verification_uri_complete {
                        println!("Direct link: {full}");
                    }
                    println!("Waiting for authorization...");
                }
                LoginPrompt::Info(msg) => println!("{msg}"),
            })
            .await?;

            println!(
                "Authenticated '{}' -> {} (permissions 0600).",
                result.provider,
                result.auth_path.display()
            );
            if let Some(base) = result.base_url {
                println!("Endpoint: {base}");
            }
            if let Some(exp) = result.expires_at {
                println!("Expires:  {exp}");
            }
            Ok(0)
        }
        Command::Refresh { provider } => {
            let summary = oauth_login::refresh(home, &provider)?;
            println!("{summary}");
            Ok(0)
        }
        Command::Logout { provider, all } => {
            if !all && provider.is_none() {
                return Err(
                    "Specify a provider name or pass `--all` to log out everywhere.".into(),
                );
            }
            let target = if all { None } else { provider.as_deref() };
            let removed = oauth_login::logout(home, target)?;
            if removed.is_empty() {
                println!("No matching stored OAuth credentials to remove.");
            } else {
                println!("Removed OAuth credentials for: {}", removed.join(", "));
            }
            Ok(0)
        }
    }
}
