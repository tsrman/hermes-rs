//! Chat input meta-features: `@` context references (`@file:`, `@folder:`, `@diff`, `@staged`,
//! `@git:N`, `@url:`), workspace `@` completion, `/<skill-name>` skill slash-command lookup,
//! and `!<command>` local shell escape.

use crate::paths::PathExt;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::redact::{is_secret_file_arg, redact_file_read};
use crate::url_safety;
use crate::website_policy::WebsitePolicy;

const DEFAULT_CONTEXT_TOKENS: usize = 256_000;
const CHARS_PER_TOKEN: usize = 4;
const MAX_SINGLE_FILE_BYTES: usize = 512 * 1024;
const MAX_URL_BYTES: usize = 512 * 1024;
const MAX_URL_CHARS: usize = 40_000;
const MAX_FOLDER_DEPTH: usize = 4;
const MAX_FOLDER_ENTRIES: usize = 200;
const MAX_GIT_COMMITS: usize = 10;
const MAX_GIT_OUTPUT_CHARS: usize = 80_000;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "dist",
    "build",
];

#[derive(Clone, Debug, Default)]
pub struct ExpansionOutcome {
    /// Prompt text to send to the model (raw prompt if nothing expanded or hard limit exceeded).
    pub text: String,
    /// Human-readable labels of expanded references (e.g. `"@file:src/main.rs:1-40"`).
    pub expanded: Vec<String>,
    /// Non-fatal warnings (25% soft warning, 50% hard refusal, missing/blocked refs).
    pub warnings: Vec<String>,
    /// True when the 50% hard context limit blocked expansion and the raw prompt was kept.
    pub blocked_by_hard_limit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtCompletion {
    pub replacement: String,
    pub display: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillCommandEntry {
    /// Slash command name without leading `/` (e.g. `"rust-helper"`).
    pub name: String,
    /// Relative path inside `<profile>/skills/` (e.g. `"dev/rust-helper"`).
    pub rel_path: String,
    /// Short one-line description from YAML frontmatter (if any).
    pub description: String,
    /// Full filesystem path to `SKILL.md`.
    pub skill_file: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillInvocation {
    pub name: String,
    pub prompt: String,
}

#[derive(Clone, Debug)]
pub struct ShellEscapeOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Expand `@` context references (`@file:path[:start-end]`, `@folder:path`, `@diff`, `@staged`,
/// `@git:N`, `@url:https://...`, and shorthand `@existing/path`) in `query`.
///
/// Mirrors upstream safety rules:
/// - Skips fenced code blocks, inline backticks, and email addresses (`user@host.tld`).
/// - Refuses secret files (`is_secret_file_arg`) and paths escaping `root`.
/// - Enforces SSRF (`url_safety`) and `WebsitePolicy` on `@url:`.
/// - Soft warning at >25% of context window; hard refusal at >50% (returns raw prompt).
pub async fn expand_context_references(
    query: &str,
    root: &Path,
    context_window_tokens: Option<usize>,
    website_policy: Option<&WebsitePolicy>,
) -> ExpansionOutcome {
    if !query.contains('@') {
        return ExpansionOutcome {
            text: query.to_owned(),
            ..ExpansionOutcome::default()
        };
    }

    let tokens = extract_at_tokens(query);
    if tokens.is_empty() {
        return ExpansionOutcome {
            text: query.to_owned(),
            ..ExpansionOutcome::default()
        };
    }

    let mut sections: Vec<(String, String)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for raw_token in tokens {
        if !seen.insert(raw_token.clone()) {
            continue;
        }
        match resolve_reference(&raw_token, root, website_policy).await {
            ResolveResult::Expanded { label, body } => sections.push((label, body)),
            ResolveResult::Warning(msg) => warnings.push(msg),
            ResolveResult::Ignore => {}
        }
    }

    if sections.is_empty() {
        return ExpansionOutcome {
            text: query.to_owned(),
            expanded: Vec::new(),
            warnings,
            blocked_by_hard_limit: false,
        };
    }

    let mut assembled = String::from(query);
    assembled.push_str("\n\n---\nReferenced context:\n");
    let mut labels = Vec::with_capacity(sections.len());
    for (label, body) in sections {
        labels.push(label.clone());
        assembled.push_str(&format!("\n<context ref=\"{label}\">\n"));
        assembled.push_str(&body);
        if !body.ends_with('\n') {
            assembled.push('\n');
        }
        assembled.push_str("</context>\n");
    }

    let window_tokens = context_window_tokens
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CONTEXT_TOKENS);
    let window_chars = window_tokens.saturating_mul(CHARS_PER_TOKEN);
    let soft_limit = window_chars / 4;
    let hard_limit = window_chars / 2;
    let total_chars = assembled.chars().count();

    if total_chars > hard_limit {
        let pct = (total_chars * 100) / window_chars.max(1);
        warnings.push(format!(
            "@reference expansion refused: {total_chars} chars (~{pct}% of context window) exceeds the 50% hard limit; sent raw message instead."
        ));
        return ExpansionOutcome {
            text: query.to_owned(),
            expanded: Vec::new(),
            warnings,
            blocked_by_hard_limit: true,
        };
    }

    if total_chars > soft_limit {
        let pct = (total_chars * 100) / window_chars.max(1);
        warnings.push(format!(
            "@reference context uses ~{pct}% of the context window ({total_chars} chars, soft limit 25%)."
        ));
    }

    ExpansionOutcome {
        text: assembled,
        expanded: labels,
        warnings,
        blocked_by_hard_limit: false,
    }
}

enum ResolveResult {
    Expanded { label: String, body: String },
    Warning(String),
    Ignore,
}

async fn resolve_reference(
    token: &str,
    root: &Path,
    website_policy: Option<&WebsitePolicy>,
) -> ResolveResult {
    if token.eq_ignore_ascii_case("@diff") {
        return resolve_git_command(root, &["diff"], "@diff");
    }
    if token.eq_ignore_ascii_case("@staged") {
        return resolve_git_command(root, &["diff", "--staged"], "@staged");
    }
    if let Some(rest) = strip_prefix_ci(token, "@git:") {
        let n = rest
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n >= 1)
            .map(|n| n.min(MAX_GIT_COMMITS));
        let Some(count) = n else {
            return ResolveResult::Warning(format!(
                "Invalid @git reference '{token}' (expected @git:1..={MAX_GIT_COMMITS})."
            ));
        };
        let arg = format!("-n{count}");
        return resolve_git_command(root, &["log", &arg, "-p"], token);
    }
    if let Some(spec) = strip_prefix_ci(token, "@file:") {
        return resolve_file_spec(root, spec, token);
    }
    if let Some(dir_spec) = strip_prefix_ci(token, "@folder:") {
        return resolve_folder_spec(root, dir_spec, token);
    }
    if let Some(url) = strip_prefix_ci(token, "@url:") {
        return resolve_url_spec(url, token, website_policy).await;
    }

    // Convenient shorthand: `@path/to/file[:start-end]` or `@path/to/dir` ONLY if it resolves to
    // an existing file or folder inside the workspace (leaves `@username` / unknown tags intact).
    if let Some(raw_path) = token.strip_prefix('@') {
        let (path_part, range) = split_line_range(raw_path);
        if let Ok(resolved) = resolve_scoped_path(root, path_part) {
            if resolved.is_file() {
                return resolve_file_path(root, &resolved, path_part, range, token);
            }
            if resolved.is_dir() && range.is_none() {
                return resolve_folder_path(&resolved, token);
            }
        }
    }

    ResolveResult::Ignore
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    if s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn resolve_file_spec(root: &Path, spec: &str, label: &str) -> ResolveResult {
    let (path_part, range) = split_line_range(spec);
    if path_part.is_empty() {
        return ResolveResult::Warning(format!("Empty file path in '{label}'."));
    }
    match resolve_scoped_path(root, path_part) {
        Ok(resolved) => {
            if !resolved.is_file() {
                return ResolveResult::Warning(format!("Referenced file '{path_part}' not found."));
            }
            resolve_file_path(root, &resolved, path_part, range, label)
        }
        Err(msg) => ResolveResult::Warning(format!("{label}: {msg}")),
    }
}

fn resolve_file_path(
    _root: &Path,
    resolved: &Path,
    display_path: &str,
    range: Option<(usize, usize)>,
    label: &str,
) -> ResolveResult {
    if is_secret_file_arg(display_path, &[])
        || resolved
            .to_str()
            .is_some_and(|p| is_secret_file_arg(p, &[]))
    {
        return ResolveResult::Warning(format!(
            "{label}: refused to expand secret/credential file '{display_path}'."
        ));
    }
    let meta = match fs::metadata(resolved) {
        Ok(m) => m,
        Err(err) => return ResolveResult::Warning(format!("{label}: cannot read file: {err}")),
    };
    if meta.len() as usize > MAX_SINGLE_FILE_BYTES {
        return ResolveResult::Warning(format!(
            "{label}: file exceeds {} KiB limit ({} bytes); use a line range like @file:{display_path}:1-200.",
            MAX_SINGLE_FILE_BYTES / 1024,
            meta.len()
        ));
    }
    let bytes = match fs::read(resolved) {
        Ok(b) => b,
        Err(err) => return ResolveResult::Warning(format!("{label}: cannot read file: {err}")),
    };
    if bytes.iter().take(4096).any(|&b| b == 0) {
        return ResolveResult::Warning(format!("{label}: skipped binary file '{display_path}'."));
    }
    let text = String::from_utf8_lossy(&bytes);
    let redacted = redact_file_read(&text, false);
    let body = if let Some((start, end)) = range {
        let lines: Vec<&str> = redacted.lines().collect();
        if start == 0 || start > lines.len() {
            return ResolveResult::Warning(format!(
                "{label}: line range {start}-{end} is out of bounds ({} lines).",
                lines.len()
            ));
        }
        let end_clamped = end.min(lines.len()).max(start);
        lines[start - 1..end_clamped]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>5} | {line}", start + i))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        redacted
    };
    ResolveResult::Expanded {
        label: label.to_owned(),
        body,
    }
}

fn resolve_folder_spec(root: &Path, spec: &str, label: &str) -> ResolveResult {
    let dir_part = if spec.trim().is_empty() {
        "."
    } else {
        spec.trim()
    };
    match resolve_scoped_path(root, dir_part) {
        Ok(resolved) => {
            if !resolved.is_dir() {
                return ResolveResult::Warning(format!(
                    "Referenced folder '{dir_part}' not found."
                ));
            }
            resolve_folder_path(&resolved, label)
        }
        Err(msg) => ResolveResult::Warning(format!("{label}: {msg}")),
    }
}

fn resolve_folder_path(resolved: &Path, label: &str) -> ResolveResult {
    let mut lines = Vec::new();
    let mut count = 0usize;
    walk_folder_tree(resolved, 0, &mut lines, &mut count);
    if count >= MAX_FOLDER_ENTRIES {
        lines.push(format!("... (truncated at {MAX_FOLDER_ENTRIES} entries)"));
    }
    let body = if lines.is_empty() {
        "(empty directory)".to_owned()
    } else {
        lines.join("\n")
    };
    ResolveResult::Expanded {
        label: label.to_owned(),
        body,
    }
}

fn walk_folder_tree(dir: &Path, depth: usize, lines: &mut Vec<String>, count: &mut usize) {
    if depth > MAX_FOLDER_DEPTH || *count >= MAX_FOLDER_ENTRIES {
        return;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let indent = "  ".repeat(depth);
    for entry in entries {
        if *count >= MAX_FOLDER_ENTRIES {
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        *count += 1;
        if meta.is_dir() {
            lines.push(format!("{indent}{name}/"));
            walk_folder_tree(&entry.path(), depth + 1, lines, count);
        } else {
            lines.push(format!(
                "{indent}{name} ({})",
                format_human_size(meta.len())
            ));
        }
    }
}

fn resolve_git_command(root: &Path, args: &[&str], label: &str) -> ResolveResult {
    let output = match Command::new("git").args(args).current_dir(root).output() {
        Ok(out) => out,
        Err(err) => {
            return ResolveResult::Warning(format!("{label}: failed running git: {err}"));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return ResolveResult::Warning(format!(
            "{label}: git failed ({})",
            if stderr.is_empty() {
                "not a git repository".to_owned()
            } else {
                stderr
            }
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let redacted = redact_file_read(&text, false);
    let body = if redacted.trim().is_empty() {
        "(no changes)".to_owned()
    } else if redacted.chars().count() > MAX_GIT_OUTPUT_CHARS {
        let mut s: String = redacted.chars().take(MAX_GIT_OUTPUT_CHARS).collect();
        s.push_str("\n... [truncated]");
        s
    } else {
        redacted
    };
    ResolveResult::Expanded {
        label: label.to_owned(),
        body,
    }
}

async fn resolve_url_spec(
    url: &str,
    label: &str,
    website_policy: Option<&WebsitePolicy>,
) -> ResolveResult {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return ResolveResult::Warning(format!(
            "{label}: URL must start with http:// or https://."
        ));
    }
    if let Some(policy) = website_policy {
        if let Some(block) = policy.check_access(url) {
            return ResolveResult::Warning(format!("{label}: {}", block.message));
        }
    }
    if !url_safety::is_safe_url(url, false).await {
        return ResolveResult::Warning(format!("{label}: blocked unsafe/private URL '{url}'."));
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("hsr-context-ref/1")
        .build()
    {
        Ok(c) => c,
        Err(err) => return ResolveResult::Warning(format!("{label}: HTTP client error: {err}")),
    };
    let mut resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(err) => return ResolveResult::Warning(format!("{label}: fetch failed: {err}")),
    };
    let final_url = resp.url().as_str().to_owned();
    if final_url != url {
        if let Some(policy) = website_policy {
            if let Some(block) = policy.check_access(&final_url) {
                return ResolveResult::Warning(format!("{label}: {}", block.message));
            }
        }
        if !url_safety::is_safe_url(&final_url, false).await {
            return ResolveResult::Warning(format!(
                "{label}: blocked redirect to unsafe URL '{final_url}'."
            ));
        }
    }
    if !resp.status().is_success() {
        return ResolveResult::Warning(format!(
            "{label}: HTTP {} fetching '{url}'.",
            resp.status().as_u16()
        ));
    }
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        buf.extend_from_slice(&chunk);
        if buf.len() >= MAX_URL_BYTES {
            buf.truncate(MAX_URL_BYTES);
            break;
        }
    }
    let raw = String::from_utf8_lossy(&buf);
    let cleaned = strip_html_if_needed(&raw);
    let truncated: String = cleaned.chars().take(MAX_URL_CHARS).collect();
    ResolveResult::Expanded {
        label: label.to_owned(),
        body: truncated,
    }
}

fn strip_html_if_needed(raw: &str) -> String {
    let trimmed = raw.trim_start();
    if !(trimmed.starts_with('<')
        && (trimmed.contains("<html")
            || trimmed.contains("<body")
            || trimmed.contains("<!DOCTYPE")))
    {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len() / 2);
    let mut in_tag = false;
    let mut in_script_or_style = false;
    let lower = raw.to_ascii_lowercase();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !in_tag && (lower[i..].starts_with("<script") || lower[i..].starts_with("<style")) {
            in_script_or_style = true;
        } else if in_script_or_style
            && (lower[i..].starts_with("</script>") || lower[i..].starts_with("</style>"))
        {
            in_script_or_style = false;
            if let Some(end) = raw[i..].find('>') {
                i += end + 1;
                continue;
            }
        }
        let ch = raw[i..].chars().next().unwrap();
        let step = ch.len_utf8();
        if !in_script_or_style {
            match ch {
                '<' => {
                    in_tag = true;
                    if !out.ends_with([' ', '\n']) {
                        out.push(' ');
                    }
                }
                '>' => in_tag = false,
                _ if !in_tag => out.push(ch),
                _ => {}
            }
        }
        i += step;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_scoped_path(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err("empty path".into());
    }
    let candidate = Path::new(trimmed);
    for comp in candidate.components() {
        if matches!(comp, Component::ParentDir) {
            return Err("path traversal ('..') is not allowed".into());
        }
    }
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let canon_root = root
        .canonical()
        .map_err(|e| format!("cannot resolve workspace root: {e}"))?;
    let canon_target = joined
        .canonical()
        .map_err(|_| format!("path '{trimmed}' not found"))?;
    if !canon_target.starts_with(&canon_root) {
        return Err(format!("path '{trimmed}' is outside the workspace root"));
    }
    Ok(canon_target)
}

fn split_line_range(spec: &str) -> (&str, Option<(usize, usize)>) {
    if let Some((path, range_part)) = spec.rsplit_once(':') {
        if !path.is_empty() {
            if let Some((a, b)) = range_part.split_once('-') {
                if let (Ok(start), Ok(end)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    return (path, Some((start, end)));
                }
            } else if let Ok(single) = range_part.parse::<usize>() {
                return (path, Some((single, single)));
            }
        }
    }
    (spec, None)
}

/// Extract `@...` tokens from `text`, skipping fenced code blocks, inline backticks, and emails.
fn extract_at_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut in_code = false;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut idx = 0;
        while idx < chars.len() {
            let (byte_pos, ch) = chars[idx];
            if ch == '`' {
                in_code = !in_code;
                idx += 1;
                continue;
            }
            if !in_code && ch == '@' {
                // Reject emails / embedded `@` like `user@example.com`
                let preceded_by_word = idx > 0
                    && (chars[idx - 1].1.is_alphanumeric()
                        || matches!(chars[idx - 1].1, '_' | '.' | '-' | '+'));
                if !preceded_by_word {
                    let mut end_idx = idx + 1;
                    while end_idx < chars.len()
                        && !chars[end_idx].1.is_whitespace()
                        && chars[end_idx].1 != '`'
                    {
                        end_idx += 1;
                    }
                    let end_byte = if end_idx < chars.len() {
                        chars[end_idx].0
                    } else {
                        line.len()
                    };
                    let raw = &line[byte_pos..end_byte];
                    let cleaned = trim_trailing_punctuation(raw);
                    if cleaned.len() > 1 {
                        out.push(cleaned.to_owned());
                    }
                    idx = end_idx;
                    continue;
                }
            }
            idx += 1;
        }
    }
    out
}

fn trim_trailing_punctuation(token: &str) -> &str {
    token.trim_end_matches(['.', ',', ';', ')', ']', '}', '?', '!', '"', '\''])
}

/// Generate `@` autocomplete candidates (special refs + workspace files/folders) for `token`
/// (which starts with `'@'`).
pub fn complete_at_references(root: &Path, token: &str, limit: usize) -> Vec<AtCompletion> {
    let Some(query) = token.strip_prefix('@') else {
        return Vec::new();
    };
    let lower = query.to_ascii_lowercase();
    let mut results = Vec::new();

    let builtins: &[(&str, &str)] = &[
        ("@file:", "inject file or @file:path:start-end"),
        ("@folder:", "inject directory tree listing"),
        ("@diff", "inject unstaged git diff"),
        ("@staged", "inject staged git diff"),
        ("@git:3", "inject last N git commits (1-10)"),
        ("@url:", "fetch and inject web page"),
    ];
    for &(rep, detail) in builtins {
        if lower.is_empty() || rep[1..].starts_with(&lower) {
            results.push(AtCompletion {
                replacement: rep.to_owned(),
                display: rep.to_owned(),
                detail: detail.to_owned(),
            });
        }
    }

    let (prefix_mode, path_filter) = if let Some(rest) = strip_prefix_ci(query, "file:") {
        ("@file:", rest.to_ascii_lowercase())
    } else if let Some(rest) = strip_prefix_ci(query, "folder:") {
        ("@folder:", rest.to_ascii_lowercase())
    } else {
        ("@file:", lower)
    };
    let folders_only = prefix_mode == "@folder:";

    let mut workspace_items = Vec::new();
    collect_workspace_entries(root, root, 0, &mut workspace_items);
    for (rel, is_dir, size) in workspace_items {
        if folders_only && !is_dir {
            continue;
        }
        let rel_lower = rel.to_ascii_lowercase();
        if path_filter.is_empty() || rel_lower.contains(&path_filter) {
            let prefix = if is_dir { "@folder:" } else { "@file:" };
            let replacement = format!("{prefix}{rel}");
            let detail = if is_dir {
                "folder".to_owned()
            } else {
                format_human_size(size)
            };
            results.push(AtCompletion {
                display: replacement.clone(),
                replacement,
                detail,
            });
            if results.len() >= limit {
                break;
            }
        }
    }

    results.truncate(limit);
    results
}

fn collect_workspace_entries(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(String, bool, u64)>,
) {
    if depth > 3 || out.len() >= 150 {
        return;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= 150 {
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if rel_str.is_empty() {
            continue;
        }
        if meta.is_dir() {
            out.push((rel_str, true, 0));
            collect_workspace_entries(root, &path, depth + 1, out);
        } else if meta.is_file() {
            out.push((rel_str, false, meta.len()));
        }
    }
}

fn format_human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

const SKILL_COMMANDS_CACHE_TTL: Duration = Duration::from_secs(30);

struct CachedSkillCommands {
    entries: Vec<SkillCommandEntry>,
    loaded_at: Option<Instant>,
    refreshing: bool,
}

static SKILL_COMMANDS_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedSkillCommands>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_key(home: &Path) -> PathBuf {
    home.canonical().unwrap_or_else(|_| home.to_path_buf())
}

fn scan_skill_commands(home: &Path) -> Vec<SkillCommandEntry> {
    let skills_root = home.join("skills");
    if !skills_root.is_dir() {
        return Vec::new();
    }
    let rels = crate::skills_sync::list_installed_skills(home);
    let mut entries = Vec::new();
    let mut used_names = BTreeSet::new();
    for rel in rels {
        let skill_file = skills_root.join(&rel).join("SKILL.md");
        let raw = fs::read_to_string(&skill_file).unwrap_or_default();
        let (fm_name, fm_desc) = parse_skill_frontmatter(&raw);
        let fallback_name = rel.rsplit('/').next().unwrap_or(&rel).to_owned();
        let base_name = fm_name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(fallback_name)
            .trim()
            .to_ascii_lowercase()
            .replace(' ', "-");
        let name = if used_names.insert(base_name.clone()) {
            base_name
        } else {
            let qualified = rel.replace('/', "-").to_ascii_lowercase();
            used_names.insert(qualified.clone());
            qualified
        };
        entries.push(SkillCommandEntry {
            name,
            rel_path: rel,
            description: fm_desc.unwrap_or_else(|| "run installed skill".into()),
            skill_file,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn spawn_skill_commands_refresh(key: PathBuf, home: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("hsr-skill-scan".into())
        .spawn(move || {
            let scanned = scan_skill_commands(&home);
            let mut cache = SKILL_COMMANDS_CACHE
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            cache.insert(
                key,
                CachedSkillCommands {
                    entries: scanned,
                    loaded_at: Some(Instant::now()),
                    refreshing: false,
                },
            );
        });
}

/// Warm the slash-command skill cache on a background thread so pressing `/` never blocks input.
pub fn prewarm_skill_commands(home: Option<&Path>) {
    let Some(home) = home else {
        return;
    };
    let key = cache_key(home);
    let mut cache = SKILL_COMMANDS_CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let should_spawn = match cache.get_mut(&key) {
        Some(slot) => {
            let fresh = slot
                .loaded_at
                .is_some_and(|t| t.elapsed() < SKILL_COMMANDS_CACHE_TTL);
            if fresh || slot.refreshing {
                false
            } else {
                slot.refreshing = true;
                true
            }
        }
        None => {
            cache.insert(
                key.clone(),
                CachedSkillCommands {
                    entries: Vec::new(),
                    loaded_at: None,
                    refreshing: true,
                },
            );
            true
        }
    };
    drop(cache);
    if should_spawn {
        spawn_skill_commands_refresh(key, home.to_path_buf());
    }
}

/// Return cached installed skill commands immediately without performing disk I/O on the caller's
/// thread. If the cache is cold or stale, triggers a background refresh for subsequent completions.
pub fn cached_skill_commands_nonblocking(home: Option<&Path>) -> Vec<SkillCommandEntry> {
    let Some(home) = home else {
        return Vec::new();
    };
    let key = cache_key(home);
    let mut cache = SKILL_COMMANDS_CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let (entries, should_spawn) = match cache.get_mut(&key) {
        Some(slot) => {
            let fresh = slot
                .loaded_at
                .is_some_and(|t| t.elapsed() < SKILL_COMMANDS_CACHE_TTL);
            let spawn = !fresh && !slot.refreshing;
            if spawn {
                slot.refreshing = true;
            }
            (slot.entries.clone(), spawn)
        }
        None => {
            cache.insert(
                key.clone(),
                CachedSkillCommands {
                    entries: Vec::new(),
                    loaded_at: None,
                    refreshing: true,
                },
            );
            (Vec::new(), true)
        }
    };
    drop(cache);
    if should_spawn {
        spawn_skill_commands_refresh(key, home.to_path_buf());
    }
    entries
}

/// Discover installed skills under `<profile>/skills/` as `/<skill-name>` slash commands
/// synchronously and refresh the in-memory completion cache.
pub fn list_skill_commands(home: Option<&Path>) -> Vec<SkillCommandEntry> {
    let Some(home) = home else {
        return Vec::new();
    };
    let entries = scan_skill_commands(home);
    let key = cache_key(home);
    let mut cache = SKILL_COMMANDS_CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cache.insert(
        key,
        CachedSkillCommands {
            entries: entries.clone(),
            loaded_at: Some(Instant::now()),
            refreshing: false,
        },
    );
    entries
}

/// Match `/<skill-name> [args]` against installed skills and build the prompt payload.
pub fn resolve_skill_invocation(home: Option<&Path>, line: &str) -> Option<SkillInvocation> {
    let trimmed = line.trim();
    let without_slash = trimmed.strip_prefix('/')?;
    let (cmd, rest) = without_slash
        .split_once(char::is_whitespace)
        .map_or((without_slash, ""), |(c, r)| (c, r.trim()));
    if cmd.is_empty() {
        return None;
    }
    let needle = cmd.to_ascii_lowercase();
    let skills = list_skill_commands(home);
    let matched = skills
        .into_iter()
        .find(|s| s.name == needle || s.rel_path.to_ascii_lowercase() == needle)?;
    let body = fs::read_to_string(&matched.skill_file).ok()?;
    let prompt = if rest.is_empty() {
        format!(
            "Follow the installed skill `{}` ({}) below.\n\n<skill name=\"{}\">\n{}\n</skill>",
            matched.name, matched.rel_path, matched.name, body
        )
    } else {
        format!(
            "Follow the installed skill `{}` ({}) to handle this request:\n\n{}\n\n<skill name=\"{}\">\n{}\n</skill>",
            matched.name, matched.rel_path, rest, matched.name, body
        )
    };
    Some(SkillInvocation {
        name: matched.name,
        prompt,
    })
}

fn parse_skill_frontmatter(raw: &str) -> (Option<String>, Option<String>) {
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut desc = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim().trim_matches('"').trim_matches('\'');
            if key == "name" && !val.is_empty() {
                name = Some(val.to_owned());
            } else if key == "description" && !val.is_empty() {
                desc = Some(val.to_owned());
            }
        }
    }
    (name, desc)
}

/// Run a direct local shell command (`!<command>`) in `root` without invoking the LLM.
pub fn run_shell_escape(root: &Path, command: &str) -> Result<ShellEscapeOutput, String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("usage: !<shell command>".into());
    }
    let output = {
        #[cfg(unix)]
        {
            Command::new("sh")
                .arg("-c")
                .arg(trimmed)
                .current_dir(root)
                .output()
        }
        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", trimmed])
                .current_dir(root)
                .output()
        }
    }
    .map_err(|e| format!("failed running shell command: {e}"))?;

    Ok(ShellEscapeOutput {
        exit_code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expands_file_range_folder_ignores_emails_and_blocks_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/demo.txt"), "line1\nline2\nline3\nline4\n").unwrap();
        fs::write(root.join(".env"), "API_KEY=secret\n").unwrap();

        let out = expand_context_references(
            "Check @file:src/demo.txt:2-3, inspect @folder:src. Ignore user@example.com and `@file:src/demo.txt`.",
            root,
            None,
            None,
        )
        .await;
        assert!(!out.blocked_by_hard_limit);
        assert_eq!(out.expanded, vec!["@file:src/demo.txt:2-3", "@folder:src"]);
        assert!(out.text.contains("2 | line2"));
        assert!(out.text.contains("3 | line3"));
        assert!(!out.text.contains("1 | line1"));
        assert!(out.text.contains("demo.txt"));

        // Secret file is refused cleanly
        let secret = expand_context_references("Read @file:.env please", root, None, None).await;
        assert!(secret.expanded.is_empty());
        assert!(secret.warnings.iter().any(|w| w.contains("secret")));
        assert!(!secret.text.contains("API_KEY=secret"));
    }

    #[tokio::test]
    async fn enforces_soft_and_hard_context_limits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("big.txt"), "a".repeat(2000)).unwrap();

        // Hard limit: 50% of (500 tokens * 4 = 2000 chars) = 1000 chars -> refused!
        let refused =
            expand_context_references("Look at @file:big.txt", root, Some(500), None).await;
        assert!(refused.blocked_by_hard_limit);
        assert_eq!(refused.text, "Look at @file:big.txt");
        assert!(refused.warnings.iter().any(|w| w.contains("50%")));

        // Soft limit: 25% of (1500 tokens * 4 = 6000 chars) = 1500 chars -> expands with warning!
        let warned =
            expand_context_references("Look at @file:big.txt", root, Some(1500), None).await;
        assert!(!warned.blocked_by_hard_limit);
        assert_eq!(warned.expanded, vec!["@file:big.txt"]);
        assert!(warned.warnings.iter().any(|w| w.contains("25%")));
    }

    #[test]
    fn completes_at_references_and_resolves_skill_slash_commands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.rs"), "fn main() {}").unwrap();

        let comps = complete_at_references(root, "@app", 10);
        assert!(comps.iter().any(|c| c.replacement == "@file:src/app.rs"));

        let builtins = complete_at_references(root, "@di", 10);
        assert!(builtins.iter().any(|c| c.replacement == "@diff"));

        let home = root.join("profile");
        fs::create_dir_all(home.join("skills/dev/rust-check")).unwrap();
        fs::write(
            home.join("skills/dev/rust-check/SKILL.md"),
            "---\nname: rust-check\ndescription: Run clippy\n---\nAlways run cargo clippy.",
        )
        .unwrap();

        let skills = list_skill_commands(Some(&home));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "rust-check");
        assert_eq!(skills[0].description, "Run clippy");

        let inv = resolve_skill_invocation(Some(&home), "/rust-check audit crates").unwrap();
        assert_eq!(inv.name, "rust-check");
        assert!(inv.prompt.contains("audit crates"));
        assert!(inv.prompt.contains("Always run cargo clippy."));
    }
}
