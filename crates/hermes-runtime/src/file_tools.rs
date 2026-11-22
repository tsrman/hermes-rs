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
#[cfg(target_os = "linux")]
mod linux;
mod search;

pub(crate) use search::{search_schema, SEARCH_FILES};

const READ_LIMIT: u64 = 10_000_000;
const WRITE_LIMIT: usize = 65_536;
pub(crate) const WRITE_FILE: &str = "write_file";
pub(crate) const PATCH: &str = "patch";

#[derive(Deserialize, Serialize)]
pub struct ApprovalRequest {
    pub target: String,
    pub content: String,
    pub action: ApprovalAction,
    pub timeout_secs: u32,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub enum ApprovalAction {
    CreateFile,
    ReplaceFile,
    RunCommand,
    /// Changes to skills: `content` describes every operation.
    ManageSkills,
    /// Questions for the person: `content` holds them as JSON and the reply is `Answered`.
    Clarify,
}

#[derive(Clone, Deserialize, Serialize)]
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
    observed: HashMap<PathBuf, Observation>,
    /// Misses per file, so a model that keeps retrying a patch is told to stop.
    patch_failures: HashMap<PathBuf, u32>,
    /// The last search and how many times in a row it ran.
    search_repeat: (Option<String>, u32),
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
        let profile = profile.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_owned()));
        let profile = profile.map(|path| {
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
            observed: HashMap::new(),
            patch_failures: HashMap::new(),
            search_repeat: (None, 0),
        }
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
            .min(2000);
        let (root, resolved) = self.resolve(path)?;
        let file = open_read(&root, &resolved)?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > READ_LIMIT {
            return Err("File must be a regular file no larger than 10 MB".into());
        }
        let mut data = String::new();
        file.take(READ_LIMIT + 1)
            .read_to_string(&mut data)
            .map_err(|error| error.to_string())?;
        if data.len() as u64 > READ_LIMIT || data.contains('\0') {
            return Err("File must be UTF-8 text no larger than 10 MB".into());
        }
        let lines: Vec<&str> = data.lines().collect();
        let start = usize::try_from(offset - 1)
            .unwrap_or(usize::MAX)
            .min(lines.len());
        let end = start.saturating_add(limit as usize).min(lines.len());
        let content = lines[start..end]
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}|{}", start + index + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        // File content reaches the model with credentials masked by a sentinel it cannot write
        // back as a usable key; a file known to hold credentials gets every pass.
        let homes: Vec<&Path> = self.profile.iter().map(PathBuf::as_path).collect();
        let content = redact_file_read(&content, is_secret_file_arg(path, &homes));
        if data.len() <= WRITE_LIMIT {
            let hash = fingerprint(data.as_bytes());
            let observation = self
                .observed
                .entry(resolved)
                .or_insert_with(|| Observation::new(hash));
            if observation.hash != hash {
                *observation = Observation::new(hash);
            }
            observation.add(start, end, lines.len());
        }
        Ok(
            json!({"content": content, "total_lines": lines.len(), "file_size": data.len(),
                  "truncated": end < lines.len(), "is_binary": false, "is_image": false, "not_found": false}),
        )
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
        let Some(seconds) = timeout_secs.filter(|_| cfg!(target_os = "linux")) else {
            return approval_error(ApprovalDecision::Unavailable);
        };
        #[cfg(target_os = "linux")]
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
            let request = ApprovalRequest {
                target: target.to_owned(),
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
                        json!({"success": true, "path": plan.display_path, "bytes_written": plan.content.len(), "verified": true})
                    }
                }
                Err(error) => json!({"error": error}),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (tool, arguments, seconds, approve);
            approval_error(ApprovalDecision::Unavailable)
        }
    }

    #[cfg(target_os = "linux")]
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
            return Err("write_file accepts text up to 64 KiB".into());
        }
        let target = self.write_target(&args.path)?;
        let before = target.inspect()?;
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
            target,
            content: args.content,
            before,
            display_path: args.path,
            patch: None,
        })
    }

    #[cfg(target_os = "linux")]
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
        ]
        .iter()
        .any(|directory| target.starts_with(directory))
        {
            return Err("Refusing to write to a protected system path".into());
        }
        linux::Target::open(&root, &target)
    }

    #[cfg(target_os = "linux")]
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
            return Err("The patched file would exceed 64 KiB".into());
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
            target,
            content: written,
            before: Some(before),
            display_path: args.path,
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
enum Prepared {
    Plan(Box<linux::WritePlan>),
    Done(Value),
}

fn open_read(root: &Path, path: &Path) -> Result<std::fs::File, String> {
    #[cfg(target_os = "linux")]
    return linux::Target::open(root, path)?.read_file();
    #[cfg(not(target_os = "linux"))]
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
    tokio::time::timeout(
        Duration::from_secs(u64::from(request.timeout_secs)),
        approve(request),
    )
    .await
    .unwrap_or(ApprovalDecision::TimedOut)
}

pub(crate) fn write_schema() -> Value {
    json!({"type": "function", "function": {
        "name": WRITE_FILE,
        "description": "Create or replace a UTF-8 text file in the working directory after human approval. For an existing file, read the entire file first. The parent directory must exist.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string"}, "content": {"type": "string"}
        }, "required": ["path", "content"], "additionalProperties": false}
    }})
}

pub(crate) fn patch_schema() -> Value {
    json!({"type": "function", "function":
        serde_json::from_str::<Value>(include_str!("schemas/patch.json")).unwrap_or_default()})
}

#[cfg(target_os = "linux")]
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
}
