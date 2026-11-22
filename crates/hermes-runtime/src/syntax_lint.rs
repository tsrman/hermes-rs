//! In-process syntax checks run after an edit, as the reference's lint tier does for the formats
//! this build can parse. Other file types report `skipped`.

use serde_json::{json, Value};

enum Verdict {
    Ok,
    Error(String),
    Skipped(String),
}

fn check(extension: &str, content: &str) -> Verdict {
    match extension {
        ".json" => match serde_json::from_str::<Value>(content) {
            Ok(_) => Verdict::Ok,
            Err(error) => Verdict::Error(format!(
                "JSONDecodeError: {} (line {}, column {})",
                error.to_string().split(" at line ").next().unwrap_or(""),
                error.line(),
                error.column()
            )),
        },
        ".yaml" | ".yml" => {
            for document in serde_yaml::Deserializer::from_str(content) {
                if let Err(error) = <serde_yaml::Value as serde::Deserialize>::deserialize(document)
                {
                    return Verdict::Error(format!("YAMLError: {error}"));
                }
            }
            Verdict::Ok
        }
        other => Verdict::Skipped(format!("No linter for {other} files")),
    }
}

fn extension_of(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(String::new, |e| format!(".{}", e.to_lowercase()))
}

/// The lint result of an edit: `None` when nothing was checked. With the text before the edit,
/// only errors the edit introduced are reported.
pub(crate) fn lint_delta(path: &str, before: Option<&str>, after: &str) -> Value {
    let extension = extension_of(path);
    match check(&extension, after) {
        Verdict::Ok => json!({"status": "ok", "output": ""}),
        Verdict::Skipped(message) => json!({"status": "skipped", "message": message}),
        Verdict::Error(output) => {
            let Some(before) = before else {
                return json!({"status": "error", "output": output});
            };
            let Verdict::Error(earlier) = check(&extension, before) else {
                return json!({"status": "error", "output": output});
            };
            let known: Vec<&str> = earlier
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let fresh: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !known.contains(l))
                .collect();
            if fresh.is_empty() {
                return json!({"status": "error", "output": output, "message":
                    "Pre-existing lint errors — this edit didn't introduce new ones but the file is still broken."});
            }
            json!({"status": "error", "output": format!(
                "New lint errors introduced by this edit (pre-existing errors filtered out):\n{}",
                fresh.join("\n"))})
        }
    }
}
