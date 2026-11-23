//! Durable cron execution ledger stored in `<home>/cron/executions.db`.
//!
//! Mirrors `cron/executions.py` of the Python reference: shares `executions.db` with
//! `cron_incidents`, records claimed/running/terminal attempts, and prunes terminal history to
//! `MAX_TERMINAL_EXECUTIONS`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::manage::random_hex;
use super::schedule::iso_instant;
use super::store::cron_dir;
use super::tick::machine_id;

pub const MAX_TERMINAL_EXECUTIONS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExecutionRecord {
    pub id: String,
    pub job_id: String,
    pub source: String,
    pub process_id: String,
    pub pid: i64,
    pub status: String,
    pub claimed_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub delivery_outcome: Option<String>,
    pub scheduled_instant: Option<String>,
    pub progress_at: Option<String>,
}

pub(super) fn db_path(home: &Path) -> PathBuf {
    cron_dir(home).join("executions.db")
}

pub(super) fn open_db(home: &Path) -> Result<Connection, String> {
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
        .map_err(|error| format!("cannot open cron executions database: {error}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("cannot configure cron executions database: {error}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS executions (
             id TEXT PRIMARY KEY,
             job_id TEXT NOT NULL,
             source TEXT NOT NULL,
             process_id TEXT NOT NULL,
             pid INTEGER NOT NULL,
             process_started_at INTEGER,
             status TEXT NOT NULL CHECK(status IN ('claimed','running','completed','failed','unknown','cancelled','missed')),
             handoff_pending INTEGER NOT NULL DEFAULT 0,
             handoff_started_at REAL,
             claimed_at TEXT NOT NULL,
             started_at TEXT,
             finished_at TEXT,
             error TEXT,
             delivery_outcome TEXT,
             scheduled_instant TEXT,
             progress_at TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_executions_job_id ON executions(job_id, claimed_at DESC);
         CREATE INDEX IF NOT EXISTS idx_executions_status ON executions(status);",
    )
    .map_err(|error| format!("cannot initialize cron executions schema: {error}"))?;
    Ok(conn)
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExecutionRecord> {
    Ok(ExecutionRecord {
        id: row.get(0)?,
        job_id: row.get(1)?,
        source: row.get(2)?,
        process_id: row.get(3)?,
        pid: row.get(4)?,
        status: row.get(5)?,
        claimed_at: row.get(6)?,
        started_at: row.get(7)?,
        finished_at: row.get(8)?,
        error: row.get(9)?,
        delivery_outcome: row.get(10)?,
        scheduled_instant: row.get(11)?,
        progress_at: row.get(12)?,
    })
}

const SELECT_COLS: &str = "id, job_id, source, process_id, pid, status, claimed_at, started_at, finished_at, error, delivery_outcome, scheduled_instant, progress_at";

/// Insert a new `claimed` execution attempt and return its generated execution ID.
pub fn create_execution(
    home: &Path,
    job_id: &str,
    source: &str,
    now: DateTime<Tz>,
    scheduled_instant: Option<&str>,
) -> Result<String, String> {
    let id = format!("exec_{}", random_hex(8)?);
    let claimed_at = iso_instant(now);
    let process_id = machine_id();
    let pid = std::process::id() as i64;
    let conn = open_db(home)?;
    conn.execute(
        "INSERT INTO executions (id, job_id, source, process_id, pid, status, claimed_at, scheduled_instant)
         VALUES (?1, ?2, ?3, ?4, ?5, 'claimed', ?6, ?7)",
        params![id, job_id, source, process_id, pid, claimed_at, scheduled_instant],
    )
    .map_err(|error| format!("cannot record cron execution: {error}"))?;
    Ok(id)
}

/// Transition an execution from `claimed` to `running`.
pub fn mark_execution_running(
    home: &Path,
    execution_id: &str,
    now: DateTime<Tz>,
) -> Result<(), String> {
    let stamp = iso_instant(now);
    let conn = open_db(home)?;
    conn.execute(
        "UPDATE executions
         SET status = 'running', started_at = COALESCE(started_at, ?1), progress_at = ?1
         WHERE id = ?2 AND status IN ('claimed', 'running')",
        params![stamp, execution_id],
    )
    .map_err(|error| format!("cannot mark cron execution running: {error}"))?;
    Ok(())
}

/// Stamp liveness progress for an in-flight execution.
pub fn touch_progress(home: &Path, execution_id: &str, now: DateTime<Tz>) -> Result<(), String> {
    let stamp = iso_instant(now);
    let conn = open_db(home)?;
    conn.execute(
        "UPDATE executions SET progress_at = ?1 WHERE id = ?2 AND status IN ('claimed', 'running')",
        params![stamp, execution_id],
    )
    .map_err(|error| format!("cannot stamp cron execution progress: {error}"))?;
    Ok(())
}

/// Mark an execution terminal (`completed`, `failed`, `cancelled`, `unknown`, `missed`) and prune
/// terminal rows past `MAX_TERMINAL_EXECUTIONS`.
pub fn finish_execution(
    home: &Path,
    execution_id: &str,
    status: &str,
    error: Option<&str>,
    delivery_outcome: Option<&str>,
    now: DateTime<Tz>,
) -> Result<(), String> {
    if !matches!(
        status,
        "completed" | "failed" | "cancelled" | "unknown" | "missed"
    ) {
        return Err(format!("invalid terminal execution status: {status}"));
    }
    let stamp = iso_instant(now);
    let mut conn = open_db(home)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("cannot begin execution finish transaction: {err}"))?;
    tx.execute(
        "UPDATE executions
         SET status = ?1, finished_at = ?2, error = ?3, delivery_outcome = ?4
         WHERE id = ?5",
        params![status, stamp, error, delivery_outcome, execution_id],
    )
    .map_err(|err| format!("cannot finish cron execution: {err}"))?;
    // Prune oldest terminal records beyond MAX_TERMINAL_EXECUTIONS.
    tx.execute(
        "DELETE FROM executions
         WHERE status IN ('completed', 'failed', 'unknown', 'cancelled', 'missed')
           AND id NOT IN (
               SELECT id FROM executions
               WHERE status IN ('completed', 'failed', 'unknown', 'cancelled', 'missed')
               ORDER BY COALESCE(finished_at, claimed_at) DESC
               LIMIT ?1
           )",
        params![MAX_TERMINAL_EXECUTIONS as i64],
    )
    .map_err(|err| format!("cannot prune cron executions: {err}"))?;
    tx.commit()
        .map_err(|err| format!("cannot commit cron execution finish: {err}"))?;
    Ok(())
}

/// Fetch one execution record by ID.
pub fn get_execution(home: &Path, execution_id: &str) -> Result<Option<ExecutionRecord>, String> {
    if !db_path(home).is_file() {
        return Ok(None);
    }
    let conn = open_db(home)?;
    let sql = format!("SELECT {SELECT_COLS} FROM executions WHERE id = ?1");
    conn.query_row(&sql, params![execution_id], decode_row)
        .optional()
        .map_err(|error| format!("cannot read cron execution: {error}"))
}

/// List recent executions, newest first, optionally filtered by `job_id`.
pub fn list_executions(
    home: &Path,
    job_id: Option<&str>,
    limit: usize,
) -> Result<Vec<ExecutionRecord>, String> {
    if !db_path(home).is_file() {
        return Ok(Vec::new());
    }
    let conn = open_db(home)?;
    let limit = limit.clamp(1, 500) as i64;
    let mut records = Vec::new();
    if let Some(job_id) = job_id {
        let sql = format!(
            "SELECT {SELECT_COLS} FROM executions
             WHERE job_id = ?1 ORDER BY claimed_at DESC LIMIT ?2"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|error| format!("cannot query cron executions: {error}"))?;
        let rows = stmt
            .query_map(params![job_id, limit], decode_row)
            .map_err(|error| format!("cannot read cron executions: {error}"))?;
        for row in rows {
            records.push(row.map_err(|error| format!("cannot decode execution: {error}"))?);
        }
    } else {
        let sql = format!("SELECT {SELECT_COLS} FROM executions ORDER BY claimed_at DESC LIMIT ?1");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|error| format!("cannot query cron executions: {error}"))?;
        let rows = stmt
            .query_map(params![limit], decode_row)
            .map_err(|error| format!("cannot read cron executions: {error}"))?;
        for row in rows {
            records.push(row.map_err(|error| format!("cannot decode execution: {error}"))?);
        }
    }
    Ok(records)
}

/// Find the newest live (`claimed` or `running`) execution for `job_id`, if any.
pub fn find_live_execution(home: &Path, job_id: &str) -> Result<Option<ExecutionRecord>, String> {
    if !db_path(home).is_file() {
        return Ok(None);
    }
    let conn = open_db(home)?;
    let sql = format!(
        "SELECT {SELECT_COLS} FROM executions
         WHERE job_id = ?1 AND status IN ('claimed', 'running')
         ORDER BY claimed_at DESC LIMIT 1"
    );
    conn.query_row(&sql, params![job_id], decode_row)
        .optional()
        .map_err(|error| format!("cannot query live cron execution: {error}"))
}
