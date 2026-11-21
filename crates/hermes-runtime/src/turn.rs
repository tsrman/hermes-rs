//! One in-memory conversation turn with local tools.

use std::future::Future;
use std::path::Path;

use hermes_core::prompt::{cli_system_prompt, update_cli_prompt_route};
use serde_json::{json, Value};

use crate::compaction::{messages_bytes, CompactedHistory, CompactionSettings};
use crate::file_tools::{
    patch_schema, search_schema, write_schema, FileWorkspace, PATCH, SEARCH_FILES, WRITE_FILE,
};
use crate::prompt::{load_cli_context, PromptSettings};
use crate::providers::openai::{OpenAiProvider, ProviderError};
use crate::skills::{SkillCatalog, SKILLS_LIST, SKILL_VIEW};
use crate::terminal::{self, TERMINAL};

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

#[derive(Default)]
pub struct TurnHistory {
    pub(crate) system_prompt: Option<String>,
    pub(crate) messages: Vec<Value>,
    pub(crate) skills: SkillCatalog,
    pub(crate) compaction: Option<CompactedHistory>,
    pub(crate) write_files: bool,
    pub(crate) terminal_enabled: bool,
}

pub enum CompactionOutcome {
    Unchanged,
    Skipped,
    Applied,
}

impl TurnHistory {
    pub fn capture_prompt(
        &mut self,
        options: &TurnOptions<'_>,
        settings: &PromptSettings,
        route: &ModelRoute,
    ) {
        self.write_files = options.can_write_files();
        self.terminal_enabled = options.can_run_terminal();
        self.skills = SkillCatalog::load(
            options.profile,
            options.root,
            &settings.skills,
            self.write_files,
            self.terminal_enabled,
        );
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
        query: &str,
    ) -> CompactionOutcome {
        if !settings.enabled() || self.messages.is_empty() {
            return CompactionOutcome::Unchanged;
        }
        let system = route.system_prompt(self.system_prompt.as_deref());
        let write_files = self.write_files;
        let terminal_enabled = self.terminal_enabled;
        let old_prefix = prefix_bytes(&system, write_files, terminal_enabled);
        let input_bytes = old_prefix + messages_bytes(&self.messages) + query.len();
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
        let refreshed = tokio::task::spawn_blocking(move || {
            let skills = SkillCatalog::load(
                profile.as_deref(),
                &root,
                &settings.skills,
                write_files,
                terminal_enabled,
            );
            let prompt = load_cli_context(profile.as_deref(), &root, &settings, skills.prompt());
            (prompt.render(&model, provider), skills)
        })
        .await;
        let Ok((system, skills)) = refreshed else {
            return CompactionOutcome::Skipped;
        };
        // A newly loaded memory or project file can outweigh the savings.
        if prefix_bytes(&system, write_files, terminal_enabled) + summary_bytes
            >= old_prefix + removed_bytes
        {
            return CompactionOutcome::Skipped;
        }
        self.system_prompt = Some(system);
        self.skills = skills;
        self.compaction = Some(compaction);
        CompactionOutcome::Applied
    }
}

pub struct ModelRoute {
    pub provider: OpenAiProvider,
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

pub struct TurnOptions<'a> {
    pub root: &'a Path,
    pub profile: Option<&'a Path>,
    pub max_turns: u32,
    pub approval_timeout_secs: Option<u32>,
    pub terminal_local: bool,
}

impl TurnOptions<'_> {
    fn can_write_files(&self) -> bool {
        self.approval_timeout_secs.is_some() && cfg!(target_os = "linux")
    }

    fn can_run_terminal(&self) -> bool {
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
    A: FnMut(ApprovalRequest) -> D,
    D: Future<Output = ApprovalDecision>,
{
    let mut files = FileWorkspace::new(options.root, options.profile);
    let saved_prompt = history.system_prompt.as_deref();
    let mut system_prompt = primary.system_prompt(saved_prompt);
    let (mut messages, original) = match &history.compaction {
        Some(compaction) => (compaction.replay(&history.messages), Some(history.messages)),
        None => (history.messages, None),
    };
    messages.insert(0, json!({"role": "system", "content": system_prompt}));
    let first_new_message = messages.len();
    messages.push(json!({"role": "user", "content": query}));
    let skill_tools = system_prompt.contains("\n<available_skills>\n");
    let tools = tools_for_prompt(
        &system_prompt,
        options.can_write_files(),
        options.can_run_terminal(),
    );
    let mut route = primary;
    let mut fallback_routes = fallbacks.iter();
    for _ in 0..options.max_turns {
        let result = loop {
            let request = json!({"model": route.model, "messages": messages, "tools": tools});
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
            let output = match call.name.as_str() {
                "read_file" => files.read(&call.arguments),
                SEARCH_FILES => files.search(&call.arguments),
                WRITE_FILE | PATCH => {
                    files
                        .edit(
                            &call.name,
                            &call.arguments,
                            options.approval_timeout_secs,
                            &mut approve,
                        )
                        .await
                }
                TERMINAL if options.terminal_local => {
                    terminal::execute(
                        &files,
                        &call.arguments,
                        options.approval_timeout_secs,
                        &mut approve,
                    )
                    .await
                }
                TERMINAL => json!({
                    "error": "The configured terminal backend is unavailable in the Rust CLI.",
                    "status": "backend_unavailable"
                }),
                SKILL_VIEW if skill_tools => history.skills.view(&call.arguments),
                SKILLS_LIST if skill_tools => history.skills.list(&call.arguments),
                _ => json!({"error": format!("Unknown tool: {}", call.name)}),
            };
            let succeeded = output.get("error").is_none()
                && (call.name != TERMINAL || output["exit_code"].as_i64() == Some(0));
            messages.push(
                json!({"role": "tool", "tool_call_id": call.id, "content": output.to_string()}),
            );
            on_event(TurnEvent::ToolFinished {
                name: &call.name,
                succeeded,
            });
        }
    }
    on_event(TurnEvent::IterationLimit(options.max_turns));
    messages.push(json!({"role": "user", "content": ITERATION_SUMMARY_REQUEST}));
    let request = json!({"model": route.model, "messages": messages, "tools": tools});
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

fn prefix_bytes(system: &str, write_files: bool, terminal_enabled: bool) -> usize {
    system.len() + messages_bytes(&tools_for_prompt(system, write_files, terminal_enabled))
}

fn tools_for_prompt(system: &str, write_files: bool, terminal_enabled: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a UTF-8 file within the current working directory with line numbers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer", "minimum": 1, "default": 1},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 2000}
                    },
                    "required": ["path"]
                }
            }
        }),
        search_schema(),
    ];
    if system.contains("\n<available_skills>\n") {
        tools.extend(SkillCatalog::schemas());
    }
    if write_files {
        tools.extend([write_schema(), patch_schema()]);
    }
    if terminal_enabled {
        tools.push(terminal::schema());
    }
    tools
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
