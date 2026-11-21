use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

use ring::digest::{digest, SHA256};
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
            let plan = match if tool == PATCH {
                self.prepare_patch(arguments)
            } else {
                self.prepare_write(arguments)
            } {
                Ok(plan) => plan,
                Err(error) => return json!({"error": error}),
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
                    self.observed.insert(plan.target.path, observation);
                    if let Some((count, diff)) = plan.patch {
                        json!({"success": true, "files_modified": [plan.display_path], "replacements": count,
                               "diff": diff, "verified": true})
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
    fn prepare_patch(&self, arguments: &str) -> Result<linux::WritePlan, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            mode: Option<String>,
            path: String,
            old_string: String,
            new_string: String,
            #[serde(default)]
            replace_all: bool,
        }
        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "patch expects path, old_string, and new_string")?;
        if args.mode.as_deref().is_some_and(|mode| mode != "replace") {
            return Err("patch supports exact replacement mode".into());
        }
        if args.old_string.is_empty()
            || args.old_string == args.new_string
            || args.new_string.contains('\0')
        {
            return Err(
                "patch requires a nonempty match and different UTF-8 replacement text".into(),
            );
        }
        let target = self.write_target(&args.path)?;
        let (before, original) = target
            .read_version()?
            .ok_or("The file to patch does not exist")?;
        let count = original.matches(&args.old_string).count();
        match count {
            0 => return Err("The exact old_string was not found".into()),
            2.. if !args.replace_all => {
                return Err(
                    "old_string has multiple matches. Include more context or set replace_all."
                        .into(),
                )
            }
            _ => {}
        }
        let result_len = original.len() - count * args.old_string.len();
        if result_len.saturating_add(count.saturating_mul(args.new_string.len())) > WRITE_LIMIT {
            return Err("The patched file would exceed 64 KiB".into());
        }
        let content = original.replace(&args.old_string, &args.new_string);
        let diff = replacement_diff(&args.path, &original, &content);
        Ok(linux::WritePlan {
            target,
            content,
            before: Some(before),
            display_path: args.path,
            patch: Some((count, diff)),
        })
    }
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
    json!({"type": "function", "function": {
        "name": PATCH,
        "description": "Replace exact text in an existing UTF-8 workspace file after approval. Provide context for a unique match or explicitly request replace_all.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"},
            "replace_all": {"type": "boolean", "default": false}
        }, "required": ["path", "old_string", "new_string"], "additionalProperties": false}
    }})
}

#[cfg(target_os = "linux")]
fn replacement_diff(path: &str, before: &str, after: &str) -> String {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = head.saturating_sub(3);
    let old_end = old.len() - tail;
    let new_end = new.len() - tail;
    let context = tail.min(3);
    let old_count = old_end + context - start;
    let new_count = new_end + context - start;
    let quote_path = serde_json::to_string(path).expect("path string");
    let mut diff = format!(
        "--- {quote_path}\n+++ {quote_path}\n@@ -{},{} +{},{} @@\n",
        start + usize::from(old_count > 0),
        old_count,
        start + usize::from(new_count > 0),
        new_count
    );
    for (prefix, lines) in [
        (' ', &old[start..head]),
        ('-', &old[head..old_end]),
        ('+', &new[head..new_end]),
        (' ', &old[old_end..old_end + context]),
    ] {
        for line in lines {
            diff.push(prefix);
            diff.push_str(line);
            if !line.ends_with('\n') {
                diff.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    diff
}

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
}
