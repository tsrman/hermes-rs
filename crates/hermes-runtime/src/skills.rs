//! Discovery and read-only access to Markdown skills for one CLI turn.

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
        }
    }
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

#[derive(Default)]
pub(crate) struct SkillCatalog {
    skills: Vec<Skill>,
    aliases: BTreeMap<String, Option<usize>>,
    rows: Vec<(String, usize)>,
    visible: HashSet<usize>,
    desc_limit: usize,
}

struct Skill {
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
        Self::resolve(skills, settings)
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
        }
    }

    pub(crate) fn prompt(&self) -> Option<String> {
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
        Some(format!(
            "## Skills\n\nLoad relevant skills with skill_view(name) before proceeding.\n\n<available_skills>\n{}\n</available_skills>",
            index.join("\n")
        ))
    }

    pub(crate) fn schemas() -> [Value; 2] {
        [
            json!({"type": "function", "function": {
                "name": SKILL_VIEW,
                "description": "Read the SKILL.md for a skill by its name or an exact relative name from the skills index.",
                "parameters": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"], "additionalProperties": false}
            }}),
            json!({"type": "function", "function": {
                "name": SKILLS_LIST,
                "description": "List available skills, optionally filtered by category.",
                "parameters": {"type": "object", "properties": {"category": {"type": "string"}}, "additionalProperties": false}
            }}),
        ]
    }

    pub(crate) fn view(&self, arguments: &str) -> Value {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            name: String,
        }
        let Ok(arguments) = serde_json::from_str::<Arguments>(arguments) else {
            return failure("skill_view expects a skill name");
        };
        let name = arguments.name.trim();
        match self.aliases.get(name) {
            Some(Some(index)) if self.visible.contains(index) => {
                let skill = &self.skills[*index];
                json!({"success": true, "name": skill.name, "description": skill.description, "content": normalize_text(&skill.content),
                       "path": format!("{}/SKILL.md", skill.relative)})
            }
            Some(None) => failure("Ambiguous skill name. Use an exact name from skills_list."),
            _ => failure("Skill is unavailable. Use a name from skills_list."),
        }
    }

    pub(crate) fn list(&self, arguments: &str) -> Value {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            category: Option<String>,
        }
        let Ok(arguments) = serde_json::from_str::<Arguments>(arguments) else {
            return failure("skills_list expects an optional category");
        };
        let category = arguments
            .category
            .as_deref()
            .filter(|category| !category.is_empty());
        let mut categories = BTreeSet::new();
        let mut rows = Vec::new();
        for (name, index) in &self.rows {
            let skill = &self.skills[*index];
            if category.is_none_or(|category| category == skill.category) {
                categories.insert(&skill.category);
                rows.push(json!({"name": name, "description": skill.description, "category": skill.category}));
            }
        }
        json!({"success": true, "skills": rows, "categories": categories, "count": rows.len(),
               "hint": "Use skill_view(name) to read SKILL.md."})
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
        if let Ok(cwd) = cwd.canonicalize() {
            let home = std::env::home_dir().and_then(|path| path.canonicalize().ok());
            if let Some(root) = cwd
                .ancestors()
                .take(64)
                .find(|dir| dir.join(".git").exists())
                .filter(|root| home.as_deref() != Some(*root))
                .filter(|root| {
                    settings
                        .trusted_project_dirs
                        .iter()
                        .any(|entry| resolve_config_path(entry, &cwd).as_deref() == Some(*root))
                })
            {
                for prefix in [".hermes", ".agents"] {
                    candidates.push((Tier::Project, root.join(prefix).join("skills"), true));
                }
            }
        }
    }
    let profile = profile.and_then(|path| path.canonicalize().ok());
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
            let resolved = path.canonicalize().ok()?;
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
    path.canonicalize().ok()
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

fn clip_description(description: &str, limit: usize) -> String {
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
