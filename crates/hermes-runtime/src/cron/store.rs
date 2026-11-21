//! Read-only access to `cron/jobs.json` in the layout used by the Python reference.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

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

fn summarize(job: &Value) -> JobSummary {
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
