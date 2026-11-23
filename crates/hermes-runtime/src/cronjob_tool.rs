//! The built-in `cronjob_manage` agent tool.
//!
//! Mirrors `tools/cronjob_tools.py` of the Python reference:
//! - actions: `create`, `list`, `update`, `pause`, `resume`, `remove`, `run`, `cancel`;
//! - `model`, `provider`, and `base_url` are intentionally NOT forwarded from model arguments
//!   (matching `_HANDLER_FORWARDED_ARGS` in `tools/cronjob_tools.py`);
//! - `monitor` splits into `(monitor_script, monitor_url)` via `cron::monitor::split_monitor_arg`;
//! - delivery remains local-only (remote delivery belongs to Stage 6 gateway).

use std::path::Path;

use serde_json::{json, Value};

use crate::cron::cancel;
use crate::cron::manage::{self, AddParams, EditParams, WrapResponse};
use crate::cron::monitor::split_monitor_arg;
use crate::cron::store;
use crate::cron::{now_in, profile_timezone};

pub const CRONJOB_MANAGE: &str = "cronjob_manage";

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": CRONJOB_MANAGE,
            "description": "Schedule recurring or one-off agent tasks (cron jobs): create, list, edit, run, cancel. action='create' schedules a job from a prompt, script, and/or skills; 'list' inspects jobs; 'update'/'pause'/'resume'/'remove'/'run'/'cancel' manage one by job_id (always list first — never guess job IDs).",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "One of: create, list, update, pause, resume, remove, run, cancel. When action=create, 'schedule' and at least one of 'prompt', 'script', or 'skills' are REQUIRED."
                    },
                    "job_id": {
                        "type": "string",
                        "description": "Required for update/pause/resume/remove/run/cancel."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "For create/update: the full self-contained prompt."
                    },
                    "schedule": {
                        "type": "string",
                        "description": "REQUIRED for create. Examples: '30m', 'every 2h', 'in 30m', 'every monday 9am', '0 9 * * *', '2026-06-01T09:00:00'."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional human-friendly name."
                    },
                    "repeat": {
                        "type": "integer",
                        "description": "Optional repeat count. Omit for defaults (once for one-shot, forever for recurring)."
                    },
                    "deliver": {
                        "type": "string",
                        "description": "Delivery target. Only 'local' is supported in the Rust CLI."
                    },
                    "failure_deliver": {
                        "type": "string",
                        "description": "Failure notice target. Only 'local' is supported in the Rust CLI; '' clears."
                    },
                    "skills": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional ordered skill names loaded before the cron prompt. On update, [] clears."
                    },
                    "script": {
                        "type": "string",
                        "description": "Optional script under <profile>/scripts/ run each tick; stdout is injected into the prompt (with no_agent=true the script IS the job). On update, '' clears."
                    },
                    "monitor": {
                        "type": "string",
                        "description": "Optional change-detector that gates the agent: an http(s) URL or a script path under <profile>/scripts/. On update, '' clears."
                    },
                    "no_agent": {
                        "type": "boolean",
                        "default": false,
                        "description": "True = no LLM: runs script on schedule and saves stdout verbatim; empty stdout skips output."
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Optional absolute existing path to run the job from. On update, '' clears."
                    },
                    "timezone": {
                        "type": "string",
                        "description": "Optional IANA zone (e.g. 'America/New_York'). On update, '' clears."
                    },
                    "grace_minutes": {
                        "type": "integer",
                        "description": "Catch-up window in minutes (0-1440) for cron schedules."
                    },
                    "catch_up": {
                        "type": "boolean",
                        "description": "Runs missed while the machine was off: true = run once on return, false = skip."
                    },
                    "wrap_response": {
                        "type": "string",
                        "enum": ["on", "off", "inherit"],
                        "description": "Per-job override for wrapping delivered output."
                    },
                    "paused": {
                        "type": "boolean",
                        "description": "Create only: persist disabled atomically."
                    },
                    "paused_reason": {
                        "type": "string",
                        "description": "Create only: auditable reason; requires paused=true."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional reason for pause or cancel."
                    },
                    "include_disabled": {
                        "type": "boolean",
                        "description": "For list: include paused and completed jobs (default true)."
                    }
                },
                "required": ["action"]
            }
        }
    })
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn opt_skills(args: &Value) -> Option<Vec<String>> {
    if let Some(val) = args.get("skills") {
        match val {
            Value::Array(items) => {
                let list = items
                    .iter()
                    .filter_map(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Null => None,
                        other => Some(other.to_string()),
                    })
                    .collect();
                return Some(list);
            }
            Value::String(s) => return Some(vec![s.clone()]),
            _ => {}
        }
    }
    if let Some(Value::String(s)) = args.get("skill") {
        if !s.trim().is_empty() {
            return Some(vec![s.clone()]);
        }
    }
    None
}

fn opt_grace(args: &Value) -> Option<String> {
    match args.get("grace_minutes") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn opt_wrap_create(args: &Value) -> Option<bool> {
    match args.get("wrap_response") {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "on" | "true" => Some(true),
            "off" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn opt_wrap_edit(args: &Value) -> Option<WrapResponse> {
    match args.get("wrap_response") {
        Some(Value::Bool(true)) => Some(WrapResponse::On),
        Some(Value::Bool(false)) => Some(WrapResponse::Off),
        Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "on" | "true" => Some(WrapResponse::On),
            "off" | "false" => Some(WrapResponse::Off),
            "inherit" => Some(WrapResponse::Inherit),
            _ => None,
        },
        _ => None,
    }
}

fn err_payload(msg: impl Into<String>) -> Value {
    json!({"success": false, "error": msg.into()})
}

/// Dispatch a `cronjob_manage` tool call against `home`.
pub fn call(home: &Path, arguments: &str) -> Value {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return err_payload("Tool arguments must be a JSON object"),
    };
    let action = args["action"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let zone = profile_timezone(Some(home));
    let now = now_in(zone);

    match action.as_str() {
        "list" => {
            let include_disabled = args["include_disabled"].as_bool().unwrap_or(true);
            match store::read_jobs(home) {
                Ok(jobs) => {
                    let filtered: Vec<_> = jobs
                        .into_iter()
                        .filter(|j| include_disabled || (j.enabled && j.state != "completed"))
                        .collect();
                    let count = filtered.len();
                    json!({"success": true, "count": count, "jobs": filtered})
                }
                Err(error) => err_payload(error),
            }
        }
        "create" => {
            let (mon_script, mon_url) = split_monitor_arg(
                args["monitor"].as_str(),
                args["monitor_script"].as_str(),
                args["monitor_url"].as_str(),
            );
            let params = AddParams {
                schedule: opt_str(&args, "schedule").unwrap_or_default(),
                prompt: opt_str(&args, "prompt"),
                name: opt_str(&args, "name"),
                deliver: opt_str(&args, "deliver"),
                failure_deliver: opt_str(&args, "failure_deliver"),
                repeat: args["repeat"].as_i64(),
                catch_up: args["catch_up"].as_bool(),
                wrap_response: opt_wrap_create(&args),
                // Model/provider/base_url intentionally NOT forwarded from model args!
                model: None,
                timezone: opt_str(&args, "timezone"),
                grace_minutes: opt_grace(&args),
                paused: args["paused"].as_bool().unwrap_or(false),
                paused_reason: opt_str(&args, "paused_reason"),
                script: opt_str(&args, "script"),
                no_agent: args["no_agent"].as_bool().unwrap_or(false),
                interpreter: None,
                skills: opt_skills(&args).unwrap_or_default(),
                monitor_script: mon_script.filter(|s| !s.is_empty()),
                monitor_url: mon_url.filter(|s| !s.is_empty()),
                workdir: opt_str(&args, "workdir"),
            };
            match manage::add_job(home, now, &params) {
                Ok(job) => json!({
                    "success": true,
                    "job_id": job["id"],
                    "name": job["name"],
                    "schedule": job["schedule_display"],
                    "next_run_at": job["next_run_at"],
                    "skills": job["skills"],
                    "job": store::summarize_job(&job),
                }),
                Err(error) => err_payload(error),
            }
        }
        "update" | "pause" | "resume" | "remove" | "run" | "cancel" => {
            let job_id = opt_str(&args, "job_id").unwrap_or_default();
            if job_id.trim().is_empty() {
                return err_payload(format!("job_id is required for action '{action}'"));
            }
            match action.as_str() {
                "update" => {
                    let (mon_script, mon_url) = split_monitor_arg(
                        args["monitor"].as_str(),
                        args["monitor_script"].as_str(),
                        args["monitor_url"].as_str(),
                    );
                    let params = EditParams {
                        schedule: opt_str(&args, "schedule"),
                        prompt: opt_str(&args, "prompt"),
                        name: opt_str(&args, "name"),
                        deliver: opt_str(&args, "deliver"),
                        failure_deliver: opt_str(&args, "failure_deliver"),
                        repeat: args["repeat"].as_i64(),
                        catch_up: args["catch_up"].as_bool(),
                        wrap_response: opt_wrap_edit(&args),
                        model: None,
                        timezone: opt_str(&args, "timezone"),
                        grace_minutes: opt_grace(&args),
                        script: opt_str(&args, "script"),
                        no_agent: args["no_agent"].as_bool(),
                        interpreter: None,
                        skills: opt_skills(&args),
                        monitor_script: mon_script,
                        monitor_url: mon_url,
                        workdir: opt_str(&args, "workdir"),
                    };
                    match manage::edit_job(home, now, &job_id, &params) {
                        Ok(job) => json!({
                            "success": true,
                            "job": store::summarize_job(&job),
                        }),
                        Err(error) => err_payload(error),
                    }
                }
                "pause" => match store::pause_job(home, now, &job_id, args["reason"].as_str()) {
                    Ok(job) => json!({"success": true, "job": job}),
                    Err(error) => err_payload(error),
                },
                "resume" => match store::resume_job(home, now, &job_id) {
                    Ok(job) => json!({"success": true, "job": job}),
                    Err(error) => err_payload(error),
                },
                "remove" => match store::remove_job(home, now, &job_id) {
                    Ok(removed_id) => json!({"success": true, "removed_job_id": removed_id}),
                    Err(error) => err_payload(error),
                },
                "cancel" => match cancel::cancel_run(home, now, &job_id, args["reason"].as_str()) {
                    Ok(result) => {
                        serde_json::to_value(result).unwrap_or_else(|e| err_payload(e.to_string()))
                    }
                    Err(error) => err_payload(error),
                },
                "run" => {
                    // Queue an immediate run via `next_run_at = now` or report the CLI command;
                    // in-process recursive turn execution from inside a tool call is avoided so
                    // it cannot deadlock the active turn or spawn an unmonitored provider loop.
                    match manage::trigger_job_now(home, now, &job_id) {
                        Ok(job) => json!({
                            "success": true,
                            "queued": true,
                            "job": store::summarize_job(&job),
                            "detail": "Job queued to run on the next scheduler tick (or run `hermes cron run <id>` directly)."
                        }),
                        Err(error) => err_payload(error),
                    }
                }
                _ => unreachable!(),
            }
        }
        other => err_payload(format!("Unknown cron action '{other}'")),
    }
}
