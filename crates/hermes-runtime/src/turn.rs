//! One in-memory conversation turn with local tools.

use std::future::Future;
use std::path::Path;

use hermes_core::prompt::{cli_system_prompt, update_cli_prompt_route};
use serde_json::{json, Value};

use crate::compaction::{messages_bytes, CompactedHistory, CompactionSettings};
use crate::file_tools::FileWorkspace;
use crate::mcp::McpClients;
use crate::memory_tool::{MemorySettings, MemoryTurn};
use crate::prompt::{load_cli_context, PromptSettings};
use crate::providers::{Provider, ProviderError};
use crate::skills::SkillCatalog;
use crate::terminal::{self, TERMINAL};
use crate::todo_tool::TodoStore;
use crate::tool_policy::ToolPolicy;
use crate::tools::{self, ToolContext, ToolEnv};
use crate::web_tools::WebSettings;

pub use crate::file_tools::{ApprovalAction, ApprovalDecision, ApprovalRequest};

const ITERATION_SUMMARY_REQUEST: &str = "You've reached the maximum number of tool-calling iterations allowed. Please provide a final response summarizing what you've found and accomplished so far, without calling any more tools.";

#[derive(Debug)]
pub enum TurnError {
    Provider(ProviderError),
    InvalidCompletion,
    InvalidIterationSummary,
}

pub enum TurnOutcome {
    Completed(CompletedTurn),
    IterationLimit,
}

pub struct CompletedTurn {
    pub(crate) model: String,
    pub(crate) system_prompt: String,
    pub(crate) replayed_history: Vec<Value>,
    pub(crate) messages: Vec<Value>,
    pub(crate) compaction: Option<CompactedHistory>,
}

#[derive(Clone, Default)]
pub struct TurnHistory {
    pub(crate) system_prompt: Option<String>,
    pub(crate) messages: Vec<Value>,
    pub(crate) skills: SkillCatalog,
    pub(crate) policy: ToolPolicy,
    pub(crate) env: ToolEnv,
    pub(crate) compaction: Option<CompactedHistory>,
    pub(crate) write_files: bool,
    pub(crate) terminal_enabled: bool,
}

/// Why a compaction pass runs.
#[derive(Clone, Copy)]
pub enum CompactionTrigger<'a> {
    /// Before a turn, when the estimated input reaches the configured threshold.
    Estimate { query: &'a str },
    /// After the provider reported a context overflow.
    Overflow,
}

pub enum CompactionOutcome {
    Unchanged,
    Skipped,
    Applied,
}

impl TurnHistory {
    pub fn is_compacted(&self) -> bool {
        self.compaction.is_some()
    }

    pub fn capture_prompt(
        &mut self,
        options: &TurnOptions<'_>,
        settings: &PromptSettings,
        route: &ModelRoute,
    ) {
        self.write_files = options.can_write_files();
        self.terminal_enabled = options.can_run_terminal();
        self.policy = options.tools.clone();
        self.env = ToolEnv::from_options(options);
        // The skills index is built only when a skills tool is enabled.
        self.skills = if self.policy.allows_skills() {
            SkillCatalog::load(
                options.profile,
                options.root,
                &settings.skills,
                self.write_files,
                self.terminal_enabled,
            )
        } else {
            SkillCatalog::default()
        };
        if self.system_prompt.is_none() {
            let context = load_cli_context(
                options.profile,
                options.root,
                settings,
                self.skills.prompt(),
            );
            self.system_prompt = Some(context.render(&route.model, route.provider_name));
        }
    }

    pub async fn compact(
        &mut self,
        profile: Option<&Path>,
        root: &Path,
        prompt: &PromptSettings,
        settings: &CompactionSettings,
        route: &ModelRoute,
        trigger: CompactionTrigger<'_>,
    ) -> CompactionOutcome {
        if !settings.enabled() || self.messages.is_empty() {
            return CompactionOutcome::Unchanged;
        }
        let system = route.system_prompt(self.system_prompt.as_deref());
        let write_files = self.write_files;
        let terminal_enabled = self.terminal_enabled;
        let old_prefix = prefix_bytes(&system, &self.env);
        let input_bytes = match trigger {
            CompactionTrigger::Estimate { query } => {
                old_prefix + messages_bytes(&self.messages) + query.len()
            }
            // The provider already rejected the request, so the estimate no longer gates it.
            CompactionTrigger::Overflow => usize::MAX,
        };
        let Some(range) = settings.select(&self.messages, input_bytes) else {
            return CompactionOutcome::Unchanged;
        };
        let middle = &self.messages[range.clone()];
        let Some(request) = settings.request(&route.model, middle) else {
            return CompactionOutcome::Skipped;
        };
        let Ok(result) = route.provider.complete(&request, |_| {}).await else {
            return CompactionOutcome::Skipped;
        };
        if result.finish_reason.as_deref() != Some("stop") || !result.tool_calls.is_empty() {
            return CompactionOutcome::Skipped;
        }
        let removed_bytes = messages_bytes(middle);
        let Some(compaction) = CompactedHistory::new(range, &result.content) else {
            return CompactionOutcome::Skipped;
        };
        let summary_bytes =
            messages_bytes(&[json!({"role": "user", "content": compaction.summary})]);
        if summary_bytes >= removed_bytes {
            return CompactionOutcome::Skipped;
        }

        let profile = profile.map(Path::to_owned);
        let root = root.to_owned();
        let settings = prompt.clone();
        let model = route.model.clone();
        let provider = route.provider_name;
        let skills_allowed = self.policy.allows_skills();
        let refreshed = tokio::task::spawn_blocking(move || {
            let skills = if skills_allowed {
                SkillCatalog::load(
                    profile.as_deref(),
                    &root,
                    &settings.skills,
                    write_files,
                    terminal_enabled,
                )
            } else {
                SkillCatalog::default()
            };
            let prompt = load_cli_context(profile.as_deref(), &root, &settings, skills.prompt());
            (prompt.render(&model, provider), skills)
        })
        .await;
        let Ok((system, skills)) = refreshed else {
            return CompactionOutcome::Skipped;
        };
        // A newly loaded memory or project file can outweigh the savings.
        if prefix_bytes(&system, &self.env) + summary_bytes >= old_prefix + removed_bytes {
            return CompactionOutcome::Skipped;
        }
        self.system_prompt = Some(system);
        self.skills = skills;
        self.compaction = Some(compaction);
        CompactionOutcome::Applied
    }
}

pub struct ModelRoute {
    pub provider: Provider,
    pub provider_name: &'static str,
    pub model: String,
}

impl ModelRoute {
    fn system_prompt(&self, saved: Option<&str>) -> String {
        match saved {
            Some(saved) => update_cli_prompt_route(saved, &self.model, self.provider_name),
            None => cli_system_prompt(&self.model, self.provider_name),
        }
    }
}

pub enum TurnEvent<'a> {
    Text(&'a str),
    Fallback(&'a str),
    ToolStarted(&'a str),
    ToolFinished { name: &'a str, succeeded: bool },
    IterationLimit(u32),
}

#[derive(Clone, Copy)]
pub struct TurnOptions<'a> {
    /// Connected MCP servers whose tools are added to every request.
    pub mcp: Option<&'a tokio::sync::Mutex<McpClients>>,
    /// Web backends; the web tools are offered only when one is configured.
    pub web: Option<&'a WebSettings>,
    /// Memory store limits; the tool is offered when a profile directory and a writable store exist.
    pub memory: Option<&'a MemorySettings>,
    pub root: &'a Path,
    pub profile: Option<&'a Path>,
    /// The saved session this turn continues, which `session_search` leaves out of recall.
    pub session_id: Option<&'a str>,
    pub max_turns: u32,
    pub approval_timeout_secs: Option<u32>,
    pub terminal_local: bool,
    /// Seconds to wait for an answer to `clarify`; 0 waits without limit.
    pub clarify_timeout_secs: u32,
    /// Tools the profile's toolset configuration allows.
    pub tools: &'a ToolPolicy,
}

impl TurnOptions<'_> {
    pub(crate) fn can_write_files(&self) -> bool {
        self.approval_timeout_secs.is_some() && cfg!(target_os = "linux")
    }

    pub(crate) fn can_run_terminal(&self) -> bool {
        self.approval_timeout_secs.is_some() && self.terminal_local && terminal::SUPPORTED
    }
}

pub async fn run_turn<F, A, D>(
    primary: &ModelRoute,
    fallbacks: &[ModelRoute],
    query: &str,
    options: TurnOptions<'_>,
    history: TurnHistory,
    mut on_event: F,
    mut approve: A,
) -> Result<TurnOutcome, TurnError>
where
    F: FnMut(TurnEvent<'_>),
    A: FnMut(ApprovalRequest) -> D + Send,
    D: Future<Output = ApprovalDecision> + Send + 'static,
{
    let mut files = FileWorkspace::new(options.root, options.profile);
    // The task list lives in the history, so resuming a session keeps it.
    let mut todo = TodoStore::from_history(&history.messages);
    let saved_prompt = history.system_prompt.as_deref();
    let mut system_prompt = primary.system_prompt(saved_prompt);
    let (mut messages, original) = match &history.compaction {
        Some(compaction) => (compaction.replay(&history.messages), Some(history.messages)),
        None => (history.messages, None),
    };
    messages.insert(0, json!({"role": "system", "content": system_prompt}));
    let first_new_message = messages.len();
    messages.push(json!({"role": "user", "content": query}));
    let mut env = ToolEnv::from_options(&options);
    env.skills_indexed = system_prompt.contains("\n<available_skills>\n");
    let mut tools = tools::offered_schemas(&env);
    if let Some(mcp) = options.mcp {
        tools.extend(mcp.lock().await.schemas());
    }
    tools::sort_by_name(&mut tools);
    let mut memory_turn = MemoryTurn::default();
    let mut approve_dyn =
        |request: ApprovalRequest| -> tools::BoxFuture<'static, ApprovalDecision> {
            Box::pin(approve(request))
        };
    let mut route = primary;
    let mut fallback_routes = fallbacks.iter();
    for _ in 0..options.max_turns {
        let result = loop {
            let request = chat_request(&route.model, &messages, &tools);
            let mut emitted_text = false;
            let result = route
                .provider
                .complete(&request, |text| {
                    emitted_text |= !text.is_empty();
                    on_event(TurnEvent::Text(text));
                })
                .await;
            match result {
                Ok(result) => break result,
                Err(error) => {
                    if emitted_text || !error.allows_fallback() {
                        return Err(TurnError::Provider(error));
                    }
                    let Some(next) = fallback_routes.next() else {
                        return Err(TurnError::Provider(error));
                    };
                    route = next;
                    system_prompt = route.system_prompt(saved_prompt);
                    messages[0] = json!({"role": "system", "content": system_prompt});
                    on_event(TurnEvent::Fallback(&route.model));
                }
            }
        };
        match result.finish_reason.as_deref() {
            Some("stop") if result.tool_calls.is_empty() => {
                messages.push(json!({"role": "assistant", "content": result.content}));
                let appended = messages.split_off(first_new_message);
                messages.remove(0);
                return Ok(TurnOutcome::Completed(CompletedTurn {
                    model: route.model.clone(),
                    system_prompt,
                    replayed_history: original.unwrap_or(messages),
                    messages: appended,
                    compaction: history.compaction,
                }));
            }
            Some("tool_calls") if !result.tool_calls.is_empty() => {}
            _ => return Err(TurnError::InvalidCompletion),
        }
        let calls: Vec<Value> = result
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments}
                })
            })
            .collect();
        messages.push(json!({"role": "assistant", "content": if result.content.is_empty() { None } else { Some(result.content) }, "tool_calls": calls}));
        for call in result.tool_calls {
            on_event(TurnEvent::ToolStarted(&call.name));
            let mcp_tool = match options.mcp {
                Some(mcp) if call.name.starts_with("mcp_") => {
                    let mcp = mcp.lock().await;
                    mcp.owns(&call.name).then_some(())
                }
                _ => None,
            };
            let output = if mcp_tool.is_some() {
                match options.mcp {
                    Some(mcp) => mcp.lock().await.call(&call.name, &call.arguments).await,
                    None => json!({"error": format!("Unknown tool: {}", call.name)}),
                }
            } else if let Some(tool) = tools::find(&call.name, options.tools) {
                let mut context = ToolContext {
                    options: &options,
                    skills: &history.skills,
                    files: &mut files,
                    memory_turn: &mut memory_turn,
                    todo: &mut todo,
                    approve: &mut approve_dyn,
                };
                tool.call(&mut context, &call.arguments).await
            } else {
                // A tool the toolset configuration does not allow is unknown, whatever the model sends.
                json!({"error": format!("Unknown tool: {}", call.name)})
            };
            // A tool may answer with ready-made text, such as the web tools' indented JSON.
            let (content, structured) = match output {
                Value::String(text) => {
                    let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
                    (text, parsed)
                }
                other => (other.to_string(), other),
            };
            let succeeded = structured.get("error").is_none_or(Value::is_null)
                && structured["success"].as_bool() != Some(false)
                && (call.name != TERMINAL || structured["exit_code"].as_i64() == Some(0));
            messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": content}));
            on_event(TurnEvent::ToolFinished {
                name: &call.name,
                succeeded,
            });
        }
    }
    on_event(TurnEvent::IterationLimit(options.max_turns));
    messages.push(json!({"role": "user", "content": ITERATION_SUMMARY_REQUEST}));
    let request = chat_request(&route.model, &messages, &tools);
    // Keep the active route and tool definitions so the provider can reuse its prompt prefix.
    // Emit the summary only after verifying that the completion contains no tool calls.
    let summary = route
        .provider
        .complete(&request, |_| {})
        .await
        .map_err(TurnError::Provider)?;
    if summary.finish_reason.as_deref() != Some("stop")
        || !summary.tool_calls.is_empty()
        || summary.content.trim().is_empty()
    {
        return Err(TurnError::InvalidIterationSummary);
    }
    on_event(TurnEvent::Text(&summary.content));
    Ok(TurnOutcome::IterationLimit)
}

/// A chat request. An empty `tools` array is omitted, since providers reject it.
fn chat_request(model: &str, messages: &[Value], tools: &[Value]) -> Value {
    let mut request = json!({"model": model, "messages": messages});
    if !tools.is_empty() {
        request["tools"] = json!(tools);
    }
    request
}

fn prefix_bytes(system: &str, env: &ToolEnv) -> usize {
    let mut env = env.clone();
    env.skills_indexed = system.contains("\n<available_skills>\n");
    system.len() + messages_bytes(&tools::offered_schemas(&env))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::file_tools::FileWorkspace;
    use serde_json::{json, Value};

    fn read_file(root: &Path, arguments: &str) -> Value {
        FileWorkspace::new(root, None).read(arguments)
    }

    #[test]
    fn reads_paginated_utf8_with_one_based_line_numbers() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("file.txt"), "one\nдва\nthree\n").unwrap();
        let result = read_file(
            directory.path(),
            r#"{"path":"file.txt","offset":2,"limit":1}"#,
        );
        assert_eq!(result["content"], "2|два");
        assert_eq!(result["total_lines"], 3);
        assert_eq!(result["file_size"], 17);
        assert_eq!(result["truncated"], true);
    }

    #[test]
    fn rejects_paths_outside_the_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let arguments = json!({"path": outside.path()}).to_string();
        let result = read_file(directory.path(), &arguments);
        assert_eq!(result["error"], "Path is outside the working directory");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside_the_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
        let result = read_file(directory.path(), r#"{"path":"link"}"#);
        assert_eq!(result["error"], "Path is outside the working directory");
    }

    #[test]
    fn rejects_invalid_pagination_and_returns_error_for_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let invalid = read_file(directory.path(), r#"{"path":"file","offset":0}"#);
        assert_eq!(invalid["error"], "Invalid offset");
        let missing = read_file(directory.path(), r#"{"path":"missing"}"#);
        assert!(missing["error"].is_string());
    }

    #[test]
    fn rejects_project_env_files_even_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".env"), "TOKEN=secret").unwrap();
        let direct = read_file(directory.path(), r#"{"path":".env"}"#);
        assert!(direct["error"].is_string());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(".env", directory.path().join("env-link")).unwrap();
            let linked = read_file(directory.path(), r#"{"path":"env-link"}"#);
            assert!(linked["error"].is_string());
        }
    }
}
