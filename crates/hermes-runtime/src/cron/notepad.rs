//! Per-job durable key/value notepad stored in `<home>/cron/notepad.db`.
//!
//! Mirrors `cron/notepad.py` of the Python reference: exact table schema, byte/character caps,
//! error messages, and prompt rendering (empty notepad renders as `""`, keeping unused jobs'
//! prompts byte-identical).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::schedule::iso_instant;
use super::store::cron_dir;

pub const MAX_KEY_CHARS: usize = 128;
pub const MAX_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_JOB_TOTAL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Note {
    pub job_id: String,
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

fn db_path(home: &Path) -> PathBuf {
    cron_dir(home).join("notepad.db")
}

fn open_db(home: &Path) -> Result<Connection, String> {
    let dir = cron_dir(home);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create cron directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = db_path(home);
    let conn = Connection::open(&path)
        .map_err(|error| format!("cannot open cron notepad database: {error}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("cannot configure cron notepad database: {error}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS cron_notepad (
             job_id TEXT NOT NULL,
             key TEXT NOT NULL,
             value TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             PRIMARY KEY (job_id, key)
         );",
    )
    .map_err(|error| format!("cannot initialize cron notepad schema: {error}"))?;
    Ok(conn)
}

fn validate(job_id: &str, key: &str, value: &str) -> Result<(), String> {
    if job_id.is_empty() {
        return Err("job_id must be non-empty".into());
    }
    if key.is_empty() {
        return Err("key must be non-empty".into());
    }
    if key.chars().count() > MAX_KEY_CHARS {
        return Err(format!("key too long (max {MAX_KEY_CHARS} characters)"));
    }
    if value.len() > MAX_VALUE_BYTES {
        return Err(format!(
            "value too large (max {MAX_VALUE_BYTES} bytes per key)"
        ));
    }
    Ok(())
}

/// Upsert one key for `job_id`. Rejects values exceeding per-key or per-job total byte caps.
pub fn set_note(
    home: &Path,
    job_id: &str,
    key: &str,
    value: &str,
    now: DateTime<Tz>,
) -> Result<Note, String> {
    validate(job_id, key, value)?;
    let mut conn = open_db(home)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("cannot begin notepad transaction: {error}"))?;
    let other_bytes: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(CAST(key AS BLOB)) + LENGTH(CAST(value AS BLOB))), 0)
             FROM cron_notepad WHERE job_id = ?1 AND key <> ?2",
            params![job_id, key],
            |row| row.get(0),
        )
        .map_err(|error| format!("cannot check notepad size: {error}"))?;
    let entry_bytes = (key.len() + value.len()) as i64;
    if other_bytes + entry_bytes > MAX_JOB_TOTAL_BYTES as i64 {
        return Err(format!(
            "notepad full: job '{job_id}' would exceed {MAX_JOB_TOTAL_BYTES} bytes total; delete unused keys first"
        ));
    }
    let updated_at = iso_instant(now);
    tx.execute(
        "INSERT INTO cron_notepad (job_id, key, value, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(job_id, key)
         DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![job_id, key, value, updated_at],
    )
    .map_err(|error| format!("cannot write notepad entry: {error}"))?;
    tx.commit()
        .map_err(|error| format!("cannot commit notepad entry: {error}"))?;
    Ok(Note {
        job_id: job_id.to_owned(),
        key: key.to_owned(),
        value: value.to_owned(),
        updated_at,
    })
}

/// Fetch one key, or `None` if absent (or if `notepad.db` does not exist yet).
pub fn get_note(home: &Path, job_id: &str, key: &str) -> Result<Option<String>, String> {
    if !db_path(home).is_file() {
        return Ok(None);
    }
    let conn = open_db(home)?;
    conn.query_row(
        "SELECT value FROM cron_notepad WHERE job_id = ?1 AND key = ?2",
        params![job_id, key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| format!("cannot read notepad entry: {error}"))
}

/// Delete one key; returns whether a row was removed.
pub fn delete_note(home: &Path, job_id: &str, key: &str) -> Result<bool, String> {
    if !db_path(home).is_file() {
        return Ok(false);
    }
    let conn = open_db(home)?;
    let deleted = conn
        .execute(
            "DELETE FROM cron_notepad WHERE job_id = ?1 AND key = ?2",
            params![job_id, key],
        )
        .map_err(|error| format!("cannot delete notepad entry: {error}"))?;
    Ok(deleted > 0)
}

/// List all keys for `job_id`, sorted by key ascending.
pub fn list_notes(home: &Path, job_id: &str) -> Result<Vec<Note>, String> {
    if !db_path(home).is_file() {
        return Ok(Vec::new());
    }
    let conn = open_db(home)?;
    let mut stmt = conn
        .prepare(
            "SELECT job_id, key, value, updated_at
             FROM cron_notepad WHERE job_id = ?1 ORDER BY key ASC",
        )
        .map_err(|error| format!("cannot query notepad: {error}"))?;
    let rows = stmt
        .query_map(params![job_id], |row| {
            Ok(Note {
                job_id: row.get(0)?,
                key: row.get(1)?,
                value: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })
        .map_err(|error| format!("cannot read notepad rows: {error}"))?;
    let mut notes = Vec::new();
    for row in rows {
        notes.push(row.map_err(|error| format!("cannot decode notepad row: {error}"))?);
    }
    Ok(notes)
}

/// Delete every notepad row for `job_id`. Does not create `notepad.db` if absent.
pub fn clear_notepad(home: &Path, job_id: &str) -> Result<usize, String> {
    let path = db_path(home);
    if !path.is_file() {
        return Ok(0);
    }
    let conn = open_db(home)?;
    conn.execute(
        "DELETE FROM cron_notepad WHERE job_id = ?1",
        params![job_id],
    )
    .map_err(|error| format!("cannot clear cron notepad: {error}"))
}

/// Render the job's notepad as a prompt section, or `""` when empty (byte-identical prompt for
/// jobs that do not use the notepad).
pub fn render_notepad_section(home: &Path, job_id: &str) -> String {
    let Ok(notes) = list_notes(home, job_id) else {
        return String::new();
    };
    if notes.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "## Job notepad (persistent across runs)\n\
         This durable scratchpad survives between scheduled runs of this job. Update it via the CLI, e.g.:\n\
         `hermes cron notepad {job_id} set <key> <value>` (also: get/delete/list; `hermes cron notepad {job_id} delete <key>` removes an entry).\n\n"
    );
    for note in notes {
        out.push_str(&format!("- {}: {}\n", note.key, note.value));
    }
    out.push('\n');
    out
}
