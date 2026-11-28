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

use crate::browser_cdp::{self, BrowserSettings, BROWSER_CDP};
use crate::browser_dialog::{self, BROWSER_DIALOG};
use crate::browser_exec::{self, BROWSER_EXEC};
use crate::browser_vault::{self, BROWSER_VAULT};
use crate::clarify::{self, CLARIFY};
use crate::cronjob_tool::{self, CRONJOB_MANAGE};
use crate::delegation::{self, DELEGATE_TASK};
use crate::execute_code::{self, EXECUTE_CODE};
use crate::file_tools::{
    patch_schema, search_schema, write_schema, FileWorkspace, PATCH, SEARCH_FILES, WRITE_FILE,
};
use crate::image_gen::{self, IMAGE_GENERATE};
use crate::memory_tool::{self, MemorySettings, MemoryTurn, MEMORY};
use crate::process::{self, PROCESS_MANAGE};
use crate::session_search::{self, Recall, SESSION_SEARCH};
use crate::skill_manage::{self, Skills, SKILL_MANAGE};
use crate::skills::{SkillCatalog, SKILLS_LIST, SKILL_VIEW};
use crate::terminal::{self, TERMINAL};
use crate::todo_tool::{self, TodoStore, TODO_LIST};
use crate::tool_policy::ToolPolicy;
use crate::tts_tool::{self, TEXT_TO_SPEECH};
use crate::turn::{ApprovalDecision, ApprovalRequest, TurnOptions};
use crate::vision_tool::{self, VISION_ANALYZE};
use crate::web_tools::{self, WebSettings, WEB_EXTRACT, WEB_SEARCH};
use crate::x_search::{self, X_SEARCH};

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
    pub code_execution: bool,
    /// Nobody can approve a run of code (`execute_code` asks every time), so it is not offered.
    pub no_code_approval: bool,
    /// The most subagents one call may start, when this turn may delegate.
    pub delegation: Option<usize>,
    pub has_profile: bool,
    pub web: Option<WebSettings>,
    pub browser: Option<BrowserSettings>,
    pub memory: Option<MemorySettings>,
    pub vision_available: bool,
    pub image_gen_available: bool,
    pub tts_available: bool,
    pub x_search_available: bool,
    pub policy: ToolPolicy,
    /// Whether the system prompt carries the skills index.
    pub skills_indexed: bool,
    /// The skill index's description width (`skills.index_desc_limit`), which the
    /// `skill_manage` description quotes; 0 means the default of 60.
    pub skill_desc_limit: usize,
}

impl ToolEnv {
    pub(crate) fn from_options(options: &TurnOptions<'_>) -> Self {
        Self {
            write_files: options.can_write_files(),
            terminal: options.can_run_terminal(),
            code_execution: options.can_execute_code(),
            no_code_approval: options.approval_timeout_secs.is_none(),
            delegation: options
                .delegation
                .filter(|d| options.delegate_depth < d.settings.max_spawn_depth)
                .map(|delegation| delegation.settings.max_concurrent_children),
            has_profile: options.profile.is_some(),
            web: options.web.cloned(),
            browser: options.browser.cloned(),
            memory: options.memory.cloned(),
            vision_available: options.vision_route.is_some()
                || options.vision.is_some_and(|v| v.native),
            image_gen_available: options.image_gen.is_some_and(|g| g.is_available()),
            tts_available: options.tts.is_some_and(|t| t.is_available()),
            x_search_available: options.x_search.is_some_and(|x| x.is_available()),
            policy: options.tools.clone(),
            skills_indexed: false,
            skill_desc_limit: 0,
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
    /// The model this turn is talking to, which a subagent talks to as well.
    pub route: &'c crate::turn::ModelRoute,
    pub fallbacks: &'c [crate::turn::ModelRoute],
    /// Optional channel for streaming live tool output previews (`terminal`, `execute_code`).
    pub tool_output: Option<tokio::sync::mpsc::UnboundedSender<String>>,
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
            "description": "Read a text file or extractable document within the current working directory with line numbers. Auto-extracts .ipynb, Office (.docx/.xlsx/.pptx), PDF, and SQLite (.db/.sqlite/.sqlite3) into readable paginated text, and loads images (PNG/JPEG/GIF/WebP) for vision inspection.",
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
        let policy = cx.options.approval_policy;
        cx.files
            .edit(name, args, timeout, policy, &mut cx.approve)
            .await
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
        let timeout = cx.options.terminal_approval_timeout_secs;
        let background = (cfg!(any(target_os = "linux", target_os = "macos"))
            && cx.options.tools.allows(PROCESS_MANAGE)
            && !cx.options.tools.is_deferred(PROCESS_MANAGE))
        .then(|| cx.options.state_key());
        let defaults = terminal::TerminalSettings::default();
        let settings = cx.options.terminal_env.unwrap_or(&defaults);
        let session = cx.options.state_key();
        terminal::execute(
            cx.files,
            args,
            timeout,
            background,
            settings,
            session,
            cx.options.approval_policy,
            cx.tool_output.clone(),
            &mut cx.approve,
        )
        .await
    })
}

/// `process_manage` manages what `terminal(background=true)` starts, so it is offered with the
/// Unix terminal unless the tool-search bridge hides it.
fn process_manage_offered(env: &ToolEnv) -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
        && env.terminal
        && env.policy.allows(PROCESS_MANAGE)
        && !env.policy.is_deferred(PROCESS_MANAGE)
}

fn process_manage_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let caller = cx.options.state_key();
        process::call(caller, args).await
    })
}

fn skill_manage_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let Some(home) = cx.options.profile else {
            return json!({"error": format!("Unknown tool: {SKILL_MANAGE}")});
        };
        let skills = Skills {
            home,
            settings: cx.skills.settings(),
        };
        let timeout = cx.options.approval_timeout_secs;
        skill_manage::call(
            &skills,
            args,
            timeout,
            cx.options.approval_policy,
            &mut cx.approve,
        )
        .await
    })
}

fn skill_view_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { cx.skills.view(args, cx.options.profile) })
}

fn skills_list_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { cx.skills.list(args) })
}

fn clarify_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move { ask_questions(args, cx.options, &mut cx.approve).await })
}

fn cronjob_manage_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let Some(home) = cx.options.profile else {
            return json!({"error": format!("Unknown tool: {CRONJOB_MANAGE}")});
        };
        cronjob_tool::call(home, args)
    })
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

fn browser_cdp_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.browser.filter(|browser| browser.offers_cdp()) {
            Some(browser) => {
                let task = cx.options.state_key();
                browser_cdp::call(browser, task, args).await
            }
            None => json!({"error": format!("Unknown tool: {BROWSER_CDP}")}),
        }
    })
}

fn browser_dialog_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.browser.filter(|browser| browser.offers_cdp()) {
            Some(_) => browser_dialog::call(cx.options.state_key(), args).await,
            None => json!({"error": format!("Unknown tool: {BROWSER_DIALOG}")}),
        }
    })
}

fn browser_exec_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.browser.filter(|browser| browser.offers_cdp()) {
            Some(browser) => {
                let task = cx.options.state_key();
                let vision_native = cx.options.vision.is_some_and(|v| v.native);
                browser_exec::call(browser, task, vision_native, args).await
            }
            None => json!({"error": format!("Unknown tool: {BROWSER_EXEC}")}),
        }
    })
}

fn browser_vault_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        if cx.options.browser.filter(|b| b.offers_cdp()).is_none() {
            return json!({"error": format!("Unknown tool: {BROWSER_VAULT}")});
        }
        let task = cx.options.state_key().to_owned();
        let browser = cx.options.browser;
        let profile = cx.options.profile;
        let approval_timeout = cx.options.approval_timeout_secs;
        browser_vault::call(browser, &task, profile, approval_timeout, cx.approve, args).await
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

fn vision_analyze_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let default_settings = vision_tool::VisionSettings::default();
        let settings = cx.options.vision.unwrap_or(&default_settings);
        vision_tool::execute(
            args,
            cx.options.root,
            cx.options.profile,
            settings,
            cx.options.vision_route,
        )
        .await
    })
}

fn image_generate_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.image_gen.filter(|g| g.is_available()) {
            Some(settings) => {
                image_gen::execute(args, settings, cx.options.root, cx.options.profile).await
            }
            None => json!({"error": format!("Unknown tool: {IMAGE_GENERATE}")}),
        }
    })
}

fn text_to_speech_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.tts.filter(|t| t.is_available()) {
            Some(settings) => {
                tts_tool::execute(args, settings, cx.options.root, cx.options.profile).await
            }
            None => json!({"error": format!("Unknown tool: {TEXT_TO_SPEECH}")}),
        }
    })
}

fn x_search_call<'a>(cx: &'a mut ToolContext<'_, '_>, args: &'a str) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        match cx.options.x_search.filter(|x| x.is_available()) {
            Some(settings) => x_search::execute(args, settings).await,
            None => json!({"error": format!("Unknown tool: {X_SEARCH}")}),
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
        name: BROWSER_CDP,
        offered: |env| {
            env.browser
                .as_ref()
                .is_some_and(BrowserSettings::offers_cdp)
                .then(browser_cdp::schema)
        },
        call: browser_cdp_call,
    },
    ToolDef {
        name: BROWSER_DIALOG,
        offered: |env| {
            env.browser
                .as_ref()
                .is_some_and(BrowserSettings::offers_cdp)
                .then(browser_dialog::schema)
        },
        call: browser_dialog_call,
    },
    ToolDef {
        name: BROWSER_EXEC,
        offered: |env| {
            env.browser
                .as_ref()
                .is_some_and(BrowserSettings::offers_cdp)
                .then(browser_exec::schema)
        },
        call: browser_exec_call,
    },
    ToolDef {
        name: BROWSER_VAULT,
        offered: |env| {
            env.browser
                .as_ref()
                .is_some_and(BrowserSettings::offers_cdp)
                .then(browser_vault::schema)
        },
        call: browser_vault_call,
    },
    ToolDef {
        name: CLARIFY,
        offered: |_| Some(clarify::schema()),
        call: clarify_call,
    },
    ToolDef {
        name: CRONJOB_MANAGE,
        offered: |env| {
            (env.has_profile && !env.policy.is_deferred(CRONJOB_MANAGE)).then(cronjob_tool::schema)
        },
        call: cronjob_manage_call,
    },
    ToolDef {
        name: DELEGATE_TASK,
        offered: |env| env.delegation.map(delegation::schema),
        call: delegation::call,
    },
    ToolDef {
        name: EXECUTE_CODE,
        offered: |env| {
            ((env.terminal || env.code_execution) && !env.no_code_approval)
                .then(|| execute_code::schema(&execute_code::sandbox_tools(env)))
        },
        call: execute_code::call,
    },
    ToolDef {
        name: IMAGE_GENERATE,
        offered: |env| env.image_gen_available.then(image_gen::schema),
        call: image_generate_call,
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
        name: PROCESS_MANAGE,
        offered: |env| process_manage_offered(env).then(process::schema),
        call: process_manage_call,
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
        name: SKILL_MANAGE,
        offered: |env| {
            (env.has_profile && env.write_files).then(|| {
                skill_manage::schema(match env.skill_desc_limit {
                    0 => 60,
                    limit => limit,
                })
            })
        },
        call: skill_manage_call,
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
        offered: |env| {
            env.terminal
                .then(|| terminal::schema(process_manage_offered(env)))
        },
        call: terminal_call,
    },
    ToolDef {
        name: TEXT_TO_SPEECH,
        offered: |env| env.tts_available.then(tts_tool::schema),
        call: text_to_speech_call,
    },
    ToolDef {
        // Hidden behind the tool-search bridge unless the profile lists it directly.
        name: TODO_LIST,
        offered: |env| (!env.policy.is_deferred(TODO_LIST)).then(todo_tool::schema),
        call: todo_call,
    },
    ToolDef {
        name: VISION_ANALYZE,
        offered: |env| env.vision_available.then(vision_tool::schema),
        call: vision_analyze_call,
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
    ToolDef {
        name: X_SEARCH,
        offered: |env| env.x_search_available.then(x_search::schema),
        call: x_search_call,
    },
];

/// The schemas of the offered tools that the profile's toolsets allow, in name order as the
/// reference lists them.
pub(crate) fn offered_schemas(env: &ToolEnv) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|tool| env.policy.allows(tool.name) && !env.policy.is_deferred(tool.name))
        .filter_map(|tool| (tool.offered)(env))
        .collect()
}

/// The schemas of the tools the profile defers behind the bridge, as they would be listed if
/// they were not deferred.
pub(crate) fn deferred_schemas(env: &ToolEnv) -> Vec<Value> {
    let open = ToolEnv {
        policy: env.policy.without_deferral(),
        ..env.clone()
    };
    TOOLS
        .iter()
        .filter(|tool| env.policy.allows(tool.name) && env.policy.is_deferred(tool.name))
        .filter_map(|tool| (tool.offered)(&open))
        .collect()
}

/// Add the bridge tools, last, as the reference lists them.
pub(crate) fn append_bridge(env: &ToolEnv, tools: &mut Vec<Value>) {
    tools.extend(crate::tool_search::bridge_for(env));
}

/// A tool the bridge may invoke: allowed, and one the profile defers.
pub(crate) fn find_deferred(name: &str, policy: &ToolPolicy) -> Option<&'static ToolDef> {
    TOOLS
        .iter()
        .find(|tool| tool.name == name && policy.allows(name) && policy.is_deferred(name))
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
            code_execution: true,
            no_code_approval: false,
            delegation: Some(10),
            has_profile: true,
            skills_indexed: true,
            skill_desc_limit: 0,
            memory: Some(MemorySettings::default()),
            browser: None,
            vision_available: false,
            image_gen_available: false,
            tts_available: false,
            x_search_available: false,
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
        assert_eq!(ours.len(), 14);
        // These tools carry the reference's schema verbatim; the rest are still hand-written.
        let verbatim = [
            "clarify",
            "delegate_task",
            "execute_code",
            "memory",
            "patch",
            "search_files",
            "skill_manage",
            "skill_view",
            "skills_list",
            "web_extract",
            "web_search",
            "write_file",
        ];
        for ours in offered_schemas(&env) {
            let name = ours["function"]["name"].as_str().unwrap().to_owned();
            let name = name.as_str();
            if verbatim.contains(&name) {
                // Outside Unix the execute_code description ends with the RustPython/Excel helper
                // paragraph, which the reference (recorded on Linux) does not have.
                let mut ours = ours;
                if cfg!(not(unix)) && name == "execute_code" {
                    let recorded_text = recorded
                        .iter()
                        .find(|tool| tool["function"]["name"] == "execute_code")
                        .and_then(|tool| tool["function"]["description"].as_str())
                        .unwrap();
                    let text = ours["function"]["description"].as_str().unwrap();
                    assert!(text.starts_with(recorded_text), "{name} differs");
                    ours["function"]["description"] = json!(recorded_text);
                }
                assert!(recorded.iter().any(|tool| tool == &ours), "{name} differs");
            }
        }
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
                "cronjob_manage",
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
                "cronjob_manage",
                "execute_code",
                "memory",
                "patch",
            ]
            .into_iter()
            // process_manage is Unix-only (see `process_stub`).
            .chain(cfg!(any(target_os = "linux", target_os = "macos")).then_some("process_manage"))
            .chain([
                "read_file",
                "search_files",
                "session_search",
                "skill_manage",
                "skill_view",
                "skills_list",
                "terminal",
                "todo_list",
                "web_search",
                "write_file"
            ])
            .collect::<Vec<_>>()
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

    // process_manage and the terminal's background options exist only where the terminal is supported.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn background_commands_need_process_manage_to_be_offered() {
        let terminal_schema = |env: &ToolEnv| -> Value {
            offered_schemas(env)
                .into_iter()
                .find(|schema| schema["function"]["name"] == TERMINAL)
                .expect("terminal is offered")
        };
        let mut env = ToolEnv {
            terminal: true,
            ..ToolEnv::default()
        };
        let schema = terminal_schema(&env);
        assert!(schema["function"]["parameters"]["properties"]
            .get("background")
            .is_some());
        // The default configuration hides process_manage behind the tool-search bridge.
        env.policy = ToolPolicy::from_config(&serde_yaml::Value::Null);
        let schema = terminal_schema(&env);
        assert!(schema["function"]["parameters"]["properties"]
            .get("background")
            .is_none());
        let names: Vec<String> = offered_schemas(&env)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(!names.contains(&PROCESS_MANAGE.to_owned()));
    }

    #[test]
    fn the_browser_tool_needs_a_configured_endpoint_and_an_enabled_toolset() {
        let names = |env: &ToolEnv| -> Vec<String> {
            offered_schemas(env)
                .iter()
                .map(|s| s["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let mut env = ToolEnv {
            policy: ToolPolicy::from_config(&serde_yaml::Value::Null),
            ..ToolEnv::default()
        };
        assert!(!names(&env).contains(&BROWSER_CDP.to_owned()));
        env.browser = Some(BrowserSettings::default());
        assert!(!names(&env).contains(&BROWSER_CDP.to_owned()));
        env.browser = Some(BrowserSettings {
            cdp_url: Some("ws://127.0.0.1:9222".into()),
            allow_private_urls: false,
        });
        assert!(names(&env).contains(&BROWSER_CDP.to_owned()));
        assert!(names(&env).contains(&BROWSER_DIALOG.to_owned()));
        assert!(names(&env).contains(&BROWSER_EXEC.to_owned()));
        assert!(!names(&env).contains(&BROWSER_VAULT.to_owned()));
        assert!(deferred_schemas(&env)
            .iter()
            .any(|s| s["function"]["name"] == BROWSER_VAULT));
        let config: serde_yaml::Value =
            serde_yaml::from_str("platform_toolsets: {cli: [file]}").unwrap();
        env.policy = ToolPolicy::from_config(&config);
        assert!(!names(&env).contains(&BROWSER_CDP.to_owned()));
        assert!(!names(&env).contains(&BROWSER_DIALOG.to_owned()));
        assert!(!names(&env).contains(&BROWSER_EXEC.to_owned()));
    }
}
