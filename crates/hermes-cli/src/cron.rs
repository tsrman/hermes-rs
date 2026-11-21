use std::io::{self, Write};
use std::path::Path;

use clap::Subcommand;
use hermes_runtime::cron::schedule::{compute_next_run, parse_schedule};
use hermes_runtime::cron::{now_in, profile_timezone, store, DateTime, Tz};
use serde_json::json;

#[derive(Subcommand)]
pub enum Command {
    /// Print the profile's scheduled jobs as JSON without changing the store.
    List {
        /// Include paused and finished jobs.
        #[arg(long)]
        all: bool,
    },
    /// Pause a job by ID or name.
    Pause {
        job: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Resume a paused job by ID or name.
    Resume { job: String },
    /// Remove a job and its saved output.
    Remove { job: String },
    /// Run the jobs that are due now, once, and print what happened as JSON.
    ///
    /// Meant for a system crontab or timer (for example every minute). It shares the
    /// profile's tick lock with the Python gateway, so only one of them runs a tick at a time.
    Tick,
    /// Print the next run times of a schedule as JSON.
    Preview {
        schedule: String,
        /// Number of runs to compute.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=50))]
        count: u32,
        /// Pin an IANA zone instead of the profile zone.
        #[arg(long, value_name = "ZONE")]
        timezone: Option<String>,
    },
}

pub async fn run(home: Option<&Path>, command: Command) -> Result<(), String> {
    let mut output = io::stdout().lock();
    let zone = profile_timezone(home);
    let value = match command {
        Command::List { all } => {
            let jobs = store::read_jobs(home.ok_or("profile root is unavailable")?)?;
            let shown: Vec<_> = jobs
                .into_iter()
                .filter(|job| all || (job.enabled && job.state != "completed"))
                .collect();
            serde_json::to_value(shown).map_err(|error| error.to_string())?
        }
        Command::Tick => tick(home.ok_or("profile root is unavailable")?, zone).await?,
        Command::Pause { job, reason } => {
            let home = home.ok_or("profile root is unavailable")?;
            let job = store::pause_job(home, now_in(zone), &job, reason.as_deref())?;
            serde_json::to_value(job).map_err(|error| error.to_string())?
        }
        Command::Resume { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let job = store::resume_job(home, now_in(zone), &job)?;
            serde_json::to_value(job).map_err(|error| error.to_string())?
        }
        Command::Remove { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let id = store::remove_job(home, now_in(zone), &job)?;
            json!({"removed": id})
        }
        Command::Preview {
            schedule,
            count,
            timezone,
        } => {
            let pinned = timezone
                .map(|name| {
                    name.parse()
                        .map_err(|_| format!("Unknown timezone {name:?}."))
                })
                .transpose()?;
            let now = now_in(zone);
            let parsed = parse_schedule(&schedule, now)?;
            let mut runs = Vec::new();
            let mut last: Option<String> = None;
            for _ in 0..count {
                let Some(next) = compute_next_run(&parsed, last.as_deref(), pinned, now) else {
                    break;
                };
                // One-shot schedules have a single run.
                let repeats = parsed.kind != "once";
                last = Some(next.clone());
                runs.push(next);
                if !repeats {
                    break;
                }
            }
            json!({"schedule": parsed, "timezone": zone.name(), "next_runs": runs})
        }
    };
    writeln!(output, "{value}").map_err(|error| error.to_string())
}

const CRON_HINT: &str = "[IMPORTANT: You are running as a scheduled cron job. DELIVERY: Your final response will be automatically delivered to the user — do NOT use send_message or try to deliver the output yourself. Just produce your report/output as your final response and the system handles the rest. SILENT: If there is genuinely nothing new to report, respond with exactly \"[SILENT]\" (nothing else) to suppress delivery. [SILENT] is a literal ASCII control token — never translate or rephrase it, whatever language the rest of your answer uses. Never combine [SILENT] with content — either report your findings normally, or say [SILENT] and nothing more. FAILURE: If a delegated child fails and this cron run must be recorded as failed, put [CRON_FAILURE] on the first line by itself, then explain the child failure on following lines. RECURSION: This is a run of an EXISTING scheduled job — execute the task now. NEVER create or update a cron job because of recurring or future-schedule language in the task prompt below; treat phrasing like \"each Monday\" or \"every day at 9\" as context for this run, not as a request to schedule another job.]\n\n";

/// Seconds of silence after which a run is stopped: `HERMES_CRON_TIMEOUT`, default 600, 0 = never.
fn inactivity_limit() -> Option<std::time::Duration> {
    let seconds = std::env::var("HERMES_CRON_TIMEOUT")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .unwrap_or(600.0);
    (seconds > 0.0).then(|| std::time::Duration::from_secs_f64(seconds))
}

/// The prompt a job runs with: the fixed hint, this run's time, and the stored prompt.
fn job_prompt(job: &serde_json::Value, now: DateTime<Tz>) -> String {
    let pinned = job["timezone"]
        .as_str()
        .and_then(|name| name.trim().parse::<Tz>().ok());
    let shown = pinned.map_or(now, |zone| now.with_timezone(&zone));
    let offset = shown.format("%z").to_string();
    let zone = if offset.len() >= 5 {
        format!(" (UTC{}:{})", &offset[..3], &offset[3..5])
    } else {
        String::new()
    };
    format!(
        "{CRON_HINT}[CURRENT RUN TIME: {}{zone}]\n\n{}",
        shown.format("%A, %Y-%m-%d %H:%M %Z").to_string().trim(),
        job["prompt"].as_str().unwrap_or_default()
    )
}

/// One due job. Returns the run's outcome and the text of its output document.
async fn run_job(
    home: &Path,
    job: &serde_json::Value,
    now: DateTime<Tz>,
) -> (bool, Option<String>, Option<&'static str>, String) {
    use std::cell::Cell;
    use std::time::Instant;

    let stored_prompt = job["prompt"].as_str().unwrap_or_default();
    // The stored prompt is scanned like a freshly created one: a hit stops the run and pauses
    // the job, because a job left enabled would fail again on every tick.
    if let Some(threat) = hermes_runtime::prompt::prompt_threat(stored_prompt) {
        let reason = format!("Prompt blocked by the injection scanner: {threat}");
        let _ = hermes_runtime::cron::tick::block_and_pause(
            home,
            now,
            job["id"].as_str().unwrap_or_default(),
            &reason,
        );
        let document = format!("# Cron Job: {}\n\n**Job ID:** {}\n**Status:** blocked (unrunnable job) — auto-paused\n\n{reason}\n",
            job["name"].as_str().unwrap_or("cron job"), job["id"].as_str().unwrap_or_default());
        return (false, Some(reason), Some("blocked"), document);
    }
    let prompt = job_prompt(job, now);
    let model = job["model"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let last_event = Cell::new(Instant::now());
    let turn = crate::chat::run_text(Some(home), prompt.clone(), model, "cron", || {
        last_event.set(Instant::now())
    });
    tokio::pin!(turn);
    let limit = inactivity_limit();
    let result = loop {
        let wait = limit.map_or(std::time::Duration::from_secs(3600), |limit| {
            limit
                .saturating_sub(last_event.get().elapsed())
                .max(std::time::Duration::from_millis(50))
        });
        tokio::select! {
            result = &mut turn => break Some(result),
            _ = tokio::time::sleep(wait) => {
                if limit.is_some_and(|limit| last_event.get().elapsed() >= limit) {
                    break None;
                }
            }
            signal = tokio::signal::ctrl_c() => {
                let _ = signal;
                let document = crate::cron::document(job, &prompt, "Run cancelled.", now);
                return (false, None, Some("cancelled"), document);
            }
        }
    };
    let (success, error, status, response) = match result {
        None => (
            false,
            Some("Cron job idle timeout".to_owned()),
            None,
            "Run stopped: no activity within the limit.".to_owned(),
        ),
        Some(Err(error)) => (
            false,
            Some(error.clone()),
            None,
            format!("Run failed: {error}"),
        ),
        Some(Ok((text, code))) => {
            let text = text.trim().to_owned();
            // The agent can declare the run failed on the first line.
            if let Some(rest) = text.strip_prefix("[CRON_FAILURE]") {
                let detail = rest.trim().to_owned();
                (
                    false,
                    Some(if detail.is_empty() {
                        "Agent declared failure".to_owned()
                    } else {
                        detail
                    }),
                    None,
                    text.clone(),
                )
            } else if code != 0 {
                (
                    false,
                    Some(format!("The turn ended with status {code}")),
                    None,
                    text.clone(),
                )
            } else {
                (true, None, None, text)
            }
        }
    };
    let document = document(job, &prompt, &response, now);
    (success, error, status, document)
}

fn document(job: &serde_json::Value, prompt: &str, response: &str, now: DateTime<Tz>) -> String {
    hermes_runtime::cron::tick::run_document(job, prompt, response, now)
}

async fn tick(home: &Path, zone: Tz) -> Result<serde_json::Value, String> {
    use hermes_runtime::cron::tick::{
        acquire_tick_lock, mark_job_run, save_output, scan_due, Outcome,
    };

    let Some(_lock) = acquire_tick_lock(home)? else {
        return Ok(json!({"ran": [], "skipped": "another tick holds the lock"}));
    };
    let settings = hermes_runtime::cron::load_tick_settings(home)?;
    let scan = scan_due(home, now_in(zone), settings)?;
    let mut ran = Vec::new();
    for due in scan.due {
        let started = now_in(zone);
        let (success, error, status, document) = run_job(home, &due.record, started).await;
        let finished = now_in(zone);
        let output = save_output(home, &due.id, finished, &document).ok();
        let outcome = Outcome {
            success,
            error: error.as_deref(),
            status,
        };
        mark_job_run(home, finished, &due.id, &outcome)?;
        ran.push(json!({
            "id": due.id,
            "name": due.name,
            "success": success,
            "status": status.unwrap_or(if success { "ok" } else { "error" }),
            "error": error,
            "output": output.map(|path| path.to_string_lossy().into_owned()),
        }));
    }
    Ok(json!({
        "ran": ran,
        "unsupported": scan.unsupported.iter().map(|(id, reason)| json!({"id": id, "reason": reason})).collect::<Vec<_>>(),
        "retired": scan.retired,
    }))
}
