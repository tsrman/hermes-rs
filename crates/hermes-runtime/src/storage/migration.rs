use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use ring::digest::{self, Context, Digest};
use rusqlite::{types::ValueRef, Connection, DatabaseName, OpenFlags, TransactionBehavior};
use serde::Serialize;

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

    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    write_verified_snapshot(profile, temporary.path(), true)?;
    ensure_destination_is_clear(destination)?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct SnapshotReport {
    source_schema: i64,
    target_schema: i64,
    sessions: u64,
    messages: u64,
    fts_integrity: bool,
    history_verified: bool,
}

/// The caller provides a private scratch file and publishes it only on success.
pub(crate) fn write_verified_snapshot(
    profile: &Path,
    destination: &Path,
    legacy_only: bool,
) -> Result<SnapshotReport, StorageError> {
    let mut source = if legacy_only {
        open_read_only(profile)?
    } else {
        open_profile_database(profile)?
    };
    let snapshot = source.transaction()?;
    let version = read_schema_version(&snapshot)?;
    if legacy_only && version != LEGACY_SCHEMA_VERSION {
        return Err(StorageError::MigrationSourceVersion(version));
    }
    if !matches!(version, LEGACY_SCHEMA_VERSION | SCHEMA_VERSION) {
        return Err(StorageError::UnsupportedSchema(version));
    }
    verify_fts_layout(&snapshot)?;
    let before = history(&snapshot, version)?;
    snapshot.backup(DatabaseName::Main, destination, None)?;
    snapshot.commit()?;
    verify_snapshot(destination, version, &before)?;
    fs::File::open(destination)?.sync_all()?;
    Ok(SnapshotReport {
        source_schema: version,
        target_schema: SCHEMA_VERSION,
        sessions: before.sessions.0,
        messages: before.messages.0,
        fts_integrity: true,
        history_verified: true,
    })
}

fn open_profile_database(profile: &Path) -> Result<Connection, StorageError> {
    for name in ["state.db-journal", "state.db-wal", "state.db-shm"] {
        match profile.join(name).symlink_metadata() {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(StorageError::MigrationIntegrity(format!(
                    "{name} must be a regular file"
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Connection::open_with_flags(
        profile.join("state.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?)
}

struct History {
    sessions: (u64, Digest),
    messages: (u64, Digest),
    prompts: (u64, Digest),
}

fn history(connection: &Connection, source_version: i64) -> rusqlite::Result<History> {
    // The v32 upgrade adds prompt_cache_clock and can update delegate metadata.
    let ignored: &[&str] = if source_version == LEGACY_SCHEMA_VERSION {
        &["model_config", "prompt_cache_clock"]
    } else {
        &[]
    };
    Ok(History {
        sessions: fingerprint_rows(connection, "SELECT * FROM sessions ORDER BY id", ignored)?,
        messages: fingerprint_rows(connection, "SELECT * FROM messages ORDER BY id", &[])?,
        prompts: fingerprint_rows(
            connection,
            "SELECT * FROM system_prompts ORDER BY hash",
            &[],
        )?,
    })
}

fn fingerprint_rows(
    connection: &Connection,
    query: &str,
    ignored: &[&str],
) -> rusqlite::Result<(u64, Digest)> {
    let mut statement = connection.prepare(query)?;
    let columns: Vec<_> = statement
        .column_names()
        .into_iter()
        .enumerate()
        .filter_map(|(index, name)| (!ignored.contains(&name)).then_some(index))
        .collect();
    let mut rows = statement.query([])?;
    let mut digest = Context::new(&digest::SHA256);
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        for &index in &columns {
            match row.get_ref(index)? {
                ValueRef::Null => digest.update(&[0]),
                ValueRef::Integer(value) => {
                    digest.update(&[1]);
                    digest.update(&value.to_le_bytes());
                }
                ValueRef::Real(value) => {
                    digest.update(&[2]);
                    digest.update(&value.to_bits().to_le_bytes());
                }
                ValueRef::Text(bytes) => hash_bytes(&mut digest, 3, bytes),
                ValueRef::Blob(bytes) => hash_bytes(&mut digest, 4, bytes),
            }
        }
    }
    Ok((count, digest.finish()))
}

fn hash_bytes(digest: &mut Context, kind: u8, bytes: &[u8]) {
    digest.update(&[kind]);
    digest.update(&(bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
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

fn verify_snapshot(path: &Path, source_version: i64, before: &History) -> Result<(), StorageError> {
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
    if read_schema_version(&transaction)? != source_version {
        return Err(StorageError::MigrationIntegrity(
            "snapshot schema changed".into(),
        ));
    }
    if source_version == LEGACY_SCHEMA_VERSION {
        upgrade_legacy(&transaction)?;
    }
    check_integrity(&transaction)?;
    let after = history(&transaction, source_version)?;
    if [
        (before.sessions, after.sessions),
        (before.messages, after.messages),
        (before.prompts, after.prompts),
    ]
    .iter()
    .any(|(before, after)| before.0 != after.0 || before.1.as_ref() != after.1.as_ref())
    {
        return Err(StorageError::MigrationIntegrity(
            "snapshot history changed".into(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

fn upgrade_legacy(transaction: &Connection) -> Result<(), StorageError> {
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
