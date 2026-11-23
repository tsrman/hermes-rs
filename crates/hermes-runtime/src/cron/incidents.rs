//! Durable cron failure incidents stored in `<home>/cron/executions.db`.
//!
//! Mirrors `cron/incidents.py` of the Python reference: exact signature hashing (with duration
//! masking), failure-type classification order, and lifecycle transitions (`detected` -> `alerted`
//! -> `resolved` on recovery / `closed` on operator ack).

use std::path::Path;
use std::sync::LazyLock;

use chrono::DateTime;
use chrono_tz::Tz;
use regex::Regex;
use ring::digest;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::executions;
use super::schedule::iso_instant;

pub const MAX_ERROR_CHARS: usize = 500;
const MAX_SIGNATURE_ERROR_CHARS: usize = 200;

static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid ws regex"));
static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d+(?:\.\d+)?\s*(?:ms|s|secs?|seconds?|m|mins?|minutes?|h|hrs?|hours?)\b")
        .expect("valid duration regex")
});
static RE_429: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b429\b").expect("valid regex"));
static RE_401: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b401\b").expect("valid regex"));

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Incident {
    pub id: String,
    pub job_id: String,
    pub error_sig: String,
    pub state: String,
    pub failure_type: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub acked_at: Option<String>,
    pub alerted_at: Option<String>,
    pub closed_at: Option<String>,
    pub error: String,
    pub output_file: Option<String>,
}

fn open_db(home: &Path) -> Result<Connection, String> {
    let conn = executions::open_db(home)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cron_incidents (
             id            TEXT PRIMARY KEY,
             job_id        TEXT NOT NULL,
             error_sig     TEXT NOT NULL,
             state         TEXT NOT NULL,
             failure_type  TEXT NOT NULL DEFAULT 'unknown',
             first_seen_at TEXT NOT NULL,
             last_seen_at  TEXT NOT NULL,
             acked_at      TEXT,
             alerted_at    TEXT,
             closed_at     TEXT,
             error         TEXT NOT NULL,
             output_file   TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_cron_incidents_job ON cron_incidents(job_id);
         CREATE INDEX IF NOT EXISTS idx_cron_incidents_state ON cron_incidents(state);",
    )
    .map_err(|error| format!("cannot initialize cron incidents schema: {error}"))?;
    // Upgrade older ledgers missing alerted_at.
    let _ = conn.execute("ALTER TABLE cron_incidents ADD COLUMN alerted_at TEXT", []);
    Ok(conn)
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Incident> {
    Ok(Incident {
        id: row.get(0)?,
        job_id: row.get(1)?,
        error_sig: row.get(2)?,
        state: row.get(3)?,
        failure_type: row.get(4)?,
        first_seen_at: row.get(5)?,
        last_seen_at: row.get(6)?,
        acked_at: row.get(7)?,
        alerted_at: row.get(8)?,
        closed_at: row.get(9)?,
        error: row.get(10)?,
        output_file: row.get(11)?,
    })
}

const SELECT_COLS: &str = "id, job_id, error_sig, state, failure_type, first_seen_at, last_seen_at, acked_at, alerted_at, closed_at, error, output_file";

/// Collapse whitespace and lowercase before signing (dedup normalization).
pub fn normalize_error(error: &str) -> String {
    WS_RE.replace_all(error, " ").trim().to_lowercase()
}

fn redact_and_bound(error: &str) -> String {
    let scrubbed = crate::redact::redact_with(
        error,
        crate::redact::Mode {
            force: true,
            ..Default::default()
        },
    );
    scrubbed.chars().take(MAX_ERROR_CHARS).collect()
}

/// Stable 12-hex-char dedup key for `(job_id, duration-masked normalized error[:200])`.
pub fn error_signature(job_id: &str, error: &str) -> String {
    let normalized = normalize_error(error);
    let masked = DURATION_RE.replace_all(&normalized, "#s");
    let prefix: String = masked.chars().take(MAX_SIGNATURE_ERROR_CHARS).collect();
    let mut input = Vec::with_capacity(job_id.len() + prefix.len());
    input.extend_from_slice(job_id.as_bytes());
    input.extend_from_slice(prefix.as_bytes());
    let sum = digest::digest(&digest::SHA256, &input);
    let hex: String = sum.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_owned()
}

/// Deterministic incident ID: `<job_id[:6]>_<error_sig>`.
pub fn incident_id(job_id: &str, sig: &str) -> String {
    let prefix: String = job_id.chars().take(6).collect();
    format!("{prefix}_{sig}")
}

/// Classify a failure from error-text keywords in the reference's priority order.
pub fn classify_failure_type(error: &str) -> &'static str {
    let text = normalize_error(error);
    if text.is_empty() {
        return "unknown";
    }
    if RE_429.is_match(&text)
        || text.contains("rate limit")
        || text.contains("usage limit")
        || text.contains("quota")
    {
        return "rate_limit";
    }
    if text.contains("timeout") || text.contains("timed out") {
        return "timeout";
    }
    if RE_401.is_match(&text)
        || text.contains("unauthorized")
        || text.contains("authentication")
        || text.contains("auth")
    {
        return "auth";
    }
    if text.contains("delivery") || text.contains("deliver") || text.contains("delivering") {
        return "delivery";
    }
    if text.contains("config") || text.contains("configuration") || text.contains("validation") {
        return "config";
    }
    if text.contains("script") || text.contains("no_agent") {
        return "script";
    }
    if text.contains("agent")
        || text.contains("model")
        || text.contains("provider")
        || text.contains("inference")
    {
        return "agent";
    }
    "unknown"
}

/// Upsert an incident for `(job_id, error)`. Returns `(incident_id, is_new_or_reopened)`.
///
/// - New signature -> inserts `detected`, returns `(id, true)`.
/// - Existing `resolved` -> re-opens to `detected` (clears `closed_at`), returns `(id, true)`.
/// - Existing `detected`, `alerted`, or `closed` -> refreshes `last_seen_at`/`error`/`output_file`
///   without altering state (`closed` stays silent!), returns `(id, false)`.
pub fn upsert_incident(
    home: &Path,
    job_id: &str,
    error: &str,
    output_file: Option<&str>,
    now: DateTime<Tz>,
) -> Result<(String, bool), String> {
    let sig = error_signature(job_id, error);
    let iid = incident_id(job_id, &sig);
    let stamp = iso_instant(now);
    let scrubbed = redact_and_bound(error);
    let failure_type = classify_failure_type(error);

    let mut conn = open_db(home)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("cannot begin incident transaction: {err}"))?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT state FROM cron_incidents WHERE id = ?1",
            params![iid],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| format!("cannot query cron incident: {err}"))?;

    let is_new = match existing.as_deref() {
        None => {
            tx.execute(
                "INSERT INTO cron_incidents
                 (id, job_id, error_sig, state, failure_type, first_seen_at, last_seen_at, error, output_file)
                 VALUES (?1, ?2, ?3, 'detected', ?4, ?5, ?5, ?6, ?7)",
                params![iid, job_id, sig, failure_type, stamp, scrubbed, output_file],
            )
            .map_err(|err| format!("cannot insert cron incident: {err}"))?;
            true
        }
        Some("resolved") => {
            tx.execute(
                "UPDATE cron_incidents
                 SET state = 'detected', last_seen_at = ?1, closed_at = NULL, error = ?2,
                     output_file = COALESCE(?3, output_file), failure_type = ?4
                 WHERE id = ?5",
                params![stamp, scrubbed, output_file, failure_type, iid],
            )
            .map_err(|err| format!("cannot reopen cron incident: {err}"))?;
            true
        }
        Some(_) => {
            tx.execute(
                "UPDATE cron_incidents
                 SET last_seen_at = ?1, error = ?2, output_file = COALESCE(?3, output_file)
                 WHERE id = ?4",
                params![stamp, scrubbed, output_file, iid],
            )
            .map_err(|err| format!("cannot refresh cron incident: {err}"))?;
            false
        }
    };
    tx.commit()
        .map_err(|err| format!("cannot commit cron incident: {err}"))?;
    Ok((iid, is_new))
}

/// Fetch one incident by ID.
pub fn get_incident(home: &Path, id: &str) -> Result<Option<Incident>, String> {
    if !executions::db_path(home).is_file() {
        return Ok(None);
    }
    let conn = open_db(home)?;
    let sql = format!("SELECT {SELECT_COLS} FROM cron_incidents WHERE id = ?1");
    conn.query_row(&sql, params![id], decode_row)
        .optional()
        .map_err(|err| format!("cannot read cron incident: {err}"))
}

/// Transition an incident's state. Never resurrects a `closed` incident.
pub fn set_incident_state(
    home: &Path,
    id: &str,
    state: &str,
    now: DateTime<Tz>,
) -> Result<bool, String> {
    if !matches!(state, "detected" | "alerted" | "resolved" | "closed") {
        return Err(format!("invalid incident state: {state}"));
    }
    if !executions::db_path(home).is_file() {
        return Ok(false);
    }
    let stamp = iso_instant(now);
    let conn = open_db(home)?;
    let updated = match state {
        "alerted" => conn.execute(
            "UPDATE cron_incidents SET state = 'alerted', alerted_at = ?1
             WHERE id = ?2 AND state <> 'closed'",
            params![stamp, id],
        ),
        "resolved" => conn.execute(
            "UPDATE cron_incidents SET state = 'resolved', closed_at = ?1
             WHERE id = ?2 AND state IN ('detected', 'alerted')",
            params![stamp, id],
        ),
        "closed" => conn.execute(
            "UPDATE cron_incidents SET state = 'closed', acked_at = COALESCE(acked_at, ?1), closed_at = ?1
             WHERE id = ?2",
            params![stamp, id],
        ),
        _ => conn.execute(
            "UPDATE cron_incidents SET state = 'detected', closed_at = NULL
             WHERE id = ?1 AND state <> 'closed'",
            params![id],
        ),
    }
    .map_err(|err| format!("cannot update cron incident state: {err}"))?;
    Ok(updated > 0)
}

/// Operator acknowledgement: marks an incident `closed` (terminal).
pub fn ack_incident(home: &Path, id: &str, now: DateTime<Tz>) -> Result<bool, String> {
    if !executions::db_path(home).is_file() {
        return Ok(false);
    }
    let stamp = iso_instant(now);
    let conn = open_db(home)?;
    let updated = conn
        .execute(
            "UPDATE cron_incidents
             SET state = 'closed', acked_at = ?1, closed_at = COALESCE(closed_at, ?1)
             WHERE id = ?2",
            params![stamp, id],
        )
        .map_err(|err| format!("cannot acknowledge cron incident: {err}"))?;
    Ok(updated > 0)
}

/// Auto-resolve open (`detected`/`alerted`) incidents for a job that just ran successfully.
/// Leaves operator-`closed` incidents untouched.
pub fn close_incidents_for_recovered_job(
    home: &Path,
    job_id: &str,
    now: DateTime<Tz>,
) -> Result<usize, String> {
    if !executions::db_path(home).is_file() {
        return Ok(0);
    }
    let stamp = iso_instant(now);
    let conn = open_db(home)?;
    conn.execute(
        "UPDATE cron_incidents
         SET state = 'resolved', closed_at = ?1
         WHERE job_id = ?2 AND state IN ('detected', 'alerted')",
        params![stamp, job_id],
    )
    .map_err(|err| format!("cannot resolve cron incidents: {err}"))
}

/// List incidents ordered by `last_seen_at` descending.
pub fn list_incidents(
    home: &Path,
    job_id: Option<&str>,
    state: Option<&str>,
    limit: usize,
) -> Result<Vec<Incident>, String> {
    if !executions::db_path(home).is_file() {
        return Ok(Vec::new());
    }
    let conn = open_db(home)?;
    let limit = limit.clamp(1, 500) as i64;
    let mut clauses = Vec::new();
    if job_id.is_some() {
        clauses.push("job_id = ?1");
    }
    if state.is_some() {
        clauses.push(if job_id.is_some() {
            "state = ?2"
        } else {
            "state = ?1"
        });
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let limit_param = match (job_id.is_some(), state.is_some()) {
        (true, true) => "?3",
        (true, false) | (false, true) => "?2",
        (false, false) => "?1",
    };
    let sql = format!(
        "SELECT {SELECT_COLS} FROM cron_incidents{where_sql} ORDER BY last_seen_at DESC LIMIT {limit_param}"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|err| format!("cannot query cron incidents: {err}"))?;
    let mut rows_out = Vec::new();
    match (job_id, state) {
        (Some(jid), Some(st)) => {
            let rows = stmt
                .query_map(params![jid, st, limit], decode_row)
                .map_err(|err| format!("cannot read cron incidents: {err}"))?;
            for row in rows {
                rows_out.push(row.map_err(|err| format!("cannot decode incident: {err}"))?);
            }
        }
        (Some(jid), None) => {
            let rows = stmt
                .query_map(params![jid, limit], decode_row)
                .map_err(|err| format!("cannot read cron incidents: {err}"))?;
            for row in rows {
                rows_out.push(row.map_err(|err| format!("cannot decode incident: {err}"))?);
            }
        }
        (None, Some(st)) => {
            let rows = stmt
                .query_map(params![st, limit], decode_row)
                .map_err(|err| format!("cannot read cron incidents: {err}"))?;
            for row in rows {
                rows_out.push(row.map_err(|err| format!("cannot decode incident: {err}"))?);
            }
        }
        (None, None) => {
            let rows = stmt
                .query_map(params![limit], decode_row)
                .map_err(|err| format!("cannot read cron incidents: {err}"))?;
            for row in rows {
                rows_out.push(row.map_err(|err| format!("cannot decode incident: {err}"))?);
            }
        }
    }
    Ok(rows_out)
}
