use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, Transaction, TransactionBehavior};
use serde_json::{Map, Value};

use super::{
    read_schema_version, resume::ResumeState, StorageError, CONTENT_JSON_PREFIX,
    MIN_MIGRATABLE_SCHEMA_VERSION, SCHEMA_VERSION,
};
use crate::turn::{CompletedTurn, TurnHistory};

pub(super) const SCHEMA: &str = include_str!("schema_v33.sql");
pub(super) const FTS_STORAGE_VERSION: &str = "3";
pub(super) const RANDOM_UID: &str = "SELECT lower(hex(randomblob(16)))";

pub struct SessionWriter<'a> {
    profile: &'a Path,
    started_at: f64,
    resume: Option<ResumeState>,
}

impl<'a> SessionWriter<'a> {
    pub fn prepare(profile: &'a Path) -> Result<Self, StorageError> {
        let path = profile.join("state.db");
        if database_exists(&path)? {
            let connection = open_database(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            verify_write_schema(&connection)?;
        }
        Ok(Self {
            profile,
            started_at: timestamp(),
            resume: None,
        })
    }

    pub fn prepare_resume(profile: &'a Path, id: &str) -> Result<Self, StorageError> {
        let mut writer = Self::prepare(profile)?;
        writer.resume = Some(ResumeState::acquire(&profile.join("state.db"), id)?);
        Ok(writer)
    }

    /// The session being resumed; a new session has no id until its first turn is saved.
    pub fn session_id(&self) -> Option<&str> {
        self.resume.as_ref().map(ResumeState::id)
    }

    pub fn saved_model(&self) -> Option<&str> {
        self.resume.as_ref().and_then(ResumeState::model)
    }

    pub fn take_history(&mut self) -> TurnHistory {
        self.resume
            .as_mut()
            .map_or_else(TurnHistory::default, ResumeState::take_history)
    }

    pub fn refresh(&self) -> Result<(), StorageError> {
        self.resume.as_ref().map_or(Ok(()), ResumeState::refresh)
    }

    /// Persist a manual compaction (`/compress`) on a resumed session without appending a new turn.
    pub fn save_compaction(&mut self, history: &TurnHistory) -> Result<usize, StorageError> {
        let ended_at = timestamp().max(self.started_at);
        let resume = self.resume.as_mut().ok_or(StorageError::InvalidTurn)?;
        resume.commit_compaction(history, ended_at)
    }

    pub fn save(&mut self, turn: &CompletedTurn, cwd: &Path) -> Result<String, StorageError> {
        let path = self.profile.join("state.db");
        let ended_at = timestamp().max(self.started_at);
        if let Some(resume) = self.resume.as_mut() {
            return resume.append(turn, self.started_at, ended_at);
        }
        if !database_exists(&path)? {
            fs::create_dir_all(self.profile)?;
            // Initialize privately. Publishing the SQLite file without overwriting prevents
            // racing first turns from replacing each other's profile databases.
            let temporary = tempfile::NamedTempFile::new_in(self.profile)?;
            let id = self.write_turn(temporary.path(), turn, cwd, ended_at, true)?;
            temporary.as_file().sync_all()?;
            match temporary.persist_noclobber(&path) {
                Ok(_) => {
                    #[cfg(unix)]
                    fs::File::open(self.profile)?.sync_all()?;
                    // Enable WAL mode after atomic publication so concurrent readers/writers
                    // do not hit SQLITE_BUSY, while keeping the pre-publish temp file self-contained.
                    if let Ok(conn) = open_write(&path) {
                        let _ = conn.execute_batch(
                            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
                        );
                    }
                    return Ok(id);
                }
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error.into()),
            }
        }
        self.write_turn(&path, turn, cwd, ended_at, false)
    }

    fn write_turn(
        &self,
        path: &Path,
        turn: &CompletedTurn,
        cwd: &Path,
        ended_at: f64,
        initialize: bool,
    ) -> Result<String, StorageError> {
        let mut connection = open_write(path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if initialize {
            initialize_schema(&transaction)?;
        } else {
            // Repeat the check under the write lock after the provider call.
            verify_write_schema(&transaction)?;
        }
        let id: String = transaction.query_row(
            "SELECT strftime('%Y%m%d_%H%M%S', ?1, 'unixepoch', 'localtime') || '_' || lower(hex(randomblob(8)))",
            [self.started_at],
            |row| row.get(0),
        )?;
        let profile_name = self
            .profile
            .parent()
            .filter(|parent| parent.ends_with("profiles"))
            .and_then(|_| self.profile.file_name()?.to_str())
            .unwrap_or("default");
        transaction.execute(
            "INSERT INTO sessions(id, source, created_source, model, system_prompt, cwd, profile_name,
                                  started_at, ended_at, end_reason, last_activity_at)
             VALUES (?1, 'cli', 'cli', ?2, ?3, ?4, ?5, ?6, ?7, 'agent_close', ?7)",
            params![id, turn.model, turn.system_prompt, cwd.to_str(), profile_name, self.started_at, ended_at],
        )?;
        let tool_call_count =
            insert_messages(&transaction, &id, &turn.messages, self.started_at, ended_at)?;
        transaction.execute(
            "UPDATE sessions SET message_count = ?2, tool_call_count = ?3 WHERE id = ?1",
            params![id, turn.messages.len(), tool_call_count],
        )?;
        transaction.commit()?;
        Ok(id)
    }
}

fn database_exists(path: &Path) -> Result<bool, StorageError> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(StorageError::InvalidDatabaseFile),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn open_database(path: &Path, flags: OpenFlags) -> Result<Connection, StorageError> {
    // SQLite's Unix NOFOLLOW also rejects directory aliases such as macOS /var.
    // Resolve the directory and leave the database filename for SQLite to check.
    #[cfg(unix)]
    let resolved = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?
        .join(path.file_name().ok_or(StorageError::InvalidDatabaseFile)?);
    #[cfg(unix)]
    let path = resolved.as_path();
    let connection = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    connection.busy_timeout(super::BUSY_TIMEOUT)?;
    Ok(connection)
}

pub(super) fn open_write(path: &Path) -> Result<Connection, StorageError> {
    let connection = open_database(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

pub(super) fn verify_write_schema(connection: &Connection) -> Result<(), StorageError> {
    match read_schema_version(connection)? {
        SCHEMA_VERSION => {}
        version if (MIN_MIGRATABLE_SCHEMA_VERSION..SCHEMA_VERSION).contains(&version) => {
            return Err(StorageError::ReadOnlySchema(version));
        }
        version => return Err(StorageError::UnsupportedSchema(version)),
    }
    verify_fts_layout(connection)
}

pub(super) fn verify_fts_layout(connection: &Connection) -> Result<(), StorageError> {
    let ready = connection.query_row(
        "SELECT COALESCE((SELECT value = ?1 FROM state_meta WHERE key = 'fts_storage_version'), 0)
          AND NOT EXISTS (SELECT 1 FROM state_meta WHERE key IN ('fts_stale', 'fts_rebuild_high_water', 'fts_rebuild_progress'))
          AND EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'messages_fts'
                      AND instr(sql, 'messages_fts_src') > 0)
          AND EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'messages_fts_trigram'
                      AND instr(sql, 'messages_fts_trigram_src') > 0)
          AND (SELECT count(*) FROM sqlite_schema WHERE type = 'trigger' AND tbl_name = 'messages'
               AND name IN ('messages_fts_insert', 'messages_fts_update', 'messages_fts_delete',
                            'messages_fts_trigram_insert', 'messages_fts_trigram_update',
                            'messages_fts_trigram_delete')) = 6",
        [FTS_STORAGE_VERSION],
        |row| row.get::<_, bool>(0),
    )?;
    if !ready {
        return Err(StorageError::UnsupportedFtsLayout);
    }
    Ok(())
}

fn initialize_schema(connection: &Transaction<'_>) -> rusqlite::Result<()> {
    connection.execute_batch(SCHEMA)?;
    connection.execute(
        "INSERT INTO schema_version(version) VALUES (?1)",
        [SCHEMA_VERSION],
    )?;
    connection.execute(
        "INSERT INTO state_meta(key, value) VALUES
            ('fts_storage_version', ?1),
            ('store_instance_id', lower(hex(randomblob(16)))),
            ('store_created_at_utc', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
            ('message_uid_backfill', '1'),
            ('legacy_refusal_repair', '1')",
        [FTS_STORAGE_VERSION],
    )?;
    Ok(())
}

pub(super) fn insert_messages(
    transaction: &Transaction<'_>,
    id: &str,
    messages: &[Value],
    started_at: f64,
    ended_at: f64,
) -> Result<usize, StorageError> {
    let mut statement = transaction.prepare(
        "INSERT INTO messages(session_id, role, content, timestamp, tool_calls, tool_call_id,
                              tool_name, finish_reason, active, tool_call_uids, tool_call_uid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10)",
    )?;
    let mut mint_uid = transaction.prepare(RANDOM_UID)?;
    let mut calls_by_id = HashMap::new();
    let mut tool_call_count = 0;
    let mut timestamp = started_at;
    for message in messages {
        let role = message["role"].as_str().ok_or(StorageError::InvalidTurn)?;
        let content = match &message["content"] {
            Value::Null => None,
            Value::String(text) if !text.starts_with(CONTENT_JSON_PREFIX) => {
                Some(Cow::Borrowed(text.as_str()))
            }
            value => Some(Cow::Owned(format!("{CONTENT_JSON_PREFIX}{value}"))),
        };
        let calls = message["tool_calls"].as_array();
        let mut uids = Map::new();
        if let Some(calls) = calls {
            tool_call_count += calls.len();
            for call in calls {
                let call_id = call["id"].as_str().ok_or(StorageError::InvalidTurn)?;
                let name = call["function"]["name"]
                    .as_str()
                    .ok_or(StorageError::InvalidTurn)?;
                if !uids.contains_key(call_id) {
                    let uid: String = mint_uid.query_row([], |row| row.get(0))?;
                    uids.insert(call_id.into(), Value::from(uid.clone()));
                    calls_by_id.insert(call_id, (name, uid));
                }
            }
        }
        let call_id = message["tool_call_id"].as_str();
        let tool = call_id.and_then(|id| calls_by_id.get(id));
        let finish_reason = (role == "assistant").then_some(if calls.is_some() {
            "tool_calls"
        } else {
            "stop"
        });
        let call_uids = (!uids.is_empty()).then(|| Value::Object(uids).to_string());
        statement.execute(params![
            id,
            role,
            content.as_deref(),
            timestamp,
            calls.map(|_| message["tool_calls"].to_string()),
            call_id,
            tool.map(|(name, _)| name),
            finish_reason,
            call_uids,
            tool.map(|(_, uid)| uid),
        ])?;
        timestamp = (timestamp + 1e-6).min(ended_at);
    }
    Ok(tool_call_count)
}

pub(super) fn timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}
