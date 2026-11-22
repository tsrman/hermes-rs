use std::io::{self, Write};
use std::path::Path;

use clap::{Args, Subcommand, ValueEnum};
use hermes_runtime::cron::manage::{self, AddParams, EditParams, ManualClaim, WrapResponse};
use hermes_runtime::cron::schedule::{compute_next_run, parse_schedule};
use hermes_runtime::cron::{now_in, profile_timezone, store, DateTime, Tz};
use serde_json::json;

#[derive(Clone, Copy, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

/// Job features of the reference that this version does not run. Each is accepted by the parser
/// so that it is refused with a reason, instead of being rejected as an unknown flag or, worse,
/// stored on a job that would never run as intended.
#[derive(Args, Default)]
pub struct Unsupported {
    /// Not supported: skills.
    #[arg(long = "skill", value_name = "SKILL", hide = true)]
    skill: Vec<String>,
    /// Not supported: pre-run scripts.
    #[arg(long, hide = true)]
    script: Option<String>,
    /// Not supported: script-only jobs.
    #[arg(long, hide = true)]
    no_agent: bool,
    /// Not supported: monitors.
    #[arg(long, hide = true)]
    monitor_script: Option<String>,
    /// Not supported: monitors.
    #[arg(long, hide = true)]
    monitor_url: Option<String>,
    /// Not supported: a working directory.
    #[arg(long, hide = true)]
    workdir: Option<String>,
    /// Not supported: pinning the main model.
    #[arg(long, hide = true)]
    pin: bool,
    /// Not supported: a provider override.
    #[arg(long, hide = true)]
    provider: Option<String>,
    /// Not supported: a reasoning effort override.
    #[arg(long, hide = true)]
    reasoning_effort: Option<String>,
    /// Not supported: an interpreter for scripts.
    #[arg(long, hide = true)]
    interpreter: Option<String>,
    /// Not supported: run-to-run continuity (`context_from`).
    #[arg(long, hide = true)]
    continuity: bool,
}

impl Unsupported {
    fn refuse(&self) -> Result<(), String> {
        let found = [
            (
                !self.skill.is_empty(),
                "--skill",
                "skills are not run by this version",
            ),
            (
                self.script.is_some(),
                "--script",
                "pre-run scripts are not run by this version",
            ),
            (
                self.no_agent,
                "--no-agent",
                "script-only jobs are not run by this version",
            ),
            (
                self.monitor_script.is_some(),
                "--monitor-script",
                "monitors are not run by this version",
            ),
            (
                self.monitor_url.is_some(),
                "--monitor-url",
                "monitors are not run by this version",
            ),
            (
                self.workdir.is_some(),
                "--workdir",
                "a job working directory is not supported by this version",
            ),
            (
                self.pin,
                "--pin",
                "pinning the main model is not supported; give the job --model instead",
            ),
            (
                self.provider.is_some(),
                "--provider",
                "a provider override is not supported; the job uses the profile's provider",
            ),
            (
                self.reasoning_effort.is_some(),
                "--reasoning-effort",
                "a reasoning effort override is not honoured by this version",
            ),
            (
                self.interpreter.is_some(),
                "--interpreter",
                "script interpreters are not supported by this version",
            ),
            (
                self.continuity,
                "--continuity",
                "run-to-run continuity (context_from) is not supported by this version",
            ),
        ];
        match found.into_iter().find(|(present, _, _)| *present) {
            Some((_, flag, reason)) => Err(format!(
                "{flag} is not supported: {reason}. Create or edit this job with the Python agent."
            )),
            None => Ok(()),
        }
    }
}

/// Edit-only spellings of features that are not supported.
#[derive(Args, Default)]
pub struct UnsupportedEdit {
    #[command(flatten)]
    common: Unsupported,
    /// Not supported: skills.
    #[arg(long, value_name = "SKILL", hide = true)]
    add_skill: Vec<String>,
    /// Not supported: skills.
    #[arg(long, value_name = "SKILL", hide = true)]
    remove_skill: Vec<String>,
    /// Not supported: skills.
    #[arg(long, hide = true)]
    clear_skills: bool,
    /// Not supported: script-only jobs.
    #[arg(long, hide = true)]
    agent: bool,
    /// Not supported: run-to-run continuity.
    #[arg(long, hide = true)]
    no_continuity: bool,
    /// Not supported: pinning the main model.
    #[arg(long, hide = true)]
    unpin: bool,
}

impl UnsupportedEdit {
    fn refuse(&self) -> Result<(), String> {
        self.common.refuse()?;
        let found = [
            (!self.add_skill.is_empty(), "--add-skill"),
            (!self.remove_skill.is_empty(), "--remove-skill"),
            (self.clear_skills, "--clear-skills"),
            (self.agent, "--agent"),
            (self.no_continuity, "--no-continuity"),
            (self.unpin, "--unpin"),
        ];
        match found.into_iter().find(|(present, _)| *present) {
            Some((_, flag)) => Err(format!(
                "{flag} is not supported: skills, scripts, continuity and model pins are not handled by this version. Edit this job with the Python agent."
            )),
            None => Ok(()),
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Print the profile's scheduled jobs as JSON without changing the store.
    List {
        /// Include paused and finished jobs.
        #[arg(long)]
        all: bool,
    },
    /// Create a scheduled job and print it as JSON.
    ///
    /// SCHEDULE is an interval ('30m', 'every 2h'), a delay ('in 30m'), a phrase ('every monday
    /// 9am'), a five-field cron expression or a timestamp. Results are saved to the job's output
    /// directory; other delivery is not supported.
    #[command(visible_alias = "create")]
    Add {
        /// Schedule like '30m', 'every 2h' or '0 9 * * *'.
        schedule: String,
        /// Self-contained prompt for the agent.
        prompt: Option<String>,
        /// Human-friendly job name.
        #[arg(long)]
        name: Option<String>,
        /// Delivery target; only 'local' is supported.
        #[arg(long)]
        deliver: Option<String>,
        /// Target for failure notices; only 'local' is supported.
        #[arg(long)]
        failure_deliver: Option<String>,
        /// Number of runs; zero or a negative number means no limit.
        #[arg(long, allow_negative_numbers = true)]
        repeat: Option<i64>,
        /// Run a missed slot once on return ('on') or skip it ('off').
        #[arg(long, value_enum)]
        catch_up: Option<Toggle>,
        /// Wrap the job's output in the cron header and footer.
        #[arg(long, conflicts_with = "no_wrap_response")]
        wrap_response: bool,
        /// Keep the job's output raw.
        #[arg(long)]
        no_wrap_response: bool,
        /// Run this job on a specific model.
        #[arg(long)]
        model: Option<String>,
        /// IANA zone whose wall clock the schedule follows.
        #[arg(long)]
        timezone: Option<String>,
        /// Catch-up window in minutes (0-1440) for a cron schedule; 0 never catches up.
        #[arg(long, allow_hyphen_values = true)]
        grace_minutes: Option<String>,
        /// Create the job disabled; resume it to schedule it.
        #[arg(long)]
        paused: bool,
        /// Reason recorded with --paused.
        #[arg(long)]
        paused_reason: Option<String>,
        #[command(flatten)]
        unsupported: Unsupported,
    },
    /// Edit a job by ID or name and print it as JSON.
    Edit {
        job: String,
        /// New schedule.
        #[arg(long)]
        schedule: Option<String>,
        /// New prompt.
        #[arg(long)]
        prompt: Option<String>,
        /// New job name.
        #[arg(long)]
        name: Option<String>,
        /// New delivery target; only 'local' is supported.
        #[arg(long)]
        deliver: Option<String>,
        /// Target for failure notices; only 'local' is supported, '' clears it.
        #[arg(long)]
        failure_deliver: Option<String>,
        /// New number of runs; zero or a negative number means no limit.
        #[arg(long, allow_negative_numbers = true)]
        repeat: Option<i64>,
        /// Run a missed slot once on return ('on') or skip it ('off').
        #[arg(long, value_enum)]
        catch_up: Option<Toggle>,
        /// Wrap the job's output in the cron header and footer.
        #[arg(long, conflicts_with_all = ["no_wrap_response", "inherit_wrap_response"])]
        wrap_response: bool,
        /// Keep the job's output raw.
        #[arg(long, conflicts_with = "inherit_wrap_response")]
        no_wrap_response: bool,
        /// Follow the global cron.wrap_response again.
        #[arg(long)]
        inherit_wrap_response: bool,
        /// Run this job on a specific model; '' clears it.
        #[arg(long)]
        model: Option<String>,
        /// IANA zone for the schedule; '' clears it.
        #[arg(long)]
        timezone: Option<String>,
        /// Catch-up window in minutes (0-1440); '' restores the computed window.
        #[arg(long, allow_hyphen_values = true)]
        grace_minutes: Option<String>,
        #[command(flatten)]
        unsupported: UnsupportedEdit,
    },
    /// Pause a job by ID or name.
    Pause {
        job: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Resume a paused job by ID or name.
    Resume { job: String },
    /// Run a job now, in this process, and print what happened as JSON.
    ///
    /// A run ahead of the job's next slot is an extra fire: the slot and the repeat budget are
    /// left alone. Exits with 1 when the run fails.
    Run { job: String },
    /// Remove a job, stopping its run in progress, and delete its saved output.
    #[command(visible_aliases = ["rm", "delete"])]
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

fn tri(on: bool, off: bool) -> Option<bool> {
    if on {
        Some(true)
    } else if off {
        Some(false)
    } else {
        None
    }
}

/// A new claim token: this machine's `host:pid`, then random bytes, as the reference stamps.
fn new_owner() -> Result<String, String> {
    Ok(format!(
        "{}:{}",
        hermes_runtime::cron::tick::machine_id(),
        manage::random_hex(16)?
    ))
}

pub async fn run(home: Option<&Path>, command: Command) -> Result<u8, String> {
    let mut output = io::stdout().lock();
    let zone = profile_timezone(home);
    let mut code = 0;
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
        Command::Add {
            schedule,
            prompt,
            name,
            deliver,
            failure_deliver,
            repeat,
            catch_up,
            wrap_response,
            no_wrap_response,
            model,
            timezone,
            grace_minutes,
            paused,
            paused_reason,
            unsupported,
        } => {
            unsupported
                .refuse()
                .map_err(|error| format!("Failed to create job: {error}"))?;
            let home = home.ok_or("profile root is unavailable")?;
            let params = AddParams {
                schedule,
                prompt,
                name,
                deliver,
                failure_deliver,
                repeat,
                catch_up: catch_up.map(|toggle| matches!(toggle, Toggle::On)),
                wrap_response: tri(wrap_response, no_wrap_response),
                model,
                timezone,
                grace_minutes,
                paused,
                paused_reason,
            };
            let job = manage::add_job(home, now_in(zone), &params)
                .map_err(|error| format!("Failed to create job: {error}"))?;
            summary(&job)
        }
        Command::Edit {
            job,
            schedule,
            prompt,
            name,
            deliver,
            failure_deliver,
            repeat,
            catch_up,
            wrap_response,
            no_wrap_response,
            inherit_wrap_response,
            model,
            timezone,
            grace_minutes,
            unsupported,
        } => {
            unsupported
                .refuse()
                .map_err(|error| format!("Failed to update job: {error}"))?;
            let home = home.ok_or("profile root is unavailable")?;
            let params = EditParams {
                schedule,
                prompt,
                name,
                deliver,
                failure_deliver,
                repeat,
                catch_up: catch_up.map(|toggle| matches!(toggle, Toggle::On)),
                wrap_response: if wrap_response {
                    Some(WrapResponse::On)
                } else if no_wrap_response {
                    Some(WrapResponse::Off)
                } else if inherit_wrap_response {
                    Some(WrapResponse::Inherit)
                } else {
                    None
                },
                model,
                timezone,
                grace_minutes,
            };
            let updated = manage::edit_job(home, now_in(zone), &job, &params).map_err(|error| {
                // A lookup failure is reported as is, as the reference's edit does.
                if error.starts_with("Job not found: ") || error.starts_with("Job name '") {
                    error
                } else {
                    format!("Failed to update job: {error}")
                }
            })?;
            summary(&updated)
        }
        Command::Pause { job, reason } => {
            let home = home.ok_or("profile root is unavailable")?;
            let job = store::pause_job(home, now_in(zone), &job, reason.as_deref())
                .map_err(|error| format!("Failed to pause job: {error}"))?;
            serde_json::to_value(job).map_err(|error| error.to_string())?
        }
        Command::Resume { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let job = store::resume_job(home, now_in(zone), &job)
                .map_err(|error| format!("Failed to resume job: {error}"))?;
            serde_json::to_value(job).map_err(|error| error.to_string())?
        }
        Command::Run { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let (value, exit) = run_now(home, zone, &job)
                .await
                .map_err(|error| format!("Failed to run job: {error}"))?;
            code = exit;
            value
        }
        Command::Remove { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let id = store::remove_job(home, now_in(zone), &job)
                .map_err(|error| format!("Failed to remove job: {error}"))?;
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
                        .map_err(|_| format!("Unknown timezone {}.", manage::py_repr(&name)))
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
    writeln!(output, "{value}").map_err(|error| error.to_string())?;
    Ok(code)
}

/// The summary of a stored job record, as `list` prints it.
fn summary(job: &serde_json::Value) -> serde_json::Value {
    serde_json::to_value(store::summarize_job(job)).unwrap_or_default()
}

/// `hermes cron run`: claim the job, run it here, save its output and book the outcome.
async fn run_now(
    home: &Path,
    zone: Tz,
    reference: &str,
) -> Result<(serde_json::Value, u8), String> {
    use hermes_runtime::cron::tick::{mark_job_run, save_output_if_present, Outcome};

    let owner = new_owner()?;
    let started = now_in(zone);
    let record = match manage::claim_manual(home, started, reference, &owner)? {
        ManualClaim::Skipped(reason) => {
            return Ok((json!({"executed": false, "skipped": reason}), 0));
        }
        ManualClaim::Claimed(record) => *record,
    };
    let id = record["id"].as_str().unwrap_or_default().to_owned();
    let (success, error, status, document) = run_job(home, &record, started, Some(&owner)).await;
    let finished = now_in(zone);
    let output = save_output_if_present(home, &id, finished, &document)
        .ok()
        .flatten();
    mark_job_run(
        home,
        finished,
        &id,
        &Outcome {
            success,
            error: error.as_deref(),
            status,
            owner: Some(&owner),
        },
    )?;
    let job = manage::find_job(home, finished, &id).map(|job| summary(&job));
    let exit = u8::from(!success && status != Some("cancelled"));
    Ok((
        json!({
            "executed": true,
            "success": success,
            "status": status.unwrap_or(if success { "ok" } else { "error" }),
            "error": error,
            "output": output.map(|path| path.to_string_lossy().into_owned()),
            "job": job,
        }),
        exit,
    ))
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

/// One job's run. Returns the run's outcome and the text of its output document.
///
/// While it runs, the fire claim is kept alive, and the run is stopped when the job is removed
/// from the store (its output is then not kept) or when the claim has been taken over.
async fn run_job(
    home: &Path,
    job: &serde_json::Value,
    now: DateTime<Tz>,
    owner: Option<&str>,
) -> (bool, Option<String>, Option<&'static str>, String) {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    let id = job["id"].as_str().unwrap_or_default();
    let stored_prompt = job["prompt"].as_str().unwrap_or_default();
    // The stored prompt is scanned like a freshly created one: a hit stops the run and pauses
    // the job, because a job left enabled would fail again on every tick.
    if let Some(threat) = hermes_runtime::cron::scan::scan_cron_prompt(stored_prompt) {
        let reason = format!("Prompt blocked by the injection scanner: {threat}");
        let _ = hermes_runtime::cron::tick::block_and_pause(home, now, id, &reason);
        let document = format!("# Cron Job: {}\n\n**Job ID:** {}\n**Status:** blocked (unrunnable job) — auto-paused\n\n{reason}\n",
            job["name"].as_str().unwrap_or("cron job"), id);
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
    let mut watch = tokio::time::interval(Duration::from_millis(100));
    watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks: u64 = 0;
    let result = loop {
        tokio::select! {
            result = &mut turn => break Some(result),
            _ = watch.tick() => {
                ticks += 1;
                if limit.is_some_and(|limit| last_event.get().elapsed() >= limit) {
                    break None;
                }
                // Once a second: is the job still there?
                if ticks % 10 == 0 && !store::job_exists(home, id) {
                    let document = document(job, &prompt, "Run cancelled: the job was removed.", now);
                    return (false, Some("job removed while running".to_owned()), Some("cancelled"), document);
                }
                // Every ten seconds: keep the fire claim alive.
                if let Some(owner) = owner.filter(|_| ticks % 100 == 0) {
                    if !manage::heartbeat(home, now_in(now.timezone()), id, owner)
                        && store::job_exists(home, id)
                    {
                        break Some(Err("Fire claim ownership lost".to_owned()));
                    }
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
        acquire_tick_lock, mark_job_run, save_output_if_present, scan_due, Outcome,
    };

    let Some(_lock) = acquire_tick_lock(home)? else {
        return Ok(json!({"ran": [], "skipped": "another tick holds the lock"}));
    };
    let settings = hermes_runtime::cron::load_tick_settings(home)?;
    let scan = scan_due(home, now_in(zone), settings)?;
    let mut ran = Vec::new();
    let mut busy = Vec::new();
    for due in scan.due {
        let started = now_in(zone);
        let owner = new_owner()?;
        // A job a manual run (or another ticker) holds is left to it.
        if !manage::claim_for_tick(home, started, &due.id, &owner)? {
            busy.push(due.id);
            continue;
        }
        let (success, error, status, document) =
            run_job(home, &due.record, started, Some(&owner)).await;
        let finished = now_in(zone);
        let output = save_output_if_present(home, &due.id, finished, &document)
            .ok()
            .flatten();
        let outcome = Outcome {
            success,
            error: error.as_deref(),
            status,
            owner: Some(&owner),
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
        "busy": busy,
        "unsupported": scan.unsupported.iter().map(|(id, reason)| json!({"id": id, "reason": reason})).collect::<Vec<_>>(),
        "retired": scan.retired,
    }))
}
