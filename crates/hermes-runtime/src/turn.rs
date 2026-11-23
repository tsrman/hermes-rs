//! One in-memory conversation turn with local tools.

use std::future::Future;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::browser_cdp::BrowserSettings;
use crate::compaction::{
    merge_summary_into_tail_row, messages_bytes, plan_summary_placement, CompactedHistory,
    CompactionSettings, PersistedSummaryPlacement,
};
pub use crate::compaction::{CompactionBlockReason, CompactionGuard};
use crate::file_tools::FileWorkspace;
use crate::mcp::McpClients;
use crate::memory_tool::{MemorySettings, MemoryTurn};
use crate::prompt::{load_cli_context, PromptSettings};
use crate::providers::{Provider, ProviderError};
use crate::redact::redact_assistant_reply;
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
    /// The model's last text as it arrived, before the stored copy was scrubbed.
    pub(crate) final_response: String,
    pub(crate) system_prompt: String,
    pub(crate) replayed_history: Vec<Value>,
    pub(crate) messages: Vec<Value>,
    pub(crate) compaction: Option<Box<CompactedHistory>>,
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
        self.final_response.clone()
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
    pub(crate) compaction: Option<Box<CompactedHistory>>,
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
    Overflow { query: &'a str },
    /// Explicit `/compress [focus]` invoked by the user between turns.
    Manual { focus: Option<&'a str> },
}

pub enum CompactionOutcome {
    Unchanged,
    Skipped,
    Applied,
}

fn is_timeout_provider_error(error: &ProviderError) -> bool {
    matches!(error, ProviderError::Http(err) if err.is_timeout())
        || matches!(error, ProviderError::HttpStatus(408 | 429 | 502 | 504))
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
        guard: Option<&Mutex<CompactionGuard>>,
    ) -> CompactionOutcome {
        if self.messages.is_empty() {
            return CompactionOutcome::Unchanged;
        }
        let manual_settings;
        let effective = match trigger {
            CompactionTrigger::Manual { .. } => {
                if let Some(guard) = guard {
                    guard.lock().unwrap().reset_for_manual();
                }
                let Some(s) = settings.for_manual() else {
                    return CompactionOutcome::Unchanged;
                };
                manual_settings = s;
                &manual_settings
            }
            CompactionTrigger::Estimate { .. } => {
                if !settings.enabled() {
                    return CompactionOutcome::Unchanged;
                }
                if guard.is_some_and(|g| g.lock().unwrap().check_blocked(false).is_some()) {
                    return CompactionOutcome::Unchanged;
                }
                settings
            }
            CompactionTrigger::Overflow { .. } => {
                if !settings.enabled() {
                    return CompactionOutcome::Unchanged;
                }
                if guard.is_some_and(|g| g.lock().unwrap().check_blocked(true).is_some()) {
                    return CompactionOutcome::Unchanged;
                }
                settings
            }
        };
        let system = route.system_prompt(self.system_prompt.as_deref());
        let write_files = self.write_files;
        let terminal_enabled = self.terminal_enabled;
        let old_prefix = prefix_bytes(&system, &self.env);
        let (query, focus, range) = match trigger {
            CompactionTrigger::Estimate { query } => {
                let input_bytes = old_prefix + messages_bytes(&self.messages) + query.len();
                let Some(range) = effective.select(&self.messages, query, input_bytes, false)
                else {
                    return CompactionOutcome::Unchanged;
                };
                (Some(query), None, range)
            }
            CompactionTrigger::Overflow { query } => {
                let Some(range) = effective.select(&self.messages, query, usize::MAX, false) else {
                    return CompactionOutcome::Unchanged;
                };
                (Some(query), None, range)
            }
            CompactionTrigger::Manual { focus } => {
                let Some(range) = effective.plan_manual(&self.messages) else {
                    return CompactionOutcome::Unchanged;
                };
                (None, focus, range)
            }
        };
        let middle = &self.messages[range.clone()];
        let summary_route = summarizer.unwrap_or(route);
        let Some(request) = effective.request(&summary_route.model, middle, focus) else {
            if let Some(guard) = guard {
                guard.lock().unwrap().record_summary_failure(false);
            }
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
        let result = match sent {
            Ok(result) => result,
            Err(error) => {
                if let Some(guard) = guard {
                    guard
                        .lock()
                        .unwrap()
                        .record_summary_failure(is_timeout_provider_error(&error));
                }
                return CompactionOutcome::Skipped;
            }
        };
        if result.finish_reason.as_deref() != Some("stop") || !result.tool_calls.is_empty() {
            let truncated = result.finish_reason.as_deref() == Some("length");
            if let Some(guard) = guard {
                guard.lock().unwrap().record_summary_failure(truncated);
            }
            return CompactionOutcome::Skipped;
        }
        let mut head = Vec::with_capacity(range.start + 1);
        head.push(json!({"role": "system", "content": system}));
        head.extend(self.messages[..range.start].iter().cloned());
        let persisted_tail_len = self.messages.len() - range.end;
        let mut tail = self.messages[range.end..].to_vec();
        if let Some(q) = query {
            tail.push(json!({"role": "user", "content": q}));
        }
        let placement_plan = plan_summary_placement(&head, &tail, head.len());
        let placement = if placement_plan.merge_into_tail
            && placement_plan.merge_target_idx() < persisted_tail_len
        {
            PersistedSummaryPlacement::MergedIntoTail {
                tail_index: placement_plan.merge_target_idx(),
                summary_role: placement_plan.summary_role,
                force_user_leading: placement_plan.force_user_leading,
            }
        } else {
            PersistedSummaryPlacement::Standalone {
                role: placement_plan.summary_role,
            }
        };
        let Some(compaction) = CompactedHistory::new(range, &result.content, placement) else {
            if let Some(guard) = guard {
                guard.lock().unwrap().record_summary_failure(false);
            }
            return CompactionOutcome::Skipped;
        };
        let old_history_bytes = messages_bytes(&self.messages);
        let replayed = compaction.replay(&self.messages);
        let new_history_bytes = messages_bytes(&replayed);
        if new_history_bytes >= old_history_bytes {
            if let Some(guard) = guard {
                guard.lock().unwrap().record_ineffective();
            }
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
        let new_prefix = prefix_bytes(&system, &self.env);
        if new_prefix + new_history_bytes >= old_prefix + old_history_bytes {
            if let Some(guard) = guard {
                guard.lock().unwrap().record_ineffective();
            }
            return CompactionOutcome::Skipped;
        }
        if let Some(guard) = guard {
            let still_over = query.is_some_and(|q| {
                effective.over_threshold(new_prefix + new_history_bytes + q.len())
            });
            guard.lock().unwrap().record_success(still_over);
        }
        self.system_prompt = Some(system);
        self.skills = skills;
        self.compaction = Some(Box::new(compaction));
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
    ToolFinished {
        name: &'a str,
        succeeded: bool,
    },
    IterationLimit(u32),
    /// The provider rejected the request as too large and the earlier steps of this turn were
    /// replaced by a summary; the request is sent again.
    ContextSqueezed,
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
    /// Compression settings and the route that writes its summaries; a turn that overflows the
    /// context after tool calls uses them to compress its own earlier steps and carry on.
    pub compression: Option<&'a CompactionSettings>,
    pub summarizer: Option<&'a ModelRoute>,
    /// Shared per-session cooldown and anti-thrash guard across turns (REPL); when `None`, a
    /// turn-local guard still protects mid-turn loops from hammering a failing summarizer.
    pub compaction_guard: Option<&'a Mutex<CompactionGuard>>,
    /// `security.redact_phone_numbers: false`: the stored assistant reply keeps its phone numbers.
    pub keep_phone_numbers: bool,
    /// `agent.stall_guards` (default `true`): identical-call loop notices/stubbing and
    /// continue-intent / degenerate-final re-prompts.
    pub stall_guards: bool,
    /// `agent.intent_ack_continuation` (default `"auto"`): governs degenerate-final re-prompting.
    pub intent_ack_continuation: &'a crate::stall_guards::IntentAckContinuation,
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
    // What each request carries. It starts as the conversation and differs from it only after a
    // mid-turn compression, which shortens this copy and never the conversation that is saved.
    let mut sent = messages.clone();
    let mut squeezes = 0usize;
    let local_guard = Mutex::new(CompactionGuard::new());
    let guard = options.compaction_guard.unwrap_or(&local_guard);
    // `compression.max_attempts`: how often this turn may compress its own earlier steps.
    let max_squeezes = options.compression.map_or(3, CompactionSettings::attempts);
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
    let mut stall_tracker = crate::stall_guards::StallGuardTracker::new();
    let mut stall_continuations = 0usize;
    let mut usage = (0u64, 0u64);
    let mut approve_dyn =
        |request: ApprovalRequest| -> tools::BoxFuture<'static, ApprovalDecision> {
            Box::pin(approve(request))
        };
    let mut route = primary;
    let mut fallback_routes = fallbacks.iter();
    for _ in 0..options.max_turns {
        let result = loop {
            // Before the request goes out: once the steps of this turn have outgrown the threshold,
            // replace the early ones with a summary instead of waiting for the provider to refuse.
            if squeezes < max_squeezes && guard.lock().unwrap().check_blocked(false).is_none() {
                if let Some(settings) = options.compression.filter(|s| {
                    s.enabled() && s.over_threshold(messages_bytes(&sent) + messages_bytes(&tools))
                }) {
                    let summarizer = options.summarizer;
                    if squeeze_turn(
                        settings,
                        summarizer.unwrap_or(route),
                        summarizer.is_some(),
                        &mut sent,
                        &tools,
                        first_new_message,
                        squeezes > 0 || original.is_some(),
                        guard,
                    )
                    .await
                    {
                        squeezes += 1;
                        on_event(TurnEvent::ContextSqueezed);
                    }
                }
            }
            let request = chat_request(&route.model, &sent, &tools, options.prefill.unwrap_or(&[]));
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
                    if matches!(error, ProviderError::ContextOverflow)
                        && !emitted_text
                        && squeezes < max_squeezes
                        && guard.lock().unwrap().check_blocked(true).is_none()
                    {
                        if let Some(settings) = options.compression.filter(|s| s.enabled()) {
                            let summarizer = options.summarizer;
                            if squeeze_turn(
                                settings,
                                summarizer.unwrap_or(route),
                                summarizer.is_some(),
                                &mut sent,
                                &tools,
                                first_new_message,
                                squeezes > 0 || original.is_some(),
                                guard,
                            )
                            .await
                            {
                                squeezes += 1;
                                on_event(TurnEvent::ContextSqueezed);
                                continue;
                            }
                        }
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
                    sent[0] = messages[0].clone();
                    on_event(TurnEvent::Fallback(&route.model));
                }
            }
        };
        match result.finish_reason.as_deref() {
            Some("stop") if result.tool_calls.is_empty() => {
                // What is stored is scrubbed like the reference's stored reply; the text the person
                // saw (and a subagent's summary) is the model's own.
                let stored = redact_assistant_reply(&result.content, options.keep_phone_numbers);
                if options.stall_guards
                    && stall_continuations < crate::stall_guards::MAX_STALL_CONTINUATIONS
                {
                    let stripped = crate::stall_guards::strip_think_blocks(&result.content);
                    let ack_mode = options.intent_ack_continuation.resolve(
                        &route.model,
                        matches!(route.provider, Provider::Responses(_)),
                    );
                    let stall_continue = !tools.is_empty()
                        && crate::stall_guards::trailing_continue_intent(&stripped);
                    let degenerate = !stall_continue
                        && ack_mode != "off"
                        && crate::stall_guards::tool_results_since_last_user(&sent) > 0
                        && crate::stall_guards::looks_like_degenerate_final(&stripped, query);
                    if stall_continue || degenerate {
                        stall_continuations += 1;
                        let interim = json!({"role": "assistant", "content": stored});
                        let nudge = json!({
                            "role": "user",
                            "content": if stall_continue {
                                crate::stall_guards::CONTINUE_INTENT_NUDGE
                            } else {
                                crate::stall_guards::DEGENERATE_FINAL_NUDGE
                            }
                        });
                        sent.push(interim.clone());
                        messages.push(interim);
                        sent.push(nudge.clone());
                        messages.push(nudge);
                        continue;
                    }
                }
                messages.push(json!({"role": "assistant", "content": stored}));
                let appended = messages.split_off(first_new_message);
                messages.remove(0);
                return Ok(TurnOutcome::Completed(CompletedTurn {
                    model: route.model.clone(),
                    final_response: result.content,
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
        let content = (!result.content.is_empty())
            .then(|| redact_assistant_reply(&result.content, options.keep_phone_numbers));
        let assistant = json!({"role": "assistant", "content": content, "tool_calls": calls});
        sent.push(assistant.clone());
        messages.push(assistant);
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
                Some(mcp) if name.starts_with("mcp__") && !through_bridge => {
                    let mcp = mcp.lock().await;
                    mcp.owns(&name).then_some(())
                }
                _ => None,
            };
            let output = if let Some(answer) = bridged {
                answer
            } else if mcp_tool.is_some() {
                match options.mcp {
                    Some(mcp) => {
                        mcp.lock()
                            .await
                            .call_with_id(&name, &arguments, &call.id)
                            .await
                    }
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
            let content = if options.stall_guards {
                stall_tracker.apply(&name, &arguments, content, &call.id, !succeeded)
            } else {
                content
            };
            let tool_message = json!({"role": "tool", "tool_call_id": call.id, "content": content});
            sent.push(tool_message.clone());
            messages.push(tool_message);
            on_event(TurnEvent::ToolFinished {
                name: &name,
                succeeded,
            });
        }
    }
    on_event(TurnEvent::IterationLimit(options.max_turns));
    messages.push(json!({"role": "user", "content": ITERATION_SUMMARY_REQUEST}));
    sent.push(json!({"role": "user", "content": ITERATION_SUMMARY_REQUEST}));
    let request = chat_request(&route.model, &sent, &tools, options.prefill.unwrap_or(&[]));
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

/// Replace the early part of `sent` with a summary, over the span the reference's compressor would
/// pick for the whole request (head, token-budget tail, anchors; see `compaction_window`), as it does
/// when the request nears the threshold or the provider refuses it. When the span reaches the running
/// turn's own request, the request is restated after the summary so that it is not lost.
/// `false` when nothing can be replaced or the summary is no shorter than what it replaces.
#[allow(clippy::too_many_arguments)]
async fn squeeze_turn(
    settings: &CompactionSettings,
    route: &ModelRoute,
    own_route: bool,
    sent: &mut Vec<Value>,
    tools: &[Value],
    query_index: usize,
    after_pass: bool,
    guard: &Mutex<CompactionGuard>,
) -> bool {
    // Before the turn's first step there is nothing of this turn to replace: the history belongs to the
    // preflight pass (and, after a refusal, to the retry that compresses the saved history).
    if sent.len() <= query_index + 1 {
        return false;
    }
    let Some(range) = settings.select_in_turn(sent, after_pass) else {
        return false;
    };
    let middle = &sent[range.clone()];
    let Some(request) = settings.request(&route.model, middle, None) else {
        guard.lock().unwrap().record_summary_failure(false);
        return false;
    };
    // A configured summary route carries its own body fields; the main route's are left out.
    let reply = if own_route {
        route.provider.complete(&request, |_| {}).await
    } else {
        route.provider.complete_plain(&request, |_| {}).await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            guard
                .lock()
                .unwrap()
                .record_summary_failure(is_timeout_provider_error(&error));
            return false;
        }
    };
    if reply.finish_reason.as_deref() != Some("stop") || !reply.tool_calls.is_empty() {
        let truncated = reply.finish_reason.as_deref() == Some("length");
        guard.lock().unwrap().record_summary_failure(truncated);
        return false;
    }
    let head = &sent[..range.start];
    let tail = &sent[range.end..];
    let placement = plan_summary_placement(head, tail, range.start);
    let persisted = if placement.merge_into_tail && placement.merge_target_idx() < tail.len() {
        PersistedSummaryPlacement::MergedIntoTail {
            tail_index: placement.merge_target_idx(),
            summary_role: placement.summary_role,
            force_user_leading: placement.force_user_leading,
        }
    } else {
        PersistedSummaryPlacement::Standalone {
            role: placement.summary_role,
        }
    };
    let Some(compaction) = CompactedHistory::new(range.clone(), &reply.content, persisted) else {
        guard.lock().unwrap().record_summary_failure(false);
        return false;
    };
    let mut text = compaction.summary;
    if range.contains(&query_index) {
        if let Some(query) = sent[query_index]["content"].as_str() {
            text.push_str("\n\n[ACTIVE REQUEST, VERBATIM]\n");
            text.push_str(query);
        }
    }
    let old_bytes = messages_bytes(sent);
    let mut candidate = sent[..range.start].to_vec();
    let mut tail_copy = sent[range.end..].to_vec();
    if placement.merge_into_tail && placement.merge_target_idx() < tail_copy.len() {
        merge_summary_into_tail_row(
            &mut tail_copy[placement.merge_target_idx()],
            &text,
            &placement,
        );
    } else {
        candidate.push(json!({"role": placement.summary_role, "content": text}));
    }
    candidate.extend(tail_copy);
    let new_bytes = messages_bytes(&candidate);
    if new_bytes >= old_bytes {
        guard.lock().unwrap().record_ineffective();
        return false;
    }
    *sent = candidate;
    let still_over = settings.over_threshold(new_bytes + messages_bytes(tools));
    guard.lock().unwrap().record_success(still_over);
    true
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
