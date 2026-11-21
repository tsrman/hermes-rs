//! Bounded workspace search over regular files.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{open_read, FileWorkspace, READ_LIMIT};
use crate::redact::{is_secret_file_arg, redact_file_read};

pub(crate) const SEARCH_FILES: &str = "search_files";
const ENTRY_LIMIT: usize = 20_000;
const BYTE_BUDGET: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 2048;

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
    pub(crate) fn search(&self, arguments: &str) -> Value {
        self.search_inner(arguments)
            .unwrap_or_else(|error| json!({"error": error}))
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
        let mut pending = vec![start];
        let mut remaining = ENTRY_LIMIT;
        let mut truncated = false;
        while let Some(path) = pending.pop() {
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
                if !name.to_string_lossy().starts_with('.')
                    && self.check_path(root, &child).is_ok()
                    && entry
                        .file_type()
                        .is_ok_and(|kind| kind.is_dir() || kind.is_file())
                {
                    children.push(child);
                }
            }
            children.sort_unstable_by(|a, b| b.cmp(a));
            pending.extend(children);
        }
        (paths, truncated)
    }
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

fn match_line(path: &str, line: usize, content: &str) -> Value {
    let limit = content.floor_char_boundary(MAX_LINE_BYTES);
    let text = redact_file_read(&content[..limit], is_secret_file_arg(path, &[]));
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
    json!({"type": "function", "function": {
        "name": SEARCH_FILES,
        "description": "Search UTF-8 text by regex or find files by a glob in the current working directory. Returns paths relative to that directory.",
        "parameters": {"type": "object", "properties": {
            "pattern": {"type": "string"},
            "target": {"type": "string", "enum": ["content", "files"], "default": "content"},
            "path": {"type": "string", "default": "."},
            "file_glob": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
            "offset": {"type": "integer", "minimum": 0, "default": 0},
            "order": {"type": "string", "enum": ["discovery", "modified"], "default": "discovery"},
            "output_mode": {"type": "string", "enum": ["content", "files_only", "count"], "default": "content"},
            "context": {"type": "integer", "minimum": 0, "maximum": 10, "default": 0}
        }, "required": ["pattern"], "additionalProperties": false}
    }})
}
