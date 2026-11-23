use std::io::{self, Write};
use std::path::Path;

use clap::{Args, Subcommand, ValueEnum};
use hermes_runtime::cron::manage::{self, AddParams, EditParams, ManualClaim, WrapResponse};
use hermes_runtime::cron::schedule::{compute_next_run, parse_schedule};
use hermes_runtime::cron::{
    cancel, doctor, executions, incidents, monitor, notepad, now_in, profile_timezone, script,
    store, DateTime, Tz,
};
use serde_json::json;

#[derive(Clone, Copy, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

/// Job features of the reference that this version still does not run. Each is accepted by the
/// parser so that it is refused with a reason, instead of being rejected as an unknown flag or,
/// worse, stored on a job that would never run as intended.
#[derive(Args, Default)]
pub struct Unsupported {
    /// Not supported: pinning the main model.
    #[arg(long, hide = true)]
    pin: bool,
    /// Not supported: a provider override.
    #[arg(long, hide = true)]
    provider: Option<String>,
    /// Not supported: a reasoning effort override.
    #[arg(long, hide = true)]
    reasoning_effort: Option<String>,
    /// Not supported: run-to-run continuity (`context_from`).
    #[arg(long, hide = true)]
    continuity: bool,
}

impl Unsupported {
    fn refuse(&self) -> Result<(), String> {
        let found = [
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
            (self.no_continuity, "--no-continuity"),
            (self.unpin, "--unpin"),
        ];
        match found.into_iter().find(|(present, _)| *present) {
            Some((_, flag)) => Err(format!(
                "{flag} is not supported: continuity and model pins are not handled by this version. Edit this job with the Python agent."
            )),
            None => Ok(()),
        }
    }
}

#[derive(Subcommand)]
pub enum IncidentCommand {
    /// List recorded failure incidents as JSON.
    List {
        /// Filter by state (`detected`, `alerted`, `resolved`, `closed`).
        #[arg(long)]
        state: Option<String>,
        /// Filter by job ID.
        #[arg(long)]
        job: Option<String>,
        /// Maximum number of incidents to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Acknowledge and permanently close an incident by ID.
    Ack { incident_id: String },
}

#[derive(Subcommand)]
pub enum NotepadCommand {
    /// List all notepad entries for the job as JSON.
    List,
    /// Get one notepad value by key.
    Get { key: String },
    /// Set one notepad key to a value.
    Set { key: String, value: String },
    /// Delete one notepad key.
    Delete { key: String },
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
        /// Self-contained prompt for the agent (optional when --script or --skill is given).
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
        /// Pre-run script relative to `<profile>/scripts/`.
        #[arg(long)]
        script: Option<String>,
        /// Script-only job (no LLM): runs --script and saves stdout verbatim.
        #[arg(long)]
        no_agent: bool,
        /// Explicit Python interpreter path for `.py` scripts.
        #[arg(long)]
        interpreter: Option<String>,
        /// Skill name(s) to load before the prompt (repeatable).
        #[arg(long = "skill", value_name = "SKILL")]
        skill: Vec<String>,
        /// Change-detecting monitor script relative to `<profile>/scripts/`.
        #[arg(long, conflicts_with = "monitor_url")]
        monitor_script: Option<String>,
        /// Change-detecting monitor HTTP(S) URL.
        #[arg(long, conflicts_with = "monitor_script")]
        monitor_url: Option<String>,
        /// Existing absolute working directory for the job and its scripts.
        #[arg(long)]
        workdir: Option<String>,
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
        /// Pre-run script relative to `<profile>/scripts/`; '' clears it.
        #[arg(long)]
        script: Option<String>,
        /// Enable script-only mode (no LLM).
        #[arg(long, conflicts_with = "agent")]
        no_agent: bool,
        /// Disable script-only mode (use LLM agent).
        #[arg(long, conflicts_with = "no_agent")]
        agent: bool,
        /// Explicit Python interpreter path; '' clears it.
        #[arg(long)]
        interpreter: Option<String>,
        /// Replace skill list (repeatable).
        #[arg(long = "skill", value_name = "SKILL")]
        skill: Vec<String>,
        /// Append skill(s) to the existing list.
        #[arg(long = "add-skill", value_name = "SKILL")]
        add_skill: Vec<String>,
        /// Remove skill(s) from the existing list.
        #[arg(long = "remove-skill", value_name = "SKILL")]
        remove_skill: Vec<String>,
        /// Clear all skills from the job.
        #[arg(long)]
        clear_skills: bool,
        /// Monitor script relative to `<profile>/scripts/`; '' clears it.
        #[arg(long)]
        monitor_script: Option<String>,
        /// Monitor HTTP(S) URL; '' clears it.
        #[arg(long)]
        monitor_url: Option<String>,
        /// Working directory for the job; '' clears it.
        #[arg(long)]
        workdir: Option<String>,
        #[command(flatten)]
        unsupported: UnsupportedEdit,
    },
    /// Pause a job by ID or name.
    Pause {
        job: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Resume a paused job by ID or name, or give a finished one-shot a new run.
    Resume {
        job: String,
        /// Re-arm a one-shot at an ISO-8601 time.
        #[arg(long = "at", value_name = "TIME", conflicts_with = "run_now")]
        run_at: Option<String>,
        /// Re-arm a one-shot to run now.
        #[arg(long)]
        run_now: bool,
    },
    /// Run a job now, in this process, and print what happened as JSON.
    ///
    /// A run ahead of the job's next slot is an extra fire: the slot and the repeat budget are
    /// left alone. Exits with 1 when the run fails.
    Run { job: String },
    /// Request cancellation of a job's live execution without touching its schedule.
    Cancel {
        job: String,
        /// Optional human-readable reason for the cancellation.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Remove a job, stopping its run in progress, and delete its saved output.
    #[command(visible_aliases = ["rm", "delete"])]
    Remove { job: String },
    /// Show cron store health, active/overdue job counts, incidents, and recent runs as JSON.
    Status,
    /// Show durable cron execution history from `cron/executions.db` as JSON.
    #[command(visible_alias = "history")]
    Runs {
        /// Optional job ID filter.
        job: Option<String>,
        /// Maximum number of runs to return.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Inspect or acknowledge deduplicated failure incidents in `cron/executions.db`.
    Incidents {
        #[command(subcommand)]
        action: Option<IncidentCommand>,
    },
    /// Inspect or update a job's persistent key/value notepad (`cron/notepad.db`).
    Notepad {
        job: String,
        #[command(subcommand)]
        action: Option<NotepadCommand>,
    },
    /// Check store writability and every active job's health; exits 1 if issues are found.
    Doctor,
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

fn resolve_edit_skills(
    home: &Path,
    reference: &str,
    replace: &[String],
    add: &[String],
    remove: &[String],
    clear: bool,
) -> Result<Option<Vec<String>>, String> {
    if clear {
        return Ok(Some(Vec::new()));
    }
    if !replace.is_empty() {
        return Ok(Some(replace.to_vec()));
    }
    if add.is_empty() && remove.is_empty() {
        return Ok(None);
    }
    // Load existing skills to merge cleanly.
    let jobs = store::read_raw_jobs(home)?;
    let lower = reference.to_lowercase();
    let existing = jobs
        .iter()
        .find(|j| j["id"].as_str() == Some(reference))
        .or_else(|| {
            jobs.iter().find(|j| {
                j["name"]
                    .as_str()
                    .is_some_and(|n| n.to_lowercase() == lower)
            })
        });
    let mut current: Vec<String> = existing
        .and_then(|job| {
            if let Some(arr) = job["skills"].as_array() {
                Some(
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            } else {
                job["skill"]
                    .as_str()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_owned()])
            }
        })
        .unwrap_or_default();
    let remove_set: std::collections::HashSet<&str> = remove.iter().map(|s| s.trim()).collect();
    current.retain(|s| !remove_set.contains(s.as_str()));
    for item in add {
        let trimmed = item.trim();
        if !trimmed.is_empty() && !current.iter().any(|s| s == trimmed) {
            current.push(trimmed.to_owned());
        }
    }
    Ok(Some(current))
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
            script,
            no_agent,
            interpreter,
            skill,
            monitor_script,
            monitor_url,
            workdir,
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
                script,
                no_agent,
                interpreter,
                skills: skill,
                monitor_script,
                monitor_url,
                workdir,
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
            script,
            no_agent,
            agent,
            interpreter,
            skill,
            add_skill,
            remove_skill,
            clear_skills,
            monitor_script,
            monitor_url,
            workdir,
            unsupported,
        } => {
            unsupported
                .refuse()
                .map_err(|error| format!("Failed to update job: {error}"))?;
            let home = home.ok_or("profile root is unavailable")?;
            let skills =
                resolve_edit_skills(home, &job, &skill, &add_skill, &remove_skill, clear_skills)?;
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
                script,
                no_agent: tri(no_agent, agent),
                interpreter,
                skills,
                monitor_script,
                monitor_url,
                workdir,
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
        Command::Resume {
            job,
            run_at,
            run_now,
        } => {
            let home = home.ok_or("profile root is unavailable")?;
            let now = now_in(zone);
            let at = if run_now {
                Some(hermes_runtime::cron::schedule::iso_instant(now))
            } else {
                run_at.filter(|at| !at.is_empty())
            };
            match at {
                Some(at) => {
                    let rearmed = manage::rearm_oneshot(home, now, &job, &at).map_err(|error| {
                        if error.starts_with("Job not found: ") {
                            error
                        } else {
                            format!("Failed to re-arm job: {error}")
                        }
                    })?;
                    summary(&rearmed)
                }
                None => {
                    let job = store::resume_job(home, now, &job)
                        .map_err(|error| format!("Failed to resume job: {error}"))?;
                    serde_json::to_value(job).map_err(|error| error.to_string())?
                }
            }
        }
        Command::Run { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let (value, exit) = run_now(home, zone, &job)
                .await
                .map_err(|error| format!("Failed to run job: {error}"))?;
            code = exit;
            value
        }
        Command::Cancel { job, reason } => {
            let home = home.ok_or("profile root is unavailable")?;
            let result = cancel::cancel_run(home, now_in(zone), &job, reason.as_deref())
                .map_err(|error| format!("Failed to cancel run: {error}"))?;
            serde_json::to_value(result).map_err(|error| error.to_string())?
        }
        Command::Remove { job } => {
            let home = home.ok_or("profile root is unavailable")?;
            let id = store::remove_job(home, now_in(zone), &job)
                .map_err(|error| format!("Failed to remove job: {error}"))?;
            json!({"removed": id})
        }
        Command::Status => {
            let home = home.ok_or("profile root is unavailable")?;
            let report = doctor::cron_status(home, now_in(zone))?;
            serde_json::to_value(report).map_err(|error| error.to_string())?
        }
        Command::Runs { job, limit } => {
            let home = home.ok_or("profile root is unavailable")?;
            let records = executions::list_executions(home, job.as_deref(), limit)?;
            serde_json::to_value(records).map_err(|error| error.to_string())?
        }
        Command::Incidents { action } => {
            let home = home.ok_or("profile root is unavailable")?;
            match action.unwrap_or(IncidentCommand::List {
                state: None,
                job: None,
                limit: 50,
            }) {
                IncidentCommand::List { state, job, limit } => {
                    let list =
                        incidents::list_incidents(home, job.as_deref(), state.as_deref(), limit)?;
                    serde_json::to_value(list).map_err(|error| error.to_string())?
                }
                IncidentCommand::Ack { incident_id } => {
                    let acked = incidents::ack_incident(home, &incident_id, now_in(zone))?;
                    if !acked {
                        return Err(format!("Incident not found: {incident_id}"));
                    }
                    json!({"acked": true, "incident_id": incident_id})
                }
            }
        }
        Command::Notepad { job, action } => {
            let home = home.ok_or("profile root is unavailable")?;
            match action.unwrap_or(NotepadCommand::List) {
                NotepadCommand::List => {
                    let notes = notepad::list_notes(home, &job)?;
                    serde_json::to_value(notes).map_err(|error| error.to_string())?
                }
                NotepadCommand::Get { key } => match notepad::get_note(home, &job, &key)? {
                    Some(value) => json!({"job_id": job, "key": key, "value": value}),
                    None => {
                        return Err(format!("No notepad key '{key}' for job {job}."));
                    }
                },
                NotepadCommand::Set { key, value } => {
                    let note = notepad::set_note(home, &job, &key, &value, now_in(zone))
                        .map_err(|error| format!("Notepad error: {error}"))?;
                    serde_json::to_value(note).map_err(|error| error.to_string())?
                }
                NotepadCommand::Delete { key } => {
                    let deleted = notepad::delete_note(home, &job, &key)?;
                    if !deleted {
                        return Err(format!("No notepad key '{key}' for job {job}."));
                    }
                    json!({"deleted": true, "job_id": job, "key": key})
                }
            }
        }
        Command::Doctor => {
            let home = home.ok_or("profile root is unavailable")?;
            let report = doctor::run_doctor(home, now_in(zone))?;
            if !report.healthy {
                code = 1;
            }
            serde_json::to_value(report).map_err(|error| error.to_string())?
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
    let exec_id = executions::create_execution(home, &id, "manual", started, None).ok();
    if let Some(ref eid) = exec_id {
        let _ = executions::mark_execution_running(home, eid, started);
    }
    let (success, error, status, document) =
        run_job(home, &record, started, Some(&owner), exec_id.as_deref()).await;
    let finished = now_in(zone);
    let output = save_output_if_present(home, &id, finished, &document)
        .ok()
        .flatten();
    if let Some(ref eid) = exec_id {
        let exec_status = if status == Some("cancelled") {
            "cancelled"
        } else if success {
            "completed"
        } else {
            "failed"
        };
        let _ = executions::finish_execution(
            home,
            eid,
            exec_status,
            error.as_deref(),
            Some("local"),
            finished,
        );
        cancel::clear_cancel(home, eid);
    }
    if success {
        let _ = incidents::close_incidents_for_recovered_job(home, &id, finished);
    } else if status != Some("cancelled") {
        let out_str = output.as_ref().map(|p| p.to_string_lossy().into_owned());
        let _ = incidents::upsert_incident(
            home,
            &id,
            error.as_deref().unwrap_or("cron job failed"),
            out_str.as_deref(),
            finished,
        );
    }
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
            "execution_id": exec_id,
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

fn job_skill_names(job: &serde_json::Value) -> Vec<String> {
    if let Some(items) = job["skills"].as_array() {
        return items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(single) = job["skill"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![single.to_owned()];
    }
    Vec::new()
}

fn run_time_line(job: &serde_json::Value, now: DateTime<Tz>) -> String {
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
        "[CURRENT RUN TIME: {}{zone}]\n\n",
        shown.format("%A, %Y-%m-%d %H:%M %Z").to_string().trim()
    )
}

fn short_circuit_header(job: &serde_json::Value, now: DateTime<Tz>, mode: &str) -> String {
    format!(
        "# Cron Job: {}\n\n**Job ID:** {}\n**Run Time:** {}\n**Mode:** {mode}\n",
        job["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or("cron job"),
        job["id"].as_str().unwrap_or_default(),
        now.format("%Y-%m-%d %H:%M:%S"),
    )
}

/// Assemble the effective prompt for an agent-backed cron job (matching `cron/scheduler_prompt.py`):
/// user prompt + optional monitor context + optional pre-run script block + notepad section +
/// cron hint + run time + optional loaded skills.
fn build_job_prompt(
    home: &Path,
    job: &serde_json::Value,
    now: DateTime<Tz>,
    workdir: Option<&Path>,
    script_block: Option<(bool, &str)>,
    monitor_context: Option<&str>,
) -> String {
    let id = job["id"].as_str().unwrap_or_default();
    let mut prompt = job["prompt"].as_str().unwrap_or_default().to_owned();
    if let Some(mon) = monitor_context.filter(|m| !m.is_empty()) {
        if prompt.is_empty() {
            prompt = format!("## Run Context\n{mon}");
        } else {
            prompt = format!("{prompt}\n\n## Run Context\n{mon}");
        }
    }
    if let Some((ok, script_out)) = script_block {
        let (heading, intro) = if ok {
            (
                "Script Output",
                "The following data was collected by a pre-run script. Use it as context for your analysis.",
            )
        } else {
            (
                "Script Error",
                "The data-collection script failed. Report this to the user.",
            )
        };
        prompt = format!("## {heading}\n{intro}\n\n```\n{script_out}\n```\n\n{prompt}");
    }
    let notepad_section = notepad::render_notepad_section(home, id);
    if !notepad_section.is_empty() {
        prompt = format!("{notepad_section}{prompt}");
    }
    prompt = format!("{CRON_HINT}{}{prompt}", run_time_line(job, now));

    let skills = job_skill_names(job);
    if skills.is_empty() {
        return prompt;
    }
    let cwd = workdir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| home.to_path_buf());
    let mut parts = hermes_runtime::cron::load_cron_skill_parts(home, &cwd, &skills);
    if !prompt.is_empty() {
        parts.push(String::new());
        parts.push(prompt);
    }
    parts.join("\n")
}

/// One job's run. Returns the run's outcome and the text of its output document.
///
/// Handles monitors, `no_agent` scripts, pre-run scripts (with wake-gate), notepad injection,
/// skill loading, `workdir`, fire-claim heartbeats, and cross-process cancellation markers.
async fn run_job(
    home: &Path,
    job: &serde_json::Value,
    now: DateTime<Tz>,
    owner: Option<&str>,
    execution_id: Option<&str>,
) -> (bool, Option<String>, Option<&'static str>, String) {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    let id = job["id"].as_str().unwrap_or_default();
    let workdir = job["workdir"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Path::new)
        .filter(|p| p.is_absolute() && p.is_dir());

    let cancel_check = || {
        execution_id
            .filter(|eid| cancel::cancel_requested(home, eid))
            .and_then(|eid| cancel::cancel_reason(home, eid))
    };

    // 1. no_agent short-circuit: script IS the job (no LLM tokens).
    if job["no_agent"].as_bool() == Some(true) {
        let script_path = job["script"].as_str().map(str::trim).unwrap_or_default();
        if script_path.is_empty() {
            let reason = "no-agent job has no script";
            let _ = hermes_runtime::cron::tick::block_and_pause(home, now, id, reason);
            let doc = format!(
                "{}**Status:** blocked (unrunnable job) — auto-paused\n\n{reason}\n",
                short_circuit_header(job, now, "no_agent (script)")
            );
            return (false, Some(reason.to_owned()), Some("blocked"), doc);
        }
        let interp = job["interpreter"].as_str();
        let (ok, output) = script::run_job_script(
            home,
            script_path,
            workdir,
            interp,
            script::script_timeout_secs(),
            cancel_check,
        )
        .await;
        let header = short_circuit_header(job, now, "no_agent (script)");
        if !ok {
            if cancel_check().is_some() {
                let doc = format!("{header}**Status:** cancelled\n\n{output}\n");
                return (false, Some(output), Some("cancelled"), doc);
            }
            let doc = format!("{header}**Status:** script failed\n\n{output}\n");
            return (false, Some(output), None, doc);
        }
        if !script::parse_wake_gate(&output) {
            let doc = format!("{header}**Status:** silent (wakeAgent=false)\n");
            return (true, None, None, doc);
        }
        if output.trim().is_empty() {
            let doc = format!("{header}**Status:** silent (empty output)\n");
            return (true, None, None, doc);
        }
        let doc = format!("{header}\n---\n\n{output}\n");
        return (true, None, None, doc);
    }

    // 2. Monitor gate: run cheap hash check BEFORE any LLM call.
    let mut monitor_context: Option<String> = None;
    if monitor::job_has_monitor(job) {
        let mon = monitor::check_monitor(home, job, now, cancel_check).await;
        let header = short_circuit_header(job, now, "monitor");
        if !mon.ok {
            if cancel_check().is_some() {
                let err = mon.error.unwrap_or_else(|| "Monitor cancelled".into());
                let doc = format!("{header}**Status:** cancelled\n\n{err}\n");
                return (false, Some(err), Some("cancelled"), doc);
            }
            let err = mon.error.unwrap_or_else(|| "Monitor source failed".into());
            let doc = format!("{header}**Status:** monitor source failed\n\n{err}\n");
            return (false, Some(err), None, doc);
        }
        if !mon.changed {
            let doc = format!("{header}**Status:** no_change (agent run suppressed)\n");
            return (true, None, None, doc);
        }
        monitor_context = Some(mon.context_block);
    }

    // 3. Stored user prompt scan (before pre-run script or LLM).
    let stored_prompt = job["prompt"].as_str().unwrap_or_default();
    if !stored_prompt.is_empty() {
        if let Some(threat) = hermes_runtime::cron::scan::scan_cron_prompt(stored_prompt) {
            let reason = format!("Prompt blocked by the injection scanner: {threat}");
            let _ = hermes_runtime::cron::tick::block_and_pause(home, now, id, &reason);
            let doc = format!(
                "# Cron Job: {}\n\n**Job ID:** {}\n**Status:** blocked (unrunnable job) — auto-paused\n\n{reason}\n",
                job["name"].as_str().unwrap_or("cron job"),
                id
            );
            return (false, Some(reason), Some("blocked"), doc);
        }
    }

    // 4. Optional pre-run script (for agent-backed jobs).
    let mut script_result: Option<(bool, String)> = None;
    if let Some(script_path) = job["script"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let interp = job["interpreter"].as_str();
        let (ok, out) = script::run_job_script(
            home,
            script_path,
            workdir,
            interp,
            script::script_timeout_secs(),
            cancel_check,
        )
        .await;
        if !ok && cancel_check().is_some() {
            let doc = document(job, stored_prompt, &out, now);
            return (false, Some(out), Some("cancelled"), doc);
        }
        if ok && (!script::parse_wake_gate(&out) || out.trim().is_empty()) {
            let header = short_circuit_header(job, now, "script-gate");
            let doc = format!("{header}**Status:** silent (agent run suppressed)\n");
            return (true, None, None, doc);
        }
        script_result = Some((ok, out));
    }

    // 5. Assemble final prompt (with monitor context, script output, notepad, and skills).
    let prompt = build_job_prompt(
        home,
        job,
        now,
        workdir,
        script_result.as_ref().map(|(ok, out)| (*ok, out.as_str())),
        monitor_context.as_deref(),
    );

    // 6. Run the agent turn under inactivity watchdog, claim heartbeat, and cancel polling.
    let model = job["model"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let last_event = Cell::new(Instant::now());
    let turn = crate::chat::run_text(Some(home), prompt.clone(), model, "cron", workdir, || {
        last_event.set(Instant::now())
    });
    tokio::pin!(turn);
    let limit = inactivity_limit();
    let mut watch = tokio::time::interval(Duration::from_millis(100));
    watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks: u64 = 0;
    let mut last_heartbeat = Instant::now();
    let result = loop {
        tokio::select! {
            result = &mut turn => break Some(result),
            _ = watch.tick() => {
                ticks += 1;
                if limit.is_some_and(|limit| last_event.get().elapsed() >= limit) {
                    break None;
                }
                // Check cross-process cancel marker every 100ms.
                if let Some(reason) = cancel_check() {
                    let doc = document(job, &prompt, &format!("Run cancelled: {reason}"), now);
                    return (false, Some(reason), Some("cancelled"), doc);
                }
                // Once a second: is the job still there?
                if ticks.is_multiple_of(10) && !store::job_exists(home, id) {
                    let doc = document(job, &prompt, "Run cancelled: the job was removed.", now);
                    return (false, Some("job removed while running".to_owned()), Some("cancelled"), doc);
                }
                // Every ten seconds: keep the fire claim and execution progress alive.
                if let Some(owner) = owner.filter(|_| last_heartbeat.elapsed() >= Duration::from_secs(10)) {
                    last_heartbeat = Instant::now();
                    let beat_now = now_in(now.timezone());
                    if let Some(eid) = execution_id {
                        let _ = executions::touch_progress(home, eid, beat_now);
                    }
                    if !manage::heartbeat(home, beat_now, id, owner)
                        && store::job_exists(home, id)
                    {
                        break Some(Err("Fire claim ownership lost".to_owned()));
                    }
                }
            }
            signal = tokio::signal::ctrl_c() => {
                let _ = signal;
                let doc = document(job, &prompt, "Run cancelled.", now);
                return (false, None, Some("cancelled"), doc);
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
    let doc = document(job, &prompt, &response, now);
    (success, error, status, doc)
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
        let scheduled_instant = due.record["last_dispatch"]["scheduled_at"].as_str();
        let exec_id =
            executions::create_execution(home, &due.id, "tick", started, scheduled_instant).ok();
        if let Some(ref eid) = exec_id {
            let _ = executions::mark_execution_running(home, eid, started);
        }
        let (success, error, status, document) =
            run_job(home, &due.record, started, Some(&owner), exec_id.as_deref()).await;
        let finished = now_in(zone);
        let output = save_output_if_present(home, &due.id, finished, &document)
            .ok()
            .flatten();
        if let Some(ref eid) = exec_id {
            let exec_status = if status == Some("cancelled") {
                "cancelled"
            } else if success {
                "completed"
            } else {
                "failed"
            };
            let _ = executions::finish_execution(
                home,
                eid,
                exec_status,
                error.as_deref(),
                Some("local"),
                finished,
            );
            cancel::clear_cancel(home, eid);
        }
        if success {
            let _ = incidents::close_incidents_for_recovered_job(home, &due.id, finished);
        } else if status != Some("cancelled") {
            let out_str = output.as_ref().map(|p| p.to_string_lossy().into_owned());
            let _ = incidents::upsert_incident(
                home,
                &due.id,
                error.as_deref().unwrap_or("cron job failed"),
                out_str.as_deref(),
                finished,
            );
        }
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
            "execution_id": exec_id,
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
