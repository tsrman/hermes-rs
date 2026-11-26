//! Discovery and read-only access to Markdown skills for one CLI turn.

use crate::paths::PathExt;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use serde_yaml::Value as Yaml;

use crate::file_tools::{PATCH, SEARCH_FILES, WRITE_FILE};
use crate::prompt::{normalize_text, read_scoped_text, threats};
use crate::terminal::TERMINAL;

pub(crate) const SKILL_VIEW: &str = "skill_view";
pub(crate) const SKILLS_LIST: &str = "skills_list";
const TOOLS: &[&str] = &["read_file", SEARCH_FILES, SKILL_VIEW, SKILLS_LIST];
const TOOLSETS: &[&str] = &["file", "skills"];
const SKILL_FILE: &str = "SKILL.md";
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".github",
    ".hub",
    ".archive",
    ".curator_backups",
    ".locks",
    ".hermes-dist",
    ".venv",
    "venv",
    "node_modules",
    "site-packages",
    "__pycache__",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];
const SUPPORT_DIRS: &[&str] = &["references", "templates", "assets", "scripts"];
const MAX_FILES: usize = 1024;
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct SkillSettings {
    trusted_project_dirs: Vec<String>,
    project_discovery: bool,
    create_dir: Option<String>,
    external_dirs: Vec<String>,
    disabled: BTreeSet<String>,
    index_desc_limit: usize,
    /// `skills.guard_agent_created`: the reference scans agent-written skills when this is on.
    #[serde(default)]
    guard_agent_created: bool,
    /// `skills.write_approval`: the reference stages skill writes for later approval when on.
    #[serde(default)]
    write_approval: bool,
    /// `skills.auto_load`: skills pinned into the system prompt of every session.
    #[serde(default)]
    auto_load: Vec<String>,
    /// `skills.template_vars` (default on): `${HERMES_SKILL_DIR}` and `${HERMES_SESSION_ID}`.
    #[serde(default = "enabled")]
    template_vars: bool,
    /// `skills.inline_shell`: the reference runs `!`cmd`` snippets of a skill; the port never does.
    #[serde(default)]
    inline_shell: bool,
    /// `skills.config`: the values skills declare under `metadata.hermes.config`.
    #[serde(default)]
    config: Yaml,
    /// `skills.ledger` (default on): append JSONL audit rows and blobs on `skill_manage` mutations.
    #[serde(default = "enabled")]
    ledger: bool,
    /// `skills.ledger_max_bytes` (default 5 MiB; 0 disables trimming).
    #[serde(default = "default_ledger_max_bytes")]
    ledger_max_bytes: usize,
}

fn enabled() -> bool {
    true
}

fn default_ledger_max_bytes() -> usize {
    5 * 1024 * 1024
}

impl Default for SkillSettings {
    fn default() -> Self {
        Self {
            trusted_project_dirs: Vec::new(),
            project_discovery: true,
            create_dir: None,
            external_dirs: Vec::new(),
            disabled: BTreeSet::new(),
            index_desc_limit: 60,
            guard_agent_created: false,
            write_approval: false,
            auto_load: Vec::new(),
            template_vars: true,
            inline_shell: false,
            config: Yaml::Null,
            ledger: true,
            ledger_max_bytes: default_ledger_max_bytes(),
        }
    }
}

impl SkillSettings {
    pub(crate) fn from_config(config: &Yaml) -> Self {
        let mut disabled: BTreeSet<_> = configured_strings(&config["disabled"])
            .into_iter()
            .chain(configured_strings(&config["platform_disabled"]["cli"]))
            .collect();
        disabled.remove("hermes-agent");
        Self {
            trusted_project_dirs: configured_strings(&config["trusted_project_dirs"]),
            project_discovery: config["project_discovery"].as_bool() != Some(false),
            create_dir: config["create_dir"]
                .as_str()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
            external_dirs: configured_strings(&config["external_dirs"]),
            disabled,
            index_desc_limit: config["index_desc_limit"]
                .as_i64()
                .or_else(|| config["index_desc_limit"].as_str()?.trim().parse().ok())
                .unwrap_or(60)
                .clamp(60, 1024) as usize,
            guard_agent_created: truthy(&config["guard_agent_created"]),
            write_approval: truthy(&config["write_approval"]),
            auto_load: auto_load_names(&config["auto_load"]),
            template_vars: config.get("template_vars").is_none_or(py_truthy),
            inline_shell: py_truthy(&config["inline_shell"]),
            config: config["config"].clone(),
            ledger: config.get("ledger").is_none_or(py_truthy),
            ledger_max_bytes: config
                .get("ledger_max_bytes")
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
                .filter(|&n| n >= 0)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or_else(default_ledger_max_bytes),
        }
    }

    pub(crate) fn inline_shell(&self) -> bool {
        self.inline_shell
    }

    pub(crate) fn create_dir(&self) -> Option<&str> {
        self.create_dir.as_deref()
    }

    pub(crate) fn index_desc_limit(&self) -> usize {
        self.index_desc_limit
    }

    pub(crate) fn guard_agent_created(&self) -> bool {
        self.guard_agent_created
    }

    pub(crate) fn write_approval(&self) -> bool {
        self.write_approval
    }

    pub(crate) fn ledger_enabled(&self) -> bool {
        self.ledger
    }

    pub(crate) fn ledger_max_bytes(&self) -> usize {
        self.ledger_max_bytes
    }
}

/// `skills.auto_load`: the string entries of a list, trimmed, without blanks and repeats.
fn auto_load_names(value: &Yaml) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for entry in value.as_sequence().into_iter().flatten() {
        let name = entry.as_str().map(str::trim).unwrap_or("");
        if !name.is_empty() && !names.iter().any(|seen| seen == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// The reference's truthy settings: a boolean, or one of `1`, `true`, `yes`, `on`.
fn truthy(value: &Yaml) -> bool {
    match value {
        Yaml::Bool(flag) => *flag,
        Yaml::String(text) => {
            matches!(
                text.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        }
        Yaml::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

/// The directories skills are managed in: the profile's own, `skills.create_dir`, then the
/// external ones (project directories are read-only context and never written).
pub(crate) fn skill_roots(home: &Path, settings: &SkillSettings) -> Vec<PathBuf> {
    let mut settings = settings.clone();
    settings.project_discovery = false;
    search_roots(Some(home), home, &settings)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

fn strings(value: &Yaml) -> Vec<&str> {
    let values = value
        .as_sequence()
        .map_or(std::slice::from_ref(value), Vec::as_slice);
    values
        .iter()
        .filter_map(Yaml::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

fn configured_strings(value: &Yaml) -> Vec<String> {
    // Profiles can store a list as a quoted YAML or JSON array.
    if let Some(value) = value
        .as_str()
        .filter(|value| value.trim_start().starts_with('['))
    {
        if let Ok(parsed @ Yaml::Sequence(_)) = serde_yaml::from_str(value) {
            return strings(&parsed).into_iter().map(str::to_owned).collect();
        }
    }
    strings(value).into_iter().map(str::to_owned).collect()
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Tier {
    Project,
    Profile,
    CreateDir,
    External,
}

#[derive(Clone, Default)]
pub(crate) struct SkillCatalog {
    skills: Vec<Skill>,
    aliases: BTreeMap<String, Option<usize>>,
    rows: Vec<(String, usize)>,
    visible: HashSet<usize>,
    desc_limit: usize,
    /// The profile's skill settings, for `skill_manage`.
    settings: SkillSettings,
    /// The profile's own skills directory, which `skill_view` paths are shown relative to.
    roots: Vec<PathBuf>,
}

#[derive(Clone)]
struct Skill {
    /// The skill's own directory.
    dir: PathBuf,
    tier: Tier,
    root: usize,
    relative: String,
    leaf: String,
    category: String,
    name: String,
    description: String,
    content: String,
    compatible: bool,
}

impl SkillCatalog {
    pub(crate) fn load(
        profile: Option<&Path>,
        cwd: &Path,
        settings: &SkillSettings,
        write_files: bool,
        terminal_enabled: bool,
    ) -> Self {
        let roots = search_roots(profile, cwd, settings);
        let mut skills = Vec::new();
        let mut bytes_left = MAX_CATALOG_BYTES;
        let mut files_left = MAX_FILES;
        for (root_index, (tier, root)) in roots.iter().enumerate() {
            let mut paths = Vec::new();
            collect_paths(root, 0, files_left, &mut paths);
            files_left -= paths.len();
            for path in paths {
                let Some(skill) = Skill::read(
                    *tier,
                    root_index,
                    root,
                    &path,
                    write_files,
                    terminal_enabled,
                ) else {
                    continue;
                };
                let Some(remaining) = bytes_left.checked_sub(skill.content.len()) else {
                    continue;
                };
                bytes_left = remaining;
                skills.push(skill);
            }
        }
        let mut catalog = Self::resolve(skills, settings);
        catalog.settings = settings.clone();
        catalog.roots = roots
            .iter()
            .filter(|(tier, _)| *tier == Tier::Profile)
            .map(|(_, path)| path.clone())
            .collect();
        catalog
    }

    /// The index description width; 60 for a catalog that was never loaded.
    pub(crate) fn desc_limit(&self) -> usize {
        if self.desc_limit == 0 {
            60
        } else {
            self.desc_limit
        }
    }

    pub(crate) fn settings(&self) -> &SkillSettings {
        &self.settings
    }

    fn resolve(skills: Vec<Skill>, settings: &SkillSettings) -> Self {
        let mut owners: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, skill) in skills.iter().enumerate() {
            for alias in [&skill.name, &skill.leaf, &skill.relative] {
                let candidates = owners.entry(alias.clone()).or_default();
                if candidates.last() != Some(&index) {
                    candidates.push(index);
                }
            }
        }
        let aliases: BTreeMap<_, _> = owners
            .iter()
            .map(|(alias, indices)| (alias.clone(), resolve_alias(&skills, indices)))
            .collect();
        let mut rows = Vec::new();
        let mut visible = HashSet::new();
        for (index, skill) in skills.iter().enumerate() {
            let best_tier = skills[owners[&skill.name][0]].tier;
            let load_name = match aliases[&skill.name] {
                Some(winner) if winner == index => Some(&skill.name),
                None if skill.tier == best_tier && aliases[&skill.relative] == Some(index) => {
                    Some(&skill.relative)
                }
                _ => None,
            };
            if !skill.compatible
                || settings.disabled.contains(&skill.name)
                || settings.disabled.contains(&skill.leaf)
                || load_name.is_some_and(|name| settings.disabled.contains(name))
            {
                continue;
            }
            visible.insert(index);
            if let Some(name) = load_name {
                rows.push((name.clone(), index));
            }
        }
        rows.sort_by(|(left_name, left), (right_name, right)| {
            (&skills[*left].category, left_name).cmp(&(&skills[*right].category, right_name))
        });
        Self {
            skills,
            aliases,
            rows,
            visible,
            desc_limit: settings.index_desc_limit.clamp(60, 1024),
            settings: settings.clone(),
            roots: Vec::new(),
        }
    }

    /// The `## Skills` block. A `-q` run (`single_query`) gets the shorter guidance; `web_search`
    /// says whether the session has web tools, which the long guidance names.
    pub(crate) fn prompt(&self, single_query: bool, web_search: bool) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        let mut by_category: BTreeMap<&str, Vec<(&str, &Skill)>> = BTreeMap::new();
        for (name, index) in &self.rows {
            let skill = &self.skills[*index];
            by_category
                .entry(&skill.category)
                .or_default()
                .push((name, skill));
        }
        let mut index = Vec::new();
        for (category, rows) in by_category {
            let indent = if rows.len() == 1 {
                "  "
            } else {
                index.push(format!("  {category}:"));
                "    "
            };
            for (name, skill) in rows {
                let mut description = clip_description(&skill.description, self.desc_limit);
                if skill.tier == Tier::Project {
                    description = format!("[project] {description}").trim_end().to_owned();
                }
                let description = description.replace('<', "&lt;").replace('>', "&gt;");
                index.push(if description.is_empty() {
                    format!("{indent}- {name}")
                } else {
                    format!("{indent}- {name}: {description}")
                });
            }
        }
        let index = index.join("\n");
        if single_query {
            return Some(format!(
                "{ONESHOT_SKILLS_GUIDANCE}\n<available_skills>\n{index}\n</available_skills>"
            ));
        }
        let basic = if web_search {
            "web_search or terminal"
        } else {
            "terminal"
        };
        Some(format!(
            "## Skills\nBefore replying, scan the skills below. If a skill matches or is even partially relevant to your task, you MUST load it with skill_view(name) and follow its instructions. Err on the side of loading — it is always better to have context you don't need than to miss critical steps, pitfalls, or established workflows. Skills contain specialized knowledge — API endpoints, tool-specific commands, and proven workflows that outperform general-purpose approaches. Load the skill even if you think you could handle the task with basic tools like {basic}. Skills also encode the user's preferred approach, conventions, and quality standards for tasks like code review, planning, and testing — load them even for tasks you already know how to do, because the skill defines how it should be done here.\nIf a skill has issues, fix it with skill_manage(action='patch').\nAfter difficult/iterative tasks, offer to save as a skill. If a skill you loaded was missing steps, had wrong commands, or needed pitfalls you discovered, update it before finishing.\n\n<available_skills>\n{index}\n</available_skills>\n\nOnly proceed without loading a skill if genuinely none are relevant to the task."
        ))
    }

    pub(crate) fn schemas() -> [Value; 2] {
        let function = |text: &str| -> Value {
            json!({"type": "function", "function": serde_json::from_str::<Value>(text).unwrap_or_default()})
        };
        [
            function(include_str!("schemas/skill_view.json")),
            function(include_str!("schemas/skills_list.json")),
        ]
    }

    /// `skill_view`: SKILL.md with its linked files, or one linked file.
    pub(crate) fn view(&self, arguments: &str, home: Option<&Path>) -> Value {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return failure("skill_view expects a skill name");
        };
        let file_path = args
            .get("file_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty());
        let name = name.trim();
        if let Some(error) = lookup_path_error(name) {
            return fail_with(error, [("hint", json!(LOOKUP_HINT))]);
        }
        let index = match self.aliases.get(name) {
            Some(Some(index)) => *index,
            Some(None) => return self.ambiguous(name),
            None => {
                let available: Vec<&str> =
                    self.rows.iter().take(20).map(|(n, _)| n.as_str()).collect();
                return fail_with(
                    format!("Skill '{name}' not found."),
                    [
                        ("available_skills", json!(available)),
                        ("hint", json!("Use skills_list to see all available skills")),
                    ],
                );
            }
        };
        let skill = &self.skills[index];
        let content =
            String::from_utf8_lossy(&std::fs::read(skill.dir.join(SKILL_FILE)).unwrap_or_default())
                .trim_start_matches('\u{feff}')
                .to_owned();
        let metadata = frontmatter(&content).unwrap_or(Yaml::Null);
        if !platform_matches(&metadata) {
            return fail_with(
                format!("Skill '{name}' is not supported on this platform."),
                [("readiness_status", json!("unsupported"))],
            );
        }
        if !self.visible.contains(&index) {
            let resolved = metadata["name"].as_str().unwrap_or(&skill.leaf);
            return failure(&format!(
                "Skill '{resolved}' is disabled. Enable it with `hermes skills` or inspect the files directly on disk."
            ));
        }
        if let Some(file_path) = file_path {
            return serve_skill_file(&skill.dir, file_path, name);
        }
        let hermes = &metadata["metadata"]["hermes"];
        let tags = parse_tags(if py_truthy(&hermes["tags"]) {
            &hermes["tags"]
        } else {
            &metadata["tags"]
        });
        let related = parse_tags(if py_truthy(&hermes["related_skills"]) {
            &hermes["related_skills"]
        } else {
            &metadata["related_skills"]
        });
        let linked = linked_files(&skill.dir);
        let rel_path = self.relative_path(skill);
        let (readiness, extras) = readiness(&metadata, home);
        let mut result = serde_json::Map::new();
        let put = |map: &mut serde_json::Map<String, Value>, key: &str, value: Value| {
            map.insert(key.into(), value);
        };
        put(&mut result, "success", json!(true));
        put(
            &mut result,
            "name",
            yaml_json(metadata.get("name")).unwrap_or_else(|| json!(skill.leaf)),
        );
        put(
            &mut result,
            "description",
            yaml_json(metadata.get("description")).unwrap_or_else(|| json!("")),
        );
        put(&mut result, "tags", json!(tags));
        put(&mut result, "related_skills", json!(related));
        put(
            &mut result,
            "content",
            json!(render_template(&content, &skill.dir)),
        );
        put(&mut result, "path", json!(rel_path));
        put(
            &mut result,
            "skill_dir",
            json!(skill.dir.display().to_string()),
        );
        let has_links = !linked.is_empty();
        put(
            &mut result,
            "linked_files",
            if has_links {
                json!(linked)
            } else {
                Value::Null
            },
        );
        put(
            &mut result,
            "usage_hint",
            if has_links {
                json!("To view linked files, call skill_view(name, file_path) where file_path is e.g. 'references/api.md' or 'assets/config.yaml'")
            } else {
                Value::Null
            },
        );
        for (key, value) in readiness.into_iter().chain(extras) {
            put(&mut result, key, value);
        }
        if let Some(compat) = metadata.get("compatibility").filter(|v| py_truthy(v)) {
            put(
                &mut result,
                "compatibility",
                yaml_json(Some(compat)).unwrap_or(Value::Null),
            );
        }
        if let Some(meta @ Yaml::Mapping(_)) = metadata.get("metadata") {
            put(
                &mut result,
                "metadata",
                yaml_json(Some(meta)).unwrap_or(Value::Null),
            );
        }
        Value::Object(result)
    }

    /// The `skills.auto_load` blocks of the system prompt, and the names that could not be
    /// loaded (missing, ambiguous or disabled). Each block is the skill as the reference renders
    /// a pinned skill: activation note, SKILL.md, directory, config values, setup note and
    /// supporting files.
    pub(crate) fn pinned(
        &self,
        home: Option<&Path>,
        session_id: Option<&str>,
    ) -> (String, Vec<String>) {
        let mut blocks = Vec::new();
        let mut missing = Vec::new();
        for identifier in &self.settings.auto_load {
            let name = identifier.trim_start_matches('/');
            let payload = self.view(&json!({ "name": name }).to_string(), home);
            let name = payload["name"].as_str().unwrap_or(name).to_owned();
            let leaf = name.split_once(':').map_or(name.as_str(), |(_, rest)| rest);
            let disabled = [identifier.as_str(), name.as_str(), leaf]
                .iter()
                .any(|candidate| self.settings.disabled.contains(*candidate));
            if payload["success"] != true || disabled {
                missing.push(identifier.clone());
                continue;
            }
            blocks.push(self.pinned_block(&payload, &name, home, session_id));
        }
        (blocks.join("\n\n"), missing)
    }

    fn pinned_block(
        &self,
        payload: &Value,
        name: &str,
        home: Option<&Path>,
        session_id: Option<&str>,
    ) -> String {
        let dir = payload["skill_dir"].as_str().map(PathBuf::from);
        let raw = dir
            .as_ref()
            .and_then(|dir| std::fs::read(dir.join(SKILL_FILE)).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let raw = raw.trim_start_matches('\u{feff}');
        let content = if self.settings.template_vars {
            let mut text = dir
                .as_deref()
                .map_or_else(|| raw.to_owned(), |dir| render_template(raw, dir));
            if let Some(id) = session_id.filter(|id| !id.is_empty()) {
                text = text.replace("${HERMES_SESSION_ID}", id);
            }
            text
        } else {
            raw.to_owned()
        };
        let mut parts = vec![
            format!(
                "[IMPORTANT: The \"{name}\" skill is auto-loaded via config (skills.auto_load). Treat its instructions as active guidance for the duration of this session unless the user overrides them.]"
            ),
            String::new(),
            content.trim().to_owned(),
        ];
        if let Some(dir) = &dir {
            parts.push(String::new());
            parts.push(format!("[Skill directory: {}]", dir.display()));
            parts.push(SKILL_DIR_NOTE.to_owned());
        }
        let metadata = frontmatter(raw).unwrap_or(Yaml::Null);
        let resolved = skill_config_values(&metadata, &self.settings.config);
        if !resolved.is_empty() {
            let shown = home.map_or_else(|| "~/.hsr".to_owned(), display_home);
            parts.push(String::new());
            parts.push(format!("[Skill config (from {shown}/config.yaml):"));
            parts.extend(
                resolved
                    .iter()
                    .map(|(key, value)| format!("  {key} = {value}")),
            );
            parts.push("]".to_owned());
        }
        if payload["setup_skipped"] == true {
            parts.push(String::new());
            parts.push(format!("[Skill setup note: {SETUP_SKIPPED_NOTE}]"));
        } else if payload["setup_needed"] == true {
            if let Some(note) = payload["setup_note"]
                .as_str()
                .filter(|note| !note.is_empty())
            {
                parts.push(String::new());
                parts.push(format!("[Skill setup note: {note}]"));
            }
        }
        if let Some(dir) = &dir {
            let supporting = supporting_files(dir);
            if !supporting.is_empty() {
                let target = self
                    .roots
                    .iter()
                    .find_map(|root| dir.strip_prefix(root).ok())
                    .map_or_else(
                        || name.to_owned(),
                        |relative| relative.to_string_lossy().replace('\\', "/"),
                    );
                parts.push(String::new());
                parts.push(
                    "[This skill has supporting files (paths relative to the skill directory above):]"
                        .to_owned(),
                );
                parts.extend(supporting.iter().map(|file| format!("- {file}")));
                parts.push(format!(
                    "\nLoad any of these with skill_view(name=\"{target}\", file_path=\"<path>\"), or run scripts directly by absolute path (e.g. `node {}/scripts/foo.js`).",
                    dir.display()
                ));
            }
        }
        parts.join("\n")
    }

    fn ambiguous(&self, name: &str) -> Value {
        let matches: Vec<String> = self
            .skills
            .iter()
            .filter(|skill| [&skill.name, &skill.leaf, &skill.relative].contains(&&name.to_owned()))
            .map(|skill| skill.dir.join(SKILL_FILE).display().to_string())
            .collect();
        let load_names: BTreeSet<&str> = self
            .skills
            .iter()
            .filter(|skill| [&skill.name, &skill.leaf, &skill.relative].contains(&&name.to_owned()))
            .map(|skill| skill.relative.as_str())
            .filter(|relative| *relative != name)
            .collect();
        let uses = if load_names.is_empty() {
            String::new()
        } else {
            format!(
                "use one of {}. ",
                load_names.iter().copied().collect::<Vec<_>>().join(", ")
            )
        };
        fail_with(
            format!(
                "Ambiguous skill name '{name}': {uses}{} different skills share this name in the same skills directory tier; refusing to guess.",
                matches.len()
            ),
            [
                ("matches", json!(matches)),
                ("load_names", json!(load_names)),
                ("hint", json!("Pass the exact relative path instead of the bare name, or rename one of the colliding skills so each name is unique.")),
            ],
        )
    }

    /// The path shown for SKILL.md: relative to the profile's skills dir, else to the parent of
    /// the skill's category directory.
    fn relative_path(&self, skill: &Skill) -> String {
        let skill_md = skill.dir.join(SKILL_FILE);
        if let Some(root) = self.roots.first() {
            if let Ok(relative) = skill_md.strip_prefix(root) {
                return relative.display().to_string();
            }
        }
        skill_md
            .parent()
            .and_then(Path::parent)
            .and_then(|base| skill_md.strip_prefix(base).ok())
            .map_or_else(
                || SKILL_FILE.to_owned(),
                |relative| relative.display().to_string(),
            )
    }

    pub(crate) fn list(&self, arguments: &str) -> Value {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let category = args
            .get("category")
            .and_then(Value::as_str)
            .filter(|category| !category.is_empty());
        let mut skills: Vec<(Option<String>, String, String)> = self
            .rows
            .iter()
            .map(|(name, index)| {
                let skill = &self.skills[*index];
                let description: String = if skill.description.chars().count() <= 1024 {
                    skill.description.clone()
                } else {
                    format!(
                        "{}...",
                        skill.description.chars().take(1021).collect::<String>()
                    )
                };
                (
                    reference_category(&skill.relative),
                    name.clone(),
                    description,
                )
            })
            .collect();
        if skills.is_empty() {
            return json!({"success": true, "skills": [], "categories": [],
                          "message": "No skills found in skills/ directory."});
        }
        if let Some(category) = category {
            skills.retain(|(c, _, _)| c.as_deref() == Some(category));
        }
        skills.sort_by(|a, b| {
            (a.0.as_deref().unwrap_or(""), &a.1).cmp(&(b.0.as_deref().unwrap_or(""), &b.1))
        });
        let categories: BTreeSet<&String> =
            skills.iter().filter_map(|(c, _, _)| c.as_ref()).collect();
        let rows: Vec<Value> = skills
            .iter()
            .map(|(category, name, description)| json!({"name": name, "description": description, "category": category}))
            .collect();
        json!({"success": true, "skills": rows, "categories": categories, "count": rows.len(),
               "hint": "Use skill_view(name) to see full content, tags, and linked files"})
    }
}

const LOOKUP_HINT: &str = "Use a skill name or relative path within the skills directory.";

fn fail_with<const N: usize>(error: impl Into<String>, extra: [(&str, Value); N]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("success".into(), json!(false));
    map.insert("error".into(), json!(error.into()));
    for (key, value) in extra {
        map.insert(key.into(), value);
    }
    Value::Object(map)
}

/// A name that could escape the directory it is joined onto.
fn lookup_path_error(name: &str) -> Option<&'static str> {
    if name.starts_with('/') || name.starts_with('\\') || name.as_bytes().get(1) == Some(&b':') {
        return Some("Skill name must be a relative path within the skills directory.");
    }
    name.split(['/', '\\'])
        .any(|part| part == "..")
        .then_some("Skill name cannot contain '..' path traversal components.")
}

/// The category the reference shows: the first directory of a skill nested at least two deep.
fn reference_category(relative: &str) -> Option<String> {
    let (first, rest) = relative.split_once('/')?;
    (!rest.is_empty()).then(|| first.to_owned())
}

fn py_truthy(value: &Yaml) -> bool {
    match value {
        Yaml::Null => false,
        Yaml::Bool(flag) => *flag,
        Yaml::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Yaml::String(text) => !text.is_empty(),
        Yaml::Sequence(items) => !items.is_empty(),
        Yaml::Mapping(map) => !map.is_empty(),
        Yaml::Tagged(_) => true,
    }
}

fn yaml_json(value: Option<&Yaml>) -> Option<Value> {
    serde_json::to_value(value?).ok()
}

/// Tags from frontmatter: a list, `[a, b]` or `a, b`.
fn parse_tags(value: &Yaml) -> Vec<String> {
    if !py_truthy(value) {
        return Vec::new();
    }
    if let Yaml::Sequence(items) = value {
        return items
            .iter()
            .filter(|item| py_truthy(item))
            .map(|item| match item {
                Yaml::String(text) => text.trim().to_owned(),
                other => serde_yaml::to_string(other)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            })
            .collect();
    }
    let text = match value {
        Yaml::String(text) => text.trim().to_owned(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    };
    let text = text
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(&text);
    text.split(',')
        .map(|tag| tag.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// `skill_matches_platform`: the `platforms:` list (absent = every OS) names this one.
fn platform_matches(metadata: &Yaml) -> bool {
    let platforms = &metadata["platforms"];
    if !py_truthy(platforms) {
        return true;
    }
    let host = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let items: Vec<&Yaml> = match platforms {
        Yaml::Sequence(items) => items.iter().collect(),
        other => vec![other],
    };
    items.iter().any(|platform| {
        let text = match platform {
            Yaml::String(text) => text.clone(),
            other => serde_yaml::to_string(other).unwrap_or_default(),
        };
        let mapped = match text.trim().to_lowercase().as_str() {
            "macos" => "darwin".to_owned(),
            "windows" => "win32".to_owned(),
            other => other.to_owned(),
        };
        host.starts_with(&mapped) || (host == "linux" && mapped == "linux")
    })
}

/// `${HERMES_SKILL_DIR}` becomes the skill's directory; the session token stays, as it does for
/// a session the reference cannot name.
fn render_template(content: &str, dir: &Path) -> String {
    content.replace("${HERMES_SKILL_DIR}", &dir.display().to_string())
}

fn glob_ext(path: &Path, extensions: &[&str]) -> bool {
    extensions.is_empty()
        || path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
}

fn walk(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        out.push(path.clone());
        if recursive && path.is_dir() {
            walk(&path, true, out);
        }
    }
}

/// references/templates/assets/scripts of a skill directory; empty groups are left out.
fn linked_files(dir: &Path) -> BTreeMap<&'static str, Vec<String>> {
    type Spec = (&'static str, &'static [&'static str], bool, bool);
    const SPECS: [Spec; 4] = [
        ("references", &["md"], false, false),
        (
            "templates",
            &["md", "py", "yaml", "yml", "json", "tex", "sh"],
            true,
            false,
        ),
        ("assets", &[], true, true),
        (
            "scripts",
            &["py", "sh", "bash", "js", "ts", "rb"],
            false,
            false,
        ),
    ];
    let mut out = BTreeMap::new();
    for (sub, extensions, recursive, files_only) in SPECS {
        let base = dir.join(sub);
        if !base.exists() {
            continue;
        }
        let mut paths = Vec::new();
        walk(&base, recursive, &mut paths);
        let found: Vec<String> = paths
            .iter()
            .filter(|path| glob_ext(path, extensions) && (!files_only || path.is_file()))
            .filter_map(|path| path.strip_prefix(dir).ok())
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .collect();
        if !found.is_empty() {
            out.insert(sub, found);
        }
    }
    out
}

/// Non-SKILL.md files grouped by support dir, plus `other` for known source extensions elsewhere.
fn available_files(dir: &Path) -> serde_json::Map<String, Value> {
    const EXTS: [&str; 7] = ["md", "py", "yaml", "yml", "json", "tex", "sh"];
    let mut paths = Vec::new();
    walk(dir, true, &mut paths);
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths.iter().filter(|path| path.is_file()) {
        if path.file_name().is_some_and(|n| n == SKILL_FILE) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(dir) else {
            continue;
        };
        let rel = relative.to_string_lossy().replace('\\', "/");
        let top = rel.split_once('/').map(|(top, _)| top.to_owned());
        let support = top.filter(|top| SUPPORT_DIRS.contains(&top.as_str()));
        let known = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTS.contains(&e));
        if support.is_some() || known {
            groups
                .entry(support.unwrap_or_else(|| "other".to_owned()))
                .or_default()
                .push(rel);
        }
    }
    let mut out = serde_json::Map::new();
    for key in ["references", "templates", "assets", "scripts", "other"] {
        if let Some(files) = groups.remove(key) {
            out.insert(key.into(), json!(files));
        }
    }
    out
}

fn serve_skill_file(dir: &Path, file_path: &str, label: &str) -> Value {
    let hint = [(
        "hint",
        json!("Use a relative path within the skill directory"),
    )];
    if file_path.split(['/', '\\']).any(|part| part == "..") {
        return fail_with("Path traversal ('..') is not allowed.", hint);
    }
    let target = dir.join(file_path);
    let inside = match (target.canonical(), dir.canonical()) {
        (Ok(target), Ok(root)) => target.starts_with(root),
        _ => true,
    };
    if !inside {
        return fail_with(
            format!(
                "Path escapes allowed directory: '{}' is not in the subpath of '{}'",
                target.display(),
                dir.display()
            ),
            hint,
        );
    }
    if !target.is_file() {
        return fail_with(
            format!("File '{file_path}' not found in skill '{label}'."),
            [
                ("available_files", Value::Object(available_files(dir))),
                (
                    "hint",
                    json!("Use one of the available file paths listed above"),
                ),
            ],
        );
    }
    let content = String::from_utf8_lossy(&fs::read(&target).unwrap_or_default())
        .trim_start_matches('\u{feff}')
        .to_owned();
    let suffix = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    json!({"success": true, "name": label, "file": file_path, "content": content, "file_type": suffix})
}

/// Required environment variables a skill declares, merged and deduplicated (first entry wins).
fn required_env_vars(metadata: &Yaml) -> Vec<serde_json::Map<String, Value>> {
    let text = |value: &Yaml| -> Option<String> {
        value
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    let setup = &metadata["setup"];
    let setup_help = text(&setup["help"]);
    let mut entries: Vec<Yaml> = Vec::new();
    let declared = &metadata["required_environment_variables"];
    match declared {
        Yaml::Mapping(_) => entries.push(declared.clone()),
        Yaml::Sequence(items) => entries.extend(
            items
                .iter()
                .filter(|i| i.is_string() || i.is_mapping())
                .cloned(),
        ),
        _ => {}
    }
    let collect = &setup["collect_secrets"];
    let secrets: Vec<&Yaml> = match collect {
        Yaml::Mapping(_) => vec![collect],
        Yaml::Sequence(items) => items.iter().collect(),
        _ => Vec::new(),
    };
    for item in secrets.into_iter().filter(|i| i.is_mapping()) {
        let url = item["provider_url"]
            .as_str()
            .or(item["url"].as_str())
            .map(str::trim)
            .filter(|u| !u.is_empty());
        let mut map = serde_yaml::Mapping::new();
        map.insert("name".into(), item["env_var"].clone());
        map.insert("prompt".into(), item["prompt"].clone());
        if let Some(url) = url {
            map.insert("url".into(), Yaml::String(url.to_owned()));
        }
        entries.push(Yaml::Mapping(map));
    }
    let legacy = &metadata["prerequisites"]["env_vars"];
    match legacy {
        Yaml::String(name) if !name.trim().is_empty() => {
            entries
                .push(serde_yaml::from_str(&format!("{{name: {name:?}}}")).unwrap_or(Yaml::Null));
        }
        Yaml::Sequence(items) => {
            for item in items {
                if let Yaml::String(name) = item {
                    if !name.trim().is_empty() {
                        entries.push(
                            serde_yaml::from_str(&format!("{{name: {name:?}}}"))
                                .unwrap_or(Yaml::Null),
                        );
                    }
                }
            }
        }
        _ => {}
    }
    let name_pattern = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("valid regex");
    let mut required: Vec<serde_json::Map<String, Value>> = Vec::new();
    for entry in entries {
        let entry = match &entry {
            Yaml::String(name) => {
                let mut map = serde_yaml::Mapping::new();
                map.insert("name".into(), Yaml::String(name.clone()));
                Yaml::Mapping(map)
            }
            other => other.clone(),
        };
        let pick = |key: &str| match &entry[key] {
            Yaml::Null | Yaml::Bool(false) => String::new(),
            Yaml::String(text) => text.clone(),
            other => serde_yaml::to_string(other)
                .unwrap_or_default()
                .trim()
                .to_owned(),
        };
        let name = {
            let first = pick("name");
            if first.is_empty() {
                pick("env_var")
            } else {
                first
            }
        }
        .trim()
        .to_owned();
        if name.is_empty()
            || !name_pattern.is_match(&name)
            || required
                .iter()
                .any(|existing| existing["name"] == name.as_str())
        {
            continue;
        }
        let prompt = {
            let prompt = pick("prompt");
            if prompt.is_empty() {
                format!("Enter value for {name}")
            } else {
                prompt
            }
        };
        let mut normalized = serde_json::Map::new();
        normalized.insert("name".into(), json!(name));
        normalized.insert("prompt".into(), json!(prompt.trim()));
        let help = ["help", "provider_url", "url"]
            .iter()
            .find_map(|key| text(&entry[*key]))
            .or_else(|| setup_help.clone());
        if let Some(help) = help {
            normalized.insert("help".into(), json!(help));
        }
        if let Some(required_for) = text(&entry["required_for"]) {
            normalized.insert("required_for".into(), json!(required_for));
        }
        if py_truthy(&entry["optional"]) {
            normalized.insert("optional".into(), json!(true));
        }
        required.push(normalized);
    }
    required
}

type Fields = Vec<(&'static str, Value)>;

/// The readiness fields of a loaded skill, and the extras that follow them. No secret can be
/// captured here, so every missing variable stays missing.
fn readiness(metadata: &Yaml, home: Option<&Path>) -> (Fields, Fields) {
    let required = required_env_vars(metadata);
    let snapshot: std::collections::HashMap<String, String> = home
        .and_then(|home| dotenvy::from_path_iter(home.join(".env")).ok())
        .map(|iter| iter.flatten().collect())
        .unwrap_or_default();
    let persisted = |name: &str| match snapshot.get(name) {
        Some(value) => !value.is_empty(),
        None => std::env::var(name).is_ok_and(|value| !value.is_empty()),
    };
    let remaining: Vec<String> = required
        .iter()
        .filter(|entry| entry.get("optional").is_none())
        .filter_map(|entry| entry["name"].as_str())
        .filter(|name| !persisted(name))
        .map(str::to_owned)
        .collect();
    let mut setup_needed = !remaining.is_empty();
    let mut missing_files: Vec<Value> = Vec::new();
    if let Some(files) = metadata["required_credential_files"].as_sequence() {
        for file in files.iter().filter_map(Yaml::as_str) {
            let present = home.is_some_and(|home| home.join(file).exists());
            if !present {
                missing_files.push(json!(file));
            }
        }
        setup_needed = setup_needed || !missing_files.is_empty();
    }
    let status = if setup_needed {
        "setup_needed"
    } else {
        "available"
    };
    let fields = vec![
        ("required_environment_variables", json!(required)),
        ("required_commands", json!([])),
        ("missing_required_environment_variables", json!(remaining)),
        ("missing_credential_files", json!(missing_files)),
        ("missing_required_commands", json!([])),
        ("setup_needed", json!(setup_needed)),
        ("setup_skipped", json!(false)),
        ("readiness_status", json!(status)),
    ];
    let mut extras: Vec<(&'static str, Value)> = Vec::new();
    let help = required
        .iter()
        .find_map(|entry| entry.get("help").and_then(Value::as_str));
    if let Some(help) = help {
        extras.push(("setup_help", json!(help)));
    }
    if setup_needed {
        let missing: Vec<String> = remaining
            .iter()
            .map(|name| format!("env ${name}"))
            .chain(
                missing_files
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|f| format!("file {f}")),
            )
            .collect();
        let mut note = format!(
            "Setup needed before using this skill: missing {}.",
            if missing.is_empty() {
                "required prerequisites".to_owned()
            } else {
                missing.join(", ")
            }
        );
        if let Some(help) = help {
            note = format!("{note} {help}");
        }
        extras.push(("setup_note", json!(note)));
    }
    (fields, extras)
}

const ONESHOT_SKILLS_GUIDANCE: &str = "## Skills\nScan the skills below and load one with skill_view(name) only when it carries domain knowledge you lack for THIS task (an API, a tool's commands, a project's conventions). Do not load general process skills (testing, debugging, review methodology) for work you already know how to do.\n";
const SKILL_DIR_NOTE: &str = "Resolve any relative paths in this skill (e.g. `scripts/foo.js`, `templates/config.yaml`) against that directory, then run them with the terminal tool using the absolute path.";
const SETUP_SKIPPED_NOTE: &str = "Required environment setup was skipped. Continue loading the skill and explain any reduced functionality if it matters.";

/// `~/…` for a directory under the user's home, else the path as is.
fn display_home(home: &Path) -> String {
    crate::paths::home_dir()
        .and_then(|user| home.strip_prefix(user).ok().map(Path::to_owned))
        .map_or_else(
            || home.display().to_string(),
            |relative| format!("~/{}", relative.to_string_lossy().replace('\\', "/")),
        )
}

/// Skill-relative support files: the linked files in the reference's directory order, or, when
/// there are none, every file under the four support directories.
fn supporting_files(dir: &Path) -> Vec<String> {
    let linked = linked_files(dir);
    let mut files: Vec<String> = ["references", "templates", "assets", "scripts"]
        .iter()
        .filter_map(|key| linked.get(key))
        .flatten()
        .cloned()
        .collect();
    if files.is_empty() {
        for sub in ["references", "templates", "scripts", "assets"] {
            let mut found = Vec::new();
            walk(&dir.join(sub), true, &mut found);
            found.sort();
            files.extend(
                found
                    .iter()
                    .filter(|path| {
                        path.is_file()
                            && !path
                                .symlink_metadata()
                                .is_ok_and(|meta| meta.file_type().is_symlink())
                    })
                    .filter_map(|path| path.strip_prefix(dir).ok())
                    .map(|relative| relative.to_string_lossy().replace('\\', "/")),
            );
        }
    }
    files
}

/// The `metadata.hermes.config` declarations of a skill resolved against `skills.config`:
/// the configured value, else the declared default, else empty (shown as `(not set)`).
fn skill_config_values(metadata: &Yaml, config: &Yaml) -> Vec<(String, String)> {
    let declared = &metadata["metadata"]["hermes"]["config"];
    let items: Vec<&Yaml> = match declared {
        Yaml::Mapping(_) => vec![declared],
        Yaml::Sequence(items) => items.iter().collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for item in items {
        let text = |field: &str| match &item[field] {
            Yaml::String(text) => text.trim().to_owned(),
            Yaml::Null => String::new(),
            other => python_str(other),
        };
        let (key, description) = (text("key"), text("description"));
        if key.is_empty() || description.is_empty() || out.iter().any(|(seen, _)| *seen == key) {
            continue;
        }
        let mut value = key
            .split('.')
            .try_fold(config, |current, part| current.get(part))
            .cloned()
            .unwrap_or(Yaml::Null);
        if matches!(&value, Yaml::Null) || value.as_str().is_some_and(|v| v.trim().is_empty()) {
            value = match item.get("default") {
                Some(Yaml::Null) | None => Yaml::String(String::new()),
                Some(default) => default.clone(),
            };
        }
        let shown = match &value {
            Yaml::String(text) if text.contains('~') || text.contains('$') => expand_path(text),
            other => python_str(other),
        };
        out.push((
            key,
            if py_truthy(&value) {
                shown
            } else {
                "(not set)".to_owned()
            },
        ));
    }
    out
}

/// Python's `str()` of a config scalar, as the reference prints skill config values.
fn python_str(value: &Yaml) -> String {
    match value {
        Yaml::Null => "None".to_owned(),
        Yaml::Bool(true) => "True".to_owned(),
        Yaml::Bool(false) => "False".to_owned(),
        Yaml::Number(number) => number.to_string(),
        Yaml::String(text) => text.clone(),
        Yaml::Sequence(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| match item {
                    Yaml::String(text) => format!("'{text}'"),
                    other => python_str(other),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `os.path.expanduser(os.path.expandvars(value))`: unknown variables stay as written.
fn expand_path(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let tail = &rest[index + 1..];
        let (name, used) = if let Some(braced) = tail.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            (&tail[..end], end)
        };
        match (name.is_empty(), std::env::var(name)) {
            (false, Ok(found)) => {
                out.push_str(&found);
                rest = &tail[used..];
            }
            _ => {
                out.push('$');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    match (out.strip_prefix('~'), crate::paths::home_dir()) {
        (Some(tail), Some(home)) if tail.is_empty() || tail.starts_with('/') => {
            format!("{}{tail}", home.display())
        }
        _ => out,
    }
}

fn failure(error: &str) -> Value {
    json!({"success": false, "error": error})
}

fn resolve_alias(skills: &[Skill], indices: &[usize]) -> Option<usize> {
    let first = *indices.first()?;
    // Candidates are collected in precedence order. Identical same-root copies
    // resolve only when there is one shallowest path.
    let mut same_tier = indices
        .iter()
        .copied()
        .take_while(|index| skills[*index].tier == skills[first].tier);
    same_tier.next();
    let mut preferred = first;
    let mut tied = false;
    for index in same_tier {
        if skills[index].root != skills[first].root
            || skills[index].content != skills[first].content
        {
            return None;
        }
        let depth = |index: usize| skills[index].relative.split('/').count();
        match depth(index).cmp(&depth(preferred)) {
            std::cmp::Ordering::Less => {
                preferred = index;
                tied = false;
            }
            std::cmp::Ordering::Equal => tied = true,
            std::cmp::Ordering::Greater => {}
        }
    }
    (!tied).then_some(preferred)
}

fn search_roots(
    profile: Option<&Path>,
    cwd: &Path,
    settings: &SkillSettings,
) -> Vec<(Tier, PathBuf)> {
    let mut candidates = Vec::new();
    if settings.project_discovery {
        if let Ok(cwd) = cwd.canonical() {
            let home = std::env::home_dir().and_then(|path| path.canonical().ok());
            if let Some(root) = crate::system_prompt::git_root(&cwd)
                .as_deref()
                .filter(|root| home.as_deref() != Some(*root))
                .filter(|root| {
                    settings
                        .trusted_project_dirs
                        .iter()
                        .any(|entry| resolve_config_path(entry, &cwd).as_deref() == Some(*root))
                })
            {
                for prefix in [".hsr", ".hermes", ".agents"] {
                    candidates.push((Tier::Project, root.join(prefix).join("skills"), true));
                }
            }
        }
    }
    let profile = profile.and_then(|path| path.canonical().ok());
    let local = profile.as_ref().map(|home| home.join("skills"));
    if let Some(home) = &profile {
        candidates.push((Tier::Profile, home.join("skills"), true));
        for (tier, path) in settings
            .create_dir
            .iter()
            .map(|path| (Tier::CreateDir, path))
            .chain(
                settings
                    .external_dirs
                    .iter()
                    .map(|path| (Tier::External, path)),
            )
        {
            if let Some(path) = resolve_config_path(path, home) {
                candidates.push((tier, path, false));
            }
        }
    }
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|(tier, path, implicit)| {
            let resolved = path.canonical().ok()?;
            if implicit && resolved != path {
                return None;
            }
            // A profile can live under .hermes in the trusted project itself.
            let local_alias =
                tier == Tier::Project && local.as_ref().is_some_and(|local| local == &resolved);
            (!local_alias && resolved.is_dir() && seen.insert(resolved.clone()))
                .then_some((tier, resolved))
        })
        .collect()
}

fn resolve_config_path(entry: &str, base: &Path) -> Option<PathBuf> {
    expand_config_path(entry, base)?.canonical().ok()
}

/// A configured path with `$VAR` and `~` expanded and relative paths joined to `base`; it need
/// not exist.
pub(crate) fn expand_config_path(entry: &str, base: &Path) -> Option<PathBuf> {
    static ENV: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)")
            .expect("valid path expansion syntax")
    });
    let expanded = ENV.replace_all(entry, |captures: &regex::Captures<'_>| {
        let name = captures
            .get(1)
            .or_else(|| captures.get(2))
            .expect("matched variable");
        std::env::var(name.as_str()).unwrap_or_else(|_| captures[0].to_owned())
    });
    let path = if expanded == "~" {
        std::env::home_dir()?
    } else if let Some(suffix) = expanded.strip_prefix("~/") {
        std::env::home_dir()?.join(suffix)
    } else {
        base.join(expanded.as_ref())
    };
    Some(path)
}

fn collect_paths(dir: &Path, depth: usize, limit: usize, out: &mut Vec<PathBuf>) {
    if depth >= 32 || out.len() >= limit {
        return;
    }
    let path = dir.join(SKILL_FILE);
    let has_skill = path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_file());
    if has_skill {
        out.push(path);
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if out.len() >= limit {
            break;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && !EXCLUDED_DIRS.contains(&name)
            && !(has_skill && SUPPORT_DIRS.contains(&name))
        {
            collect_paths(&entry.path(), depth + 1, limit, out);
        }
    }
}

impl Skill {
    fn read(
        tier: Tier,
        root_index: usize,
        root: &Path,
        path: &Path,
        write_files: bool,
        terminal_enabled: bool,
    ) -> Option<Self> {
        let directory = path.parent()?;
        let leaf = directory.file_name()?.to_str()?.to_owned();
        let mut relative = directory
            .strip_prefix(root)
            .ok()?
            .iter()
            .map(|part| part.to_str())
            .collect::<Option<Vec<_>>>()?
            .join("/");
        if relative.is_empty() {
            relative.clone_from(&leaf);
        }
        if !valid_name(&relative) {
            return None;
        }
        let content = read_scoped_text(path, root, false)?;
        let normalized = normalize_text(&content);
        if normalized.trim().is_empty()
            || !threats::findings(&normalized, tier == Tier::Project).is_empty()
        {
            return None;
        }
        let metadata = frontmatter(&normalized)?;
        let name = metadata["name"]
            .as_str()
            .filter(|name| valid_name(name) && !name.contains('/'))
            .unwrap_or(&leaf)
            .to_owned();
        let description = metadata["description"]
            .as_str()
            .unwrap_or_default()
            .trim_matches(['\'', '"'])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let category = relative
            .rsplit_once('/')
            .map_or_else(|| leaf.clone(), |(category, _)| category.to_owned());
        Some(Self {
            dir: directory.to_path_buf(),
            tier,
            root: root_index,
            relative,
            leaf,
            category,
            name,
            description,
            compatible: compatible(&metadata, write_files, terminal_enabled),
            content,
        })
    }
}

fn valid_name(name: &str) -> bool {
    name.split('/').all(|part| {
        !part.is_empty()
            && !matches!(part, "." | "..")
            && part
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    })
}

fn frontmatter(text: &str) -> Option<Yaml> {
    let Some(rest) = text.strip_prefix("---\n") else {
        return Some(Yaml::Null);
    };
    let mut end = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return serde_yaml::from_str(&rest[..end]).ok();
        }
        end += line.len();
    }
    None
}

fn compatible(metadata: &Yaml, write_files: bool, terminal_enabled: bool) -> bool {
    let platforms = strings(&metadata["platforms"]);
    let host = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        host => host,
    };
    if !platforms.is_empty()
        && !platforms.iter().any(|platform| {
            let platform = platform.to_ascii_lowercase();
            let expected = match platform.as_str() {
                "macos" => "darwin",
                "windows" => "win32",
                text => text,
            };
            host.starts_with(expected)
        })
    {
        return false;
    }
    // Plugin application declarations have no provider in the local Rust CLI.
    if !strings(&metadata["requires_apps"]).is_empty() {
        return false;
    }
    let environment = strings(&metadata["environments"]);
    if !environment.is_empty()
        && !environment
            .iter()
            .any(|tag| match tag.to_ascii_lowercase().as_str() {
                "kanban" => false,
                "docker" => {
                    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
                }
                "s6" => {
                    Path::new("/run/s6").is_dir() || Path::new("/package/admin/s6-overlay").is_dir()
                }
                _ => true,
            })
    {
        return false;
    }
    let hermes = &metadata["metadata"]["hermes"];
    let has_tool = |name: &&str| {
        TOOLS.contains(name)
            || (write_files && matches!(*name, WRITE_FILE | PATCH))
            || (terminal_enabled && *name == TERMINAL)
    };
    let has_toolset =
        |name: &&str| TOOLSETS.contains(name) || (terminal_enabled && *name == "terminal");
    let surface = strings(&hermes["session_platforms"]);
    (surface.is_empty() || surface.iter().any(|name| name.eq_ignore_ascii_case("cli")))
        && !strings(&hermes["fallback_for_tools"]).iter().any(has_tool)
        && !strings(&hermes["fallback_for_toolsets"])
            .iter()
            .any(has_toolset)
        && strings(&hermes["requires_tools"]).iter().all(has_tool)
        && strings(&hermes["requires_toolsets"])
            .iter()
            .all(has_toolset)
}

pub(crate) fn clip_description(description: &str, limit: usize) -> String {
    if description.chars().count() <= limit {
        return description.to_owned();
    }
    let byte = description
        .char_indices()
        .nth(limit - 2)
        .expect("long description")
        .0;
    let head = &description[..byte];
    let clipped = head
        .rsplit_once(char::is_whitespace)
        .map_or("", |(prefix, _)| {
            prefix.trim_end_matches([',', ';', ':', '.', '-', ' '])
        });
    if clipped.is_empty() {
        format!(
            "{}...",
            description.chars().take(limit - 3).collect::<String>()
        )
    } else {
        format!("{clipped}...")
    }
}

#[cfg(test)]
mod reference_tests {
    use super::*;

    fn normalize(value: &Value, root: &str) -> Value {
        match value {
            // Windows paths use `\`; the reference output was recorded with `/`.
            Value::String(text) => json!(crate::paths::forward_slashes_after(
                &text.replace(root, "<skills>"),
                "<skills>"
            )),
            Value::Array(items) => {
                let mut items: Vec<Value> = items.iter().map(|i| normalize(i, root)).collect();
                if !items.is_empty() && items.iter().all(Value::is_string) {
                    items.sort_by_key(|item| item.as_str().unwrap_or("").to_owned());
                }
                Value::Array(items)
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), normalize(v, root)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    #[test]
    fn listings_and_views_match_the_reference() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/skill_view_cases.json")).unwrap();
        let home = tempfile::tempdir().unwrap();
        let skills_dir = home.path().join("skills");
        for (relative, text) in fixture["files"].as_object().unwrap() {
            let target = if relative == ".env" {
                home.path().join(relative)
            } else {
                skills_dir.join(relative)
            };
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            let mut text = text.as_str().unwrap().to_owned();
            if cfg!(windows) {
                // `winonly` is the "another OS" skill: on Windows it is the Linux one that is foreign.
                text = text.replace("platforms: [windows]", "platforms: [linux]");
            }
            fs::write(target, text).unwrap();
        }
        let config: Yaml = serde_yaml::from_str(fixture["config"].as_str().unwrap()).unwrap();
        let settings = SkillSettings::from_config(&config["skills"]);
        let catalog = SkillCatalog::load(Some(home.path()), home.path(), &settings, true, true);
        let root = skills_dir.canonical().unwrap().display().to_string();
        for case in fixture["cases"].as_array().unwrap() {
            let arguments = case["arguments"].to_string();
            let ours = match case["kind"].as_str().unwrap() {
                "list" => catalog.list(&arguments),
                "view" => catalog.view(&arguments, Some(home.path())),
                _ => {
                    let empty = SkillCatalog::load(
                        Some(tempfile::tempdir().unwrap().path()),
                        home.path(),
                        &settings,
                        true,
                        true,
                    );
                    empty.list(&arguments)
                }
            };
            assert_eq!(
                normalize(&ours, &root),
                case["result"],
                "{} {arguments}",
                case["kind"]
            );
        }
    }

    #[test]
    fn auto_load_names_keep_order_and_drop_blanks_repeats_and_non_strings() {
        let list: Yaml = serde_yaml::from_str("[a, ' a ', '', 7, b, a]").unwrap();
        assert_eq!(auto_load_names(&list), ["a", "b"]);
        let scalar: Yaml = serde_yaml::from_str("a").unwrap();
        assert!(auto_load_names(&scalar).is_empty());
    }

    #[test]
    fn skill_config_values_follow_the_reference_rules() {
        let metadata: Yaml = serde_yaml::from_str(
            "metadata: {hermes: {config: [\
             {key: demo.path, description: Where, default: '~/dflt'},\
             {key: demo.flag, description: Flag},\
             {key: demo.count, description: Count},\
             {key: demo.on, description: On},\
             {key: demo.list, description: List, default: [a, 2]},\
             {key: demo.path, description: Repeat},\
             {key: nodesc}]}}",
        )
        .unwrap();
        let config: Yaml = serde_yaml::from_str("demo: {flag: true, count: 0, on: yes}").unwrap();
        let home = crate::paths::home_dir()
            .unwrap_or_default()
            .display()
            .to_string();
        let values = skill_config_values(&metadata, &config);
        assert_eq!(
            values,
            [
                ("demo.path".to_owned(), format!("{home}/dflt")),
                ("demo.flag".to_owned(), "True".to_owned()),
                ("demo.count".to_owned(), "(not set)".to_owned()),
                ("demo.on".to_owned(), "yes".to_owned()),
                ("demo.list".to_owned(), "['a', 2]".to_owned()),
            ]
        );
    }

    #[test]
    fn config_paths_expand_known_variables_only() {
        std::env::set_var("HERMES_TEST_SKILL_VAR", "/srv");
        assert_eq!(expand_path("$HERMES_TEST_SKILL_VAR/x"), "/srv/x");
        assert_eq!(expand_path("${HERMES_TEST_SKILL_VAR}/x"), "/srv/x");
        assert_eq!(
            expand_path("$HERMES_TEST_SKILL_UNSET/x"),
            "$HERMES_TEST_SKILL_UNSET/x"
        );
        assert_eq!(expand_path("a$"), "a$");
    }
}
