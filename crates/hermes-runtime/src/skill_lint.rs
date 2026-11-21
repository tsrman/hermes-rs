//! Advisory authoring checks for SKILL.md files, as `tools/skill_linter.py` runs them after a write.
//! Findings never block anything; `skill_manage` attaches them to its result.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_yaml::{Mapping, Value as Yaml};

const SHELL_UTILITIES: [(&str, &str); 9] = [
    ("grep", "search_files"),
    ("rg", "search_files"),
    ("cat", "read_file"),
    ("head", "read_file"),
    ("tail", "read_file"),
    ("sed", "patch"),
    ("awk", "patch"),
    ("find", "search_files (target='files')"),
    ("ls", "search_files (target='files')"),
];
const MARKETING_WORDS: [&str; 8] = [
    "powerful",
    "comprehensive",
    "seamless",
    "advanced",
    "cutting-edge",
    "state-of-the-art",
    "revolutionary",
    "robust",
];
const POSIX_PRIMITIVES: [&str; 8] = [
    "fcntl",
    "termios",
    "os.setsid",
    "signal.SIGKILL",
    "osascript",
    "/proc/",
    "apt-get",
    "systemctl",
];
const FORBIDDEN_FILES: [&str; 6] = [
    "README.md",
    "CHANGELOG.md",
    "install.sh",
    ".env",
    ".env.example",
    ".gitignore",
];
const EXPECTED_SECTIONS: [&str; 2] = ["When to Use", "When to use"];
const INCIDENT_REF_MIN: usize = 4;
const INCIDENT_REF_PER_KCHAR: f64 = 0.5;
const MAX_REFERENCE_FILES: usize = 60;
const BODY_SOFT_BUDGET_CHARS: usize = 24_000;

pub(crate) struct Finding {
    pub severity: &'static str,
    pub rule: String,
    pub message: String,
}

fn error(rule: &str, message: String) -> Finding {
    Finding {
        severity: "error",
        rule: rule.into(),
        message,
    }
}

fn warning(rule: &str, message: String) -> Finding {
    Finding {
        severity: "warning",
        rule: rule.into(),
        message,
    }
}

static FRONTMATTER_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n---\s*\n").expect("valid regex"));

/// `agent.skill_utils.parse_frontmatter`: the YAML mapping and the body. Malformed YAML falls back
/// to splitting `key: value` lines.
pub(crate) fn parse_frontmatter(content: &str) -> (Mapping, String) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let end = content
        .strip_prefix("---")
        .and_then(|after| FRONTMATTER_END.find(after));
    let Some(end) = end else {
        return (Mapping::new(), content.to_owned());
    };
    let yaml = &content[3..end.start() + 3];
    let body = content[end.end() + 3..].to_owned();
    let frontmatter = match serde_yaml::from_str::<Yaml>(yaml) {
        Ok(Yaml::Mapping(map)) => map,
        Ok(_) => Mapping::new(),
        Err(_) => {
            let mut map = Mapping::new();
            for line in yaml.trim().split('\n') {
                if let Some((key, value)) = line.split_once(':') {
                    map.insert(
                        Yaml::String(key.trim().to_owned()),
                        Yaml::String(value.trim().to_owned()),
                    );
                }
            }
            map
        }
    };
    (frontmatter, body)
}

fn py_str(value: Option<&Yaml>) -> String {
    match value {
        None | Some(Yaml::Null) => String::new(),
        Some(Yaml::Bool(true)) => "True".into(),
        Some(Yaml::Bool(false)) => "False".into(),
        Some(Yaml::Number(number)) => number.to_string(),
        Some(Yaml::String(text)) => text.clone(),
        Some(other) => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    }
}

fn word_regex(word: &str) -> Regex {
    Regex::new(&format!(r"\b{}\b", regex::escape(word))).expect("valid regex")
}

fn check_frontmatter(
    frontmatter: &Mapping,
    skill_dir: Option<&Path>,
    limit: usize,
    out: &mut Vec<Finding>,
) {
    static NAME_FORMAT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]*$").expect("valid regex"));
    let name = py_str(frontmatter.get("name")).trim().to_owned();
    if !name.is_empty() && !NAME_FORMAT.is_match(&name) {
        out.push(error(
            "name-format",
            format!(
                "name '{name}' must be lowercase letters, digits, hyphens, and underscores only."
            ),
        ));
    }
    if let Some(dir) = skill_dir {
        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.is_empty() && name != dir_name {
            out.push(error(
                "name-dir-mismatch",
                format!("frontmatter name '{name}' does not match directory '{dir_name}'; they must be identical."),
            ));
        }
    }
    let description = py_str(frontmatter.get("description"));
    let description = description.trim().trim_matches(['\'', '"']);
    let length = description.chars().count();
    if length > limit {
        out.push(warning(
            "description-length",
            format!("description is {length} chars; the skill index shows {limit} and clips the rest, losing routing signal. Keep it to one sentence."),
        ));
    }
    let lowered = description.to_lowercase();
    let hits: Vec<&str> = MARKETING_WORDS
        .iter()
        .copied()
        .filter(|word| word_regex(word).is_match(&lowered))
        .collect();
    if !hits.is_empty() {
        let shown = hits
            .iter()
            .map(|word| format!("'{word}'"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(warning(
            "description-marketing",
            format!("description contains marketing words [{shown}]; state the capability, not adjectives."),
        ));
    }
    for key in ["version", "author", "license"] {
        if !frontmatter.contains_key(key) {
            out.push(warning(
                "missing-metadata",
                format!("frontmatter is missing '{key}'; every peer skill has it."),
            ));
        }
    }
    let hermes = frontmatter
        .get("metadata")
        .and_then(Yaml::as_mapping)
        .and_then(|meta| meta.get("hermes"))
        .and_then(Yaml::as_mapping);
    match hermes {
        None => out.push(warning(
            "missing-metadata",
            "frontmatter is missing metadata.hermes.{tags, related_skills}.".into(),
        )),
        Some(hermes) if !hermes.contains_key("tags") => {
            out.push(warning(
                "missing-metadata",
                "metadata.hermes.tags is missing.".into(),
            ));
        }
        Some(_) => {}
    }
    let author = py_str(frontmatter.get("author"));
    if !author.is_empty()
        && matches!(
            author.trim().to_lowercase().as_str(),
            "hermes" | "agent" | "hermes agent"
        )
        && author != "Hermes Agent"
    {
        out.push(warning(
            "author-caps",
            format!("author '{author}' should be 'Hermes Agent' (proper caps) or a real contributor name."),
        ));
    }
    if let Some(platforms) = frontmatter
        .get("platforms")
        .filter(|value| !matches!(value, Yaml::Null | Yaml::Bool(false)))
        .filter(|value| value.as_sequence().is_none_or(|items| !items.is_empty()))
    {
        let items: Vec<&Yaml> = match platforms {
            Yaml::Sequence(items) => items.iter().collect(),
            other => vec![other],
        };
        let bad: Vec<String> = items
            .iter()
            .filter(|item| {
                !matches!(
                    py_str(Some(item)).to_lowercase().as_str(),
                    "linux" | "macos" | "windows" | "darwin"
                )
            })
            .map(|item| match item {
                Yaml::String(text) => format!("'{text}'"),
                other => py_str(Some(other)),
            })
            .collect();
        if !bad.is_empty() {
            out.push(warning(
                "platforms-value",
                format!(
                    "platforms contains unrecognized value(s) [{}]; expected a subset of ['darwin', 'linux', 'macos', 'windows'].",
                    bad.join(", ")
                ),
            ));
        }
    }
}

fn strip_code_blocks(body: &str) -> String {
    static FENCE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)```.*?```").expect("valid regex"));
    FENCE.replace_all(body, "").into_owned()
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

fn check_body(body: &str, skill_dir: Option<&Path>, out: &mut Vec<Finding>) {
    let length = body.chars().count();
    if length > BODY_SOFT_BUDGET_CHARS {
        out.push(warning(
            "oversized-body",
            format!(
                "SKILL.md body is {} chars (~{} tokens); skill_view loads all of it and it stays in context for every later call of the session. Keep the always-on rules here (~200 lines) and move topic depth into references/<topic>.md, linked from the body.",
                group_thousands(length),
                group_thousands(length / 4)
            ),
        ));
    }
    let prose = strip_code_blocks(body);
    for (utility, tool) in SHELL_UTILITIES {
        if prose.contains(&format!("`{utility}`")) {
            out.push(warning(
                "shell-utility-reference",
                format!("prose references `{utility}`; name the native tool `{tool}` instead."),
            ));
        }
    }
    let has_section = EXPECTED_SECTIONS.iter().any(|section| {
        Regex::new(&format!(r"(?m)^#+\s+{}", regex::escape(section)))
            .expect("valid regex")
            .is_match(body)
    });
    if !has_section {
        out.push(warning(
            "missing-section",
            "no '## When to Use' section found; skills need explicit trigger conditions near the top.".into(),
        ));
    }
    static REFS: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?<![\w/])#\d{3,6}\b|\b(?:PR|issue)\s*#?\d{3,6}\b")
            .expect("valid regex")
    });
    let refs = REFS.find_iter(&prose).flatten().count();
    let prose_length = prose.chars().count().max(1);
    if refs >= INCIDENT_REF_MIN
        && refs as f64 / prose_length as f64 * 1000.0 >= INCIDENT_REF_PER_KCHAR
    {
        out.push(warning(
            "incident-log-shape",
            format!("{refs} PR/issue references in prose; write the generalizable rule + why and drop the incident numbers — the rule must stand without the story."),
        ));
    }
    let Some(dir) = skill_dir else {
        return;
    };
    static LINKS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(references|templates|assets)/[\w./-]+").expect("valid regex")
    });
    let mut seen: Vec<String> = Vec::new();
    for found in LINKS.find_iter(body) {
        let relative = found.as_str();
        if seen.iter().any(|s| s == relative) || relative.contains('*') || relative.ends_with('/') {
            continue;
        }
        seen.push(relative.to_owned());
        if !dir.join(relative).exists() {
            out.push(warning(
                "dangling-reference",
                format!("body references '{relative}' but that file does not exist in the skill directory."),
            ));
        }
    }
}

fn walk_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn check_files(frontmatter: &Mapping, skill_dir: &Path, out: &mut Vec<Finding>) {
    let scripts = skill_dir.join("scripts");
    let declared = frontmatter.get("platforms").is_some_and(|value| {
        !matches!(value, Yaml::Null | Yaml::Bool(false))
            && value.as_sequence().is_none_or(|i| !i.is_empty())
    });
    let mut offenders: Vec<(String, Vec<&str>)> = Vec::new();
    if !declared && scripts.is_dir() {
        let mut files = Vec::new();
        walk_files(&scripts, &mut files);
        for script in files {
            let extension = script.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(extension, "py" | "sh" | "bash") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&script) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let hits: Vec<&str> = POSIX_PRIMITIVES
                .iter()
                .copied()
                .filter(|primitive| text.contains(primitive))
                .collect();
            if !hits.is_empty() {
                let name = script
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_owned();
                offenders.push((name, hits));
            }
        }
    }
    if !offenders.is_empty() {
        let detail = offenders
            .iter()
            .map(|(name, hits)| {
                format!(
                    "{name}: [{}]",
                    hits.iter()
                        .map(|hit| format!("'{hit}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        out.push(warning(
            "platforms-gating",
            format!("scripts use POSIX-only primitives ({detail}) but no 'platforms:' frontmatter is declared. Fix cross-platform or gate with platforms: [linux, macos]."),
        ));
    }
    for file in FORBIDDEN_FILES {
        if skill_dir.join(file).exists() {
            out.push(warning(
                "forbidden-file",
                format!(
                    "skill ships '{file}'; skills should not include scaffolding/config files."
                ),
            ));
        }
    }
    let references = skill_dir.join("references");
    if references.is_dir() {
        let mut files = Vec::new();
        walk_files(&references, &mut files);
        let count = files
            .iter()
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
            .filter(|path| {
                !path
                    .strip_prefix(skill_dir)
                    .unwrap_or(path)
                    .components()
                    .any(|part| part.as_os_str().to_string_lossy().starts_with('_'))
            })
            .count();
        if count > MAX_REFERENCE_FILES {
            out.push(warning(
                "references-sprawl",
                format!("{count} files under references/; that is a per-session log, not topical depth. Merge same-topic files into one rule set and drop incident narration."),
            ));
        }
    }
}

/// Lint raw SKILL.md content; with `skill_dir`, the on-disk checks run too.
pub(crate) fn lint_content(content: &str, skill_dir: Option<&Path>, limit: usize) -> Vec<Finding> {
    let (frontmatter, body) = parse_frontmatter(content);
    let mut out = Vec::new();
    check_frontmatter(&frontmatter, skill_dir, limit, &mut out);
    check_body(&body, skill_dir, &mut out);
    if let Some(dir) = skill_dir {
        check_files(&frontmatter, dir, &mut out);
    }
    out
}
