//! Read-only access to `cron/jobs.json` in the layout used by the Python reference.

use std::path::{Path, PathBuf};

use chrono::DateTime;
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::{json, Value};

use super::schedule::{compute_next_run, iso_instant, parse_iso_instant, Schedule};

#[derive(Debug, Serialize)]
pub struct JobSummary {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub state: String,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub timezone: Option<String>,
}

/// Jobs from `<home>/cron/jobs.json`. A missing file is an empty list. The file is never
/// rewritten, so a damaged shape is reported instead of repaired.
pub fn read_jobs(home: &Path) -> Result<Vec<JobSummary>, String> {
    let path = home.join("cron").join("jobs.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read cron jobs: {error}")),
    };
    let data: Value = serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw))
        .map_err(|_| "cron jobs.json is not valid JSON")?;
    let records: Vec<Value> = match data {
        Value::Array(jobs) => jobs,
        Value::Object(mut object) => match object.remove("jobs") {
            None => Vec::new(),
            Some(Value::Array(jobs)) => jobs,
            // An id-keyed map from external tools: the inline id wins over the key.
            Some(Value::Object(map)) => map
                .into_iter()
                .filter_map(|(key, mut job)| {
                    let fields = job.as_object_mut()?;
                    if fields
                        .get("id")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                    {
                        fields.insert("id".into(), Value::String(key));
                    }
                    Some(job)
                })
                .collect(),
            Some(_) => return Err("cron jobs.json has an invalid 'jobs' field".into()),
        },
        _ => return Err("cron jobs.json must contain an object or a list".into()),
    };
    Ok(records
        .iter()
        .filter(|job| job.is_object())
        .map(summarize)
        .collect())
}

fn text(job: &Value, key: &str) -> String {
    match job.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn optional(job: &Value, key: &str) -> Option<String> {
    job.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(super) fn summarize(job: &Value) -> JobSummary {
    let id = Some(text(job, "id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let enabled = job.get("enabled").is_none_or(|value| match value {
        Value::Bool(flag) => *flag,
        Value::Null => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    });
    let name = {
        let name = text(job, "name").trim().to_owned();
        if name.is_empty() {
            let source = [
                text(job, "prompt"),
                text(job, "script").trim().to_owned(),
                id.clone(),
            ]
            .into_iter()
            .find(|value| !value.is_empty())
            .unwrap_or_default();
            let label: String = source.chars().take(50).collect();
            Some(label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| "cron job".into())
        } else {
            name
        }
    };
    let schedule = {
        let display = text(job, "schedule_display").trim().to_owned();
        if !display.is_empty() {
            display
        } else {
            match job.get("schedule") {
                Some(Value::Object(schedule)) => ["display", "value", "expr", "run_at"]
                    .iter()
                    .find_map(|key| {
                        schedule
                            .get(*key)
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "?".into()),
                Some(Value::Null) | None => "?".into(),
                Some(other) => other
                    .as_str()
                    .map_or_else(|| other.to_string(), str::to_owned),
            }
        }
    };
    JobSummary {
        state: effective_state(job, enabled),
        id,
        name,
        schedule,
        enabled,
        next_run_at: optional(job, "next_run_at"),
        last_run_at: optional(job, "last_run_at"),
        timezone: optional(job, "timezone"),
    }
}

/// Operator-facing state: `enabled` is authoritative, terminal states are kept.
fn effective_state(job: &Value, enabled: bool) -> String {
    let stored = text(job, "state").trim().to_owned();
    if stored == "completed" || stored == "error" {
        return stored;
    }
    let paused_marker = stored == "paused"
        || job
            .get("paused_at")
            .is_some_and(|v| !v.is_null() && v != "" && v != false);
    if !enabled {
        return if paused_marker || stored == "paused" || stored.is_empty() {
            "paused".into()
        } else {
            stored
        };
    }
    if paused_marker || stored.is_empty() {
        "scheduled".into()
    } else {
        stored
    }
}

pub(super) fn cron_dir(home: &Path) -> PathBuf {
    home.join("cron")
}

/// Run `change` on the job list under the cross-process lock and save it atomically when it
/// reports a change. Only the canonical `{"jobs": [...]}` shape is written: anything else could
/// lose data, so it is refused rather than repaired.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn with_jobs<T>(
    home: &Path,
    now: DateTime<Tz>,
    change: impl FnOnce(&mut Vec<Value>) -> Result<(T, bool), String>,
) -> Result<T, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    use rustix::fs::{flock, FlockOperation};

    let dir = cron_dir(home);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create cron directory: {error}"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(".jobs.lock"))
        .map_err(|error| format!("cannot open cron lock: {error}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for the cron jobs lock".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let path = dir.join("jobs.json");
    let mut jobs = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let data: Value = serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw))
                .map_err(|_| "cron jobs.json is not valid JSON")?;
            match data.get("jobs") {
                Some(Value::Array(jobs)) if jobs.iter().all(Value::is_object) => jobs.clone(),
                _ => {
                    return Err("cron jobs.json has a layout this version does not rewrite; run `hermes cron list` and repair it with the Python agent first".into())
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("cannot read cron jobs: {error}")),
    };
    let (result, changed) = change(&mut jobs)?;
    if changed {
        let payload = serde_json::to_string_pretty(&json!({
            "jobs": jobs,
            "updated_at": iso_instant(now),
        }))
        .map_err(|error| error.to_string())?;
        let mut staged = tempfile::Builder::new()
            .prefix(".jobs_")
            .suffix(".tmp")
            .tempfile_in(&dir)
            .map_err(|error| format!("cannot stage cron jobs: {error}"))?;
        staged
            .write_all(payload.as_bytes())
            .and_then(|()| staged.as_file().sync_all())
            .map_err(|error| format!("cannot write cron jobs: {error}"))?;
        staged
            .persist(&path)
            .map_err(|error| format!("cannot replace cron jobs: {}", error.error))?;
        // Sync the directory so the rename survives a crash.
        if let Ok(directory) = std::fs::File::open(&dir) {
            let _ = directory.sync_all();
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(result)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn with_jobs<T>(
    _home: &Path,
    _now: DateTime<Tz>,
    _change: impl FnOnce(&mut Vec<Value>) -> Result<(T, bool), String>,
) -> Result<T, String> {
    Err("changing cron jobs is not supported on this platform yet".into())
}

/// The index of the job named by an ID or, failing that, a case-insensitive name.
fn resolve(jobs: &[Value], reference: &str) -> Result<usize, String> {
    if reference.is_empty() {
        return Err("job reference is empty".into());
    }
    if let Some(index) = jobs
        .iter()
        .position(|job| job["id"].as_str() == Some(reference))
    {
        return Ok(index);
    }
    let lower = reference.to_lowercase();
    let matches: Vec<usize> = jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| {
            job["name"]
                .as_str()
                .is_some_and(|name| name.to_lowercase() == lower)
        })
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [] => Err(format!("job '{reference}' was not found")),
        [index] => Ok(*index),
        many => {
            let ids: Vec<&str> = many
                .iter()
                .map(|i| jobs[*i]["id"].as_str().unwrap_or("?"))
                .collect();
            Err(format!(
                "Job name '{reference}' is ambiguous — matches {} jobs: {}. Use the job ID instead.",
                many.len(),
                ids.join(", ")
            ))
        }
    }
}

fn reject_terminal(job: &Value, action: &str) -> Result<(), String> {
    match job["state"].as_str() {
        Some(state @ ("completed" | "error")) => Err(format!(
            "Cannot {action} a job in the terminal state '{state}' with this version"
        )),
        _ => Ok(()),
    }
}

pub(super) fn job_zone(job: &Value) -> Option<Tz> {
    job["timezone"]
        .as_str()
        .and_then(|name| name.trim().parse().ok())
}

pub fn pause_job(
    home: &Path,
    now: DateTime<Tz>,
    reference: &str,
    reason: Option<&str>,
) -> Result<JobSummary, String> {
    with_jobs(home, now, |jobs| {
        let index = resolve(jobs, reference)?;
        reject_terminal(&jobs[index], "pause")?;
        let job = jobs[index].as_object_mut().ok_or("invalid job record")?;
        job.insert("enabled".into(), json!(false));
        job.insert("state".into(), json!("paused"));
        job.insert("paused_at".into(), json!(iso_instant(now)));
        job.insert("paused_reason".into(), json!(reason));
        // An explicit lifecycle change supersedes an occurrence left unclaimed before it.
        job.shift_remove("pending_slot");
        Ok((summarize(&jobs[index]), true))
    })
}

pub fn resume_job(home: &Path, now: DateTime<Tz>, reference: &str) -> Result<JobSummary, String> {
    with_jobs(home, now, |jobs| {
        let index = resolve(jobs, reference)?;
        reject_terminal(&jobs[index], "resume")?;
        let schedule: Schedule = serde_json::from_value(jobs[index]["schedule"].clone())
            .map_err(|_| "job has an unreadable schedule")?;
        let zone = job_zone(&jobs[index]);
        let stored = jobs[index]["next_run_at"]
            .as_str()
            .filter(|value| !value.is_empty());
        // A recurring slot that elapsed while paused stays due; the scheduler's catch-up policy
        // decides whether to run or skip it.
        let keep = matches!(schedule.kind.as_str(), "cron" | "interval")
            && stored
                .and_then(|text| parse_iso_instant(text, now.timezone()))
                .is_some_and(|instant| instant <= now.timestamp());
        let next = if keep {
            stored.map(str::to_owned)
        } else {
            compute_next_run(&schedule, None, zone, now)
        };
        if next.is_none() && schedule.kind == "once" {
            return Err(format!(
                "Cannot resume: one-shot time {} is in the past (grace window: 120s) and will never fire.",
                schedule.run_at.as_deref().unwrap_or("unknown")
            ));
        }
        let job = jobs[index].as_object_mut().ok_or("invalid job record")?;
        job.insert("enabled".into(), json!(true));
        job.insert("state".into(), json!("scheduled"));
        job.insert("paused_at".into(), Value::Null);
        job.insert("paused_reason".into(), Value::Null);
        job.insert("next_run_at".into(), json!(next));
        job.shift_remove("pending_slot");
        Ok((summarize(&jobs[index]), true))
    })
}

/// Remove a job and, once the store is saved, its saved output. Returns the removed ID.
pub fn remove_job(home: &Path, now: DateTime<Tz>, reference: &str) -> Result<String, String> {
    let (id, output) = with_jobs(home, now, |jobs| {
        let index = resolve(jobs, reference)?;
        let id = jobs[index]["id"].as_str().unwrap_or_default().to_owned();
        // The ID becomes a path component, so only a single plain name may be used.
        if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
            return Err(format!("Invalid cron job id for output path: {id:?}"));
        }
        jobs.remove(index);
        let output = cron_dir(home).join("output").join(&id);
        Ok(((id, output), true))
    })?;
    if output.is_dir() {
        std::fs::remove_dir_all(&output)
            .map_err(|error| format!("job {id} was removed but its output was not: {error}"))?;
    }
    Ok(id)
}
