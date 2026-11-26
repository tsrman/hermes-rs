//! Path helpers shared by every tool that shows a filesystem path to the model or the user.

use std::io;
use std::path::{Path, PathBuf};

/// `Path::canonicalize` without the Windows verbatim prefix (`\\?\C:\x` becomes `C:\x`,
/// `\\?\UNC\host\share` becomes `\\host\share`). Identical to `canonicalize` on other systems.
pub trait PathExt {
    fn canonical(&self) -> io::Result<PathBuf>;
}

impl PathExt for Path {
    fn canonical(&self) -> io::Result<PathBuf> {
        let resolved = self.canonicalize()?;
        Ok(if cfg!(windows) {
            simplify(resolved)
        } else {
            resolved
        })
    }
}

/// The user's home directory as `os.path.expanduser` finds it: `$HOME` on Unix, the account
/// profile (`USERPROFILE`) on Windows, where `HOME` is normally unset and ignored by the reference.
pub fn home_dir() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::home_dir()
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    };
    home.filter(|path| !path.as_os_str().is_empty())
}

/// `canonical` for a path that may not exist yet: the deepest existing ancestor is resolved and
/// the missing tail is appended, so the result is comparable with resolved directories (Windows
/// short `RUNNER~1` names and symlinked temp directories included). Relative paths are returned as is.
pub fn canonical_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonical() {
        return resolved;
    }
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let mut existing = path;
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            missing.push(name);
        }
        existing = parent;
        if let Ok(mut resolved) = existing.canonical() {
            resolved.extend(missing.iter().rev());
            return resolved;
        }
    }
    path.to_path_buf()
}

/// `std::fs::canonicalize` with the verbatim prefix removed on Windows.
pub fn canonical(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    path.as_ref().canonical()
}

/// Drop a Windows verbatim prefix when the plain form names the same file.
pub fn simplify(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(simplify_verbatim) {
        Some(plain) => PathBuf::from(plain),
        None => path,
    }
}

/// The non-verbatim spelling of a verbatim path, or `None` when it has none or must stay verbatim
/// (too long for `MAX_PATH`, reserved device names, trailing dots or spaces).
fn simplify_verbatim(text: &str) -> Option<String> {
    let plain = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        let rest = text.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if !drive || (bytes.len() > 2 && bytes[2] != b'\\') {
            return None;
        }
        rest.to_owned()
    };
    if plain.len() >= 260 || plain.contains('/') {
        return None;
    }
    let drive_len = if plain.starts_with(r"\\") { 2 } else { 3 };
    let unsafe_component = plain.get(drive_len.min(plain.len())..).is_some_and(|tail| {
        tail.split('\\').any(|part| {
            if part == "." || part == ".." || part.ends_with('.') || part.ends_with(' ') {
                return true;
            }
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && stem.as_bytes()[3].is_ascii_digit())
        })
    });
    (!unsafe_component).then_some(plain)
}

/// Test helper: on Windows, the separators after each `marker` (a scrubbed directory) become `/`, so
/// native paths compare with the reference output recorded on Linux. A no-op elsewhere.
#[doc(hidden)]
pub fn forward_slashes_after(text: &str, marker: &str) -> String {
    if !cfg!(windows) {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let end_of_marker = at + marker.len();
        out.push_str(&rest[..end_of_marker]);
        rest = &rest[end_of_marker..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ','))
            .unwrap_or(rest.len());
        out.push_str(&rest[..end].replace('\\', "/"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::simplify_verbatim as simple;

    #[test]
    fn verbatim_prefixes_are_removed_when_safe() {
        assert_eq!(
            simple(r"\\?\C:\Users\me\a.txt").as_deref(),
            Some(r"C:\Users\me\a.txt")
        );
        assert_eq!(simple(r"\\?\C:\").as_deref(), Some(r"C:\"));
        assert_eq!(
            simple(r"\\?\UNC\host\share\a").as_deref(),
            Some(r"\\host\share\a")
        );
        assert_eq!(simple(r"C:\plain"), None);
        assert_eq!(simple(r"\\?\Volume{1234}\a"), None);
        assert_eq!(simple(r"\\?\C:\dir\NUL.txt"), None);
        assert_eq!(simple(r"\\?\C:\dir\trailing."), None);
        assert_eq!(simple(&format!(r"\\?\C:\{}", "a".repeat(300))), None);
    }
}
