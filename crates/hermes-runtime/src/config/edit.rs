//! Atomic updates to a profile's raw YAML document.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde_yaml::{Mapping, Value};

use super::read_config;

pub fn set_config_value(home: &Path, key: &str, input: &str) -> Result<(), String> {
    let mut parts = split_key(key)?;
    if parts.len() == 1
        && key.starts_with(|ch: char| ch.is_ascii_uppercase())
        && key
            .bytes()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == b'_')
    {
        return Err("set environment variables in the profile's .env file".into());
    }

    fs::create_dir_all(home)
        .map_err(|error| format!("cannot create profile directory: {error}"))?;
    // Lock a stable inode across the read and the atomic replacement of config.yaml.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("config.yaml.lock"))
        .map_err(|error| format!("cannot open profile config lock: {error}"))?;
    lock.lock()
        .map_err(|error| format!("cannot lock profile config: {error}"))?;

    let path = home.join("config.yaml");
    match path.symlink_metadata() {
        Ok(metadata) if !metadata.is_file() => {
            return Err("profile config.yaml must be a regular file".into())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot access profile config: {error}")),
    }

    let mut config = read_config(&path)?;
    if parts[0] == "model" {
        if parts.len() == 1 && config.get("model").is_some_and(Value::is_mapping) {
            parts.push("default".into());
        }
        if parts.len() > 1 {
            if let Some(model @ Value::String(_)) = config.get_mut("model") {
                let default = std::mem::take(model);
                *model = Value::Mapping(Mapping::from_iter([(Value::from("default"), default)]));
            }
        }
    }
    let value = parse_value(&parts, input)?;
    let target = find_slot(&mut config, &parts)?;
    if (target.is_mapping() && (parts.len() == 1 || !value.is_mapping()))
        || (target.is_sequence() && !value.is_sequence())
    {
        return Err("set an individual field to preserve the existing config section".into());
    }
    *target = value;
    let contents = serde_yaml::to_string(&config).map_err(|_| "cannot serialize profile config")?;
    replace_config(home, &path, contents.as_bytes())
        .map_err(|error| format!("cannot save profile config: {error}"))
}

fn split_key(key: &str) -> Result<Vec<String>, String> {
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut chars = key.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '.' => segments.push(std::mem::take(&mut segment)),
            '\\' if chars.peek() == Some(&'.') => {
                chars.next();
                segment.push('.');
            }
            _ => segment.push(ch),
        }
    }
    segments.push(segment);
    if key.trim() != key || segments.iter().any(String::is_empty) {
        return Err("invalid config key path".into());
    }
    Ok(segments)
}

fn parse_value(parts: &[String], input: &str) -> Result<Value, String> {
    if parts[0] == "model"
        && (parts.len() == 1
            || (parts.len() == 2
                && matches!(
                    parts[1].as_str(),
                    "default" | "provider" | "base_url" | "api_mode"
                )))
    {
        return Ok(Value::from(input));
    }
    let value = input.trim();
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => return Ok(Value::from(true)),
        "false" | "no" | "off" => return Ok(Value::from(false)),
        "null" | "none" | "~" => return Ok(Value::Null),
        _ => {}
    }
    if let Ok(number) = value.parse::<i64>() {
        return Ok(Value::from(number));
    }
    if let Ok(number) = value.parse::<u64>() {
        return Ok(Value::from(number));
    }
    if let Ok(number) = value.parse::<f64>() {
        if number.is_finite() {
            return Ok(Value::from(number));
        }
    }
    if value.starts_with(['[', '{']) {
        return serde_yaml::from_str(value).map_err(|_| "invalid YAML list or mapping".into());
    }
    Ok(Value::from(input))
}

fn find_slot<'a>(
    mut current: &'a mut Value,
    mut parts: &[String],
) -> Result<&'a mut Value, String> {
    while let Some((first, rest)) = parts.split_first() {
        if current.is_null() {
            *current = Value::Mapping(Mapping::new());
        }
        match current {
            Value::Mapping(mapping) => {
                // Existing model, server and room IDs may contain literal dots.
                let existing = (1..=parts.len()).rev().find_map(|count| {
                    let key = Value::from(parts[..count].join("."));
                    mapping.contains_key(&key).then_some((key, count))
                });
                let (key, consumed) = if let Some(existing) = existing {
                    existing
                } else {
                    let prefix = format!("{first}.");
                    if !rest.is_empty()
                        && mapping
                            .keys()
                            .filter_map(Value::as_str)
                            .any(|key| key.starts_with(&prefix))
                    {
                        return Err(
                            "config path overlaps a literal dotted key; escape its dots".into()
                        );
                    }
                    (Value::from(first.as_str()), 1)
                };
                current = mapping.entry(key).or_insert(Value::Null);
                parts = &parts[consumed..];
            }
            Value::Sequence(items) => {
                let index = first
                    .parse::<isize>()
                    .map_err(|_| "config list index must be an integer")?;
                let offset = if index < 0 {
                    items.len().checked_sub(index.unsigned_abs())
                } else {
                    Some(index as usize)
                };
                current = offset
                    .and_then(|offset| items.get_mut(offset))
                    .ok_or("config list index is out of bounds")?;
                parts = rest;
            }
            _ => return Err("cannot descend into a scalar config value".into()),
        }
    }
    Ok(current)
}

fn replace_config(home: &Path, path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600));
    }
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(home)?.sync_all()?;
    Ok(())
}

/// Atomically set or update `KEY=VALUE` in `<home>/.env` with owner-only (`0600`) permissions.
pub fn set_env_value(home: &Path, key: &str, value: &str) -> Result<std::path::PathBuf, String> {
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!("invalid environment variable name: {key:?}"));
    }
    let value = value.trim();
    if value.contains(['\n', '\r']) {
        return Err(format!(
            "environment variable {key} cannot contain newlines"
        ));
    }

    fs::create_dir_all(home)
        .map_err(|error| format!("cannot create profile directory: {error}"))?;
    let env_path = home.join(".env");
    let existing = fs::read_to_string(&env_path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    let prefix = format!("{key}=");
    let formatted = format_env_assignment(key, value);
    let mut replaced = false;
    for line in &mut lines {
        let stripped = line.trim_start();
        let after_export = stripped.strip_prefix("export ").unwrap_or(stripped);
        if after_export.starts_with(&prefix) {
            *line = formatted.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(formatted);
    }
    let mut content = lines.join("\n");
    if !content.ends_with('\n') {
        content.push('\n');
    }

    let mut staged = tempfile::Builder::new()
        .prefix(".env_")
        .suffix(".tmp")
        .tempfile_in(home)
        .map_err(|error| format!("cannot stage .env: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600));
    }
    staged
        .write_all(content.as_bytes())
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|error| format!("cannot write .env: {error}"))?;
    staged
        .persist(&env_path)
        .map_err(|error| format!("cannot save .env: {}", error.error))?;
    Ok(env_path)
}

fn format_env_assignment(key: &str, value: &str) -> String {
    if value.is_empty()
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'#' | b'"' | b'\'' | b'$'))
    {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key}=\"{escaped}\"")
    } else {
        format!("{key}={value}")
    }
}

/// Summary of directories, starter files, and database initialized for a Hermes profile.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ProfileScaffoldReport {
    pub created_dirs: Vec<String>,
    pub created_files: Vec<String>,
    pub database_initialized: bool,
}

/// Required profile subdirectories and whether they should be restricted to `0700` on Unix.
pub const PROFILE_DIRECTORIES: &[(&str, bool)] = &[
    ("memories", false),
    ("skills", false),
    ("plugins", false),
    ("scripts", false),
    ("cron", true),
    ("cron/output", true),
    ("cron/cancels", true),
    ("cache/scratch", true),
    ("cache/images", false),
    ("cache/audio", false),
    ("cache/documents", false),
    ("cache/spillover", false),
    ("logs", false),
    ("vault", true),
    ("mcp_oauth", true),
    ("rust-owner", true),
];

/// Create all runtime directories, starter files, and canonical `state.db` in `home` without
/// overwriting any existing user files or databases.
pub fn ensure_profile_scaffold(home: &Path) -> Result<ProfileScaffoldReport, String> {
    let mut report = ProfileScaffoldReport::default();
    if !home.exists() {
        fs::create_dir_all(home).map_err(|error| {
            format!(
                "cannot create profile directory {}: {error}",
                home.display()
            )
        })?;
        report.created_dirs.push(".".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(home, fs::Permissions::from_mode(0o700));
    }

    for &(rel, private) in PROFILE_DIRECTORIES {
        let dir = home.join(rel);
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .map_err(|error| format!("cannot create directory {rel}: {error}"))?;
            report.created_dirs.push(rel.to_owned());
        }
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        let _ = private;
    }

    let starter_files: &[(&str, &str, bool)] = &[
        (
            ".env",
            "# Hermes Agent profile environment variables (chmod 600)\n",
            true,
        ),
        (
            "config.yaml",
            "# Hermes Agent configuration (run `hsr setup` to update)\n",
            true,
        ),
        ("memories/MEMORY.md", "", false),
        ("memories/USER.md", "", false),
        ("cron/jobs.json", "{\"jobs\": []}\n", true),
    ];

    for &(rel, contents, private) in starter_files {
        let path = home.join(rel);
        if !path.exists() {
            let parent = path.parent().unwrap_or(home);
            let mut tmp = tempfile::NamedTempFile::new_in(parent)
                .map_err(|error| format!("cannot stage {rel}: {error}"))?;
            #[cfg(unix)]
            if private {
                use std::os::unix::fs::PermissionsExt;
                let _ = tmp
                    .as_file()
                    .set_permissions(fs::Permissions::from_mode(0o600));
            }
            #[cfg(not(unix))]
            let _ = private;
            tmp.write_all(contents.as_bytes())
                .and_then(|()| tmp.as_file().sync_all())
                .map_err(|error| format!("cannot write {rel}: {error}"))?;
            match tmp.persist_noclobber(&path) {
                Ok(_) => report.created_files.push(rel.to_owned()),
                Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(format!("cannot create {rel}: {}", err.error)),
            }
        }
    }

    match crate::storage::ensure_initialized(home) {
        Ok(created) => {
            report.database_initialized = created;
            if created {
                report.created_files.push("state.db".into());
            }
        }
        Err(crate::storage::StorageError::ReadOnlySchema(_)) => {
            // Preserve legacy databases untouched; `hsr doctor` / `hsr sessions migrate` handles them.
        }
        Err(error) => return Err(format!("cannot initialize state.db: {error}")),
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_creates_all_dirs_and_preserves_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("profile");

        let first = ensure_profile_scaffold(&home).unwrap();
        assert!(first.database_initialized);
        for &(rel, _) in PROFILE_DIRECTORIES {
            assert!(home.join(rel).is_dir(), "missing directory {rel}");
        }
        assert!(home.join(".env").is_file());
        assert!(home.join("config.yaml").is_file());
        assert!(home.join("memories/MEMORY.md").is_file());
        assert!(home.join("memories/USER.md").is_file());
        assert_eq!(
            fs::read_to_string(home.join("cron/jobs.json")).unwrap(),
            "{\"jobs\": []}\n"
        );
        assert!(home.join("state.db").is_file());

        // Populate a user file and verify a second run never clobbers it.
        fs::write(home.join("memories/MEMORY.md"), "Remember Rust\n").unwrap();
        let second = ensure_profile_scaffold(&home).unwrap();
        assert!(!second.database_initialized);
        assert!(second.created_dirs.is_empty());
        assert!(second.created_files.is_empty());
        assert_eq!(
            fs::read_to_string(home.join("memories/MEMORY.md")).unwrap(),
            "Remember Rust\n"
        );
    }

    #[test]
    fn set_env_value_updates_in_place_and_quotes_spaces() {
        let dir = tempfile::tempdir().unwrap();
        set_env_value(dir.path(), "OPENROUTER_API_KEY", "sk-or-1").unwrap();
        set_env_value(dir.path(), "OPENROUTER_API_KEY", "sk-or-2").unwrap();
        set_env_value(dir.path(), "CUSTOM_NOTE", "hello world").unwrap();
        let raw = fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(raw.contains("OPENROUTER_API_KEY=sk-or-2\n"));
        assert!(!raw.contains("sk-or-1"));
        assert!(raw.contains("CUSTOM_NOTE=\"hello world\"\n"));
        assert!(set_env_value(dir.path(), "BAD-KEY", "x").is_err());
        assert!(set_env_value(dir.path(), "GOOD_KEY", "a\nb").is_err());
    }
}
