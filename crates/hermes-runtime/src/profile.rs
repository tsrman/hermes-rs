//! Inventory a profile and prepare a private, verified copy for the Linux CLI pilot.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{openat, renameat_with, Mode, OFlags, RenameFlags};
use serde::Serialize;
use tempfile::{Builder, NamedTempFile};

use crate::file_tools::directory::open_directory;
use crate::storage::{write_verified_snapshot, SnapshotReport};

#[derive(Clone, Copy)]
enum Asset {
    File,
    Directory,
    Database,
    Credential,
}

const ASSETS: &[(&str, Asset)] = &[
    ("config.yaml", Asset::File),
    (".env", Asset::File),
    ("auth.json", Asset::Credential),
    ("auth", Asset::Credential),
    (".anthropic_oauth.json", Asset::Credential),
    ("google_token.json", Asset::Credential),
    ("mcp-tokens", Asset::Credential),
    ("vault", Asset::Credential),
    ("state.db", Asset::Database),
    ("SOUL.md", Asset::File),
    ("memories", Asset::Directory),
    ("skills", Asset::Directory),
];

#[derive(Serialize)]
pub struct ProfileReport {
    pub ready: bool,
    entries: Vec<Entry>,
    database: Option<SnapshotReport>,
    database_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

#[derive(Serialize)]
struct Entry {
    path: &'static str,
    action: &'static str,
    state: &'static str,
    permissions: Option<String>,
    shared_access: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<String>,
}

pub fn check_profile(profile: &Path) -> Result<ProfileReport, String> {
    let (path, directory) = open_profile(profile)?;
    let mut report = inspect(&directory, None);
    if report.has_database() {
        let scratch = NamedTempFile::new().map_err(|error| error.to_string())?;
        match write_verified_snapshot(&path, scratch.path(), false) {
            Ok(database) => report.database = Some(database),
            Err(error) => {
                report.ready = false;
                report.database_error = Some(error.to_string());
            }
        }
    }
    Ok(report)
}

pub fn migrate_profile(profile: &Path, destination: &Path) -> Result<ProfileReport, String> {
    let (source, directory) = open_profile(profile)?;
    let name = destination
        .file_name()
        .ok_or("missing output directory name")?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if parent.starts_with(&source) {
        return Err("the output directory must be outside the source profile".into());
    }
    let destination = parent.join(name);
    match destination.symlink_metadata() {
        Ok(_) => return Err("the output directory already exists".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let parent_handle = open_directory(&parent, &parent)?;
    let staged = Builder::new()
        .prefix(".hermes-migrate-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(fd_path(&parent_handle))
        .map_err(|error| error.to_string())?;
    let mut report = inspect(&directory, Some(staged.path()));
    if let Some((path, issue)) = report
        .entries
        .iter()
        .find_map(|entry| entry.issue.as_deref().map(|issue| (entry.path, issue)))
    {
        return Err(format!("{path}: {issue}"));
    }
    if report.has_database() {
        // SQLite's NOFOLLOW check also rejects the symlink in /proc/self/fd.
        let staged_path = staged
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let database = NamedTempFile::new_in(&staged_path).map_err(|error| error.to_string())?;
        report.database = Some(
            write_verified_snapshot(&source, database.path(), false)
                .map_err(|error| error.to_string())?,
        );
        database
            .persist_noclobber(staged_path.join("state.db"))
            .map_err(|error| error.error.to_string())?;
    }
    File::open(staged.path())
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
    let staged_name = staged.path().file_name().expect("temporary directory name");
    renameat_with(
        &parent_handle,
        staged_name,
        &parent_handle,
        name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| error.to_string())?;
    let _ = staged.keep();
    parent_handle
        .sync_all()
        .map_err(|error| error.to_string())?;
    report.output = Some(destination.display().to_string());
    Ok(report)
}

impl ProfileReport {
    fn has_database(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.path == "state.db" && entry.state == "file")
    }
}

fn open_profile(profile: &Path) -> Result<(PathBuf, File), String> {
    let path = profile.canonicalize().map_err(|error| error.to_string())?;
    let directory = open_directory(&path, &path)?;
    Ok((path, directory))
}

fn inspect(directory: &File, destination: Option<&Path>) -> ProfileReport {
    let entries: Vec<_> = ASSETS
        .iter()
        .map(|&(name, asset)| inspect_entry(directory, name, asset, destination))
        .collect();
    ProfileReport {
        ready: entries.iter().all(|entry| entry.issue.is_none()),
        entries,
        database: None,
        database_error: None,
        output: None,
    }
}

fn inspect_entry(
    directory: &File,
    name: &'static str,
    asset: Asset,
    destination: Option<&Path>,
) -> Entry {
    let mut entry = Entry {
        path: name,
        action: match asset {
            Asset::File | Asset::Directory => "copy",
            Asset::Database => "sqlite_backup",
            Asset::Credential => "inspect_only",
        },
        state: "missing",
        permissions: None,
        shared_access: false,
        issue: None,
    };
    let metadata = match fd_path(directory).join(name).symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return entry,
        Err(error) => {
            entry.state = "unreadable";
            entry.issue = Some(error.to_string());
            return entry;
        }
    };
    let mode = metadata.permissions().mode() & 0o7777;
    entry.permissions = Some(format!("{mode:04o}"));
    entry.shared_access = mode & 0o077 != 0;
    entry.state = if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "unsupported"
    };
    if matches!(asset, Asset::Credential) {
        return entry;
    }
    let expect_directory = matches!(asset, Asset::Directory);
    if if expect_directory {
        !metadata.is_dir()
    } else {
        !metadata.is_file()
    } {
        entry.issue = Some("unexpected file type (expected a regular file or directory)".into());
        return entry;
    }
    if matches!(asset, Asset::Database) {
        return entry;
    }
    let target = destination.map(|path| path.join(name));
    let mut result = inspect_or_copy(
        directory,
        name.as_ref(),
        target.as_deref(),
        expect_directory,
    );
    if name == "config.yaml" && result.is_ok() {
        let config = match target {
            Some(path) => File::open(path),
            None => open_asset(directory, name.as_ref()),
        };
        result = config.and_then(validate_config);
    };
    entry.issue = result.err().map(|error| error.to_string());
    entry
}

fn validate_config(file: File) -> io::Result<()> {
    let config: serde_yaml::Value = serde_yaml::from_reader(file)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid YAML configuration"))?;
    if config.is_mapping() || config.is_null() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration must be a YAML mapping",
        ))
    }
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn open_asset(directory: &File, name: &OsStr) -> io::Result<File> {
    openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(Into::into)
}

fn inspect_or_copy(
    directory: &File,
    name: &OsStr,
    destination: Option<&Path>,
    tree: bool,
) -> io::Result<()> {
    let mut source = open_asset(directory, name)?;
    let before = source.metadata()?;
    if before.is_dir() && tree {
        if let Some(path) = destination {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        for result in fs::read_dir(fd_path(&source))? {
            let child = result?;
            let kind = child.file_type()?;
            if !kind.is_dir() && !kind.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tree contains a symlink or special file",
                ));
            }
            let name = child.file_name();
            let target = destination.map(|path| path.join(&name));
            inspect_or_copy(&source, &name, target.as_deref(), true)?;
        }
        if let Some(path) = destination {
            File::open(path)?.sync_all()?;
        }
    } else if before.is_file() {
        if let Some(path) = destination {
            let mode = if tree && before.permissions().mode() & 0o100 != 0 {
                0o700
            } else {
                0o600
            };
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(path)?;
            let copied = io::copy(&mut source, &mut target)?;
            let after = source.metadata()?;
            if copied != before.len()
                || after.len() != before.len()
                || after.modified()? != before.modified()?
            {
                return Err(io::Error::other("source file changed while being copied"));
            }
            target.set_permissions(Permissions::from_mode(mode))?;
            target.sync_all()?;
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected file type",
        ));
    }
    Ok(())
}
