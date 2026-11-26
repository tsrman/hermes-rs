//! Cron store health checks (`hermes cron doctor`) and status summary (`hermes cron status`).
//!
//! Mirrors `hermes_cli/cron.py::_cron_doctor_issues_for_job`, `cron_doctor`, and `cron_status`.

use std::path::Path;

use chrono::DateTime;
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;

use super::executions;
use super::incidents;
use super::manage::{normalize_grace_minutes, normalize_timezone, py_repr};
use super::schedule::parse_iso_instant;
use super::script::scripts_dir;
use super::store::{cron_dir, read_raw_jobs, summarize_job};

const OVERDUE_GRACE_SECONDS: i64 = 15 * 60;

#[derive(Clone, Debug, Serialize)]
pub struct StoreProbe {
    pub writable: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JobHealthReport {
    pub id: String,
    pub name: String,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub store: StoreProbe,
    pub checked_jobs: usize,
    pub job_issues: Vec<JobHealthReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub store: StoreProbe,
    pub total_jobs: usize,
    pub active_jobs: usize,
    pub paused_jobs: usize,
    pub completed_jobs: usize,
    pub error_jobs: usize,
    pub overdue_jobs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    pub open_incidents: usize,
    pub recent_executions: Vec<executions::ExecutionRecord>,
}

/// Probe `<home>/cron` writability via a temporary file create + delete.
pub fn probe_store(home: &Path) -> StoreProbe {
    let dir = cron_dir(home);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return StoreProbe {
            writable: false,
            path: dir.to_string_lossy().into_owned(),
            error: Some(format!("cannot create cron directory: {error}")),
        };
    }
    match tempfile::Builder::new().prefix(".probe_").tempfile_in(&dir) {
        Ok(file) => {
            drop(file);
            StoreProbe {
                writable: true,
                path: dir.to_string_lossy().into_owned(),
                error: None,
            }
        }
        Err(error) => StoreProbe {
            writable: false,
            path: dir.to_string_lossy().into_owned(),
            error: Some(format!("cron directory is not writable: {error}")),
        },
    }
}

fn short_reason(text: &str) -> String {
    let first = text
        .trim()
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first.is_empty() {
        return "no details".into();
    }
    if first.chars().count() > 120 {
        let prefix: String = first.chars().take(119).collect();
        format!("{prefix}…")
    } else {
        first
    }
}

fn format_lateness(seconds: f64) -> String {
    let secs = seconds.round().max(0.0) as i64;
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins > 0 {
            format!("{hours}h {mins}m")
        } else {
            format!("{hours}h")
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn script_health_issue(home: &Path, script: &str) -> Option<String> {
    let base_raw = scripts_dir(home);
    let base = crate::paths::canonical(&base_raw).unwrap_or(base_raw);
    let expanded = if let Some(rest) = script.strip_prefix("~/") {
        crate::paths::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| Path::new(script).to_path_buf())
    } else {
        Path::new(script).to_path_buf()
    };
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    let resolved = crate::paths::canonical(&candidate).unwrap_or_else(|_| candidate.clone());
    if !resolved.starts_with(&base) {
        return Some(format!(
            "script resolves outside {}: {}",
            base.display(),
            py_repr(script)
        ));
    }
    if !resolved.exists() {
        return Some(format!("script not found: {}", resolved.display()));
    }
    if !resolved.is_file() {
        return Some(format!("script path is not a file: {}", resolved.display()));
    }
    None
}

fn next_run_overdue_issue(next_run: &str, now: DateTime<Tz>) -> Option<String> {
    let Some(instant) = parse_iso_instant(next_run, now.timezone()) else {
        return Some(format!(
            "next_run_at is not a valid timestamp: {}",
            py_repr(next_run)
        ));
    };
    let overdue_s = now.timestamp() - instant;
    if overdue_s <= OVERDUE_GRACE_SECONDS {
        return None;
    }
    let amount = if overdue_s >= 3600 {
        format!("{:.1}h", overdue_s as f64 / 3600.0)
    } else {
        format!("{:.0}m", overdue_s as f64 / 60.0)
    };
    Some(format!(
        "next_run_at is {amount} overdue — job is not firing (is the scheduler running?)"
    ))
}

/// Inspect one stored job record and return human-readable health issues matching
/// `hermes_cli/cron.py::_cron_doctor_issues_for_job`.
pub fn doctor_issues_for_job(home: &Path, job: &Value, now: DateTime<Tz>) -> Vec<String> {
    let mut issues = Vec::new();
    let id = job["id"].as_str().unwrap_or("<id>");

    let last_status = job["last_status"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !last_status.is_empty()
        && !matches!(
            last_status.as_str(),
            "ok" | "delivery_failed" | "delivery_queued"
        )
    {
        let err = job["last_error"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown error");
        issues.push(format!("last run failed: {err}"));
    }

    if let Some(delivery_err) = job["last_delivery_error"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        issues.push(format!(
            "last run finished but the result was not delivered ({}). Check the target with `hermes cron status` or change it with `hermes cron edit {id} --deliver <target>`.",
            short_reason(delivery_err)
        ));
    }

    if let Some(unverified) = job.get("last_delivery_unverified").filter(|v| {
        !v.is_null()
            && v.as_str().is_none_or(|s| !s.trim().is_empty())
            && v.as_array().is_none_or(|a| !a.is_empty())
    }) {
        let label = match unverified {
            Value::String(s) => s.clone(),
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        };
        issues.push(format!(
            "last delivery unverified (adapter acked without evidence): {label}"
        ));
    }

    if let Some(dispatch) = job["last_dispatch"].as_object() {
        let kind = dispatch
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let label = match kind {
            "late" => Some("late"),
            "catch_up" => Some("catch-up"),
            _ => None,
        };
        if let Some(label) = label {
            let scheduled_at = dispatch
                .get("scheduled_at")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let lateness = dispatch
                .get("lateness_seconds")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            issues.push(format!(
                "last fire was {label} (scheduled {scheduled_at}, {} late). This warning clears at the next on-time fire.",
                format_lateness(lateness)
            ));
        }
    }

    if let Some(fire_err) = job["last_fire_error"].as_object() {
        if let Some(detail) = fire_err
            .get("detail")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let at = fire_err.get("at").and_then(Value::as_str).unwrap_or("?");
            issues.push(format!(
                "missed scheduled fire at {at}: {}. The messaging gateway was unreachable. Run `hermes gateway restart`, then `hermes cron run {id}` to run it now.",
                short_reason(detail)
            ));
        }
    }

    let enabled = job["enabled"].as_bool().unwrap_or(true);
    let state = job["state"].as_str().unwrap_or_default();
    if enabled && !matches!(state, "paused" | "completed") {
        let next_run = job["next_run_at"]
            .as_str()
            .map(str::trim)
            .unwrap_or_default();
        if next_run.is_empty() {
            issues.push("active job has no next_run_at".into());
        } else if let Some(issue) = next_run_overdue_issue(next_run, now) {
            issues.push(issue);
        }
    }

    let script = job["script"].as_str().map(str::trim).unwrap_or_default();
    if job["no_agent"].as_bool() == Some(true) && script.is_empty() {
        issues.push("no-agent job has no script".into());
    }
    if !script.is_empty() {
        if let Some(issue) = script_health_issue(home, script) {
            issues.push(issue);
        }
    }

    if let Some(workdir) = job["workdir"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let path = if let Some(rest) = workdir.strip_prefix("~/") {
            crate::paths::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| Path::new(workdir).to_path_buf())
        } else {
            Path::new(workdir).to_path_buf()
        };
        if !path.exists() {
            issues.push(format!("workdir not found: {workdir}"));
        }
    }

    if let Some(tz) = job["timezone"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if normalize_timezone(Some(tz)).is_err() {
            issues.push(format!(
                "timezone {} is not a valid IANA zone; the job falls back to the profile timezone",
                py_repr(tz)
            ));
        }
    }

    if let Some(grace) = job.get("grace_minutes").filter(|v| !v.is_null()) {
        let text = match grace {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            other => Some(other.to_string()),
        };
        match normalize_grace_minutes(text.as_deref()) {
            Err(exc) => issues.push(format!("grace_minutes is unusable ({exc})")),
            Ok(Some(_)) => {
                if job["schedule"]["kind"].as_str() != Some("cron") {
                    issues.push("grace_minutes has no effect on a non-cron schedule".into());
                }
            }
            Ok(None) => {}
        }
    }

    issues
}

/// Run `hermes cron doctor` across the store and every active job.
pub fn run_doctor(home: &Path, now: DateTime<Tz>) -> Result<DoctorReport, String> {
    let store = probe_store(home);
    let jobs = read_raw_jobs(home)?;
    let mut job_issues = Vec::new();
    let mut checked = 0;
    for job in &jobs {
        let summary = summarize_job(job);
        if !summary.enabled || summary.state == "completed" {
            continue;
        }
        checked += 1;
        let issues = doctor_issues_for_job(home, job, now);
        if !issues.is_empty() {
            job_issues.push(JobHealthReport {
                id: summary.id,
                name: summary.name,
                issues,
            });
        }
    }
    let healthy = store.writable && job_issues.is_empty();
    Ok(DoctorReport {
        healthy,
        store,
        checked_jobs: checked,
        job_issues,
    })
}

/// Gather the status summary for `hermes cron status`.
pub fn cron_status(home: &Path, now: DateTime<Tz>) -> Result<StatusReport, String> {
    let store = probe_store(home);
    let jobs = read_raw_jobs(home)?;
    let mut active_jobs = 0;
    let mut paused_jobs = 0;
    let mut completed_jobs = 0;
    let mut error_jobs = 0;
    let mut overdue_jobs = 0;
    let mut earliest: Option<(i64, String)> = None;

    for job in &jobs {
        let summary = summarize_job(job);
        match summary.state.as_str() {
            "paused" => paused_jobs += 1,
            "completed" => completed_jobs += 1,
            "error" => error_jobs += 1,
            _ => {}
        }
        if summary.enabled && summary.state != "completed" {
            active_jobs += 1;
            if let Some(next_run) = summary.next_run_at.as_deref() {
                if let Some(ts) = parse_iso_instant(next_run, now.timezone()) {
                    if now.timestamp() - ts > OVERDUE_GRACE_SECONDS {
                        overdue_jobs += 1;
                    }
                    if earliest.as_ref().is_none_or(|(best, _)| ts < *best) {
                        earliest = Some((ts, next_run.to_owned()));
                    }
                }
            }
        }
    }

    let open_detected = incidents::list_incidents(home, None, Some("detected"), 500)?.len();
    let open_alerted = incidents::list_incidents(home, None, Some("alerted"), 500)?.len();
    let recent_executions = executions::list_executions(home, None, 10)?;

    Ok(StatusReport {
        store,
        total_jobs: jobs.len(),
        active_jobs,
        paused_jobs,
        completed_jobs,
        error_jobs,
        overdue_jobs,
        next_run_at: earliest.map(|(_, s)| s),
        open_incidents: open_detected + open_alerted,
        recent_executions,
    })
}
