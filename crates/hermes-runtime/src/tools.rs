//! The registry of built-in tools.
//!
//! One table names every built-in tool with the way it is offered and the way it runs, so that
//! availability, toolset gating, ordering and dispatch live in one place. A tool is *offered*
//! when its schema goes into the request, and *callable* when the profile's toolset
//! configuration allows it: a model that calls a hidden tool still gets that tool's own
//! refusal (for example that nobody can approve a write), not an "unknown tool" error.

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::clarify::{self, CLARIFY};
use crate::file_tools::{
    patch_schema, search_schema, write_schema, FileWorkspace, PATCH, SEARCH_FILES, WRITE_FILE,
};
use crate::memory_tool::{self, MemorySettings, MemoryTurn, MEMORY};
use crate::session_search::{self, Recall, SESSION_SEARCH};
use crate::skills::{SkillCatalog, SKILLS_LIST, SKILL_VIEW};
use crate::terminal::{self, TERMINAL};
use crate::todo_tool::{self, TodoStore, TODO_LIST};
use crate::tool_policy::ToolPolicy;
use crate::turn::{ApprovalDecision, ApprovalRequest, TurnOptions};
use crate::web_tools::{self, WebSettings, WEB_EXTRACT, WEB_SEARCH};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Asks the person at the terminal. The turn loop boxes its generic callback into this.
pub(crate) type Approver<'a> =
    dyn FnMut(ApprovalRequest) -> BoxFuture<'static, ApprovalDecision> + Send + 'a;

pub(crate) const READ_FILE: &str = "read_file";

/// What decides whether a tool is offered. Owned, so that a session can keep it for
/// estimating the size of the request prefix.
#[derive(Clone, Default)]
pub(crate) struct ToolEnv {
    pub write_files: bool,
    pub terminal: bool,
    pub has_profile: bool,
    pub web: Option<WebSettings>,
    pub memory: Option<MemorySettings>,
    pub policy: ToolPolicy,
    /// Whether the system prompt carries the skills index.
    pub skills_indexed: bool,
}

impl ToolEnv {
    pub(crate) fn from_options(options: &TurnOptions<'_>) -> Self {
        Self {
            write_files: options.can_write_files(),
            terminal: options.can_run_terminal(),
            has_profile: options.profile.is_some(),
            web: options.web.cloned(),
            memory: options.memory.cloned(),
            policy: options.tools.clone(),
            skills_indexed: false,
        }
    }
}

/// What a running tool may use.
pub(crate) struct ToolContext<'c, 'w> {
    pub options: &'c TurnOptions<'w>,
    pub skills: &'c SkillCatalog,
    pub files: &'c mut FileWorkspace<'w>,
    pub memory_turn: &'c mut MemoryTurn,
    pub todo: &'c mut TodoStore,
    pub approve: &'c mut Approver<'c>,
}

type Call = for<'a, 'b, 'w> fn(&'a mut ToolContext<'b, 'w>, &'a str) -> BoxFuture<'a, Value>;

pub(crate) struct ToolDef {
    pub name: &'static str,
    /// The schema when the tool is offered to the model.
    offered: fn(&ToolEnv) -> Option<Value>,
    call: Call,
}

fn read_file_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": READ_FILE,
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
    })
}

fn skill_schema(name: &str) -> Option<Value> {
    SkillCatalog::schemas()
        .into_iter()
        .find(|schema| schema["function"]["name"] == name)
}

fn read_file_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { cx.files.read(args) })
}

fn search_files_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { cx.files.search(args) })
}

fn edit_call<'a>(
    name: &'static str,
    cx: &'a mut ToolContext<'_, '_>,
    args: &'a str,
) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let timeout = cx.options.approval_timeout_secs;
        cx.files.edit(name, args, timeout, &mut cx.approve).await
    })
}

fn write_file_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    edit_call(WRITE_FILE, cx, args)
}

fn patch_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    edit_call(PATCH, cx, args)
}

fn terminal_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        if !cx.options.terminal_local {
            return json!({
                "error": "The configured terminal backend is unavailable in the Rust CLI.",
                "status": "backend_unavailable"
            });
        }
        let timeout = cx.options.approval_timeout_secs;
        terminal::execute(cx.files, args, timeout, &mut cx.approve).await
    })
}

fn skill_view_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { cx.skills.view(args) })
}

fn skills_list_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { cx.skills.list(args) })
}

fn clarify_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { ask_questions(args, cx.options, &mut cx.approve).await })
}

fn todo_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { todo_tool::call(cx.todo, args) })
}

fn session_search_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let Some(home) = cx.options.profile else {
            return json!({"error": format!("Unknown tool: {SESSION_SEARCH}")});
        };
        let recall = Recall {
            home,
            session_id: cx.options.session_id,
        };
        let args = args.to_owned();
        tokio::task::block_in_place(|| session_search::call(&recall, &args))
    })
}

fn memory_available(env: &ToolEnv) -> Option<&MemorySettings> {
    env.memory
        .as_ref()
        .filter(|settings| settings.offered() && env.has_profile)
}

fn memory_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let (Some(settings), Some(home)) = (cx.options.memory, cx.options.profile) else {
            return json!({"error": "Unknown tool: memory"});
        };
        if !settings.offered() {
            return json!({"error": "Unknown tool: memory"});
        }
        memory_tool::call(settings, cx.memory_turn, home, args)
    })
}

fn web_search_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.web.filter(|web| web.offers_search()) {
            Some(web) => web_tools::search(web, args).await,
            None => json!({"error": format!("Unknown tool: {WEB_SEARCH}")}),
        }
    })
}

fn web_extract_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.web.filter(|web| web.offers_extract()) {
            Some(web) => web_tools::extract(web, args).await,
            None => json!({"error": format!("Unknown tool: {WEB_EXTRACT}")}),
        }
    })
}

/// Put the questions of a `clarify` call to the person, when someone is there to answer.
async fn ask_questions(
    arguments: &str,
    options: &TurnOptions<'_>,
    approve: &mut &mut Approver<'_>,
) -> Value {
    use crate::turn::ApprovalAction;

    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let questions = match clarify::normalize(&args) {
        Ok(questions) => questions,
        Err(message) => return json!({"error": message}),
    };
    // Without a person at the terminal (one-shot runs, cron) nothing can answer.
    if options.approval_timeout_secs.is_none() {
        return json!({"error": clarify::UNAVAILABLE});
    }
    let limit = options.clarify_timeout_secs;
    let request = ApprovalRequest {
        target: String::new(),
        content: clarify::request_payload(&questions),
        action: ApprovalAction::Clarify,
        timeout_secs: limit,
    };
    let reply = match approve(request).await {
        ApprovalDecision::Answered(text) => {
            serde_json::from_str::<Value>(&text).unwrap_or_else(|_| {
                clarify::empty_reply("undelivered", "The answer could not be read.")
            })
        }
        ApprovalDecision::TimedOut => {
            let minutes = limit.div_ceil(60).max(1);
            clarify::empty_reply(
                "timed_out",
                &format!("[user did not respond within {minutes}m]"),
            )
        }
        ApprovalDecision::Denied => {
            clarify::empty_reply("cancelled", "The user cancelled the question.")
        }
        ApprovalDecision::Approved | ApprovalDecision::Unavailable => {
            return json!({"error": clarify::UNAVAILABLE});
        }
    };
    clarify::result(&questions, &reply)
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: CLARIFY,
        offered: |_| Some(clarify::schema()),
        call: clarify_call,
    },
    ToolDef {
        name: MEMORY,
        offered: |env| memory_available(env).map(|_| memory_tool::schema()),
        call: memory_call,
    },
    ToolDef {
        name: PATCH,
        offered: |env| env.write_files.then(patch_schema),
        call: patch_call,
    },
    ToolDef {
        name: READ_FILE,
        offered: |_| Some(read_file_schema()),
        call: read_file_call,
    },
    ToolDef {
        name: SEARCH_FILES,
        offered: |_| Some(search_schema()),
        call: search_files_call,
    },
    ToolDef {
        name: SESSION_SEARCH,
        offered: |env| {
            (env.has_profile && !env.policy.is_deferred(SESSION_SEARCH))
                .then(session_search::schema)
        },
        call: session_search_call,
    },
    ToolDef {
        name: SKILL_VIEW,
        offered: |env| {
            env.skills_indexed
                .then(|| skill_schema(SKILL_VIEW))
                .flatten()
        },
        call: skill_view_call,
    },
    ToolDef {
        name: SKILLS_LIST,
        offered: |env| {
            env.skills_indexed
                .then(|| skill_schema(SKILLS_LIST))
                .flatten()
        },
        call: skills_list_call,
    },
    ToolDef {
        name: TERMINAL,
        offered: |env| env.terminal.then(terminal::schema),
        call: terminal_call,
    },
    ToolDef {
        // Hidden behind the tool-search bridge unless the profile lists it directly.
        name: TODO_LIST,
        offered: |env| (!env.policy.is_deferred(TODO_LIST)).then(todo_tool::schema),
        call: todo_call,
    },
    ToolDef {
        name: WEB_EXTRACT,
        offered: |env| {
            env.web
                .as_ref()
                .is_some_and(WebSettings::offers_extract)
                .then(web_tools::extract_schema)
        },
        call: web_extract_call,
    },
    ToolDef {
        name: WEB_SEARCH,
        offered: |env| {
            env.web
                .as_ref()
                .is_some_and(WebSettings::offers_search)
                .then(web_tools::search_schema)
        },
        call: web_search_call,
    },
    ToolDef {
        name: WRITE_FILE,
        offered: |env| env.write_files.then(write_schema),
        call: write_file_call,
    },
];

/// The schemas of the offered tools that the profile's toolsets allow, in name order as the
/// reference lists them.
pub(crate) fn offered_schemas(env: &ToolEnv) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|tool| env.policy.allows(tool.name))
        .filter_map(|tool| (tool.offered)(env))
        .collect()
}

/// The built-in tool of that name, when the profile's toolsets allow it and it is not kept
/// behind the tool-search bridge (the bridge calls those itself).
pub(crate) fn find(name: &str, policy: &ToolPolicy) -> Option<&'static ToolDef> {
    TOOLS
        .iter()
        .find(|tool| tool.name == name && policy.allows(name) && !policy.is_deferred(name))
}

impl ToolDef {
    pub(crate) fn call<'a>(
        &self,
        cx: &'a mut ToolContext<'_, '_>,
        arguments: &'a str,
    ) -> BoxFuture<'a, Value> {
        (self.call)(cx, arguments)
    }
}

/// Order schemas by tool name, which is how the reference lists them.
pub(crate) fn sort_by_name(schemas: &mut [Value]) {
    schemas.sort_by(|a, b| {
        a["function"]["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["function"]["name"].as_str().unwrap_or(""))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_so_that_schemas_come_out_in_name_order() {
        let names: Vec<&str> = TOOLS.iter().map(|tool| tool.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "a tool is registered twice");
    }

    #[test]
    fn the_order_matches_the_reference_default_list() {
        let recorded: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/default_tools.json")).unwrap();
        let env = ToolEnv {
            write_files: true,
            terminal: true,
            has_profile: true,
            skills_indexed: true,
            memory: Some(MemorySettings::default()),
            web: Some(WebSettings {
                search: crate::web_tools::Backend::Tavily,
                extract: crate::web_tools::Backend::Tavily,
                ..WebSettings::default()
            }),
            // The default configuration hides the todo tool, as the reference does.
            policy: ToolPolicy::from_config(&serde_yaml::Value::Null),
        };
        let ours: Vec<String> = offered_schemas(&env)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        let reference: Vec<String> = recorded
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .filter(|name| ours.contains(name))
            .collect();
        assert_eq!(ours, reference);
        assert_eq!(ours.len(), 11);
    }

    #[test]
    fn offering_follows_the_environment_and_the_policy() {
        let mut env = ToolEnv {
            memory: Some(MemorySettings::default()),
            has_profile: true,
            ..ToolEnv::default()
        };
        let names = |env: &ToolEnv| -> Vec<String> {
            offered_schemas(env)
                .iter()
                .map(|s| s["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            names(&env),
            [
                "clarify",
                "memory",
                "read_file",
                "search_files",
                "session_search",
                "todo_list"
            ]
        );
        env.write_files = true;
        env.terminal = true;
        env.skills_indexed = true;
        env.web = Some(WebSettings {
            search: crate::web_tools::Backend::Searxng,
            ..WebSettings::default()
        });
        assert_eq!(
            names(&env),
            [
                "clarify",
                "memory",
                "patch",
                "read_file",
                "search_files",
                "session_search",
                "skill_view",
                "skills_list",
                "terminal",
                "todo_list",
                "web_search",
                "write_file"
            ]
        );
        let config: serde_yaml::Value =
            serde_yaml::from_str("platform_toolsets: {cli: [file]}").unwrap();
        env.policy = ToolPolicy::from_config(&config);
        assert_eq!(
            names(&env),
            ["patch", "read_file", "search_files", "write_file"]
        );
        assert!(find("terminal", &env.policy).is_none());
        assert!(find("read_file", &env.policy).is_some());
    }
}
