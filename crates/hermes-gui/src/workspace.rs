//! Project working directory selection, recent workspaces persistence, and safe file attachment
//! into the active workspace (`<workspace>/.hermes-attachments/` for external files).

use std::fs;
use std::path::{Component, Path, PathBuf};

use hermes_runtime::redact::is_secret_file_arg;
use serde::{Deserialize, Serialize};

const GUI_STATE_FILE: &str = "gui_state.json";
const ATTACHMENTS_DIR: &str = ".hermes-attachments";
const MAX_RECENT_WORKSPACES: usize = 5;
const MAX_ATTACH_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WorkspacePrefs {
    pub current: Option<String>,
    #[serde(default)]
    pub recent: Vec<String>,
}

impl WorkspacePrefs {
    pub fn load(profile: &Path) -> Self {
        let path = profile.join(GUI_STATE_FILE);
        fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn resolve_initial(&self) -> PathBuf {
        if let Some(ref saved) = self.current {
            let p = PathBuf::from(saved);
            if p.is_dir() {
                return p;
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    pub fn record_and_save(&mut self, profile: &Path, dir: &Path) {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let text = canonical.display().to_string();
        self.current = Some(text.clone());
        self.recent.retain(|item| item != &text);
        self.recent.insert(0, text);
        self.recent.truncate(MAX_RECENT_WORKSPACES);

        let _ = fs::create_dir_all(profile);
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(profile.join(GUI_STATE_FILE), json);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachedFile {
    /// Workspace-relative POSIX-style path (e.g. `src/main.rs` or `.hermes-attachments/spec.pdf`).
    pub rel_path: String,
    /// Display filename for UI chip.
    pub display_name: String,
    /// True if the file was copied from outside the project into `<workspace>/.hermes-attachments/`.
    pub imported: bool,
    /// True if binary/media/document (model should inspect via `read_file` / `vision_analyze`).
    pub is_media_or_doc: bool,
    pub size_bytes: u64,
}

/// Expand `~` and resolve `raw` as an existing directory path.
pub fn resolve_workspace_dir(raw: &str, current: &Path) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Workspace path cannot be empty.".into());
    }
    let expanded = expand_tilde(trimmed);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        current.join(expanded)
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|_| format!("Directory not found: {trimmed}"))?;
    if !canonical.is_dir() {
        return Err(format!("Not a directory: {}", canonical.display()));
    }
    Ok(canonical)
}

/// Attach a file (or directory) to `workspace_root`:
/// - If `source` is already inside `workspace_root`, returns its workspace-relative path directly.
/// - If `source` is outside `workspace_root`, copies it into `<workspace_root>/.hermes-attachments/`
///   (deduplicating filenames, enforcing 25 MiB limit, and refusing secret/credential files).
pub fn attach_path_to_workspace(
    source: &Path,
    workspace_root: &Path,
) -> Result<AttachedFile, String> {
    let canon_root = workspace_root
        .canonicalize()
        .map_err(|e| format!("Invalid workspace directory: {e}"))?;
    let canon_src = source
        .canonicalize()
        .map_err(|_| format!("File not found: {}", source.display()))?;
    if !canon_src.is_file() {
        return Err(format!(
            "Only files can be attached (select a folder with the Folder picker): {}",
            source.display()
        ));
    }
    if canon_src
        .to_str()
        .is_some_and(|p| is_secret_file_arg(p, &[]))
    {
        return Err(format!(
            "Refused to attach secret/credential file: {}",
            canon_src.display()
        ));
    }

    let meta = fs::metadata(&canon_src)
        .map_err(|e| format!("Cannot read file metadata for {}: {e}", canon_src.display()))?;
    let size_bytes = meta.len();
    if size_bytes > MAX_ATTACH_BYTES {
        return Err(format!(
            "File exceeds {} MiB attachment limit ({} bytes).",
            MAX_ATTACH_BYTES / (1024 * 1024),
            size_bytes
        ));
    }

    let file_name = canon_src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "File name must be valid UTF-8".to_owned())?
        .to_owned();
    let is_media_or_doc = is_binary_or_rich_doc(&canon_src);

    if let Ok(rel) = canon_src.strip_prefix(&canon_root) {
        let rel_path = path_to_posix(rel);
        return Ok(AttachedFile {
            rel_path,
            display_name: file_name,
            imported: false,
            is_media_or_doc,
            size_bytes,
        });
    }

    // External file: safely copy into `<workspace_root>/.hermes-attachments/`
    let attach_dir = canon_root.join(ATTACHMENTS_DIR);
    fs::create_dir_all(&attach_dir)
        .map_err(|e| format!("Cannot create {ATTACHMENTS_DIR} in workspace: {e}"))?;
    let target = unique_dest_path(&attach_dir, &file_name);
    fs::copy(&canon_src, &target)
        .map_err(|e| format!("Failed copying {} into workspace: {e}", canon_src.display()))?;

    let final_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&file_name)
        .to_owned();
    let rel_path = format!("{ATTACHMENTS_DIR}/{final_name}");
    Ok(AttachedFile {
        rel_path,
        display_name: final_name,
        imported: true,
        is_media_or_doc,
        size_bytes,
    })
}

/// Persist raw clipboard bytes (e.g. a pasted screenshot PNG) into
/// `<workspace_root>/.hermes-attachments/` with deduplicated filename and attach it.
pub fn attach_bytes_to_workspace(
    bytes: &[u8],
    base_name: &str,
    workspace_root: &Path,
) -> Result<AttachedFile, String> {
    if bytes.is_empty() {
        return Err("Clipboard image is empty.".into());
    }
    let size_bytes = bytes.len() as u64;
    if size_bytes > MAX_ATTACH_BYTES {
        return Err(format!(
            "Clipboard image exceeds {} MiB attachment limit ({} bytes).",
            MAX_ATTACH_BYTES / (1024 * 1024),
            size_bytes
        ));
    }
    let canon_root = workspace_root
        .canonicalize()
        .map_err(|e| format!("Invalid workspace directory: {e}"))?;
    let attach_dir = canon_root.join(ATTACHMENTS_DIR);
    fs::create_dir_all(&attach_dir)
        .map_err(|e| format!("Cannot create {ATTACHMENTS_DIR} in workspace: {e}"))?;
    let target = unique_dest_path(&attach_dir, base_name);
    fs::write(&target, bytes).map_err(|e| format!("Failed writing clipboard attachment: {e}"))?;
    let final_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(base_name)
        .to_owned();
    let rel_path = format!("{ATTACHMENTS_DIR}/{final_name}");
    Ok(AttachedFile {
        rel_path,
        display_name: final_name,
        imported: true,
        is_media_or_doc: true,
        size_bytes,
    })
}

/// Combine user prompt text with attached workspace files:
/// - Text files use `@file:<rel_path>` so `context_refs` injects their content automatically.
/// - Images/PDFs/Office docs note their workspace-relative path so the model inspects them via
///   `read_file` (multimodal / doc extraction) or `vision_analyze`.
pub fn format_prompt_with_attachments(user_text: &str, attachments: &[AttachedFile]) -> String {
    if attachments.is_empty() {
        return user_text.to_owned();
    }
    let mut parts = Vec::new();
    let trimmed = user_text.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_owned());
    }
    let mut text_refs = Vec::new();
    let mut media_notes = Vec::new();
    for att in attachments {
        if att.is_media_or_doc {
            media_notes.push(format!(
                "- `{}` ({}, {})",
                att.rel_path,
                format_size(att.size_bytes),
                if att.imported {
                    "copied into workspace"
                } else {
                    "workspace file"
                }
            ));
        } else {
            text_refs.push(format!("@file:{}", att.rel_path));
        }
    }
    if !text_refs.is_empty() {
        parts.push(text_refs.join(" "));
    }
    if !media_notes.is_empty() {
        parts.push(format!(
            "Attached workspace files (inspect with read_file / vision_analyze):\n{}",
            media_notes.join("\n")
        ));
    }
    parts.join("\n\n")
}

pub fn short_folder_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(raw)
}

fn path_to_posix(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn unique_dest_path(dir: &Path, filename: &str) -> PathBuf {
    let clean: String = filename
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let candidate = dir.join(&clean);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(&clean);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment");
    let ext = path.extension().and_then(|e| e.to_str());
    for idx in 2..10_000 {
        let name = match ext {
            Some(e) => format!("{stem}_{idx}.{e}"),
            None => format!("{stem}_{idx}"),
        };
        let next = dir.join(name);
        if !next.exists() {
            return next;
        }
    }
    dir.join(format!("{stem}_copy"))
}

fn is_binary_or_rich_doc(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "pdf"
            | "docx"
            | "xlsx"
            | "pptx"
            | "db"
            | "sqlite"
            | "sqlite3"
            | "zip"
            | "tar"
            | "gz"
            | "mp3"
            | "wav"
            | "ogg"
            | "mp4"
    ) {
        return true;
    }
    fs::read(path)
        .ok()
        .is_some_and(|bytes| bytes.iter().take(4096).any(|&b| b == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaches_in_workspace_file_by_rel_path_and_copies_external_file() {
        let base = std::env::temp_dir().join(format!("hsr-gui-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let profile = base.join("profile");
        let work = base.join("project");
        let downloads = base.join("downloads");
        fs::create_dir_all(work.join("src")).unwrap();
        fs::create_dir_all(&downloads).unwrap();

        let in_ws = work.join("src/main.rs");
        fs::write(&in_ws, "fn main() {}\n").unwrap();
        let ext_txt = downloads.join("notes.md");
        fs::write(&ext_txt, "# Notes\n").unwrap();
        let ext_img = downloads.join("shot.png");
        fs::write(&ext_img, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let secret = downloads.join(".env");
        fs::write(&secret, "SECRET=1\n").unwrap();

        let a1 = attach_path_to_workspace(&in_ws, &work).unwrap();
        assert_eq!(a1.rel_path, "src/main.rs");
        assert!(!a1.imported);
        assert!(!a1.is_media_or_doc);

        let a2 = attach_path_to_workspace(&ext_txt, &work).unwrap();
        assert_eq!(a2.rel_path, ".hermes-attachments/notes.md");
        assert!(a2.imported);
        assert!(work.join(".hermes-attachments/notes.md").is_file());

        // Second copy of same filename deduplicates to notes_2.md
        let a2_dup = attach_path_to_workspace(&ext_txt, &work).unwrap();
        assert_eq!(a2_dup.rel_path, ".hermes-attachments/notes_2.md");

        let a3 = attach_path_to_workspace(&ext_img, &work).unwrap();
        assert!(a3.is_media_or_doc);
        assert!(a3.imported);

        // Secret file is refused
        assert!(attach_path_to_workspace(&secret, &work).is_err());

        let prompt = format_prompt_with_attachments("Analyze these", &[a1, a3]);
        assert!(prompt.contains("@file:src/main.rs"));
        assert!(prompt.contains(".hermes-attachments/shot.png"));

        // Clipboard byte attachment persists and deduplicates cleanly
        let clip1 =
            attach_bytes_to_workspace(b"\x89PNG\r\n\x1a\n1234", "clipboard_image.png", &work)
                .unwrap();
        assert_eq!(clip1.rel_path, ".hermes-attachments/clipboard_image.png");
        assert!(clip1.is_media_or_doc);
        let clip2 =
            attach_bytes_to_workspace(b"\x89PNG\r\n\x1a\n5678", "clipboard_image.png", &work)
                .unwrap();
        assert_eq!(clip2.rel_path, ".hermes-attachments/clipboard_image_2.png");
        assert!(attach_bytes_to_workspace(b"", "clipboard_image.png", &work).is_err());

        // Persistence round-trip
        let mut prefs = WorkspacePrefs::load(&profile);
        prefs.record_and_save(&profile, &work);
        let reloaded = WorkspacePrefs::load(&profile);
        assert_eq!(reloaded.resolve_initial(), work.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&base);
    }
}
