//! `hermes cron add|edit|run` and the update core that pause and resume share.
//!
//! Mirrors `create_job`, `update_job`, `claim_job_for_fire` and the tool layer in front of them
//! (`tools/cronjob_tools.py`) of the Python reference, including the order in which a bad input
//! is reported and the key order of the records written, so that either side reads the other's
//! `jobs.json` unchanged. Features that this version does not run (scripts, skills, monitors,
//! `workdir`, `context_from`, a provider override, delivery other than local) are refused by the
//! command line, never stored.

use std::path::Path;

use chrono::DateTime;
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};

use super::schedule::{compute_next_run, iso_instant, parse_iso_instant, parse_schedule, Schedule};
use super::store::{job_zone, summarize, with_jobs};
use super::tick::{
    claim_is_live, truthy, unsupported_reason, FIRE_CLAIM_TTL_SECONDS, ONESHOT_CLAIM_TTL_SECONDS,
};

/// Seconds a one-shot stays eligible after its time.
const ONESHOT_GRACE_SECONDS: i64 = 120;
const EMPTY_PAYLOAD_ERROR: &str = "Cron job has nothing to run: the prompt is blank and no script or skill(s) are set. Provide a prompt, a script, or at least one skill.";

/// Python's `repr` of a string, which the reference puts in its error messages.
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `str.strip()`: whitespace as `str.isspace` sees it.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

fn schedule_of(job: &Value) -> Option<Schedule> {
    serde_json::from_value(job["schedule"].clone()).ok()
}

/// `compute_next_run(schedule, timezone=...)` at `now`.
fn next_run_of(schedule: &Value, zone: Option<Tz>, now: DateTime<Tz>) -> Option<String> {
    let schedule: Schedule = serde_json::from_value(schedule.clone()).ok()?;
    compute_next_run(&schedule, None, zone, now)
}

fn once_past_error(run_at: &str) -> String {
    format!(
        "Requested one-shot time {run_at} is more than {ONESHOT_GRACE_SECONDS}s in the past and cannot be scheduled."
    )
}

/// `jobs_time.normalize_timezone`: a canonical IANA key, or `None` for "inherit the profile".
pub fn normalize_timezone(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = value else { return Ok(None) };
    let name = py_strip(raw);
    if name.is_empty() {
        return Ok(None);
    }
    name.parse::<Tz>().map_err(|_| {
        format!(
            "Unknown timezone {}. Use an IANA key from `zoneinfo.available_timezones()`, e.g. 'Asia/Hong_Kong'.",
            py_repr(name)
        )
    })?;
    Ok(Some(name.to_owned()))
}

/// `jobs_time.normalize_grace_minutes` for the text the command line passes.
pub fn normalize_grace_minutes(value: Option<&str>) -> Result<Option<i64>, String> {
    let Some(raw) = value else { return Ok(None) };
    let text = py_strip(raw);
    if text.is_empty() {
        return Ok(None);
    }
    let not_whole = || {
        format!(
            "grace_minutes must be a whole number of minutes, not {}.",
            py_repr(raw)
        )
    };
    let digits = text.trim_start_matches(['+', '-']);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(not_whole());
    }
    let minutes: i128 = text.parse().map_err(|_| not_whole())?;
    if !(0..=1440).contains(&minutes) {
        return Err(format!(
            "grace_minutes must be between 0 and 1440 (0 = never catch up), got {minutes}."
        ));
    }
    Ok(Some(minutes as i64))
}

/// `validate_schedule_knobs`: a grace window only means something to a cron schedule.
fn validate_grace_kind(kind: Option<&str>, grace: Option<i64>) -> Result<(), String> {
    if grace.is_none() || kind == Some("cron") {
        return Ok(());
    }
    Err(format!(
        "grace_minutes only applies to cron schedules (a fixed wall clock that can be missed); this job's schedule kind is {}.",
        kind.map_or_else(|| "None".to_owned(), py_repr)
    ))
}

/// `normalize_repeat_value` for an integer: zero and negative mean "forever".
fn repeat_times(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

/// A new random hex string of `bytes` bytes.
pub fn random_hex(bytes: usize) -> Result<String, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut raw = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut raw)
        .map_err(|_| "cannot gather random bytes for an ID".to_owned())?;
    Ok(raw.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Delivery is local-only in this version: `local` (or nothing) is stored, anything else is refused.
fn local_only(value: &str, flag: &str) -> Result<Option<String>, String> {
    let text = py_strip(value);
    match text {
        "" => Ok(None),
        "local" => Ok(Some("local".to_owned())),
        other => Err(format!(
            "{flag} {} is not supported: this version delivers results to local output only (use 'local')",
            py_repr(other)
        )),
    }
}

/// The `--wrap-response` family on `cron edit`: `inherit` removes the override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapResponse {
    On,
    Off,
    Inherit,
}

impl<'de> Deserialize<'de> for WrapResponse {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Bool(true) => Ok(Self::On),
            Value::Bool(false) => Ok(Self::Off),
            Value::String(text) if text == "inherit" => Ok(Self::Inherit),
            other => Err(serde::de::Error::custom(format!(
                "invalid wrap_response {other}"
            ))),
        }
    }
}

/// What `hermes cron add` was given. Field names follow the reference's argument names.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AddParams {
    pub schedule: String,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub deliver: Option<String>,
    pub failure_deliver: Option<String>,
    pub repeat: Option<i64>,
    pub catch_up: Option<bool>,
    pub wrap_response: Option<bool>,
    pub model: Option<String>,
    pub timezone: Option<String>,
    pub grace_minutes: Option<String>,
    pub paused: bool,
    pub paused_reason: Option<String>,
}

/// What `hermes cron edit` was given.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditParams {
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub deliver: Option<String>,
    pub failure_deliver: Option<String>,
    pub repeat: Option<i64>,
    pub catch_up: Option<bool>,
    pub wrap_response: Option<WrapResponse>,
    pub model: Option<String>,
    pub timezone: Option<String>,
    pub grace_minutes: Option<String>,
}

/// Create a job with a fresh random ID and save it.
pub fn add_job(home: &Path, now: DateTime<Tz>, params: &AddParams) -> Result<Value, String> {
    add_job_with_id(home, now, &random_hex(6)?, params)
}

/// Create a job with the given ID and save it. Returns the stored record.
pub fn add_job_with_id(
    home: &Path,
    now: DateTime<Tz>,
    id: &str,
    params: &AddParams,
) -> Result<Value, String> {
    if params.schedule.is_empty() {
        return Err("schedule is required for create".into());
    }
    let prompt = params.prompt.as_deref().unwrap_or_default();
    if prompt.is_empty() {
        return Err("create requires either prompt or at least one skill".into());
    }
    if let Some(threat) = super::scan::scan_cron_prompt(prompt) {
        return Err(threat);
    }
    let deliver = match params.deliver.as_deref() {
        Some(value) => local_only(value, "--deliver")?,
        None => None,
    };
    let failure_deliver = match params.failure_deliver.as_deref() {
        Some(value) => local_only(value, "--failure-deliver")?,
        None => None,
    };
    let model = params
        .model
        .as_deref()
        .map(py_strip)
        .filter(|model| !model.is_empty());

    // From here on this is `create_job`, whose checks run in this order.
    if params.paused_reason.is_some() && !params.paused {
        return Err("paused_reason requires paused=True.".into());
    }
    let schedule = parse_schedule(&params.schedule, now)?;
    let mut repeat = params.repeat.and_then(repeat_times);
    if schedule.kind == "once" && repeat.is_none() {
        repeat = Some(1);
    }
    let timezone = normalize_timezone(params.timezone.as_deref().map(py_strip))?;
    let grace = normalize_grace_minutes(params.grace_minutes.as_deref())?;
    validate_grace_kind(Some(&schedule.kind), grace)?;
    let prompt_text = py_strip(prompt);
    if prompt_text.is_empty() {
        return Err(EMPTY_PAYLOAD_ERROR.into());
    }
    let name = match params.name.as_deref().filter(|name| !name.is_empty()) {
        Some(name) => name.to_owned(),
        None => {
            let label: String = prompt_text.chars().take(50).collect();
            py_strip(&label).to_owned()
        }
    };
    let zone = timezone.as_deref().and_then(|name| name.parse::<Tz>().ok());
    let next_run_at = compute_next_run(&schedule, None, zone, now);
    if schedule.kind == "once" && next_run_at.is_none() {
        return Err(once_past_error(
            schedule.run_at.as_deref().unwrap_or(&params.schedule),
        ));
    }

    let stamp = iso_instant(now);
    let paused = params.paused;
    let mut job = json!({
        "id": id,
        "name": name,
        "prompt": prompt_text,
        "skills": [],
        "skill": null,
        "model": model,
        "provider": null,
        "base_url": null,
        "script": null,
        "no_agent": false,
        "monitor_script": null,
        "monitor_url": null,
        "monitor_state": null,
        "context_from": null,
        "schedule": schedule,
        "schedule_display": schedule.display,
        "repeat": {"times": repeat, "completed": 0},
        "enabled": !paused,
        "state": if paused { "paused" } else { "scheduled" },
        "paused_at": if paused { json!(stamp) } else { Value::Null },
        "paused_reason": if paused {
            let reason = py_strip(params.paused_reason.as_deref().unwrap_or_default());
            json!(if reason.is_empty() { "Created paused; awaiting operator approval." } else { reason })
        } else {
            Value::Null
        },
        "created_at": stamp,
        "next_run_at": if paused { Value::Null } else { json!(next_run_at) },
        "last_run_at": null,
        "last_status": null,
        "last_error": null,
        "last_delivery_error": null,
        "last_delivery_unverified": null,
        "last_failure": null,
        "failure_streak": 0,
        "deliver": deliver.unwrap_or_else(|| "local".to_owned()),
        "origin": null,
        "enabled_toolsets": null,
        "workdir": null,
    });
    // Optional keys are written only when set, as the reference does.
    let record = job.as_object_mut().ok_or("invalid job record")?;
    for (key, value) in [
        ("failure_deliver", failure_deliver.map(Value::from)),
        ("timezone", timezone.map(Value::from)),
        ("grace_minutes", grace.map(Value::from)),
        ("catch_up", params.catch_up.map(Value::from)),
        ("wrap_response", params.wrap_response.map(Value::from)),
    ] {
        if let Some(value) = value {
            record.insert(key.into(), value);
        }
    }
    let saved = job.clone();
    with_jobs(home, now, |jobs| {
        jobs.push(job);
        Ok(((), true))
    })?;
    Ok(saved)
}

/// A job named by an ID or, failing that, a case-insensitive name, as `resolve_job_ref` finds it.
pub(super) enum Lookup {
    Found(usize),
    Missing,
    Ambiguous(Vec<usize>),
}

pub(super) fn lookup(jobs: &[Value], reference: &str) -> Lookup {
    if reference.is_empty() {
        return Lookup::Missing;
    }
    if let Some(index) = jobs
        .iter()
        .position(|job| job["id"].as_str() == Some(reference))
    {
        return Lookup::Found(index);
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
        [] => Lookup::Missing,
        [index] => Lookup::Found(*index),
        _ => Lookup::Ambiguous(matches),
    }
}

fn ambiguous_text(jobs: &[Value], reference: &str, matches: &[usize]) -> String {
    let ids: Vec<&str> = matches
        .iter()
        .map(|index| jobs[*index]["id"].as_str().unwrap_or("?"))
        .collect();
    format!(
        "Job name '{reference}' is ambiguous — matches {} jobs: {}. Use the job ID instead.",
        matches.len(),
        ids.join(", ")
    )
}

/// The lookup the reference's tool layer does for `pause`, `resume`, `run` and `remove`.
pub(super) fn resolve_for(jobs: &[Value], reference: &str, action: &str) -> Result<usize, String> {
    if reference.is_empty() {
        return Err(format!("job_id is required for action '{action}'"));
    }
    match lookup(jobs, reference) {
        Lookup::Found(index) => Ok(index),
        Lookup::Missing => Err(format!(
            "Job with ID or name '{reference}' not found. Use cronjob(action='list') to inspect jobs."
        )),
        Lookup::Ambiguous(matches) => Err(ambiguous_text(jobs, reference, &matches)),
    }
}

/// The lookup `hermes cron edit` does itself: its own wording, and the candidates listed.
fn resolve_edit(jobs: &[Value], reference: &str) -> Result<usize, String> {
    match lookup(jobs, reference) {
        Lookup::Found(index) => Ok(index),
        Lookup::Missing => Err(format!("Job not found: {reference}")),
        Lookup::Ambiguous(matches) => {
            let mut text = ambiguous_text(jobs, reference, &matches);
            for index in matches {
                text.push_str(&format!(
                    "\n  {}  (name: {})",
                    jobs[index]["id"].as_str().unwrap_or("?"),
                    py_repr(jobs[index]["name"].as_str().unwrap_or_default())
                ));
            }
            Err(text)
        }
    }
}

/// Edit a job. Returns the stored record.
pub fn edit_job(
    home: &Path,
    now: DateTime<Tz>,
    reference: &str,
    params: &EditParams,
) -> Result<Value, String> {
    with_jobs(home, now, |jobs| {
        let index = resolve_edit(jobs, reference)?;
        let state = summarize(&jobs[index]).state;
        let mut updates = Map::new();

        // The tool layer's checks, in its order: the first failing field wins.
        if let Some(prompt) = &params.prompt {
            if let Some(threat) = super::scan::scan_cron_prompt(prompt) {
                return Err(threat);
            }
            updates.insert("prompt".into(), json!(prompt));
        }
        if let Some(name) = params
            .name
            .as_deref()
            .filter(|name| !py_strip(name).is_empty())
        {
            updates.insert("name".into(), json!(name));
        }
        if let Some(deliver) = &params.deliver {
            updates.insert("deliver".into(), json!(local_only(deliver, "--deliver")?));
        }
        if let Some(failure) = &params.failure_deliver {
            updates.insert(
                "failure_deliver".into(),
                json!(local_only(failure, "--failure-deliver")?),
            );
        }
        if let Some(catch_up) = params.catch_up {
            updates.insert("catch_up".into(), json!(catch_up));
        }
        match params.wrap_response {
            Some(WrapResponse::On) => {
                updates.insert("wrap_response".into(), json!(true));
            }
            Some(WrapResponse::Off) => {
                updates.insert("wrap_response".into(), json!(false));
            }
            Some(WrapResponse::Inherit) => {
                updates.insert("wrap_response".into(), Value::Null);
            }
            None => {}
        }
        if let Some(model) = &params.model {
            let model = py_strip(model);
            updates.insert(
                "model".into(),
                if model.is_empty() {
                    Value::Null
                } else {
                    json!(model)
                },
            );
        }
        if let Some(times) = params.repeat {
            let mut repeat = jobs[index]["repeat"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            repeat.insert("times".into(), json!(repeat_times(times)));
            updates.insert("repeat".into(), Value::Object(repeat));
        }
        if let Some(timezone) = &params.timezone {
            updates.insert("timezone".into(), json!(py_strip(timezone)));
        }
        if let Some(grace) = &params.grace_minutes {
            let value = if py_strip(grace).is_empty() {
                Value::Null
            } else {
                json!(grace)
            };
            updates.insert("grace_minutes".into(), value.clone());
            // Checked against the schedule as stored: a schedule given in the same edit is not
            // placed in the updates yet.
            if !value.is_null() && jobs[index]["schedule"]["kind"].as_str() != Some("cron") {
                return Err("grace_minutes only applies to cron schedules.".into());
            }
        }
        if let Some(text) = &params.schedule {
            let parsed = parse_schedule(text, now)?;
            updates.insert("schedule_display".into(), json!(parsed.display));
            updates.insert("schedule".into(), json!(parsed));
            // The display goes first in the reference's updates only when it was placed first;
            // the key order of a record is that of the stored fields, so this is not visible.
            if state != "paused" {
                updates.insert("state".into(), json!("scheduled"));
                updates.insert("enabled".into(), json!(true));
            }
        }
        if updates.is_empty() {
            return Err("No updates provided.".into());
        }
        update_job(jobs, index, updates, now)?;
        Ok((jobs[index].clone(), true))
    })
}

fn is_terminal(job: &Value) -> bool {
    matches!(job["state"].as_str(), Some("completed" | "error"))
}

/// `_reject_terminal_activation`: a finished job is not revived through an update.
fn reject_terminal_activation(job: &Value, updated: &Value) -> Result<(), String> {
    let recoverable = job["state"].as_str() == Some("error")
        && matches!(job["schedule"]["kind"].as_str(), Some("cron" | "interval"));
    if !is_terminal(job) || recoverable {
        return Ok(());
    }
    let activates = !is_terminal(updated)
        || updated["enabled"] == Value::Bool(true)
        || !updated["next_run_at"].is_null();
    if !activates {
        return Ok(());
    }
    let name = match job.get("name") {
        Some(Value::String(name)) => name.clone(),
        Some(Value::Null) => "None".to_owned(),
        Some(other) => other.to_string(),
        None => job["id"].as_str().unwrap_or_default().to_owned(),
    };
    Err(format!(
        "Cannot activate terminal cron job '{name}' through update_job; use cron resume --run-now or --at."
    ))
}

/// `_normalize_skill_list` for a stored record: the unique, stripped skill names.
fn skill_list(job: &Value) -> Vec<String> {
    let text = |value: &Value| match value {
        Value::Null | Value::Bool(false) => String::new(),
        Value::String(text) => py_strip(text).to_owned(),
        other => py_strip(&other.to_string()).to_owned(),
    };
    let raw: Vec<Value> = match job.get("skills") {
        None | Some(Value::Null) => match &job["skill"] {
            Value::Null => Vec::new(),
            Value::String(skill) if skill.is_empty() => Vec::new(),
            skill => vec![skill.clone()],
        },
        Some(Value::String(skill)) => vec![json!(skill)],
        Some(Value::Array(items)) => items.clone(),
        Some(_) => Vec::new(),
    };
    let mut names: Vec<String> = Vec::new();
    for item in &raw {
        let name = text(item);
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// `_apply_skill_fields`: the canonical `skills` and legacy `skill` fields, in place.
fn apply_skill_fields(job: &mut Value) {
    let skills = skill_list(job);
    let first = skills.first().cloned();
    if let Some(record) = job.as_object_mut() {
        record.insert("skills".into(), json!(skills));
        record.insert("skill".into(), json!(first));
    }
}

fn text_of(job: &Value, key: &str) -> String {
    match job.get(key) {
        Some(Value::String(text)) => py_strip(text).to_owned(),
        Some(Value::Null) | None => String::new(),
        Some(other) => py_strip(&other.to_string()).to_owned(),
    }
}

/// `job_payload_is_empty`: nothing to run, though a payload field is present.
fn payload_is_empty(job: &Value) -> bool {
    if !text_of(job, "prompt").is_empty() || !text_of(job, "script").is_empty() {
        return false;
    }
    if !skill_list(job).is_empty() {
        return false;
    }
    ["prompt", "script", "skill", "skills"]
        .iter()
        .any(|key| job.get(*key).is_some())
}

/// `update_job`: merge `updates` into the job at `index`, refresh the schedule fields they
/// affect, and check what the reference checks, in its order. Nothing is written on an error.
pub(super) fn update_job(
    jobs: &mut [Value],
    index: usize,
    mut updates: Map<String, Value>,
    now: DateTime<Tz>,
) -> Result<(), String> {
    let job = jobs[index].clone();

    // A schedule that changes kind re-derives the repeat default, unless the edit sets it.
    if updates.contains_key("schedule") && !updates.contains_key("repeat") {
        let new_kind = updates["schedule"]["kind"].as_str().map(str::to_owned);
        let old_kind = job["schedule"]["kind"].as_str().map(str::to_owned);
        if old_kind != new_kind {
            let mut repeat = job["repeat"].as_object().cloned().unwrap_or_default();
            let times = repeat.get("times").cloned().unwrap_or(Value::Null);
            let changed = if new_kind.as_deref() == Some("once") && times.is_null() {
                repeat.insert("times".into(), json!(1));
                true
            } else if new_kind.as_deref() != Some("once")
                && old_kind.as_deref() == Some("once")
                && times == json!(1)
            {
                repeat.insert("times".into(), Value::Null);
                true
            } else {
                false
            };
            if changed {
                repeat.entry("completed").or_insert(json!(0));
                updates.insert("repeat".into(), Value::Object(repeat));
            }
        }
    }

    // Values are normalized before anything is merged: the zone, then the grace window.
    if let Some(value) = updates.get("timezone").cloned() {
        updates.insert(
            "timezone".into(),
            json!(normalize_timezone(value.as_str())?),
        );
    }
    if let Some(value) = updates.get("grace_minutes").cloned() {
        let text = match &value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        };
        updates.insert(
            "grace_minutes".into(),
            json!(normalize_grace_minutes(text.as_deref())?),
        );
    }
    if let Some(Value::Object(repeat)) = updates.get("repeat").cloned() {
        let mut repeat = repeat;
        let times = repeat
            .get("times")
            .and_then(Value::as_i64)
            .and_then(repeat_times);
        repeat.insert("times".into(), json!(times));
        if !repeat.contains_key("completed") {
            let completed = job["repeat"]["completed"].as_i64().unwrap_or(0);
            repeat.insert("completed".into(), json!(completed));
        }
        updates.insert("repeat".into(), Value::Object(repeat));
    }

    let mut updated = job.clone();
    if let Some(record) = updated.as_object_mut() {
        for (key, value) in &updates {
            record.insert(key.clone(), value.clone());
        }
    }
    apply_skill_fields(&mut updated);
    reject_terminal_activation(&job, &updated)?;
    if ["prompt", "script", "skill", "skills", "no_agent"]
        .iter()
        .any(|key| updates.contains_key(*key))
        && payload_is_empty(&updated)
    {
        return Err(EMPTY_PAYLOAD_ERROR.into());
    }
    if updates.contains_key("timezone") || updates.contains_key("grace_minutes") {
        validate_grace_kind(
            updated["schedule"]["kind"].as_str(),
            updated["grace_minutes"].as_i64(),
        )?;
        // A cleared knob is dropped, not stored as null.
        if let Some(record) = updated.as_object_mut() {
            if record.get("timezone").is_some_and(|value| !truthy(value)) {
                record.shift_remove("timezone");
            }
            if record.get("grace_minutes").is_some_and(Value::is_null) {
                record.shift_remove("grace_minutes");
            }
        }
    }
    let paused = updated["state"].as_str() == Some("paused");
    if updates.contains_key("schedule") {
        let display = updates
            .get("schedule_display")
            .cloned()
            .or_else(|| updated["schedule"].get("display").cloned())
            .unwrap_or_else(|| updated["schedule_display"].clone());
        updated["schedule_display"] = display;
        if !paused {
            let next = next_run_of(&updated["schedule"], job_zone(&updated), now);
            if updated["schedule"]["kind"].as_str() == Some("once") && next.is_none() {
                let run_at = updated["schedule"]["run_at"].as_str().unwrap_or_default();
                return Err(once_past_error(run_at));
            }
            updated["next_run_at"] = json!(next);
        }
        // A new schedule drops the quota hold parked for the old one.
        if let Some(record) = updated.as_object_mut() {
            record.shift_remove("quota_hold_until");
            record.shift_remove("quota_hold_cron_expr");
        }
    } else if updates.contains_key("timezone") && !paused {
        // The instant on file was computed in the old zone.
        if let Some(next) = next_run_of(&updated["schedule"], job_zone(&updated), now) {
            updated["next_run_at"] = json!(next);
        }
    }
    if ["schedule", "next_run_at", "enabled", "state", "timezone"]
        .iter()
        .any(|key| updates.contains_key(*key))
    {
        if let Some(record) = updated.as_object_mut() {
            record.shift_remove("pending_slot");
        }
    }
    // An enabled, unpaused record must never be stored without a next run.
    let enabled = updated.get("enabled").is_none_or(truthy);
    if enabled && updated["state"].as_str() != Some("paused") && !truthy(&updated["next_run_at"]) {
        let next = next_run_of(&updated["schedule"], job_zone(&updated), now);
        if next.is_none() && updated["schedule"]["kind"].as_str() == Some("once") {
            let run_at = updated["schedule"]["run_at"].as_str().unwrap_or("unknown");
            return Err(format!(
                "Requested one-shot time {run_at} is in the past (grace window: {ONESHOT_GRACE_SECONDS}s) and cannot be scheduled."
            ));
        }
        updated["next_run_at"] = json!(next);
    }
    reject_terminal_activation(&job, &updated)?;
    jobs[index] = updated;
    Ok(())
}

/// The record a manual run was claimed on, or why it was not.
#[derive(Debug)]
pub enum ManualClaim {
    Claimed(Box<Value>),
    Skipped(String),
}

/// Whether a record's owner is provably gone: the claim names a process on this host
/// (`host:pid[:token]`) that no longer exists.
fn owner_is_dead(claim: &Value) -> bool {
    let by = claim["by"].as_str().unwrap_or_default();
    let mut parts = by.split(':');
    let (Some(host), Some(pid)) = (parts.next(), parts.next()) else {
        return false;
    };
    let Ok(pid) = pid.parse::<i32>() else {
        return false;
    };
    let own = super::tick::machine_id();
    if own.split(':').next() != Some(host) {
        return false;
    }
    pid_is_gone(pid)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pid_is_gone(pid: i32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return false;
    };
    matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pid_is_gone(_pid: i32) -> bool {
    false
}

/// A claim that is young enough and whose owner is not provably dead.
fn live_claim(claim: &Value, now: DateTime<Tz>, ttl: i64) -> bool {
    claim_is_live(claim, now, ttl) && !owner_is_dead(claim)
}

pub(super) fn fire_claim_is_live(claim: &Value, now: DateTime<Tz>) -> bool {
    live_claim(claim, now, FIRE_CLAIM_TTL_SECONDS)
}

const REARM_RECURRING_ERROR: &str =
    "Cannot re-arm recurring jobs: re-arm is one-shot-only; use plain resume or cron run.";

/// `rearm_oneshot`: give a finished one-shot a new occurrence at `run_at` (an ISO time, or the
/// current time for a run-now). Returns the stored record. A lookup that finds nothing is
/// reported as the reference's `Job not found: ...`.
pub fn rearm_oneshot(
    home: &Path,
    now: DateTime<Tz>,
    reference: &str,
    run_at: &str,
) -> Result<Value, String> {
    with_jobs(home, now, |jobs| {
        let index = match lookup(jobs, reference) {
            Lookup::Found(index) => index,
            Lookup::Missing => return Err(format!("Job not found: {reference}")),
            Lookup::Ambiguous(matches) => return Err(ambiguous_text(jobs, reference, &matches)),
        };
        let parsed = parse_schedule(run_at, now)?;
        if parsed.kind != "once" {
            return Err(REARM_RECURRING_ERROR.into());
        }
        let next = compute_next_run(&parsed, None, job_zone(&jobs[index]), now)
            .ok_or_else(|| once_past_error(parsed.run_at.as_deref().unwrap_or(run_at)))?;
        let job = &jobs[index];
        if live_claim(&job["run_claim"], now, ONESHOT_CLAIM_TTL_SECONDS) {
            return Err("Cannot re-arm one-shot over a live run claim.".into());
        }
        if fire_claim_is_live(&job["fire_claim"], now) {
            return Err("Cannot re-arm one-shot over a live fire claim.".into());
        }
        if job["schedule"]["kind"].as_str() != Some("once") {
            return Err(REARM_RECURRING_ERROR.into());
        }
        let mut repeat = job["repeat"].as_object().cloned().unwrap_or_default();
        repeat.insert("completed".into(), json!(0));
        let display = if parsed.display.is_empty() {
            run_at.to_owned()
        } else {
            parsed.display.clone()
        };
        let record = jobs[index].as_object_mut().ok_or("invalid job record")?;
        record.insert("schedule".into(), json!(parsed));
        record.insert("schedule_display".into(), json!(display));
        record.insert("repeat".into(), Value::Object(repeat));
        record.insert("run_claim".into(), Value::Null);
        record.insert("fire_claim".into(), Value::Null);
        record.insert("enabled".into(), json!(true));
        record.insert("state".into(), json!("scheduled"));
        record.insert("paused_at".into(), Value::Null);
        record.insert("paused_reason".into(), Value::Null);
        record.insert("next_run_at".into(), json!(next));
        Ok((jobs[index].clone(), true))
    })
}

fn future_slot(value: &Value, now: DateTime<Tz>) -> Option<String> {
    let text = value.as_str()?;
    (parse_iso_instant(text, now.timezone())? > now.timestamp()).then(|| text.to_owned())
}

/// `claim_job_for_fire(manual=True)`: take the job for a run now, outside the tick. A run whose
/// slot is still in the future is an extra fire: the slot stays pending and the repeat budget
/// is left alone (see `mark_job_run`).
pub fn claim_manual(
    home: &Path,
    now: DateTime<Tz>,
    reference: &str,
    owner: &str,
) -> Result<ManualClaim, String> {
    with_jobs(home, now, |jobs| {
        let index = resolve_for(jobs, reference, "run")?;
        let job = &jobs[index];
        let recoverable = job["state"].as_str() == Some("error")
            && matches!(job["schedule"]["kind"].as_str(), Some("cron" | "interval"));
        let paused_marker = job["state"].as_str() == Some("paused") || truthy(&job["paused_at"]);
        let runnable = job.get("enabled").is_none_or(truthy) && !paused_marker;
        let blocked = (is_terminal(job) && !recoverable)
            || !runnable
            || fire_claim_is_live(&job["fire_claim"], now);
        if blocked {
            let reason = if runnable {
                "Job is already being fired by the scheduler; not run again."
            } else {
                "Job is paused/disabled; resume it before running."
            };
            return Ok((ManualClaim::Skipped(reason.into()), false));
        }
        if let Some(reason) = unsupported_reason(job) {
            return Err(format!(
                "Cannot run job '{}': it {reason}.",
                job["name"].as_str().unwrap_or_default()
            ));
        }
        let now_iso = iso_instant(now);
        let record = jobs[index].as_object_mut().ok_or("invalid job record")?;
        record.insert("fire_claim".into(), json!({"at": now_iso, "by": owner}));
        record.shift_remove("pending_slot");
        // A queued run-now stamped by the reference keeps the slot it overwrote.
        let stored = record.get("next_run_at").cloned().unwrap_or(Value::Null);
        let slot = if record.get("manual_run_at").unwrap_or(&Value::Null) == &stored {
            future_slot(
                record.get("manual_run_restore_at").unwrap_or(&Value::Null),
                now,
            )
        } else {
            future_slot(&stored, now)
        };
        let kind = record["schedule"]["kind"].as_str().map(str::to_owned);
        if let Some(slot) = slot {
            record["fire_claim"]["extra_slot"] = json!(slot);
            record.insert("next_run_at".into(), json!(slot));
            record.shift_remove("manual_run_restore_at");
        } else if matches!(kind.as_deref(), Some("cron" | "interval")) {
            let schedule = schedule_of(&Value::Object(record.clone()));
            let zone = job_zone(&Value::Object(record.clone()));
            if let Some(next) =
                schedule.and_then(|schedule| compute_next_run(&schedule, Some(&now_iso), zone, now))
            {
                record.insert("next_run_at".into(), json!(next));
            }
        }
        Ok((ManualClaim::Claimed(Box::new(jobs[index].clone())), true))
    })
}

/// Take a due job for this tick: a job whose fire claim is live belongs to a manual run (or
/// another ticker) and is left to it. Returns whether the claim was taken.
pub fn claim_for_tick(
    home: &Path,
    now: DateTime<Tz>,
    id: &str,
    owner: &str,
) -> Result<bool, String> {
    with_jobs(home, now, |jobs| {
        let Some(job) = jobs.iter_mut().find(|job| job["id"].as_str() == Some(id)) else {
            return Ok((false, false));
        };
        if fire_claim_is_live(&job["fire_claim"], now) {
            return Ok((false, false));
        }
        if let Some(record) = job.as_object_mut() {
            record.insert(
                "fire_claim".into(),
                json!({"at": iso_instant(now), "by": owner}),
            );
            record.shift_remove("pending_slot");
        }
        Ok((true, true))
    })
}

/// Keep a run's fire claim alive. False once the claim is no longer this owner's, which is
/// also what a removed job looks like.
pub fn heartbeat(home: &Path, now: DateTime<Tz>, id: &str, owner: &str) -> bool {
    with_jobs(home, now, |jobs| {
        let Some(job) = jobs.iter_mut().find(|job| job["id"].as_str() == Some(id)) else {
            return Ok((false, false));
        };
        if job["fire_claim"]["by"].as_str() != Some(owner) {
            return Ok((false, false));
        }
        job["fire_claim"]["at"] = json!(iso_instant(now));
        Ok((true, true))
    })
    .unwrap_or(true)
}

/// The job as stored, for the summary of a finished run. `None` once it is gone.
pub fn find_job(home: &Path, now: DateTime<Tz>, id: &str) -> Option<Value> {
    with_jobs(home, now, |jobs| {
        Ok((
            jobs.iter()
                .find(|job| job["id"].as_str() == Some(id))
                .cloned(),
            false,
        ))
    })
    .ok()
    .flatten()
}
