//! `hsr browser` subcommand and interactive `/browser connect|status|disconnect` handler.

use std::path::Path;

use clap::Subcommand;
use hermes_runtime::browser_connect::{
    browser_status, connect_browser, disconnect_browser, BrowserConnectOptions,
};
use hermes_runtime::config::set_config_value;

use crate::ui::ReplState;

#[derive(Subcommand)]
pub enum Command {
    /// Discover, optionally launch, and connect to a Chromium DevTools (CDP) browser.
    Connect {
        /// Optional CDP URL (`ws://...`, `http://...`) or loopback port (e.g. `9222`).
        target: Option<String>,
        /// Launch a local Chromium-family browser with remote debugging enabled.
        #[arg(long)]
        launch: bool,
        /// Pass `--headless=new` when launching a browser.
        #[arg(long)]
        headless: bool,
        /// Explicit loopback port when launching a browser.
        #[arg(long)]
        port: Option<u16>,
        /// Persist the resolved WebSocket URL into `browser.cdp_url` in config.yaml.
        #[arg(long)]
        save: bool,
    },
    /// Show the current CDP browser connection status and open tabs.
    Status {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Disconnect the active CDP supervisor and stop any launched browser process.
    Disconnect,
}

pub async fn run(home: Option<&Path>, command: Option<Command>) -> Result<u8, String> {
    let configured = hermes_runtime::config::load_chat_settings(home)
        .ok()
        .and_then(|c| c.browser.cdp_url);
    match command.unwrap_or(Command::Status { json: false }) {
        Command::Connect {
            target,
            launch,
            headless,
            port,
            save,
        } => {
            let info = connect_browser(
                "cli",
                home,
                configured.as_deref(),
                BrowserConnectOptions {
                    target,
                    launch,
                    headless,
                    port,
                },
            )
            .await?;
            if save {
                let h = home.ok_or("profile root is unavailable; cannot save browser.cdp_url")?;
                set_config_value(h, "browser.cdp_url", &info.ws_url)?;
            }
            println!("{}", format_connect_summary(&info, save));
            // For standalone `hsr browser connect` without `--launch`, disconnect the temporary
            // supervisor cleanly before exiting; if `--launch` was used, save or print URL.
            if !info.launched {
                let _ = disconnect_browser("cli").await;
            }
            Ok(0)
        }
        Command::Status { json: as_json } => {
            let st = browser_status("cli", configured.as_deref()).await;
            if as_json {
                let text = serde_json::to_string_pretty(&st)
                    .map_err(|e| format!("failed formatting browser status JSON: {e}"))?;
                println!("{text}");
                return Ok(0);
            }
            println!("{}", format_status_summary(&st));
            Ok(0)
        }
        Command::Disconnect => {
            let had = disconnect_browser("cli").await;
            if had {
                println!("Disconnected browser session.");
            } else {
                println!("No active browser session.");
            }
            Ok(0)
        }
    }
}

fn format_connect_summary(
    info: &hermes_runtime::browser_connect::BrowserConnectionInfo,
    saved: bool,
) -> String {
    let ver = info.browser_version.as_deref().unwrap_or("Chromium CDP");
    let launch_note = match (info.launched, info.pid) {
        (true, Some(pid)) => format!(" (launched pid {pid})"),
        _ => String::new(),
    };
    let save_note = if saved {
        " Saved browser.cdp_url to config.yaml."
    } else {
        ""
    };
    format!(
        "Connected to {ver}{launch_note} at {} ({} page tab(s)). Tools active: browser_cdp, browser_dialog, browser_exec, browser_vault (deferred).{save_note}",
        info.ws_url, info.page_count
    )
}

fn format_status_summary(st: &hermes_runtime::browser_connect::BrowserStatusInfo) -> String {
    if st.connected {
        let mut lines = vec![format!(
            "Browser connected: {} ({} page tab(s), {} pending dialog(s))",
            st.ws_url.as_deref().unwrap_or("(unknown)"),
            st.page_tabs.len(),
            st.pending_dialogs
        )];
        if let Some(pid) = st.spawned_pid {
            lines.push(format!("  Managed browser PID: {pid}"));
        }
        for (idx, tab) in st.page_tabs.iter().take(10).enumerate() {
            let title = if tab.title.is_empty() {
                "(untitled)"
            } else {
                tab.title.as_str()
            };
            lines.push(format!("  [{}] {} — {}", idx + 1, title, tab.url));
        }
        lines.join("\n")
    } else if let Some(cfg) = st.configured_url.as_deref() {
        format!(
            "Browser not attached (configured endpoint: {cfg}). Run `/browser connect` to attach."
        )
    } else {
        "Browser not connected. Run `/browser connect [url|port|--launch]` to attach.".to_owned()
    }
}

/// Execute `/browser [connect|status|disconnect ...]` inside an interactive REPL or TUI session.
/// Returns `(is_error, message)`.
pub async fn handle_slash_browser(
    home: Option<&Path>,
    state: &mut ReplState,
    argument: &str,
) -> (bool, String) {
    let mut tokens = argument.split_whitespace();
    let sub = tokens.next().unwrap_or("status").to_ascii_lowercase();
    match sub.as_str() {
        "status" => {
            let st = browser_status(&state.scope, state.request.browser_cdp_url()).await;
            (false, format_status_summary(&st))
        }
        "disconnect" => {
            let had = disconnect_browser(&state.scope).await;
            state.request.set_browser_cdp_url(None);
            if had {
                (false, "Disconnected live browser session and disabled dynamic CDP tools for subsequent turns.".into())
            } else {
                (false, "No live browser session was attached.".into())
            }
        }
        "connect" => {
            let mut opts = BrowserConnectOptions::default();
            let mut save = false;
            let rest: Vec<&str> = tokens.collect();
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "--launch" => opts.launch = true,
                    "--headless" => {
                        opts.launch = true;
                        opts.headless = true;
                    }
                    "--save" => save = true,
                    "--port" => {
                        if let Some(val) = rest.get(i + 1) {
                            match val.parse::<u16>() {
                                Ok(p) => {
                                    opts.port = Some(p);
                                    i += 1;
                                }
                                Err(_) => {
                                    return (true, format!("Invalid port value '{val}'."));
                                }
                            }
                        } else {
                            return (true, "Missing port number after `--port`.".into());
                        }
                    }
                    other if other.starts_with("--") => {
                        return (
                            true,
                            format!("Unknown /browser connect flag '{other}'. Expected: --launch, --headless, --port <N>, --save."),
                        );
                    }
                    other => {
                        if opts.target.is_none() {
                            opts.target = Some(other.to_owned());
                        }
                    }
                }
                i += 1;
            }
            let configured = state.request.browser_cdp_url().map(str::to_owned);
            match connect_browser(&state.scope, home, configured.as_deref(), opts).await {
                Ok(info) => {
                    state.request.set_browser_cdp_url(Some(info.ws_url.clone()));
                    let mut saved = false;
                    if save {
                        if let Some(h) = home {
                            if let Err(err) = set_config_value(h, "browser.cdp_url", &info.ws_url) {
                                return (
                                    true,
                                    format!(
                                        "{}\nWarning: failed saving browser.cdp_url: {err}",
                                        format_connect_summary(&info, false)
                                    ),
                                );
                            }
                            saved = true;
                        }
                    }
                    (false, format_connect_summary(&info, saved))
                }
                Err(err) => (true, format!("Browser connect failed: {err}")),
            }
        }
        other => (
            true,
            format!("Unknown /browser subcommand '{other}'. Usage: /browser [connect [url|port|--launch|--headless|--save] | status | disconnect]"),
        ),
    }
}
