//! One in-memory conversation turn with local tools.

use std::future::Future;
use std::path::Path;

use serde_json::{json, Value};

use crate::browser_cdp::BrowserSettings;
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
use crate::tool_search;
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
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

impl CompletedTurn {
    /// Provider requests the turn made: one per assistant message.
    pub(crate) fn assistant_messages(&self) -> usize {
        self.messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .count()
    }

    /// Tokens the provider reported for the turn.
    pub fn usage(&self) -> (u64, u64) {
        (self.input_tokens, self.output_tokens)
    }

    /// The text of the last assistant message.
    pub(crate) fn final_text(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|message| message["role"] == "assistant")
            .and_then(|message| message["content"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    /// The conversation so far, to continue it with another turn.
    pub(crate) fn into_history(self) -> TurnHistory {
        let mut messages = self.replayed_history;
        messages.extend(self.messages);
        TurnHistory {
            system_prompt: Some(self.system_prompt),
            messages,
            ..TurnHistory::default()
        }
    }
}

#[derive(Clone, Default)]
pub struct TurnHistory {
    pub(crate) system_prompt: Option<String>,
    pub(crate) messages: Vec<Value>,
    pub(crate) skills: SkillCatalog,
    /// `skills.auto_load` blocks, rendered once and kept across prompt rebuilds.
    pub(crate) pinned: Option<String>,
    pub(crate) policy: ToolPolicy,
    pub(crate) env: ToolEnv,
    pub(crate) compaction: Option<CompactedHistory>,
    pub(crate) write_files: bool,
    pub(crate) terminal_enabled: bool,
    /// A `-q` run: the skills guidance is the shorter one.
    pub(crate) single_query: bool,
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
        self.single_query = options.single_query;
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
        self.env.skill_desc_limit = self.skills.desc_limit();
        // The skills tools are offered when the prompt carries the index, so the index decides
        // the tool names the prompt's own gates see.
        let index = self.skills.prompt(
            self.single_query,
            offered_names(&self.env).contains("web_search"),
        );
        if self.system_prompt.is_none() {
            self.env.skills_indexed = index.is_some();
        }
        if self.pinned.is_none() {
            self.pinned = Some(pinned_skills(
                &self.skills,
                self.policy.allows_skills(),
                options.profile,
                options.session_id,
            ));
        }
        if self.system_prompt.is_none() {
            self.system_prompt = Some(load_cli_context(
                options.profile,
                options.root,
                settings,
                index,
                self.pinned.clone(),
                &offered_names(&self.env),
                &route.model,
                route.provider_name,
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn compact(
        &mut self,
        profile: Option<&Path>,
        root: &Path,
        prompt: &PromptSettings,
        settings: &CompactionSettings,
        route: &ModelRoute,
        summarizer: Option<&ModelRoute>,
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
        let summary_route = summarizer.unwrap_or(route);
        let Some(request) = settings.request(&summary_route.model, middle) else {
            return CompactionOutcome::Skipped;
        };
        // A configured summary route carries its own body fields; the main route's are left out.
        let sent = if summarizer.is_some() {
            summary_route.provider.complete(&request, |_| {}).await
        } else {
            summary_route
                .provider
                .complete_plain(&request, |_| {})
                .await
        };
        let Ok(result) = sent else {
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
        let tools_offered = offered_names(&self.env);
        let pinned = self.pinned.clone();
        let single_query = self.single_query;
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
            let prompt = load_cli_context(
                profile.as_deref(),
                &root,
                &settings,
                skills.prompt(single_query, tools_offered.contains("web_search")),
                pinned,
                &tools_offered,
                &model,
                provider,
            );
            (prompt, skills)
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

/// The `skills.auto_load` blocks for a new agent. Like the reference it is empty when no skills
/// tool is offered or `HERMES_IGNORE_RULES` is set, and skills that cannot be loaded are named on
/// stderr and skipped.
fn pinned_skills(
    skills: &SkillCatalog,
    offered: bool,
    profile: Option<&Path>,
    session_id: Option<&str>,
) -> String {
    let ignored = std::env::var("HERMES_IGNORE_RULES").is_ok_and(|value| {
        matches!(
            value.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    });
    if !offered || ignored {
        return String::new();
    }
    if skills.settings().inline_shell() {
        eprintln!("warning: skills.inline_shell is set but the port never runs a skill's !`cmd` snippets; they stay as written");
    }
    let (text, missing) = skills.pinned(profile, session_id);
    if !missing.is_empty() {
        eprintln!(
            "warning: skills.auto_load: skill(s) not found or disabled, skipped: {}",
            missing.join(", ")
        );
    }
    text
}

/// The names of the tools a model is offered under `env`, as the prompt gates see them.
pub(crate) fn offered_names(env: &ToolEnv) -> std::collections::BTreeSet<String> {
    let mut tools = tools::offered_schemas(env);
    tools::append_bridge(env, &mut tools);
    tools
        .iter()
        .filter_map(|schema| schema["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

pub struct ModelRoute {
    pub provider: Provider,
    pub provider_name: &'static str,
    pub model: String,
}

impl ModelRoute {
    fn system_prompt(&self, saved: Option<&str>) -> String {
        match saved {
            Some(saved) => {
                crate::system_prompt::update_route_lines(saved, &self.model, self.provider_name)
            }
            None => format!(
                "{}\n\nModel: {}\nProvider: {}\nPlatform: cli",
                "You are Hermes Agent, built by Nous Research.", self.model, self.provider_name
            ),
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
    /// The browser's DevTools endpoint; `browser_cdp` is offered when one is configured.
    pub browser: Option<&'a BrowserSettings>,
    /// Memory store limits; the tool is offered when a profile directory and a writable store exist.
    pub memory: Option<&'a MemorySettings>,
    pub root: &'a Path,
    pub profile: Option<&'a Path>,
    /// The saved session this turn continues, which `session_search` leaves out of recall.
    pub session_id: Option<&'a str>,
    /// This is a `-q` run, not an interactive conversation.
    pub single_query: bool,
    pub max_turns: u32,
    pub approval_timeout_secs: Option<u32>,
    /// The terminal's own limit. A command the detector does not flag needs no answer, so the terminal is
    /// offered to a run with no person to ask; a flagged one is refused there unless the run's mode approves.
    pub terminal_approval_timeout_secs: Option<u32>,
    pub terminal_local: bool,
    /// Which environment variables commands see.
    pub terminal_env: Option<&'a crate::terminal::TerminalSettings>,
    /// Seconds to wait for an answer to `clarify`; 0 waits without limit.
    pub clarify_timeout_secs: u32,
    /// Tools the profile's toolset configuration allows.
    pub tools: &'a ToolPolicy,
    /// Where to record what the turn sent, received and ran, when asked.
    pub journal: Option<&'a crate::journal::Journal>,
    /// Subagents: offered when set, and what a run may spend on them.
    pub delegation: Option<&'a crate::delegation::Delegation>,
    /// A subagent's own system prompt, used as is.
    pub system_prompt_override: Option<&'a str>,
    /// The index of the subagent this turn is, when it is one.
    pub child: Option<usize>,
    /// Messages sent after the system prompt on every request (`prefill_messages_file`).
    pub prefill: Option<&'a [Value]>,
    /// Keeps the working directory, background processes and Python kernel of an interactive
    /// conversation together across turns, whatever session id each turn is saved under.
    pub state_scope: Option<&'a str>,
}

impl TurnOptions<'_> {
    /// The key under which a turn's working directory, processes and kernel are kept.
    pub(crate) fn state_key(&self) -> &str {
        self.state_scope.or(self.session_id).unwrap_or("default")
    }

    pub(crate) fn can_write_files(&self) -> bool {
        self.approval_timeout_secs.is_some() && cfg!(target_os = "linux")
    }

    pub(crate) fn can_run_terminal(&self) -> bool {
        self.terminal_approval_timeout_secs.is_some() && self.terminal_local && terminal::SUPPORTED
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
    let mut files = FileWorkspace::new(options.root, options.profile).with_limits(
        options
            .terminal_env
            .map(|terminal| terminal.output.clone())
            .unwrap_or_default(),
    );
    // The task list lives in the history, so resuming a session keeps it.
    let mut todo = TodoStore::from_history(&history.messages);
    let saved_prompt = history.system_prompt.as_deref();
    let mut system_prompt = match options.system_prompt_override {
        Some(prompt) => prompt.to_owned(),
        None => primary.system_prompt(saved_prompt),
    };
    let (mut messages, original) = match &history.compaction {
        Some(compaction) => (compaction.replay(&history.messages), Some(history.messages)),
        None => (history.messages, None),
    };
    messages.insert(0, json!({"role": "system", "content": system_prompt}));
    let first_new_message = messages.len();
    messages.push(json!({"role": "user", "content": query}));
    let mut env = ToolEnv::from_options(&options);
    env.skills_indexed = system_prompt.contains("\n<available_skills>\n");
    env.skill_desc_limit = history.skills.desc_limit();
    let mut tools = tools::offered_schemas(&env);
    if let Some(mcp) = options.mcp {
        tools.extend(mcp.lock().await.schemas());
    }
    tools::sort_by_name(&mut tools);
    tools::append_bridge(&env, &mut tools);
    let mut memory_turn = MemoryTurn::default();
    let mut usage = (0u64, 0u64);
    let mut approve_dyn =
        |request: ApprovalRequest| -> tools::BoxFuture<'static, ApprovalDecision> {
            Box::pin(approve(request))
        };
    let mut route = primary;
    let mut fallback_routes = fallbacks.iter();
    for _ in 0..options.max_turns {
        let result = loop {
            let request = chat_request(
                &route.model,
                &messages,
                &tools,
                options.prefill.unwrap_or(&[]),
            );
            if let Some(journal) = options.journal {
                journal.record_for(options.child, "request", json!({"body": request}));
            }
            let mut emitted_text = false;
            let result = route
                .provider
                .complete(&request, |text| {
                    emitted_text |= !text.is_empty();
                    on_event(TurnEvent::Text(text));
                })
                .await;
            match result {
                Ok(result) => {
                    if let Some(reported) = result.usage {
                        usage.0 += reported.input_tokens;
                        usage.1 += reported.output_tokens;
                    }
                    if let Some(journal) = options.journal {
                        journal.record_for(
                            options.child,
                            "response",
                            json!({
                                "content": result.content,
                                "finish_reason": result.finish_reason,
                                "usage": result.usage.map(|usage| json!({
                                    "input_tokens": usage.input_tokens,
                                    "output_tokens": usage.output_tokens,
                                })),
                                "tool_calls": result.tool_calls.iter().map(|call| json!({
                                    "id": call.id, "name": call.name, "arguments": call.arguments
                                })).collect::<Vec<_>>(),
                            }),
                        );
                    }
                    break result;
                }
                Err(error) => {
                    if let Some(journal) = options.journal {
                        journal.record_for(
                            options.child,
                            "provider_error",
                            json!({"error": format!("{error:?}")}),
                        );
                    }
                    if emitted_text || !error.allows_fallback() {
                        return Err(TurnError::Provider(error));
                    }
                    let Some(next) = fallback_routes.next() else {
                        return Err(TurnError::Provider(error));
                    };
                    route = next;
                    if options.system_prompt_override.is_none() {
                        system_prompt = route.system_prompt(saved_prompt);
                    }
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
                    input_tokens: usage.0,
                    output_tokens: usage.1,
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
            // The bridge answers searches and descriptions itself and hands a call back as the
            // real tool, which every later step then sees by its own name.
            let (name, arguments, bridged) =
                match tool_search::dispatch(&env, &call.name, &call.arguments) {
                    Some(tool_search::Bridged::Call { name, arguments }) => (name, arguments, None),
                    Some(tool_search::Bridged::Done(answer)) => {
                        (call.name.clone(), call.arguments.clone(), Some(answer))
                    }
                    None => (call.name.clone(), call.arguments.clone(), None),
                };
            let through_bridge = name != call.name;
            on_event(TurnEvent::ToolStarted(&name));
            let started = std::time::Instant::now();
            let mcp_tool = match options.mcp {
                Some(mcp) if name.starts_with("mcp_") && !through_bridge => {
                    let mcp = mcp.lock().await;
                    mcp.owns(&name).then_some(())
                }
                _ => None,
            };
            let output = if let Some(answer) = bridged {
                answer
            } else if mcp_tool.is_some() {
                match options.mcp {
                    Some(mcp) => mcp.lock().await.call(&name, &arguments).await,
                    None => json!({"error": format!("Unknown tool: {name}")}),
                }
            } else if let Some(tool) = if through_bridge {
                tools::find_deferred(&name, options.tools)
            } else {
                tools::find(&name, options.tools)
            } {
                let mut context = ToolContext {
                    options: &options,
                    skills: &history.skills,
                    files: &mut files,
                    memory_turn: &mut memory_turn,
                    todo: &mut todo,
                    approve: &mut approve_dyn,
                    route,
                    fallbacks,
                };
                tool.call(&mut context, &arguments).await
            } else {
                // A tool the toolset configuration does not allow is unknown, whatever the model sends.
                json!({"error": format!("Unknown tool: {name}")})
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
                && (name != TERMINAL || structured["exit_code"].as_i64() == Some(0));
            if let Some(journal) = options.journal {
                journal.record_for(
                    options.child,
                    "tool",
                    json!({
                        "id": call.id, "name": name, "via": call.name, "arguments": arguments,
                        "output": content, "succeeded": succeeded,
                        "millis": started.elapsed().as_millis() as u64,
                    }),
                );
            }
            messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": content}));
            on_event(TurnEvent::ToolFinished {
                name: &name,
                succeeded,
            });
        }
    }
    on_event(TurnEvent::IterationLimit(options.max_turns));
    messages.push(json!({"role": "user", "content": ITERATION_SUMMARY_REQUEST}));
    let request = chat_request(
        &route.model,
        &messages,
        &tools,
        options.prefill.unwrap_or(&[]),
    );
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
/// The request body. `prefill` messages go right after the system prompt in what is sent, but
/// never into the conversation that is kept.
fn chat_request(model: &str, messages: &[Value], tools: &[Value], prefill: &[Value]) -> Value {
    let mut sent: Vec<Value> = messages.to_vec();
    let at = usize::from(sent.first().is_some_and(|first| first["role"] == "system"));
    for (index, message) in prefill.iter().enumerate() {
        sent.insert(at + index, message.clone());
    }
    let mut request = json!({"model": model, "messages": sent});
    if !tools.is_empty() {
        request["tools"] = json!(tools);
    }
    request
}

fn prefix_bytes(system: &str, env: &ToolEnv) -> usize {
    let mut env = env.clone();
    env.skills_indexed = system.contains("\n<available_skills>\n");
    let mut tools = tools::offered_schemas(&env);
    tools::append_bridge(&env, &mut tools);
    system.len() + messages_bytes(&tools)
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
    fn file_content_is_masked_with_a_sentinel_that_cannot_be_written_back() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(".netrc"),
            "machine example.test login alice password hunter2hunter2\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("app.py"),
            "MAX_TOKENS=100\nKEY = 'ghp_abcdefghijklmnop1234567890'\nAPI_KEY=Zq8x4kLm2PvN7wRt9s\n",
        )
        .unwrap();
        let netrc = read_file(directory.path(), r#"{"path": ".netrc"}"#);
        assert_eq!(
            netrc["content"],
            "1|machine example.test login alice password «redacted-secret»"
        );
        let source = read_file(directory.path(), r#"{"path": "app.py"}"#);
        assert_eq!(
            source["content"],
            "1|MAX_TOKENS=100\n2|KEY = '«redacted:ghp_…»'\n3|API_KEY=Zq8x4kLm2PvN7wRt9s"
        );
    }

    #[test]
    fn reads_follow_the_reference_budget_hints_and_suggestions() {
        let directory = tempfile::tempdir().unwrap();
        let body: String = (1..=3000).map(|i| format!("row {i}\n")).collect();
        std::fs::write(directory.path().join("big.txt"), &body).unwrap();
        std::fs::write(directory.path().join("empty.txt"), "").unwrap();
        std::fs::write(directory.path().join("config.yaml"), "a: 1\n").unwrap();
        let page = read_file(directory.path(), r#"{"path": "big.txt"}"#);
        assert_eq!(page["total_lines"], 3000);
        assert_eq!(page["truncated"], true);
        assert_eq!(
            page["hint"],
            "Use offset=2001 to continue reading (showing 1-2000 of 3000 lines)"
        );
        let long: String = (1..=2500)
            .map(|i| format!("{i} {}\n", "x".repeat(100)))
            .collect();
        std::fs::write(directory.path().join("long.txt"), &long).unwrap();
        let budget = read_file(directory.path(), r#"{"path": "long.txt", "limit": 2000}"#);
        assert_eq!(budget["truncated_by"], "bytes");
        assert!(budget["hint"]
            .as_str()
            .unwrap()
            .starts_with("Output truncated at the 100,000-char read budget"));
        assert!(budget["content"].as_str().unwrap().chars().count() <= 100_000);
        assert_eq!(
            read_file(directory.path(), r#"{"path": "empty.txt"}"#)["hint"],
            "File is empty (0 bytes)."
        );
        let past = read_file(directory.path(), r#"{"path": "config.yaml", "offset": 9}"#);
        assert!(past["hint"]
            .as_str()
            .unwrap()
            .contains("beyond the end of the file (1 lines total)"));
        let missing = read_file(directory.path(), r#"{"path": "config.yml"}"#);
        assert_eq!(missing["error"], "File not found: config.yml");
        assert_eq!(missing["not_found"], true);
        assert_eq!(missing["similar_files"][0], "./config.yaml");
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
