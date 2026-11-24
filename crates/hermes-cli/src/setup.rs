//! `hsr setup`, `hsr model`, and `hsr doctor`: interactive configuration wizard, quick model
//! switcher, and profile diagnostics.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use hermes_runtime::config::{
    ensure_profile_scaffold, load_chat_settings, set_config_value, set_env_value, ChatSettings,
    PROFILE_DIRECTORIES,
};
use hermes_runtime::local_owner::{send_v2_command, GuiCommand, GuiReply};
use hermes_runtime::providers::oauth_login::{
    self, canonical_login_provider, LoginOptions, LoginPrompt,
};
use hermes_runtime::providers::profiles;
use hermes_runtime::skills_sync::{self, SkillsSyncOptions};
use hermes_runtime::storage::SessionWriter;
use serde::Serialize;

#[derive(Args)]
pub struct SetupArgs {
    /// Quick setup using Nous Portal OAuth login.
    #[arg(long)]
    pub portal: bool,
    /// Run without interactive prompts, applying flags and defaults.
    #[arg(long)]
    pub non_interactive: bool,
    /// Provider name (e.g. openrouter, nous, openai-codex, openai, anthropic, deepseek, xai, gemini, ollama, custom).
    #[arg(long)]
    pub provider: Option<String>,
    /// Default model identifier.
    #[arg(long)]
    pub model: Option<String>,
    /// Custom or override base URL (used for `custom`, `ollama`, or endpoint overrides).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Wire API mode for custom endpoints (`openai`, `responses`, or `anthropic`).
    #[arg(long)]
    pub api_mode: Option<String>,
    /// API key to store securely in `<profile>/.env` (`0600`).
    #[arg(long, hide_env_values = true)]
    pub api_key: Option<String>,
    /// Console UI mode (`auto`, `tui`, `inline`, or `plain`).
    #[arg(long)]
    pub ui: Option<String>,
    /// Optional web search backend (`none`, `tavily`, `brave`, `exa`, `firecrawl`, `searxng`, `ddgs`).
    #[arg(long)]
    pub web_backend: Option<String>,
    /// Optional API key (or SearXNG base URL) for the chosen web search backend.
    #[arg(long, hide_env_values = true)]
    pub web_api_key: Option<String>,
    /// Skip interactive OAuth login when configuring an OAuth provider.
    #[arg(long)]
    pub skip_oauth: bool,
    /// Use device-code OAuth flow instead of browser callback when signing in.
    #[arg(long)]
    pub device: bool,
    /// Print OAuth login URL instead of automatically launching a browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Skills seeding mode (`download`, `copy`, or `skip`).
    #[arg(long)]
    pub skills: Option<String>,
    /// Custom ZIP archive URL for bundled skills (defaults to official NousResearch/hermes-agent).
    #[arg(long)]
    pub skills_url: Option<String>,
    /// Local directory containing skills to copy into `<profile>/skills/`.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum SkillsCommand {
    /// Download or copy bundled skills into `<profile>/skills/` without overwriting local edits.
    Sync {
        /// Custom ZIP archive URL (defaults to official NousResearch/hermes-agent main archive).
        #[arg(long)]
        url: Option<String>,
        /// Copy skills from a local directory instead of downloading.
        #[arg(long)]
        from_dir: Option<PathBuf>,
        /// Reinstall deleted bundled skills and overwrite existing bundled skills.
        #[arg(long)]
        force: bool,
    },
    /// List installed skills in `<profile>/skills/`.
    List {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Opt out of automatic bundled skills seeding (creates `.no-bundled-skills`).
    OptOut,
    /// Re-enable bundled skills seeding (removes `.no-bundled-skills`, optionally syncing immediately).
    OptIn {
        /// Immediately download/sync bundled skills after opting in.
        #[arg(long)]
        sync: bool,
        /// Custom ZIP archive URL when used with `--sync`.
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Args)]
pub struct ModelArgs {
    /// Model specification (`model` or `provider:model`). Omit to launch the interactive picker.
    pub spec: Option<String>,
    /// Explicitly override provider name.
    #[arg(long)]
    pub provider: Option<String>,
    /// Explicitly override base URL (for `custom` / local endpoints).
    #[arg(long)]
    pub base_url: Option<String>,
}

#[derive(Args)]
pub struct DoctorArgs {
    /// Output machine-readable JSON diagnostics.
    #[arg(long)]
    pub json: bool,
    /// Create missing profile directories, starter files, and `state.db` before checking.
    #[arg(long)]
    pub fix: bool,
}

struct ProviderPreset {
    id: &'static str,
    label: &'static str,
    default_model: &'static str,
    key_env: Option<&'static str>,
    default_base_url: Option<&'static str>,
    keyless: bool,
}

const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter (200+ models via one API key)",
        default_model: "anthropic/claude-sonnet-4",
        key_env: Some("OPENROUTER_API_KEY"),
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "nous",
        label: "Nous Portal (OAuth login)",
        default_model: "hermes-4-70b",
        key_env: None,
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "openai-codex",
        label: "OpenAI Codex / ChatGPT (OAuth login)",
        default_model: "gpt-5.3-codex",
        key_env: None,
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "openai",
        label: "OpenAI API",
        default_model: "gpt-4.1",
        key_env: Some("OPENAI_API_KEY"),
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic Claude (API key or Claude Code OAuth token)",
        default_model: "claude-sonnet-4-20250514",
        key_env: Some("ANTHROPIC_API_KEY"),
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        default_model: "deepseek-chat",
        key_env: Some("DEEPSEEK_API_KEY"),
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "xai",
        label: "xAI Grok",
        default_model: "grok-4",
        key_env: Some("XAI_API_KEY"),
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "gemini",
        label: "Google Gemini",
        default_model: "gemini-2.5-pro",
        key_env: Some("GEMINI_API_KEY"),
        default_base_url: None,
        keyless: false,
    },
    ProviderPreset {
        id: "ollama",
        label: "Ollama (local OpenAI-compatible server, no key needed)",
        default_model: "qwen2.5-coder:14b",
        key_env: None,
        default_base_url: Some("http://127.0.0.1:11434/v1"),
        keyless: true,
    },
    ProviderPreset {
        id: "custom",
        label: "Custom endpoint (vLLM, LiteLLM, self-hosted gateway)",
        default_model: "gpt-4o-mini",
        key_env: Some("HERMES_API_KEY"),
        default_base_url: Some("http://127.0.0.1:8000/v1"),
        keyless: false,
    },
];

pub async fn run_setup(home: &Path, args: SetupArgs) -> Result<u8, String> {
    println!("Hermes Setup Wizard");
    println!("Profile: {}\n", home.display());

    // 1. Always initialize all required directories, starter files, and canonical state.db first.
    let scaffold = ensure_profile_scaffold(home)?;
    if !scaffold.created_dirs.is_empty() || !scaffold.created_files.is_empty() {
        println!(
            "Initialized profile layout: {} directories, {} starter files.",
            scaffold.created_dirs.len(),
            scaffold.created_files.len()
        );
    } else {
        println!("Profile directories and starter files are already in place.");
    }

    let current = load_chat_settings(Some(home)).ok();
    let existing_env = read_env_map(home);
    let interactive = !args.non_interactive;

    let stdin = io::stdin();
    let mut reader = stdin.lock();

    // 2. Provider selection
    let provider = if args.portal {
        "nous".to_owned()
    } else if let Some(explicit) = args.provider.as_deref() {
        normalize_provider(explicit)
    } else if interactive {
        prompt_provider(&mut reader, current.as_ref().map(|c| c.provider))?
    } else {
        current
            .as_ref()
            .map(|c| c.provider.to_owned())
            .unwrap_or_else(|| "openrouter".to_owned())
    };

    let preset = find_preset(&provider);
    let named_profile = profiles::lookup(&provider);
    let is_oauth = is_oauth_provider(&provider);
    let is_keyless = preset.is_some_and(|p| p.keyless);

    // 3. Credentials / OAuth / Endpoint
    let mut base_url = args.base_url.clone();
    let mut api_mode = args.api_mode.clone();

    if provider == "custom" || provider == "ollama" {
        let default_url = base_url
            .clone()
            .or_else(|| current.as_ref().and_then(|c| c.base_url.clone()))
            .or_else(|| preset.and_then(|p| p.default_base_url.map(str::to_owned)))
            .unwrap_or_else(|| "http://127.0.0.1:8000/v1".into());
        if base_url.is_none() && interactive {
            base_url = Some(prompt_line(&mut reader, "Endpoint base URL", &default_url)?);
        } else if base_url.is_none() {
            base_url = Some(default_url);
        }

        if provider == "custom" {
            let default_mode = api_mode.clone().unwrap_or_else(|| "openai".into());
            if api_mode.is_none() && interactive {
                let raw = prompt_line(
                    &mut reader,
                    "API mode (openai / responses / anthropic)",
                    &default_mode,
                )?;
                let mode = raw.trim().to_ascii_lowercase();
                if matches!(mode.as_str(), "openai" | "responses" | "anthropic") {
                    api_mode = Some(mode);
                }
            }
        }
    }

    let mut env_key_saved: Option<String> = None;
    if is_oauth {
        let already_authed = oauth_login::status(home)
            .into_iter()
            .any(|s| s.provider == provider && s.authenticated && !s.expired);
        let should_login = if args.skip_oauth {
            false
        } else if args.portal {
            true
        } else if interactive {
            let prompt = if already_authed {
                format!("OAuth credentials for '{provider}' are active. Re-authenticate now?")
            } else {
                format!("Sign in with '{provider}' via OAuth now?")
            };
            prompt_yes_no(&mut reader, &prompt, !already_authed)?
        } else {
            false
        };

        if should_login {
            let opts = LoginOptions {
                provider: provider.clone(),
                device_code: args.device,
                no_browser: args.no_browser,
                port: None,
                region: None,
                portal_url: base_url.clone(),
                timeout_secs: 300,
                token: args.api_key.clone(),
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
            println!("Authenticated '{}'.", result.provider);
        }
    } else {
        let key_env = key_env_for_provider(&provider, preset, named_profile);
        if let Some(var_name) = key_env {
            if let Some(explicit_key) = args
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let target_var =
                    if provider == "anthropic" && explicit_key.starts_with("sk-ant-oat") {
                        "ANTHROPIC_TOKEN"
                    } else {
                        var_name
                    };
                set_env_value(home, target_var, explicit_key)?;
                env_key_saved = Some(target_var.to_owned());
            } else if interactive && !is_keyless {
                let has_existing = existing_env.contains_key(var_name)
                    || std::env::var(var_name).is_ok_and(|v| !v.trim().is_empty());
                let label = if has_existing {
                    format!("{var_name} (press Enter to keep existing)")
                } else if provider == "custom" {
                    format!("{var_name} (optional for local servers)")
                } else {
                    var_name.to_owned()
                };
                let entered = prompt_line(&mut reader, &label, "")?;
                if !entered.trim().is_empty() {
                    let target_var =
                        if provider == "anthropic" && entered.trim().starts_with("sk-ant-oat") {
                            "ANTHROPIC_TOKEN"
                        } else {
                            var_name
                        };
                    set_env_value(home, target_var, entered.trim())?;
                    env_key_saved = Some(target_var.to_owned());
                }
            }
        }
    }

    // 4. Default model
    let default_model = args
        .model
        .clone()
        .or_else(|| {
            current
                .as_ref()
                .filter(|c| c.provider == provider)
                .and_then(|c| c.model.clone())
        })
        .or_else(|| preset.map(|p| p.default_model.to_owned()))
        .unwrap_or_else(|| "gpt-4o-mini".into());

    let model = if args.model.is_some() || !interactive {
        default_model
    } else {
        prompt_line(&mut reader, "Default model", &default_model)?
    };

    // 5. Console UI mode
    let ui_mode = if let Some(explicit) = args.ui.as_deref() {
        validate_ui_mode(explicit)?
    } else if interactive {
        let raw = prompt_line(
            &mut reader,
            "Console UI mode (auto / tui / inline / plain)",
            "auto",
        )?;
        validate_ui_mode(&raw)?
    } else {
        "auto".to_owned()
    };

    // 6. Memory preferences
    let (memory_enabled, user_profile_enabled) = if interactive {
        let enabled = prompt_yes_no(
            &mut reader,
            "Enable persistent curated memory (MEMORY.md & USER.md)?",
            true,
        )?;
        (enabled, enabled)
    } else {
        (true, true)
    };

    // 7. Optional Web Search backend
    let web_backend = if let Some(explicit) = args.web_backend.as_deref() {
        normalize_web_backend(explicit)?
    } else if interactive {
        let raw = prompt_line(
            &mut reader,
            "Web search backend (none / tavily / brave / exa / firecrawl / searxng / ddgs)",
            "none",
        )?;
        normalize_web_backend(&raw)?
    } else {
        None
    };

    let mut web_env_saved: Option<String> = None;
    if let Some(ref backend) = web_backend {
        if let Some(var_name) = web_env_for_backend(backend) {
            if let Some(explicit) = args
                .web_api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                set_env_value(home, var_name, explicit)?;
                web_env_saved = Some(var_name.to_owned());
            } else if interactive {
                let has_existing = existing_env.contains_key(var_name)
                    || std::env::var(var_name).is_ok_and(|v| !v.trim().is_empty());
                let prompt = if has_existing {
                    format!("{var_name} (press Enter to keep existing)")
                } else {
                    var_name.to_owned()
                };
                let entered = prompt_line(&mut reader, &prompt, "")?;
                if !entered.trim().is_empty() {
                    set_env_value(home, var_name, entered.trim())?;
                    web_env_saved = Some(var_name.to_owned());
                }
            }
        }
    }

    // 8. Skills seeding (download official bundled skills or copy from sibling profile)
    let mut skills_summary: Option<String> = None;
    let existing_skills = skills_sync::list_installed_skills(home);
    let sibling_skills = skills_sync::detect_sibling_skills_dir(home);

    let skills_action = if let Some(explicit) = args.skills.as_deref() {
        explicit.trim().to_ascii_lowercase()
    } else if args.skills_dir.is_some() {
        "copy".into()
    } else if args.skills_url.is_some() && !interactive {
        "download".into()
    } else if interactive {
        if skills_sync::is_opted_out(home) {
            "skip".into()
        } else if let Some((ref sibling_dir, sibling_count)) = sibling_skills {
            println!(
                "\nSkills directory: {} ({} installed)",
                home.join("skills").display(),
                existing_skills.len()
            );
            println!(
                "  1. Download official bundled skills from GitHub (NousResearch/hermes-agent)"
            );
            println!(
                "  2. Copy {sibling_count} skills from {}",
                sibling_dir.display()
            );
            println!("  3. Skip for now");
            let def = if existing_skills.is_empty() { "1" } else { "3" };
            let pick = prompt_line(&mut reader, "Skills setup [1-3]", def)?;
            match pick.trim() {
                "1" | "download" => "download".into(),
                "2" | "copy" => "copy".into(),
                _ => "skip".into(),
            }
        } else {
            let prompt = if existing_skills.is_empty() {
                "Download official bundled skills into skills/ from GitHub?"
            } else {
                "Sync missing official bundled skills into skills/ from GitHub?"
            };
            if prompt_yes_no(&mut reader, prompt, existing_skills.is_empty())? {
                "download".into()
            } else {
                "skip".into()
            }
        }
    } else {
        "skip".into()
    };

    match skills_action.as_str() {
        "download" => {
            println!(
                "Downloading bundled skills into {}...",
                home.join("skills").display()
            );
            match skills_sync::sync_skills(
                home,
                SkillsSyncOptions {
                    url: args.skills_url.clone(),
                    from_dir: None,
                    force: false,
                },
            )
            .await
            {
                Ok(rep) => {
                    skills_summary = Some(format!(
                        "{} installed, {} existing kept ({})",
                        rep.installed.len(),
                        rep.skipped_existing.len(),
                        rep.source
                    ));
                }
                Err(err) => {
                    eprintln!("Warning: could not download bundled skills ({err}).");
                    eprintln!("         You can retry later with `hsr skills sync`.");
                }
            }
        }
        "copy" => {
            let src = args
                .skills_dir
                .clone()
                .or_else(|| sibling_skills.map(|(dir, _)| dir));
            if let Some(src_dir) = src {
                match skills_sync::sync_skills(
                    home,
                    SkillsSyncOptions {
                        url: None,
                        from_dir: Some(src_dir),
                        force: false,
                    },
                )
                .await
                {
                    Ok(rep) => {
                        skills_summary = Some(format!(
                            "{} copied, {} existing kept (from {})",
                            rep.installed.len(),
                            rep.skipped_existing.len(),
                            rep.source
                        ));
                    }
                    Err(err) => {
                        eprintln!("Warning: could not copy skills ({err}).");
                    }
                }
            }
        }
        "skip" | "none" | "" => {}
        other => {
            return Err(format!(
                "invalid --skills mode '{other}' (expected download, copy, or skip)"
            ));
        }
    }

    // 9. Persist config.yaml atomically
    set_config_value(home, "model.provider", &provider)?;
    set_config_value(home, "model.default", &model)?;
    if let Some(ref url) = base_url {
        if !url.trim().is_empty() {
            set_config_value(home, "model.base_url", url.trim())?;
        }
    }
    if let Some(ref mode) = api_mode {
        set_config_value(home, "model.api_mode", mode)?;
    }
    set_config_value(home, "ui.mode", &ui_mode)?;
    set_config_value(
        home,
        "memory.memory_enabled",
        if memory_enabled { "true" } else { "false" },
    )?;
    set_config_value(
        home,
        "memory.user_profile_enabled",
        if user_profile_enabled {
            "true"
        } else {
            "false"
        },
    )?;
    if let Some(ref backend) = web_backend {
        set_config_value(home, "web.search_backend", backend)?;
    }

    println!("\nSetup complete!");
    println!("  Profile:  {}", home.display());
    println!("  Config:   {}", home.join("config.yaml").display());
    println!("  Provider: {provider}");
    println!("  Model:    {model}");
    if let Some(ref url) = base_url {
        println!("  Base URL: {url}");
    }
    println!("  UI mode:  {ui_mode}");
    if let Some(var) = env_key_saved {
        println!("  Secret:   saved {var} in .env (0600)");
    }
    if let Some(ref backend) = web_backend {
        println!("  Web:      {backend}");
    }
    if let Some(var) = web_env_saved {
        println!("  Web env:  saved {var} in .env (0600)");
    }
    if let Some(ref summary) = skills_summary {
        println!("  Skills:   {summary}");
    }
    println!("\nNext steps:");
    println!("  hsr doctor        # verify configuration, directories & skills");
    println!("  hsr skills sync   # download or refresh official bundled skills");
    println!("  hsr               # start interactive chat");
    Ok(0)
}

pub async fn run_skills(home: &Path, command: Option<SkillsCommand>) -> Result<u8, String> {
    match command.unwrap_or(SkillsCommand::List { json: false }) {
        SkillsCommand::List { json: as_json } => {
            let skills = skills_sync::list_installed_skills(home);
            if as_json {
                let out = serde_json::to_string_pretty(&skills)
                    .map_err(|e| format!("failed formatting skills JSON: {e}"))?;
                println!("{out}");
            } else if skills.is_empty() {
                println!(
                    "No skills installed in {}. Run `hsr skills sync` to download official bundled skills.",
                    home.join("skills").display()
                );
            } else {
                println!(
                    "Installed skills in {} ({}):",
                    home.join("skills").display(),
                    skills.len()
                );
                for skill in skills {
                    println!("  - {skill}");
                }
            }
            Ok(0)
        }
        SkillsCommand::Sync {
            url,
            from_dir,
            force,
        } => {
            let _ = ensure_profile_scaffold(home)?;
            let report = skills_sync::sync_skills(
                home,
                SkillsSyncOptions {
                    url,
                    from_dir,
                    force,
                },
            )
            .await?;
            if report.opted_out {
                println!(
                    "Bundled skills sync is opted out (.no-bundled-skills). Run `hsr skills opt-in --sync` to re-enable."
                );
                return Ok(0);
            }
            println!(
                "Skills synced from {}: {} installed, {} updated, {} kept, {} deleted skipped.",
                report.source,
                report.installed.len(),
                report.updated.len(),
                report.skipped_existing.len(),
                report.skipped_deleted.len()
            );
            Ok(0)
        }
        SkillsCommand::OptOut => {
            skills_sync::set_opt_out(home, true)?;
            println!(
                "Opted out of bundled skills seeding for {}.",
                home.display()
            );
            Ok(0)
        }
        SkillsCommand::OptIn { sync, url } => {
            skills_sync::set_opt_out(home, false)?;
            println!("Re-enabled bundled skills seeding for {}.", home.display());
            if sync {
                let _ = ensure_profile_scaffold(home)?;
                let report = skills_sync::sync_skills(
                    home,
                    SkillsSyncOptions {
                        url,
                        from_dir: None,
                        force: false,
                    },
                )
                .await?;
                println!(
                    "Skills synced: {} installed, {} kept.",
                    report.installed.len(),
                    report.skipped_existing.len()
                );
            }
            Ok(0)
        }
    }
}

pub async fn run_model(home: &Path, args: ModelArgs) -> Result<u8, String> {
    let _ = ensure_profile_scaffold(home)?;
    let current = load_chat_settings(Some(home)).ok();

    if let Some(spec) = args.spec {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err("model specification cannot be empty".into());
        }
        let (provider, model) = if let Some((prov, rest)) = split_provider_model(trimmed) {
            (Some(prov), rest.to_owned())
        } else {
            (
                args.provider.map(|p| normalize_provider(&p)),
                trimmed.to_owned(),
            )
        };
        if let Some(ref prov) = provider {
            set_config_value(home, "model.provider", prov)?;
        }
        if let Some(ref url) = args.base_url {
            set_config_value(home, "model.base_url", url.trim())?;
        }
        set_config_value(home, "model.default", &model)?;
        if let Some(prov) = provider {
            println!("Updated model -> {prov}:{model}");
        } else {
            println!("Updated model.default -> {model}");
        }
        return Ok(0);
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let provider = if let Some(explicit) = args.provider {
        normalize_provider(&explicit)
    } else {
        prompt_provider(&mut reader, current.as_ref().map(|c| c.provider))?
    };
    let preset = find_preset(&provider);
    let default_model = current
        .as_ref()
        .filter(|c| c.provider == provider)
        .and_then(|c| c.model.clone())
        .or_else(|| preset.map(|p| p.default_model.to_owned()))
        .unwrap_or_else(|| "gpt-4o-mini".into());
    let model = prompt_line(&mut reader, "Model name", &default_model)?;

    set_config_value(home, "model.provider", &provider)?;
    if let Some(ref url) = args.base_url {
        set_config_value(home, "model.base_url", url.trim())?;
    } else if provider == "custom" || provider == "ollama" {
        let default_url = current
            .as_ref()
            .and_then(|c| c.base_url.clone())
            .or_else(|| preset.and_then(|p| p.default_base_url.map(str::to_owned)))
            .unwrap_or_else(|| "http://127.0.0.1:8000/v1".into());
        let url = prompt_line(&mut reader, "Endpoint base URL", &default_url)?;
        set_config_value(home, "model.base_url", url.trim())?;
    }
    set_config_value(home, "model.default", &model)?;
    println!("Updated default model -> {provider}:{model}");
    Ok(0)
}

#[derive(Serialize)]
struct DoctorCheck {
    name: String,
    status: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct DoctorReport {
    profile: String,
    ready: bool,
    checks: Vec<DoctorCheck>,
}

pub async fn run_doctor(home: &Path, args: DoctorArgs) -> Result<u8, String> {
    if args.fix {
        let _ = ensure_profile_scaffold(home)?;
    }

    let mut checks = Vec::new();
    let mut has_error = false;

    // 1. Profile directory & required subdirectories
    if !home.is_dir() {
        has_error = true;
        checks.push(DoctorCheck {
            name: "profile_dir".into(),
            status: "error",
            detail: format!(
                "{} does not exist (run `hsr setup` or `hsr doctor --fix`)",
                home.display()
            ),
        });
    } else {
        let missing: Vec<&str> = PROFILE_DIRECTORIES
            .iter()
            .map(|&(rel, _)| rel)
            .filter(|rel| !home.join(rel).is_dir())
            .collect();
        if missing.is_empty() {
            checks.push(DoctorCheck {
                name: "directories".into(),
                status: "ok",
                detail: format!(
                    "all {} runtime directories present",
                    PROFILE_DIRECTORIES.len()
                ),
            });
        } else {
            checks.push(DoctorCheck {
                name: "directories".into(),
                status: "warn",
                detail: format!(
                    "missing {} directories ({}); run `hsr doctor --fix`",
                    missing.len(),
                    missing.join(", ")
                ),
            });
        }
    }

    // 2. config.yaml & provider/model/credentials
    let config_path = home.join("config.yaml");
    if !config_path.is_file() {
        has_error = true;
        checks.push(DoctorCheck {
            name: "config".into(),
            status: "error",
            detail: "config.yaml missing (run `hsr setup`)".into(),
        });
    } else {
        match load_chat_settings(Some(home)) {
            Ok(settings) => {
                let model_ok = settings
                    .model
                    .as_deref()
                    .is_some_and(|m| !m.trim().is_empty());
                if !model_ok {
                    has_error = true;
                    checks.push(DoctorCheck {
                        name: "model".into(),
                        status: "error",
                        detail: format!(
                            "provider '{}' has no model.default set (run `hsr setup` or `hsr model`)",
                            settings.provider
                        ),
                    });
                } else {
                    checks.push(DoctorCheck {
                        name: "model".into(),
                        status: "ok",
                        detail: format!(
                            "{}:{}",
                            settings.provider,
                            settings.model.as_deref().unwrap_or("")
                        ),
                    });
                }

                let (cred_ok, cred_detail) = check_provider_credentials(home, &settings);
                if !cred_ok {
                    has_error = true;
                }
                checks.push(DoctorCheck {
                    name: "credentials".into(),
                    status: if cred_ok { "ok" } else { "error" },
                    detail: cred_detail,
                });
            }
            Err(err) => {
                has_error = true;
                checks.push(DoctorCheck {
                    name: "config".into(),
                    status: "error",
                    detail: format!("config.yaml error: {err}"),
                });
            }
        }
    }

    // 3. state.db
    let db_path = home.join("state.db");
    if !db_path.is_file() {
        checks.push(DoctorCheck {
            name: "state_db".into(),
            status: "warn",
            detail: "state.db not initialized yet (run `hsr doctor --fix` or start a chat)".into(),
        });
    } else {
        match SessionWriter::prepare(home) {
            Ok(_) => checks.push(DoctorCheck {
                name: "state_db".into(),
                status: "ok",
                detail: "state.db schema v33 + FTS3 ready".into(),
            }),
            Err(err) => {
                has_error = true;
                checks.push(DoctorCheck {
                    name: "state_db".into(),
                    status: "error",
                    detail: err.to_string(),
                });
            }
        }
    }

    // 4. skills/
    let installed_skills = skills_sync::list_installed_skills(home);
    if !installed_skills.is_empty() {
        checks.push(DoctorCheck {
            name: "skills".into(),
            status: "ok",
            detail: format!("{} installed in skills/", installed_skills.len()),
        });
    } else if skills_sync::is_opted_out(home) {
        checks.push(DoctorCheck {
            name: "skills".into(),
            status: "info",
            detail: "bundled skills opted out (.no-bundled-skills)".into(),
        });
    } else {
        checks.push(DoctorCheck {
            name: "skills".into(),
            status: "warn",
            detail:
                "0 skills installed (run `hsr skills sync` to download official bundled skills)"
                    .into(),
        });
    }

    // 5. cron/jobs.json
    match hermes_runtime::cron::store::read_jobs(home) {
        Ok(jobs) => checks.push(DoctorCheck {
            name: "cron".into(),
            status: "ok",
            detail: format!("{} scheduled jobs", jobs.len()),
        }),
        Err(err) => checks.push(DoctorCheck {
            name: "cron".into(),
            status: "warn",
            detail: err,
        }),
    }

    // 5. Local owner daemon status (informational)
    let owner_status = match send_v2_command(home, GuiCommand::Ping).await {
        Ok(GuiReply::Pong { .. }) => DoctorCheck {
            name: "local_owner".into(),
            status: "ok",
            detail: "running".into(),
        },
        _ => DoctorCheck {
            name: "local_owner".into(),
            status: "info",
            detail: "not running (starts on demand via `hsr owner` or `hsr-gui`)".into(),
        },
    };
    checks.push(owner_status);

    let report = DoctorReport {
        profile: home.display().to_string(),
        ready: !has_error,
        checks,
    };

    if args.json {
        let out = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("failed formatting doctor JSON: {e}"))?;
        println!("{out}");
    } else {
        println!("Hermes Doctor — {}\n", report.profile);
        for check in &report.checks {
            let icon = match check.status {
                "ok" => "✓",
                "warn" => "!",
                "error" => "✗",
                _ => "·",
            };
            println!("  {icon} {:<14} {}", check.name, check.detail);
        }
        println!(
            "\nStatus: {}",
            if report.ready {
                "ready"
            } else {
                "action required (run `hsr setup`)"
            }
        );
    }

    Ok(if report.ready { 0 } else { 1 })
}

fn check_provider_credentials(home: &Path, settings: &ChatSettings) -> (bool, String) {
    let provider = settings.provider;
    let base_url = settings.base_url.as_deref().unwrap_or("");
    if provider == "custom" && base_url.trim().is_empty() {
        return (
            false,
            "custom provider requires model.base_url (run `hsr setup`)".into(),
        );
    }
    if ChatSettings::keyless_endpoint(provider) {
        let has_key = settings
            .config_api_key
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
            || settings
                .api_key
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
        return if has_key {
            (
                true,
                format!("endpoint + API key configured for '{provider}'"),
            )
        } else {
            (true, format!("keyless local/custom endpoint '{provider}'"))
        };
    }
    if is_oauth_provider(provider) {
        let active = oauth_login::status(home)
            .into_iter()
            .find(|s| s.provider == provider && s.authenticated);
        return match active {
            Some(item) if !item.expired => (
                true,
                format!("OAuth active for '{provider}' ({})", item.source),
            ),
            Some(_) => (
                false,
                format!(
                    "OAuth token for '{provider}' is expired (run `hsr auth login {provider}`)"
                ),
            ),
            None => (
                false,
                format!("OAuth not configured for '{provider}' (run `hsr auth login {provider}`)"),
            ),
        };
    }

    if provider == "bedrock" || provider == "vertex" {
        return (
            true,
            format!("cloud credentials resolved at runtime for '{provider}'"),
        );
    }

    let base_url = settings.base_url.as_deref().unwrap_or("");
    let has_key = settings
        .config_api_key
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
        || settings
            .api_key
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || settings
            .vendor_key_for(provider, base_url)
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());

    if has_key {
        (true, format!("API key configured for '{provider}'"))
    } else if provider == "custom" && base_url.is_empty() {
        (
            false,
            "custom provider requires model.base_url (run `hsr setup`)".into(),
        )
    } else if provider == "custom" {
        (
            false,
            "custom provider has no HERMES_API_KEY / model.api_key set (run `hsr setup`)".into(),
        )
    } else {
        (
            false,
            format!("no API key found for provider '{provider}' (run `hsr setup`)"),
        )
    }
}

fn find_preset(provider: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|p| p.id == provider)
}

fn normalize_provider(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase().replace('_', "-");
    if let Some(canonical) = canonical_login_provider(&trimmed) {
        if canonical != "anthropic" || trimmed == "anthropic" {
            return canonical.to_owned();
        }
    }
    if let Some(profile) = profiles::lookup(&trimmed) {
        return profile.name.clone();
    }
    match trimmed.as_str() {
        "local" | "vllm" | "litellm" => "custom".into(),
        _ => trimmed,
    }
}

fn is_oauth_provider(provider: &str) -> bool {
    matches!(
        provider,
        "nous" | "openai-codex" | "xai-oauth" | "qwen-oauth" | "minimax-oauth"
    )
}

fn key_env_for_provider(
    provider: &str,
    preset: Option<&'static ProviderPreset>,
    profile: Option<&'static profiles::Profile>,
) -> Option<&'static str> {
    if let Some(p) = preset {
        return p.key_env;
    }
    if let Some(prof) = profile {
        return prof.key_envs.first().map(String::as_str);
    }
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "nvidia" => Some("NVIDIA_API_KEY"),
        "custom" => Some("HERMES_API_KEY"),
        _ => Some("HERMES_API_KEY"),
    }
}

fn validate_ui_mode(raw: &str) -> Result<String, String> {
    let mode = raw.trim().to_ascii_lowercase();
    match mode.as_str() {
        "" | "auto" => Ok("auto".into()),
        "tui" | "inline" | "plain" => Ok(mode),
        other => Err(format!(
            "invalid ui mode '{other}' (expected auto, tui, inline, or plain)"
        )),
    }
}

fn normalize_web_backend(raw: &str) -> Result<Option<String>, String> {
    let value = raw.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "none" | "skip" => Ok(None),
        "tavily" | "brave" | "exa" | "firecrawl" | "searxng" | "ddgs" | "perplexity"
        | "parallel" | "keenable" | "xai" | "openai-native" => Ok(Some(value)),
        other => Err(format!("unsupported web search backend '{other}'")),
    }
}

fn web_env_for_backend(backend: &str) -> Option<&'static str> {
    match backend {
        "tavily" => Some("TAVILY_API_KEY"),
        "brave" => Some("BRAVE_API_KEY"),
        "exa" => Some("EXA_API_KEY"),
        "firecrawl" => Some("FIRECRAWL_API_KEY"),
        "searxng" => Some("SEARXNG_URL"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "parallel" => Some("PARALLEL_API_KEY"),
        "keenable" => Some("KEENABLE_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        _ => None,
    }
}

fn split_provider_model(spec: &str) -> Option<(String, &str)> {
    let (left, right) = spec.split_once(':')?;
    let left_trimmed = left.trim();
    let right_trimmed = right.trim();
    if left_trimmed.is_empty() || right_trimmed.is_empty() {
        return None;
    }
    let normalized = normalize_provider(left_trimmed);
    let known = find_preset(&normalized).is_some()
        || profiles::lookup(&normalized).is_some()
        || is_oauth_provider(&normalized)
        || matches!(
            normalized.as_str(),
            "anthropic" | "openai" | "nvidia" | "bedrock" | "vertex" | "custom"
        );
    known.then_some((normalized, right_trimmed))
}

fn read_env_map(home: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(raw) = fs::read_to_string(home.join(".env")) else {
        return map;
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((k, v)) = body.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !k.is_empty() && !v.is_empty() {
                map.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    map
}

fn prompt_provider(reader: &mut impl BufRead, current: Option<&str>) -> Result<String, String> {
    println!("\nSelect LLM provider:");
    for (idx, preset) in PRESETS.iter().enumerate() {
        let mark = if current == Some(preset.id) {
            " (current)"
        } else {
            ""
        };
        println!(
            "  {:>2}. {:<14} — {}{mark}",
            idx + 1,
            preset.id,
            preset.label
        );
    }
    let default_choice = current
        .and_then(|cur| PRESETS.iter().position(|p| p.id == cur))
        .map(|i| (i + 1).to_string())
        .unwrap_or_else(|| "1".into());
    let answer = prompt_line(reader, "Provider [1-10 or provider name]", &default_choice)?;
    if let Ok(num) = answer.trim().parse::<usize>() {
        if let Some(preset) = num.checked_sub(1).and_then(|i| PRESETS.get(i)) {
            return Ok(preset.id.to_owned());
        }
    }
    Ok(normalize_provider(&answer))
}

fn prompt_line(reader: &mut impl BufRead, label: &str, default: &str) -> Result<String, String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    let _ = io::stdout().flush();
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|e| format!("failed reading input: {e}"))?;
    if bytes == 0 {
        return Ok(default.to_owned());
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

fn prompt_yes_no(reader: &mut impl BufRead, label: &str, default: bool) -> Result<bool, String> {
    let hint = if default { "Y/n" } else { "y/N" };
    let answer = prompt_line(reader, &format!("{label} ({hint})"), "")?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "y" | "yes" | "1" | "true" | "д" | "да" => Ok(true),
        "n" | "no" | "0" | "false" | "н" | "нет" => Ok(false),
        _ => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_known_provider_and_keeps_ollama_tags_intact() {
        assert_eq!(
            split_provider_model("openrouter:anthropic/claude-sonnet-4"),
            Some(("openrouter".into(), "anthropic/claude-sonnet-4"))
        );
        assert_eq!(
            split_provider_model("or:deepseek/deepseek-r1"),
            Some(("openrouter".into(), "deepseek/deepseek-r1"))
        );
        // Bare Ollama model tag with colon is NOT mistaken for a provider prefix
        assert_eq!(split_provider_model("qwen2.5-coder:14b"), None);
    }

    #[test]
    fn validates_ui_and_web_backend_names() {
        assert_eq!(validate_ui_mode("TUI").unwrap(), "tui");
        assert_eq!(validate_ui_mode("").unwrap(), "auto");
        assert!(validate_ui_mode("invalid").is_err());
        assert_eq!(normalize_web_backend("none").unwrap(), None);
        assert_eq!(
            normalize_web_backend("tavily").unwrap(),
            Some("tavily".into())
        );
        assert!(normalize_web_backend("unknown_engine").is_err());
    }
}
