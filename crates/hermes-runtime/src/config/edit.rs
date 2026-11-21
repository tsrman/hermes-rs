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
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(home)?.sync_all()?;
    Ok(())
}
