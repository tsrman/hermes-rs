//! Append-only JSONL audit ledger and content-addressed blob snapshots for `skill_manage`.
//!
//! Mirrors `tools/skill_ledger.py`:
//! - Appends one JSONL row per committed skill mutation to `<home>/skills/.curator_ledger.jsonl`
//! - Stores file snapshots content-addressed by SHA-256 in `<home>/.curator_backups/blobs/`
//! - Keeps only changed `(path, sha256)` entries (`delta`) so unchanged support files don't bloat rows
//! - Trims oldest rows down to 80% of `skills.ledger_max_bytes` while always preserving the newest row
//! - Telemetry, never a gate: I/O failures never fail a skill mutation

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::{json, Value};

const TRANSIENT_DIRS: &[&str] = &[
    ".venv",
    "venv",
    "env",
    ".env",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".git",
];

static LEDGER_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ManifestEntry {
    pub path: String,
    pub sha256: String,
}

pub(crate) fn ledger_path(home: &Path) -> PathBuf {
    home.join("skills").join(".curator_ledger.jsonl")
}

pub(crate) fn blobs_dir(home: &Path) -> PathBuf {
    home.join(".curator_backups").join("blobs")
}

fn sha256_hex(data: &[u8]) -> String {
    let hash = digest(&SHA256, data);
    let mut out = String::with_capacity(64);
    for b in hash.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn random_hex_12() -> String {
    let mut bytes = [0u8; 6];
    let _ = SecureRandom::fill(&SystemRandom::new(), &mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn store_blob(home: &Path, data: &[u8]) -> std::io::Result<String> {
    let hex = sha256_hex(data);
    let dir = blobs_dir(home);
    let dest = dir.join(&hex);
    if !dest.exists() {
        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".tmp-{}-{hex}", random_hex_12()));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &dest)?;
    }
    Ok(hex)
}

fn collect_snapshot_files(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let is_transient = rel.components().any(|comp| {
            comp.as_os_str()
                .to_str()
                .is_some_and(|name| TRANSIENT_DIRS.contains(&name))
        });
        if is_transient {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_snapshot_files(&path, root, out);
        } else if ft.is_file() {
            out.push(path);
        }
    }
}

/// Capture sorted `{path, sha256}` entries for every non-transient file under `root`.
/// Best-effort: returns whatever could be stored so telemetry never breaks a mutation.
pub(crate) fn snapshot_paths(home: &Path, root: Option<&Path>) -> Vec<ManifestEntry> {
    let Some(root) = root else {
        return Vec::new();
    };
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
    } else if root.is_dir() {
        collect_snapshot_files(root, root, &mut files);
    }
    files.sort();
    let mut manifest = Vec::with_capacity(files.len());
    for file in files {
        let Ok(bytes) = fs::read(&file) else {
            continue;
        };
        let Ok(sha256) = store_blob(home, &bytes) else {
            continue;
        };
        manifest.push(ManifestEntry {
            path: file.display().to_string(),
            sha256,
        });
    }
    manifest
}

/// Drop paths whose `(path, sha256)` is identical in `before` and `after`.
pub(crate) fn delta(
    before: Vec<ManifestEntry>,
    after: Vec<ManifestEntry>,
) -> (Vec<ManifestEntry>, Vec<ManifestEntry>) {
    let b_map: HashMap<&str, &str> = before
        .iter()
        .map(|e| (e.path.as_str(), e.sha256.as_str()))
        .collect();
    let a_map: HashMap<&str, &str> = after
        .iter()
        .map(|e| (e.path.as_str(), e.sha256.as_str()))
        .collect();
    let changed_before = before
        .iter()
        .filter(|e| a_map.get(e.path.as_str()).copied() != Some(e.sha256.as_str()))
        .cloned()
        .collect();
    let changed_after = after
        .iter()
        .filter(|e| b_map.get(e.path.as_str()).copied() != Some(e.sha256.as_str()))
        .cloned()
        .collect();
    (changed_before, changed_after)
}

fn now_rfc3339() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos()).map_or_else(
        || "1970-01-01T00:00:00+00:00".to_owned(),
        |dt| dt.to_rfc3339(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_mutation(
    home: &Path,
    enabled: bool,
    max_bytes: usize,
    action: &str,
    skill: &str,
    before: Vec<ManifestEntry>,
    after: Vec<ManifestEntry>,
    evidence: Value,
) {
    if !enabled {
        return;
    }
    let (delta_before, delta_after) = delta(before, after);
    let entry = json!({
        "id": random_hex_12(),
        "ts": now_rfc3339(),
        "actor": "agent",
        "action": action,
        "skill": skill,
        "evidence": evidence,
        "before": delta_before,
        "after": delta_after,
    });
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    let path = ledger_path(home);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _guard = LEDGER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if writeln!(file, "{line}").is_err() {
        return;
    }
    drop(file);
    if max_bytes > 0 {
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() as usize > max_bytes {
                let low_water = (max_bytes as f64 * 0.8) as usize;
                let _ = trim_oldest_locked(&path, low_water);
            }
        }
    }
}

fn trim_oldest_locked(path: &Path, low_water: usize) -> std::io::Result<()> {
    let raw = fs::read(path)?;
    let raw_str = String::from_utf8_lossy(&raw);
    let raw_str = raw_str.strip_prefix('\u{feff}').unwrap_or(&raw_str);
    let mut lines: Vec<&str> = raw_str.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut size = 0usize;
    for &line in lines.iter().rev() {
        let addition = line.len() + 1;
        if !kept.is_empty() && size + addition > low_water {
            break;
        }
        kept.push(line);
        size += addition;
    }
    kept.reverse();
    let mut data = kept.join("\n");
    if !kept.is_empty() {
        data.push('\n');
    }
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let tmp = parent.join(format!(".curator_ledger.{}.tmp", random_hex_12()));
    fs::write(&tmp, data.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_records_deltas_skips_transient_dirs_and_trims_to_low_water() {
        let home = tempfile::tempdir().unwrap();
        let skill_dir = home.path().join("skills").join("demo");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::create_dir_all(skill_dir.join("node_modules/pkg")).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "v1").unwrap();
        fs::write(skill_dir.join("references/keep.md"), "same").unwrap();
        fs::write(skill_dir.join("node_modules/pkg/ignored.js"), "skip").unwrap();

        let before = snapshot_paths(home.path(), Some(&skill_dir));
        assert_eq!(before.len(), 2);
        assert!(before.iter().all(|e| !e.path.contains("node_modules")));

        fs::write(skill_dir.join("SKILL.md"), "v2").unwrap();
        let after = snapshot_paths(home.path(), Some(&skill_dir));

        record_mutation(
            home.path(),
            true,
            5 * 1024 * 1024,
            "patch",
            "demo",
            before,
            after,
            json!({"old": "v1", "new": "v2"}),
        );

        let rows: Vec<Value> = fs::read_to_string(ledger_path(home.path()))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["action"], "patch");
        assert_eq!(rows[0]["skill"], "demo");
        assert_eq!(rows[0]["before"].as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["after"].as_array().unwrap().len(), 1);
        let blob_sha = rows[0]["after"][0]["sha256"].as_str().unwrap();
        assert_eq!(
            fs::read(blobs_dir(home.path()).join(blob_sha)).unwrap(),
            b"v2"
        );

        // Disabled ledger writes no new rows.
        record_mutation(
            home.path(),
            false,
            5 * 1024 * 1024,
            "edit",
            "demo",
            Vec::new(),
            Vec::new(),
            json!({}),
        );
        assert_eq!(
            fs::read_to_string(ledger_path(home.path()))
                .unwrap()
                .lines()
                .count(),
            1
        );

        // Trimming keeps newest row even when max_bytes is tiny.
        for idx in 0..5 {
            record_mutation(
                home.path(),
                true,
                300,
                "edit",
                &format!("skill-{idx}"),
                Vec::new(),
                Vec::new(),
                json!({}),
            );
        }
        let final_lines: Vec<Value> = fs::read_to_string(ledger_path(home.path()))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(final_lines.len(), 1);
        assert_eq!(final_lines[0]["skill"], "skill-4");
    }
}
