//! One in-memory conversation turn with local file reading.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::providers::openai::{OpenAiProvider, ProviderError};

#[derive(Debug)]
pub enum TurnError {
    Provider(ProviderError),
    InvalidCompletion,
    IterationLimit,
}

pub async fn run_turn<F>(
    provider: &OpenAiProvider,
    model: &str,
    query: &str,
    root: &Path,
    mut on_text: F,
) -> Result<(), TurnError>
where
    F: FnMut(&str),
{
    let mut messages = vec![json!({"role": "user", "content": query})];
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a UTF-8 file within the current working directory with line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1, "default": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 2000}
                },
                "required": ["path"]
            }
        }
    }]);
    for _ in 0..16 {
        let request = json!({"model": model, "messages": messages, "tools": tools});
        let result = provider
            .complete(&request, &mut on_text)
            .await
            .map_err(TurnError::Provider)?;
        match result.finish_reason.as_deref() {
            Some("stop") if result.tool_calls.is_empty() => return Ok(()),
            Some("tool_calls") if !result.tool_calls.is_empty() => {}
            _ => return Err(TurnError::InvalidCompletion),
        }
        let calls: Vec<Value> = result
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments}
                })
            })
            .collect();
        messages.push(json!({"role": "assistant", "content": if result.content.is_empty() { None } else { Some(result.content) }, "tool_calls": calls}));
        for call in result.tool_calls {
            let output = if call.name == "read_file" {
                read_file(root, &call.arguments)
            } else {
                json!({"error": format!("Unknown tool: {}", call.name)})
            };
            messages.push(
                json!({"role": "tool", "tool_call_id": call.id, "content": output.to_string()}),
            );
        }
    }
    Err(TurnError::IterationLimit)
}

fn read_file(root: &Path, arguments: &str) -> Value {
    match read_file_inner(root, arguments) {
        Ok(value) => value,
        Err(error) => json!({"error": error}),
    }
}

fn read_file_inner(root: &Path, arguments: &str) -> Result<Value, String> {
    let args: Value = serde_json::from_str(arguments).map_err(|_| "Invalid tool arguments")?;
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("Missing path")?;
    let offset = args
        .get("offset")
        .map_or(Some(1), Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or("Invalid offset")?;
    let limit = args
        .get("limit")
        .map_or(Some(2000), Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or("Invalid limit")?
        .min(2000);
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let path = PathBuf::from(path);
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !resolved.starts_with(&root) {
        return Err("Path is outside the working directory".into());
    }
    let filename = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if [
        ".env",
        ".env.local",
        ".env.development",
        ".env.production",
        ".env.test",
        ".env.staging",
        ".envrc",
    ]
    .iter()
    .any(|blocked| filename.eq_ignore_ascii_case(blocked))
    {
        return Err("Project environment files cannot be read directly".into());
    }
    let metadata = resolved.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > 10_000_000 {
        return Err("File must be a regular file no larger than 10 MB".into());
    }
    let data = std::fs::read_to_string(resolved).map_err(|error| error.to_string())?;
    let lines: Vec<&str> = data.lines().collect();
    let start = usize::try_from(offset - 1)
        .unwrap_or(usize::MAX)
        .min(lines.len());
    let end = start.saturating_add(limit as usize).min(lines.len());
    let content = lines[start..end]
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{}|{}", start + index + 1, line))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(
        json!({"content": content, "total_lines": lines.len(), "file_size": metadata.len(), "truncated": end < lines.len(), "is_binary": false, "is_image": false, "not_found": false}),
    )
}

#[cfg(test)]
mod tests {
    use super::read_file;
    use serde_json::json;

    #[test]
    fn reads_paginated_utf8_with_one_based_line_numbers() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("file.txt"), "one\nдва\nthree\n").unwrap();
        let result = read_file(
            directory.path(),
            r#"{"path":"file.txt","offset":2,"limit":1}"#,
        );
        assert_eq!(result["content"], "2|два");
        assert_eq!(result["total_lines"], 3);
        assert_eq!(result["file_size"], 17);
        assert_eq!(result["truncated"], true);
    }

    #[test]
    fn rejects_paths_outside_the_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let arguments = json!({"path": outside.path()}).to_string();
        let result = read_file(directory.path(), &arguments);
        assert_eq!(result["error"], "Path is outside the working directory");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside_the_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
        let result = read_file(directory.path(), r#"{"path":"link"}"#);
        assert_eq!(result["error"], "Path is outside the working directory");
    }

    #[test]
    fn rejects_invalid_pagination_and_returns_error_for_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let invalid = read_file(directory.path(), r#"{"path":"file","offset":0}"#);
        assert_eq!(invalid["error"], "Invalid offset");
        let missing = read_file(directory.path(), r#"{"path":"missing"}"#);
        assert!(missing["error"].is_string());
    }

    #[test]
    fn rejects_project_env_files_even_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".env"), "TOKEN=secret").unwrap();
        let direct = read_file(directory.path(), r#"{"path":".env"}"#);
        assert!(direct["error"].is_string());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(".env", directory.path().join("env-link")).unwrap();
            let linked = read_file(directory.path(), r#"{"path":"env-link"}"#);
            assert!(linked["error"].is_string());
        }
    }
}
