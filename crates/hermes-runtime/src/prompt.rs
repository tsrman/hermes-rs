//! Profile-scoped and project-scoped parts of the saved CLI system prompt.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use hermes_core::prompt::CliPromptContext;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::config::truthy;
use crate::skills::SkillSettings;

mod patterns;
pub(crate) mod threats;

const FILE_LIMIT: u64 = 1_048_576;
const MEMORY_SEPARATOR: &str = "\n§\n";
const AGENT_FILES: &[&str] = &["AGENTS.override.md", "AGENTS.md", "agents.md"];

#[derive(Clone, Deserialize, Serialize)]
pub struct PromptSettings {
    context_file_max_chars: usize,
    memory_enabled: bool,
    user_profile_enabled: bool,
    memory_char_limit: usize,
    user_char_limit: usize,
    #[serde(default)]
    pub(crate) skills: SkillSettings,
}

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            context_file_max_chars: 20_000,
            memory_enabled: true,
            user_profile_enabled: true,
            memory_char_limit: 2_200,
            user_char_limit: 1_375,
            skills: SkillSettings::default(),
        }
    }
}

impl PromptSettings {
    pub(crate) fn from_config(config: &Value) -> Self {
        let defaults = Self::default();
        let memory = &config["memory"];
        Self {
            context_file_max_chars: count(&config["context_file_max_chars"])
                .filter(|value| *value > 0)
                .unwrap_or(defaults.context_file_max_chars),
            memory_enabled: truthy(&memory["memory_enabled"], true),
            user_profile_enabled: truthy(&memory["user_profile_enabled"], true),
            memory_char_limit: count(&memory["memory_char_limit"])
                .unwrap_or(defaults.memory_char_limit),
            user_char_limit: count(&memory["user_char_limit"]).unwrap_or(defaults.user_char_limit),
            skills: SkillSettings::from_config(&config["skills"]),
        }
    }
}

fn count(value: &Value) -> Option<usize> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .and_then(|number| usize::try_from(number).ok())
}

pub(crate) fn load_cli_context(
    profile: Option<&Path>,
    root: &Path,
    settings: &PromptSettings,
    skills_prompt: Option<String>,
) -> CliPromptContext {
    let profile = profile.and_then(|path| path.canonicalize().ok());
    let cap = settings.context_file_max_chars;
    let identity = profile.as_deref().and_then(|profile| {
        let path = profile.join("SOUL.md");
        let mut text = read_prompt_file(&path, profile, false)?;
        if !user_owned_soul(profile) {
            text = guarded(&text, "SOUL.md");
        }
        Some(truncate(&text, "SOUL.md", &path, cap))
    });
    let mut sections = Vec::new();
    if let Some(context) = project_context(root, cap) {
        sections.push(context);
    }
    sections.extend(skills_prompt);
    if let Some(profile) = &profile {
        for (enabled, filename, title, budget) in [
            (
                settings.memory_enabled,
                "MEMORY.md",
                "MEMORY (your personal notes)",
                settings.memory_char_limit,
            ),
            (
                settings.user_profile_enabled,
                "USER.md",
                "USER PROFILE (who the user is)",
                settings.user_char_limit,
            ),
        ] {
            if enabled {
                if let Some(memory) = memory_block(profile, filename, title, budget) {
                    sections.push(memory);
                }
            }
        }
    }
    CliPromptContext { identity, sections }
}

fn user_owned_soul(profile: &Path) -> bool {
    let path = profile.join("distribution.yaml");
    if fs::symlink_metadata(&path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return true;
    }
    let manifest = read_prompt_file(&path, profile, false)
        .and_then(|text| serde_yaml::from_str::<Value>(&text).ok());
    manifest
        .as_ref()
        .and_then(|manifest| manifest["distribution_owned"].as_sequence())
        .is_some_and(|files| {
            !files.is_empty()
                && files.iter().all(|file| {
                    file.as_str()
                        .is_some_and(|name| name.trim().trim_matches('/') != "SOUL.md")
                })
        })
}

fn read_prompt_file(path: &Path, root: &Path, allow_markdown_link: bool) -> Option<String> {
    let text = read_scoped_text(path, root, allow_markdown_link)?;
    let text = normalize_text(&text);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub(crate) fn read_scoped_text(
    path: &Path,
    root: &Path,
    allow_markdown_link: bool,
) -> Option<String> {
    let resolved = path.canonicalize().ok()?;
    let relative = resolved.strip_prefix(root).ok()?;
    // Automatic project context can follow in-tree Markdown links, including shared
    // documentation. Profile files must resolve to their own fixed locations.
    if resolved != path
        && (!allow_markdown_link
            || !resolved.extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdc")
            })
            || relative.components().any(|part| {
                [
                    ".git",
                    ".ssh",
                    ".aws",
                    "vault",
                    "mcp-tokens",
                    "browser-profile",
                ]
                .iter()
                .any(|name| part.as_os_str().eq_ignore_ascii_case(name))
            }))
    {
        return None;
    }
    let metadata = resolved.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > FILE_LIMIT {
        return None;
    }
    let mut text = String::new();
    let file = File::open(resolved).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let bytes = file.take(FILE_LIMIT + 1).read_to_string(&mut text).ok()? as u64;
    if bytes > FILE_LIMIT {
        return None;
    }
    Some(text)
}

pub(crate) fn normalize_text(text: &str) -> String {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn project_context(root: &Path, cap: usize) -> Option<String> {
    let cwd = root.canonicalize().ok()?;
    let git_root = cwd.ancestors().find(|dir| dir.join(".git").exists());
    let root = git_root.unwrap_or(&cwd);
    let home = std::env::home_dir().and_then(|dir| dir.canonicalize().ok());
    let broad_root = home.as_ref().is_none_or(|home| home.starts_with(root));
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir);
        if dir == root {
            break;
        }
    }
    let containment = |dir| if broad_root { dir } else { root };

    let explicit = dirs.iter().find_map(|dir| {
        [".hermes.md", "HERMES.md"]
            .into_iter()
            .map(|name| dir.join(name))
            .find(|file| file.is_file())
    });
    let mut sections = Vec::new();
    if let Some(path) = explicit {
        let boundary = if broad_root { path.parent()? } else { root };
        if let Some(text) = read_prompt_file(&path, boundary, true) {
            let label = path.file_name()?.to_string_lossy();
            sections.push(context_section(
                strip_frontmatter(&text),
                &label,
                &path,
                cap,
            ));
        }
    }
    if sections.is_empty() {
        let mut seen = HashSet::new();
        for (depth, dir) in dirs.iter().enumerate().rev() {
            if let Some((name, path, text)) = first_context_file(dir, containment(dir), AGENT_FILES)
            {
                if seen.insert(text.clone()) {
                    let label = format!("{}{}", "../".repeat(depth), name);
                    sections.push(context_section(&text, &label, &path, cap));
                }
            }
        }
        if sections.len() > 1 {
            sections = vec![truncate(
                &sections.join("\n\n"),
                "AGENTS.md (directory chain)",
                &cwd.join("AGENTS.md"),
                cap,
            )];
        }
    }
    if sections.is_empty() {
        if let Some((name, path, text)) =
            first_context_file(&cwd, containment(&cwd), &["CLAUDE.md", "claude.md"])
        {
            sections.push(context_section(&text, name, &path, cap));
        } else {
            sections.extend(cursor_context(&cwd, containment(&cwd), cap));
        }
    }
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "# Project Context\n\nThe following project context files have been loaded and should be followed:\n\n{}",
        sections.join("\n")
    ))
}

fn first_context_file<'a>(
    directory: &Path,
    root: &Path,
    names: &'a [&'a str],
) -> Option<(&'a str, PathBuf, String)> {
    names.iter().find_map(|&name| {
        let path = directory.join(name);
        let text = read_prompt_file(&path, root, true)?;
        Some((name, path, text))
    })
}

fn cursor_context(cwd: &Path, root: &Path, cap: usize) -> Option<String> {
    let mut files = vec![(String::from(".cursorrules"), cwd.join(".cursorrules"))];
    let rules = cwd.join(".cursor/rules");
    if let Some(entries) = rules
        .canonicalize()
        .ok()
        .filter(|path| path.starts_with(root))
        .and_then(|path| fs::read_dir(path).ok())
    {
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "mdc") {
                let label = format!(".cursor/rules/{}", entry.file_name().to_string_lossy());
                files.push((label, path));
            }
        }
    }
    let content = files
        .into_iter()
        .filter_map(|(label, path)| {
            let text = read_prompt_file(&path, root, true)?;
            Some(format!("## {label}\n\n{}\n\n", guarded(&text, &label)))
        })
        .collect::<String>();
    (!content.is_empty())
        .then(|| truncate(&content, ".cursorrules", &cwd.join(".cursorrules"), cap))
}

fn strip_frontmatter(text: &str) -> &str {
    text.strip_prefix("---\n")
        .and_then(|text| text.split_once("\n---"))
        .map_or(text, |(_, body)| {
            let body = body.trim_start_matches('\n');
            if body.is_empty() {
                text
            } else {
                body
            }
        })
}

fn context_section(text: &str, label: &str, file: &Path, cap: usize) -> String {
    let body = guarded(text, label);
    truncate(&format!("## {label}\n\n{body}"), label, file, cap)
}

fn guarded(text: &str, label: &str) -> String {
    let hits = threats::findings(text, false);
    if hits.is_empty() {
        text.to_owned()
    } else {
        format!(
            "[BLOCKED: {label} contained potential prompt injection ({}). Content not loaded.]",
            hits.join(", ")
        )
    }
}

fn truncate(text: &str, label: &str, path: &Path, cap: usize) -> String {
    let len = text.chars().count();
    if len <= cap {
        return text.to_owned();
    }
    let head = cap * 7 / 10;
    let tail = cap / 5;
    let head_byte = text
        .char_indices()
        .nth(head)
        .map_or(text.len(), |(byte, _)| byte);
    let tail_byte = text
        .char_indices()
        .nth(len - tail)
        .map_or(text.len(), |(byte, _)| byte);
    format!(
        "{}\n\n[...truncated {label}: kept {head}+{tail} of {len} chars. The middle is omitted. Full file: {}]\n\n{}",
        &text[..head_byte],
        path.display(),
        &text[tail_byte..]
    )
}

fn memory_block(profile: &Path, file: &str, title: &str, limit: usize) -> Option<String> {
    let text = read_prompt_file(&profile.join("memories").join(file), profile, false)?;
    let mut seen = HashSet::new();
    let entries = text
        .split(MEMORY_SEPARATOR)
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && seen.insert(*entry))
        .map(|entry| {
            let hits = threats::findings(entry, true);
            if hits.is_empty() {
                entry.to_owned()
            } else {
                format!(
                    "[BLOCKED: {file} entry contained threat pattern(s): {}.]",
                    hits.join(", ")
                )
            }
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    let content = entries.join(MEMORY_SEPARATOR);
    let chars = content.chars().count();
    let usage = (chars * 100).checked_div(limit).unwrap_or(0).min(100);
    let divider = "═".repeat(46);
    Some(format!(
        "{divider}\n{title} [{usage}% — {}/{} chars]\n{divider}\n{content}",
        grouped_count(chars),
        grouped_count(limit),
    ))
}

fn grouped_count(count: usize) -> String {
    let digits = count.to_string();
    let mut grouped = String::new();
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}
