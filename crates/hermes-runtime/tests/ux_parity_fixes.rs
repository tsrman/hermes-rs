//! Targeted integration tests for UX & parity fixes:
//! - Legacy `_config_version < 34` ignores default `display.personality: kawaii`
//! - Default profile home resolves to `~/.hsr` (with `HSR_HOME` > `HERMES_HOME` overrides)
//! - SQLite `state.db` uses `busy_timeout` + WAL mode to avoid transient `database is locked`
//! - Resume target resolution (ID, prefix, title, compression tip) and `repair_alternation` +
//!   context sanitization during session resume.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use hermes_runtime::config::{load_chat_settings, profile_home};
use hermes_runtime::storage::{SessionStore, SessionWriter};
use rusqlite::{params, Connection};
use serde_json::json;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn temp_dir(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("hsr-{label}-"))
        .tempdir()
        .unwrap()
}

fn init_v33_db(profile: &Path) -> Connection {
    let db_path = profile.join("state.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(include_str!("../src/storage/schema_v33.sql"))
        .unwrap();
    conn.execute("INSERT INTO schema_version(version) VALUES (33)", [])
        .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO state_meta(key, value) VALUES ('fts_storage_version', '3')",
        [],
    )
    .unwrap();
    conn
}

#[test]
fn legacy_config_version_under_34_suppresses_kawaii_personality() {
    let dir = temp_dir("legacy-kawaii");
    fs::write(
        dir.path().join("config.yaml"),
        "_config_version: 23\ndisplay:\n  personality: kawaii\nmodel:\n  default: test-model\n",
    )
    .unwrap();
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!(
        settings.prompt.ephemeral_system_prompt, None,
        "configs with _config_version < 34 must not inject legacy kawaii personality"
    );

    // Explicitly migrated config (_config_version: 34) honors display.personality.
    fs::write(
        dir.path().join("config.yaml"),
        "_config_version: 34\ndisplay:\n  personality: pirate\nmodel:\n  default: test-model\n",
    )
    .unwrap();
    let settings_v34 = load_chat_settings(Some(dir.path())).unwrap();
    let prompt_v34 = settings_v34
        .prompt
        .ephemeral_system_prompt
        .as_deref()
        .expect("v34 config should honor display.personality");
    assert!(prompt_v34.contains("Captain Hermes"));

    // Legacy config with verbatim kawaii text copied into agent.system_prompt is also scrubbed.
    fs::write(
        dir.path().join("config.yaml"),
        "_config_version: 23\nagent:\n  system_prompt: \"You are a kawaii assistant! Use cute expressions like (◕‿◕), ★, ♪, and ~! Add sparkles and be super enthusiastic about everything! Every response should feel warm and adorable desu~! ヽ(>∀<☆)ノ\"\n",
    )
    .unwrap();
    let settings_scrubbed = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!(
        settings_scrubbed.prompt.ephemeral_system_prompt, None,
        "pre-v34 verbatim kawaii agent.system_prompt must be scrubbed"
    );
}

#[test]
fn profile_home_defaults_to_hsr_and_prefers_hsr_home_over_hermes_home() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev_home = std::env::var_os("HOME");
    // Windows resolves the default profile root from USERPROFILE, not HOME.
    let prev_userprofile = std::env::var_os("USERPROFILE");
    let prev_hsr = std::env::var_os("HSR_HOME");
    let prev_hermes = std::env::var_os("HERMES_HOME");

    let fake = temp_dir("home");
    let fake_home = fake.path().join("userhome");
    let custom_hermes = fake.path().join("custom-hermes");
    let custom_hsr = fake.path().join("custom-hsr");
    fs::create_dir_all(fake_home.join(".hsr").join("profiles").join("work")).unwrap();
    fs::write(
        fake_home
            .join(".hsr")
            .join("profiles")
            .join("work")
            .join("config.yaml"),
        "",
    )
    .unwrap();

    // 1. Neither HSR_HOME nor HERMES_HOME -> ~/.hsr
    unsafe {
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("USERPROFILE", &fake_home);
        std::env::remove_var("HSR_HOME");
        std::env::remove_var("HERMES_HOME");
    }
    assert_eq!(profile_home(None), Ok(Some(fake_home.join(".hsr"))));
    assert_eq!(
        profile_home(Some("work")),
        Ok(Some(fake_home.join(".hsr").join("profiles").join("work")))
    );

    // 2. HERMES_HOME fallback when HSR_HOME unset
    unsafe {
        std::env::set_var("HERMES_HOME", &custom_hermes);
    }
    assert_eq!(profile_home(None), Ok(Some(custom_hermes.clone())));

    // 3. HSR_HOME takes precedence over HERMES_HOME
    unsafe {
        std::env::set_var("HSR_HOME", &custom_hsr);
    }
    assert_eq!(profile_home(None), Ok(Some(custom_hsr.clone())));

    // Restore process environment.
    unsafe {
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        match prev_hsr {
            Some(v) => std::env::set_var("HSR_HOME", v),
            None => std::env::remove_var("HSR_HOME"),
        }
        match prev_hermes {
            Some(v) => std::env::set_var("HERMES_HOME", v),
            None => std::env::remove_var("HERMES_HOME"),
        }
    }
}

#[test]
fn sqlite_busy_timeout_waits_out_brief_write_lock() {
    let dir = temp_dir("busy-timeout");
    let conn = init_v33_db(dir.path());
    conn.execute(
        "INSERT INTO sessions(id, source, model, title, started_at, message_count)
         VALUES ('sess-lock-1', 'cli', 'test-model', 'Lock Test', 100.0, 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages(session_id, role, content, timestamp, active, compacted)
         VALUES ('sess-lock-1', 'user', 'hello', 101.0, 1, 0),
                ('sess-lock-1', 'assistant', 'world', 102.0, 1, 0)",
        [],
    )
    .unwrap();

    // Hold a short BEGIN IMMEDIATE write lock on another connection for 250ms while
    // `SessionWriter::prepare_resume` acquires its lease. Without `busy_timeout`, SQLite
    // fails immediately with SQLITE_BUSY (`database is locked`).
    let db_path = dir.path().join("state.db");
    let holder = thread::spawn(move || {
        let locker = Connection::open(&db_path).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE;").unwrap();
        thread::sleep(Duration::from_millis(250));
        locker.execute_batch("COMMIT;").unwrap();
    });
    // Give locker a head start so the lock is actually held when prepare_resume runs.
    thread::sleep(Duration::from_millis(40));

    let writer = SessionWriter::prepare_resume(dir.path(), "sess-lock-1")
        .expect("busy_timeout should wait out brief SQLite lock instead of failing");
    assert_eq!(writer.session_id(), Some("sess-lock-1"));
    drop(writer);
    holder.join().unwrap();
}

#[test]
fn resolve_resume_target_and_display_transcript_follow_compression_and_titles() {
    let dir = temp_dir("resume-target");
    let conn = init_v33_db(dir.path());

    // Parent compressed session + active child continuation.
    conn.execute(
        "INSERT INTO sessions(id, source, model, title, started_at, ended_at, end_reason, message_count)
         VALUES ('sess-alpha-root', 'cli', 'gpt-old', 'Deploy Plan', 100.0, 120.0, 'compression', 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions(id, source, model, title, parent_session_id, started_at, message_count)
         VALUES ('sess-alpha-tip', 'cli', 'gpt-new', NULL, 'sess-alpha-root', 121.0, 3)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages(session_id, role, content, timestamp, active, compacted, display_kind)
         VALUES ('sess-alpha-root', 'user', 'original question', 101.0, 0, 1, NULL),
                ('sess-alpha-root', 'assistant', 'original answer', 102.0, 0, 1, NULL),
                ('sess-alpha-tip', 'user', 'summary text', 121.0, 1, 0, '_compressed_summary'),
                ('sess-alpha-tip', 'user', 'hidden internal', 122.0, 1, 0, 'hidden'),
                ('sess-alpha-tip', 'user', 'follow-up question', 123.0, 1, 0, NULL),
                ('sess-alpha-tip', 'assistant', 'follow-up answer', 124.0, 1, 0, NULL)",
        [],
    )
    .unwrap();

    let store = SessionStore::open_read_only(dir.path()).unwrap();
    // Exact root ID resolves to compression tip.
    assert_eq!(
        store.resolve_resume_target("sess-alpha-root").unwrap(),
        "sess-alpha-tip"
    );
    // Case-insensitive title resolves to tip.
    assert_eq!(
        store.resolve_resume_target("deploy plan").unwrap(),
        "sess-alpha-tip"
    );
    // Unique prefix resolves cleanly.
    assert_eq!(
        store.resolve_resume_target("sess-alpha-t").unwrap(),
        "sess-alpha-tip"
    );

    // Display transcript walks lineage root -> tip, includes compacted history, and omits hidden rows.
    let transcript = store.resume_display_transcript("sess-alpha-tip").unwrap();
    assert_eq!(transcript.session.id, "sess-alpha-tip");
    assert_eq!(transcript.session.model.as_deref(), Some("gpt-new"));
    let contents: Vec<String> = transcript
        .messages
        .iter()
        .filter_map(|m| m.content.as_str().map(str::to_owned))
        .collect();
    assert!(contents.contains(&"original question".to_owned()));
    assert!(contents.contains(&"original answer".to_owned()));
    assert!(contents.contains(&"follow-up question".to_owned()));
    assert!(contents.contains(&"follow-up answer".to_owned()));
    assert!(!contents.contains(&"hidden internal".to_owned()));
}

#[test]
fn resume_repairs_alternation_and_sanitizes_context_blocks() {
    let dir = temp_dir("resume-repair");
    let conn = init_v33_db(dir.path());
    conn.execute(
        "INSERT INTO sessions(id, source, model, system_prompt, started_at, message_count)
         VALUES ('sess-repair-1', 'cli', 'test-model', 'Base prompt', 100.0, 9)",
        [],
    )
    .unwrap();

    let dangling_calls = json!([{
        "id": "call_orphan",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
    }])
    .to_string();

    // Insert pathological history that upstream `repair_alternation` + `sanitize_context` fixes:
    // 1. User message with ephemeral `<memory-context>` block
    // 2. Hidden message (must be dropped)
    // 3. Consecutive user message (must merge with #1)
    // 4. Stray tool result with no preceding assistant tool call (must be dropped)
    // 5. Assistant message with an unanswered tool call and no text (must be dropped completely)
    // 6. Two consecutive assistant text replies (must merge into one)
    // 7. Final user + assistant exchange
    conn.execute(
        "INSERT INTO messages(session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, active, compacted, display_kind)
         VALUES
           ('sess-repair-1', 'user', '<memory-context>ephemeral stuff</memory-context>\nFirst part', NULL, NULL, NULL, 101.0, 1, 0, NULL),
           ('sess-repair-1', 'user', 'invisible', NULL, NULL, NULL, 101.5, 1, 0, 'hidden'),
           ('sess-repair-1', 'user', 'Second part', NULL, NULL, NULL, 102.0, 1, 0, NULL),
           ('sess-repair-1', 'tool', 'orphan result', 'call_missing', NULL, 'read_file', 103.0, 1, 0, NULL),
           ('sess-repair-1', 'assistant', '', NULL, ?1, NULL, 104.0, 1, 0, NULL),
           ('sess-repair-1', 'assistant', 'Part A', NULL, NULL, NULL, 105.0, 1, 0, NULL),
           ('sess-repair-1', 'assistant', 'Part B', NULL, NULL, NULL, 106.0, 1, 0, NULL),
           ('sess-repair-1', 'user', 'Next question', NULL, NULL, NULL, 107.0, 1, 0, NULL),
           ('sess-repair-1', 'assistant', 'Final answer', NULL, NULL, NULL, 108.0, 1, 0, NULL)",
        params![dangling_calls],
    )
    .unwrap();

    // Previously `prepare_resume` would fail with `InvalidReplay` on dangling/stray tool rows;
    // with upstream `repair_alternation` parity, it recovers cleanly!
    let mut writer = SessionWriter::prepare_resume(dir.path(), "sess-repair-1")
        .expect("resume should repair stray/unanswered tool messages and consecutive roles");
    assert_eq!(writer.session_id(), Some("sess-repair-1"));
    let _history = writer.take_history();
}
