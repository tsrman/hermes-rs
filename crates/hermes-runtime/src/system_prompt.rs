//! The CLI system prompt, assembled like the reference's three tiers.
//!
//! `stable` (identity, guidance, coding brief, platform hint), `context` (project context files,
//! the workspace snapshot) and `volatile` (skills index, memory, profile line, date and route
//! lines, runtime environment) are joined with blank lines. The paragraphs are recorded from the
//! reference (`compat/create_prompt_fixture.py`, `prompt_text.json`); this module holds the
//! conditions that decide which ones appear.
//!
//! Not ported: the mid-turn steering note (there is no steering channel), the Python toolchain
//! probe line, Bot Mode and Bot Screen blocks, remote-backend environment text and plugin
//! sections.

use crate::paths::PathExt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use serde_yaml::Value;

#[derive(Deserialize)]
struct EditFormat {
    needles: Vec<String>,
    line: String,
}

#[derive(Deserialize)]
struct Text {
    identity: String,
    help_guidance: String,
    help_guidance_no_skills: String,
    task_completion: String,
    parallel_tool_calls: String,
    session_search_guidance: String,
    skills_guidance: String,
    tool_use_enforcement: String,
    google_operational: String,
    execution_guidance: String,
    execution_line_tools: BTreeMap<String, Vec<String>>,
    enforcement_models: Vec<String>,
    execution_models: Vec<String>,
    platform_hint_cli: String,
    wsl_hint: String,
    coding_guidance: String,
    todo_sentence: String,
    no_todo_sentence: String,
    edit_format: BTreeMap<String, EditFormat>,
    workspace_header: String,
    runtime_heading: String,
    runtime_end: String,
    project_markers: Vec<String>,
    context_files: Vec<String>,
    code_extensions: Vec<String>,
    scan_skip_dirs: Vec<String>,
    py_lockfiles: Vec<[String; 2]>,
    js_lockfiles: Vec<[String; 2]>,
    verify_targets: Vec<String>,
}

fn text() -> &'static Text {
    static TEXT: OnceLock<Text> = OnceLock::new();
    TEXT.get_or_init(|| {
        serde_json::from_str(include_str!("prompt_text.json")).expect("prompt_text.json is valid")
    })
}

/// A config gate: on, off, or decided by the model name.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Gate {
    On,
    Off,
    #[default]
    Auto,
    Models(Vec<String>),
}

impl Gate {
    /// `agent.tool_use_enforcement` and `agent.execution_guidance`: booleans and boolean words
    /// force the gate, a list names model substrings, anything else means the default models.
    pub(crate) fn from_config(value: &Value) -> Self {
        match value {
            Value::Bool(true) => Self::On,
            Value::Bool(false) => Self::Off,
            Value::String(word) => match word.to_lowercase().as_str() {
                "true" | "always" | "yes" | "on" => Self::On,
                "false" | "never" | "no" | "off" => Self::Off,
                _ => Self::Auto,
            },
            Value::Sequence(items) => Self::Models(
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_lowercase))
                    .collect(),
            ),
            _ => Self::Auto,
        }
    }

    fn allows(&self, model: &str, defaults: &[String]) -> bool {
        let model = model.to_lowercase();
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => defaults.iter().any(|name| model.contains(name.as_str())),
            Self::Models(names) => names.iter().any(|name| model.contains(name.as_str())),
        }
    }
}

/// `agent.coding_context`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CodingMode {
    #[default]
    Auto,
    On,
    Off,
}

impl CodingMode {
    pub(crate) fn from_config(value: &Value) -> Self {
        let word = match value {
            Value::String(word) => word.trim().to_lowercase(),
            Value::Bool(flag) => if *flag { "true" } else { "false" }.to_owned(),
            Value::Number(number) => number.to_string(),
            _ => String::new(),
        };
        match word.as_str() {
            "on" | "true" | "yes" | "1" | "always" => Self::On,
            "off" | "false" | "no" | "0" | "never" => Self::Off,
            _ => Self::Auto,
        }
    }
}

/// Python's `bool(value)` for a config value, with `default` when the key is missing.
pub(crate) fn python_bool(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Sequence(items) => !items.is_empty(),
        Value::Mapping(map) => !map.is_empty(),
        Value::Tagged(_) => true,
    }
}

/// What `agent.*`, `platform_hints` and `timezone` change in the prompt.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptGates {
    pub task_completion: bool,
    pub parallel_tool_calls: bool,
    pub enforcement: Gate,
    pub execution: Gate,
    pub coding: CodingMode,
    pub coding_instructions: String,
    pub environment_hint: String,
    /// `platform_hints.cli`: replace and append text.
    pub hint_replace: String,
    pub hint_append: String,
    pub timezone: String,
}

impl Default for PromptGates {
    fn default() -> Self {
        Self {
            task_completion: true,
            parallel_tool_calls: true,
            enforcement: Gate::Auto,
            execution: Gate::Auto,
            coding: CodingMode::Auto,
            coding_instructions: String::new(),
            environment_hint: String::new(),
            hint_replace: String::new(),
            hint_append: String::new(),
            timezone: String::new(),
        }
    }
}

impl PromptGates {
    pub(crate) fn from_config(config: &Value) -> Self {
        let agent = &config["agent"];
        let flag = |key: &str| match agent.get(key) {
            None => true,
            Some(value) => python_bool(value),
        };
        let instructions = match &agent["coding_instructions"] {
            Value::String(one) => one.trim().to_owned(),
            Value::Sequence(items) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let spec = &config["platform_hints"]["cli"];
        let clean = |value: &Value| value.as_str().map(str::trim).unwrap_or("").to_owned();
        let (replace, append) = match spec {
            Value::String(text) => (String::new(), text.trim().to_owned()),
            Value::Mapping(_) => (clean(&spec["replace"]), clean(&spec["append"])),
            _ => (String::new(), String::new()),
        };
        Self {
            task_completion: flag("task_completion_guidance"),
            parallel_tool_calls: flag("parallel_tool_call_guidance"),
            enforcement: agent
                .get("tool_use_enforcement")
                .map_or(Gate::Auto, Gate::from_config),
            execution: agent
                .get("execution_guidance")
                .map_or(Gate::Auto, Gate::from_config),
            coding: CodingMode::from_config(&agent["coding_context"]),
            coding_instructions: instructions,
            environment_hint: agent["environment_hint"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_owned(),
            hint_replace: replace,
            hint_append: append,
            timezone: config["timezone"].as_str().unwrap_or("").trim().to_owned(),
        }
    }
}

/// Everything the assembly needs besides the gates.
pub(crate) struct Inputs<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub tools: &'a BTreeSet<String>,
    pub gates: &'a PromptGates,
    pub profile: Option<&'a Path>,
    pub root: &'a Path,
    /// SOUL.md text, or `None` for the default identity.
    pub soul: Option<String>,
    pub memory_guidance: Option<String>,
    pub project_context: Option<String>,
    pub skills_prompt: Option<String>,
    /// `skills.auto_load` blocks: per-agent constants, so they sit in the stable tier.
    pub pinned_skills: Option<String>,
    pub memory_blocks: Vec<String>,
}

pub(crate) fn build(inputs: &Inputs<'_>) -> String {
    let t = text();
    let has = |name: &str| inputs.tools.contains(name);
    let model = inputs.model;
    let has_tools = !inputs.tools.is_empty();
    let skills = inputs.skills_prompt.as_deref().unwrap_or("");

    // Stable tier.
    let mut stable: Vec<String> = vec![inputs.soul.clone().unwrap_or_else(|| t.identity.clone())];
    stable.push(
        if has("skill_view") && skills.contains("- hermes-agent:") {
            &t.help_guidance
        } else {
            &t.help_guidance_no_skills
        }
        .clone(),
    );
    if has_tools {
        if inputs.gates.task_completion {
            stable.push(t.task_completion.clone());
        }
        if inputs.gates.parallel_tool_calls {
            stable.push(t.parallel_tool_calls.clone());
        }
    }
    let tool_guidance = [
        inputs.memory_guidance.clone().filter(|_| has("memory")),
        has("session_search").then(|| t.session_search_guidance.clone()),
        has("skill_manage").then(|| t.skills_guidance.clone()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    stable.push(tool_guidance);
    if has_tools {
        if inputs
            .gates
            .enforcement
            .allows(model, &t.enforcement_models)
        {
            stable.push(t.tool_use_enforcement.clone());
            let lowered = model.to_lowercase();
            if ["gemini", "gemma"]
                .iter()
                .any(|name| lowered.contains(name))
            {
                stable.push(t.google_operational.clone());
            }
        }
        if inputs.gates.execution.allows(model, &t.execution_models) {
            stable.push(execution_text(inputs.tools));
        }
    }

    stable.extend(inputs.pinned_skills.clone());

    // Coding posture.
    let coding = coding_workspace(inputs.root, inputs.gates.coding, has_tools);
    let mut context: Vec<String> = Vec::new();
    let mut brief = Vec::new();
    let mut workspace = Vec::new();
    if let Some(snapshot) = &coding {
        brief.push(coding_brief(model, has("todo_list")));
        workspace.extend(snapshot.clone());
    }
    stable.extend(brief);
    let trailing: Vec<String> = if coding.is_some() && !inputs.gates.coding_instructions.is_empty()
    {
        vec![format!(
            "Operator instructions (from config):\n{}",
            inputs.gates.coding_instructions
        )]
    } else {
        Vec::new()
    };
    let post_workspace = vec![platform_hint(inputs.gates)];

    context.extend(inputs.project_context.clone());
    if workspace.is_empty() {
        stable.extend(trailing);
        stable.extend(post_workspace);
    } else {
        context.extend(workspace);
        context.extend(trailing);
        context.extend(post_workspace);
    }

    // Volatile tier.
    let mut volatile: Vec<String> = vec![skills.to_owned()];
    volatile.extend(inputs.memory_blocks.clone());
    volatile.push(profile_line(inputs.profile));
    volatile.push(timestamp_lines(inputs));
    let environment = environment_hints(inputs.gates, inputs.profile);
    if !environment.is_empty() {
        volatile.push(format!(
            "{}\n\n{environment}\n\n{}",
            t.runtime_heading, t.runtime_end
        ));
    }
    [stable, context, volatile]
        .iter()
        .map(|tier| join(tier))
        .filter(|tier| !tier.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The execution guidance with the lines dropped whose tools are not offered.
pub(crate) fn execution_text(tools: &BTreeSet<String>) -> String {
    let t = text();
    let mut text = t.execution_guidance.clone();
    for (line, needed) in &t.execution_line_tools {
        if !needed.iter().any(|name| tools.contains(name)) {
            text = text.replace(line.as_str(), "");
        }
    }
    text
}

/// The coding brief, without the `todo_list` sentence when that tool is absent and with the edit
/// format line of the model family.
pub(crate) fn coding_brief(model: &str, todo_list: bool) -> String {
    let t = text();
    let mut brief = t.coding_guidance.clone();
    if !todo_list {
        brief = brief.replace(&t.todo_sentence, &t.no_todo_sentence);
    }
    let lowered = model.to_lowercase();
    if !lowered.is_empty() {
        for format in t.edit_format.values() {
            if format.needles.iter().any(|needle| lowered.contains(needle)) {
                brief = format!("{brief}\n{}", format.line);
                break;
            }
        }
    }
    brief
}

fn platform_hint(gates: &PromptGates) -> String {
    let t = text();
    let base = if gates.hint_replace.is_empty() {
        t.platform_hint_cli.clone()
    } else {
        gates.hint_replace.clone()
    };
    if gates.hint_append.is_empty() {
        base
    } else {
        format!("{base}\n\n{}", gates.hint_append).trim().to_owned()
    }
}

// ---- workspace

fn home_dir() -> Option<PathBuf> {
    std::env::home_dir().and_then(|dir| dir.canonical().ok())
}

pub(crate) fn git_root(cwd: &Path) -> Option<PathBuf> {
    let candidate = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    // Verify both a working tree and its root. An empty .git marker can occur in
    // shared temporary directories or inside a real parent repository.
    let command = GitProbe::command_at(
        candidate,
        &[],
        &["rev-parse", "--is-inside-work-tree", "--show-prefix"],
    );
    let output = bounded(command)?;
    output
        .lines()
        .eq(["true", ""])
        .then(|| candidate.to_path_buf())
}

/// The nearest ancestor (up to six levels above) that looks like a project root; the home
/// directory and the shared temp root never count.
fn marker_root(cwd: &Path) -> Option<PathBuf> {
    let skip = [home_dir(), std::env::temp_dir().canonical().ok()];
    cwd.ancestors().take(7).find_map(|dir| {
        let skipped = skip.iter().flatten().any(|path| path == dir);
        let marked = text()
            .project_markers
            .iter()
            .any(|marker| dir.join(marker).exists());
        (!skipped && marked).then(|| dir.to_path_buf())
    })
}

fn has_code_files(root: &Path) -> bool {
    let t = text();
    let mut seen = 0;
    let mut stack = vec![(root.to_path_buf(), true)];
    while let Some((directory, is_root)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 500 {
                return false;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() {
                let extension = Path::new(&name)
                    .extension()
                    .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
                    .unwrap_or_default();
                if t.code_extensions.contains(&extension) {
                    return true;
                }
            } else if is_root
                && kind.is_dir()
                && !t.scan_skip_dirs.contains(&name)
                && !name.starts_with('.')
            {
                stack.push((entry.path(), false));
            }
        }
    }
    false
}

/// The workspace snapshot when this is a code workspace: `Some(vec![])` for a coding posture
/// without a workspace block, `Some(vec![block])` with one, `None` outside the posture.
fn coding_workspace(root: &Path, mode: CodingMode, has_tools: bool) -> Option<Vec<String>> {
    if !has_tools || mode == CodingMode::Off {
        return None;
    }
    let cwd = root.canonical().unwrap_or_else(|_| root.to_path_buf());
    let git = git_root(&cwd);
    let coding = mode == CodingMode::On
        || marker_root(&cwd).is_some()
        || git
            .as_ref()
            .is_some_and(|git| Some(git) != home_dir().as_ref() && has_code_files(git));
    if !coding {
        return None;
    }
    let workspace = git.clone().or_else(|| marker_root(&cwd));
    Some(
        workspace
            .map(|workspace| snapshot(&workspace, git.as_deref()))
            .filter(|block| !block.is_empty())
            .into_iter()
            .collect(),
    )
}

fn snapshot(root: &Path, git: Option<&Path>) -> String {
    let t = text();
    let mut lines = vec![
        t.workspace_header.clone(),
        format!("- Root: {}", root.display()),
    ];
    if git.is_some() {
        let probe = GitProbe::new(root);
        let status = probe.status();
        let mut head = String::new();
        let mut upstream = String::new();
        let (mut ahead, mut behind) = ("0".to_owned(), "0".to_owned());
        let mut counts = [0usize; 4];
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("# branch.head ") {
                head = value.to_owned();
            } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
                upstream = value.to_owned();
            } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
                let mut parts = rest.split_whitespace();
                ahead = parts
                    .next()
                    .unwrap_or("+0")
                    .trim_start_matches('+')
                    .to_owned();
                behind = parts
                    .next()
                    .unwrap_or("-0")
                    .trim_start_matches('-')
                    .to_owned();
            } else if line.starts_with("1 ") || line.starts_with("2 ") {
                let xy: Vec<char> = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("..")
                    .chars()
                    .collect();
                counts[0] += usize::from(xy.first() != Some(&'.'));
                counts[1] += usize::from(xy.get(1) != Some(&'.'));
            } else if line.starts_with("u ") {
                counts[3] += 1;
            } else if line.starts_with("? ") {
                counts[2] += 1;
            }
        }
        if head == "(detached)" {
            lines.push("- Branch: (detached HEAD)".into());
        } else if !head.is_empty() {
            let arrow = if upstream.is_empty() {
                String::new()
            } else {
                format!(" \u{2192} {upstream}")
            };
            let tracking =
                if !upstream.is_empty() && (ahead.as_str(), behind.as_str()) != ("0", "0") {
                    format!(" (ahead {ahead}, behind {behind})")
                } else {
                    String::new()
                };
            lines.push(format!("- Branch: {head}{arrow}{tracking}"));
        }
        let git_dir = probe.run(&["rev-parse", "--git-dir"]);
        let common = probe.run(&["rev-parse", "--git-common-dir"]);
        if !git_dir.is_empty()
            && !common.is_empty()
            && root.join(&git_dir).canonical().ok() != root.join(&common).canonical().ok()
        {
            let on_branch = if !head.is_empty() && head != "(detached)" {
                format!(" on branch {head}")
            } else {
                String::new()
            };
            lines.push(format!(
                "- Worktree: linked, isolated{on_branch} (git state shared with primary tree). Changes here do not affect the main working tree or other agents; commit and push your changes, and open a PR if appropriate."
            ));
        }
        let dirty: Vec<String> = ["staged", "modified", "untracked", "conflicts"]
            .iter()
            .zip(counts)
            .filter(|(_, count)| *count > 0)
            .map(|(label, count)| format!("{count} {label}"))
            .collect();
        lines.push(format!(
            "- Status: {}",
            if dirty.is_empty() {
                "clean".to_owned()
            } else {
                dirty.join(", ")
            }
        ));
        let recent = probe.run(&["log", "-3", "--pretty=%h %s"]);
        if !recent.is_empty() {
            lines.push("- Recent commits:".into());
            lines.extend(recent.lines().map(|commit| format!("    {commit}")));
        }
    }
    let facts = ProjectFacts::detect(root);
    if !facts.manifests.is_empty() {
        let managers = if facts.package_managers.is_empty() {
            String::new()
        } else {
            format!(" ({})", facts.package_managers.join("/"))
        };
        let shown: Vec<&str> = facts.manifests.iter().take(6).map(String::as_str).collect();
        lines.push(format!("- Project: {}{managers}", shown.join(", ")));
    }
    if !facts.verify.is_empty() {
        lines.push(format!("- Verify: {}", facts.verify.join("; ")));
    }
    if !facts.context_files.is_empty() {
        lines.push(format!(
            "- Context files: {}",
            facts.context_files.join(", ")
        ));
    }
    lines.join("\n")
}

struct ProjectFacts {
    manifests: Vec<String>,
    package_managers: Vec<String>,
    verify: Vec<String>,
    context_files: Vec<String>,
}

fn read_small(path: &Path) -> String {
    match path.metadata() {
        Ok(meta) if meta.is_file() && meta.len() <= 256 * 1024 => std::fs::read(path)
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .trim_start_matches('\u{feff}')
                    .to_owned()
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

impl ProjectFacts {
    fn detect(root: &Path) -> Self {
        let t = text();
        let mut verify: Vec<String> = Vec::new();
        if root.join("scripts/run_tests.sh").is_file() {
            verify.push("scripts/run_tests.sh".into());
        }
        if root.join("package.json").is_file() {
            let scripts =
                serde_json::from_str::<serde_json::Value>(&read_small(&root.join("package.json")))
                    .ok()
                    .and_then(|json| json.get("scripts").cloned())
                    .and_then(|scripts| scripts.as_object().cloned())
                    .unwrap_or_default();
            let manager = t
                .js_lockfiles
                .iter()
                .find(|[lock, _]| root.join(lock).is_file())
                .map_or("npm", |[_, manager]| manager.as_str());
            verify.extend(
                t.verify_targets
                    .iter()
                    .filter(|name| scripts.contains_key(name.as_str()))
                    .map(|name| format!("{manager} run {name}")),
            );
        }
        if root.join("pytest.ini").is_file()
            || read_small(&root.join("pyproject.toml")).contains("[tool.pytest")
        {
            verify.push("pytest".into());
        }
        let makefile = read_small(&root.join("Makefile"));
        if !makefile.is_empty() {
            for name in &t.verify_targets {
                let found = makefile.lines().any(|line| {
                    line.strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.trim_start().starts_with(':'))
                });
                if found {
                    verify.push(format!("make {name}"));
                }
            }
        }
        let mut seen = BTreeSet::new();
        verify.retain(|command| seen.insert(command.clone()));
        verify.truncate(8);
        let mut managers: Vec<String> = Vec::new();
        for [lock, manager] in t.py_lockfiles.iter().chain(t.js_lockfiles.iter()) {
            if root.join(lock).is_file() && !managers.contains(manager) {
                managers.push(manager.clone());
            }
        }
        Self {
            manifests: t
                .project_markers
                .iter()
                .filter(|marker| !t.context_files.contains(marker) && root.join(marker).is_file())
                .cloned()
                .collect(),
            package_managers: managers,
            verify,
            context_files: t
                .context_files
                .iter()
                .filter(|file| root.join(file).is_file())
                .cloned()
                .collect(),
        }
    }
}

/// `git` probes that must be safe in a repository we do not trust: they run before any tool
/// call, so repository-configured programs (fsmonitor, hooks, filters, pagers) are disabled
/// and nothing may prompt.
struct GitProbe {
    cwd: PathBuf,
    config: Vec<(String, String)>,
    filters_known: bool,
}

impl GitProbe {
    fn new(cwd: &Path) -> Self {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut config: Vec<(String, String)> = [
            ("core.fsmonitor", "false"),
            ("core.hooksPath", null),
            ("core.pager", "cat"),
            ("core.editor", "true"),
            ("core.sshCommand", "ssh -o BatchMode=yes"),
            ("credential.helper", ""),
            ("diff.external", ""),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        // Directories the user already declared safe, read before the global config is cut off.
        let mut safe = Command::new("git");
        safe.args(["config", "--global", "--get-all", "safe.directory"])
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        if let Some(output) = bounded(safe) {
            config.extend(
                output
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| ("safe.directory".to_owned(), line.trim().to_owned())),
            );
        }
        let mut probe = Self {
            cwd: cwd.to_path_buf(),
            config,
            filters_known: false,
        };
        // Filters are named by the repository's attributes; override every command they could run.
        let keys = probe.output(&[
            "config",
            "--includes",
            "--name-only",
            "-z",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ]);
        if let Some(keys) = keys {
            for key in keys.split('\0').filter(|key| !key.is_empty()) {
                probe.config.push((key.to_owned(), String::new()));
                if let Some(name) = key.rsplit_once('.').map(|(name, _)| name) {
                    probe
                        .config
                        .push((format!("{name}.required"), "false".to_owned()));
                }
            }
            probe.filters_known = true;
        }
        probe
    }

    fn command_at(cwd: &Path, config: &[(String, String)], args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(cwd).args(args);
        command.stdin(Stdio::null()).stderr(Stdio::null());
        for (key, _) in std::env::vars() {
            if key.starts_with("GIT_CONFIG")
                || matches!(
                    key.as_str(),
                    "GIT_DIR"
                        | "GIT_WORK_TREE"
                        | "GIT_INDEX_FILE"
                        | "GIT_OBJECT_DIRECTORY"
                        | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                        | "GIT_EXTERNAL_DIFF"
                )
            {
                command.env_remove(key);
            }
        }
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_CONFIG_SYSTEM", null)
            .env("GIT_CONFIG_COUNT", config.len().to_string());
        for (index, (key, value)) in config.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        command
    }

    fn output(&self, args: &[&str]) -> Option<String> {
        bounded(Self::command_at(&self.cwd, &self.config, args))
    }

    fn run(&self, args: &[&str]) -> String {
        self.output(args).unwrap_or_default().trim().to_owned()
    }

    /// `status` only runs when every filter could be overridden; otherwise it is skipped.
    fn status(&self) -> String {
        if !self.filters_known {
            return String::new();
        }
        self.run(&["status", "--porcelain=2", "--branch"])
    }
}

/// Run a command for at most 2.5 seconds; `None` on failure, a non-zero exit or a timeout.
fn bounded(mut command: Command) -> Option<String> {
    command.stdout(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = sender.send(text);
    });
    let deadline = std::time::Instant::now() + Duration::from_millis(2500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = receiver.recv_timeout(Duration::from_millis(500)).ok()?;
                // `config --get-regexp` exits 1 for "no match"; its empty output is the answer.
                return (status.success() || status.code() == Some(1)).then_some(text);
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// ---- volatile tier

fn profile_line(profile: Option<&Path>) -> String {
    let Some(home) = profile else {
        return String::new();
    };
    let name = home
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "profiles"))
        .and_then(|_| home.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    match name {
        None => format!(
            "Active Hermes profile: default. Other profiles (if any) live under {}/profiles/<name>/. Each profile has its own skills/, plugins/, cron/, and memories/ that affect a different session than this one. Do not modify another profile's skills/plugins/cron/memories unless the user explicitly directs you to.",
            home.display()
        ),
        Some(name) => {
            let root = home.parent().and_then(Path::parent).unwrap_or(home);
            format!(
                "Active Hermes profile: {name}. This session reads and writes {home}/. The default profile's data lives at {root}/skills/, {root}/plugins/, {root}/cron/, {root}/memories/ — those belong to a different session run from a different shell. Do NOT modify another profile's skills/plugins/cron/memories unless the user explicitly directs you to.",
                home = home.display(),
                root = root.display()
            )
        }
    }
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// A date and the zone details the prompt shows.
struct Moment {
    weekday: usize,
    month: usize,
    day: u32,
    year: i32,
    key: Option<String>,
    abbreviation: String,
    /// Seconds east of UTC.
    offset: i32,
}

fn now_in(timezone: &str) -> Moment {
    let name = std::env::var("HERMES_TIMEZONE")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| timezone.to_owned());
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    if let Ok(zone) = name.parse::<chrono_tz::Tz>() {
        use chrono::{Datelike, Offset, TimeZone};
        if let Some(time) = zone.timestamp_opt(seconds, 0).single() {
            return Moment {
                weekday: time.weekday().num_days_from_sunday() as usize,
                month: time.month0() as usize,
                day: time.day(),
                year: time.year(),
                key: Some(name),
                abbreviation: time.format("%Z").to_string(),
                offset: time.offset().fix().local_minus_utc(),
            };
        }
    }
    local_moment(seconds)
}

#[cfg(unix)]
fn local_moment(seconds: i64) -> Moment {
    // SAFETY: localtime_r fills the zeroed struct it is given; tm_zone points to static zone data.
    unsafe {
        let time: libc::time_t = seconds as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&time, &mut tm);
        let abbreviation = if tm.tm_zone.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(tm.tm_zone)
                .to_string_lossy()
                .into_owned()
        };
        Moment {
            weekday: tm.tm_wday as usize % 7,
            month: tm.tm_mon as usize % 12,
            day: tm.tm_mday as u32,
            year: tm.tm_year + 1900,
            key: None,
            abbreviation,
            offset: tm.tm_gmtoff as i32,
        }
    }
}

#[cfg(not(unix))]
fn local_moment(seconds: i64) -> Moment {
    use chrono::Datelike;
    let time = chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default();
    Moment {
        weekday: time.weekday().num_days_from_sunday() as usize,
        month: time.month0() as usize,
        day: time.day(),
        year: time.year(),
        key: None,
        abbreviation: "UTC".into(),
        offset: 0,
    }
}

fn timestamp_lines(inputs: &Inputs<'_>) -> String {
    let now = now_in(&inputs.gates.timezone);
    let mut bits: Vec<String> = Vec::new();
    if let Some(key) = &now.key {
        bits.push(key.clone());
    }
    if !now.abbreviation.is_empty() && Some(&now.abbreviation) != now.key.as_ref() {
        bits.push(now.abbreviation.clone());
    }
    let sign = if now.offset < 0 { '-' } else { '+' };
    let minutes = now.offset.abs() / 60;
    bits.push(format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60));
    let suffix = format!(" ({})", bits.join(", "));
    format!(
        "Conversation started: {}, {} {:02}, {}{suffix}\nModel: {}\nProvider: {}\nPlatform: cli",
        WEEKDAYS[now.weekday], MONTHS[now.month], now.day, now.year, inputs.model, inputs.provider
    )
}

fn environment_hints(gates: &PromptGates, profile: Option<&Path>) -> String {
    let mut hints: Vec<String> = Vec::new();
    let wsl = is_wsl();
    let host = if wsl {
        "WSL (Windows Subsystem for Linux)".to_owned()
    } else if cfg!(target_os = "macos") {
        format!("macOS ({})", command_text("sw_vers", &["-productVersion"]))
    } else if cfg!(windows) {
        "Windows".to_owned()
    } else {
        format!(
            "Linux ({})",
            std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
        )
    };
    let home = std::env::home_dir().map(|dir| dir.display().to_string());
    let mut lines = vec![format!("Host: {host}")];
    if let Some(home) = home {
        lines.push(format!("User home directory: {home}"));
    }
    let cwd = std::env::var("TERMINAL_CWD")
        .ok()
        .filter(|dir| Path::new(dir).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    if let Some(cwd) = cwd {
        lines.push(format!("Current working directory: {}", cwd.display()));
    }
    if let Some(scratch) = profile.and_then(crate::scratch::directory) {
        lines.push(format!(
            "Scratch directory: {} (TMPDIR points here; write temporary files and probes there, never under the system temp dir; entries idle for 24h are pruned)",
            scratch.display()
        ));
    }
    hints.push(lines.join("\n"));
    if wsl {
        hints.push(text().wsl_hint.clone());
    }
    let embedder = std::env::var("HERMES_ENVIRONMENT_HINT")
        .ok()
        .map(|hint| hint.trim().to_owned())
        .filter(|hint| !hint.is_empty())
        .unwrap_or_else(|| gates.environment_hint.clone());
    hints.push(embedder);
    hints
        .into_iter()
        .filter(|hint| !hint.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn command_text(program: &str, args: &[&str]) -> String {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    bounded(command).unwrap_or_default().trim().to_owned()
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/version")
            .is_ok_and(|text| text.to_lowercase().contains("microsoft"))
}

/// Replace the route lines of a saved prompt, which a resume under another model updates.
pub fn update_route_lines(saved: &str, model: &str, provider: &str) -> String {
    let lines: Vec<&str> = saved.split('\n').collect();
    for index in 0..lines.len().saturating_sub(2) {
        if lines[index].starts_with("Model: ")
            && lines[index + 1].starts_with("Provider: ")
            && lines[index + 2] == "Platform: cli"
        {
            let mut updated: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
            updated[index] = format!("Model: {model}");
            updated[index + 1] = format!("Provider: {provider}");
            return updated.join("\n");
        }
    }
    saved.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Gates {
        setting: serde_json::Value,
        model: String,
        enforcement: bool,
        execution: bool,
    }

    #[derive(Deserialize)]
    struct Execution {
        tools: Option<Vec<String>>,
        text: String,
    }

    #[derive(Deserialize)]
    struct Brief {
        model: String,
        todo_list: bool,
        brief: String,
    }

    #[derive(Deserialize)]
    struct Cases {
        gates: Vec<Gates>,
        execution_texts: Vec<Execution>,
        briefs: Vec<Brief>,
    }

    fn cases() -> Cases {
        serde_json::from_str(include_str!("../tests/fixtures/prompt_cases.json")).unwrap()
    }

    #[test]
    fn gates_match_the_reference() {
        let t = text();
        for case in cases().gates {
            let value: Value = serde_yaml::to_value(&case.setting).unwrap();
            // Python's `_model_gate` sees JSON null and numbers as "anything else".
            let gate = Gate::from_config(&value);
            assert_eq!(
                gate.allows(&case.model, &t.enforcement_models),
                case.enforcement,
                "enforcement {:?} {}",
                case.setting,
                case.model
            );
            assert_eq!(
                gate.allows(&case.model, &t.execution_models),
                case.execution,
                "execution {:?} {}",
                case.setting,
                case.model
            );
        }
    }

    #[test]
    fn execution_guidance_drops_lines_for_missing_tools_like_the_reference() {
        for case in cases().execution_texts {
            let tools: BTreeSet<String> = match case.tools {
                Some(tools) => tools.into_iter().collect(),
                // `None` keeps every line: the reference is called without a tool set.
                None => {
                    assert_eq!(text().execution_guidance, case.text);
                    continue;
                }
            };
            assert_eq!(execution_text(&tools), case.text, "{tools:?}");
        }
    }

    #[test]
    fn the_coding_brief_matches_the_reference_for_every_model_family() {
        for case in cases().briefs {
            assert_eq!(
                coding_brief(&case.model, case.todo_list),
                case.brief,
                "{} todo={}",
                case.model,
                case.todo_list
            );
        }
    }

    #[test]
    fn route_lines_are_updated_in_a_saved_prompt() {
        let saved = "a\n\nConversation started: x\nModel: old\nProvider: p\nPlatform: cli\n\nmore";
        assert_eq!(
            update_route_lines(saved, "new", "q"),
            "a\n\nConversation started: x\nModel: new\nProvider: q\nPlatform: cli\n\nmore"
        );
        assert_eq!(update_route_lines("no route", "m", "p"), "no route");
    }
}
