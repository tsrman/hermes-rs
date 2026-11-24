import json
import os
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug/hermes"


class SessionsTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        self.profile = self.home / "profiles/history"
        self.profile.mkdir(parents=True)
        self.database = self.profile / "state.db"
        with sqlite3.connect(self.database) as db:
            db.executescript("""
                CREATE TABLE schema_version(version INTEGER NOT NULL);
                INSERT INTO schema_version VALUES (33);
                CREATE TABLE sessions(id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT,
                    title TEXT, parent_session_id TEXT, started_at REAL NOT NULL,
                    ended_at REAL, end_reason TEXT, model_config TEXT, last_activity_at REAL);
                INSERT INTO sessions(id, source, model, title, started_at)
                    VALUES ('compat', 'cli', 'fixture', 'Example', 10);
                CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                    role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
                    timestamp REAL NOT NULL, finish_reason TEXT, active INTEGER NOT NULL DEFAULT 1,
                    compacted INTEGER NOT NULL DEFAULT 0, display_metadata TEXT,
                    _compressed_summary INTEGER NOT NULL DEFAULT 0);
                CREATE VIRTUAL TABLE messages_fts USING fts5(content);
                INSERT INTO messages(id, session_id, role, content, timestamp, active, compacted) VALUES
                    (1, 'compat', 'user', 'привет marker', 30, 1, 0),
                    (2, 'compat', 'assistant', 'archive marker', 20, 0, 1),
                    (3, 'compat', 'user', 'rewound marker', 10, 0, 0),
                    (5, 'compat', 'tool', 'tool marker', 11, 1, 0);
                INSERT INTO messages_fts(rowid, content) SELECT id, content FROM messages;
            """)
            db.execute(
                "INSERT INTO messages(id, session_id, role, content, tool_calls, timestamp, display_metadata) "
                "VALUES (4, 'compat', 'assistant', ?, ?, 12, ?)",
                ('\x00json:[{"type":"text","text":"structured"}]', '[{"id":"fixture-call"}]', '{"source":"fixture"}'),
            )
            db.execute("UPDATE messages SET tool_call_id = ?, tool_name = ? WHERE id = 5", ("fixture-call", "read_file"))

    def hermes(self, *arguments, profile="history"):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home)}
        return subprocess.run(
            [str(BINARY), "-p", profile, "sessions", *arguments], cwd=self.home,
            env=env, text=True, capture_output=True, timeout=15,
        )

    def read_json(self, *arguments):
        result = self.hermes(*arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_reads_live_and_archived_rows_in_insertion_order(self):
        initial = self.database.read_bytes()
        current = self.read_json("show", "compat")
        self.assertEqual(current["session"]["id"], "compat")
        self.assertEqual(current["session"]["title"], "Example")
        self.assertEqual([row["id"] for row in current["messages"]], [1, 4, 5])
        call = current["messages"][1]
        self.assertEqual(call["content"], [{"type": "text", "text": "structured"}])
        self.assertEqual(call["tool_calls"], [{"id": "fixture-call"}])
        self.assertEqual(call["display_metadata"], {"source": "fixture"})
        self.assertEqual(current["messages"][2]["tool_call_id"], "fixture-call")

        all_rows = self.read_json("show", "compat", "--all")["messages"]
        self.assertEqual([row["id"] for row in all_rows], [1, 2, 3, 4, 5])
        self.assertEqual([(row["active"], row["compacted"]) for row in all_rows[1:3]],
                         [(False, True), (False, False)])
        self.assertEqual(self.database.read_bytes(), initial)

    def test_fts_search_includes_compacted_rows_and_excludes_rewound_and_tools(self):
        for layout in ("inline", "external"):
            with self.subTest(layout=layout):
                if layout == "external":
                    with sqlite3.connect(self.database) as db:
                        db.executescript("""
                            DROP TABLE messages_fts;
                            CREATE VIEW messages_fts_src AS
                                SELECT id, content, tool_name, tool_calls FROM messages;
                            CREATE VIRTUAL TABLE messages_fts USING fts5(
                                content, tool_name, tool_calls,
                                content='messages_fts_src', content_rowid='id');
                            INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');
                        """)
                results = self.read_json("search", "marker")
                self.assertEqual({row["id"] for row in results}, {1, 2})
                self.assertEqual({row["session_id"] for row in results}, {"compat"})
                self.assertEqual({row["snippet"] for row in results}, {"привет marker", "archive marker"})
                self.assertEqual(len(self.read_json("search", "marker", "--limit", "1")), 1)
        self.assertEqual(self.read_json("search", "absent"), [])

    def test_unknown_schema_and_missing_databases_are_never_created_or_rewritten(self):
        missing = self.hermes("show", "compat", profile="default")
        self.assertNotEqual(missing.returncode, 0)
        self.assertFalse((self.home / "state.db").exists())
        with sqlite3.connect(self.database) as db:
            db.execute("UPDATE schema_version SET version = 999")
        initial = self.database.read_bytes()
        unknown = self.hermes("show", "compat")
        self.assertNotEqual(unknown.returncode, 0)
        self.assertIn("unsupported state.db schema version 999", unknown.stderr)
        self.assertEqual(self.database.read_bytes(), initial)

    def test_v32_reads_archive_and_skips_unmarked_delegate_after_compression(self):
        with sqlite3.connect(self.database) as db:
            db.executescript("""
                UPDATE schema_version SET version = 32;
                UPDATE sessions SET ended_at = 15, end_reason = 'compression' WHERE id = 'compat';
                INSERT INTO sessions(id, source, parent_session_id, started_at)
                    VALUES ('tip', 'cli', 'compat', 16), ('unmarked-delegate', 'subagent', 'compat', 20);
                INSERT INTO messages(id, session_id, role, content, timestamp)
                    VALUES (6, 'tip', 'assistant', 'continuation', 18),
                           (7, 'unmarked-delegate', 'assistant', 'unrelated delegate', 25);
            """)
        initial = self.database.read_bytes()

        rows = self.read_json("show", "compat", "--all")["messages"]
        self.assertEqual([row["id"] for row in rows], [1, 2, 3, 4, 5])
        self.assertTrue(rows[1]["compacted"])
        self.assertEqual({hit["id"] for hit in self.read_json("search", "marker")}, {1, 2})
        continuation = self.read_json("show", "compat", "--follow-compression")
        self.assertEqual(continuation["session"]["id"], "tip")
        delegate = self.read_json("show", "unmarked-delegate", "--follow-compression")
        self.assertEqual(delegate["messages"][0]["content"], "unrelated delegate")
        self.assertEqual(self.database.read_bytes(), initial)

    def test_v32_keeps_a_subagent_session_own_compression_continuation(self):
        with sqlite3.connect(self.database) as db:
            db.executescript("""
                UPDATE schema_version SET version = 32;
                UPDATE sessions SET source = 'subagent', ended_at = 15, end_reason = 'compression'
                WHERE id = 'compat';
                INSERT INTO sessions(id, source, parent_session_id, started_at)
                    VALUES ('delegate-tip', 'subagent', 'compat', 16);
            """)
        result = self.read_json("show", "compat", "--follow-compression")
        self.assertEqual(result["session"]["id"], "delegate-tip")

    def test_follows_compression_without_selecting_a_fork_or_closed_sibling(self):
        with sqlite3.connect(self.database) as db:
            db.executescript("""
                UPDATE sessions SET end_reason = 'compression', ended_at = 15 WHERE id = 'compat';
                INSERT INTO sessions(id, source, parent_session_id, started_at, ended_at, end_reason, model_config)
                    VALUES ('middle', 'cli', 'compat', 16, 18, 'compression', NULL),
                           ('tip', 'cli', 'middle', 19, NULL, NULL, NULL),
                           ('sibling', 'cli', 'compat', 20, NULL, NULL, NULL),
                           ('closed', 'cli', 'middle', 21, 22, 'agent_close', NULL),
                           ('branch', 'cli', 'middle', 23, NULL, NULL, '{"_branched_from":"middle"}'),
                           ('delegate', 'cli', 'middle', 24, NULL, NULL, '{"_delegate_from":"middle"}'),
                           ('reset', 'cli', 'middle', 25, NULL, NULL, '{"_reset_from":"middle"}'),
                           ('tool', 'tool', 'middle', 26, NULL, NULL, NULL);
                INSERT INTO messages(id, session_id, role, content, timestamp)
                    VALUES (6, 'tip', 'assistant', 'continuation', 27);
            """)
        result = self.read_json("show", "compat", "--follow-compression")
        self.assertEqual(result["session"]["id"], "tip")
        self.assertEqual([row["content"] for row in result["messages"]], ["continuation"])
        branch = self.read_json("show", "branch", "--follow-compression")
        self.assertEqual(branch["session"]["id"], "branch")


class SessionMigrationTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        self.source = self.home / "state.db"
        self.output = self.home / "migrated"
        self.output.mkdir()
        self.destination = self.output / "state.db"
        schema = ROOT / "crates/hermes-runtime/src/storage/schema_v33.sql"
        with closing(sqlite3.connect(self.source)) as db, db:
            db.executescript(schema.read_text())
            db.executescript("""
                ALTER TABLE sessions DROP COLUMN prompt_cache_clock;
                DROP TABLE resumable_subagents;
                INSERT INTO schema_version VALUES (32);
                INSERT INTO state_meta VALUES ('fts_storage_version', '3');
                INSERT INTO sessions(id, source, started_at, ended_at, end_reason)
                    VALUES ('root', 'cli', 1, 2, 'compression');
                INSERT INTO sessions(id, source, started_at, parent_session_id, model_config)
                    VALUES ('tip', 'cli', 3, 'root', NULL),
                           ('delegate', 'subagent', 4, 'root', '{"keep":true}'),
                           ('malformed', 'subagent', 5, 'tip', '{');
                UPDATE sessions SET ended_at = 6, end_reason = 'compression' WHERE id = 'delegate';
                INSERT INTO sessions(id, source, started_at, parent_session_id)
                    VALUES ('delegate-tip', 'subagent', 7, 'delegate');
                INSERT INTO messages(id, session_id, role, content, timestamp, active, compacted,
                                     display_metadata)
                    VALUES (1, 'root', 'user', 'archive marker', 1, 0, 1, '{"page":2}'),
                           (2, 'root', 'assistant', 'active marker', 2, 1, 0, NULL),
                           (3, 'root', 'user', 'rewound marker', 2, 0, 0, NULL);
            """)

    def hermes(self, home, *arguments):
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(home)}
        return subprocess.run(
            [str(BINARY), "sessions", *arguments], cwd=self.home, env=env,
            text=True, capture_output=True, timeout=15,
        )

    def read_json(self, home, *arguments):
        result = self.hermes(home, *arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def profile_command(self, *arguments):
        return subprocess.run(
            [str(BINARY), "profile", *arguments], cwd=self.home,
            env={"HERMES_HOME": str(self.home), "PATH": os.environ.get("PATH", "")},
            text=True, capture_output=True, timeout=15,
        )

    @unittest.skipUnless(sys.platform == "linux", "Linux profile migration")
    def test_profile_migration_supports_named_profiles_without_history(self):
        selected = self.home / "profiles" / "empty"
        selected.mkdir(parents=True)
        (selected / "config.yaml").write_text("model: fixture\n")
        (self.home / ".env").write_text("HERMES_API_KEY=fixture-default\n")
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "pilot"
            result = self.profile_command("-p", "empty", "migrate", "--output", str(destination))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIsNone(json.loads(result.stdout)["database"])
            self.assertEqual((destination / "config.yaml").read_text(), "model: fixture\n")
            self.assertFalse((destination / ".env").exists())
            self.assertFalse((destination / "state.db").exists())

    @unittest.skipUnless(sys.platform == "linux", "Linux profile migration")
    def test_profile_preflight_reports_permissions_and_database_without_secrets(self):
        secret = "fixture-key-never-printed"
        dotenv = self.home / ".env"
        dotenv.write_text(f"HERMES_API_KEY={secret}\n")
        dotenv.chmod(0o644)
        (self.home / "auth.json").write_text(secret)
        initial = self.source.read_bytes()
        result = self.profile_command("check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(secret, result.stdout + result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(report["ready"])
        inventory = {entry["path"]: entry for entry in report["entries"]}
        self.assertEqual(inventory[".env"]["permissions"], "0644")
        self.assertTrue(inventory[".env"]["shared_access"])
        self.assertEqual(inventory["auth.json"]["action"], "inspect_only")
        self.assertEqual(inventory["memories"]["state"], "missing")
        self.assertEqual(report["database"]["source_schema"], 32)
        self.assertEqual(report["database"]["sessions"], 5)
        self.assertEqual(report["database"]["messages"], 3)
        self.assertEqual(self.source.read_bytes(), initial)

    @unittest.skipUnless(sys.platform == "linux", "Linux profile migration")
    def test_profile_migration_copies_committed_history_and_supported_assets(self):
        for relative, contents in {
            "config.yaml": "model: fixture\nplugin: {unknown: true}\n",
            ".env": "HERMES_API_KEY=fixture-api-key\n",
            "auth.json": '{"refresh_token":"fixture-refresh"}',
            "SOUL.md": "Fixture identity",
            "memories/USER.md": "Fixture memory",
            "skills/demo/SKILL.md": "Fixture skill",
        }.items():
            path = self.home / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)
        with tempfile.TemporaryDirectory() as directory, closing(sqlite3.connect(self.source)) as live:
            target = Path(directory) / "pilot"
            self.assertEqual(live.execute("PRAGMA journal_mode=WAL").fetchone(), ("wal",))
            with live:
                live.execute("INSERT INTO messages(id, session_id, role, content, timestamp) "
                             "VALUES (4, 'tip', 'assistant', 'WAL marker', 3)")
            wal = self.home / "state.db-wal"
            original = (self.source.read_bytes(), wal.read_bytes())
            result = self.profile_command("migrate", "--output", str(target))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("fixture-api-key", result.stdout + result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(report["database"]["target_schema"], 33)
            self.assertEqual(report["database"]["messages"], 4)
            self.assertEqual((self.source.read_bytes(), wal.read_bytes()), original)
            for relative in ("config.yaml", ".env", "SOUL.md", "memories/USER.md", "skills/demo/SKILL.md"):
                self.assertEqual((target / relative).read_bytes(), (self.home / relative).read_bytes())
            self.assertFalse((target / "auth.json").exists())
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE((target / ".env").stat().st_mode), 0o600)
            self.assertFalse((target / "state.db-wal").exists())
            self.assertEqual({row["id"] for row in self.read_json(target, "search", "marker")}, {1, 2, 4})
            newer = Path(directory) / "current"
            result = subprocess.run(
                [str(BINARY), "profile", "migrate", "--output", str(newer)], cwd=target,
                env={"HERMES_HOME": str(target), "PATH": os.environ.get("PATH", "")},
                text=True, capture_output=True, timeout=15,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["database"]["source_schema"], 33)
            self.assertEqual(self.read_json(newer, "show", "root", "--all"),
                             self.read_json(target, "show", "root", "--all"))

    @unittest.skipUnless(sys.platform == "linux", "Linux profile migration")
    def test_profile_rejects_unsafe_paths_existing_outputs_and_damaged_indexes(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "pilot"
            destination.mkdir()
            (destination / "keep").write_text("existing")
            existing = self.profile_command("migrate", "--output", str(destination))
            self.assertNotEqual(existing.returncode, 0)
            self.assertEqual((destination / "keep").read_text(), "existing")
            output = Path(directory) / "new"
            (self.home / ".env").symlink_to(destination / "keep")
            blocked = self.profile_command("migrate", "--output", str(output))
            self.assertNotEqual(blocked.returncode, 0)
            self.assertFalse(output.exists())
            (self.home / ".env").unlink()
            with closing(sqlite3.connect(self.source)) as db, db:
                db.execute("INSERT INTO messages_fts(messages_fts) VALUES ('delete-all')")
            check = self.profile_command("check")
            self.assertNotEqual(check.returncode, 0)
            self.assertFalse(json.loads(check.stdout)["ready"])
            blocked = self.profile_command("migrate", "--output", str(output))
            self.assertNotEqual(blocked.returncode, 0)
            self.assertFalse(output.exists())

    def test_migrates_a_snapshot_with_committed_wal_and_preserves_history(self):
        with closing(sqlite3.connect(self.source)) as live:
            self.assertEqual(live.execute("PRAGMA journal_mode=WAL").fetchone(), ("wal",))
            with live:
                live.execute("INSERT INTO messages(id, session_id, role, content, timestamp) "
                             "VALUES (4, 'tip', 'assistant', 'WAL marker', 3)")
            # Only committed WAL frames belong in the backup.
            live.execute("INSERT INTO messages(id, session_id, role, content, timestamp) "
                         "VALUES (5, 'tip', 'assistant', 'uncommitted', 4)")
            wal = self.home / "state.db-wal"
            original = (self.source.read_bytes(), wal.read_bytes())
            before = self.read_json(self.home, "show", "root", "--all")
            result = self.hermes(self.home, "migrate", "--output", "migrated/state.db")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((self.source.read_bytes(), wal.read_bytes()), original)

        after = self.read_json(self.output, "show", "root", "--all")
        self.assertEqual(after, before)
        self.assertEqual(after["messages"][0]["display_metadata"], {"page": 2})
        tip = self.read_json(self.output, "show", "root", "--follow-compression")
        self.assertEqual(tip["session"]["id"], "tip")
        self.assertEqual([row["content"] for row in tip["messages"]], ["WAL marker"])
        self.assertEqual({row["id"] for row in self.read_json(self.output, "search", "marker")},
                         {1, 2, 4})

        with closing(sqlite3.connect(self.destination)) as db:
            self.assertEqual(db.execute("SELECT version FROM schema_version").fetchone(), (33,))
            self.assertEqual(db.execute("PRAGMA journal_mode").fetchone(), ("delete",))
            self.assertEqual(db.execute("SELECT prompt_cache_clock FROM sessions WHERE id = 'root'")
                             .fetchone(), (None,))
            self.assertEqual(db.execute("SELECT count(*) FROM resumable_subagents").fetchone(), (0,))
            config = dict(db.execute("SELECT id, model_config FROM sessions WHERE source = 'subagent'"))
            self.assertEqual(json.loads(config["delegate"]), {"keep": True, "_delegate_from": "root"})
            self.assertEqual(config["malformed"], "{")
            self.assertIsNone(config["delegate-tip"])

    def test_never_replaces_the_source_or_an_existing_destination(self):
        original = self.source.read_bytes()
        self.destination.write_bytes(b"existing")
        for destination in (self.source, self.destination):
            with self.subTest(destination=destination):
                result = self.hermes(self.home, "migrate", "--output", str(destination))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("already exists", result.stderr)
        self.assertEqual(self.source.read_bytes(), original)
        self.assertEqual(self.destination.read_bytes(), b"existing")

    def test_does_not_publish_over_existing_sqlite_sidecars(self):
        wal = self.destination.with_name("state.db-wal")
        wal.write_bytes(b"existing WAL")
        for destination in (self.destination, wal):
            with self.subTest(destination=destination):
                result = self.hermes(self.home, "migrate", "--output", str(destination))
                self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.destination.exists())
        self.assertEqual(wal.read_bytes(), b"existing WAL")

    def test_rejects_unsupported_source_layout_or_version(self):
        # Migrating a v32 DB with legacy FTS layout 2 now succeeds and upgrades FTS to layout 3.
        with closing(sqlite3.connect(self.source)) as db, db:
            db.execute("UPDATE state_meta SET value = '2' WHERE key = 'fts_storage_version'")
        legacy_fts_out = self.output / "legacy_fts.db"
        result = self.hermes(self.home, "migrate", "--output", str(legacy_fts_out))
        self.assertEqual(result.returncode, 0, result.stderr)
        with closing(sqlite3.connect(legacy_fts_out)) as db:
            self.assertEqual(db.execute("SELECT version FROM schema_version").fetchone(), (33,))
            self.assertEqual(
                db.execute("SELECT value FROM state_meta WHERE key = 'fts_storage_version'").fetchone(),
                ("3",),
            )
        legacy_fts_out.unlink()

        # Already-v33 DB with FTS layout 3 is rejected by `sessions migrate`.
        with closing(sqlite3.connect(self.source)) as db, db:
            db.execute("UPDATE state_meta SET value = '3' WHERE key = 'fts_storage_version'")
            db.execute("UPDATE schema_version SET version = 33")
        result = self.hermes(self.home, "migrate", "--output", str(self.destination))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires state.db schema version 1..=32", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [])

        # Unknown / future schema versions are rejected.
        with closing(sqlite3.connect(self.source)) as db, db:
            db.execute("UPDATE schema_version SET version = 999")
        result = self.hermes(self.home, "migrate", "--output", str(self.destination))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported state.db schema version 999", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [])

    def test_migrates_early_v15_schema_via_cli_and_supports_search_and_resume(self):
        early_home = self.home / "early_v15"
        early_home.mkdir()
        early_db = early_home / "state.db"
        with closing(sqlite3.connect(early_db)) as db, db:
            db.executescript(
                """
                CREATE TABLE schema_version (version INTEGER NOT NULL);
                INSERT INTO schema_version VALUES (15);
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL,
                    model TEXT,
                    system_prompt TEXT,
                    started_at REAL NOT NULL,
                    ended_at REAL,
                    message_count INTEGER DEFAULT 0,
                    title TEXT
                );
                CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id),
                    role TEXT NOT NULL,
                    content TEXT,
                    timestamp REAL NOT NULL
                );
                INSERT INTO sessions(id, source, model, system_prompt, started_at, ended_at, message_count, title)
                VALUES ('v15-sess', 'cli', 'fixture', 'Early v15 prompt', 500.0, 505.0, 2, 'Old Chat');
                INSERT INTO messages(id, session_id, role, content, timestamp)
                VALUES
                    (1, 'v15-sess', 'user', 'Legacy v15 needle query', 500.0),
                    (2, 'v15-sess', 'assistant', 'Legacy v15 reply', 501.0);
                """
            )
        migrated_home = self.home / "early_v15_migrated"
        migrated_home.mkdir()
        migrated_db = migrated_home / "state.db"
        result = self.hermes(early_home, "migrate", "--output", str(migrated_db))
        self.assertEqual(result.returncode, 0, result.stderr)
        shown = self.read_json(migrated_home, "show", "v15-sess")
        self.assertEqual(shown["session"]["id"], "v15-sess")
        self.assertEqual([m["content"] for m in shown["messages"]], ["Legacy v15 needle query", "Legacy v15 reply"])
        hits = self.read_json(migrated_home, "search", "needle")
        self.assertEqual([h["session_id"] for h in hits], ["v15-sess"])

    def test_fts_integrity_failure_does_not_publish_a_copy(self):
        with closing(sqlite3.connect(self.source)) as db, db:
            db.execute("INSERT INTO messages_fts(messages_fts, rowid, content) "
                       "VALUES ('delete', 1, 'archive marker')")
        original = self.source.read_bytes()
        result = self.hermes(self.home, "migrate", "--output", str(self.destination))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("messages_fts", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [])
        self.assertEqual(self.source.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
