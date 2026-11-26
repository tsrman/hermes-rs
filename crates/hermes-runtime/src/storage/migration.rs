use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use ring::digest::{self, Context, Digest};
use rusqlite::{params, types::ValueRef, Connection, OpenFlags, TransactionBehavior, MAIN_DB};
use serde::Serialize;

use super::{
    open_read_only, read_schema_version,
    write::{open_write, verify_fts_layout, FTS_STORAGE_VERSION, SCHEMA},
    StorageError, LEGACY_SCHEMA_VERSION, MIN_MIGRATABLE_SCHEMA_VERSION, SCHEMA_VERSION,
};

const SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];
const FTS_TRIGGER_NAMES: [&str; 6] = [
    "messages_fts_insert",
    "messages_fts_update",
    "messages_fts_delete",
    "messages_fts_trigram_insert",
    "messages_fts_trigram_update",
    "messages_fts_trigram_delete",
];

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
    if !(MIN_MIGRATABLE_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&version) {
        return Err(StorageError::UnsupportedSchema(version));
    }
    let fts_already_v3 = verify_fts_layout(&snapshot).is_ok();
    if legacy_only && version == SCHEMA_VERSION && fts_already_v3 {
        return Err(StorageError::MigrationSourceVersion(version));
    }
    // Preserve strict corruption detection for v32/v33 databases that already claim FTS layout 3,
    // while upgrading earlier schemas (v1..v31) or legacy FTS layouts (v0/v1/v2) cleanly to v3.
    let rebuild_fts = version < LEGACY_SCHEMA_VERSION || !fts_already_v3;
    let plan = HistoryPlan::capture(&snapshot, version)?;
    let before = history(&snapshot, &plan)?;
    snapshot.backup(MAIN_DB, destination, None::<fn(rusqlite::backup::Progress)>)?;
    snapshot.commit()?;
    verify_snapshot(destination, version, rebuild_fts, &plan, &before)?;
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

struct HistoryPlan {
    session_columns: Vec<String>,
    message_columns: Vec<String>,
    check_effective_prompts: bool,
    has_system_prompts_table: bool,
    source_version: i64,
}

impl HistoryPlan {
    fn capture(connection: &Connection, source_version: i64) -> rusqlite::Result<Self> {
        let session_ignored: &[&str] = if source_version < 25 {
            // Earlier schemas may deduplicate system_prompt into system_prompts, tag
            // model_config._delegate_from, add prompt_cache_clock, or deduplicate duplicate titles.
            &[
                "model_config",
                "prompt_cache_clock",
                "system_prompt",
                "system_prompt_hash",
                "title",
            ]
        } else if source_version < SCHEMA_VERSION {
            &["model_config", "prompt_cache_clock", "title"]
        } else {
            &[]
        };
        let message_ignored: &[&str] = if source_version < LEGACY_SCHEMA_VERSION {
            // Backfilled columns that early schemas left NULL or omitted.
            &[
                "active",
                "compacted",
                "_compressed_summary",
                "display_order",
                "message_uid",
            ]
        } else {
            &[]
        };
        let session_columns = table_column_names(connection, "sessions")?
            .into_iter()
            .filter(|name| !session_ignored.contains(&name.as_str()))
            .collect();
        let message_columns = table_column_names(connection, "messages")?
            .into_iter()
            .filter(|name| !message_ignored.contains(&name.as_str()))
            .collect();
        let has_system_prompt_col = table_column_names(connection, "sessions")?
            .iter()
            .any(|c| c == "system_prompt");
        let has_system_prompts_table = table_exists(connection, "system_prompts")?;
        Ok(Self {
            session_columns,
            message_columns,
            check_effective_prompts: has_system_prompt_col,
            has_system_prompts_table,
            source_version,
        })
    }
}

struct History {
    sessions: (u64, Digest),
    messages: (u64, Digest),
    prompts: (u64, Digest),
    effective_prompts: Option<(u64, Digest)>,
}

fn history(connection: &Connection, plan: &HistoryPlan) -> rusqlite::Result<History> {
    let sessions_sql = if plan.session_columns.is_empty() {
        "SELECT id FROM sessions ORDER BY id".to_owned()
    } else {
        let cols = plan
            .session_columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT {cols} FROM sessions ORDER BY id")
    };
    let messages_sql = if plan.message_columns.is_empty() {
        "SELECT id FROM messages ORDER BY id".to_owned()
    } else {
        let cols = plan
            .message_columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT {cols} FROM messages ORDER BY id")
    };
    let prompts = if plan.has_system_prompts_table && plan.source_version >= 25 {
        fingerprint_rows(connection, "SELECT * FROM system_prompts ORDER BY hash")?
    } else {
        (0, digest::digest(&digest::SHA256, &[]))
    };
    let effective_prompts = if plan.check_effective_prompts {
        let has_sp_table = table_exists(connection, "system_prompts")?;
        let has_sp_hash = table_column_names(connection, "sessions")?
            .iter()
            .any(|c| c == "system_prompt_hash");
        let query = if has_sp_table && has_sp_hash {
            "SELECT s.id, COALESCE(s.system_prompt, sp.prompt)
             FROM sessions AS s
             LEFT JOIN system_prompts AS sp ON sp.hash = s.system_prompt_hash
             ORDER BY s.id"
        } else {
            "SELECT id, system_prompt FROM sessions ORDER BY id"
        };
        Some(fingerprint_rows(connection, query)?)
    } else {
        None
    };
    Ok(History {
        sessions: fingerprint_rows(connection, &sessions_sql)?,
        messages: fingerprint_rows(connection, &messages_sql)?,
        prompts,
        effective_prompts,
    })
}

fn fingerprint_rows(connection: &Connection, query: &str) -> rusqlite::Result<(u64, Digest)> {
    let mut statement = connection.prepare(query)?;
    let column_count = statement.column_count();
    let mut rows = statement.query([])?;
    let mut digest = Context::new(&digest::SHA256);
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        for index in 0..column_count {
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

fn verify_snapshot(
    path: &Path,
    source_version: i64,
    rebuild_fts: bool,
    plan: &HistoryPlan,
    before: &History,
) -> Result<(), StorageError> {
    let mut connection = open_write(path)?;
    // Publish a self-contained database file with no WAL to move or replay.
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    if mode != "delete" {
        return Err(StorageError::MigrationIntegrity(format!(
            "could not switch journal mode from {mode} to delete"
        )));
    }
    let needs_upgrade = source_version < SCHEMA_VERSION || rebuild_fts;
    if needs_upgrade {
        // Disable FK enforcement during table rebuilds (matching upstream `_schema.py`);
        // re-enabled and verified via `PRAGMA foreign_key_check` before publishing.
        connection.pragma_update(None, "foreign_keys", false)?;
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if read_schema_version(&transaction)? != source_version {
        return Err(StorageError::MigrationIntegrity(
            "snapshot schema changed".into(),
        ));
    }
    if needs_upgrade {
        upgrade_to_v33(&transaction, source_version, rebuild_fts)?;
    }
    verify_fts_layout(&transaction)?;
    check_integrity(&transaction)?;
    let after = history(&transaction, plan)?;
    let mut changed = [
        (before.sessions, after.sessions),
        (before.messages, after.messages),
        (before.prompts, after.prompts),
    ]
    .iter()
    .any(|(b, a)| b.0 != a.0 || b.1.as_ref() != a.1.as_ref());
    if let (Some(b), Some(a)) = (before.effective_prompts, after.effective_prompts) {
        changed |= b.0 != a.0 || b.1.as_ref() != a.1.as_ref();
    }
    if changed {
        return Err(StorageError::MigrationIntegrity(
            "snapshot history changed".into(),
        ));
    }
    transaction.commit()?;
    if needs_upgrade {
        connection.pragma_update(None, "foreign_keys", true)?;
        if connection.prepare("PRAGMA foreign_key_check")?.exists([])? {
            return Err(StorageError::MigrationIntegrity(
                "foreign key violations".into(),
            ));
        }
    }
    Ok(())
}

struct ColumnSpec {
    name: String,
    col_type: String,
    notnull: bool,
    dflt_value: Option<String>,
    pk: i64,
}

struct ReferenceSchema {
    tables: Vec<(String, String, Vec<ColumnSpec>)>,
    indexes: Vec<(String, String)>,
    views: Vec<(String, String)>,
    virtual_tables: Vec<(String, String)>,
    triggers: Vec<(String, String)>,
}

impl ReferenceSchema {
    fn load() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;

        let mut tables = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT name, sql FROM sqlite_schema
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                   AND sql IS NOT NULL AND sql NOT LIKE 'CREATE VIRTUAL TABLE%'
                 ORDER BY rowid",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (name, sql) = row?;
                // Skip FTS5 shadow tables generated by CREATE VIRTUAL TABLE.
                if name.starts_with("messages_fts_") {
                    continue;
                }
                let cols = table_column_specs(&conn, &name)?;
                tables.push((name, sql, cols));
            }
        }

        let query_pairs = |sql: &str| -> rusqlite::Result<Vec<(String, String)>> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect()
        };

        let indexes = query_pairs(
            "SELECT name, sql FROM sqlite_schema WHERE type = 'index' AND sql IS NOT NULL ORDER BY rowid",
        )?;
        let views = query_pairs(
            "SELECT name, sql FROM sqlite_schema WHERE type = 'view' AND sql IS NOT NULL ORDER BY rowid",
        )?;
        let virtual_tables = query_pairs(
            "SELECT name, sql FROM sqlite_schema
             WHERE type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'
             ORDER BY rowid",
        )?;
        let triggers = query_pairs(
            "SELECT name, sql FROM sqlite_schema WHERE type = 'trigger' AND sql IS NOT NULL ORDER BY rowid",
        )?;

        Ok(Self {
            tables,
            indexes,
            views,
            virtual_tables,
            triggers,
        })
    }
}

/// Upgrade any Hermes SQLite schema v1..v32 (or v33 with legacy FTS layout) to canonical v33 + FTS layout 3,
/// mirroring `persistence/sqlite/_schema.py` declaratively.
fn upgrade_to_v33(
    tx: &Connection,
    source_version: i64,
    rebuild_fts: bool,
) -> Result<(), StorageError> {
    let reference = ReferenceSchema::load()?;

    // 1. Always retire legacy CJK FTS index/triggers before any table alteration or rebuild.
    drop_retired_cjk_index(tx)?;

    // 2. If we are rebuilding FTS (v1..v31 or legacy FTS layout), drop old triggers/views/vtables
    // first so UPDATEs and ALTERs below never trip stale pre-v33 triggers.
    if rebuild_fts {
        drop_legacy_fts_and_triggers(tx)?;
    }

    // 3. Declaratively create missing tables and ADD missing columns from canonical schema_v33.sql.
    reconcile_tables_and_columns(tx, &reference)?;

    // 4. Heal primary keys that ALTER TABLE ADD COLUMN cannot change.
    heal_gateway_routing_pk(tx, &reference)?;
    heal_session_model_usage_pk(tx, &reference)?;

    // 5. Idempotent data backfills & version-gated migrations matching `_schema.py`.
    run_data_migrations(tx, source_version)?;

    // 6. Deduplicate legacy duplicate session titles before creating the unique index.
    dedupe_session_titles(tx)?;

    // 7. Create all missing indexes from canonical schema_v33.sql.
    for (name, sql) in &reference.indexes {
        if !sqlite_object_exists(tx, "index", name)? {
            tx.execute_batch(sql)?;
        }
    }

    // 8. Refresh non-FTS triggers (and rebuild full FTS5 layout 3 when needed).
    if rebuild_fts {
        for (name, sql) in &reference.views {
            tx.execute(&format!("DROP VIEW IF EXISTS {}", quote_ident(name)), [])?;
            tx.execute_batch(sql)?;
        }
        for (name, sql) in &reference.virtual_tables {
            tx.execute(&format!("DROP TABLE IF EXISTS {}", quote_ident(name)), [])?;
            tx.execute_batch(sql)?;
        }
        for (name, sql) in &reference.triggers {
            tx.execute(&format!("DROP TRIGGER IF EXISTS {}", quote_ident(name)), [])?;
            tx.execute_batch(sql)?;
        }
        tx.execute_batch(
            "DELETE FROM state_meta WHERE key IN ('fts_stale', 'fts_rebuild_high_water', 'fts_rebuild_progress', 'fts_optimize_available');
             INSERT OR REPLACE INTO state_meta(key, value) VALUES
                 ('fts_storage_version', '3'),
                 ('message_uid_backfill', '1'),
                 ('legacy_refusal_repair', '1');
             INSERT OR IGNORE INTO state_meta(key, value) VALUES
                 ('store_instance_id', lower(hex(randomblob(16)))),
                 ('store_created_at_utc', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'));
             INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');
             INSERT INTO messages_fts_trigram(messages_fts_trigram) VALUES ('rebuild');",
        )?;
    } else {
        for (name, sql) in &reference.triggers {
            if !FTS_TRIGGER_NAMES.contains(&name.as_str()) {
                tx.execute(&format!("DROP TRIGGER IF EXISTS {}", quote_ident(name)), [])?;
                tx.execute_batch(sql)?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO state_meta(key, value) VALUES ('fts_storage_version', ?1)",
            [FTS_STORAGE_VERSION],
        )?;
    }

    // 9. Advance schema_version to v33.
    let updated = tx.execute("UPDATE schema_version SET version = ?1", [SCHEMA_VERSION])?;
    if updated == 0 {
        tx.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            [SCHEMA_VERSION],
        )?;
    }
    Ok(())
}

fn drop_retired_cjk_index(tx: &Connection) -> rusqlite::Result<()> {
    for trigger in [
        "messages_fts_cjk_insert",
        "messages_fts_cjk_update",
        "messages_fts_cjk_delete",
    ] {
        tx.execute(
            &format!("DROP TRIGGER IF EXISTS {}", quote_ident(trigger)),
            [],
        )?;
    }
    tx.execute("DROP TABLE IF EXISTS messages_fts_cjk", [])?;
    for suffix in ["data", "idx", "content", "docsize", "config"] {
        let shadow = format!("messages_fts_cjk_{suffix}");
        if table_exists(tx, &shadow)? {
            tx.execute(
                &format!("DROP TABLE IF EXISTS {}", quote_ident(&shadow)),
                [],
            )?;
        }
    }
    Ok(())
}

fn drop_legacy_fts_and_triggers(tx: &Connection) -> rusqlite::Result<()> {
    let trigger_names: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'trigger' AND tbl_name IN ('messages', 'sessions')",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for name in trigger_names {
        tx.execute(
            &format!("DROP TRIGGER IF EXISTS {}", quote_ident(&name)),
            [],
        )?;
    }
    for view in ["messages_fts_src", "messages_fts_trigram_src"] {
        tx.execute(&format!("DROP VIEW IF EXISTS {}", quote_ident(view)), [])?;
    }
    for vtable in ["messages_fts", "messages_fts_trigram"] {
        tx.execute(&format!("DROP TABLE IF EXISTS {}", quote_ident(vtable)), [])?;
        for suffix in ["data", "idx", "content", "docsize", "config"] {
            let shadow = format!("{vtable}_{suffix}");
            if table_exists(tx, &shadow)? {
                tx.execute(
                    &format!("DROP TABLE IF EXISTS {}", quote_ident(&shadow)),
                    [],
                )?;
            }
        }
    }
    Ok(())
}

fn reconcile_tables_and_columns(
    tx: &Connection,
    reference: &ReferenceSchema,
) -> rusqlite::Result<()> {
    for (table, create_sql, ref_cols) in &reference.tables {
        if !table_exists(tx, table)? {
            tx.execute_batch(create_sql)?;
            continue;
        }
        let existing: HashSet<String> = table_column_names(tx, table)?.into_iter().collect();
        for col in ref_cols {
            if existing.contains(&col.name) {
                continue;
            }
            let mut def = Vec::new();
            if !col.col_type.is_empty() {
                def.push(col.col_type.clone());
            }
            // SQLite permits `NOT NULL` on `ALTER TABLE ADD COLUMN` only when a `DEFAULT` is given.
            if col.notnull && col.pk == 0 && col.dflt_value.is_some() {
                def.push("NOT NULL".to_owned());
            }
            if let Some(dflt) = &col.dflt_value {
                def.push(format!("DEFAULT {dflt}"));
            }
            let alter = format!(
                "ALTER TABLE {} ADD COLUMN {} {}",
                quote_ident(table),
                quote_ident(&col.name),
                def.join(" ")
            );
            tx.execute(&alter, [])?;
        }
    }
    Ok(())
}

fn heal_gateway_routing_pk(tx: &Connection, reference: &ReferenceSchema) -> rusqlite::Result<()> {
    let pk = live_pk_columns(tx, "gateway_routing")?;
    if pk.is_empty() || pk == ["scope", "session_key"] {
        return Ok(());
    }
    let Some((_, ddl, _)) = reference
        .tables
        .iter()
        .find(|(t, _, _)| t == "gateway_routing")
    else {
        return Ok(());
    };
    tx.execute("DROP TABLE IF EXISTS gateway_routing_legacy_pk", [])?;
    tx.execute(
        "ALTER TABLE gateway_routing RENAME TO gateway_routing_legacy_pk",
        [],
    )?;
    tx.execute_batch(ddl)?;
    tx.execute(
        "INSERT OR REPLACE INTO gateway_routing (scope, session_key, entry_json, updated_at)
         SELECT COALESCE(scope, ''), session_key, entry_json, updated_at
         FROM gateway_routing_legacy_pk
         ORDER BY updated_at ASC",
        [],
    )?;
    tx.execute("DROP TABLE gateway_routing_legacy_pk", [])?;
    Ok(())
}

fn heal_session_model_usage_pk(
    tx: &Connection,
    reference: &ReferenceSchema,
) -> rusqlite::Result<()> {
    let pk = live_pk_columns(tx, "session_model_usage")?;
    if pk.is_empty() || pk.iter().any(|c| c == "task") {
        return Ok(());
    }
    let Some((_, ddl, _)) = reference
        .tables
        .iter()
        .find(|(t, _, _)| t == "session_model_usage")
    else {
        return Ok(());
    };
    tx.execute("DROP TABLE IF EXISTS session_model_usage_legacy_pk", [])?;
    tx.execute(
        "ALTER TABLE session_model_usage RENAME TO session_model_usage_legacy_pk",
        [],
    )?;
    tx.execute_batch(ddl)?;
    tx.execute(
        "INSERT OR IGNORE INTO session_model_usage (
            session_id, model, billing_provider, billing_base_url,
            billing_mode, task, api_call_count, input_tokens,
            output_tokens, cache_read_tokens, cache_write_tokens,
            reasoning_tokens, estimated_cost_usd, actual_cost_usd,
            cost_status, cost_source, first_seen, last_seen
         )
         SELECT session_id, model,
                COALESCE(billing_provider, ''),
                COALESCE(billing_base_url, ''),
                COALESCE(billing_mode, ''),
                COALESCE(task, ''),
                COALESCE(api_call_count, 0),
                COALESCE(input_tokens, 0),
                COALESCE(output_tokens, 0),
                COALESCE(cache_read_tokens, 0),
                COALESCE(cache_write_tokens, 0),
                COALESCE(reasoning_tokens, 0),
                COALESCE(estimated_cost_usd, 0),
                COALESCE(actual_cost_usd, 0),
                cost_status, cost_source, first_seen, last_seen
         FROM session_model_usage_legacy_pk",
        [],
    )?;
    tx.execute("DROP TABLE session_model_usage_legacy_pk", [])?;
    Ok(())
}

fn run_data_migrations(tx: &Connection, source_version: i64) -> rusqlite::Result<()> {
    // Unconditional idempotent repairs for legacy NULLs (matching `_schema.py`).
    tx.execute("UPDATE messages SET active = 1 WHERE active IS NULL", [])?;
    tx.execute(
        "UPDATE messages SET compacted = 0 WHERE compacted IS NULL",
        [],
    )?;
    tx.execute(
        "UPDATE messages SET _compressed_summary = 0 WHERE _compressed_summary IS NULL",
        [],
    )?;
    tx.execute(
        "UPDATE messages SET display_order = id
         WHERE display_order IS NULL AND (active = 1 OR compacted = 1)",
        [],
    )?;
    tx.execute(
        "UPDATE messages SET message_uid = lower(hex(randomblob(16)))
         WHERE message_uid IS NULL",
        [],
    )?;

    // v20: seed session_model_usage from historical sessions aggregates.
    if source_version < 20 {
        tx.execute(
            "INSERT OR IGNORE INTO session_model_usage (
                session_id, model, billing_provider, billing_base_url,
                billing_mode, task, api_call_count, input_tokens,
                output_tokens, cache_read_tokens, cache_write_tokens,
                reasoning_tokens, estimated_cost_usd, actual_cost_usd,
                cost_status, cost_source, first_seen, last_seen
             )
             SELECT id, model,
                    COALESCE(billing_provider, ''),
                    COALESCE(billing_base_url, ''),
                    COALESCE(billing_mode, ''),
                    '',
                    COALESCE(api_call_count, 0),
                    COALESCE(input_tokens, 0),
                    COALESCE(output_tokens, 0),
                    COALESCE(cache_read_tokens, 0),
                    COALESCE(cache_write_tokens, 0),
                    COALESCE(reasoning_tokens, 0),
                    COALESCE(estimated_cost_usd, 0),
                    COALESCE(actual_cost_usd, 0),
                    cost_status, cost_source, started_at, COALESCE(last_activity_at, ended_at, started_at)
             FROM sessions
             WHERE model IS NOT NULL AND model <> ''
               AND (COALESCE(input_tokens, 0) > 0
                 OR COALESCE(output_tokens, 0) > 0
                 OR COALESCE(api_call_count, 0) > 0)",
            [],
        )?;
    }

    // v25: deduplicate inline sessions.system_prompt into content-addressed system_prompts table.
    if source_version < 25 {
        let rows: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, system_prompt FROM sessions WHERE system_prompt IS NOT NULL",
            )?;
            let iter = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            iter.collect::<rusqlite::Result<_>>()?
        };
        for (session_id, prompt) in rows {
            let hash = sha256_hex(prompt.as_bytes());
            tx.execute(
                "INSERT OR IGNORE INTO system_prompts (hash, prompt) VALUES (?1, ?2)",
                params![hash, prompt],
            )?;
            tx.execute(
                "UPDATE sessions SET system_prompt_hash = ?1, system_prompt = NULL WHERE id = ?2",
                params![hash, session_id],
            )?;
        }
    }

    // v33: restore missing `_delegate_from` markers on subagent rows while keeping subagent
    // compression chains intact.
    if source_version < SCHEMA_VERSION {
        tx.execute(
            "UPDATE sessions
             SET model_config = json_set(COALESCE(model_config, '{}'), '$._delegate_from', parent_session_id)
             WHERE source = 'subagent' AND parent_session_id IS NOT NULL
               AND json_valid(COALESCE(model_config, '{}'))
               AND json_extract(CASE WHEN json_valid(model_config) THEN model_config ELSE '{}' END,
                                '$._delegate_from') IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM sessions AS parent WHERE parent.id = sessions.parent_session_id
                   AND parent.source = 'subagent' AND parent.end_reason = 'compression'
               )",
            [],
        )?;
    }

    Ok(())
}

/// Mirror `_ensure_unique_title_index` in `_schema.py`: keep user titles (oldest keeps base,
/// newer duplicates get ` #2`, ` #3`, ...), clear lower-ranked duplicate auto titles.
fn dedupe_session_titles(tx: &Connection) -> rusqlite::Result<()> {
    let rows: Vec<(i64, String, Option<String>, f64)> = {
        let mut stmt = tx.prepare(
            "SELECT rowid, title, title_source, started_at FROM sessions
             WHERE title IN (
                 SELECT title FROM sessions WHERE title IS NOT NULL GROUP BY title HAVING COUNT(*) > 1
             )",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        iter.collect::<rusqlite::Result<_>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let rank = |source: Option<&str>| match source {
        None | Some("user") => 3i32,
        Some("llm") => 2,
        _ => 1,
    };
    let mut existing_titles: HashSet<String> = {
        let mut stmt = tx.prepare("SELECT title FROM sessions WHERE title IS NOT NULL")?;
        let iter = stmt.query_map([], |row| row.get(0))?;
        iter.collect::<rusqlite::Result<_>>()?
    };
    let mut groups: BTreeMap<String, Vec<(i64, Option<String>, f64)>> = BTreeMap::new();
    for (rowid, title, source, started_at) in rows {
        groups
            .entry(title)
            .or_default()
            .push((rowid, source, started_at));
    }
    for (title, mut group) in groups {
        group.sort_by(|a, b| {
            rank(b.1.as_deref())
                .cmp(&rank(a.1.as_deref()))
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| b.0.cmp(&a.0))
        });
        let mut users: Vec<(i64, Option<String>, f64)> = group
            .iter()
            .filter(|r| rank(r.1.as_deref()) == 3)
            .cloned()
            .collect();
        users.reverse(); // oldest first
        let keep_rowid = users.first().map(|r| r.0).unwrap_or(group[0].0);
        for (rowid, _, _) in users.iter().skip(1) {
            let mut n = 2usize;
            let renamed = loop {
                let candidate = format!("{title} #{n}");
                if existing_titles.insert(candidate.clone()) {
                    break candidate;
                }
                n += 1;
            };
            tx.execute(
                "UPDATE sessions SET title = ?1 WHERE rowid = ?2",
                params![renamed, rowid],
            )?;
        }
        for (rowid, source, _) in &group {
            if *rowid != keep_rowid && rank(source.as_deref()) != 3 {
                tx.execute("UPDATE sessions SET title = NULL WHERE rowid = ?1", [rowid])?;
            }
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let d = digest::digest(&digest::SHA256, bytes);
    let mut out = String::with_capacity(64);
    for byte in d.as_ref() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    sqlite_object_exists(conn, "table", name)
}

fn sqlite_object_exists(conn: &Connection, kind: &str, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = ?1 AND name = ?2)",
        params![kind, name],
        |row| row.get(0),
    )
}

fn table_column_specs(conn: &Connection, table: &str) -> rusqlite::Result<Vec<ColumnSpec>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))?;
    let rows = stmt.query_map([], |row| {
        Ok(ColumnSpec {
            name: row.get(1)?,
            col_type: row.get(2)?,
            notnull: row.get::<_, i64>(3)? != 0,
            dflt_value: row.get(4)?,
            pk: row.get(5)?,
        })
    })?;
    rows.collect()
}

fn table_column_names(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    Ok(table_column_specs(conn, table)?
        .into_iter()
        .map(|c| c.name)
        .collect())
}

fn live_pk_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    if !table_exists(conn, table)? {
        return Ok(Vec::new());
    }
    let mut pk: Vec<(i64, String)> = table_column_specs(conn, table)?
        .into_iter()
        .filter(|c| c.pk > 0)
        .map(|c| (c.pk, c.name))
        .collect();
    pk.sort_by_key(|(ord, _)| *ord);
    Ok(pk.into_iter().map(|(_, name)| name).collect())
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
