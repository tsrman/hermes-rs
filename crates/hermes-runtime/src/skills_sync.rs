//! Sync and seed bundled Hermes skills into `<profile>/skills/`.
//!
//! Supports:
//! - Downloading the official upstream `NousResearch/hermes-agent` archive (or a custom URL via
//!   `HERMES_SKILLS_ARCHIVE_URL` / `--skills-url`) and extracting entries under `skills/` using
//!   the crate's pure-Rust ZIP/DEFLATE reader (zero external dependencies).
//! - Copying skills from a local directory (such as an existing `~/.hermes/skills` profile).
//! - Respecting `<profile>/.no-bundled-skills` opt-out and `<profile>/skills/.bundled_manifest`
//!   so user edits and intentional deletions are never silently overwritten unless `force = true`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::doc_extract::ZipArchive;

pub const DEFAULT_SKILLS_ARCHIVE_URL: &str =
    "https://github.com/NousResearch/hermes-agent/archive/refs/heads/main.zip";
pub const SKILLS_ARCHIVE_URL_ENV: &str = "HERMES_SKILLS_ARCHIVE_URL";
const BUNDLED_MANIFEST: &str = ".bundled_manifest";
const NO_BUNDLED_MARKER: &str = ".no-bundled-skills";
const MAX_ARCHIVE_BYTES: usize = 80 * 1024 * 1024;
const MAX_SKILL_FILES: usize = 4096;

#[derive(Clone, Debug, Default, Serialize)]
pub struct SkillsSyncReport {
    pub source: String,
    pub installed: Vec<String>,
    pub updated: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub skipped_deleted: Vec<String>,
    pub opted_out: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SkillsSyncOptions {
    pub url: Option<String>,
    pub from_dir: Option<PathBuf>,
    pub force: bool,
}

/// Whether the profile has opted out of automatic bundled skills seeding.
pub fn is_opted_out(home: &Path) -> bool {
    home.join(NO_BUNDLED_MARKER).exists()
}

pub fn set_opt_out(home: &Path, opt_out: bool) -> Result<(), String> {
    fs::create_dir_all(home)
        .map_err(|error| format!("cannot create profile directory: {error}"))?;
    let marker = home.join(NO_BUNDLED_MARKER);
    if opt_out {
        fs::write(&marker, "opted out via hsr skills opt-out\n")
            .map_err(|error| format!("cannot write {NO_BUNDLED_MARKER}: {error}"))?;
    } else if marker.exists() {
        fs::remove_file(&marker)
            .map_err(|error| format!("cannot remove {NO_BUNDLED_MARKER}: {error}"))?;
    }
    Ok(())
}

/// List all installed skill relative paths (folders containing `SKILL.md`) under `<home>/skills/`.
pub fn list_installed_skills(home: &Path) -> Vec<String> {
    let root = home.join("skills");
    let mut found = BTreeSet::new();
    collect_skill_dirs(&root, &root, &mut found);
    found.into_iter().collect()
}

fn collect_skill_dirs(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with('.') {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if path.join("SKILL.md").is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    let rel_str = rel
                        .components()
                        .filter_map(|c| match c {
                            Component::Normal(s) => s.to_str(),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("/");
                    if !rel_str.is_empty() {
                        out.insert(rel_str);
                    }
                }
            } else {
                collect_skill_dirs(root, &path, out);
            }
        }
    }
}

/// Detect an existing `~/.hermes/skills` (or `~/.hsr/skills`) directory with skills that differs
/// from `home`, so `hsr setup` can offer copying skills locally without network access.
pub fn detect_sibling_skills_dir(home: &Path) -> Option<(PathBuf, usize)> {
    let user_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    let current_canon = home.canonicalize().ok();
    for candidate_root in [user_home.join(".hermes"), user_home.join(".hsr")] {
        if current_canon
            .as_ref()
            .is_some_and(|cur| candidate_root.canonicalize().ok().as_ref() == Some(cur))
        {
            continue;
        }
        if candidate_root == home {
            continue;
        }
        let skills_dir = candidate_root.join("skills");
        if skills_dir.is_dir() {
            let count = list_installed_skills(&candidate_root).len();
            if count > 0 {
                return Some((skills_dir, count));
            }
        }
    }
    None
}

/// Download or copy bundled skills into `<home>/skills/`.
pub async fn sync_skills(home: &Path, opts: SkillsSyncOptions) -> Result<SkillsSyncReport, String> {
    if is_opted_out(home) && !opts.force {
        return Ok(SkillsSyncReport {
            source: "opted-out".into(),
            opted_out: true,
            ..SkillsSyncReport::default()
        });
    }

    if let Some(src_dir) = opts.from_dir {
        return sync_skills_from_dir(home, &src_dir, opts.force);
    }

    let url = opts
        .url
        .or_else(|| {
            std::env::var(SKILLS_ARCHIVE_URL_ENV)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_SKILLS_ARCHIVE_URL.to_owned());

    let bytes = download_archive(&url).await?;
    sync_skills_from_zip_bytes(home, &bytes, &url, opts.force)
}

async fn download_archive(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("hsr-skills-sync/1")
        .build()
        .map_err(|error| format!("cannot build HTTP client for skills sync: {error}"))?;

    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("failed downloading skills archive from {url}: {error}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "skills archive download failed with HTTP {} from {url}",
            resp.status().as_u16()
        ));
    }

    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|error| format!("failed reading skills archive body: {error}"))?
    {
        if buf.len().saturating_add(chunk.len()) > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "skills archive exceeds maximum allowed size ({MAX_ARCHIVE_BYTES} bytes)"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Extract skills under `skills/` from a ZIP archive byte slice into `<home>/skills/`.
pub fn sync_skills_from_zip_bytes(
    home: &Path,
    zip_bytes: &[u8],
    source_label: &str,
    force: bool,
) -> Result<SkillsSyncReport, String> {
    if is_opted_out(home) && !force {
        return Ok(SkillsSyncReport {
            source: source_label.to_owned(),
            opted_out: true,
            ..SkillsSyncReport::default()
        });
    }

    let archive = ZipArchive::from_bytes(zip_bytes, "skills ZIP archive")?;

    // Collect every normalized path under `skills/` (handling both `<repo-root>/skills/...` and
    // direct `skills/...` layouts).
    let mut files_by_rel: BTreeMap<String, String> = BTreeMap::new();
    for entry_name in archive.entry_names() {
        if entry_name.ends_with('/') {
            continue;
        }
        let Some(rel_under_skills) = strip_skills_prefix(entry_name) else {
            continue;
        };
        if !is_safe_rel_path(&rel_under_skills) {
            continue;
        }
        files_by_rel.insert(rel_under_skills, entry_name.clone());
        if files_by_rel.len() >= MAX_SKILL_FILES {
            break;
        }
    }

    // Group files by skill directory (any folder containing `SKILL.md`).
    let skill_dirs: BTreeSet<String> = files_by_rel
        .keys()
        .filter_map(|rel| rel.strip_suffix("/SKILL.md").map(str::to_owned))
        .filter(|dir| !dir.is_empty())
        .collect();

    if skill_dirs.is_empty() {
        return Err(format!(
            "no skills (*/SKILL.md) found under skills/ in {source_label}"
        ));
    }

    let dest_root = home.join("skills");
    fs::create_dir_all(&dest_root)
        .map_err(|error| format!("cannot create skills directory: {error}"))?;
    let mut manifest = read_manifest(&dest_root);
    let mut report = SkillsSyncReport {
        source: source_label.to_owned(),
        ..SkillsSyncReport::default()
    };

    for skill in &skill_dirs {
        let skill_path = dest_root.join(skill);
        let exists = skill_path.join("SKILL.md").is_file();
        let in_manifest = manifest.contains(skill);

        if exists && !force {
            report.skipped_existing.push(skill.clone());
            manifest.insert(skill.clone());
            continue;
        }
        if !exists && in_manifest && !force {
            report.skipped_deleted.push(skill.clone());
            continue;
        }

        let prefix = format!("{skill}/");
        for (rel, zip_name) in files_by_rel.range(prefix.clone()..) {
            if !rel.starts_with(&prefix) {
                break;
            }
            let data = archive.read_entry(zip_name)?;
            write_skill_file(&dest_root, rel, &data)?;
        }

        manifest.insert(skill.clone());
        if exists {
            report.updated.push(skill.clone());
        } else {
            report.installed.push(skill.clone());
        }
    }

    write_manifest(&dest_root, &manifest)?;
    Ok(report)
}

/// Copy skills from an existing local directory (e.g. `~/.hermes/skills`) into `<home>/skills/`.
pub fn sync_skills_from_dir(
    home: &Path,
    src_root: &Path,
    force: bool,
) -> Result<SkillsSyncReport, String> {
    if is_opted_out(home) && !force {
        return Ok(SkillsSyncReport {
            source: src_root.display().to_string(),
            opted_out: true,
            ..SkillsSyncReport::default()
        });
    }
    if !src_root.is_dir() {
        return Err(format!(
            "skills source directory does not exist: {}",
            src_root.display()
        ));
    }

    let mut skill_dirs = BTreeSet::new();
    collect_skill_dirs(src_root, src_root, &mut skill_dirs);
    if skill_dirs.is_empty() {
        return Err(format!(
            "no skills (*/SKILL.md) found in {}",
            src_root.display()
        ));
    }

    let dest_root = home.join("skills");
    fs::create_dir_all(&dest_root)
        .map_err(|error| format!("cannot create skills directory: {error}"))?;
    let mut manifest = read_manifest(&dest_root);
    let mut report = SkillsSyncReport {
        source: src_root.display().to_string(),
        ..SkillsSyncReport::default()
    };

    for skill in &skill_dirs {
        let dst_skill = dest_root.join(skill);
        let exists = dst_skill.join("SKILL.md").is_file();
        let in_manifest = manifest.contains(skill);

        if exists && !force {
            report.skipped_existing.push(skill.clone());
            manifest.insert(skill.clone());
            continue;
        }
        if !exists && in_manifest && !force {
            report.skipped_deleted.push(skill.clone());
            continue;
        }

        let src_skill = src_root.join(skill);
        copy_skill_tree(&src_skill, &dest_root, skill)?;
        manifest.insert(skill.clone());
        if exists {
            report.updated.push(skill.clone());
        } else {
            report.installed.push(skill.clone());
        }
    }

    write_manifest(&dest_root, &manifest)?;
    Ok(report)
}

fn copy_skill_tree(src_dir: &Path, dest_root: &Path, rel_prefix: &str) -> Result<(), String> {
    let entries = fs::read_dir(src_dir)
        .map_err(|error| format!("cannot read {}: {error}", src_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with('.') {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let rel = format!("{rel_prefix}/{name_str}");
        if !is_safe_rel_path(&rel) {
            continue;
        }
        if meta.is_dir() {
            copy_skill_tree(&path, dest_root, &rel)?;
        } else if meta.is_file() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            write_skill_file(dest_root, &rel, &bytes)?;
        }
    }
    Ok(())
}

fn write_skill_file(dest_root: &Path, rel: &str, data: &[u8]) -> Result<(), String> {
    let target = dest_root.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "cannot create skill directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let parent = target.parent().unwrap_or(dest_root);
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot stage skill file {rel}: {error}"))?;
    tmp.write_all(data)
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|error| format!("cannot write skill file {rel}: {error}"))?;
    tmp.persist(&target)
        .map_err(|error| format!("cannot save skill file {rel}: {}", error.error))?;
    Ok(())
}

fn strip_skills_prefix(entry_name: &str) -> Option<String> {
    let normalized = entry_name.replace('\\', "/");
    let trimmed = normalized.trim_start_matches("./").trim_start_matches('/');
    if let Some(rest) = trimmed.strip_prefix("skills/") {
        return (!rest.is_empty()).then(|| rest.to_owned());
    }
    // GitHub repo archives wrap files in `<repo>-<ref>/skills/...`
    let (_, after_root) = trimmed.split_once('/')?;
    let rest = after_root.strip_prefix("skills/")?;
    (!rest.is_empty()).then(|| rest.to_owned())
}

fn is_safe_rel_path(rel: &str) -> bool {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return false;
    }
    let mut has_part = false;
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.starts_with('.') {
            return false;
        }
        has_part = true;
    }
    has_part
}

fn read_manifest(skills_dir: &Path) -> BTreeSet<String> {
    let Ok(raw) = fs::read_to_string(skills_dir.join(BUNDLED_MANIFEST)) else {
        return BTreeSet::new();
    };
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn write_manifest(skills_dir: &Path, entries: &BTreeSet<String>) -> Result<(), String> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(entry);
        out.push('\n');
    }
    let mut tmp = tempfile::NamedTempFile::new_in(skills_dir)
        .map_err(|error| format!("cannot stage {BUNDLED_MANIFEST}: {error}"))?;
    tmp.write_all(out.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|error| format!("cannot write {BUNDLED_MANIFEST}: {error}"))?;
    tmp.persist(skills_dir.join(BUNDLED_MANIFEST))
        .map_err(|error| format!("cannot save {BUNDLED_MANIFEST}: {}", error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for &(name, data) in entries {
            let offset = out.len() as u32;
            let name_bytes = name.as_bytes();
            let size = data.len() as u32;
            // Local file header
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method = stored
            out.extend_from_slice(&0u16.to_le_bytes()); // mtime
            out.extend_from_slice(&0u16.to_le_bytes()); // mdate
            out.extend_from_slice(&0u32.to_le_bytes()); // crc32 (not checked by reader)
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_bytes);
            out.extend_from_slice(data);

            // Central directory header
            central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_bytes);
        }
        let cd_offset = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        // EOCD
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn extracts_zip_skills_blocks_traversal_and_respects_edits_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let zip = build_stored_zip(&[
            ("hermes-agent-main/README.md", b"ignored"),
            (
                "hermes-agent-main/skills/dev/rust/SKILL.md",
                b"---\nname: rust\ndescription: Rust helper\n---\nBody",
            ),
            (
                "hermes-agent-main/skills/dev/rust/references/tips.md",
                b"Tip 1",
            ),
            (
                "hermes-agent-main/skills/ops/deploy/SKILL.md",
                b"---\nname: deploy\ndescription: Deploy\n---\nGo",
            ),
            ("hermes-agent-main/skills/../escape/SKILL.md", b"evil"),
        ]);

        let first = sync_skills_from_zip_bytes(dir.path(), &zip, "test-zip", false).unwrap();
        assert_eq!(first.installed, vec!["dev/rust", "ops/deploy"]);
        assert!(!dir.path().join("escape").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("skills/dev/rust/references/tips.md")).unwrap(),
            "Tip 1"
        );
        assert_eq!(
            list_installed_skills(dir.path()),
            vec!["dev/rust", "ops/deploy"]
        );

        // Edit one skill and delete the other; re-sync must respect both!
        fs::write(dir.path().join("skills/dev/rust/SKILL.md"), "user edited").unwrap();
        fs::remove_dir_all(dir.path().join("skills/ops/deploy")).unwrap();

        let second = sync_skills_from_zip_bytes(dir.path(), &zip, "test-zip", false).unwrap();
        assert!(second.installed.is_empty());
        assert_eq!(second.skipped_existing, vec!["dev/rust"]);
        assert_eq!(second.skipped_deleted, vec!["ops/deploy"]);
        assert_eq!(
            fs::read_to_string(dir.path().join("skills/dev/rust/SKILL.md")).unwrap(),
            "user edited"
        );
        assert!(!dir.path().join("skills/ops/deploy").exists());

        // Force sync restores deleted and updates existing
        let forced = sync_skills_from_zip_bytes(dir.path(), &zip, "test-zip", true).unwrap();
        assert_eq!(forced.installed, vec!["ops/deploy"]);
        assert_eq!(forced.updated, vec!["dev/rust"]);
        assert!(dir.path().join("skills/ops/deploy/SKILL.md").is_file());
    }

    #[test]
    fn opt_out_skips_seeding_until_re_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let zip = build_stored_zip(&[("skills/sample/SKILL.md", b"---\nname: sample\n---\nHi")]);

        set_opt_out(dir.path(), true).unwrap();
        assert!(is_opted_out(dir.path()));
        let skipped = sync_skills_from_zip_bytes(dir.path(), &zip, "test-zip", false).unwrap();
        assert!(skipped.opted_out);
        assert!(skipped.installed.is_empty());

        set_opt_out(dir.path(), false).unwrap();
        let seeded = sync_skills_from_zip_bytes(dir.path(), &zip, "test-zip", false).unwrap();
        assert!(!seeded.opted_out);
        assert_eq!(seeded.installed, vec!["sample"]);
    }
}
