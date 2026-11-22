//! A debug journal: one JSON object per line describing what a turn sent, received and ran.
//!
//! It exists to explain a run afterwards (what the model saw, which tool ran with which
//! arguments, how long it took). Strings are redacted before they are written, and the file is
//! private to its owner.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::redact::redact_sensitive_text;

pub struct Journal {
    file: Mutex<File>,
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), redact_value(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

impl Journal {
    /// Open (or create) the journal at `path`, appending to what is there.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(|error| {
            format!("cannot open the debug journal {}: {error}", path.display())
        })?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Write one event. A failed write is ignored: the journal must never break a run.
    pub fn record(&self, event: &str, mut fields: Value) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64());
        let mut line = json!({"at": at, "event": event});
        if let (Some(target), Value::Object(extra)) = (line.as_object_mut(), &mut fields) {
            for (key, value) in std::mem::take(extra) {
                target.insert(key, redact_value(&value));
            }
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_written_redacted_one_per_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.jsonl");
        let journal = Journal::open(&path).unwrap();
        journal.record(
            "tool_result",
            json!({"name": "terminal", "output": "key ghp_abcdefghijklmnop1234567890"}),
        );
        journal.record("note", json!({"n": 2}));
        let text = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["event"], "tool_result");
        assert_eq!(rows[0]["output"], "key ghp_ab...7890");
        assert!(rows[0]["at"].as_f64().unwrap() > 0.0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
