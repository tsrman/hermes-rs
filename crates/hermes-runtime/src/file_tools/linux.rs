use std::ffi::{OsStr, OsString};
use std::fs::{File, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{openat, renameat, renameat_with, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use super::{directory::open_directory, fingerprint, WRITE_LIMIT};

pub(super) struct WritePlan {
    pub(super) target: Target,
    pub(super) content: String,
    pub(super) before: Option<Stamp>,
    pub(super) display_path: String,
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
        let parent = open_directory(root, &root.join(components.as_path()))?;
        let canonical_path = root.join(relative.components().as_path());
        Ok(Self {
            path: canonical_path,
            root: root.to_owned(),
            parent,
            filename: filename.to_owned(),
        })
    }

    pub(super) fn inspect(&self) -> Result<Option<Stamp>, String> {
        self.read_version()
            .map(|version| version.map(|(stamp, _)| stamp))
    }

    pub(super) fn read_version(&self) -> Result<Option<(Stamp, String)>, String> {
        read_version(&self.parent, &self.filename)
    }

    pub(super) fn read_file(&self) -> Result<File, String> {
        open_file(&self.parent, &self.filename)
            .map(File::from)
            .map_err(error)
    }

    pub(super) fn commit(&self, content: &str, before: Option<Stamp>) -> Result<(), String> {
        // Serialize approved writes in one directory across local CLI processes.
        // The lock is held only during the write, never while waiting for approval.
        self.parent.try_lock().map_err(|err| err.to_string())?;
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

fn read_version(parent: &File, name: &OsStr) -> Result<Option<(Stamp, String)>, String> {
    let file = match open_file(parent, name) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => return Err("Refusing to write through a symbolic link".into()),
        Err(err) => return Err(error(err)),
    };
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() || metadata.len() > WRITE_LIMIT as u64 {
        return Err("The target must be a regular text file no larger than 64 KiB".into());
    }
    let mut content = String::new();
    file.take(WRITE_LIMIT as u64 + 1)
        .read_to_string(&mut content)
        .map_err(error)?;
    if content.len() > WRITE_LIMIT || content.contains('\0') {
        return Err("The target must be a UTF-8 text file no larger than 64 KiB".into());
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
