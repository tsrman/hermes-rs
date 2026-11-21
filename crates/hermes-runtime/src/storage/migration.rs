use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use rusqlite::{Connection, DatabaseName, TransactionBehavior};

use super::{
    open_read_only, read_schema_version,
    write::{open_write, verify_fts_layout},
    StorageError, LEGACY_SCHEMA_VERSION, SCHEMA_VERSION,
};

const SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

/// Make and validate a migrated snapshot before publishing it as a new file.
pub fn migrate_session_copy(profile: &Path, destination: &Path) -> Result<(), StorageError> {
    let name = destination
        .file_name()
        .ok_or(StorageError::InvalidMigrationDestination)?;
    if SIDECAR_SUFFIXES
        .iter()
        .any(|suffix| name.as_encoded_bytes().ends_with(suffix.as_bytes()))
    {
        return Err(StorageError::InvalidMigrationDestination);
    }
    ensure_destination_is_clear(destination)?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let temporary = {
        let mut source = open_read_only(profile)?;
        let snapshot = source.transaction()?;
        let version = read_schema_version(&snapshot)?;
        if version != LEGACY_SCHEMA_VERSION {
            return Err(StorageError::MigrationSourceVersion(version));
        }
        verify_fts_layout(&snapshot)?;
        let temporary = tempfile::NamedTempFile::new_in(parent)?;
        snapshot.backup(DatabaseName::Main, temporary.path(), None)?;
        snapshot.commit()?;
        temporary
    };

    migrate_snapshot(temporary.path())?;
    temporary.as_file().sync_all()?;
    ensure_destination_is_clear(destination)?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn ensure_destination_is_clear(destination: &Path) -> Result<(), StorageError> {
    for suffix in std::iter::once("").chain(SIDECAR_SUFFIXES) {
        let mut path = destination.as_os_str().to_owned();
        path.push(suffix);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(StorageError::MigrationDestinationExists(path.into())),
        }
    }
    Ok(())
}

fn migrate_snapshot(path: &Path) -> Result<(), StorageError> {
    let mut connection = open_write(path)?;
    // Publish a self-contained database file with no WAL to move or replay.
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    if mode != "delete" {
        return Err(StorageError::MigrationIntegrity(format!(
            "could not switch journal mode from {mode} to delete"
        )));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_cache_clock: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'prompt_cache_clock')",
        [],
        |row| row.get(0),
    )?;
    if !has_cache_clock {
        transaction.execute(
            "ALTER TABLE sessions ADD COLUMN prompt_cache_clock TEXT",
            [],
        )?;
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS resumable_subagents (
            subagent_id TEXT PRIMARY KEY,
            state TEXT NOT NULL,
            pinned INTEGER NOT NULL DEFAULT 0,
            stop_cause TEXT,
            owner_pid INTEGER,
            owner_started_at INTEGER,
            record_json TEXT NOT NULL,
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL,
            expires_at REAL
        );
        -- v33 restores missing delegate markers while keeping a subagent's own compression chain.
        UPDATE sessions
        SET model_config = json_set(COALESCE(model_config, '{}'), '$._delegate_from', parent_session_id)
        WHERE source = 'subagent' AND parent_session_id IS NOT NULL
          AND json_valid(COALESCE(model_config, '{}'))
          AND json_extract(CASE WHEN json_valid(model_config) THEN model_config ELSE '{}' END,
                           '$._delegate_from') IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM sessions AS parent WHERE parent.id = sessions.parent_session_id
              AND parent.source = 'subagent' AND parent.end_reason = 'compression'
          );",
    )?;
    transaction.execute("UPDATE schema_version SET version = ?1", [SCHEMA_VERSION])?;
    check_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn check_integrity(connection: &Connection) -> Result<(), StorageError> {
    for index in ["messages_fts", "messages_fts_trigram"] {
        // Rank 1 also compares an external-content index to the rows in its backing view.
        connection
            .execute(
                &format!("INSERT INTO {index}({index}, rank) VALUES ('integrity-check', 1)"),
                [],
            )
            .map_err(|error| StorageError::MigrationIntegrity(format!("{index}: {error}")))?;
    }
    let integrity: String =
        connection.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StorageError::MigrationIntegrity(integrity));
    }
    if connection.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(StorageError::MigrationIntegrity(
            "foreign key violations".into(),
        ));
    }
    Ok(())
}
