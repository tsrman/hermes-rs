use hermes_runtime::storage::{
    migrate_session_copy, SessionStore, StorageError, TranscriptOptions,
};
use rusqlite::Connection;

fn create_v1_minimal_db(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (1);
         CREATE TABLE sessions (
             id TEXT PRIMARY KEY,
             source TEXT NOT NULL,
             model TEXT,
             system_prompt TEXT,
             parent_session_id TEXT,
             started_at REAL NOT NULL,
             ended_at REAL,
             end_reason TEXT,
             message_count INTEGER DEFAULT 0,
             input_tokens INTEGER DEFAULT 0,
             output_tokens INTEGER DEFAULT 0,
             title TEXT
         );
         CREATE TABLE messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL REFERENCES sessions(id),
             role TEXT NOT NULL,
             content TEXT,
             tool_call_id TEXT,
             tool_calls TEXT,
             tool_name TEXT,
             timestamp REAL NOT NULL
         );
         CREATE VIRTUAL TABLE messages_fts USING fts5(content);
         INSERT INTO sessions(id, source, model, system_prompt, started_at, ended_at, message_count, input_tokens, output_tokens, title)
         VALUES
             ('s1', 'cli', 'gpt-4o', 'Legacy system prompt A', 100.0, 105.0, 2, 120, 45, 'Duplicate Title'),
             ('s2', 'cli', 'gpt-4o', 'Legacy system prompt A', 200.0, 205.0, 1, 50, 10, 'Duplicate Title');
         INSERT INTO messages(id, session_id, role, content, timestamp)
         VALUES
             (1, 's1', 'user', 'Hello from v1 database', 100.0),
             (2, 's1', 'assistant', 'Hi back from v1', 101.0),
             (3, 's2', 'user', 'Привет мир подстрока тест', 200.0);
         INSERT INTO messages_fts(rowid, content) VALUES (1, 'Hello from v1 database');",
    )
    .unwrap();
}

/// Simulates an upstream v0.15-era database (schema ~v15): has legacy inline FTS + retired CJK
/// table, legacy 1-col PK `gateway_routing`, legacy 5-col PK `session_model_usage`, NULL `active`
/// rows, inline `system_prompt`, and unmarked `subagent` rows.
fn create_v15_era_db(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (15);
         CREATE TABLE state_meta (key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO state_meta VALUES ('fts_storage_version', '1');
         CREATE TABLE sessions (
             id TEXT PRIMARY KEY,
             source TEXT NOT NULL,
             model TEXT,
             model_config TEXT,
             system_prompt TEXT,
             parent_session_id TEXT,
             started_at REAL NOT NULL,
             ended_at REAL,
             end_reason TEXT,
             message_count INTEGER DEFAULT 0,
             input_tokens INTEGER DEFAULT 0,
             output_tokens INTEGER DEFAULT 0,
             api_call_count INTEGER DEFAULT 0,
             title TEXT,
             title_source TEXT
         );
         CREATE TABLE messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL REFERENCES sessions(id),
             role TEXT NOT NULL,
             content TEXT,
             tool_call_id TEXT,
             tool_calls TEXT,
             tool_name TEXT,
             timestamp REAL NOT NULL,
             active INTEGER
         );
         CREATE TABLE gateway_routing (
             session_key TEXT PRIMARY KEY,
             entry_json TEXT NOT NULL,
             updated_at REAL NOT NULL
         );
         CREATE TABLE session_model_usage (
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             model TEXT NOT NULL,
             billing_provider TEXT NOT NULL DEFAULT '',
             billing_base_url TEXT NOT NULL DEFAULT '',
             billing_mode TEXT NOT NULL DEFAULT '',
             api_call_count INTEGER NOT NULL DEFAULT 0,
             input_tokens INTEGER NOT NULL DEFAULT 0,
             output_tokens INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode)
         );
         CREATE VIRTUAL TABLE messages_fts USING fts5(content, tool_name, tool_calls);
         CREATE VIRTUAL TABLE messages_fts_cjk USING fts5(content);
         CREATE TRIGGER messages_fts_cjk_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts_cjk(rowid, content) VALUES (new.id, new.content);
         END;
         INSERT INTO sessions(id, source, model, model_config, system_prompt, parent_session_id, started_at, ended_at, message_count, input_tokens, output_tokens, api_call_count, title, title_source)
         VALUES
             ('parent1', 'cli', 'claude-sonnet', '{}', 'Shared v15 prompt', NULL, 1000.0, 1010.0, 2, 500, 120, 2, 'Work Chat', 'user'),
             ('parent2', 'cli', 'claude-sonnet', '{}', 'Shared v15 prompt', NULL, 1020.0, 1030.0, 1, 200, 80, 1, 'Work Chat', 'llm'),
             ('child1', 'subagent', 'claude-sonnet', '{}', 'Child prompt', 'parent1', 1005.0, 1008.0, 1, 100, 40, 1, NULL, NULL);
         INSERT INTO messages(id, session_id, role, content, timestamp, active)
         VALUES
             (1, 'parent1', 'user', 'Check the deployment checklist', 1000.0, NULL),
             (2, 'parent1', 'assistant', 'Deployment checklist verified', 1001.0, 1),
             (3, 'parent2', 'user', 'Подстрока кириллица для триграмм', 1020.0, NULL),
             (4, 'child1', 'assistant', 'Subagent hidden from trigram', 1005.0, 1);
         INSERT INTO gateway_routing(session_key, entry_json, updated_at)
         VALUES ('tg:123', '{\"session_id\":\"parent1\"}', 1000.0);
         INSERT INTO session_model_usage(session_id, model, billing_provider, billing_base_url, billing_mode, api_call_count, input_tokens, output_tokens)
         VALUES ('parent1', 'claude-sonnet', 'anthropic', '', '', 2, 500, 120);",
    )
    .unwrap();
}

#[test]
fn migrates_minimal_v1_schema_to_v33_with_deduped_prompts_titles_and_fts3() {
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    create_v1_minimal_db(&src_dir.path().join("state.db"));

    let dst_db = dst_dir.path().join("state.db");
    migrate_session_copy(src_dir.path(), &dst_db).unwrap();

    // Migrated DB opens cleanly with SessionStore (v33), reads all messages, and searches via FTS5.
    let store = SessionStore::open_read_only(dst_dir.path()).unwrap();
    let t1 = store
        .transcript("s1", TranscriptOptions::default())
        .unwrap();
    assert_eq!(t1.session.title.as_deref(), Some("Duplicate Title"));
    assert_eq!(t1.messages.len(), 2);
    assert!(t1.messages.iter().all(|m| m.active));

    let t2 = store
        .transcript("s2", TranscriptOptions::default())
        .unwrap();
    assert_eq!(t2.session.title.as_deref(), Some("Duplicate Title #2"));
    assert_eq!(t2.messages.len(), 1);

    let hits = store.search("Hello", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "s1");

    let conn = Connection::open(&dst_db).unwrap();
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 33);
    let fts_ver: String = conn
        .query_row(
            "SELECT value FROM state_meta WHERE key = 'fts_storage_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_ver, "3");
    // Shared system prompt deduplicated into a single row in system_prompts.
    let sp_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM system_prompts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sp_count, 1);
    // v20 seeded session_model_usage from sessions aggregates.
    let usage_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_model_usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(usage_count, 2);
}

#[test]
fn migrates_v15_era_db_healing_pks_null_active_cjk_fts_and_delegate_markers() {
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    create_v15_era_db(&src_dir.path().join("state.db"));

    let dst_db = dst_dir.path().join("state.db");
    migrate_session_copy(src_dir.path(), &dst_db).unwrap();

    let conn = Connection::open(&dst_db).unwrap();
    // 1. Retired CJK FTS index and its trigger are gone.
    let cjk_exists: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name LIKE 'messages_fts_cjk%')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!cjk_exists, "retired messages_fts_cjk should be dropped");

    // 2. gateway_routing PK healed to (scope, session_key) and data preserved.
    let mut gw_pk: Vec<(i64, String)> = conn
        .prepare("PRAGMA table_info('gateway_routing')")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(5)?, r.get::<_, String>(1)?)))
        .unwrap()
        .filter_map(|r| r.ok().filter(|(pk, _)| *pk > 0))
        .collect();
    gw_pk.sort_by_key(|(pk, _)| *pk);
    assert_eq!(
        gw_pk.into_iter().map(|(_, c)| c).collect::<Vec<_>>(),
        vec!["scope", "session_key"]
    );
    let gw_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM gateway_routing", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gw_count, 1);

    // 3. session_model_usage PK healed to include `task`, existing row preserved + parent2 seeded.
    let has_task_pk: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('session_model_usage') WHERE name = 'task' AND pk > 0)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(has_task_pk);
    let usage_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_model_usage", [], |r| r.get(0))
        .unwrap();
    assert!(usage_rows >= 2);

    // 4. NULL `active` rows healed to 1, `message_uid` and `display_order` populated.
    let null_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE active IS NULL OR message_uid IS NULL OR display_order IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(null_active, 0);

    // 5. Subagent row tagged with _delegate_from and excluded from trigram index.
    let delegate_from: Option<String> = conn
        .query_row(
            "SELECT json_extract(model_config, '$._delegate_from') FROM sessions WHERE id = 'child1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(delegate_from.as_deref(), Some("parent1"));
    let trigram_child_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'Subagent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(trigram_child_hits, 0);

    // 6. Cyrillic substring search works via rebuilt trigram FTS5, and duplicate llm title cleared.
    let trigram_cyrillic_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'кирилл'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(trigram_cyrillic_hits, 1);

    let p1_title: Option<String> = conn
        .query_row("SELECT title FROM sessions WHERE id = 'parent1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let p2_title: Option<String> = conn
        .query_row("SELECT title FROM sessions WHERE id = 'parent2'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(p1_title.as_deref(), Some("Work Chat"));
    assert_eq!(
        p2_title, None,
        "duplicate llm title should be cleared in favor of user title"
    );

    // 7. Full SessionStore reads work on the migrated DB.
    let store = SessionStore::open_read_only(dst_dir.path()).unwrap();
    let t = store
        .transcript("parent1", TranscriptOptions::default())
        .unwrap();
    assert_eq!(t.messages.len(), 2);
}

#[test]
fn rejects_unsupported_versions_zero_and_future_and_already_v33_fts3() {
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    create_v1_minimal_db(&src_dir.path().join("state.db"));

    // First migrate to v33 + FTS3.
    let v33_dir = tempfile::tempdir().unwrap();
    let v33_db = v33_dir.path().join("state.db");
    migrate_session_copy(src_dir.path(), &v33_db).unwrap();

    // Migrating an already-current v33 + FTS3 DB is refused by migrate_session_copy.
    let err = migrate_session_copy(v33_dir.path(), &dst_dir.path().join("out1.db")).unwrap_err();
    assert!(
        matches!(err, StorageError::MigrationSourceVersion(33)),
        "{err}"
    );

    // Version 0 and version 34 are unsupported.
    for bad in [0i64, 34, 999] {
        let conn = Connection::open(src_dir.path().join("state.db")).unwrap();
        conn.execute("UPDATE schema_version SET version = ?1", [bad])
            .unwrap();
        drop(conn);
        let err = migrate_session_copy(
            src_dir.path(),
            &dst_dir.path().join(format!("bad_{bad}.db")),
        )
        .unwrap_err();
        assert!(
            matches!(err, StorageError::UnsupportedSchema(v) if v == bad),
            "bad={bad}: {err}"
        );
    }
}
