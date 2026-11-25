//! Profile-scoped access to the Hermes SQLite session database.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, types::ValueRef, Connection, OpenFlags, OptionalExtension, Row};
use serde::Serialize;
use serde_json::Value;

mod compression;
mod migration;
pub mod recall;
mod resume;
mod write;
pub use migration::migrate_session_copy;
#[cfg(target_os = "linux")]
pub(crate) use migration::{write_verified_snapshot, SnapshotReport};
pub use write::SessionWriter;

/// Ensure `<profile>/state.db` exists with schema v33 and FTS layout 3 without clobbering.
pub fn ensure_initialized(profile: &Path) -> Result<bool, StorageError> {
    SessionWriter::ensure_initialized(profile)
}

/// Roll back the last conversational exchange (last user message and following assistant/tool rows).
pub fn undo_last_turn(profile: &Path, session_id: &str) -> Result<usize, StorageError> {
    SessionWriter::undo_last_turn(profile, session_id)
}

/// Permanently delete a saved session and all of its messages from `<profile>/state.db`.
///
/// Returns `Ok(true)` if the session existed and was removed, or `Ok(false)` if not found.
pub fn delete_session(profile: &Path, session_id: &str) -> Result<bool, StorageError> {
    SessionWriter::delete_session(profile, session_id)
}

pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const SCHEMA_VERSION: i64 = 33;
const LEGACY_SCHEMA_VERSION: i64 = 32;
const MIN_MIGRATABLE_SCHEMA_VERSION: i64 = 1;
const CONTENT_JSON_PREFIX: &str = "\0json:";

#[derive(Debug)]
pub enum StorageError {
    Database(rusqlite::Error),
    Io(std::io::Error),
    MissingSchemaVersion,
    UnsupportedSchema(i64),
    ReadOnlySchema(i64),
    UnsupportedFtsLayout,
    MigrationSourceVersion(i64),
    MigrationDestinationExists(PathBuf),
    InvalidMigrationDestination,
    MigrationIntegrity(String),
    InvalidDatabaseFile,
    InvalidTurn,
    SessionNotFound(String),
    InvalidCompressionChain,
    SessionBusy(String),
    SessionChanged(String),
    InvalidReplay(String),
    NoCompressionSuccessor(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(f, "state.db: {error}"),
            Self::Io(error) => write!(f, "state.db: {error}"),
            Self::MissingSchemaVersion => write!(f, "state.db lacks a single schema version"),
            Self::UnsupportedSchema(version) => write!(f, "unsupported state.db schema version {version}"),
            Self::ReadOnlySchema(version) => write!(
                f,
                "state.db schema version {version} is read-only in Rust (writing requires version {SCHEMA_VERSION}; run `hsr sessions migrate` or `hsr profile migrate`)"
            ),
            Self::UnsupportedFtsLayout => {
                write!(f, "writing state.db requires a complete FTS layout 3")
            }
            Self::MigrationSourceVersion(version) => write!(
                f,
                "migration requires state.db schema version {MIN_MIGRATABLE_SCHEMA_VERSION}..={LEGACY_SCHEMA_VERSION} (or legacy FTS layout), found {version}"
            ),
            Self::MigrationDestinationExists(path) => {
                write!(f, "migration output already exists at {}", path.display())
            }
            Self::InvalidMigrationDestination => {
                write!(f, "migration output must be a database file, distinct from SQLite sidecars")
            }
            Self::MigrationIntegrity(detail) => write!(f, "migration integrity check failed: {detail}"),
            Self::InvalidDatabaseFile => write!(f, "state.db must be a regular file for writing"),
            Self::InvalidTurn => write!(f, "the completed turn includes an invalid message"),
            Self::SessionNotFound(id) => write!(f, "session {id:?} was not found"),
            Self::InvalidCompressionChain => {
                write!(f, "state.db compression chain is cyclic or too long")
            }
            Self::SessionBusy(id) => {
                write!(f, "session {id:?} already has an active turn or operation")
            }
            Self::SessionChanged(id) => write!(
                f,
                "session {id:?} transcript changed or its turn lease expired"
            ),
            Self::InvalidReplay(id) => write!(
                f,
                "session {id:?} contains an incompatible message or tool exchange"
            ),
            Self::NoCompressionSuccessor(id) => {
                write!(f, "session {id:?} has no live compression successor")
            }
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub source: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub parent_session_id: Option<String>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub end_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Message {
    pub id: i64,
    pub role: String,
    pub content: Value,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<Value>,
    pub tool_name: Option<String>,
    pub timestamp: f64,
    pub finish_reason: Option<String>,
    pub active: bool,
    pub compacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_kind: Option<String>,
    pub display_metadata: Option<Value>,
    #[serde(rename = "_compressed_summary")]
    pub compressed_summary: bool,
}

#[derive(Debug, Serialize)]
pub struct Transcript {
    pub session: Session,
    pub messages: Vec<Message>,
}

#[derive(Debug, Default)]
pub struct TranscriptOptions {
    pub include_inactive: bool,
    pub follow_compression: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub snippet: Option<String>,
}

/// One row of the session list.
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub source: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub started_at: f64,
    pub last_activity_at: Option<f64>,
    pub message_count: i64,
    /// The first thing the user asked, shortened.
    pub preview: Option<String>,
}

pub struct SessionStore {
    connection: Connection,
}

impl SessionStore {
    pub fn open_read_only(profile: &Path) -> Result<Self, StorageError> {
        let connection = open_read_only(profile)?;
        match read_schema_version(&connection)? {
            LEGACY_SCHEMA_VERSION | SCHEMA_VERSION => Ok(Self { connection }),
            version => Err(StorageError::UnsupportedSchema(version)),
        }
    }

    pub fn transcript(
        &self,
        id: &str,
        options: TranscriptOptions,
    ) -> Result<Transcript, StorageError> {
        // Use one read snapshot so the row and transcript agree during concurrent writes.
        let transaction = self.connection.unchecked_transaction()?;
        let id = if options.follow_compression {
            Cow::Owned(compression::tip(&transaction, id)?)
        } else {
            Cow::Borrowed(id)
        };
        let session = transaction
            .query_row(
                "SELECT id, source, model, title, parent_session_id, started_at, ended_at, end_reason
                 FROM sessions WHERE id = ?1",
                [id.as_ref()],
                |row| {
                    Ok(Session {
                        id: row.get(0)?,
                        source: row.get(1)?,
                        model: row.get(2)?,
                        title: row.get(3)?,
                        parent_session_id: row.get(4)?,
                        started_at: row.get(5)?,
                        ended_at: row.get(6)?,
                        end_reason: row.get(7)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StorageError::SessionNotFound(id.to_string()))?;
        let has_display_kind = has_display_kind_column(&transaction)?;
        let messages = {
            let sql = if has_display_kind {
                "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp,
                        finish_reason, active, compacted, display_metadata, _compressed_summary,
                        display_kind
                 FROM messages WHERE session_id = ?1 AND (?2 OR active = 1) ORDER BY id"
            } else {
                "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp,
                        finish_reason, active, compacted, display_metadata, _compressed_summary,
                        NULL
                 FROM messages WHERE session_id = ?1 AND (?2 OR active = 1) ORDER BY id"
            };
            let mut statement = transaction.prepare(sql)?;
            let rows = statement
                .query_map(params![id.as_ref(), options.include_inactive], read_message)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        transaction.commit()?;
        Ok(Transcript { session, messages })
    }

    /// Resolve a session reference (exact ID, case-insensitive title, or unique ID prefix) and
    /// follow its compression chain to the live tip session ID.
    pub fn resolve_resume_target(&self, query: &str) -> Result<String, StorageError> {
        let mut target = query.trim();
        if target.len() >= 2 {
            let bytes = target.as_bytes();
            let paired = matches!(
                (bytes[0], bytes[bytes.len() - 1]),
                (b'<', b'>') | (b'[', b']') | (b'"', b'"') | (b'\'', b'\'')
            );
            if paired {
                target = target[1..target.len() - 1].trim();
            }
        }
        if target.is_empty() {
            return Err(StorageError::SessionNotFound(query.to_owned()));
        }
        let transaction = self.connection.unchecked_transaction()?;
        let matched = transaction
            .query_row("SELECT id FROM sessions WHERE id = ?1", [target], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .or_else(|| {
                transaction
                    .query_row(
                        "SELECT id FROM sessions WHERE lower(title) = lower(?1)
                         ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT 1",
                        [target],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
            })
            .or_else(|| {
                let mut stmt = transaction
                    .prepare(
                        "SELECT id FROM sessions WHERE id LIKE ?1 || '%'
                         ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT 2",
                    )
                    .ok()?;
                let mut rows = stmt
                    .query_map([target], |row| row.get::<_, String>(0))
                    .ok()?
                    .filter_map(Result::ok);
                let first = rows.next()?;
                if rows.next().is_some() {
                    None
                } else {
                    Some(first)
                }
            })
            .ok_or_else(|| StorageError::SessionNotFound(target.to_owned()))?;
        let tip = compression::tip(&transaction, &matched)?;
        transaction.commit()?;
        Ok(tip)
    }

    /// Load the displayable transcript for a resumed session (following compression lineage from
    /// root to tip, including compacted history while omitting hidden and queued-prompt rows).
    pub fn resume_display_transcript(&self, id: &str) -> Result<Transcript, StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        let tip_id = compression::tip(&transaction, id)?;
        let root_id = compression::root(&transaction, &tip_id)?;
        let session = transaction
            .query_row(
                "SELECT id, source, model, title, parent_session_id, started_at, ended_at, end_reason
                 FROM sessions WHERE id = ?1",
                [tip_id.as_str()],
                |row| {
                    Ok(Session {
                        id: row.get(0)?,
                        source: row.get(1)?,
                        model: row.get(2)?,
                        title: row.get(3)?,
                        parent_session_id: row.get(4)?,
                        started_at: row.get(5)?,
                        ended_at: row.get(6)?,
                        end_reason: row.get(7)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StorageError::SessionNotFound(tip_id.clone()))?;

        // Collect lineage segments root -> tip without cycles.
        let mut lineage = Vec::new();
        let mut cursor = root_id;
        let mut seen = std::collections::HashSet::new();
        while seen.insert(cursor.clone()) {
            lineage.push(cursor.clone());
            if cursor == tip_id {
                break;
            }
            let next = compression::tip(&transaction, &cursor)?;
            if next == cursor {
                break;
            }
            // Walk one step at a time using parent->child chain; fallback directly to tip.
            let step: Option<String> = transaction
                .query_row(
                    "SELECT id FROM sessions WHERE parent_session_id = ?1 ORDER BY started_at ASC LIMIT 1",
                    [cursor.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            cursor = step.unwrap_or_else(|| tip_id.clone());
        }
        if lineage.last().map(String::as_str) != Some(tip_id.as_str()) {
            lineage.push(tip_id.clone());
        }

        let has_display_kind = has_display_kind_column(&transaction)?;
        let mut messages = Vec::new();
        let sql = if has_display_kind {
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp,
                    finish_reason, active, compacted, display_metadata, _compressed_summary,
                    display_kind
             FROM messages
             WHERE session_id = ?1
               AND (active = 1 OR compacted = 1)
               AND COALESCE(display_kind, '') != 'hidden'
               AND COALESCE(json_extract(CASE WHEN json_valid(display_metadata) THEN display_metadata ELSE '{}' END,
                                         '$._queued_prompt'), 0) != 1
             ORDER BY id"
        } else {
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp,
                    finish_reason, active, compacted, display_metadata, _compressed_summary,
                    NULL
             FROM messages
             WHERE session_id = ?1
               AND (active = 1 OR compacted = 1)
               AND COALESCE(json_extract(CASE WHEN json_valid(display_metadata) THEN display_metadata ELSE '{}' END,
                                         '$._queued_prompt'), 0) != 1
             ORDER BY id"
        };
        let mut statement = transaction.prepare(sql)?;
        for seg_id in &lineage {
            let rows = statement.query_map([seg_id.as_str()], read_message)?;
            for msg in rows {
                messages.push(msg?);
            }
        }
        drop(statement);
        transaction.commit()?;
        Ok(Transcript { session, messages })
    }

    /// The most recently active sessions that have messages, newest first. `source` limits the
    /// list to one origin such as `cli`.
    pub fn recent(
        &self,
        limit: u32,
        source: Option<&str>,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.source, s.model, s.title, s.started_at, s.last_activity_at,
                    COALESCE(s.message_count, 0),
                    (SELECT substr(m.content, 1, 80) FROM messages m
                     WHERE m.session_id = s.id AND m.role = 'user' AND m.active = 1
                     ORDER BY m.id LIMIT 1)
             FROM sessions s
             WHERE COALESCE(s.message_count, 0) > 0 AND (?1 IS NULL OR s.source = ?1)
             ORDER BY COALESCE(s.last_activity_at, s.started_at) DESC, s.started_at DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![source, limit], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                source: row.get(1)?,
                model: row.get(2)?,
                title: row.get(3)?,
                started_at: row.get(4)?,
                last_activity_at: row.get(5)?,
                message_count: row.get(6)?,
                preview: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>, StorageError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Both the inline and external-content layouts use the stored message id as the FTS rowid.
        let mut statement = self.connection.prepare(
            "SELECT m.id, m.session_id, m.role, snippet(messages_fts, 0, '', '', ' … ', 24)
             FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND (m.active = 1 OR m.compacted = 1) AND m.role != 'tool'
             ORDER BY messages_fts.rank, m.id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], |row| {
            Ok(SearchHit {
                id: row.get(0)?,
                session_id: row.get(1)?,
                role: row.get(2)?,
                snippet: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn open_read_only(profile: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        profile.join("state.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}

fn has_display_kind_column(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name = 'display_kind')",
        [],
        |row| row.get(0),
    )
}

fn read_schema_version(connection: &Connection) -> Result<i64, StorageError> {
    connection
        .query_row(
            "SELECT CASE WHEN COUNT(*) = 1 THEN MAX(version) END FROM schema_version",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .ok_or(StorageError::MissingSchemaVersion)
}

fn read_message(row: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        role: row.get(1)?,
        content: read_content(row, 2)?,
        tool_call_id: row.get(3)?,
        tool_calls: row
            .get::<_, Option<String>>(4)?
            .map(|text| serde_json::from_str(&text).unwrap_or_else(|_| Value::Array(Vec::new()))),
        tool_name: row.get(5)?,
        timestamp: row.get(6)?,
        finish_reason: row.get(7)?,
        active: row.get(8)?,
        compacted: row.get(9)?,
        display_metadata: row.get::<_, Option<String>>(10)?.and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .ok()
                .filter(Value::is_object)
        }),
        compressed_summary: row.get(11)?,
        display_kind: row.get(12)?,
    })
}

fn read_content(row: &Row<'_>, column: usize) -> rusqlite::Result<Value> {
    let value = row.get_ref(column)?;
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => value.into(),
        ValueRef::Real(value) => value.into(),
        ValueRef::Blob(value) => value.to_vec().into(),
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    value.data_type(),
                    Box::new(error),
                )
            })?;
            text.strip_prefix(CONTENT_JSON_PREFIX)
                .and_then(|encoded| serde_json::from_str(encoded).ok())
                .unwrap_or_else(|| Value::String(text.into()))
        }
    })
}
