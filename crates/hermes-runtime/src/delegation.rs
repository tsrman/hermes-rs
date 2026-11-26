//! `delegate_task`: subagents that each work on one goal and return a summary.
//!
//! A child is a fresh turn with its own conversation, file tracking, working directory and task
//! list. It never sees the parent's conversation: only the goal and the context it is given. It
//! cannot delegate, ask questions, write memory, send messages or schedule work. A child has no
//! person to ask, so a request that the approval rules do not settle is refused, unless
//! `delegation.subagent_auto_approve` is set. Children of one call run in parallel and the call
//! returns when all have finished; background dispatch and the `list`/`steer`/`stop`/`continue`
//! actions are not ported.

use crate::paths::PathExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
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

/// Tools a leaf child never has.
pub(crate) const BLOCKED_FOR_CHILDREN: [&str; 5] = [
    "delegate_task",
    "clarify",
    "memory",
    "send_message",
    "cronjob_manage",
];

/// Tools blocked for every child regardless of `max_spawn_depth`.
const ALWAYS_BLOCKED_FOR_CHILDREN: [&str; 4] =
    ["clarify", "memory", "send_message", "cronjob_manage"];

const MIN_BATCH_GOAL_LEN: usize = 10;
const COMPLETION_INSTRUCTIONS: &str = "\nComplete this task using the tools available to you. When finished, provide a clear, concise summary of:\n- What you did\n- What you found or accomplished\n- Any files you created or modified\n- Any issues encountered\n\nImportant workspace rule: Never assume a repository lives at /workspace/... or any other container-style path unless the task/context explicitly gives that path. If no exact local path is provided, discover it first before issuing git/workdir-specific commands.\n\nKeep your final summary tight: lead with outcomes, prefer bullet points over paragraphs, and don't replay your whole process. Your response is returned to the parent agent as a summary, and overlong summaries crowd out the parent's context window.";

fn default_max_spawn_depth() -> u32 {
    1
}

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
    /// Per-child timeout in seconds (`delegation.child_timeout_seconds`, floor 30s; `None` = no cap).
    #[serde(default)]
    pub child_timeout_seconds: Option<f64>,
    /// Maximum delegation nesting depth (`delegation.max_spawn_depth`, default 1, floor 1).
    #[serde(default = "default_max_spawn_depth")]
    pub max_spawn_depth: u32,
}

impl Default for DelegationSettings {
    fn default() -> Self {
        Self {
            max_concurrent_children: 10,
            oneshot_max_children: 2,
            max_iterations: 250,
            subagent_auto_approve: false,
            child_timeout_seconds: None,
            max_spawn_depth: 1,
        }
    }
}

/// Parse `delegation.child_timeout_seconds` matching `delegate_tool_config.py::_parse_timeout`:
/// `<= 0` disables (`None`), positive values are floored at 30.0s.
pub fn parse_child_timeout_seconds(raw: &serde_yaml::Value) -> Option<f64> {
    let parsed = match raw {
        serde_yaml::Value::Number(n) => n.as_f64(),
        serde_yaml::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !parsed.is_finite() || parsed <= 0.0 {
        None
    } else {
        Some(parsed.max(30.0))
    }
}

impl DelegationSettings {
    pub(crate) fn from_config(
        config: &serde_yaml::Value,
        secrets: &std::collections::HashMap<String, String>,
    ) -> Self {
        let section = &config["delegation"];
        let number = |key: &str| {
            let value = &section[key];
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        };
        let defaults = Self::default();
        let child_timeout_seconds = match section.get("child_timeout_seconds") {
            Some(v) if !v.is_null() => parse_child_timeout_seconds(v),
            _ => std::env::var("DELEGATION_CHILD_TIMEOUT_SECONDS")
                .ok()
                .or_else(|| secrets.get("DELEGATION_CHILD_TIMEOUT_SECONDS").cloned())
                .and_then(|raw| parse_child_timeout_seconds(&serde_yaml::Value::String(raw))),
        };
        let max_spawn_depth = number("max_spawn_depth")
            .or_else(|| {
                std::env::var("DELEGATION_MAX_SPAWN_DEPTH")
                    .ok()
                    .or_else(|| secrets.get("DELEGATION_MAX_SPAWN_DEPTH").cloned())
                    .and_then(|raw| raw.trim().parse::<i64>().ok())
            })
            .map_or(defaults.max_spawn_depth, |n| n.max(1) as u32);
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
            child_timeout_seconds,
            max_spawn_depth,
        }
    }
}

struct LiveSubagent {
    subagent_id: String,
    goal: String,
    started_at: Instant,
    stop_flag: Arc<AtomicBool>,
    steer_queue: Arc<Mutex<Vec<String>>>,
}

struct FinishedSubagent {
    task: Task,
    history: Option<TurnHistory>,
    claimed: bool,
}

#[derive(Default)]
struct SubagentRegistry {
    next_seq: u64,
    live: BTreeMap<String, LiveSubagent>,
    finished: BTreeMap<String, FinishedSubagent>,
}

/// The settings with what one run has spent, plus its in-process subagent control registry.
pub struct Delegation {
    pub settings: DelegationSettings,
    /// Whether this is a finite one-shot run, whose child budget is limited.
    pub oneshot: bool,
    /// `delegation.model` and friends: the route subagents use instead of the parent's. A pinned
    /// route never borrows the parent's fallback chain.
    route: Option<ModelRoute>,
    spawned: AtomicU32,
    registry: Mutex<SubagentRegistry>,
}

impl Delegation {
    pub fn new(settings: DelegationSettings, oneshot: bool) -> Self {
        Self {
            settings,
            oneshot,
            route: None,
            spawned: AtomicU32::new(0),
            registry: Mutex::new(SubagentRegistry::default()),
        }
    }

    pub fn with_route(mut self, route: Option<ModelRoute>) -> Self {
        self.route = route;
        self
    }

    fn allocate_id(&self) -> String {
        let mut reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        reg.next_seq += 1;
        format!("sa_{:08x}", reg.next_seq)
    }

    fn register_live(
        &self,
        subagent_id: String,
        goal: String,
    ) -> (Arc<AtomicBool>, Arc<Mutex<Vec<String>>>) {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let steer_queue = Arc::new(Mutex::new(Vec::new()));
        let mut reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        reg.live.insert(
            subagent_id.clone(),
            LiveSubagent {
                subagent_id,
                goal,
                started_at: Instant::now(),
                stop_flag: Arc::clone(&stop_flag),
                steer_queue: Arc::clone(&steer_queue),
            },
        );
        (stop_flag, steer_queue)
    }

    fn finish_live(
        &self,
        subagent_id: &str,
        task: Task,
        history: Option<TurnHistory>,
    ) -> Vec<String> {
        let mut reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        let missed = reg
            .live
            .remove(subagent_id)
            .map(|live| {
                let mut q = live.steer_queue.lock().unwrap_or_else(|p| p.into_inner());
                std::mem::take(&mut *q)
            })
            .unwrap_or_default();
        if let Some(hist) = history {
            reg.finished.insert(
                subagent_id.to_owned(),
                FinishedSubagent {
                    task,
                    history: Some(hist),
                    claimed: false,
                },
            );
        } else {
            reg.finished.remove(subagent_id);
        }
        missed
    }

    /// Snapshot of currently running subagents (for `action="list"` and external callers).
    pub fn list_live(&self) -> Vec<Value> {
        let reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        reg.live
            .values()
            .map(|entry| {
                json!({
                    "subagent_id": entry.subagent_id,
                    "goal": entry.goal,
                    "status": if entry.stop_flag.load(Ordering::SeqCst) { "stopping" } else { "running" },
                    "elapsed_seconds": round2(entry.started_at.elapsed()),
                })
            })
            .collect()
    }

    /// Queue steering guidance for a currently running subagent (`action="steer"`).
    pub fn steer_live(&self, subagent_id: &str, message: &str) -> bool {
        let reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = reg.live.get(subagent_id) else {
            return false;
        };
        if entry.stop_flag.load(Ordering::SeqCst) {
            return false;
        }
        entry
            .steer_queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(message.to_owned());
        true
    }

    /// Request cooperative cancellation of a running subagent (`action="stop"`).
    pub fn stop_live(&self, subagent_id: &str) -> bool {
        let reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = reg.live.get(subagent_id) else {
            return false;
        };
        entry.stop_flag.store(true, Ordering::SeqCst);
        true
    }

    fn claim_finished(&self, subagent_id: &str) -> Result<(Task, TurnHistory), String> {
        let mut reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        if reg.live.contains_key(subagent_id) {
            return Err(format!(
                "Subagent '{subagent_id}' is still running: use action='steer' to send it this message."
            ));
        }
        let Some(record) = reg.finished.get_mut(subagent_id) else {
            return Err(format!(
                "No finished subagent '{subagent_id}' in this conversation's spawn tree can be continued (unknown id, not yours, or no longer retained). Spawn a new task instead."
            ));
        };
        if record.claimed {
            return Err(format!(
                "Subagent '{subagent_id}' is already being continued. Wait for that run to finish before continuing it again."
            ));
        }
        let Some(history) = record.history.take() else {
            return Err(format!(
                "Subagent '{subagent_id}' has no saved transcript to continue from. Spawn a new task instead."
            ));
        };
        record.claimed = true;
        Ok((record.task.clone(), history))
    }

    fn release_finished(&self, subagent_id: &str, history: TurnHistory) {
        let mut reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = reg.finished.get_mut(subagent_id) {
            record.history = Some(history);
            record.claimed = false;
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

#[derive(Clone, Debug)]
struct Task {
    goal: String,
    context: Option<String>,
    output_schema: Option<Value>,
}

fn normalize(args: &Value, max_children: usize) -> Result<Vec<Task>, String> {
    let mut tasks = args.get("tasks").cloned().unwrap_or(Value::Null);
    if tasks.is_null() {
        if let Some(goal) = args.get("goal").and_then(Value::as_str) {
            let mut single = Map::new();
            single.insert("goal".into(), Value::String(goal.to_owned()));
            if let Some(ctx) = args.get("context") {
                single.insert("context".into(), ctx.clone());
            }
            if let Some(schema) = args.get("output_schema") {
                single.insert("output_schema".into(), schema.clone());
            }
            tasks = Value::Array(vec![Value::Object(single)]);
        }
    }
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
    subagent_id: String,
    status: &'static str,
    exit_reason: &'static str,
    summary: String,
    error: Option<String>,
    api_calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    trace: Vec<Value>,
    missed_steer: Vec<String>,
    history: Option<TurnHistory>,
}

/// What a child does when something needs approval and nobody can be asked.
fn answer_for_child(
    request: &ApprovalRequest,
    policy: Option<&approval_policy::ApprovalPolicy>,
    auto_approve: bool,
) -> ApprovalDecision {
    if matches!(request.action, ApprovalAction::Clarify) {
        return ApprovalDecision::Unavailable;
    }
    let effective = match policy {
        Some(policy) => Some(policy),
        None => approval_policy::installed(),
    };
    match effective.map(|policy| policy.decide(request)) {
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

async fn run_child(
    index: usize,
    subagent_id: String,
    task: &Task,
    initial_history: Option<TurnHistory>,
    followup_query: Option<String>,
    family: &Family<'_>,
) -> ChildRun {
    let (stop_flag, steer_queue) = family
        .delegation
        .register_live(subagent_id.clone(), task.goal.clone());
    let inner = run_child_inner(
        index,
        subagent_id.clone(),
        task,
        initial_history,
        followup_query,
        Arc::clone(&stop_flag),
        Arc::clone(&steer_queue),
        family,
    );
    let mut run = match family.delegation.settings.child_timeout_seconds {
        Some(secs) if secs > 0.0 => {
            match tokio::time::timeout(Duration::from_secs_f64(secs), inner).await {
                Ok(run) => run,
                Err(_) => ChildRun {
                    subagent_id: subagent_id.clone(),
                    status: "timeout",
                    exit_reason: "timeout",
                    summary: String::new(),
                    error: Some(format!(
                        "Subagent timed out after {secs:.0}s without finishing. Raise delegation.child_timeout_seconds in config.yaml if these tasks legitimately take longer."
                    )),
                    api_calls: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    trace: Vec::new(),
                    missed_steer: Vec::new(),
                    history: None,
                },
            }
        }
        _ => inner.await,
    };
    let missed = family
        .delegation
        .finish_live(&subagent_id, task.clone(), run.history.take());
    run.missed_steer = missed;
    run
}

#[allow(clippy::too_many_arguments)]
async fn run_child_inner(
    index: usize,
    subagent_id: String,
    task: &Task,
    initial_history: Option<TurnHistory>,
    followup_query: Option<String>,
    stop_flag: Arc<AtomicBool>,
    steer_queue: Arc<Mutex<Vec<String>>>,
    family: &Family<'_>,
) -> ChildRun {
    let Family {
        options: parent,
        delegation,
        route,
        fallbacks,
        workspace,
        session: parent_session,
    } = family;
    let session = format!("{parent_session}#child{index}");
    let next_depth = parent.delegate_depth.saturating_add(1);
    let policy = if next_depth >= delegation.settings.max_spawn_depth {
        parent.tools.without_tools(&BLOCKED_FOR_CHILDREN)
    } else {
        parent.tools.without_tools(&ALWAYS_BLOCKED_FOR_CHILDREN)
    };
    let prompt = child_system_prompt(workspace);
    let options = TurnOptions {
        single_query: false,
        session_id: Some(&session),
        max_turns: delegation.settings.max_iterations,
        tools: &policy,
        system_prompt_override: Some(&prompt),
        ephemeral_system_prompt: None,
        state_scope: None,
        prefill: None,
        child: Some(index),
        delegate_depth: next_depth,
        cancel_flag: Some(stop_flag.as_ref()),
        steer_queue: Some(steer_queue.as_ref()),
        // Memory and clarify are off for children; the rest follows the parent.
        memory: None,
        compaction_guard: None,
        ..**parent
    };
    let auto = delegation.settings.subagent_auto_approve;
    let mut query = followup_query.unwrap_or_else(|| child_task_message(task));
    let mut history = initial_history.unwrap_or_default();
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
                TurnEvent::ToolFinished {
                    name, succeeded, ..
                } => {
                    finished.push((name.to_owned(), succeeded));
                }
                _ => {}
            },
            move |request| {
                let decision = answer_for_child(&request, parent.approval_policy, auto);
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
                    subagent_id,
                    status: if usable { "completed" } else { "failed" },
                    exit_reason: "completed",
                    error: (!usable).then(|| "Subagent did not produce a response.".to_owned()),
                    summary,
                    api_calls,
                    input_tokens,
                    output_tokens,
                    trace,
                    missed_steer: Vec::new(),
                    history: Some(done.into_history()),
                };
            }
            Ok(TurnOutcome::Interrupted(done)) => {
                api_calls += done.assistant_messages();
                let (i, o) = done.usage();
                input_tokens += i;
                output_tokens += o;
                let summary = done.final_text();
                return ChildRun {
                    subagent_id,
                    status: "interrupted",
                    exit_reason: "interrupted",
                    summary,
                    error: Some("Subagent was stopped by parent request.".to_owned()),
                    api_calls,
                    input_tokens,
                    output_tokens,
                    trace,
                    missed_steer: Vec::new(),
                    history: Some(done.into_history()),
                };
            }
            Ok(TurnOutcome::IterationLimit) => {
                return ChildRun {
                    subagent_id,
                    status: "completed",
                    exit_reason: "max_iterations",
                    summary: limit_text,
                    error: None,
                    api_calls: api_calls + delegation.settings.max_iterations as usize,
                    input_tokens,
                    output_tokens,
                    trace,
                    missed_steer: Vec::new(),
                    history: None,
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
                    subagent_id,
                    status: "failed",
                    exit_reason: "error",
                    summary: String::new(),
                    error: Some(message),
                    api_calls,
                    input_tokens,
                    output_tokens,
                    trace,
                    missed_steer: Vec::new(),
                    history: None,
                };
            }
        }
    }
}

fn format_result_entry(
    index: usize,
    task: &Task,
    run: &ChildRun,
    elapsed: Duration,
    model: &str,
) -> Value {
    let mut entry = Map::new();
    entry.insert("task_index".into(), json!(index));
    entry.insert("subagent_id".into(), json!(run.subagent_id));
    entry.insert("status".into(), json!(run.status));
    entry.insert("summary".into(), json!(run.summary));
    entry.insert("api_calls".into(), json!(run.api_calls));
    entry.insert("duration_seconds".into(), json!(round2(elapsed)));
    entry.insert("model".into(), json!(model));
    entry.insert("exit_reason".into(), json!(run.exit_reason));
    entry.insert(
        "truncated".into(),
        json!(run.exit_reason == "max_iterations"),
    );
    entry.insert(
        "tokens".into(),
        json!({"input": run.input_tokens, "output": run.output_tokens}),
    );
    entry.insert("tool_trace".into(), Value::Array(run.trace.clone()));
    if let Some(message) = &run.error {
        entry.insert("error".into(), json!(message));
    }
    if !run.missed_steer.is_empty() {
        entry.insert("missed_steer".into(), json!(run.missed_steer));
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
    Value::Object(entry)
}

fn handle_control_action(
    action: &str,
    subagent_id: Option<&str>,
    message: Option<&str>,
    delegation: &Delegation,
) -> Value {
    let error = |text: String| json!({"error": text});
    if action == "list" {
        let entries = delegation.list_live();
        let mut payload = Map::new();
        payload.insert("action".into(), json!("list"));
        payload.insert("count".into(), json!(entries.len()));
        payload.insert("subagents".into(), Value::Array(entries.clone()));
        if entries.is_empty() {
            payload.insert(
                "note".into(),
                json!(
                    "No live subagents right now. Children that already finished have delivered (or will deliver) their results as normal completion messages — there is nothing to steer or stop."
                ),
            );
        }
        return Value::Object(payload);
    }

    let sid = subagent_id.map(str::trim).unwrap_or("");
    if sid.is_empty() {
        return error(format!(
            "action='{action}' requires subagent_id (from the spawn dispatch response or action='list')."
        ));
    }
    if action == "steer" {
        let text = message.map(str::trim).unwrap_or("");
        if text.is_empty() {
            return error(
                "action='steer' requires a non-empty 'message' describing the course correction."
                    .into(),
            );
        }
        if delegation.steer_live(sid, text) {
            return json!({
                "action": "steer",
                "subagent_id": sid,
                "status": "queued",
                "note": "Steering text queued. The subagent sees it appended to its next tool result — the current tool call is never cut. If the child finishes before a delivery boundary remains, the text is reported back as missed_steer in its completion entry."
            });
        }
        return error(format!(
            "No live subagent '{sid}' in this conversation's spawn tree. It may have already finished (its result arrives as a normal completion message). Use action='list' to see live children."
        ));
    }
    if action == "stop" {
        if delegation.stop_live(sid) {
            return json!({
                "action": "stop",
                "subagent_id": sid,
                "status": "interrupt_requested",
                "note": "The subagent stops at its next iteration boundary (in-flight tool calls are asked to cancel). Its partial result still re-enters the conversation as a completion message — do not wait or poll."
            });
        }
        return error(format!(
            "No live subagent '{sid}' in this conversation's spawn tree. It may have already finished (its result arrives as a normal completion message). Use action='list' to see live children."
        ));
    }
    error(format!(
        "Unknown action '{action}'. Use spawn (default), list, steer, stop, or continue."
    ))
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
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .map(|a| a.trim().to_lowercase())
            .unwrap_or_default();
        let subagent_id_arg = args.get("subagent_id").and_then(Value::as_str);
        let message_arg = args.get("message").and_then(Value::as_str);

        if matches!(action.as_str(), "list" | "steer" | "stop") {
            return handle_control_action(&action, subagent_id_arg, message_arg, delegation);
        }
        if action == "continue" {
            let sid = subagent_id_arg.map(str::trim).unwrap_or("");
            let msg = message_arg.map(str::trim).unwrap_or("");
            if sid.is_empty() || msg.is_empty() {
                return error(
                    "action='continue' requires subagent_id (a finished child of yours) and a non-empty 'message' (its next instruction).",
                );
            }
            if delegation.oneshot {
                return error(
                    "action='continue' is unavailable in one-shot sessions. Spawn a new task instead.",
                );
            }
            let depth = cx.options.delegate_depth;
            let max_spawn = delegation.settings.max_spawn_depth;
            if depth >= max_spawn {
                return error(&format!(
                    "Delegation depth limit reached (depth={depth}, max_spawn_depth={max_spawn})."
                ));
            }
            let (task, history) = match delegation.claim_finished(sid) {
                Ok(pair) => pair,
                Err(message) => return error(&message),
            };
            if let Err(message) = delegation.charge(1) {
                delegation.release_finished(sid, history);
                return error(&message);
            }
            let workspace = cx
                .options
                .root
                .canonical()
                .unwrap_or_else(|_| cx.options.root.to_path_buf())
                .display()
                .to_string();
            let family = Family {
                options: cx.options,
                delegation,
                route: delegation.route.as_ref().unwrap_or(cx.route),
                fallbacks: if delegation.route.is_some() {
                    &[]
                } else {
                    cx.fallbacks
                },
                workspace,
                session: cx.options.state_key().to_owned(),
            };
            let started = Instant::now();
            let run = run_child(
                0,
                sid.to_owned(),
                &task,
                Some(history),
                Some(msg.to_owned()),
                &family,
            )
            .await;
            let elapsed = started.elapsed();
            let entry = format_result_entry(0, &task, &run, elapsed, &family.route.model);
            return json!({"results": [entry], "total_duration_seconds": round2(elapsed)});
        }
        if !action.is_empty() && action != "spawn" {
            return error(&format!(
                "Unknown action '{action}'. Use spawn (default), list, steer, stop, or continue."
            ));
        }

        let depth = cx.options.delegate_depth;
        let max_spawn = delegation.settings.max_spawn_depth;
        if depth >= max_spawn {
            return error(&format!(
                "Delegation depth limit reached (depth={depth}, max_spawn_depth={max_spawn}). Raise delegation.max_spawn_depth in config.yaml if deeper nesting is required (no hard ceiling, but each level multiplies API cost)."
            ));
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
            .canonical()
            .unwrap_or_else(|_| cx.options.root.to_path_buf())
            .display()
            .to_string();
        let family = Family {
            options: cx.options,
            delegation,
            route: delegation.route.as_ref().unwrap_or(cx.route),
            fallbacks: if delegation.route.is_some() {
                &[]
            } else {
                cx.fallbacks
            },
            workspace,
            session: cx.options.state_key().to_owned(),
        };
        let ids: Vec<String> = (0..tasks.len()).map(|_| delegation.allocate_id()).collect();
        let started = Instant::now();
        let runs = join_all(tasks.iter().enumerate().map(|(index, task)| {
            let family = &family;
            let sid = ids[index].clone();
            async move {
                let began = Instant::now();
                let run = run_child(index, sid, task, None, None, family).await;
                let session = format!("{}#child{index}", family.session);
                crate::terminal::forget_session(&session);
                crate::execute_code::end_session(&session).await;
                (run, began.elapsed())
            }
        }))
        .await;
        let mut results = Vec::new();
        for (index, (run, elapsed)) in runs.into_iter().enumerate() {
            let task = &tasks[index];
            results.push(format_result_entry(
                index,
                task,
                &run,
                elapsed,
                &family.route.model,
            ));
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
            answer_for_child(&ask, None, false),
            ApprovalDecision::Denied
        ));
        assert!(matches!(
            answer_for_child(&ask, None, true),
            ApprovalDecision::Approved
        ));
        let clarify = request(ApprovalAction::Clarify, "[]");
        assert!(matches!(
            answer_for_child(&clarify, None, true),
            ApprovalDecision::Unavailable
        ));
    }

    #[test]
    fn settings_read_the_delegation_section() {
        let config: serde_yaml::Value = serde_yaml::from_str(
            "delegation: {max_concurrent_children: 4, oneshot_max_children: 0, max_iterations: 30, subagent_auto_approve: true}",
        )
        .unwrap();
        let settings = DelegationSettings::from_config(&config, &std::collections::HashMap::new());
        assert_eq!(
            (
                settings.max_concurrent_children,
                settings.oneshot_max_children,
                settings.max_iterations,
                settings.subagent_auto_approve
            ),
            (4, 0, 30, true)
        );
        let default = DelegationSettings::from_config(
            &serde_yaml::Value::Null,
            &std::collections::HashMap::new(),
        );
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
