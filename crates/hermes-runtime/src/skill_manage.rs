//! `skill_manage`: create, patch, rewrite and delete skills and their supporting files.
//!
//! Mirrors `tools/skill_manager_tool.py` and `skill_manager_batch.py`: the advertised call is an
//! array of operations applied atomically (every touched skill is snapshotted and restored when
//! one fails); the older flat single-operation shape is still accepted. Differences by choice:
//! each call asks the person for approval, as `write_file` does, since skill text is loaded into
//! later prompts; `skills.guard_agent_created` and `skills.write_approval` make the tool refuse
//! instead of skipping a check it cannot perform. Not ported: fuzzy matching in `patch` (only
//! exact text is replaced), the audit ledger, usage statistics and the curator.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde_json::{json, Map, Value};
use serde_yaml::Value as Yaml;

use crate::file_tools::ApprovalRequest;
use crate::file_tools::{approval_error, request_approval, ApprovalAction, ApprovalDecision};
use crate::session_search::{active_profile_name, profile_root};
use crate::skill_lint::{lint_content, parse_frontmatter, Finding};
use crate::skills::{clip_description, expand_config_path, skill_roots, SkillSettings};

pub(crate) const SKILL_MANAGE: &str = "skill_manage";

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const MAX_SKILL_CONTENT_CHARS: usize = 100_000;
const MAX_SKILL_FILE_BYTES: usize = 1_048_576;
const ALLOWED_SUBDIRS: [&str; 4] = ["references", "templates", "scripts", "assets"];
const BATCH_MAX_OPS: usize = 20;
const NAME_RULE: &str = "Use lowercase letters, numbers, hyphens, dots, and underscores.";
const ESSENTIAL_SKILLS: [&str; 1] = ["hermes-agent"];

const PATCH_NEEDS_OLD_STRING: &str = "old_string is required for 'patch' and must be the EXACT text currently in the file. Read the target file first (read_file on the skill's SKILL.md, or the file named by file_path) and copy the snippet verbatim, then retry 'patch'. Do NOT fall back to action='write_file' — that rewrites the entire file and destroys unrelated content.";
const PATCH_NEEDS_NEW_STRING: &str =
    "new_string is required for 'patch'. Use an empty string to delete matched text.";
const PATCH_EITHER_OR: &str = "Pass EITHER content (full SKILL.md rewrite) OR old_string/new_string (targeted replacement), not both.";

static NAME_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]*\n?$").expect("valid regex"));
static FRONTMATTER_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n---\s*\n").expect("valid regex"));
/// One skill change at a time, as the reference's per-skill locks serialize them.
static MUTATION: Mutex<()> = Mutex::new(());

pub(crate) fn schema(desc_limit: usize) -> Value {
    let mut function: Value =
        serde_json::from_str(include_str!("schemas/skill_manage.json")).unwrap_or_default();
    if desc_limit != 60 {
        function["description"] = json!(description(desc_limit));
    }
    json!({"type": "function", "function": function})
}

fn description(limit: usize) -> String {
    format!(
        "Create, update or delete skills: your procedural memory for recurring tasks. The operations apply atomically — any failure rolls every touched skill back. Ops: create (lands in the profile's skills directory or configured skills.create_dir; must precede that skill's other ops), patch (targeted old_string/new_string fix — preferred), write_file/remove_file (supporting files), delete (sole op only). Existing skills are modified wherever they live. Description: max {limit} chars, 'Use when <trigger>. <behavior>.', never the category name (the index shows it). Write lessons, not logs: imperative rule + why, no PR numbers/dates/incident narration, one rule per lesson, references/ named by topic (extend before adding). skill_view() shows format conventions."
    )
}

/// What a call needs to know about the profile.
pub(crate) struct Skills<'a> {
    pub home: &'a Path,
    pub settings: &'a SkillSettings,
}

type Outcome = Map<String, Value>;

/// `{"success": false, "error": ...}`
fn refusal(message: impl Into<String>) -> Outcome {
    let mut map = Map::new();
    map.insert("success".into(), json!(false));
    map.insert("error".into(), json!(message.into()));
    map
}

/// `tool_error(message, success=False)`: the error first.
fn shape_error(message: impl Into<String>) -> Value {
    json!({"error": message.into(), "success": false})
}

fn failed(message: impl Into<String>, extra: &[(&str, Value)]) -> Outcome {
    let mut map = refusal(message);
    for (key, value) in extra {
        map.insert((*key).into(), value.clone());
    }
    map
}

// ── Validation ───────────────────────────────────────────────────────────────────────────────────

fn check_identifier(value: &str, label: &str, invalid: String) -> Option<String> {
    if value.chars().count() > MAX_NAME_LENGTH {
        return Some(format!("{label} exceeds {MAX_NAME_LENGTH} characters."));
    }
    (!NAME_PATTERN.is_match(value)).then_some(invalid)
}

fn validate_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Skill name is required.".into());
    }
    check_identifier(
        name,
        "Skill name",
        format!("Invalid skill name '{name}'. {NAME_RULE} Must start with a letter or digit."),
    )
}

fn validate_category(category: Option<&Value>) -> Option<String> {
    let category = match category {
        None | Some(Value::Null) => return None,
        Some(Value::String(text)) => text,
        Some(_) => return Some("Category must be a string.".into()),
    };
    if category.trim().is_empty() {
        return None;
    }
    let category = category.trim();
    let invalid = format!(
        "Invalid category '{category}'. {NAME_RULE} Categories must be a single directory name."
    );
    if category.contains(['/', '\\']) {
        return Some(invalid);
    }
    check_identifier(category, "Category", invalid)
}

fn py_str(value: &Yaml) -> String {
    match value {
        Yaml::Null => "None".into(),
        Yaml::Bool(true) => "True".into(),
        Yaml::Bool(false) => "False".into(),
        Yaml::Number(number) => number.to_string(),
        Yaml::String(text) => text.clone(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    }
}

fn validate_frontmatter(content: &str) -> Option<String> {
    if content.trim().is_empty() {
        return Some("Content cannot be empty.".into());
    }
    let content = content.trim_start_matches('\u{feff}');
    if !content.starts_with("---") {
        return Some(
            "SKILL.md must start with YAML frontmatter (---). See existing skills for format."
                .into(),
        );
    }
    let after = &content[3..];
    let Some(end) = FRONTMATTER_END.find(after) else {
        return Some(
            "SKILL.md frontmatter is not closed. Ensure you have a closing '---' line.".into(),
        );
    };
    let parsed: Yaml = match serde_yaml::from_str(&after[..end.start()]) {
        Ok(parsed) => parsed,
        Err(error) => return Some(format!("YAML frontmatter parse error: {error}")),
    };
    let Yaml::Mapping(map) = &parsed else {
        return Some("Frontmatter must be a YAML mapping (key: value pairs).".into());
    };
    for field in ["name", "description"] {
        if !map.contains_key(field) {
            return Some(format!("Frontmatter must include '{field}' field."));
        }
    }
    let description = py_str(&map["description"]);
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        return Some(format!(
            "Description exceeds {MAX_DESCRIPTION_LENGTH} characters."
        ));
    }
    if after[end.end()..].trim().is_empty() {
        return Some(
            "SKILL.md must have content after the frontmatter (instructions, procedures, etc.)."
                .into(),
        );
    }
    None
}

/// The description the index shows: stripped of quotes, like the reference.
fn description_of(content: &str) -> Option<String> {
    let (frontmatter, _) = parse_frontmatter(content);
    let value = frontmatter.get("description")?;
    Some(py_str(value).trim().trim_matches(['\'', '"']).to_owned())
}

/// A description written by this call must fit the index width; an unchanged legacy over-limit
/// one stays editable.
fn description_budget_error(content: &str, before: Option<&str>, limit: usize) -> Option<String> {
    let description = description_of(content).unwrap_or_default();
    if description.chars().count() <= limit
        || before.is_some_and(|before| description_of(before).as_deref() == Some(&description))
    {
        return None;
    }
    Some(format!(
        "Description is {} chars; a skill description must fit the {limit}-char skill index (one sentence, trigger first, ends with a period, without repeating the category name). The index clips anything longer. Move detail into the skill body.",
        description.chars().count()
    ))
}

fn group_thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn validate_content_size(content: &str, label: &str) -> Option<String> {
    let size = content.chars().count();
    (size > MAX_SKILL_CONTENT_CHARS).then(|| {
        format!(
            "{label} content is {} characters (limit: {}). Consider splitting into a smaller SKILL.md with supporting files in references/ or templates/.",
            group_thousands(size),
            group_thousands(MAX_SKILL_CONTENT_CHARS)
        )
    })
}

fn description_preview(content: &str) -> String {
    let Some(end) = content
        .get(3..)
        .and_then(|after| FRONTMATTER_END.find(after))
    else {
        return String::new();
    };
    let Ok(Yaml::Mapping(map)) = serde_yaml::from_str::<Yaml>(&content[3..end.start() + 3]) else {
        return String::new();
    };
    let text = map.get("description").map(py_str).unwrap_or_default();
    text.chars().take(120).collect()
}

fn clip(text: &str, limit: usize, ellipsis: &str) -> String {
    if text.chars().count() > limit {
        format!("{}{ellipsis}", text.chars().take(limit).collect::<String>())
    } else {
        text.to_owned()
    }
}

// ── Skills on disk ───────────────────────────────────────────────────────────────────────────────

impl Skills<'_> {
    fn local_root(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// Where a new skill lands: `skills.create_dir` when it names something other than the local dir.
    fn create_base(&self) -> PathBuf {
        let Some(entry) = self.settings.create_dir() else {
            return self.local_root();
        };
        let Some(path) = expand_config_path(entry, self.home) else {
            return self.local_root();
        };
        let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
        let local = self
            .local_root()
            .canonicalize()
            .unwrap_or_else(|_| self.local_root());
        if resolved == local {
            self.local_root()
        } else {
            path
        }
    }

    fn roots(&self) -> Vec<PathBuf> {
        skill_roots(self.home, self.settings)
    }

    fn skill_dir_for(&self, name: &str, category: Option<&str>) -> PathBuf {
        let base = self.create_base();
        match category.filter(|category| !category.is_empty()) {
            Some(category) => base.join(category).join(name),
            None => base.join(name),
        }
    }

    /// Every skill directory under `root`, shallow first within a sorted walk.
    fn skill_dirs(root: &Path) -> Vec<PathBuf> {
        const EXCLUDED: [&str; 17] = [
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
        fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
            if depth > 32 {
                return;
            }
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            children.sort();
            let has_skill = dir.join("SKILL.md").is_file();
            if has_skill && depth > 0 {
                out.push(dir.to_owned());
            }
            for child in children {
                let Some(name) = child.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !child.is_dir()
                    || EXCLUDED.contains(&name)
                    || (has_skill && ALLOWED_SUBDIRS.contains(&name))
                {
                    continue;
                }
                walk(&child, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(root, 0, &mut out);
        out
    }

    fn frontmatter_name(skill_dir: &Path) -> Option<String> {
        let text = fs::read(skill_dir.join("SKILL.md")).ok()?;
        let text = String::from_utf8_lossy(&text);
        let text = text.trim_start_matches('\u{feff}');
        let head: String = text.chars().take(4000).collect();
        let (frontmatter, _) = parse_frontmatter(&head);
        match frontmatter.get("name") {
            Some(Yaml::String(name)) => Some(name.chars().take(MAX_NAME_LENGTH).collect()),
            _ => None,
        }
    }

    /// A skill by directory name, categorized path (`category/name`) or, last, frontmatter name
    /// when exactly one skill claims it.
    fn find(&self, name: &str) -> Option<PathBuf> {
        let local = self.local_root();
        let local_resolved = local.canonicalize().unwrap_or(local);
        let categorized = name.contains(['/', '\\']);
        let mut display: Vec<PathBuf> = Vec::new();
        for root in self.roots() {
            for dir in Self::skill_dirs(&root) {
                if dir.file_name().and_then(|n| n.to_str()) == Some(name) {
                    return Some(dir);
                }
                if categorized {
                    if let Ok(resolved) = dir.canonicalize() {
                        if resolved
                            .strip_prefix(&local_resolved)
                            .is_ok_and(|relative| relative.to_string_lossy() == name)
                        {
                            return Some(dir);
                        }
                    }
                }
                if Self::frontmatter_name(&dir).as_deref() == Some(name) {
                    let key = dir
                        .join("SKILL.md")
                        .canonicalize()
                        .unwrap_or(dir.join("SKILL.md"));
                    if !display.iter().any(|seen| {
                        seen.join("SKILL.md").canonicalize().ok().as_ref() == Some(&key)
                    }) {
                        display.push(dir);
                    }
                }
            }
        }
        (display.len() == 1).then(|| display.remove(0))
    }

    fn in_other_profiles(&self, name: &str) -> Vec<(String, PathBuf)> {
        let root = profile_root(self.home);
        let active = self
            .local_root()
            .canonicalize()
            .unwrap_or_else(|_| self.local_root());
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        let default_skills = root.join("skills");
        if default_skills
            .canonicalize()
            .unwrap_or(default_skills.clone())
            != active
        {
            candidates.push(("default".into(), default_skills));
        }
        if let Ok(entries) = fs::read_dir(root.join("profiles")) {
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let skills = entry.path().join("skills");
                if !entry.path().is_dir()
                    || skills
                        .canonicalize()
                        .is_ok_and(|resolved| resolved == active)
                {
                    continue;
                }
                candidates.push((entry.file_name().to_string_lossy().into_owned(), skills));
            }
        }
        candidates
            .into_iter()
            .filter(|(_, skills)| skills.is_dir())
            .filter_map(|(profile, skills)| {
                Self::skill_dirs(&skills)
                    .into_iter()
                    .find(|dir| dir.file_name().and_then(|n| n.to_str()) == Some(name))
                    .map(|hit| (profile, hit))
            })
            .collect()
    }

    fn not_found(&self, name: &str, suffix: &str) -> String {
        let mut base = format!(
            "Skill '{name}' not found in active profile '{}'.",
            active_profile_name(self.home)
        );
        let others = self.in_other_profiles(name);
        match others.as_slice() {
            [] => base.push_str(" Use skills_list() to see available skills."),
            [(profile, path)] => base.push_str(&format!(
                " A skill by that name exists in profile '{profile}' ({}). To edit it, switch profiles (`hermes -p {profile}`) or edit the file directly (file tools / terminal).",
                path.display()
            )),
            many => {
                let names = many
                    .iter()
                    .map(|(profile, _)| format!("'{profile}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                base.push_str(&format!(
                    " Skills by that name exist in other profiles: {names}. Switch profiles (`hermes -p <name>`) to edit there, or edit the files directly (file tools / terminal)."
                ));
            }
        }
        base + suffix
    }

    fn usage_record(&self, name: &str) -> Option<Value> {
        let text = fs::read_to_string(self.local_root().join(".usage.json")).ok()?;
        let all: Value = serde_json::from_str(&text).ok()?;
        all.get(name).cloned()
    }

    /// The user's lock on the skill, whichever form of its name was used.
    fn locked(&self, name: &str) -> Option<String> {
        let mut candidates = vec![name.to_owned()];
        if let Some(leaf) = Path::new(name).file_name().and_then(|n| n.to_str()) {
            candidates.push(leaf.to_owned());
        }
        if let Some(found) = self.find(name) {
            if let Some(leaf) = found.file_name().and_then(|n| n.to_str()) {
                candidates.push(leaf.to_owned());
            }
        }
        candidates.into_iter().find(|candidate| {
            self.usage_record(candidate)
                .is_some_and(|record| record["locked"].as_bool() == Some(true))
        })
    }

    fn pinned(&self, name: &str) -> bool {
        self.usage_record(name)
            .is_some_and(|record| record["pinned"].as_bool() == Some(true))
    }

    fn lock_refusal(&self, name: &str, action: &str) -> Option<Outcome> {
        if action == "create" || name.is_empty() {
            return None;
        }
        let locked = self.locked(name)?;
        let mut map = refusal(format!(
            "Refusing {action}: Skill '{locked}' is locked by the user, so the agent cannot change it: skill_manage, write_file, patch and terminal all refuse. Don't retry or work around it (no copying it under another name); tell the user what you would change. They can unlock it with `hermes curator unlock {locked}` or the lock switch in Desktop."
        ));
        map.insert("_locked".into(), json!(true));
        Some(map)
    }
}

fn is_redirect(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Parts of a relative path as Python's `PurePath.parts` reads them.
fn path_parts(path: &str) -> Vec<String> {
    let mut parts = Vec::new();
    if path.starts_with('/') {
        parts.push("/".to_owned());
    }
    parts.extend(
        path.split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned),
    );
    parts
}

fn validate_file_path(file_path: &str) -> Option<String> {
    if file_path.is_empty() {
        return Some("file_path is required.".into());
    }
    let parts = path_parts(file_path);
    if parts.iter().any(|part| part == "..") {
        return Some("Path traversal ('..') is not allowed.".into());
    }
    if parts.last().is_some_and(|last| last == "SKILL.md") && matches!(parts.len(), 1 | 2) {
        return None;
    }
    if parts.is_empty() || !ALLOWED_SUBDIRS.contains(&parts[0].as_str()) {
        let mut allowed = ALLOWED_SUBDIRS;
        allowed.sort_unstable();
        return Some(format!(
            "File must be under one of: {}. Got: '{file_path}'",
            allowed.join(", ")
        ));
    }
    if parts.len() < 2 {
        return Some(format!(
            "Provide a file path, not just a directory. Example: '{}/myfile.md'",
            parts[0]
        ));
    }
    None
}

/// Resolve a supporting file inside the skill directory, refusing anything that escapes it.
fn resolve_supporting_file(skill_dir: &Path, file_path: &str) -> Result<PathBuf, Outcome> {
    let target = skill_dir.join(file_path);
    if let Some(error) = validate_file_path(file_path) {
        return Err(refusal(error));
    }
    let root = skill_dir
        .canonicalize()
        .map_err(|error| refusal(format!("Path escapes allowed directory: {error}")))?;
    // Resolve through the deepest existing ancestor, so a symlinked parent cannot redirect.
    let mut existing = target.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() && existing.symlink_metadata().is_err() {
        let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
            break;
        };
        tail.push(name);
        if !existing.pop() {
            break;
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| refusal(format!("Path escapes allowed directory: {error}")))?;
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    if resolved.strip_prefix(&root).is_err() {
        return Err(refusal(format!(
            "Path escapes allowed directory: '{}' is not in the subpath of '{}'",
            resolved.display(),
            root.display()
        )));
    }
    Ok(target)
}

fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(target)
        .map(|metadata| metadata.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let directory = target.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".skill_")
        .tempfile_in(directory)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(mode))?;
    temporary.persist(target).map_err(|error| error.error)?;
    Ok(())
}

fn read_utf8(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

// ── Lint advisories ──────────────────────────────────────────────────────────────────────────────

fn attach_lint(result: &mut Outcome, skill_md: &Path, before: Option<&str>, limit: usize) {
    let Ok(text) = fs::read(skill_md) else {
        return;
    };
    let text = String::from_utf8_lossy(&text);
    let text = text.trim_start_matches('\u{feff}');
    let dir = skill_md.parent();
    let mut findings: Vec<Finding> = lint_content(text, dir, limit);
    if let Some(before) = before {
        let standing: Vec<String> = lint_content(before, dir, limit)
            .into_iter()
            .map(|finding| finding.rule)
            .collect();
        findings.retain(|finding| !standing.contains(&finding.rule));
    }
    if findings.is_empty() {
        return;
    }
    result.insert(
        "lint_warnings".into(),
        Value::Array(
            findings
                .iter()
                .map(|f| json!({"severity": f.severity, "rule": f.rule, "message": f.message}))
                .collect(),
        ),
    );
    result.insert("lint_hint".into(), json!("The write succeeded. These are advisory authoring-convention findings (not blockers) — fix them with skill_manage(action='patch') to match Hermes skill standards."));
}

fn add_description_preview(result: &mut Outcome, content: &str, limit: usize) {
    let (frontmatter, _) = parse_frontmatter(content);
    let description = frontmatter
        .get("description")
        .filter(|value| !matches!(value, Yaml::Null | Yaml::Bool(false)))
        .map(py_str)
        .map(|text| text.trim().trim_matches(['\'', '"']).to_owned())
        .unwrap_or_default();
    if description.chars().count() > limit {
        result.insert(
            "system_prompt_preview".into(),
            json!(format!(
                "System prompt will show: \"{}\" — shorten the description to {limit} chars so the index shows all of it.",
                clip_description(&description, limit)
            )),
        );
    }
}

// ── Actions ──────────────────────────────────────────────────────────────────────────────────────

/// The flat arguments of one operation.
#[derive(Default, Clone)]
struct Op {
    action: String,
    name: String,
    content: Option<String>,
    category: Option<Value>,
    file_path: Option<String>,
    file_content: Option<String>,
    old_string: Option<String>,
    new_string: Option<String>,
    replace_all: bool,
    absorbed_into: Option<Value>,
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

impl Op {
    fn from_value(value: &Value, name: &str) -> Self {
        Self {
            action: text(value.get("action")).unwrap_or_default(),
            name: name.to_owned(),
            content: text(value.get("content")),
            category: value.get("category").filter(|v| !v.is_null()).cloned(),
            file_path: text(value.get("file_path")),
            file_content: text(value.get("file_content")),
            old_string: text(value.get("old_string")),
            new_string: text(value.get("new_string")),
            replace_all: value.get("replace_all").is_some_and(truthy),
            absorbed_into: value.get("absorbed_into").filter(|v| !v.is_null()).cloned(),
        }
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn text_slot_owner(key: &str) -> &'static str {
    match key {
        "content" => "create (and a full-rewrite patch)",
        "new_string" => "a targeted patch (with old_string)",
        _ => "write_file",
    }
}

fn misplaced_text_hint(op: &Op) -> String {
    let (reads, destination): (&[&str], &str) = match op.action.as_str() {
        "create" | "edit" => (&["content"], "'content'"),
        "patch" => (
            &["content", "new_string"],
            "old_string/new_string (targeted) or 'content' (full rewrite, last resort)",
        ),
        "write_file" => (&["file_content"], "'file_content'"),
        _ => return String::new(),
    };
    let present = |key: &str| match key {
        "content" => op.content.is_some(),
        "new_string" => op.new_string.is_some(),
        _ => op.file_content.is_some(),
    };
    let stray: Vec<&str> = ["content", "new_string", "file_content"]
        .into_iter()
        .filter(|key| !reads.contains(key) && present(key))
        .collect();
    if stray.is_empty() {
        return String::new();
    }
    let carried = stray
        .iter()
        .map(|key| format!("'{key}' (that key is for {})", text_slot_owner(key)))
        .collect::<Vec<_>>()
        .join(" and ");
    format!(" Note: this op carries {carried} — move that text to {destination}.")
}

/// Argument-shape error text for one op, or none.
fn op_shape_error(op: &Op) -> Option<String> {
    let missing = |value: &Option<String>| value.as_deref().is_none_or(str::is_empty);
    let hint = || misplaced_text_hint(op);
    match op.action.as_str() {
        "create" if missing(&op.content) => {
            return Some(format!("content is required for 'create'. Provide the full SKILL.md text (frontmatter + body).{}", hint()));
        }
        "edit" if missing(&op.content) => {
            return Some(format!(
                "content is required for a full rewrite. Provide the full updated SKILL.md text.{}",
                hint()
            ));
        }
        "write_file" => {
            if missing(&op.file_path) {
                return Some(format!(
                    "file_path is required for 'write_file'. Example: 'references/api-guide.md'{}",
                    hint()
                ));
            }
            if op.file_content.is_none() {
                return Some(format!(
                    "file_content is required for 'write_file'.{}",
                    hint()
                ));
            }
        }
        "remove_file" if missing(&op.file_path) => {
            return Some(format!(
                "file_path is required for 'remove_file'.{}",
                hint()
            ));
        }
        "patch" => {
            let has_content = !missing(&op.content);
            let has_old = !missing(&op.old_string);
            if has_content && (has_old || op.new_string.is_some()) {
                return Some(PATCH_EITHER_OR.into());
            }
            if !has_old && !has_content {
                return Some(format!("{PATCH_NEEDS_OLD_STRING}{}", hint()));
            }
            if !has_content && op.new_string.is_none() {
                return Some(PATCH_NEEDS_NEW_STRING.into());
            }
        }
        _ => {}
    }
    None
}

impl Skills<'_> {
    fn desc_limit(&self) -> usize {
        self.settings.index_desc_limit()
    }

    /// Read-before-write guard for the root SKILL.md budget, then the atomic write.
    fn guarded_write(&self, skill_dir: &Path, target: &Path, content: &str) -> Option<Outcome> {
        let original = if target.exists() {
            match read_utf8(target) {
                Ok(text) => Some(text),
                Err(error) => return Some(refusal(error)),
            }
        } else {
            None
        };
        if target == skill_dir.join("SKILL.md") {
            if let Some(error) =
                description_budget_error(content, original.as_deref(), self.desc_limit())
            {
                return Some(refusal(error));
            }
        }
        if let Err(error) = atomic_write(target, content) {
            return Some(refusal(format!(
                "Could not write {}: {error}",
                target.display()
            )));
        }
        None
    }

    fn create(&self, op: &Op) -> Outcome {
        let content = op.content.clone().unwrap_or_default();
        let category = op.category.as_ref();
        if let Some(error) = validate_name(&op.name)
            .or_else(|| validate_category(category))
            .or_else(|| validate_frontmatter(&content))
            .or_else(|| validate_content_size(&content, "SKILL.md"))
        {
            return refusal(error);
        }
        if let Some(existing) = self.find(&op.name) {
            return refusal(format!(
                "A skill named '{}' already exists at {}.",
                op.name,
                existing.display()
            ));
        }
        let category = category
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|category| !category.is_empty());
        let skill_dir = self.skill_dir_for(&op.name, category);
        if let Some(parent) = skill_dir.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                return refusal(format!("Could not create {}: {error}", parent.display()));
            }
        }
        if fs::create_dir(&skill_dir).is_err() {
            // Only an empty real directory (left by an earlier create) may be reused.
            let usable = !is_redirect(&skill_dir)
                && skill_dir.is_dir()
                && fs::read_dir(&skill_dir).is_ok_and(|mut entries| entries.next().is_none());
            if !usable {
                return refusal(format!(
                    "Cannot create skill '{}': {} already exists (not an empty directory, or unreadable). Choose another name, or move/remove that path and retry.",
                    op.name,
                    skill_dir.display()
                ));
            }
        }
        let skill_md = skill_dir.join("SKILL.md");
        if let Some(guard) = self.guarded_write(&skill_dir, &skill_md, &content) {
            let _ = fs::remove_dir(&skill_dir);
            return guard;
        }
        let root = self.local_root();
        let display = skill_dir.strip_prefix(&root).map_or_else(
            |_| skill_dir.display().to_string(),
            |p| p.display().to_string(),
        );
        let mut result = Outcome::new();
        result.insert("success".into(), json!(true));
        result.insert(
            "message".into(),
            json!(format!("Skill '{}' created.", op.name)),
        );
        result.insert("path".into(), json!(display));
        result.insert("skill_md".into(), json!(skill_md.display().to_string()));
        result.insert(
            "_change".into(),
            json!({"description": description_preview(&content)}),
        );
        if let Some(category) = category {
            result.insert("category".into(), json!(category));
        }
        result.insert("hint".into(), json!(format!(
            "To add reference files, templates, or scripts, use skill_manage(action='write_file', name='{}', file_path='references/example.md', file_content='...')",
            op.name
        )));
        add_description_preview(&mut result, &content, self.desc_limit());
        attach_lint(&mut result, &skill_md, None, self.desc_limit());
        result
    }

    fn locate(&self, name: &str, suffix: &str) -> Result<PathBuf, Outcome> {
        self.find(name)
            .ok_or_else(|| refusal(self.not_found(name, suffix)))
    }

    fn edit(&self, name: &str, content: &str) -> Outcome {
        if let Some(error) =
            validate_frontmatter(content).or_else(|| validate_content_size(content, "SKILL.md"))
        {
            return refusal(error);
        }
        let skill_dir = match self.locate(name, "") {
            Ok(dir) => dir,
            Err(error) => return error,
        };
        if let Some(guard) = self.guarded_write(&skill_dir, &skill_dir.join("SKILL.md"), content) {
            return guard;
        }
        let mut result = Outcome::new();
        result.insert("success".into(), json!(true));
        result.insert(
            "message".into(),
            json!(format!("Skill '{name}' updated (full rewrite).")),
        );
        result.insert("path".into(), json!(skill_dir.display().to_string()));
        result.insert(
            "_change".into(),
            json!({"description": description_preview(content)}),
        );
        add_description_preview(&mut result, content, self.desc_limit());
        result
    }

    fn patch(&self, op: &Op) -> Outcome {
        let Some(old) = op.old_string.as_deref().filter(|old| !old.is_empty()) else {
            return refusal(PATCH_NEEDS_OLD_STRING);
        };
        let Some(new) = op.new_string.as_deref() else {
            return refusal(PATCH_NEEDS_NEW_STRING);
        };
        let skill_dir = match self.locate(&op.name, "") {
            Ok(dir) => dir,
            Err(error) => return error,
        };
        let label = op.file_path.as_deref().filter(|path| !path.is_empty());
        let target = match label {
            Some(path) => match resolve_supporting_file(&skill_dir, path) {
                Ok(target) => target,
                Err(error) => return error,
            },
            None => skill_dir.join("SKILL.md"),
        };
        let target_label = label.unwrap_or("SKILL.md");
        if !target.exists() {
            let relative = target.strip_prefix(&skill_dir).unwrap_or(&target);
            return refusal(format!("File not found: {}", relative.display()));
        }
        let content = match read_utf8(&target) {
            Ok(text) => text,
            Err(error) => return refusal(error),
        };
        let mut notes: Vec<String> = Vec::new();
        let found = crate::fuzzy::find_and_replace(&content, old, new, op.replace_all, &mut notes);
        if let Some(error) = found.error {
            let hint = crate::fuzzy::no_match_hint(&error, found.count, old, &content);
            return failed(
                format!("{error}{hint}"),
                &[("file_preview", json!(clip(&content, 500, "...")))],
            );
        }
        let (updated, count) = (found.content, found.count);
        if let Some(error) = validate_content_size(&updated, target_label) {
            return refusal(error);
        }
        if label.is_none() {
            if let Some(error) = validate_frontmatter(&updated) {
                return refusal(format!("Patch would break SKILL.md structure: {error}"));
            }
        }
        if let Some(guard) = self.guarded_write(&skill_dir, &target, &updated) {
            return guard;
        }
        let mut result = Outcome::new();
        result.insert("success".into(), json!(true));
        result.insert(
            "message".into(),
            json!(format!(
                "Patched {target_label} in skill '{}' ({count} replacement{}).",
                op.name,
                if count > 1 { "s" } else { "" }
            )),
        );
        result.insert(
            "_change".into(),
            json!({"old": clip(old, 200, "…"), "new": clip(new, 200, "…")}),
        );
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("\n")));
        }
        if label.is_none() {
            attach_lint(&mut result, &target, Some(&content), self.desc_limit());
        }
        result
    }

    fn delete(&self, name: &str, absorbed_into: Option<&Value>) -> Outcome {
        let skill_dir = match self.locate(name, "") {
            Ok(dir) => dir,
            Err(error) => return error,
        };
        if ESSENTIAL_SKILLS.contains(&name) {
            return refusal(format!(
                "Skill '{name}' is essential to Hermes (the agent's own operating manual referenced by the system prompt) and cannot be deleted. Patches and edits are still allowed."
            ));
        }
        if self.pinned(name) {
            return refusal(format!(
                "Skill '{name}' is pinned and cannot be deleted by skill_manage. Ask the user to run `hermes curator unpin {name}` if they want to delete it. Patches and edits are allowed on pinned skills; only deletion is blocked."
            ));
        }
        let absorbed = absorbed_into
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !absorbed.is_empty() {
            if absorbed == name {
                return refusal(format!(
                    "absorbed_into='{absorbed}' cannot equal the skill being deleted."
                ));
            }
            if self.find(absorbed).is_none() {
                return refusal(format!(
                    "absorbed_into='{absorbed}' does not exist. Create or patch the umbrella skill first, then retry the delete."
                ));
            }
        }
        let resolved = match skill_dir.canonicalize() {
            Ok(resolved) => resolved,
            Err(error) => {
                return refusal(format!(
                    "Refusing to delete '{}': could not resolve path ({error}).",
                    skill_dir.display()
                ))
            }
        };
        if is_redirect(&skill_dir) {
            return refusal(format!(
                "Refusing to delete '{}': the skill directory is a symlink/junction. Remove the link target manually if intended.",
                skill_dir.display()
            ));
        }
        let mut containing: Option<PathBuf> = None;
        for root in self.roots() {
            let root = root.canonicalize().unwrap_or(root);
            if resolved == root {
                return refusal(format!(
                    "Refusing to delete '{}': resolves to the skills root itself, which would remove every installed skill.",
                    skill_dir.display()
                ));
            }
            if resolved.starts_with(&root) {
                containing = Some(root);
                break;
            }
        }
        let Some(skills_root) = containing else {
            return refusal(format!(
                "Refusing to delete '{}': path does not resolve inside any known skills root.",
                skill_dir.display()
            ));
        };
        if let Err(error) = fs::remove_dir_all(&skill_dir) {
            return refusal(format!("Could not delete {}: {error}", skill_dir.display()));
        }
        if let Some(parent) = skill_dir.parent() {
            let parent_resolved = parent.canonicalize().unwrap_or_else(|_| parent.to_owned());
            if parent_resolved != skills_root
                && parent.exists()
                && fs::read_dir(parent).is_ok_and(|mut entries| entries.next().is_none())
            {
                let _ = fs::remove_dir(parent);
            }
        }
        let note = if absorbed.is_empty() {
            String::new()
        } else {
            format!(" Content absorbed into '{absorbed}'.")
        };
        let mut result = Outcome::new();
        result.insert("success".into(), json!(true));
        result.insert(
            "message".into(),
            json!(format!("Skill '{name}' deleted.{note}")),
        );
        result
    }

    fn write_file(&self, op: &Op) -> Outcome {
        let file_path = op.file_path.clone().unwrap_or_default();
        if let Some(error) = validate_file_path(&file_path) {
            return refusal(error);
        }
        let Some(file_content) = op.file_content.as_deref() else {
            return refusal("file_content is required.");
        };
        let bytes = file_content.len();
        if bytes > MAX_SKILL_FILE_BYTES {
            return refusal(format!(
                "File content is {} bytes (limit: {} bytes / 1 MiB). Consider splitting into smaller files.",
                group_thousands(bytes),
                group_thousands(MAX_SKILL_FILE_BYTES)
            ));
        }
        if let Some(error) = validate_content_size(file_content, &file_path) {
            return refusal(error);
        }
        let skill_dir = match self.locate(&op.name, " Create it first with action='create'.") {
            Ok(dir) => dir,
            Err(error) => return error,
        };
        let target = match resolve_supporting_file(&skill_dir, &file_path) {
            Ok(target) => target,
            Err(error) => return error,
        };
        if let Some(guard) = self.guarded_write(&skill_dir, &target, file_content) {
            return guard;
        }
        let mut result = Outcome::new();
        result.insert("success".into(), json!(true));
        result.insert(
            "message".into(),
            json!(format!(
                "File '{file_path}' written to skill '{}'.",
                op.name
            )),
        );
        result.insert("path".into(), json!(target.display().to_string()));
        if file_path.starts_with("references/") && skill_dir.join("SKILL.md").exists() {
            attach_lint(
                &mut result,
                &skill_dir.join("SKILL.md"),
                None,
                self.desc_limit(),
            );
        }
        result
    }

    fn remove_file(&self, op: &Op) -> Outcome {
        let file_path = op.file_path.clone().unwrap_or_default();
        if let Some(error) = validate_file_path(&file_path) {
            return refusal(error);
        }
        let skill_dir = match self.locate(&op.name, "") {
            Ok(dir) => dir,
            Err(error) => return error,
        };
        let target = match resolve_supporting_file(&skill_dir, &file_path) {
            Ok(target) => target,
            Err(error) => return error,
        };
        if !target.exists() {
            let mut available: Vec<String> = Vec::new();
            for subdir in ALLOWED_SUBDIRS {
                let dir = skill_dir.join(subdir);
                if dir.exists() {
                    collect_files(&dir, &skill_dir, &mut available);
                }
            }
            let extra = if available.is_empty() {
                Value::Null
            } else {
                json!(available)
            };
            let mut map = refusal(format!(
                "File '{file_path}' not found in skill '{}'.",
                op.name
            ));
            if !extra.is_null() {
                map.insert("available_files".into(), extra);
            }
            return map;
        }
        if let Err(error) = fs::remove_file(&target) {
            return refusal(format!("Could not remove {}: {error}", target.display()));
        }
        if let Some(parent) = target.parent() {
            if parent != skill_dir
                && parent.exists()
                && fs::read_dir(parent).is_ok_and(|mut entries| entries.next().is_none())
            {
                let _ = fs::remove_dir(parent);
            }
        }
        let mut result = Outcome::new();
        result.insert("success".into(), json!(true));
        result.insert(
            "message".into(),
            json!(format!(
                "File '{file_path}' removed from skill '{}'.",
                op.name
            )),
        );
        result
    }
}

fn collect_files(dir: &Path, base: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, base, out);
        } else if let Ok(relative) = path.strip_prefix(base) {
            out.push(relative.display().to_string());
        }
    }
}

// ── One operation, then the batch ────────────────────────────────────────────────────────────────

impl Skills<'_> {
    /// The flat single-operation path: shape check, name check, lock check, the action.
    fn run_one(&self, op: &Op) -> Value {
        if let Some(locked) = self.lock_refusal(&op.name, &op.action) {
            return Value::Object(locked);
        }
        if let Some(error) = op_shape_error(op) {
            return shape_error(error);
        }
        let checked = if op.action == "create" || op.name.is_empty() {
            op.name.clone()
        } else {
            Path::new(&op.name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_owned()
        };
        if let Some(error) = validate_name(&checked) {
            return Value::Object(refusal(error));
        }
        let _guard = MUTATION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = match op.action.as_str() {
            "create" => self.create(op),
            "edit" => self.edit(&op.name, op.content.as_deref().unwrap_or("")),
            "patch" => {
                if op.content.as_deref().is_some_and(|c| !c.is_empty()) {
                    self.edit(&op.name, op.content.as_deref().unwrap_or(""))
                } else {
                    self.patch(op)
                }
            }
            "delete" => self.delete(&op.name, op.absorbed_into.as_ref()),
            "write_file" => self.write_file(op),
            "remove_file" => self.remove_file(op),
            other => refusal(format!(
                "Unknown action '{other}'. Use: create, edit, patch, delete, write_file, remove_file"
            )),
        };
        Value::Object(result)
    }
}

type Snapshot = (Option<PathBuf>, Option<PathBuf>, bool);

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(link, &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn normpath(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.trim_start_matches('/').split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

struct Applied {
    name: String,
    action: String,
    file_path: Option<String>,
}

impl Skills<'_> {
    fn restore(&self, snapshots: &[(String, Snapshot)], applied: &[Applied]) -> (String, bool) {
        let mut notes: Vec<String> = Vec::new();
        for (name, (pre_dir, snap, pre_existed)) in snapshots {
            let written: Vec<String> = applied
                .iter()
                .filter(|op| &op.name == name && op.action == "write_file")
                .filter_map(|op| op.file_path.as_deref().map(normpath))
                .collect();
            let post = self.find(name);
            let outcome = restore_snapshot(
                pre_dir.as_deref(),
                snap.as_deref(),
                post.as_deref(),
                *pre_existed,
                &written,
            );
            if let Err(error) = outcome {
                notes.push(format!(
                    "ROLLBACK FAILED for '{name}' ({error}){}",
                    snap.as_ref()
                        .map(|snap| format!("; snapshot preserved at '{}'", snap.display()))
                        .unwrap_or_default()
                ));
            }
        }
        if notes.is_empty() {
            ("all touched skills rolled back".into(), false)
        } else {
            (notes.join("; "), true)
        }
    }

    fn validate_batch(
        &self,
        operations: &[Value],
        default_name: Option<&str>,
    ) -> Result<Vec<String>, Value> {
        let fail = |index: usize, message: String| {
            Err(shape_error(format!("operations[{index}]{message}")))
        };
        let mut names: Vec<String> = Vec::new();
        for (index, op) in operations.iter().enumerate() {
            let action = op
                .get("action")
                .filter(|a| truthy(a))
                .and_then(Value::as_str);
            let Some(action) = action.filter(|_| op.is_object()) else {
                return fail(index, " needs an 'action'.".into());
            };
            if !matches!(action, "create" | "patch" | "write_file" | "remove_file") {
                return fail(
                    index,
                    format!(": unknown action '{action}'. Batchable: create, patch, remove_file, write_file; delete must be sole."),
                );
            }
            let name = text(op.get("name"))
                .filter(|name| !name.is_empty())
                .or_else(|| default_name.map(str::to_owned));
            let Some(name) = name else {
                return fail(index, " needs a 'name' (the skill it targets).".into());
            };
            let flat = Op::from_value(op, &name);
            if let Some(error) = op_shape_error(&flat) {
                return fail(index, format!(" ({action} on '{name}'): {error}"));
            }
            if action == "create" {
                if let Some(error) = validate_category(op.get("category")) {
                    return fail(index, format!(" ({action} on '{name}'): {error}"));
                }
            }
            names.push(name.clone());
            if action == "create" && names[..names.len() - 1].contains(&name) {
                return fail(
                    index,
                    format!(": create for '{name}' must precede that skill's other ops."),
                );
            }
            if let Some(locked) = self.lock_refusal(&name, action) {
                return Err(Value::Object(locked));
            }
        }
        let mut touched: Vec<(String, String)> = Vec::new();
        for (index, op) in operations.iter().enumerate() {
            let action = op["action"].as_str().unwrap_or("");
            let name = &names[index];
            let full_rewrite = action == "patch" && op.get("content").is_some_and(truthy);
            let file_path = text(op.get("file_path")).unwrap_or_default();
            let file_path = file_path.trim();
            let target = if action == "create" || full_rewrite || file_path.is_empty() {
                "SKILL.md".to_owned()
            } else {
                normpath(file_path)
            };
            let key = (name.clone(), target.clone());
            if (matches!(action, "create" | "write_file" | "remove_file") || full_rewrite)
                && touched.contains(&key)
            {
                return fail(
                    index,
                    format!(": {action} on '{target}' of skill '{name}' — an earlier op in this batch already touched that file, and this op would silently discard its work. One destructive op (write_file/remove_file/full rewrite) per file per batch; put it first, or fold the change in. Patch chains are fine."),
                );
            }
            touched.push(key);
        }
        Ok(names)
    }

    fn batch(&self, operations: &Value, default_name: Option<&str>) -> Value {
        let Some(list) = operations.as_array().filter(|list| !list.is_empty()) else {
            return shape_error("operations must be a non-empty array.");
        };
        if list.len() > BATCH_MAX_OPS {
            return shape_error(format!(
                "operations is capped at {BATCH_MAX_OPS} ops per call."
            ));
        }
        if list
            .iter()
            .any(|op| op.get("action").and_then(Value::as_str) == Some("delete"))
        {
            if list.len() != 1 {
                return shape_error("delete must be the SOLE op in its call — it doesn't compose with other ops' rollback.");
            }
            let name = text(list[0].get("name"))
                .filter(|name| !name.is_empty())
                .or_else(|| default_name.map(str::to_owned));
            let Some(name) = name else {
                return shape_error("operations[0] (delete) needs a 'name'.");
            };
            return self.run_one(&Op::from_value(&list[0], &name));
        }
        let names = match self.validate_batch(list, default_name) {
            Ok(names) => names,
            Err(error) => return error,
        };
        let _guard = MUTATION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Ok(snap_root) = tempfile::Builder::new().prefix("skill_batch_").tempdir() else {
            return shape_error("Could not snapshot for atomic batch: no temporary directory");
        };
        let mut snapshots: Vec<(String, Snapshot)> = Vec::new();
        let mut unique: Vec<&String> = Vec::new();
        for name in &names {
            if !unique.contains(&name) {
                unique.push(name);
            }
        }
        for name in unique {
            let pre = self.find(name);
            let snap = match pre.as_ref().filter(|dir| dir.is_dir()) {
                Some(dir) => {
                    let target = snap_root.path().join(name.replace('/', "_"));
                    if let Err(error) = copy_tree(dir, &target) {
                        return shape_error(format!(
                            "Could not snapshot '{name}' for atomic batch: {error}"
                        ));
                    }
                    Some(target)
                }
                None => None,
            };
            let create_target = if pre.is_none() {
                list.iter()
                    .zip(&names)
                    .find(|(op, n)| op["action"] == "create" && n == &name)
                    .map(|(op, n)| {
                        self.skill_dir_for(n, op.get("category").and_then(Value::as_str))
                    })
            } else {
                None
            };
            snapshots.push((
                name.clone(),
                (
                    pre,
                    snap,
                    create_target.is_some_and(|target| target.is_dir()),
                ),
            ));
        }
        let mut results: Vec<Applied> = Vec::new();
        let mut rows: Vec<Value> = Vec::new();
        for (index, op) in list.iter().enumerate() {
            let flat = Op::from_value(op, &names[index]);
            let parsed = self.run_one_unlocked(&flat);
            if parsed["success"].as_bool() != Some(true) {
                let (note, rollback_failed) = self.restore(&snapshots, &results);
                if rollback_failed {
                    // Keep the snapshots so the operator can still recover by hand.
                    let _ = snap_root.keep();
                }
                let mut fail = Map::new();
                fail.insert("success".into(), json!(false));
                fail.insert(
                    "error".into(),
                    json!(format!(
                        "operations[{index}] ({} on '{}') failed: {} — batch aborted, {note}.",
                        flat.action,
                        names[index],
                        parsed["error"].as_str().unwrap_or("unknown error")
                    )),
                );
                fail.insert("failed_index".into(), json!(index));
                fail.insert("completed_before_failure".into(), json!(index));
                if let Value::Object(map) = &parsed {
                    for (key, value) in map {
                        if key != "success" && key != "error" && !value.is_null() {
                            fail.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                }
                return Value::Object(fail);
            }
            let mut row = Map::new();
            row.insert("name".into(), json!(names[index]));
            row.insert("action".into(), json!(flat.action));
            row.insert(
                "file_path".into(),
                json!(op.get("file_path").filter(|v| !v.is_null())),
            );
            row.insert("success".into(), json!(true));
            for key in ["lint_warnings", "lint_hint"] {
                if let Some(value) = parsed.get(key).filter(|value| !value.is_null()) {
                    row.insert(key.into(), value.clone());
                }
            }
            rows.push(Value::Object(row));
            results.push(Applied {
                name: names[index].clone(),
                action: flat.action.clone(),
                file_path: flat.file_path.clone(),
            });
        }
        json!({"success": true, "operations_applied": rows.len(), "results": rows})
    }

    /// An operation of a batch: the same checks, with the mutation lock already held.
    fn run_one_unlocked(&self, op: &Op) -> Value {
        if let Some(locked) = self.lock_refusal(&op.name, &op.action) {
            return Value::Object(locked);
        }
        if let Some(error) = op_shape_error(op) {
            return shape_error(error);
        }
        if let Some(error) = validate_name(
            Path::new(&op.name)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|_| op.action != "create")
                .unwrap_or(&op.name),
        ) {
            return Value::Object(refusal(error));
        }
        let result = match op.action.as_str() {
            "create" => self.create(op),
            "patch" => {
                if op.content.as_deref().is_some_and(|c| !c.is_empty()) {
                    self.edit(&op.name, op.content.as_deref().unwrap_or(""))
                } else {
                    self.patch(op)
                }
            }
            "write_file" => self.write_file(op),
            "remove_file" => self.remove_file(op),
            other => refusal(format!("Unknown action '{other}'.")),
        };
        Value::Object(result)
    }
}

fn restore_snapshot(
    pre_dir: Option<&Path>,
    snap: Option<&Path>,
    post_dir: Option<&Path>,
    pre_existed: bool,
    written: &[String],
) -> std::io::Result<()> {
    let post_exists = post_dir.is_some_and(Path::is_dir);
    let Some(snap) = snap else {
        let Some(post) = post_dir.filter(|_| post_exists) else {
            return Ok(());
        };
        if !pre_existed {
            return fs::remove_dir_all(post);
        }
        // The directory predates the batch: remove exactly the files the batch wrote.
        for relative in std::iter::once("SKILL.md").chain(written.iter().map(String::as_str)) {
            let target = post.join(relative);
            let _ = fs::remove_file(&target);
            let mut parent = target.parent();
            while let Some(dir) = parent {
                if dir == post || !dir.starts_with(post) {
                    break;
                }
                let _ = fs::remove_dir(dir);
                parent = dir.parent();
            }
        }
        let _ = fs::remove_dir(post);
        return Ok(());
    };
    let Some(pre) = pre_dir else {
        return Ok(());
    };
    let Some(post) = post_dir.filter(|_| post_exists) else {
        return copy_tree(snap, pre);
    };
    // Move the broken state aside and delete it only once the snapshot is back.
    let aside = post.with_file_name(format!(
        "{}.rollback-broken",
        post.file_name().and_then(|n| n.to_str()).unwrap_or("skill")
    ));
    let _ = fs::remove_dir_all(&aside);
    fs::rename(post, &aside)?;
    if let Err(error) = copy_tree(snap, pre) {
        let _ = fs::remove_dir_all(pre);
        fs::rename(&aside, pre)?;
        return Err(error);
    }
    let _ = fs::remove_dir_all(&aside);
    Ok(())
}

// ── The tool ─────────────────────────────────────────────────────────────────────────────────────

/// A readable account of what the call will change, for the person who approves it.
fn describe_changes(args: &Value) -> String {
    let render = |op: &Value, default_name: Option<&str>| -> String {
        let name = text(op.get("name"))
            .or_else(|| default_name.map(str::to_owned))
            .unwrap_or_default();
        let action = text(op.get("action")).unwrap_or_default();
        let mut lines = vec![format!("{action} skill '{name}'")];
        if let Some(path) = text(op.get("file_path")) {
            lines.push(format!("  file: {path}"));
        }
        if let Some(category) = text(op.get("category")) {
            lines.push(format!("  category: {category}"));
        }
        for (key, label) in [
            ("content", "SKILL.md"),
            ("file_content", "file content"),
            ("old_string", "replace"),
            ("new_string", "with"),
            ("absorbed_into", "absorbed into"),
        ] {
            if let Some(value) = text(op.get(key)) {
                lines.push(format!("  {label}:\n{}", clip(&value, 6000, "\n  … (cut)")));
            }
        }
        lines.join("\n")
    };
    match args.get("operations").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .map(|op| render(op, text(args.get("name")).as_deref()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        None => render(args, None),
    }
}

pub(crate) async fn call<A, F>(
    skills: &Skills<'_>,
    arguments: &str,
    approval_timeout: Option<u32>,
    approve: &mut A,
) -> Value
where
    A: FnMut(ApprovalRequest) -> F,
    F: std::future::Future<Output = ApprovalDecision>,
{
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    if skills.settings.guard_agent_created() {
        return json!({"success": false, "error": "skills.guard_agent_created is on, which needs the skill security scan; this CLI cannot run it, so skill_manage refuses instead of writing unchecked text."});
    }
    if skills.settings.write_approval() {
        return json!({"success": false, "error": "skills.write_approval is on, which stages skill writes for later approval; this CLI cannot stage them, so skill_manage refuses."});
    }
    let operations = args.get("operations").filter(|value| !value.is_null());
    // Call shapes that fail before anything is written need no approval.
    if let Some(operations) = operations {
        if operations.as_array().is_none_or(|list| list.is_empty()) {
            return shape_error("operations must be a non-empty array.");
        }
    }
    let Some(timeout) = approval_timeout else {
        return approval_error(ApprovalDecision::Unavailable);
    };
    let decision = request_approval(
        ApprovalRequest {
            target: skills.local_root().display().to_string(),
            content: describe_changes(&args),
            action: ApprovalAction::ManageSkills,
            timeout_secs: timeout,
        },
        approve,
    )
    .await;
    if !matches!(decision, ApprovalDecision::Approved) {
        return approval_error(decision);
    }
    let default_name = text(args.get("name")).filter(|name| !name.is_empty());
    match operations {
        Some(operations) => skills.batch(operations, default_name.as_deref()),
        None => {
            let name = default_name.unwrap_or_default();
            skills.run_one(&Op::from_value(&args, &name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible_tree(root: &Path) -> serde_json::Map<String, Value> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let relative = path.strip_prefix(base).unwrap().to_owned();
                if relative
                    .components()
                    .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
                {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, base, out);
                } else if let Ok(text) = fs::read_to_string(&path) {
                    out.push((relative.to_string_lossy().replace('\\', "/"), text));
                }
            }
        }
        let mut files = Vec::new();
        walk(root, root, &mut files);
        files.sort();
        files.into_iter().map(|(k, v)| (k, json!(v))).collect()
    }

    fn normalize(value: &Value, roots: &[String]) -> Value {
        match value {
            Value::String(text) => {
                let mut text = text.clone();
                for root in roots {
                    text = text.replace(root, "<skills>");
                }
                json!(text)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|i| normalize(i, roots)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let mut value = normalize(v, roots);
                        // The reference lists these in set-iteration order.
                        if k == "available_files" {
                            if let Value::Array(items) = &mut value {
                                items.sort_by_key(|item| item.as_str().unwrap_or("").to_owned());
                            }
                        }
                        (k.clone(), value)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    #[tokio::test]
    async fn results_and_files_match_the_reference() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/skill_manage_cases.json"))
                .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let home = tempfile::tempdir().unwrap();
            let skills_dir = home.path().join("skills");
            for (relative, text) in case["seed"].as_object().unwrap() {
                let target = skills_dir.join(relative);
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(target, text.as_str().unwrap()).unwrap();
            }
            fs::create_dir_all(&skills_dir).unwrap();
            let config: Yaml =
                serde_yaml::from_str(case["config"].as_str().unwrap()).unwrap_or(Yaml::Null);
            let settings = SkillSettings::from_config(&config["skills"]);
            let skills = Skills {
                home: home.path(),
                settings: &settings,
            };
            let roots = vec![
                skills_dir.display().to_string(),
                skills_dir.canonicalize().unwrap().display().to_string(),
            ];
            for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                let mut approve = |_request: ApprovalRequest| async { ApprovalDecision::Approved };
                let result = call(
                    &skills,
                    &step["arguments"].to_string(),
                    Some(5),
                    &mut approve,
                )
                .await;
                let result = normalize(&result, &roots);
                assert_eq!(
                    result,
                    normalize(&step["result"], &roots),
                    "{name} #{index}: {}",
                    step["arguments"]
                );
                let tree = Value::Object(visible_tree(&skills_dir));
                assert_eq!(
                    tree, step["tree"],
                    "{name} #{index}: files after {}",
                    step["arguments"]
                );
            }
        }
    }

    #[test]
    fn the_wider_description_matches_the_reference() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/skill_manage_cases.json"))
                .unwrap();
        assert_eq!(
            schema(120)["function"]["description"],
            fixture["wide_description"]
        );
        assert_eq!(
            schema(60)["function"]["description"],
            json!(description(60))
        );
    }
}
