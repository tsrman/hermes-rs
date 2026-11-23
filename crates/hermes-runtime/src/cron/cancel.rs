//! Cross-process cancellation markers for live cron runs (`<home>/cron/cancels/<exec_id>.cancel`).
//!
//! Mirrors `cron/cancellation.py` of the Python reference: atomic first-writer-wins marker files
//! keyed by `execution_id`, polled by the running job loop, with stale-marker pruning.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::DateTime;
use chrono_tz::Tz;
use serde::Serialize;

use super::executions;
use super::manage::resolve_for;
use super::store::{cron_dir, summarize_job, with_jobs, JobSummary};

pub const DEFAULT_CANCEL_REASON: &str = "cancelled on request (hermes cron cancel)";
const STALE_MARKER_SECS: u64 = 24 * 3600;

fn cancels_dir(home: &Path) -> PathBuf {
    cron_dir(home).join("cancels")
}

fn marker_path(home: &Path, execution_id: &str) -> Result<PathBuf, String> {
    if execution_id.is_empty()
        || execution_id == "."
        || execution_id == ".."
        || execution_id.contains(['/', '\\', '\0'])
    {
        return Err(format!("invalid execution id: {execution_id:?}"));
    }
    Ok(cancels_dir(home).join(format!("{execution_id}.cancel")))
}

/// Write a cancellation marker for `execution_id`. First-writer-wins: if a marker already exists,
/// preserves the original reason and returns `false`.
pub fn request_cancel(
    home: &Path,
    execution_id: &str,
    reason: Option<&str>,
) -> Result<bool, String> {
    let dir = cancels_dir(home);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create cron cancels directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = marker_path(home, execution_id)?;
    let reason = reason
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_CANCEL_REASON);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&path) {
        Ok(mut file) => {
            let _ = file.write_all(reason.as_bytes());
            let _ = file.sync_all();
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("cannot create cancel marker: {error}")),
    }
}

/// Whether a cancel marker exists for `execution_id`.
pub fn cancel_requested(home: &Path, execution_id: &str) -> bool {
    marker_path(home, execution_id)
        .map(|path| path.is_file())
        .unwrap_or(false)
}

/// Read the cancellation reason if a marker exists for `execution_id`.
pub fn cancel_reason(home: &Path, execution_id: &str) -> Option<String> {
    let path = marker_path(home, execution_id).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    Some(if trimmed.is_empty() {
        DEFAULT_CANCEL_REASON.to_owned()
    } else {
        trimmed.to_owned()
    })
}

/// Remove the cancellation marker after a run finishes or acknowledges cancellation.
pub fn clear_cancel(home: &Path, execution_id: &str) {
    if let Ok(path) = marker_path(home, execution_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Prune `.cancel` files older than `STALE_MARKER_SECS`.
pub fn prune_stale_cancels(home: &Path) {
    let dir = cancels_dir(home);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(STALE_MARKER_SECS))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "cancel") {
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified < cutoff {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CancelResult {
    pub success: bool,
    pub cancelled: bool,
    pub job: JobSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    pub detail: String,
}

/// Request cancellation of a job's live execution, leaving its schedule untouched.
pub fn cancel_run(
    home: &Path,
    now: DateTime<Tz>,
    reference: &str,
    reason: Option<&str>,
) -> Result<CancelResult, String> {
    prune_stale_cancels(home);
    let job = with_jobs(home, now, |jobs| {
        let index = resolve_for(jobs, reference, "cancel")?;
        Ok((summarize_job(&jobs[index]), false))
    })?;
    let Some(live) = executions::find_live_execution(home, &job.id)? else {
        return Ok(CancelResult {
            success: true,
            cancelled: false,
            job,
            execution_id: None,
            pid: None,
            detail: "No live execution is currently running for this job.".into(),
        });
    };
    let created = request_cancel(home, &live.id, reason)?;
    let detail = if created {
        format!(
            "Cancellation marker written; the running job will stop on its next check (reason: {}).",
            reason
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or(DEFAULT_CANCEL_REASON)
        )
    } else {
        "Cancellation was already requested for this execution.".into()
    };
    Ok(CancelResult {
        success: true,
        cancelled: true,
        job,
        execution_id: Some(live.id),
        pid: Some(live.pid),
        detail,
    })
}
