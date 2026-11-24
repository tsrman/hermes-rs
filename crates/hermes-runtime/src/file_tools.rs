use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

use ring::digest::{digest, SHA256};

use crate::redact::{is_secret_file_arg, redact_file_read};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod directory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod linux;
mod search;

pub(crate) use search::{search_schema, SEARCH_FILES};

const READ_LIMIT: u64 = 10_000_000;
const WRITE_LIMIT: usize = 1_048_576;
pub(crate) const WRITE_FILE: &str = "write_file";
pub(crate) const PATCH: &str = "patch";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApprovalRequest {
    pub target: String,
    pub content: String,
    pub action: ApprovalAction,
    pub timeout_secs: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApprovalAction {
    CreateFile,
    ReplaceFile,
    RunCommand,
    /// Python source for `execute_code`; `target` is the directory it runs in.
    RunCode,
    /// Changes to skills: `content` describes every operation.
    ManageSkills,
    /// Questions for the person: `content` holds them as JSON and the reply is `Answered`.
    Clarify,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApprovalDecision {
    Approved,
    Denied,
    TimedOut,
    Unavailable,
    /// The reply to `Clarify`: `{"answers": {...}, "outcome": ...}` as JSON.
    Answered(String),
}

pub(crate) struct FileWorkspace<'a> {
    root: &'a Path,
    profile: Option<PathBuf>,
    /// Exact selected profile directory, whose `cache/spillover` and `cache/documents`
    /// subtrees are readable (read-only) by `read_file` for spilled MCP results and blobs.
    cache_home: Option<PathBuf>,
    observed: HashMap<PathBuf, Observation>,
    /// Misses per file, so a model that keeps retrying a patch is told to stop.
    patch_failures: HashMap<PathBuf, u32>,
    /// The last search and how many times in a row it ran.
    search_repeat: (Option<String>, u32),
    limits: crate::terminal::OutputLimits,
}

struct Observation {
    hash: [u8; 32],
    ranges: Vec<(usize, usize)>,
    complete: bool,
}

impl Observation {
    fn new(hash: [u8; 32]) -> Self {
        Self {
            hash,
            ranges: Vec::new(),
            complete: false,
        }
    }

    fn add(&mut self, start: usize, end: usize, lines: usize) {
        if self.complete {
            return;
        }
        self.ranges.push((start, end));
        self.ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut().filter(|last| last.1 >= start) {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        self.complete = merged.as_slice() == [(0, lines)];
        self.ranges = merged;
    }
}

impl<'a> FileWorkspace<'a> {
    pub(crate) fn new(root: &'a Path, profile: Option<&Path>) -> Self {
        let cache_home =
            profile.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_owned()));
        let profile = cache_home.clone().map(|path| {
            // Named profiles live under <root>/profiles/<name>. Protect the
            // entire store, including the default and other named profiles.
            match path.parent() {
                Some(parent) if parent.ends_with("profiles") => {
                    parent.parent().unwrap_or(&path).to_owned()
                }
                _ => path,
            }
        });
        Self {
            root,
            profile,
            cache_home,
            observed: HashMap::new(),
            patch_failures: HashMap::new(),
            search_repeat: (None, 0),
            limits: crate::terminal::OutputLimits::default(),
        }
    }

    /// How much of a read the model sees (`tool_output.*`, `file_read_max_chars`).
    pub(crate) fn with_limits(mut self, limits: crate::terminal::OutputLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn read(&mut self, arguments: &str) -> Value {
        match self.read_inner(arguments) {
            Ok(value) => value,
            Err(error) => json!({"error": error}),
        }
    }

    fn read_inner(&mut self, arguments: &str) -> Result<Value, String> {
        let args: Value = serde_json::from_str(arguments).map_err(|_| "Invalid tool arguments")?;
        let path = args["path"].as_str().ok_or("Missing path")?;
        let offset = args
            .get("offset")
            .map_or(Some(1), Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or("Invalid offset")?;
        let limit = args
            .get("limit")
            .map_or(Some(2000), Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or("Invalid limit")?
            .min(self.limits.max_lines as u64);
        let (root, resolved) = match self.resolve_read(path) {
            Ok(found) => found,
            Err(error) if error.contains("No such file") => {
                // A path that does not exist: name similar files, but only inside the workspace.
                let candidate = self
                    .root
                    .canonicalize()
                    .map_err(|e| e.to_string())?
                    .join(path);
                self.check_path(
                    &self.root.canonicalize().map_err(|e| e.to_string())?,
                    &candidate,
                )?;
                return Ok(similar_files(path, &candidate));
            }
            Err(error) => return Err(error),
        };
        let file = match open_read(&root, &resolved) {
            Ok(file) => file,
            Err(_) if !resolved.exists() && resolved.symlink_metadata().is_err() => {
                return Ok(similar_files(path, &resolved));
            }
            Err(error) => return Err(error),
        };
        let extractable = crate::doc_extract::is_extractable_document(&resolved);
        let max_read = if extractable {
            crate::doc_extract::MAX_DOCUMENT_BYTES
        } else {
            READ_LIMIT
        };
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > max_read {
            return Err(format!(
                "File must be a regular file no larger than {} MB",
                max_read / 1_000_000
            ));
        }
        let mut raw_bytes = Vec::new();
        file.take(max_read + 1)
            .read_to_end(&mut raw_bytes)
            .map_err(|error| error.to_string())?;
        if raw_bytes.len() as u64 > max_read {
            return Err(format!(
                "File must be a regular file no larger than {} MB",
                max_read / 1_000_000
            ));
        }
        if let Some(mime) = detect_image_mime(&raw_bytes) {
            let b64 = crate::providers::oauth::base64_encode(&raw_bytes);
            let data_url = format!("data:{mime};base64,{b64}");
            return Ok(json!({
                "is_image": true,
                "is_binary": true,
                "mime_type": mime,
                "file_size": raw_bytes.len(),
                "path": path,
                "note": format!("Image loaded ({mime}, {} bytes) and attached for vision inspection.", raw_bytes.len()),
                "_hermes_multimodal_images": [{
                    "data_url": data_url,
                    "media_type": mime,
                    "label": format!("Image from read_file({path})")
                }]
            }));
        }
        if extractable {
            match crate::doc_extract::extract_document_bytes(&raw_bytes, &resolved, path) {
                Ok(extracted) => {
                    let observe = !crate::doc_extract::is_binary_document(&resolved);
                    return self.paginate_text_result(
                        path,
                        &resolved,
                        &extracted,
                        raw_bytes.len(),
                        offset,
                        limit,
                        observe,
                        true,
                    );
                }
                Err(err) if crate::doc_extract::is_binary_document(&resolved) => {
                    let ext = crate::doc_extract::lower_extension(&resolved);
                    return Err(format!(
                        "Cannot read '{path}' ({ext}): document extraction failed — {err}. Use terminal utilities to inspect or convert the file."
                    ));
                }
                Err(_) => {}
            }
        }
        let data = String::from_utf8(raw_bytes)
            .map_err(|_| "File must be UTF-8 text no larger than 10 MB".to_owned())?;
        if data.contains('\0') {
            return Err("File must be UTF-8 text no larger than 10 MB".into());
        }
        self.paginate_text_result(
            path,
            &resolved,
            &data,
            data.len(),
            offset,
            limit,
            true,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paginate_text_result(
        &mut self,
        path: &str,
        resolved: &Path,
        data: &str,
        size: usize,
        offset: u64,
        limit: u64,
        observe: bool,
        extracted_document: bool,
    ) -> Result<Value, String> {
        // A leading BOM belongs to the first page only, and is not shown.
        let shown_data = if offset == 1 {
            data.strip_prefix('\u{feff}').unwrap_or(data)
        } else {
            data
        };
        let mut lines: Vec<&str> = shown_data.split('\n').collect();
        if shown_data.ends_with('\n') || shown_data.is_empty() {
            lines.pop();
        }
        let total = lines.len();
        let start = usize::try_from(offset - 1).unwrap_or(usize::MAX).min(total);
        let end_line = offset.saturating_add(limit) - 1;
        let end = start.saturating_add(limit as usize).min(total);
        if data.is_empty() && !extracted_document {
            return Ok(
                json!({"content": "", "total_lines": 0, "file_size": 0, "hint": "File is empty (0 bytes)."}),
            );
        }
        if offset as usize > total && total > 0 {
            return Ok(
                json!({"content": "", "total_lines": total, "file_size": size, "hint": format!(
                "Note: offset {offset} is beyond the end of the file ({total} lines total). Retry with offset <= {total}.")}),
            );
        }
        let max_line = self.limits.max_line_length;
        let mut clipped_lines = false;
        let mut content = lines[start..end]
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let line = if line.chars().count() > max_line {
                    clipped_lines = true;
                    format!(
                        "{}... [truncated]",
                        line.chars().take(max_line).collect::<String>()
                    )
                } else {
                    (*line).to_owned()
                };
                format!("{}|{}", start + index + 1, line)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut truncated = total as u64 > end_line;
        let mut hint = truncated.then(|| {
            format!(
                "Use offset={} to continue reading (showing {offset}-{end_line} of {total} lines)",
                end_line + 1
            )
        });
        let mut extra = serde_json::Map::new();
        // A read is capped at a character budget, trimmed to the last whole line.
        let max_chars = self.limits.file_read_max_chars;
        if content.chars().count() > max_chars {
            let mut kept: Vec<&str> = Vec::new();
            let mut running = 0;
            for line in content.split('\n') {
                let addition = line.chars().count() + usize::from(!kept.is_empty());
                if running + addition > max_chars {
                    break;
                }
                kept.push(line);
                running += addition;
            }
            let first_clamped = kept.is_empty();
            let trimmed = if first_clamped {
                content
                    .split('\n')
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(max_chars)
                    .collect::<String>()
            } else {
                kept.join("\n")
            };
            let lines_kept = if first_clamped { 1 } else { kept.len() };
            let next_offset = offset as usize + lines_kept;
            let mut text = format!(
                "Output truncated at the 100,000-char read budget after {lines_kept} line(s) (showing lines {offset}-{} of {total}). Use offset={next_offset} to continue.",
                next_offset - 1
            );
            if first_clamped {
                extra.insert("truncated_lines".into(), json!(true));
                text.push_str(" Note: the first line alone exceeded the budget and was clamped mid-line; its remainder is not retrievable via offset.");
            }
            content = trimmed;
            truncated = true;
            hint = Some(text);
            extra.insert("truncated_by".into(), json!("bytes"));
            extra.insert("next_offset".into(), json!(next_offset));
        } else if clipped_lines {
            extra.insert("truncated_lines".into(), json!(true));
        }
        // File content reaches the model with credentials masked by a sentinel it cannot write
        // back as a usable key; a file known to hold credentials gets every pass.
        let homes: Vec<&Path> = self.profile.iter().map(PathBuf::as_path).collect();
        let content = redact_file_read(&content, is_secret_file_arg(path, &homes));
        if observe && data.len() <= WRITE_LIMIT {
            let hash = fingerprint(data.as_bytes());
            let observation = self
                .observed
                .entry(resolved.to_path_buf())
                .or_insert_with(|| Observation::new(hash));
            if observation.hash != hash {
                *observation = Observation::new(hash);
            }
            // A page trimmed by the character budget only covers the lines kept.
            let seen_end = extra
                .get("next_offset")
                .and_then(Value::as_u64)
                .map_or(end, |next| (next as usize - 1).min(end));
            observation.add(start, seen_end, total);
        }
        let mut result = serde_json::Map::new();
        result.insert("content".into(), json!(content));
        result.insert("total_lines".into(), json!(total));
        result.insert("file_size".into(), json!(size));
        result.insert("truncated".into(), json!(truncated));
        if extracted_document {
            result.insert("extracted_document".into(), json!(true));
        }
        if let Some(hint) = hint {
            result.insert("hint".into(), json!(hint));
        }
        for (key, value) in extra {
            result.insert(key, value);
        }
        result.insert("is_binary".into(), json!(false));
        result.insert("is_image".into(), json!(false));
        result.insert("not_found".into(), json!(false));
        Ok(Value::Object(result))
    }

    pub(crate) fn resolve(&self, path: &str) -> Result<(PathBuf, PathBuf), String> {
        let root = self
            .root
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let requested = root.join(path);
        self.check_path(&root, &requested)?;
        let resolved = requested
            .canonicalize()
            .map_err(|error| error.to_string())?;
        self.check_path(&root, &resolved)?;
        Ok((root, resolved))
    }

    /// Resolve a path for `read_file`: workspace files plus read-only access to the selected
    /// profile's `cache/spillover` and `cache/documents` directories.
    fn resolve_read(&self, path: &str) -> Result<(PathBuf, PathBuf), String> {
        if let Some(found) = self.resolve_cache_read(path) {
            return Ok(found);
        }
        self.resolve(path)
    }

    fn resolve_cache_read(&self, path: &str) -> Option<(PathBuf, PathBuf)> {
        let requested = Path::new(path);
        if !requested.is_absolute() {
            return None;
        }
        let home = self.cache_home.as_ref()?;
        let cache = home.join("cache");
        let resolved = requested.canonicalize().ok()?;
        for subdir in ["spillover", "documents"] {
            let Ok(allowed) = cache.join(subdir).canonicalize() else {
                continue;
            };
            if let Ok(rel) = resolved.strip_prefix(&allowed) {
                if !rel.as_os_str().is_empty()
                    && !rel.components().any(protected_component)
                    && !requested.components().any(protected_component)
                {
                    return Some((allowed, resolved));
                }
            }
        }
        None
    }

    fn check_path(&self, root: &Path, path: &Path) -> Result<(), String> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "Path is outside the working directory")?;
        if self
            .profile
            .as_ref()
            .is_some_and(|profile| path.starts_with(profile))
            || relative.components().any(protected_component)
        {
            return Err(
                "Path belongs to a protected profile, credential store or repository metadata"
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) async fn edit<A, F>(
        &mut self,
        tool: &str,
        arguments: &str,
        timeout_secs: Option<u32>,
        approve: &mut A,
    ) -> Value
    where
        A: FnMut(ApprovalRequest) -> F,
        F: Future<Output = ApprovalDecision>,
    {
        let Some(seconds) =
            timeout_secs.filter(|_| cfg!(any(target_os = "linux", target_os = "macos")))
        else {
            return approval_error(ApprovalDecision::Unavailable);
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let plan = if tool == PATCH {
                match self.prepare_patch(arguments) {
                    Ok(Prepared::Plan(plan)) => *plan,
                    Ok(Prepared::Done(answer)) => return answer,
                    Err(error) => return json!({"error": error}),
                }
            } else {
                match self.prepare_write(arguments) {
                    Ok(plan) => plan,
                    Err(error) => return json!({"error": error}),
                }
            };
            let Some(target) = plan.target.path.to_str() else {
                return json!({"error": "The target path must be UTF-8 to display it for approval"});
            };
            let created = plan.target.directories_to_create();
            let request = ApprovalRequest {
                target: if created.is_empty() {
                    target.to_owned()
                } else {
                    format!("{target} (creates directories: {})", created.join(", "))
                },
                content: plan.content.clone(),
                action: if plan.before.is_some() {
                    ApprovalAction::ReplaceFile
                } else {
                    ApprovalAction::CreateFile
                },
                timeout_secs: seconds,
            };
            let decision = request_approval(request, approve).await;
            if !matches!(decision, ApprovalDecision::Approved) {
                return approval_error(decision);
            }
            match plan.target.commit(&plan.content, plan.before) {
                Ok(()) => {
                    let mut observation = Observation::new(fingerprint(plan.content.as_bytes()));
                    observation.complete = true;
                    self.observed.insert(plan.target.path.clone(), observation);
                    if let Some((notes, diff, lint)) = plan.patch {
                        let resolved = plan.target.path.display().to_string();
                        let mut result = serde_json::Map::new();
                        result.insert("success".into(), json!(true));
                        if !notes.is_empty() {
                            result.insert("note".into(), json!(notes.join("\n")));
                        }
                        result.insert("diff".into(), json!(diff));
                        result.insert("files_modified".into(), json!([resolved]));
                        result.insert("lint".into(), lint);
                        result.insert("resolved_path".into(), json!(resolved));
                        Value::Object(result)
                    } else {
                        let resolved = plan.target.path.display().to_string();
                        let mut result = serde_json::Map::new();
                        result.insert("bytes_written".into(), json!(plan.content.len()));
                        result.insert("dirs_created".into(), json!(!created.is_empty()));
                        result.insert("verified".into(), json!(true));
                        if let Some(lint) = plan.lint {
                            result.insert("lint".into(), lint);
                        }
                        result.insert("resolved_path".into(), json!(resolved));
                        result.insert("files_modified".into(), json!([resolved]));
                        Value::Object(result)
                    }
                }
                Err(error) => json!({"error": error}),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (tool, arguments, seconds, approve);
            approval_error(ApprovalDecision::Unavailable)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn prepare_write(&self, arguments: &str) -> Result<linux::WritePlan, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            path: String,
            content: String,
        }
        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "write_file expects a path and UTF-8 text content")?;
        if args.content.len() > WRITE_LIMIT || args.content.contains('\0') {
            return Err("write_file accepts text up to 1 MiB".into());
        }
        let target = self.write_target(&args.path)?;
        let previous = target.read_version()?;
        let before = previous.as_ref().map(|(stamp, _)| *stamp);
        let lint = crate::syntax_lint::lint_delta(
            &args.path,
            previous.as_ref().map(|(_, text)| text.as_str()),
            &args.content,
        );
        if let Some(stamp) = before {
            if !self
                .observed
                .get(&target.path)
                .is_some_and(|read| read.complete && read.hash == stamp.content)
            {
                return Err(
                    "Read the entire current file before replacing it with write_file".into(),
                );
            }
        }
        Ok(linux::WritePlan {
            lint: Some(lint),
            target,
            content: args.content,
            before,
            patch: None,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn write_target(&self, path: &str) -> Result<linux::Target, String> {
        let root = self
            .root
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let target = root.join(path);
        self.check_path(&root, &target)?;
        if [
            "/etc",
            "/boot",
            "/usr/lib/systemd",
            "/dev",
            "/proc",
            "/sys",
            "/run",
            "/System",
            "/Library/LaunchDaemons",
        ]
        .iter()
        .any(|directory| target.starts_with(directory))
        {
            return Err("Refusing to write to a protected system path".into());
        }
        if crate::doc_extract::is_sqlite_sidecar(&target) {
            return Err(format!(
                "Refusing to write plain text to binary SQLite sidecar '{path}'. Use sqlite3 CLI or a SQLite library via the terminal."
            ));
        }
        if target.exists() && crate::doc_extract::is_binary_document(&target) {
            return Err(format!(
                "Refusing to overwrite existing binary document '{path}' with plain text. Use appropriate libraries or utilities via the terminal to modify it."
            ));
        }
        linux::Target::open(&root, &target)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    /// A patch: the plan for the write, or a finished answer (a miss, or an edit that is already
    /// in the file).
    fn prepare_patch(&mut self, arguments: &str) -> Result<Prepared, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            mode: Option<String>,
            path: String,
            old_string: Option<String>,
            new_string: Option<String>,
            #[serde(default)]
            replace_all: bool,
        }
        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "patch expects path, old_string, and new_string")?;
        if args.mode.as_deref().is_some_and(|mode| mode != "replace") {
            return Err(format!("Unknown mode: {}", args.mode.unwrap_or_default()));
        }
        let (Some(old), Some(new)) = (args.old_string.as_deref(), args.new_string.as_deref())
        else {
            return Err("old_string and new_string required".into());
        };
        if new.contains('\0') {
            return Err("patch requires UTF-8 replacement text".into());
        }
        let target = self.write_target(&args.path)?;
        let (before, raw) = target
            .read_version()?
            .ok_or("Failed to read file: the file to patch does not exist")?;
        // Match and diff on BOM-stripped text; the BOM is written back.
        let (had_bom, original) = match raw.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest.to_owned()),
            None => (false, raw.clone()),
        };
        let mut notes: Vec<String> = Vec::new();
        let found =
            crate::fuzzy::find_and_replace(&original, old, new, args.replace_all, &mut notes);
        if found.error.is_some() || found.count == 0 {
            return Ok(Prepared::Done(self.patch_miss(
                &target.path,
                &args.path,
                &original,
                old,
                new,
                found.count,
                found.error,
            )));
        }
        // Models send bare-LF strings; keep the file's line endings.
        let mut content = found.content;
        let head: String = original.chars().take(4096).collect();
        let ending = if head.contains("\r\n") {
            Some("\r\n")
        } else if head.contains('\n') {
            Some("\n")
        } else {
            None
        };
        if let Some(ending) = ending {
            let lf = content.replace("\r\n", "\n").replace('\r', "\n");
            content = if ending == "\r\n" {
                lf.replace('\n', "\r\n")
            } else {
                lf
            };
        }
        if let Some(note) = crate::fuzzy::boundary_note(found.strategy) {
            notes.push(note);
        }
        let written = if had_bom {
            format!("\u{feff}{content}")
        } else {
            content.clone()
        };
        if written.len() > WRITE_LIMIT {
            return Err("The patched file would exceed 1 MiB".into());
        }
        let lint = crate::syntax_lint::lint_delta(&args.path, Some(&original), &content);
        let path_text = target.path.display().to_string();
        let diff = crate::difflib::unified_diff(
            &original,
            &content,
            &format!("a/{path_text}"),
            &format!("b/{path_text}"),
        );
        self.patch_failures.remove(&target.path);
        Ok(Prepared::Plan(Box::new(linux::WritePlan {
            lint: None,
            target,
            content: written,
            before: Some(before),
            patch: Some((notes, diff, lint)),
        })))
    }

    /// The answer for a patch that matched nothing: an already-applied edit is a no-op success;
    /// otherwise the error carries a "Did you mean" snippet and, after repeated misses, a hint.
    #[allow(clippy::too_many_arguments)]
    fn patch_miss(
        &mut self,
        path: &Path,
        shown: &str,
        content: &str,
        old: &str,
        new: &str,
        count: usize,
        error: Option<String>,
    ) -> Value {
        if crate::fuzzy::is_already_applied(content, old, new) {
            return json!({"success": true, "no_change": true, "note": format!(
                "File already contains the target text — the edit appears to be already applied to {}. No write performed; do not re-send this patch.",
                path.display())});
        }
        let mut message = error.unwrap_or_else(|| {
            format!("Could not find match for old_string in {}", path.display())
        });
        message.push_str(&crate::fuzzy::no_match_hint(&message, count, old, content));
        let mut result = json!({"success": false, "error": message});
        if message.contains("Could not find") {
            let failures = self.patch_failures.entry(path.to_owned()).or_insert(0);
            *failures += 1;
            if *failures >= 3 {
                result["_hint"] = json!(format!(
                    "This is failure #{failures} patching {shown:?}. Stop retrying with variations of the same old_string. Either: (1) re-read the file fresh to verify current content, (2) use a longer / more unique old_string with surrounding context lines, or (3) re-read the file in full, then use write_file to replace it if the targeted region is hard to anchor."
                ));
            } else if !message.contains("Did you mean one of these sections?") {
                result["_hint"] = json!("old_string not found. Use read_file to verify the current content, or search_files to locate the text.");
            }
        }
        result
    }
}

/// What preparing a patch produced.
#[cfg(any(target_os = "linux", target_os = "macos"))]
enum Prepared {
    Plan(Box<linux::WritePlan>),
    Done(Value),
}

/// "File not found" with up to five similar names from the same directory, best first.
fn similar_files(path: &str, resolved: &Path) -> Value {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    let stem = |text: &str| {
        Path::new(text)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase()
    };
    let extension = |text: &str| {
        Path::new(text)
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default()
    };
    let (basename, ext) = (stem(&name), extension(&name));
    let directory = Path::new(path)
        .parent()
        .map(Path::to_owned)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut listing: Vec<String> = resolved
        .parent()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    listing.sort();
    listing.truncate(50);
    let mut scored: Vec<(u32, String)> = Vec::new();
    for entry in listing.iter().filter(|e| !e.is_empty()) {
        let lower = entry.to_lowercase();
        let mut score = if lower == name {
            100
        } else if stem(entry) == basename {
            90
        } else if lower.starts_with(&name) || name.starts_with(&lower) {
            70
        } else if lower.contains(&name) {
            60
        } else if name.contains(&lower) && lower.chars().count() > 2 {
            40
        } else if !ext.is_empty() && extension(entry) == ext {
            let common = name
                .chars()
                .collect::<std::collections::HashSet<_>>()
                .intersection(&lower.chars().collect())
                .count();
            if common as f64 >= name.chars().count().max(lower.chars().count()) as f64 * 0.4 {
                30
            } else {
                0
            }
        } else {
            0
        };
        if score == 0 && crate::difflib::ratio(&name, &lower) >= 0.8 {
            score = 50;
        }
        if score > 0 {
            scored.push((score, directory.join(entry).display().to_string()));
        }
    }
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let similar: Vec<String> = scored.into_iter().take(5).map(|(_, p)| p).collect();
    let mut result = json!({"error": format!("File not found: {path}"), "not_found": true});
    if !similar.is_empty() {
        result["similar_files"] = json!(similar);
    }
    result
}

fn open_read(root: &Path, path: &Path) -> Result<std::fs::File, String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return linux::Target::open(root, path)?.read_file();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = root;
        std::fs::File::open(path).map_err(|error| error.to_string())
    }
}

fn protected_component(component: Component<'_>) -> bool {
    let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
    (name != ".env.example" && (name == ".env" || name.starts_with(".env.")))
        || matches!(
            name.as_str(),
            ".envrc" | ".git" | ".ssh" | ".aws" | "vault" | "mcp-tokens" | "browser-profile"
        )
}

pub(crate) fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    digest(&SHA256, bytes)
        .as_ref()
        .try_into()
        .expect("SHA-256 length")
}

pub(crate) fn approval_error(decision: ApprovalDecision) -> Value {
    let (status, error) = match decision {
        ApprovalDecision::Denied => ("approval_denied", "The user declined this action."),
        ApprovalDecision::TimedOut => ("approval_timeout", "Approval timed out."),
        ApprovalDecision::Unavailable
        | ApprovalDecision::Approved
        | ApprovalDecision::Answered(_) => (
            "approval_unavailable",
            "This action requires an interactive terminal and explicit approval.",
        ),
    };
    json!({"status": status, "error": error})
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) async fn request_approval<A, F>(
    request: ApprovalRequest,
    approve: &mut A,
) -> ApprovalDecision
where
    A: FnMut(ApprovalRequest) -> F,
    F: Future<Output = ApprovalDecision>,
{
    // What the rules already decide costs the person nothing, so it is settled before the clock
    // for their answer starts (the first check of a command can take a moment).
    if let Some(policy) = crate::approval_policy::installed() {
        match policy.decide(&request) {
            crate::approval_policy::Verdict::Allow => return ApprovalDecision::Approved,
            crate::approval_policy::Verdict::Deny => return ApprovalDecision::Denied,
            crate::approval_policy::Verdict::Ask => {}
        }
    }
    tokio::time::timeout(
        Duration::from_secs(u64::from(request.timeout_secs)),
        approve(request),
    )
    .await
    .unwrap_or(ApprovalDecision::TimedOut)
}

/// Where commands and file changes cannot be approved (no terminal support), nothing is asked.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) async fn request_approval<A, F>(
    request: ApprovalRequest,
    approve: &mut A,
) -> ApprovalDecision
where
    A: FnMut(ApprovalRequest) -> F,
    F: Future<Output = ApprovalDecision>,
{
    let _ = (request, approve);
    ApprovalDecision::Unavailable
}

pub(crate) fn write_schema() -> Value {
    json!({"type": "function", "function":
        serde_json::from_str::<Value>(include_str!("schemas/write_file.json")).unwrap_or_default()})
}

pub(crate) fn patch_schema() -> Value {
    json!({"type": "function", "function":
        serde_json::from_str::<Value>(include_str!("schemas/patch.json")).unwrap_or_default()})
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(test)]
mod tests {
    use super::Observation;

    #[test]
    fn paginated_reads_cover_a_file_only_without_gaps() {
        let mut observation = Observation::new([0; 32]);
        observation.add(2, 4, 4);
        observation.add(0, 1, 4);
        assert!(!observation.complete);
        observation.add(1, 2, 4);
        assert!(observation.complete);
    }

    #[tokio::test]
    async fn patch_matches_loosely_and_reports_a_diff() {
        use super::{ApprovalDecision, ApprovalRequest, FileWorkspace, PATCH};
        use serde_json::json;

        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("a.json");
        std::fs::write(&file, "{\n  \"a\": 1,\n  \"b\": 2\n}\n").unwrap();
        let mut workspace = FileWorkspace::new(directory.path(), None);
        let mut approve = |_: ApprovalRequest| async { ApprovalDecision::Approved };
        let patch = |arguments: serde_json::Value| {
            let arguments = arguments.to_string();
            (arguments, ())
        };
        // Indentation differs from the file: the fuzzy chain still lands the edit.
        let (arguments, ()) =
            patch(json!({"path": "a.json", "old_string": "\"a\": 1,", "new_string": "\"a\": 10,"}));
        let result = workspace
            .edit(PATCH, &arguments, Some(5), &mut approve)
            .await;
        assert_eq!(result["success"], true, "{result}");
        assert!(result["diff"]
            .as_str()
            .unwrap()
            .contains("-  \"a\": 1,\n+  \"a\": 10,"));
        assert_eq!(result["lint"]["status"], "ok");
        // An edit that breaks the syntax is flagged, not refused.
        let (arguments, ()) =
            patch(json!({"path": "a.json", "old_string": "\"b\": 2", "new_string": "\"b\": ,"}));
        let broken = workspace
            .edit(PATCH, &arguments, Some(5), &mut approve)
            .await;
        assert_eq!(broken["lint"]["status"], "error");
        // A re-sent edit is a no-op, and a miss gets the reference's hint.
        std::fs::write(directory.path().join("b.txt"), "alpha beta gamma delta\n").unwrap();
        let (arguments, ()) = patch(
            json!({"path": "b.txt", "old_string": "completely different words", "new_string": "beta gamma"}),
        );
        let again = workspace
            .edit(PATCH, &arguments, Some(5), &mut approve)
            .await;
        assert_eq!(again["no_change"], true, "{again}");
        let (arguments, ()) =
            patch(json!({"path": "a.json", "old_string": "zzz not here", "new_string": "q"}));
        let miss = workspace
            .edit(PATCH, &arguments, Some(5), &mut approve)
            .await;
        assert!(miss["error"]
            .as_str()
            .unwrap()
            .starts_with("Could not find a match"));
        assert!(miss["_hint"]
            .as_str()
            .unwrap()
            .starts_with("old_string not found"));
    }

    #[test]
    fn search_follows_the_reference_layout_and_ignore_files() {
        use super::FileWorkspace;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        std::fs::write(root.join("ignored.log"), "alpha log\n").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/out.txt"), "alpha build\n").unwrap();
        std::fs::write(root.join("src/a.txt"), "alpha 1\nbeta\nalpha 2\n").unwrap();
        std::fs::write(root.join("src/sub/b.txt"), "alpha 3\nalpha 4\nalpha 5\n").unwrap();
        let mut workspace = FileWorkspace::new(root, None);
        let few = workspace.search(r#"{"pattern": "alpha 1"}"#);
        assert_eq!(few["matches"][0]["path"], "src/a.txt");
        assert!(few.get("truncated").is_none());
        // Five matches or more are grouped by path; ignored files and directories are skipped.
        let many = workspace.search(r#"{"pattern": "alpha"}"#);
        assert_eq!(many["total_count"], 5);
        let text = many["matches_text"].as_str().unwrap();
        assert!(
            text.starts_with("src/a.txt\n  1: alpha 1\n  3: alpha 2\nsrc/sub/b.txt\n  1: alpha 3"),
            "{text}"
        );
        assert!(!text.contains("log") && !text.contains("build"));
        // A page that stops short says how to continue; the same search again warns, then stops.
        let page = workspace.search(r#"{"pattern": "alpha", "limit": 2}"#);
        assert_eq!(page["truncated"], true);
        assert!(page["_hint"].as_str().unwrap().contains("offset=2"));
        let _ = workspace.search(r#"{"pattern": "alpha", "limit": 2}"#);
        let third = workspace.search(r#"{"pattern": "alpha", "limit": 2}"#);
        assert!(third["_warning"].as_str().unwrap().contains("3 times"));
        let fourth = workspace.search(r#"{"pattern": "alpha", "limit": 2}"#);
        assert_eq!(fourth["guardrail_refusal"], true);
    }

    #[tokio::test]
    async fn write_file_creates_directories_and_accepts_large_text() {
        use super::{ApprovalDecision, ApprovalRequest, FileWorkspace, WRITE_FILE};
        use serde_json::json;

        let directory = tempfile::tempdir().unwrap();
        let mut workspace = FileWorkspace::new(directory.path(), None);
        let shown = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&shown);
        let mut approve = move |request: ApprovalRequest| {
            recorder.lock().unwrap().push(request.target.clone());
            async { ApprovalDecision::Approved }
        };
        let big = "x".repeat(300_000);
        let arguments = json!({"path": "new/deep/file.json", "content": "{\"a\": 1}"}).to_string();
        let result = workspace
            .edit(WRITE_FILE, &arguments, Some(5), &mut approve)
            .await;
        assert_eq!(result["dirs_created"], true, "{result}");
        assert_eq!(result["verified"], true);
        assert_eq!(result["lint"]["status"], "ok");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("new/deep/file.json")).unwrap(),
            "{\"a\": 1}"
        );
        let arguments = json!({"path": "new/big.txt", "content": big}).to_string();
        let result = workspace
            .edit(WRITE_FILE, &arguments, Some(5), &mut approve)
            .await;
        assert_eq!(result["bytes_written"], 300_000, "{result}");
        assert_eq!(result["dirs_created"], false);
        let seen = shown.lock().unwrap().clone();
        assert!(
            seen[0].contains("creates directories: new, new/deep"),
            "{seen:?}"
        );
        // Broken JSON is flagged by the syntax check, not refused.
        let mut approve = |_: ApprovalRequest| async { ApprovalDecision::Approved };
        let arguments = json!({"path": "bad.json", "content": "{oops"}).to_string();
        let result = workspace
            .edit(WRITE_FILE, &arguments, Some(5), &mut approve)
            .await;
        assert_eq!(result["lint"]["status"], "error");
        // The workspace stays the limit: a path that climbs out is still refused.
        let arguments = json!({"path": "../escape.txt", "content": "x"}).to_string();
        let result = workspace
            .edit(WRITE_FILE, &arguments, Some(5), &mut approve)
            .await;
        assert!(result["error"].is_string(), "{result}");
        assert!(!directory
            .path()
            .parent()
            .unwrap()
            .join("escape.txt")
            .exists());
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    use crate::terminal::OutputLimits;

    #[test]
    fn read_file_follows_the_configured_output_limits() {
        let directory = tempfile::tempdir().unwrap();
        let body: String = (1..=10)
            .map(|n| format!("{}\n", "x".repeat(n * 10)))
            .collect();
        std::fs::write(directory.path().join("f.txt"), body).unwrap();
        let limits = OutputLimits {
            max_lines: 4,
            max_line_length: 25,
            file_read_max_chars: 100_000,
            ..OutputLimits::default()
        };
        let mut workspace = FileWorkspace::new(directory.path(), None).with_limits(limits);
        let page = workspace.read(r#"{"path": "f.txt", "limit": 50}"#);
        let content = page["content"].as_str().unwrap();
        assert_eq!(content.lines().count(), 4, "{page}");
        assert!(content.contains("... [truncated]"), "{content}");
        assert_eq!(page["truncated"], true);

        let tiny = OutputLimits {
            file_read_max_chars: 40,
            ..OutputLimits::default()
        };
        let mut workspace = FileWorkspace::new(directory.path(), None).with_limits(tiny);
        let page = workspace.read(r#"{"path": "f.txt"}"#);
        assert!(
            page["content"].as_str().unwrap().chars().count() <= 40,
            "{page}"
        );
        assert!(page.get("next_offset").is_some(), "{page}");
    }

    #[tokio::test]
    async fn read_file_reads_spillover_and_document_caches_only_read_only() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let spill_dir = profile.path().join("cache").join("spillover");
        let doc_dir = profile.path().join("cache").join("documents");
        let img_dir = profile.path().join("cache").join("images");
        std::fs::create_dir_all(&spill_dir).unwrap();
        std::fs::create_dir_all(&doc_dir).unwrap();
        std::fs::create_dir_all(&img_dir).unwrap();

        let spill_file = spill_dir.join("call_1.txt");
        let doc_file = doc_dir.join("doc_abc_report.txt");
        let env_file = profile.path().join(".env");
        let img_file = img_dir.join("img_1.png");
        std::fs::write(&spill_file, "alpha\nbeta\ngamma\n").unwrap();
        std::fs::write(&doc_file, "doc body\n").unwrap();
        std::fs::write(&env_file, "SECRET=do-not-read\n").unwrap();
        std::fs::write(&img_file, "not text").unwrap();

        let mut workspace = FileWorkspace::new(workspace_dir.path(), Some(profile.path()));

        // Allowed: read_file on spillover with offset/limit and on documents.
        let read_spill = workspace.read(
            &json!({"path": spill_file.display().to_string(), "offset": 2, "limit": 1}).to_string(),
        );
        assert_eq!(read_spill["total_lines"], 3, "{read_spill}");
        assert!(read_spill["content"].as_str().unwrap().contains("2|beta"));

        let read_doc = workspace.read(&json!({"path": doc_file.display().to_string()}).to_string());
        assert!(
            read_doc["content"].as_str().unwrap().contains("doc body"),
            "{read_doc}"
        );

        // Denied: profile .env, traversal out of spillover, images dir, and any write_file.
        let read_env = workspace.read(&json!({"path": env_file.display().to_string()}).to_string());
        assert!(read_env["error"].is_string(), "expected error: {read_env}");

        let traversal = spill_dir.join("..").join("..").join(".env");
        let read_trav =
            workspace.read(&json!({"path": traversal.display().to_string()}).to_string());
        assert!(
            read_trav["error"].is_string(),
            "expected traversal error: {read_trav}"
        );

        let read_img = workspace.read(&json!({"path": img_file.display().to_string()}).to_string());
        assert!(
            read_img["error"].is_string(),
            "expected image dir error: {read_img}"
        );

        let mut approve = |_: ApprovalRequest| async { ApprovalDecision::Approved };
        let write_spill = workspace
            .edit(
                WRITE_FILE,
                &json!({"path": spill_file.display().to_string(), "content": "hijack"}).to_string(),
                Some(5),
                &mut approve,
            )
            .await;
        assert!(
            write_spill["error"].is_string(),
            "expected write refusal: {write_spill}"
        );
        assert_eq!(
            std::fs::read_to_string(&spill_file).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn read_file_extracts_documents_and_write_guards_reject_binary_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let nb = json!({
            "cells": [
                {"cell_type": "markdown", "source": "# Notebook Heading"},
                {"cell_type": "code", "source": "x = 1", "outputs": [{"output_type": "stream", "text": "ok\n"}]}
            ]
        });
        std::fs::write(dir.path().join("demo.ipynb"), nb.to_string()).unwrap();

        let db_path = dir.path().join("sample.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT); INSERT INTO users VALUES (1, 'a@b.c');").unwrap();
        }
        std::fs::write(dir.path().join("sample.db-wal"), b"walbytes").unwrap();
        std::fs::write(
            dir.path().join("doc.pdf"),
            b"%PDF-1.4\n1 0 obj\n<< /Length 25 >>\nstream\nBT (Invoice 99) Tj ET\nendstream\nendobj\n",
        )
        .unwrap();

        let mut workspace = FileWorkspace::new(dir.path(), None);

        let nb_res = workspace.read(r#"{"path": "demo.ipynb", "offset": 1, "limit": 2}"#);
        assert_eq!(nb_res["extracted_document"], true, "{nb_res}");
        assert_eq!(nb_res["truncated"], true, "{nb_res}");
        assert!(nb_res["content"]
            .as_str()
            .unwrap()
            .contains("1|# ── Markdown cell 1 ──"));

        let db_res = workspace.read(r#"{"path": "sample.db"}"#);
        assert_eq!(db_res["extracted_document"], true, "{db_res}");
        assert!(
            db_res["content"]
                .as_str()
                .unwrap()
                .contains("CREATE TABLE users"),
            "{db_res}"
        );

        let pdf_res = workspace.read(r#"{"path": "doc.pdf"}"#);
        assert_eq!(pdf_res["extracted_document"], true, "{pdf_res}");
        assert!(
            pdf_res["content"].as_str().unwrap().contains("Invoice 99"),
            "{pdf_res}"
        );

        // Binary document overwrite & SQLite sidecar writes must be rejected even after read.
        let mut approve = |_: ApprovalRequest| async { ApprovalDecision::Approved };
        let err_db = workspace
            .edit(
                WRITE_FILE,
                &json!({"path": "sample.db", "content": "corrupt"}).to_string(),
                Some(5),
                &mut approve,
            )
            .await;
        assert!(
            err_db["error"]
                .as_str()
                .unwrap()
                .contains("binary document"),
            "{err_db}"
        );

        let err_wal = workspace
            .edit(
                WRITE_FILE,
                &json!({"path": "sample.db-wal", "content": "corrupt"}).to_string(),
                Some(5),
                &mut approve,
            )
            .await;
        assert!(
            err_wal["error"]
                .as_str()
                .unwrap()
                .contains("SQLite sidecar"),
            "{err_wal}"
        );

        // Existing .ipynb remains writable as UTF-8 after full read.
        let nb_full = workspace.read(r#"{"path": "demo.ipynb"}"#);
        assert_eq!(nb_full["truncated"], false, "{nb_full}");
        let bad_magic = dir.path().join("corrupt.db");
        std::fs::write(&bad_magic, b"not sqlite").unwrap();
        let err_magic = workspace.read(r#"{"path": "corrupt.db"}"#);
        assert!(
            err_magic["error"]
                .as_str()
                .unwrap()
                .contains("document extraction failed"),
            "{err_magic}"
        );
    }
}
