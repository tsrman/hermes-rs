//! Bounded workspace search over regular files.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{open_read, FileWorkspace, READ_LIMIT};
use crate::redact::{is_secret_file_arg, redact_file_read};

pub(crate) const SEARCH_FILES: &str = "search_files";
const ENTRY_LIMIT: usize = 20_000;
const BYTE_BUDGET: u64 = 64 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    pattern: String,
    #[serde(default)]
    target: Target,
    path: Option<String>,
    file_glob: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default)]
    context: usize,
    #[serde(default)]
    order: Order,
}

#[derive(Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Target {
    #[default]
    #[serde(alias = "grep")]
    Content,
    #[serde(alias = "find")]
    Files,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesOnly,
    Count,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Order {
    #[default]
    Discovery,
    Modified,
}

struct Results {
    rows: Vec<Value>,
    total: usize,
    offset: usize,
    limit: usize,
}

impl Results {
    fn add(&mut self, row: impl FnOnce() -> Value) {
        if self.total >= self.offset && self.rows.len() < self.limit {
            self.rows.push(row());
        }
        self.total += 1;
    }

    fn finish(self, target: Target, mode: OutputMode, bounded: bool) -> Value {
        let truncated = bounded || self.total > self.offset.saturating_add(self.limit);
        let mut result = json!({"total_count": self.total, "truncated": truncated});
        if bounded {
            result["total_count_is_lower_bound"] = json!(true);
            result["limit_reason"] = json!("workspace_scan_budget");
        }
        match (target, mode) {
            (Target::Files, _) | (_, OutputMode::FilesOnly) => result["files"] = json!(self.rows),
            (_, OutputMode::Content) => result["matches"] = json!(self.rows),
            (_, OutputMode::Count) => {
                let counts: BTreeMap<_, _> = self
                    .rows
                    .iter()
                    .filter_map(|row| Some((row["path"].as_str()?, row["count"].as_u64()?)))
                    .collect();
                result["total_count"] = json!(counts.values().sum::<u64>());
                result["counts"] = json!(counts);
            }
        }
        result
    }
}

impl FileWorkspace<'_> {
    pub(crate) fn search(&mut self, arguments: &str) -> Value {
        let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let text = |key: &str| parsed.get(key).and_then(Value::as_str).unwrap_or("");
        let number =
            |key: &str, default: i64| parsed.get(key).and_then(Value::as_i64).unwrap_or(default);
        let offset = number("offset", 0).max(0);
        let limit = number("limit", 50).clamp(1, 200);
        // Paging is part of the key, so walking through truncated results is not a repeat.
        let key = format!(
            "search|{}|{}|{}|{}|{limit}|{offset}|{}",
            text("pattern"),
            text("target"),
            text("path"),
            text("file_glob"),
            text("order")
        );
        let count = match &mut self.search_repeat {
            (last, count) if last.as_deref() == Some(&key) => {
                *count += 1;
                *count
            }
            slot => {
                *slot = (Some(key), 1);
                1
            }
        };
        if count >= 4 {
            return json!({
                "error": format!("BLOCKED: You have run this exact search {count} times in a row. The results have NOT changed. You already have this information. STOP re-searching and proceed with your task."),
                "pattern": text("pattern"),
                "already_searched": count,
                "guardrail_refusal": true,
            });
        }
        let found = self
            .search_inner(arguments)
            .unwrap_or_else(|error| json!({"error": error}));
        shape_result(&found, count, offset, limit)
    }

    fn search_inner(&self, arguments: &str) -> Result<Value, String> {
        let args: Arguments =
            serde_json::from_str(arguments).map_err(|_| "Invalid search_files arguments")?;
        let (root, start) = self.resolve(
            args.path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .unwrap_or("."),
        )?;
        let content = (args.target == Target::Content)
            .then(|| compile(&args.pattern))
            .transpose()?;
        let name_pattern = match args.target {
            Target::Files => Some(Glob::new(if args.pattern.is_empty() {
                "*"
            } else {
                &args.pattern
            })?),
            Target::Content => args.file_glob.as_deref().map(Glob::new).transpose()?,
        };
        let (mut paths, mut bounded) = self.search_paths(&root, start);
        if args.target == Target::Files && matches!(args.order, Order::Modified) {
            paths.sort_by_cached_key(|path| {
                let modified = path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok();
                (std::cmp::Reverse(modified), path.clone())
            });
        }
        let mut results = Results {
            rows: Vec::new(),
            total: 0,
            offset: args.offset,
            limit: args.limit.unwrap_or(50).clamp(1, 200),
        };
        let mut remaining = BYTE_BUDGET;
        for path in paths {
            let relative = path
                .strip_prefix(&root)
                .map_err(|error| error.to_string())?;
            let display = relative
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if name_pattern
                .as_ref()
                .is_some_and(|glob| !glob.matches(&path, &display))
            {
                continue;
            }
            let Some(regex) = &content else {
                if open_read(&root, &path)
                    .and_then(|file| file.metadata().map_err(|error| error.to_string()))
                    .is_ok_and(|metadata| metadata.is_file())
                {
                    results.add(|| json!(display));
                }
                continue;
            };
            let Ok(file) = open_read(&root, &path) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > READ_LIMIT {
                continue;
            }
            if metadata.len() > remaining {
                bounded = true;
                break;
            }
            let mut bytes = Vec::new();
            let size = file
                .take(remaining.min(READ_LIMIT) + 1)
                .read_to_end(&mut bytes);
            let bytes_read = bytes.len() as u64;
            if bytes_read > remaining {
                bounded = true;
                break;
            }
            remaining -= bytes_read;
            if size.is_err() || bytes.contains(&0) {
                continue;
            }
            if bytes_read > READ_LIMIT {
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&bytes) {
                add_content_results(
                    &mut results,
                    regex,
                    &display,
                    text,
                    args.output_mode,
                    args.context.min(10),
                );
            }
        }
        Ok(results.finish(args.target, args.output_mode, bounded))
    }

    fn search_paths(&self, root: &Path, start: PathBuf) -> (Vec<PathBuf>, bool) {
        let mut paths = Vec::new();
        // Like ripgrep inside a git repository, skip what `.gitignore` and `.ignore` name.
        let in_repository = start
            .ancestors()
            .take_while(|dir| dir.starts_with(root))
            .any(|dir| dir.join(".git").exists());
        let mut inherited: Vec<Gitignore> = Vec::new();
        if in_repository {
            let mut chain: Vec<&Path> = start
                .ancestors()
                .take_while(|dir| dir.starts_with(root))
                .collect();
            chain.reverse();
            for dir in chain.into_iter().filter(|dir| *dir != start) {
                inherited.extend(ignore_files(dir));
            }
        }
        let mut pending = vec![(start, inherited)];
        let mut remaining = ENTRY_LIMIT;
        let mut truncated = false;
        while let Some((path, mut matchers)) = pending.pop() {
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if metadata.is_file() {
                paths.push(path);
                continue;
            }
            if !metadata.is_dir() {
                continue;
            }
            #[cfg(target_os = "linux")]
            let Ok(directory) = super::directory::open_directory(root, &path) else {
                continue;
            };
            #[cfg(target_os = "linux")]
            let Ok(entries) = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())) else {
                continue;
            };
            #[cfg(not(target_os = "linux"))]
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            if in_repository {
                matchers.extend(ignore_files(&path));
            }
            let mut children = Vec::new();
            for entry in entries {
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                remaining -= 1;
                let Ok(entry) = entry else { continue };
                let name = entry.file_name();
                let child = path.join(&name);
                let kind = entry.file_type();
                let is_dir = kind.as_ref().is_ok_and(|kind| kind.is_dir());
                if !name.to_string_lossy().starts_with('.')
                    && self.check_path(root, &child).is_ok()
                    && kind.is_ok_and(|kind| kind.is_dir() || kind.is_file())
                    && !is_ignored(&matchers, &child, is_dir)
                {
                    children.push(child);
                }
            }
            children.sort_unstable_by(|a, b| b.cmp(a));
            pending.extend(children.into_iter().map(|child| (child, matchers.clone())));
        }
        (paths, truncated)
    }
}

/// The `.gitignore` and `.ignore` matchers of one directory.
fn ignore_files(dir: &Path) -> Vec<Gitignore> {
    [".gitignore", ".ignore"]
        .iter()
        .filter_map(|name| {
            let file = dir.join(name);
            file.is_file().then(|| {
                let mut builder = GitignoreBuilder::new(dir);
                builder.add(&file);
                builder.build().ok()
            })?
        })
        .collect()
}

/// Whether the nearest matcher that names `path` ignores it (a later, deeper file wins).
fn is_ignored(matchers: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for matcher in matchers.iter().rev() {
        let found = matcher.matched(path, is_dir);
        if found.is_ignore() {
            return true;
        }
        if found.is_whitelist() {
            return false;
        }
    }
    false
}

fn add_content_results(
    results: &mut Results,
    regex: &Regex,
    path: &str,
    text: &str,
    mode: OutputMode,
    context: usize,
) {
    match mode {
        OutputMode::FilesOnly => {
            if text.lines().any(|line| regex.is_match(line)) {
                results.add(|| json!(path));
            }
        }
        OutputMode::Count => {
            let count = text.lines().filter(|line| regex.is_match(line)).count();
            if count > 0 {
                results.add(|| json!({"path": path, "count": count}));
            }
        }
        OutputMode::Content if context == 0 => {
            for (line, content) in text.lines().enumerate() {
                if regex.is_match(content) {
                    results.add(|| match_line(path, line, content));
                }
            }
        }
        OutputMode::Content => {
            let lines: Vec<_> = text.lines().collect();
            let mut emitted = 0;
            for (line, content) in lines.iter().enumerate() {
                if regex.is_match(content) {
                    let start = line.saturating_sub(context).max(emitted);
                    let end = (line + context + 1).min(lines.len());
                    for (offset, content) in lines[start..end].iter().enumerate() {
                        results.add(|| match_line(path, start + offset, content));
                    }
                    emitted = end;
                }
            }
        }
    }
}

/// The result as the reference lays it out: sparse keys, matches grouped by path once there are
/// five or more, and hints for truncation and repetition.
fn shape_result(found: &Value, count: u32, offset: i64, limit: i64) -> Value {
    if found.get("error").is_some() {
        return found.clone();
    }
    let mut out = serde_json::Map::new();
    out.insert("total_count".into(), found["total_count"].clone());
    if let Some(matches) = found["matches"].as_array().filter(|m| !m.is_empty()) {
        if matches.len() >= 5 {
            let mut lines: Vec<String> = Vec::new();
            let mut current: Option<&str> = None;
            for entry in matches {
                let path = entry["path"].as_str().unwrap_or("");
                if current != Some(path) {
                    lines.push(path.to_owned());
                    current = Some(path);
                }
                lines.push(format!(
                    "  {}: {}",
                    entry["line"],
                    entry["content"].as_str().unwrap_or("").trim_end()
                ));
            }
            out.insert("matches_format".into(), json!("path-grouped: each file path on its own line, followed by indented '<line>: <content>' rows for matches in that file"));
            out.insert("matches_text".into(), json!(lines.join("\n")));
        } else {
            out.insert("matches".into(), json!(matches));
        }
    }
    if let Some(files) = found["files"].as_array().filter(|f| !f.is_empty()) {
        out.insert("files".into(), json!(files));
    }
    if let Some(counts) = found["counts"].as_object().filter(|c| !c.is_empty()) {
        out.insert("counts".into(), json!(counts));
    }
    let truncated = found["truncated"] == true;
    if truncated {
        out.insert("truncated".into(), json!(true));
        out.insert("total_count_is_lower_bound".into(), json!(true));
    }
    for key in ["limit_reason", "warning"] {
        if let Some(value) = found
            .get(key)
            .filter(|v| v.as_str().is_some_and(|t| !t.is_empty()))
        {
            out.insert(key.into(), value.clone());
        }
    }
    if count >= 3 {
        out.insert("_warning".into(), json!(format!("You have run this exact search {count} times consecutively. The results have not changed. Use the information you already have.")));
    }
    if truncated {
        out.insert("_hint".into(), json!(format!(
            "Results truncated. Use offset={} to see more, or narrow with a more specific pattern or file_glob.",
            offset + limit
        )));
    }
    Value::Object(out)
}

fn match_line(path: &str, line: usize, content: &str) -> Value {
    let shown: String = content.chars().take(MAX_LINE_CHARS).collect();
    let text = redact_file_read(&shown, is_secret_file_arg(path, &[]));
    json!({"path": path, "line": line + 1, "content": text})
}

fn compile(pattern: &str) -> Result<Regex, String> {
    if pattern.is_empty() || pattern.len() > 4096 {
        return Err("The search pattern must be between 1 and 4096 bytes".into());
    }
    RegexBuilder::new(pattern)
        .size_limit(1024 * 1024)
        .build()
        .map_err(|error| error.to_string())
}

struct Glob {
    regex: Regex,
    relative_path: bool,
}

impl Glob {
    fn new(pattern: &str) -> Result<Self, String> {
        let mut regex = String::from("^");
        let mut chars = pattern.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '*' if chars.next_if_eq(&'*').is_some() => {
                    if chars.next_if_eq(&'/').is_some() {
                        regex.push_str("(?:.*/)?");
                    } else {
                        regex.push_str(".*");
                    }
                }
                '*' => regex.push_str("[^/]*"),
                '?' => regex.push_str("[^/]"),
                ch => regex.push_str(&regex::escape(&ch.to_string())),
            }
        }
        regex.push('$');
        Ok(Self {
            regex: compile(&regex)?,
            relative_path: pattern.contains('/'),
        })
    }

    fn matches(&self, path: &Path, relative: &str) -> bool {
        if self.relative_path {
            return self.regex.is_match(relative);
        }
        path.file_name()
            .is_some_and(|name| self.regex.is_match(&name.to_string_lossy()))
    }
}

pub(crate) fn search_schema() -> Value {
    json!({"type": "function", "function":
        serde_json::from_str::<Value>(include_str!("../schemas/search_files.json")).unwrap_or_default()})
}
