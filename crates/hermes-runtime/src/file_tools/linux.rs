use std::ffi::{OsStr, OsString};
use std::fs::{File, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{mkdirat, openat, renameat, renameat_with, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use super::{directory::open_directory, fingerprint, WRITE_LIMIT};

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
    device: u64,
    inode: u64,
    mode: u32,
}

pub(super) struct Target {
    pub(super) path: PathBuf,
    root: PathBuf,
    parent: File,
    filename: OsString,
    /// Directories on the way to the file that do not exist yet, created at commit time.
    pending: Vec<OsString>,
}

impl Target {
    pub(super) fn open(root: &Path, path: &Path) -> Result<Self, String> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "Path is outside the working directory")?;
        let mut components = relative.components();
        let Some(Component::Normal(filename)) = components.next_back() else {
            return Err("A file name is required".into());
        };
        let (parent, pending) = open_existing_prefix(root, components.as_path())?;
        let canonical_path = root.join(relative.components().as_path());
        Ok(Self {
            path: canonical_path,
            root: root.to_owned(),
            parent,
            filename: filename.to_owned(),
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
        read_version(&self.parent, &self.filename)
    }

    pub(super) fn read_file(&self) -> Result<File, String> {
        open_file(&self.parent, &self.filename)
            .map(File::from)
            .map_err(error)
    }

    pub(super) fn commit(&self, content: &str, before: Option<Stamp>) -> Result<(), String> {
        if !self.pending.is_empty() {
            // Make the missing directories one at a time, never following a link, then write
            // through a fresh handle on the finished path.
            let mut directory = self.parent.try_clone().map_err(error)?;
            for name in &self.pending {
                match mkdirat(&directory, name, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(err) => return Err(error(err)),
                }
                directory =
                    File::from(openat(&directory, name, DIRECTORY, Mode::empty()).map_err(error)?);
            }
            let finished = Self::open(&self.root, &self.path)?;
            if !finished.pending.is_empty() {
                return Err("The target directory could not be created".into());
            }
            return finished.commit(content, None);
        }
        // Serialize approved writes in one directory across local CLI processes and across
        // subagents of this one. The lock is held only during the write, never while waiting
        // for approval, so waiting for it is short.
        self.parent.lock().map_err(|err| err.to_string())?;
        let current = Self::open(&self.root, &self.path)
            .map_err(|_| "The target directory changed while approval was pending".to_owned())?;
        let original_dir = self.parent.metadata().map_err(error)?;
        let current_dir = current.parent.metadata().map_err(error)?;
        if original_dir.dev() != current_dir.dev()
            || original_dir.ino() != current_dir.ino()
            || self.inspect()? != before
        {
            return Err("The target changed while approval was pending".into());
        }

        // The descriptor path pins tempfile creation to the inspected directory.
        // renameat also uses that descriptor, so it never follows replacement symlinks.
        let parent = format!("/proc/self/fd/{}", self.parent.as_raw_fd());
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(error)?;
        temporary.write_all(content.as_bytes()).map_err(error)?;
        if let Some(before) = before {
            temporary
                .as_file()
                .set_permissions(Permissions::from_mode(before.mode & 0o777))
                .map_err(error)?;
        }
        temporary.as_file().sync_all().map_err(error)?;
        let temporary_name = temporary
            .path()
            .file_name()
            .ok_or("Missing temporary file name")?;
        if self.inspect()? != before {
            return Err("The target changed before writing".into());
        }
        if before.is_some() {
            renameat(&self.parent, temporary_name, &self.parent, &self.filename).map_err(error)?;
        } else {
            renameat_with(
                &self.parent,
                temporary_name,
                &self.parent,
                &self.filename,
                RenameFlags::NOREPLACE,
            )
            .map_err(error)?;
        }
        self.parent.sync_all().map_err(error)?;
        if self.inspect()?.map(|stamp| stamp.content) != Some(fingerprint(content.as_bytes())) {
            return Err("The published file changed before verification".into());
        }
        Ok(())
    }
}

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The deepest existing directory of `relative` under `root`, and the names after it.
fn open_existing_prefix(root: &Path, relative: &Path) -> Result<(File, Vec<OsString>), String> {
    let mut directory = open_directory(root, root)?;
    let mut components = relative.components();
    let mut pending: Vec<OsString> = Vec::new();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err("A path without parent traversal is required".into());
        };
        match openat(&directory, name, DIRECTORY, Mode::empty()) {
            Ok(next) => directory = File::from(next),
            Err(Errno::NOENT) => {
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
    Ok((directory, pending))
}

fn read_version(parent: &File, name: &OsStr) -> Result<Option<(Stamp, String)>, String> {
    let file = match open_file(parent, name) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => return Err("Refusing to write through a symbolic link".into()),
        Err(err) => return Err(error(err)),
    };
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() || metadata.len() > WRITE_LIMIT as u64 {
        return Err("The target must be a regular text file no larger than 1 MiB".into());
    }
    let mut content = String::new();
    file.take(WRITE_LIMIT as u64 + 1)
        .read_to_string(&mut content)
        .map_err(error)?;
    if content.len() > WRITE_LIMIT || content.contains('\0') {
        return Err("The target must be a UTF-8 text file no larger than 1 MiB".into());
    }
    let stamp = Stamp {
        content: fingerprint(content.as_bytes()),
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    };
    Ok(Some((stamp, content)))
}

fn open_file(parent: &File, name: &OsStr) -> rustix::io::Result<std::os::fd::OwnedFd> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn error(err: impl std::fmt::Display) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_in_one_directory_wait_for_each_other_instead_of_failing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let target = Target::open(&root, &root.join(format!("f{index}.txt"))).unwrap();
                    let before = target.inspect().unwrap();
                    target.commit(&format!("content {index}"), before)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        for index in 0..8 {
            assert_eq!(
                std::fs::read_to_string(root.join(format!("f{index}.txt"))).unwrap(),
                format!("content {index}")
            );
        }
    }
}
