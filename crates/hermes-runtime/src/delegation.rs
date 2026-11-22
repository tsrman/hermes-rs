//! `delegate_task`: subagents that each work on one goal and return a summary.
//!
//! A child is a fresh turn with its own conversation, file tracking, working directory and task
//! list. It never sees the parent's conversation: only the goal and the context it is given. It
//! cannot delegate, ask questions, write memory, send messages or schedule work. A child has no
//! person to ask, so a request that the approval rules do not settle is refused, unless
//! `delegation.subagent_auto_approve` is set. Children of one call run in parallel and the call
//! returns when all have finished; background dispatch and the `list`/`steer`/`stop`/`continue`
//! actions are not ported.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::approval_policy::{self, Verdict};
use crate::tools::{BoxFuture, ToolContext};
use crate::turn::{
    ApprovalAction, ApprovalDecision, ApprovalRequest, ModelRoute, TurnError, TurnEvent,
    TurnHistory, TurnOptions, TurnOutcome,
};

pub(crate) const DELEGATE_TASK: &str = "delegate_task";

/// Tools a child never has.
pub(crate) const BLOCKED_FOR_CHILDREN: [&str; 5] = [
    "delegate_task",
    "clarify",
    "memory",
    "send_message",
    "cronjob_manage",
];

const MIN_BATCH_GOAL_LEN: usize = 10;
const COMPLETION_INSTRUCTIONS: &str = "\nComplete this task using the tools available to you. When finished, provide a clear, concise summary of:\n- What you did\n- What you found or accomplished\n- Any files you created or modified\n- Any issues encountered\n\nImportant workspace rule: Never assume a repository lives at /workspace/... or any other container-style path unless the task/context explicitly gives that path. If no exact local path is provided, discover it first before issuing git/workdir-specific commands.\n\nKeep your final summary tight: lead with outcomes, prefer bullet points over paragraphs, and don't replay your whole process. Your response is returned to the parent agent as a summary, and overlong summaries crowd out the parent's context window.";

/// `delegation.*` from the profile's configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegationSettings {
    pub max_concurrent_children: usize,
    /// Children one finite one-shot run may spawn in all; 0 is unlimited.
    pub oneshot_max_children: u32,
    /// Turns a child may take (`delegation.max_iterations`).
    pub max_iterations: u32,
    /// Approve what the rules leave open instead of refusing it.
    pub subagent_auto_approve: bool,
}

impl Default for DelegationSettings {
    fn default() -> Self {
        Self {
            max_concurrent_children: 10,
            oneshot_max_children: 2,
            max_iterations: 250,
            subagent_auto_approve: false,
        }
    }
}

impl DelegationSettings {
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Self {
        let section = &config["delegation"];
        let number = |key: &str| {
            let value = &section[key];
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        };
        let defaults = Self::default();
        Self {
            max_concurrent_children: number("max_concurrent_children")
                .map_or(defaults.max_concurrent_children, |n| n.max(1) as usize),
            oneshot_max_children: number("oneshot_max_children")
                .map_or(defaults.oneshot_max_children, |n| n.max(0) as u32),
            max_iterations: number("max_iterations")
                .map_or(defaults.max_iterations, |n| n.max(1) as u32),
            subagent_auto_approve: matches!(
                &section["subagent_auto_approve"],
                serde_yaml::Value::Bool(true)
            ) || section["subagent_auto_approve"].as_str().is_some_and(
                |text| {
                    matches!(
                        text.trim().to_lowercase().as_str(),
                        "true" | "yes" | "1" | "on"
                    )
                },
            ),
        }
    }
}

/// The settings with what one run has spent.
pub struct Delegation {
    pub settings: DelegationSettings,
    /// Whether this is a finite one-shot run, whose child budget is limited.
    pub oneshot: bool,
    spawned: AtomicU32,
}

impl Delegation {
    pub fn new(settings: DelegationSettings, oneshot: bool) -> Self {
        Self {
            settings,
            oneshot,
            spawned: AtomicU32::new(0),
        }
    }

    /// Charge `requested` children against the one-shot budget; the error says to do the work inline.
    fn charge(&self, requested: u32) -> Result<(), String> {
        let cap = self.settings.oneshot_max_children;
        if !self.oneshot || cap == 0 {
            return Ok(());
        }
        let mut spent = self.spawned.load(Ordering::SeqCst);
        loop {
            if spent + requested > cap {
                return Err(format!(
                    "Delegation budget for this one-shot run is exhausted ({spent}/{cap} subagents used; delegation.oneshot_max_children). Do the remaining work yourself in this session — reviewing your own diff and running the tests inline is expected here, not a delegated review."
                ));
            }
            match self.spawned.compare_exchange(
                spent,
                spent + requested,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(now) => spent = now,
            }
        }
    }
}

pub(crate) fn schema(max_children: usize) -> Value {
    let mut schema: Value =
        serde_json::from_str(include_str!("schemas/delegate_task.json")).unwrap_or_default();
    if let Some(text) = schema
        .pointer_mut("/parameters/properties/tasks/description")
        .filter(|text| text.is_string())
    {
        let replaced = text.as_str().unwrap_or_default().replace(
            "up to 10 for this user",
            &format!("up to {max_children} for this user"),
        );
        *text = Value::String(replaced);
    }
    json!({"type": "function", "function": schema})
}

#[derive(Debug)]
struct Task {
    goal: String,
    context: Option<String>,
    output_schema: Option<Value>,
}

fn normalize(args: &Value, max_children: usize) -> Result<Vec<Task>, String> {
    let mut tasks = args.get("tasks").cloned().unwrap_or(Value::Null);
    if let Value::String(text) = &tasks {
        tasks = match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(items)) => Value::Array(items),
            Ok(other) => {
                return Err(format!(
                    "tasks must be a JSON array of task objects; parsed {} instead.",
                    match other {
                        Value::Object(_) => "dict",
                        Value::String(_) => "str",
                        Value::Number(_) => "number",
                        Value::Bool(_) => "bool",
                        _ => "NoneType",
                    }
                ))
            }
            Err(error) => {
                return Err(format!(
                    "tasks must be a JSON array of task objects; received a string that could not be parsed as JSON ({}).",
                    error.to_string().split(" at line").next().unwrap_or("invalid")
                ))
            }
        };
    }
    let items = match tasks {
        Value::Array(items) if !items.is_empty() => items,
        Value::Null | Value::Array(_) => {
            return Err("No tasks provided. Pass tasks=[{goal: '...', context: '...'}, ...] — one entry per subagent (a single task is a one-entry array).".into())
        }
        _ => return Err("tasks must be an array of task objects.".into()),
    };
    if items.len() > max_children {
        return Err(format!(
            "Too many tasks: {} provided, but max_concurrent_children is {max_children}. Either reduce the task count, split into multiple delegate_task calls, or increase delegation.max_concurrent_children in config.yaml.",
            items.len()
        ));
    }
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!(
                "Task {index} must be an object, got {}.",
                match item {
                    Value::String(_) => "str",
                    Value::Array(_) => "list",
                    Value::Number(_) => "int",
                    Value::Bool(_) => "bool",
                    _ => "NoneType",
                }
            ));
        };
        let goal = object
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if goal.trim().is_empty() {
            return Err(format!("Task {index} is missing a 'goal'."));
        }
        let squeezed = goal
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let is_placeholder = squeezed == "todo"
            || squeezed.strip_prefix("task").is_some_and(|rest| {
                let rest = rest.trim_start();
                !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
            });
        if is_placeholder {
            return Err(format!(
                "Task {index} has a placeholder goal ({:?}). Replace it with a specific, self-contained description of what the subagent should accomplish.",
                goal.trim()
            ));
        }
        if goal.trim().chars().count() < MIN_BATCH_GOAL_LEN && items.len() > 1 {
            return Err(format!(
                "Task {index} goal is too short ({:?}). Write a specific, self-contained goal of at least {MIN_BATCH_GOAL_LEN} characters so the subagent knows exactly what to do.",
                goal.trim()
            ));
        }
        let schema = match object.get("output_schema") {
            None | Some(Value::Null) => None,
            Some(Value::Object(map)) if !map.is_empty() => Some(Value::Object(map.clone())),
            Some(Value::Object(_)) => None,
            Some(_) => {
                return Err(format!(
                    "Task {index}: output_schema must be a JSON Schema object."
                ))
            }
        };
        out.push(Task {
            goal: goal.trim().to_owned(),
            context: object
                .get("context")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned),
            output_schema: schema,
        });
    }
    Ok(out)
}

fn child_system_prompt(workspace: &str) -> String {
    format!(
        "You are a focused subagent working on a specific delegated task.\n\nWORKSPACE PATH:\n{workspace}\nUse this exact path for local repository/workdir operations unless the task explicitly says otherwise.\n{COMPLETION_INSTRUCTIONS}"
    )
}

fn child_task_message(task: &Task) -> String {
    let mut context = task.context.clone().unwrap_or_default();
    if let Some(schema) = &task.output_schema {
        if !context.is_empty() {
            context.push_str("\n\n");
        }
        context.push_str(&format!(
            "OUTPUT CONTRACT: your final answer must be a single JSON value that validates against this JSON Schema (no prose around it, no code fence):\n{schema}"
        ));
    }
    if context.trim().is_empty() {
        task.goal.clone()
    } else {
        format!("CONTEXT:\n{context}\n\nTASK:\n{}", task.goal)
    }
}

/// The first violation of the parts of JSON Schema a child's answer is likely to be held to:
/// `type`, `required`, `properties`, `items` and `enum`.
fn schema_errors(value: &Value, schema: &Value, path: &str, out: &mut Vec<String>) {
    let here = if path.is_empty() {
        "$".to_owned()
    } else {
        path.to_owned()
    };
    if let Some(expected) = schema.get("type") {
        let names: Vec<&str> = match expected {
            Value::String(name) => vec![name.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let fits = |name: &str| match name {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            _ => true,
        };
        if !names.is_empty() && !names.iter().any(|name| fits(name)) {
            out.push(format!(
                "{here}: {value} is not of type {}",
                names.join(", ")
            ));
            return;
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            out.push(format!(
                "{here}: {value} is not one of {}",
                Value::Array(options.clone())
            ));
        }
    }
    if let (Some(object), Some(required)) = (
        value.as_object(),
        schema.get("required").and_then(Value::as_array),
    ) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                out.push(format!("{here}: '{name}' is a required property"));
            }
        }
    }
    if let (Some(object), Some(properties)) = (
        value.as_object(),
        schema.get("properties").and_then(Value::as_object),
    ) {
        for (name, sub) in properties {
            if let Some(inner) = object.get(name) {
                schema_errors(inner, sub, &format!("{here}.{name}"), out);
            }
        }
    }
    if let (Some(items), Some(sub)) = (value.as_array(), schema.get("items")) {
        for (index, inner) in items.iter().enumerate() {
            schema_errors(inner, sub, &format!("{here}[{index}]"), out);
        }
    }
}

/// The JSON in an answer that may carry a code fence or a sentence around it.
fn json_candidate(text: &str) -> String {
    let trimmed = text.trim();
    if let (Some(start), Some(end)) = (trimmed.find("```"), trimmed.rfind("```")) {
        if end > start {
            let inner = &trimmed[start + 3..end];
            let inner = inner.strip_prefix("json").unwrap_or(inner);
            return inner.trim().to_owned();
        }
    }
    let first = trimmed.find(['{', '[']);
    let last = trimmed.rfind(['}', ']']);
    match (first, last) {
        (Some(a), Some(b)) if b > a => trimmed[a..=b].to_owned(),
        _ => trimmed.to_owned(),
    }
}

fn validate(text: &str, schema: &Value) -> Vec<String> {
    match serde_json::from_str::<Value>(&json_candidate(text)) {
        Err(error) => vec![format!("answer is not valid JSON: {error}")],
        Ok(value) => {
            let mut errors = Vec::new();
            schema_errors(&value, schema, "", &mut errors);
            errors.truncate(10);
            errors
        }
    }
}

struct ChildRun {
    status: &'static str,
    exit_reason: &'static str,
    summary: String,
    error: Option<String>,
    api_calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    trace: Vec<Value>,
}

/// What a child does when something needs approval and nobody can be asked.
fn answer_for_child(request: &ApprovalRequest, auto_approve: bool) -> ApprovalDecision {
    if matches!(request.action, ApprovalAction::Clarify) {
        return ApprovalDecision::Unavailable;
    }
    match approval_policy::installed().map(|policy| policy.decide(request)) {
        Some(Verdict::Allow) => ApprovalDecision::Approved,
        Some(Verdict::Deny) => ApprovalDecision::Denied,
        _ if auto_approve => ApprovalDecision::Approved,
        _ => ApprovalDecision::Denied,
    }
}

/// What every child of one call shares.
struct Family<'a> {
    options: &'a TurnOptions<'a>,
    delegation: &'a Delegation,
    route: &'a ModelRoute,
    fallbacks: &'a [ModelRoute],
    workspace: String,
    session: String,
}

async fn run_child(index: usize, task: &Task, family: &Family<'_>) -> ChildRun {
    let Family {
        options: parent,
        delegation,
        route,
        fallbacks,
        workspace,
        session: parent_session,
    } = family;
    let session = format!("{parent_session}#child{index}");
    let policy = parent.tools.without_tools(&BLOCKED_FOR_CHILDREN);
    let prompt = child_system_prompt(workspace);
    let options = TurnOptions {
        session_id: Some(&session),
        max_turns: delegation.settings.max_iterations,
        tools: &policy,
        system_prompt_override: Some(&prompt),
        child: Some(index),
        // Memory and clarify are off for children; the rest follows the parent.
        memory: None,
        ..**parent
    };
    let auto = delegation.settings.subagent_auto_approve;
    let mut query = child_task_message(task);
    let mut history = TurnHistory::default();
    let mut api_calls = 0usize;
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let mut trace: Vec<Value> = Vec::new();
    let mut attempts = 0;
    loop {
        let mut limit_text = String::new();
        let mut limit_hit = false;
        let mut finished: Vec<(String, bool)> = Vec::new();
        let outcome = crate::turn::run_turn(
            route,
            fallbacks,
            &query,
            options,
            history,
            |event| match event {
                TurnEvent::IterationLimit(_) => limit_hit = true,
                TurnEvent::Text(text) if limit_hit => limit_text.push_str(text),
                TurnEvent::ToolFinished { name, succeeded } => {
                    finished.push((name.to_owned(), succeeded));
                }
                _ => {}
            },
            move |request| {
                let decision = answer_for_child(&request, auto);
                async move { decision }
            },
        )
        .await;
        trace.extend(
            finished
                .into_iter()
                .map(|(name, ok)| json!({"tool": name, "status": if ok { "ok" } else { "error" }})),
        );
        match outcome {
            Ok(TurnOutcome::Completed(done)) => {
                api_calls += done.assistant_messages();
                let (i, o) = done.usage();
                input_tokens += i;
                output_tokens += o;
                let summary = done.final_text();
                if let Some(schema) = &task.output_schema {
                    let errors = validate(&summary, schema);
                    if !errors.is_empty() && attempts == 0 {
                        // One bounded correction turn, carrying the errors verbatim.
                        attempts += 1;
                        query = format!(
                            "Your final answer did not pass validation. Validation errors:\n{}\n\nReply again with only the corrected JSON value that satisfies the schema:\n{schema}",
                            errors.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
                        );
                        history = done.into_history();
                        continue;
                    }
                }
                let usable = !summary.trim().is_empty() && summary.trim() != "(empty)";
                return ChildRun {
                    status: if usable { "completed" } else { "failed" },
                    exit_reason: "completed",
                    error: (!usable).then(|| "Subagent did not produce a response.".to_owned()),
                    summary,
                    api_calls,
                    input_tokens,
                    output_tokens,
                    trace,
                };
            }
            Ok(TurnOutcome::IterationLimit) => {
                return ChildRun {
                    status: "completed",
                    exit_reason: "max_iterations",
                    summary: limit_text,
                    error: None,
                    api_calls: api_calls + delegation.settings.max_iterations as usize,
                    input_tokens,
                    output_tokens,
                    trace,
                };
            }
            Err(error) => {
                let message = match error {
                    TurnError::Provider(error) => format!("{error:?}"),
                    TurnError::InvalidCompletion => {
                        "The provider returned an invalid completion.".to_owned()
                    }
                    TurnError::InvalidIterationSummary => {
                        "The subagent could not summarize after reaching its budget.".to_owned()
                    }
                };
                return ChildRun {
                    status: "failed",
                    exit_reason: "error",
                    summary: String::new(),
                    error: Some(message),
                    api_calls,
                    input_tokens,
                    output_tokens,
                    trace,
                };
            }
        }
    }
}

pub(crate) fn call<'a>(
    cx: &'a mut ToolContext<'_, '_>,
    arguments: &'a str,
) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let error = |text: &str| json!({"error": text});
        let Some(delegation) = cx.options.delegation else {
            return error("delegate_task requires a parent agent context.");
        };
        let args: Value = match serde_json::from_str(arguments) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => return error("delegate_task expects a JSON object."),
        };
        match args.get("action").and_then(Value::as_str).map(|a| a.trim().to_lowercase()).as_deref() {
            None | Some("") | Some("spawn") => {}
            Some(action @ ("list" | "steer" | "stop" | "continue")) => {
                return error(&format!(
                    "delegate_task action '{action}' is not supported in this runtime: subagents run to completion inside the spawning call."
                ))
            }
            Some(other) => {
                return error(&format!(
                    "Unknown action '{other}'. Use spawn (default), list, steer, stop, or continue."
                ))
            }
        }
        if cx.options.child.is_some() {
            return error("Delegation depth limit reached (depth=1, max_spawn_depth=1). Raise delegation.max_spawn_depth in config.yaml if deeper nesting is required (no hard ceiling, but each level multiplies API cost).");
        }
        let tasks = match normalize(&args, delegation.settings.max_concurrent_children) {
            Ok(tasks) => tasks,
            Err(message) => return error(&message),
        };
        if args["tasks"].as_array().is_some_and(|tasks| {
            tasks.iter().any(|task| {
                task.get("images")
                    .is_some_and(|images| images.as_array().is_some_and(|a| !a.is_empty()))
            })
        }) {
            return error("delegate_task 'images' are not supported in this runtime: subagents cannot be given images.");
        }
        if let Err(message) = delegation.charge(tasks.len() as u32) {
            return error(&message);
        }
        let workspace = cx
            .options
            .root
            .canonicalize()
            .unwrap_or_else(|_| cx.options.root.to_path_buf())
            .display()
            .to_string();
        let family = Family {
            options: cx.options,
            delegation,
            route: cx.route,
            fallbacks: cx.fallbacks,
            workspace,
            session: cx.options.session_id.unwrap_or("default").to_owned(),
        };
        let started = Instant::now();
        let runs = join_all(tasks.iter().enumerate().map(|(index, task)| {
            let family = &family;
            async move {
                let began = Instant::now();
                let run = run_child(index, task, family).await;
                crate::terminal::forget_session(&format!("{}#child{index}", family.session));
                (run, began.elapsed())
            }
        }))
        .await;
        let mut results = Vec::new();
        for (index, (run, elapsed)) in runs.into_iter().enumerate() {
            let task = &tasks[index];
            let mut entry = Map::new();
            entry.insert("task_index".into(), json!(index));
            entry.insert("status".into(), json!(run.status));
            entry.insert("summary".into(), json!(run.summary));
            entry.insert("api_calls".into(), json!(run.api_calls));
            entry.insert("duration_seconds".into(), json!(round2(elapsed)));
            entry.insert("model".into(), json!(cx.route.model));
            entry.insert("exit_reason".into(), json!(run.exit_reason));
            entry.insert(
                "truncated".into(),
                json!(run.exit_reason == "max_iterations"),
            );
            entry.insert(
                "tokens".into(),
                json!({"input": run.input_tokens, "output": run.output_tokens}),
            );
            entry.insert("tool_trace".into(), Value::Array(run.trace));
            if let Some(message) = &run.error {
                entry.insert("error".into(), json!(message));
            }
            if let Some(schema) = &task.output_schema {
                let errors = if run.status == "completed" {
                    validate(&run.summary, schema)
                } else {
                    vec!["no answer".to_owned()]
                };
                entry.insert("schema_valid".into(), json!(errors.is_empty()));
                if !errors.is_empty() {
                    entry.insert("schema_errors".into(), json!(errors));
                    entry.insert("schema_note".into(), json!("Final answer does not satisfy the declared output_schema (after 1 retry); `summary` is the child's raw, UNVALIDATED final text — extract what you need from it yourself (see schema_errors) rather than re-running the task."));
                }
            }
            results.push(Value::Object(entry));
        }
        json!({"results": results, "total_duration_seconds": round2(started.elapsed())})
    })
}

fn round2(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tasks(value: Value) -> Result<Vec<Task>, String> {
        normalize(&json!({"tasks": value}), 3)
    }

    #[test]
    fn task_lists_are_checked_with_the_reference_messages() {
        assert!(tasks(Value::Null)
            .unwrap_err()
            .starts_with("No tasks provided."));
        assert!(tasks(json!([]))
            .unwrap_err()
            .starts_with("No tasks provided."));
        assert_eq!(
            tasks(json!([{"context": "x"}])).unwrap_err(),
            "Task 0 is missing a 'goal'."
        );
        assert_eq!(
            tasks(json!(["go"])).unwrap_err(),
            "Task 0 must be an object, got str."
        );
        assert!(
            tasks(Value::Array(vec![json!({"goal": "one goal here"}); 4]))
                .unwrap_err()
                .starts_with("Too many tasks: 4 provided, but max_concurrent_children is 3.")
        );
        assert!(tasks(json!([{"goal": "todo"}]))
            .unwrap_err()
            .contains("placeholder goal"));
        // A short goal is fine alone and refused in a batch.
        assert!(tasks(json!([{"goal": "Fix tests"}])).is_ok());
        assert!(
            tasks(json!([{"goal": "Fix tests"}, {"goal": "Write the docs please"}]))
                .unwrap_err()
                .contains("too short")
        );
        // A batch may arrive as a JSON string.
        assert_eq!(
            tasks(json!("[{\"goal\": \"Fix the tests\"}]"))
                .unwrap()
                .len(),
            1
        );
        assert!(tasks(json!("{nope"))
            .unwrap_err()
            .starts_with("tasks must be a JSON array"));
    }

    #[test]
    fn the_task_message_puts_context_first_and_states_the_contract() {
        let plain = Task {
            goal: "Do it".into(),
            context: None,
            output_schema: None,
        };
        assert_eq!(child_task_message(&plain), "Do it");
        let with = Task {
            goal: "Do it".into(),
            context: Some("About x".into()),
            output_schema: Some(json!({"type": "object"})),
        };
        let text = child_task_message(&with);
        assert!(
            text.starts_with("CONTEXT:\nAbout x\n\nOUTPUT CONTRACT:"),
            "{text}"
        );
        assert!(text.ends_with("\n\nTASK:\nDo it"));
    }

    #[test]
    fn answers_are_validated_against_the_declared_schema() {
        let schema = json!({"type": "object", "required": ["name", "count"],
            "properties": {"name": {"type": "string"}, "count": {"type": "integer"},
                           "tags": {"type": "array", "items": {"type": "string"}}}});
        assert!(validate("{\"name\": \"a\", \"count\": 2}", &schema).is_empty());
        assert!(validate("```json\n{\"name\": \"a\", \"count\": 2}\n```", &schema).is_empty());
        assert!(validate("Here you go: {\"name\": \"a\", \"count\": 2} done", &schema).is_empty());
        let errors = validate("{\"name\": 5, \"tags\": [1]}", &schema);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors
            .iter()
            .any(|e| e.contains("'count' is a required property")));
        assert!(validate("not json", &schema)[0].starts_with("answer is not valid JSON"));
    }

    #[test]
    fn the_one_shot_budget_is_spent_once_and_then_refuses() {
        let limited = Delegation::new(DelegationSettings::default(), true);
        assert!(limited.charge(2).is_ok());
        let error = limited.charge(1).unwrap_err();
        assert!(
            error.contains("(2/2 subagents used; delegation.oneshot_max_children)"),
            "{error}"
        );
        // An interactive run is never charged.
        assert!(Delegation::new(DelegationSettings::default(), false)
            .charge(50)
            .is_ok());
    }

    #[test]
    fn a_child_is_refused_what_nobody_can_approve() {
        let request = |action, content: &str| ApprovalRequest {
            target: "/w".into(),
            content: content.into(),
            action,
            timeout_secs: 5,
        };
        let ask = request(ApprovalAction::RunCommand, "make");
        assert!(matches!(
            answer_for_child(&ask, false),
            ApprovalDecision::Denied
        ));
        assert!(matches!(
            answer_for_child(&ask, true),
            ApprovalDecision::Approved
        ));
        let clarify = request(ApprovalAction::Clarify, "[]");
        assert!(matches!(
            answer_for_child(&clarify, true),
            ApprovalDecision::Unavailable
        ));
    }

    #[test]
    fn settings_read_the_delegation_section() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "delegation: {max_concurrent_children: 4, oneshot_max_children: 0, max_iterations: 30, subagent_auto_approve: true}",
        )
        .unwrap();
        let settings = DelegationSettings::from_config(&config);
        assert_eq!(
            (
                settings.max_concurrent_children,
                settings.oneshot_max_children,
                settings.max_iterations,
                settings.subagent_auto_approve
            ),
            (4, 0, 30, true)
        );
        let default = DelegationSettings::from_config(&serde_yaml::Value::Null);
        assert_eq!(
            (
                default.max_concurrent_children,
                default.oneshot_max_children,
                default.max_iterations
            ),
            (10, 2, 250)
        );
        let text =
            schema(4)["function"]["parameters"]["properties"]["tasks"]["description"].to_string();
        assert!(text.contains("up to 4 for this user"), "{text}");
    }
}
