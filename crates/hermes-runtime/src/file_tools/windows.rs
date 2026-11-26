use crate::paths::PathExt;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ring::digest::{digest, SHA256};

use super::{fingerprint, WRITE_LIMIT};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary file inside the target directory, removed automatically on drop unless
/// explicitly disarmed after publish.
struct ScopedTempFile {
    path: PathBuf,
    committed: bool,
}

impl ScopedTempFile {
    fn create(parent_dir: &Path) -> Result<(Self, File), String> {
        let pid = std::process::id();
        for _ in 0..64 {
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent_dir.join(format!(".hermes-tmp-{pid}-{seq}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            path,
                            committed: false,
                        },
                        file,
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(error(err)),
            }
        }
        Err("Failed to create a unique temporary file in the target directory".into())
    }
}

impl Drop for ScopedTempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) struct WritePlan {
    /// The syntax check of a whole-file write.
    pub(super) lint: Option<serde_json::Value>,
    pub(super) target: Target,
    pub(super) content: String,
    pub(super) before: Option<Stamp>,
    pub(super) patch: Option<(Vec<String>, String, serde_json::Value)>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Stamp {
    pub(super) content: [u8; 32],
    len: u64,
    modified: Option<SystemTime>,
}

pub(super) struct Target {
    pub(super) path: PathBuf,
    root: PathBuf,
    parent_dir: PathBuf,
    /// Directories on the way to the file that do not exist yet, created at commit time.
    pending: Vec<OsString>,
}

impl Target {
    pub(super) fn open(root: &Path, path: &Path) -> Result<Self, String> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "Path is outside the working directory")?;
        let mut components = relative.components();
        let Some(Component::Normal(_filename)) = components.next_back() else {
            return Err("A file name is required".into());
        };
        let (parent_dir, pending) = inspect_existing_prefix(root, components.as_path())?;
        let canonical_path = root.join(relative.components().as_path());
        Ok(Self {
            path: canonical_path,
            root: root.to_owned(),
            parent_dir,
            pending,
        })
    }

    /// Directories this write will create, as paths relative to the workspace.
    pub(super) fn directories_to_create(&self) -> Vec<String> {
        let existing = self
            .path
            .strip_prefix(&self.root)
            .ok()
            .map(|relative| {
                relative
                    .components()
                    .count()
                    .saturating_sub(1 + self.pending.len())
            })
            .unwrap_or(0);
        let mut base: PathBuf = self
            .path
            .strip_prefix(&self.root)
            .map(|relative| relative.components().take(existing).collect())
            .unwrap_or_default();
        self.pending
            .iter()
            .map(|name| {
                base.push(name);
                base.display().to_string()
            })
            .collect()
    }

    pub(super) fn inspect(&self) -> Result<Option<Stamp>, String> {
        self.read_version()
            .map(|version| version.map(|(stamp, _)| stamp))
    }

    pub(super) fn read_version(&self) -> Result<Option<(Stamp, String)>, String> {
        if !self.pending.is_empty() {
            return Ok(None);
        }
        verify_existing_chain(&self.root, &self.path, false)?;
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(error(err)),
        };
        if is_symlink_or_reparse(&meta) {
            return Err("Refusing to write through a symbolic link".into());
        }
        if !meta.is_file() || meta.len() > WRITE_LIMIT as u64 {
            return Err("The target must be a regular text file no larger than 1 MiB".into());
        }
        let file = File::open(&self.path).map_err(error)?;
        let mut content = String::new();
        file.take(WRITE_LIMIT as u64 + 1)
            .read_to_string(&mut content)
            .map_err(error)?;
        if content.len() > WRITE_LIMIT || content.contains('\0') {
            return Err("The target must be a UTF-8 text file no larger than 1 MiB".into());
        }
        let stamp = Stamp {
            content: fingerprint(content.as_bytes()),
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        Ok(Some((stamp, content)))
    }

    pub(super) fn read_file(&self) -> Result<File, String> {
        if !self.pending.is_empty() {
            return Err("No such file or directory".into());
        }
        verify_existing_chain(&self.root, &self.path, true)?;
        let meta = fs::symlink_metadata(&self.path).map_err(error)?;
        if is_symlink_or_reparse(&meta) {
            return Err("Refusing to read through a symbolic link".into());
        }
        if !meta.is_file() {
            return Err("The target is not a regular file".into());
        }
        File::open(&self.path).map_err(error)
    }

    pub(super) fn commit(&self, content: &str, before: Option<Stamp>) -> Result<(), String> {
        if !self.pending.is_empty() {
            let mut current = self.parent_dir.clone();
            for name in &self.pending {
                current.push(name);
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(error(err)),
                }
                let meta = fs::symlink_metadata(&current).map_err(error)?;
                if is_symlink_or_reparse(&meta) || !meta.is_dir() {
                    return Err("Refusing to traverse a symbolic link or non-directory".into());
                }
            }
            let finished = Self::open(&self.root, &self.path)?;
            if !finished.pending.is_empty() {
                return Err("The target directory could not be created".into());
            }
            return finished.commit(content, None);
        }

        let _dir_lock = lock_directory(&self.parent_dir)?;
        verify_existing_chain(&self.root, &self.path, false)?;
        if self.inspect()? != before {
            return Err("The target changed while approval was pending".into());
        }

        let (mut temporary, mut temp_file) = ScopedTempFile::create(&self.parent_dir)?;
        temp_file.write_all(content.as_bytes()).map_err(error)?;
        temp_file.sync_all().map_err(error)?;
        // Close the file handle before renaming/hard-linking; Windows rejects renaming an open file.
        drop(temp_file);

        if self.inspect()? != before {
            return Err("The target changed before writing".into());
        }
        if before.is_some() {
            fs::rename(&temporary.path, &self.path).map_err(error)?;
            temporary.committed = true;
        } else {
            match fs::hard_link(&temporary.path, &self.path) {
                Ok(()) => {
                    let _ = fs::remove_file(&temporary.path);
                    temporary.committed = true;
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(error(err));
                }
                Err(err) if err.kind() == io::ErrorKind::Unsupported => {
                    // Filesystems without hard links (e.g. FAT32/exFAT): re-check absence under
                    // directory lock, then rename.
                    match fs::symlink_metadata(&self.path) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            fs::rename(&temporary.path, &self.path).map_err(error)?;
                            temporary.committed = true;
                        }
                        Ok(_) => return Err("File already exists".into()),
                        Err(e) => return Err(error(e)),
                    }
                }
                Err(err) => return Err(error(err)),
            }
        }

        if self.inspect()?.map(|stamp| stamp.content) != Some(fingerprint(content.as_bytes())) {
            return Err("The published file changed before verification".into());
        }
        Ok(())
    }
}

fn is_symlink_or_reparse(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// The deepest existing directory of `relative` under `root`, and the names after it.
fn inspect_existing_prefix(
    root: &Path,
    relative: &Path,
) -> Result<(PathBuf, Vec<OsString>), String> {
    let root_canon = root.canonical().map_err(error)?;
    let mut current = root.to_owned();
    let mut components = relative.components();
    let mut pending: Vec<OsString> = Vec::new();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err("A path without parent traversal is required".into());
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                if is_symlink_or_reparse(&meta) {
                    return Err("Refusing to traverse a symbolic link or junction".into());
                }
                if !meta.is_dir() {
                    return Err("A path component is not a directory".into());
                }
                let canon = current.canonical().map_err(error)?;
                if !canon.starts_with(&root_canon) {
                    return Err("Path is outside the working directory".into());
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                current.pop();
                pending.push(name.to_owned());
                for rest in components.by_ref() {
                    let Component::Normal(rest) = rest else {
                        return Err("A path without parent traversal is required".into());
                    };
                    pending.push(rest.to_owned());
                }
                break;
            }
            Err(err) => return Err(error(err)),
        }
    }
    Ok((current, pending))
}

/// Verify that every parent directory between `root` and `path` exists as a real non-reparse
/// directory inside `root`.
fn verify_existing_chain(root: &Path, path: &Path, require_file: bool) -> Result<(), String> {
    let root_canon = root.canonical().map_err(error)?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Path is outside the working directory")?;
    let mut components = relative.components();
    let Some(Component::Normal(_)) = components.next_back() else {
        return Err("A file name is required".into());
    };
    let mut current = root.to_owned();
    for component in components {
        let Component::Normal(name) = component else {
            return Err("A path without parent traversal is required".into());
        };
        current.push(name);
        let meta = fs::symlink_metadata(&current).map_err(error)?;
        if is_symlink_or_reparse(&meta) {
            return Err("Refusing to traverse a symbolic link or junction".into());
        }
        if !meta.is_dir() {
            return Err("A path component is not a directory".into());
        }
    }
    let parent_canon = current.canonical().map_err(error)?;
    if !parent_canon.starts_with(&root_canon) {
        return Err("Path is outside the working directory".into());
    }
    if require_file {
        let file_canon = path.canonical().map_err(error)?;
        if !file_canon.starts_with(&root_canon) {
            return Err("Path is outside the working directory".into());
        }
    }
    Ok(())
}

/// Cross-process per-directory write lock stored in the OS temp directory so workspace directories
/// stay clean.
fn lock_directory(parent_dir: &Path) -> Result<File, String> {
    let canon = parent_dir.canonical().map_err(error)?;
    let hash = digest(&SHA256, canon.to_string_lossy().as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in &hash.as_ref()[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    let lock_path = std::env::temp_dir().join(format!("hermes-dir-{hex}.lock"));
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(error)?;
    lock_file.lock().map_err(error)?;
    Ok(lock_file)
}

fn error(err: impl std::fmt::Display) -> String {
    err.to_string()
}
