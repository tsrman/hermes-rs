//! The profile's scratch directory: `<profile>/cache/scratch`, where commands are told to put
//! temporary files. Entries idle for 24 hours are removed, at most once an hour.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const IDLE: Duration = Duration::from_secs(24 * 3600);
const EVERY: Duration = Duration::from_secs(3600);

/// The scratch directory, created with owner-only access, after an hourly cleanup.
pub fn directory(profile: &Path) -> Option<PathBuf> {
    let scratch = profile.join("cache").join("scratch");
    std::fs::create_dir_all(&scratch).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700));
    }
    prune(profile, &scratch);
    Some(scratch)
}

fn prune(profile: &Path, scratch: &Path) {
    let stamp = profile.join("cache").join(".scratch_pruned");
    let due = std::fs::metadata(&stamp)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| SystemTime::now().duration_since(time).ok())
        .is_none_or(|age| age >= EVERY);
    if !due || std::fs::write(&stamp, b"").is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return;
    };
    for entry in entries.flatten() {
        let idle = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| SystemTime::now().duration_since(time).ok())
            .is_some_and(|age| age >= IDLE);
        if idle {
            let path = entry.path();
            let _ = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_entries_go_and_fresh_ones_stay() {
        let home = tempfile::tempdir().unwrap();
        let scratch = directory(home.path()).unwrap();
        std::fs::write(scratch.join("fresh"), "x").unwrap();
        let old = scratch.join("old");
        std::fs::write(&old, "x").unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(48 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        // The first call stamped the hour; make the stamp old so the next call prunes.
        let stamp = home.path().join("cache/.scratch_pruned");
        std::fs::File::options()
            .write(true)
            .open(&stamp)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 3600))
            .unwrap();
        directory(home.path()).unwrap();
        assert!(scratch.join("fresh").exists());
        assert!(!old.exists());
    }
}
