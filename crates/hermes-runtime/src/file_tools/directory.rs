use std::fs::File;
use std::path::{Component, Path};

use rustix::fs::{open, openat, Mode, OFlags};

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(crate) fn open_directory(root: &Path, path: &Path) -> Result<File, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Path is outside the working directory")?;
    let mut directory = open(root, DIRECTORY, Mode::empty()).map_err(|error| error.to_string())?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("A path without parent traversal is required".into());
        };
        directory = openat(&directory, name, DIRECTORY, Mode::empty())
            .map_err(|error| error.to_string())?;
    }
    Ok(File::from(directory))
}
