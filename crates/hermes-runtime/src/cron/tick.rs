//! The scheduler tick: find due jobs, advance their schedules, and record run outcomes.
//!
//! A reduced port of `cron/scheduler_tick.py` and the due scan of `cron/jobs.py`. It shares
//! the reference's files (`jobs.json`, `.jobs.lock`, `.tick.lock`, `output/`), so a Python
//! gateway and this tick can serve one profile: the tick lock keeps them from overlapping.
//!
//! Ported: the cross-process tick lock; skipping paused, disabled and finished jobs; the
//! half-paused self-heal; recovery of a missing `next_run_at`; one-shot run claims and the
//! 120 s grace window; the catch-up grace for recurring jobs (`catch_up`, `grace_minutes`,
//! `cron.catch_up_missed`); advancing a recurring job before it runs (at most once across a
//! crash) with the `pending_slot` and `last_dispatch` stamps; and `mark_job_run`.
//! Not ported: the executions ledger, restoring an unclaimed `pending_slot`, repair of stale
//! cron lattices and timezone shifts, quota holds and unreachable-model retries.

use std::path::{Path, PathBuf};

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::{json, Map, Value};

use super::expr::CronExpr;
use super::schedule::{compute_next_run, iso_instant, parse_iso_instant, Schedule};
use super::store::{cron_dir, job_zone, with_jobs};

const ONESHOT_GRACE_SECONDS: i64 = 120;
pub(super) const ONESHOT_CLAIM_TTL_SECONDS: i64 = 1800;
/// How long a fire claim stays live without a heartbeat.
pub(super) const FIRE_CLAIM_TTL_SECONDS: i64 = 300;
const MIN_GRACE_SECONDS: i64 = 120;
const MAX_GRACE_SECONDS: i64 = 7200;
const LATE_TOLERANCE_SECONDS: f64 = 300.0;
const OUTPUT_KEEP: usize = 50;

/// Settings of the `cron` config section that the tick reads.
#[derive(Clone, Copy, Debug)]
pub struct TickSettings {
    /// `cron.catch_up_missed`: run a recurring job once for a missed slot.
    pub catch_up_missed: bool,
}

impl TickSettings {
    pub fn from_config(config: &serde_yaml::Value) -> Self {
        Self {
            catch_up_missed: config
                .get("cron")
                .and_then(|cron| cron.get("catch_up_missed"))
                .is_none_or(|value| value.as_bool() != Some(false)),
        }
    }
}

/// A job that is due and may run here.
#[derive(Debug)]
pub struct DueJob {
    pub id: String,
    pub name: String,
    pub record: Value,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub due: Vec<DueJob>,
    /// Due jobs that this tick cannot run, with the reason. They are left untouched.
    pub unsupported: Vec<(String, String)>,
    /// One-shots that missed their window and were removed.
    pub retired: Vec<String>,
}

/// Why a job's shape cannot run here, or `None`. Checked before anything is advanced so a
/// slot is never consumed without a run.
pub fn unsupported_reason(job: &Value) -> Option<String> {
    let text = |key: &str| {
        job[key]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let listed = |key: &str| job[key].as_array().is_some_and(|items| !items.is_empty());
    let feature = [
        (text("script").is_some(), "a pre-run script"),
        (job["no_agent"].as_bool() == Some(true), "no_agent mode"),
        (listed("skills") || text("skill").is_some(), "skills"),
        (
            !job["context_from"].is_null()
                && job["context_from"]
                    .as_str()
                    .is_none_or(|v| !v.trim().is_empty())
                && job["context_from"].as_array().is_none_or(|v| !v.is_empty()),
            "context_from",
        ),
        (text("workdir").is_some(), "a workdir"),
        (
            text("monitor_script").is_some() || text("monitor_url").is_some(),
            "a monitor",
        ),
        (
            text("provider").is_some() || text("base_url").is_some(),
            "a provider override",
        ),
        (text("interpreter").is_some(), "an interpreter"),
        (listed("enabled_toolsets"), "enabled_toolsets"),
        (text("manual_run_prompt").is_some(), "a manual run prompt"),
    ]
    .into_iter()
    .find_map(|(present, name)| present.then_some(name));
    if let Some(feature) = feature {
        return Some(format!("uses {feature}, which the Rust tick does not run"));
    }
    if text("prompt").is_none() {
        return Some("has no prompt".into());
    }
    let deliver = job["deliver"].as_str().unwrap_or("local");
    (deliver != "local")
        .then(|| format!("delivers to '{deliver}', and only local delivery is supported"))
}

/// This machine's claim stamp: `HERMES_MACHINE_ID`, or `host:pid`.
pub fn machine_id() -> String {
    if let Ok(explicit) = std::env::var("HERMES_MACHINE_ID") {
        if !explicit.trim().is_empty() {
            return explicit.trim().to_owned();
        }
    }
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into());
    format!("{host}:{}", std::process::id())
}

/// `grace_minutes` as seconds, or `None` when absent or invalid.
fn grace_override_seconds(job: &Value) -> Option<i64> {
    let minutes = match &job["grace_minutes"] {
        Value::Number(number) if number.is_i64() => number.as_i64()?,
        Value::String(text)
            if text.trim().chars().all(|c| c.is_ascii_digit()) && !text.trim().is_empty() =>
        {
            text.trim().parse().ok()?
        }
        _ => return None,
    };
    (0..=1440).contains(&minutes).then_some(minutes * 60)
}

/// Approximate period of a recurring schedule in seconds.
fn cadence_seconds(schedule: &Schedule, now: DateTime<Tz>, zone: Tz) -> Option<f64> {
    match schedule.kind.as_str() {
        "interval" => schedule.minutes.filter(|m| *m > 0).map(|m| m as f64 * 60.0),
        "cron" => {
            let expr = CronExpr::parse(schedule.expr.as_deref()?).ok()?;
            let wall = now.with_timezone(&zone).naive_local();
            let first = expr.next_after(wall)?;
            let second = expr.next_after(first)?;
            let gap = (second - first).num_seconds();
            (gap > 0).then_some(gap as f64)
        }
        _ => None,
    }
}

/// How late a job may be and still run for its own slot: half the period, within 2 minutes
/// to 2 hours, or an explicit `grace_minutes` (floored at the tick tolerance).
fn grace_seconds(job: &Value, schedule: &Schedule, now: DateTime<Tz>, zone: Tz) -> i64 {
    if let Some(seconds) = grace_override_seconds(job) {
        return seconds.max(MIN_GRACE_SECONDS);
    }
    match cadence_seconds(schedule, now, zone) {
        Some(period) => ((period as i64) / 2).clamp(MIN_GRACE_SECONDS, MAX_GRACE_SECONDS),
        None => MIN_GRACE_SECONDS,
    }
}

fn catches_up(job: &Value, schedule: &Schedule, settings: TickSettings) -> bool {
    if let Some(value) = job["catch_up"].as_bool() {
        return value;
    }
    if schedule.kind == "once" {
        return false;
    }
    // An explicit grace_minutes is the job's own catch-up limit.
    if grace_override_seconds(job).is_some() {
        return false;
    }
    settings.catch_up_missed
}

fn lateness_kind(lateness: f64, grace: i64) -> &'static str {
    if lateness > grace as f64 {
        "catch_up"
    } else if lateness > LATE_TOLERANCE_SECONDS {
        "late"
    } else {
        "on_time"
    }
}

pub(super) fn claim_is_live(claim: &Value, now: DateTime<Tz>, ttl: i64) -> bool {
    let Some(at) = claim["at"]
        .as_str()
        .and_then(|at| parse_iso_instant(at, now.timezone()))
    else {
        return false;
    };
    // A claim dated in the future is clock skew, never eternally fresh.
    let age = now.timestamp() - at;
    (0..ttl).contains(&age)
}

/// Take the cross-process tick lock without waiting. `Ok(None)` when another ticker holds it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn acquire_tick_lock(home: &Path) -> Result<Option<std::fs::File>, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = cron_dir(home);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create cron directory: {error}"))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(".tick.lock"))
        .map_err(|error| format!("cannot open the tick lock: {error}"))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(error)
            if error == rustix::io::Errno::WOULDBLOCK || error == rustix::io::Errno::ACCESS =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("cannot take the tick lock: {error}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn acquire_tick_lock(_home: &Path) -> Result<Option<std::fs::File>, String> {
    Err("the cron tick is not supported on this platform yet".into())
}

pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Find the jobs due at `now`, persist the repairs, claims and schedule advances, and return
/// the jobs to run.
pub fn scan_due(home: &Path, now: DateTime<Tz>, settings: TickSettings) -> Result<Scan, String> {
    with_jobs(home, now, |jobs| {
        let profile = now.timezone();
        let now_iso = iso_instant(now);
        let mut scan = Scan::default();
        let mut changed = false;
        let mut retire: Vec<usize> = Vec::new();
        let mut advance: Vec<usize> = Vec::new();
        for (index, job) in jobs.iter_mut().enumerate() {
            let id = job["id"].as_str().unwrap_or_default().to_owned();
            if matches!(job["state"].as_str(), Some("completed" | "error")) {
                continue;
            }
            if job.get("enabled").is_some_and(|value| !truthy(value)) {
                continue;
            }
            let paused_marker =
                job["state"].as_str() == Some("paused") || truthy(&job["paused_at"]);
            if paused_marker {
                // Pause must be authoritative: an enabled record with pause markers cannot fire.
                if let Some(record) = job.as_object_mut() {
                    record.insert("enabled".into(), json!(false));
                    record.insert("state".into(), json!("paused"));
                    if !record.get("paused_at").is_some_and(truthy) {
                        record.insert("paused_at".into(), json!(now_iso));
                    }
                    if !record.get("paused_reason").is_some_and(truthy) {
                        record.insert(
                            "paused_reason".into(),
                            json!("auto-disabled: enabled+paused contradiction"),
                        );
                    }
                    changed = true;
                }
                continue;
            }
            let Ok(schedule) = serde_json::from_value::<Schedule>(job["schedule"].clone()) else {
                continue;
            };
            let once = schedule.kind == "once";
            if once && claim_is_live(&job["run_claim"], now, ONESHOT_CLAIM_TTL_SECONDS) {
                continue;
            }
            let zone = job_zone(job);
            let schedule_zone = zone.unwrap_or(profile);
            let mut next_run = job["next_run_at"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned);
            if next_run.is_none() {
                // One-shots recover within their grace window; recurring jobs from now.
                let last = job["last_run_at"].as_str().map(str::to_owned);
                let recovered = if once {
                    compute_next_run(&schedule, last.as_deref(), zone, now)
                } else {
                    compute_next_run(&schedule, Some(&now_iso), zone, now)
                };
                let Some(recovered) = recovered else { continue };
                job["next_run_at"] = json!(recovered);
                changed = true;
                next_run = Some(recovered);
            }
            let Some(next_run) = next_run else { continue };
            let Some(due_at) = parse_iso_instant(&next_run, profile) else {
                continue;
            };
            if due_at > now.timestamp() {
                continue;
            }
            let lateness = (now.timestamp() - due_at) as f64;
            let name = job["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .unwrap_or(&id)
                .to_owned();
            if let Some(reason) = unsupported_reason(job) {
                scan.unsupported.push((id, reason));
                continue;
            }
            if once {
                if lateness > ONESHOT_GRACE_SECONDS as f64 && !catches_up(job, &schedule, settings)
                {
                    // Outside the window and no claim: it never fires. A claim means a run
                    // may be in flight elsewhere, so the record stays.
                    if !truthy(&job["run_claim"]) && !truthy(&job["fire_claim"]) {
                        write_missed_note(home, job, &next_run, now);
                        retire.push(index);
                        scan.retired.push(id);
                    }
                    continue;
                }
                let repeat = &job["repeat"];
                let times = repeat["times"].as_i64().filter(|t| *t > 0);
                if times.is_some_and(|times| repeat["completed"].as_i64().unwrap_or(0) >= times) {
                    retire.push(index);
                    scan.retired.push(id);
                    continue;
                }
                job["run_claim"] = json!({"at": now_iso, "by": machine_id()});
                changed = true;
            } else {
                let grace = grace_seconds(job, &schedule, now, schedule_zone);
                if lateness > grace as f64 {
                    // Collapse the backlog: skip to the next slot, and run once now only when
                    // the job catches up.
                    if let Some(following) = compute_next_run(&schedule, Some(&now_iso), zone, now)
                    {
                        let in_future = parse_iso_instant(&following, profile)
                            .is_some_and(|at| at > now.timestamp());
                        job["next_run_at"] = json!(following);
                        changed = true;
                        if in_future && !catches_up(job, &schedule, settings) {
                            continue;
                        }
                    }
                }
                let stamp = json!({
                    "scheduled_at": next_run,
                    "dispatched_at": now_iso,
                    "lateness_seconds": (lateness * 10.0).round() / 10.0,
                    "kind": lateness_kind(lateness, grace),
                });
                job["last_dispatch"] = stamp;
                // The slot is off the schedule before it runs: this stamp keeps it recoverable.
                job["pending_slot"] =
                    json!({"scheduled_at": next_run, "at": now_iso, "by": machine_id()});
                changed = true;
                advance.push(index);
            }
            scan.due.push(DueJob {
                id: job["id"].as_str().unwrap_or_default().to_owned(),
                name,
                record: job.clone(),
            });
        }
        // Advance recurring jobs before any of them runs: a crash mid-run then skips a slot
        // instead of re-firing it on every restart.
        for index in advance {
            let job = &mut jobs[index];
            let Ok(schedule) = serde_json::from_value::<Schedule>(job["schedule"].clone()) else {
                continue;
            };
            if let Some(following) = compute_next_run(&schedule, Some(&now_iso), job_zone(job), now)
            {
                if job["next_run_at"].as_str() != Some(following.as_str()) {
                    job["next_run_at"] = json!(following);
                    changed = true;
                }
            }
        }
        for index in retire.into_iter().rev() {
            jobs.remove(index);
            changed = true;
        }
        Ok((scan, changed))
    })
}

fn write_missed_note(home: &Path, job: &Value, next_run: &str, now: DateTime<Tz>) {
    let id = job["id"].as_str().unwrap_or("unknown");
    let note = format!(
        "# Cron job removed before firing (run time outside grace window)\n\n- job id: {id}\n- name: {}\n- scheduled run time: {next_run}\n- grace window: {ONESHOT_GRACE_SECONDS}s\n- removed at: {}\n\nThis one-shot's run time is more than the grace window in the past, so it was removed without running. Recreate it to schedule it again.\n",
        job["name"].as_str().unwrap_or(""),
        iso_instant(now),
    );
    if let Ok(path) = output_path(home, id, now, "missed-oneshot") {
        let _ = write_private(&path, &note);
    }
}

/// The output file of a run: `cron/output/<job>/<timestamp>[-suffix].md`.
fn output_path(home: &Path, id: &str, now: DateTime<Tz>, suffix: &str) -> Result<PathBuf, String> {
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
        return Err(format!("Invalid cron job id for output path: {id:?}"));
    }
    let stamp = now.format("%Y-%m-%d_%H-%M-%S");
    let name = if suffix.is_empty() {
        format!("{stamp}.md")
    } else {
        format!("{stamp}-{suffix}.md")
    };
    Ok(cron_dir(home).join("output").join(id).join(name))
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;

    let directory = path.parent().ok_or("invalid output path")?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create the output directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700));
    }
    let mut staged = tempfile::Builder::new()
        .prefix(".output_")
        .tempfile_in(directory)
        .map_err(|error| format!("cannot stage the output: {error}"))?;
    staged
        .write_all(text.as_bytes())
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|error| format!("cannot write the output: {error}"))?;
    staged
        .persist(path)
        .map_err(|error| format!("cannot save the output: {}", error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Save a run document and keep only the newest runs of the job.
pub fn save_output(
    home: &Path,
    id: &str,
    now: DateTime<Tz>,
    text: &str,
) -> Result<PathBuf, String> {
    let path = output_path(home, id, now, "")?;
    write_private(&path, text)?;
    if let Some(directory) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(directory) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
                .collect();
            files.sort();
            let excess = files.len().saturating_sub(OUTPUT_KEEP);
            for stale in files.into_iter().take(excess) {
                let _ = std::fs::remove_file(stale);
            }
        }
    }
    Ok(path)
}

/// Save a run document only while the job is still in the store. Taken under the store lock,
/// so it cannot interleave with a removal: a removed job's output is never written back.
pub fn save_output_if_present(
    home: &Path,
    id: &str,
    now: DateTime<Tz>,
    text: &str,
) -> Result<Option<PathBuf>, String> {
    with_jobs(home, now, |jobs| {
        if !jobs.iter().any(|job| job["id"].as_str() == Some(id)) {
            return Ok((None, false));
        }
        Ok((Some(save_output(home, id, now, text)?), false))
    })
}

/// The run document the reference writes: a header, the prompt, and the response. The
/// unframed form is what its `context_from` reader accepts for older runs.
pub fn run_document(job: &Value, prompt: &str, response: &str, now: DateTime<Tz>) -> String {
    format!(
        "# Cron Job: {}\n\n**Job ID:** {}\n**Run Time:** {}\n**Schedule:** {}\n\n## Prompt\n\n{prompt}\n\n## Response\n\n{response}\n",
        job["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("cron job"),
        job["id"].as_str().unwrap_or_default(),
        now.format("%Y-%m-%d %H:%M:%S"),
        job["schedule_display"].as_str().unwrap_or("N/A"),
    )
}

/// What a finished run reports to the store.
#[derive(Debug)]
pub struct Outcome<'a> {
    pub success: bool,
    pub error: Option<&'a str>,
    pub status: Option<&'a str>,
    /// The fire claim this run holds. A completion whose claim has since changed hands is
    /// discarded, as the reference does (`expected_fire_owner`).
    pub owner: Option<&'a str>,
}

/// `mark_job_run`: stamp the run, count it, and move the schedule on (or retire a finished
/// one-shot). A manual run on a slot that is still ahead is an extra fire and leaves both the
/// slot and the repeat budget alone.
pub fn mark_job_run(
    home: &Path,
    now: DateTime<Tz>,
    id: &str,
    outcome: &Outcome<'_>,
) -> Result<(), String> {
    with_jobs(home, now, |jobs| {
        let Some(job) = jobs.iter_mut().find(|job| job["id"].as_str() == Some(id)) else {
            // The job was removed while it ran.
            return Ok(((), false));
        };
        if let Some(owner) = outcome.owner {
            if job["fire_claim"]["by"].as_str() != Some(owner) {
                // The claim changed hands: this completion is stale.
                return Ok(((), false));
            }
        }
        let now_iso = iso_instant(now);
        let extra = truthy(&job["fire_claim"]["extra_slot"]);
        let held = if extra { held_markers(job) } else { Map::new() };
        let Some(record) = job.as_object_mut() else {
            return Ok(((), false));
        };
        record.insert("last_run_at".into(), json!(now_iso));
        for key in [
            "manual_run_at",
            "manual_run_restore_at",
            "manual_run_prompt",
        ] {
            record.shift_remove(key);
        }
        let cancelled = outcome.status == Some("cancelled");
        let status = outcome
            .status
            .unwrap_or(if outcome.success { "ok" } else { "error" });
        record.insert("last_status".into(), json!(status));
        let error_text = outcome
            .error
            .map(|text| text.chars().take(2000).collect::<String>());
        record.insert(
            "last_error".into(),
            if outcome.success || cancelled {
                Value::Null
            } else {
                json!(error_text)
            },
        );
        if outcome.success {
            record.shift_remove("preflight_alerted");
            record.shift_remove("last_fire_error");
            record.insert("failure_streak".into(), json!(0));
        } else if !cancelled {
            let streak = record
                .get("failure_streak")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1;
            record.insert("failure_streak".into(), json!(streak));
            record.insert(
                "last_failure".into(),
                json!({"at": now_iso, "detail": error_text.clone().unwrap_or_else(|| status.to_owned())}),
            );
        }
        record.insert("last_delivery_error".into(), Value::Null);
        record.insert("fire_claim".into(), Value::Null);
        record.shift_remove("pending_slot");
        if record
            .get("run_claim")
            .is_some_and(|claim| !claim.is_null())
        {
            record.insert("run_claim".into(), Value::Null);
        }
        if let Some(record) = job.as_object_mut() {
            for (key, value) in held {
                record.insert(key, value);
            }
        }
        if extra {
            settle_extra_fire(job, now);
        } else {
            advance_after_run(job, now);
        }
        Ok(((), true))
    })
}

/// A run-now queued by the reference (`manual_run_at` equal to `next_run_at`, with the slot it
/// overwrote saved) and not yet claimed.
fn manual_run_queued(job: &Value) -> bool {
    !job["manual_run_at"].is_null()
        && job["manual_run_at"] == job["next_run_at"]
        && job.get("manual_run_restore_at").is_some()
}

/// The run-now markers of a trigger that landed while this extra fire was in flight: it is its
/// own run, so the outcome must not clear them.
fn held_markers(job: &Value) -> Map<String, Value> {
    let mut held = Map::new();
    if manual_run_queued(job) {
        for key in [
            "manual_run_at",
            "manual_run_restore_at",
            "manual_run_prompt",
        ] {
            if let Some(value) = job.get(key) {
                held.insert(key.to_owned(), value.clone());
            }
        }
    }
    held
}

/// After an extra fire: the repeat budget is untouched and the job keeps its pending slot. A
/// slot that passed during the run is taken by a one-shot at once; a recurring job drops it for
/// its next natural slot.
fn settle_extra_fire(job: &mut Value, now: DateTime<Tz>) {
    let in_future = job["next_run_at"].as_str().is_some_and(|slot| {
        parse_iso_instant(slot, now.timezone()).is_some_and(|at| at > now.timestamp())
    });
    if manual_run_queued(job) || in_future {
        return;
    }
    let now_iso = iso_instant(now);
    if job["schedule"]["kind"].as_str() == Some("once") {
        job["next_run_at"] = json!(now_iso);
        return;
    }
    if let Ok(schedule) = serde_json::from_value::<Schedule>(job["schedule"].clone()) {
        if let Some(next) = compute_next_run(&schedule, Some(&now_iso), job_zone(job), now) {
            job["next_run_at"] = json!(next);
        }
    }
}

fn complete(job: &mut Value) {
    if let Some(record) = job.as_object_mut() {
        record.insert("enabled".into(), json!(false));
        record.insert("state".into(), json!("completed"));
        record.insert("next_run_at".into(), Value::Null);
    }
}

/// Count the run and compute the next one; retire the job at its repeat limit or when a
/// one-shot has nothing left.
fn advance_after_run(job: &mut Value, now: DateTime<Tz>) {
    let kind = job["schedule"]["kind"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if job["repeat"].is_object() {
        let times = job["repeat"]["times"].as_i64();
        let finite = times.is_some_and(|times| times > 0);
        let mut completed = job["repeat"]["completed"].as_i64().unwrap_or(0);
        // A finite one-shot was counted when it was claimed.
        if !(kind == "once" && finite && completed > 0) {
            completed += 1;
            job["repeat"]["completed"] = json!(completed);
        }
        if finite && times.is_some_and(|times| completed >= times) {
            complete(job);
            return;
        }
    }
    let Ok(schedule) = serde_json::from_value::<Schedule>(job["schedule"].clone()) else {
        return;
    };
    let now_iso = iso_instant(now);
    let next = compute_next_run(&schedule, Some(&now_iso), job_zone(job), now);
    match next {
        Some(next) => {
            job["next_run_at"] = json!(next);
            if job["state"].as_str() != Some("paused") {
                job["state"] = json!("scheduled");
            }
        }
        None if matches!(kind.as_str(), "cron" | "interval") => {
            // A recurring schedule that cannot compute must never be silently disabled.
            job["state"] = json!("error");
            if !truthy(&job["last_error"]) {
                job["last_error"] = json!("Failed to compute next run for recurring schedule");
            }
        }
        None => complete(job),
    }
}

/// Pause an unrunnable job so that it does not fail on every tick.
pub fn block_and_pause(
    home: &Path,
    now: DateTime<Tz>,
    id: &str,
    reason: &str,
) -> Result<(), String> {
    with_jobs(home, now, |jobs| {
        let Some(job) = jobs.iter_mut().find(|job| job["id"].as_str() == Some(id)) else {
            return Ok(((), false));
        };
        if let Some(record) = job.as_object_mut() {
            record.insert("enabled".into(), json!(false));
            record.insert("state".into(), json!("paused"));
            record.insert("paused_at".into(), json!(iso_instant(now)));
            record.insert(
                "paused_reason".into(),
                json!(format!("Auto-paused by scheduler: {reason}")),
            );
            record.shift_remove("pending_slot");
            record.insert("run_claim".into(), Value::Null);
        }
        Ok(((), true))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn schedule(kind: &str, minutes: Option<u64>, expr: Option<&str>) -> Schedule {
        Schedule {
            kind: kind.into(),
            display: String::new(),
            run_at: None,
            minutes,
            expr: expr.map(str::to_owned),
        }
    }

    #[test]
    fn grace_is_half_the_period_within_bounds_unless_the_job_sets_it() {
        let zone: Tz = "UTC".parse().unwrap();
        let now = zone.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let job = json!({});
        assert_eq!(
            grace_seconds(&job, &schedule("interval", Some(1), None), now, zone),
            120
        );
        assert_eq!(
            grace_seconds(&job, &schedule("interval", Some(60), None), now, zone),
            1800
        );
        assert_eq!(
            grace_seconds(&job, &schedule("interval", Some(1440), None), now, zone),
            7200
        );
        assert_eq!(
            grace_seconds(&job, &schedule("cron", None, Some("0 9 * * *")), now, zone),
            7200
        );
        assert_eq!(
            grace_seconds(
                &json!({"grace_minutes": 0}),
                &schedule("interval", Some(60), None),
                now,
                zone
            ),
            120
        );
        assert_eq!(
            grace_seconds(
                &json!({"grace_minutes": "30"}),
                &schedule("interval", Some(60), None),
                now,
                zone
            ),
            1800
        );
        assert_eq!(
            grace_seconds(
                &json!({"grace_minutes": 5000}),
                &schedule("interval", Some(60), None),
                now,
                zone
            ),
            1800
        );
    }

    #[test]
    fn catch_up_follows_the_job_then_the_schedule_then_the_setting() {
        let on = TickSettings {
            catch_up_missed: true,
        };
        let off = TickSettings {
            catch_up_missed: false,
        };
        let every = schedule("interval", Some(60), None);
        let once = schedule("once", None, None);
        assert!(catches_up(&json!({}), &every, on));
        assert!(!catches_up(&json!({}), &every, off));
        assert!(catches_up(&json!({"catch_up": true}), &every, off));
        assert!(!catches_up(&json!({"catch_up": false}), &every, on));
        assert!(!catches_up(&json!({}), &once, on));
        assert!(!catches_up(&json!({"grace_minutes": 10}), &every, on));
    }

    #[test]
    fn jobs_with_unsupported_features_are_named_before_anything_changes() {
        let plain = json!({"prompt": "p", "deliver": "local", "skills": [], "context_from": null});
        assert_eq!(unsupported_reason(&plain), None);
        for (field, value, needle) in [
            ("script", json!("a.py"), "pre-run script"),
            ("skills", json!(["x"]), "skills"),
            ("context_from", json!("other"), "context_from"),
            ("workdir", json!("/tmp"), "workdir"),
            ("provider", json!("openrouter"), "provider override"),
            ("deliver", json!("telegram"), "only local delivery"),
        ] {
            let mut job = plain.clone();
            job[field] = value;
            let reason = unsupported_reason(&job).unwrap();
            assert!(reason.contains(needle), "{field}: {reason}");
        }
        assert_eq!(
            unsupported_reason(&json!({"prompt": " "})).as_deref(),
            Some("has no prompt")
        );
    }
}
