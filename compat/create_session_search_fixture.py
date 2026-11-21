#!/usr/bin/env python3
"""Record session_search results from the Python reference over a synthetic store.

Usage: create_session_search_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. The output holds the rows of the store (the Rust test
rebuilds a database from them) and, per case, the arguments and the reference result. The
read by `session_search.rs` unit tests. Times are fixed and the zone is UTC.
"""

import json
import os
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

LONG = "long-content " * 400
SECRETS = "deploy with password=hunter2hunter2xyz and https://example.test/cb?access_token=abcdefgh12345&page=2"
DATA_URL = "data:image/png;base64," + "QUJD" * 20


def build(home: Path):
    from hermes_state import SessionDB
    db = SessionDB(home / "state.db")

    def session(sid, source="cli", **kwargs):
        db.create_session(sid, source, **kwargs)

    def say(sid, role, content, **kwargs):
        return db.append_message(sid, role, content, **kwargs)

    session("s_alpha", model="m-1")
    db.set_session_title("s_alpha", "Alpha project")
    say("s_alpha", "user", f"how do we run the kubernetes rollout? {SECRETS}")
    say("s_alpha", "assistant", "Use kubectl rollout status for the kubernetes deployment.")
    say("s_alpha", "tool", "kubernetes output line", tool_name="terminal")
    say("s_alpha", "assistant", "", tool_calls=[{"id": "c1", "type": "function", "function": {
        "name": "terminal", "arguments": json.dumps({"command": "curl -H 'Authorization: Bearer abcdefghijklmnop12345678' x"})}}])
    for n in range(14):
        say("s_alpha", "user" if n % 2 == 0 else "assistant", f"filler turn {n} about rollouts")
    session("s_parent")
    db.set_session_title("s_parent", "Parent chat")
    say("s_parent", "user", "the zebra migration plan")
    say("s_parent", "assistant", "zebra migration step one")
    db.end_session("s_parent", "compression")
    session("s_child", parent_session_id="s_parent")
    from agent.context_compressor import SUMMARY_PREFIX
    say("s_child", "user", SUMMARY_PREFIX + "earlier work was about zebras")
    say("s_child", "user", "continuing zebra work")
    session("s_reset_old")
    say("s_reset_old", "user", "the walrus experiment notes")
    db.end_session("s_reset_old", "new_session")
    session("s_reset_new")
    say("s_reset_new", "user", "fresh start")
    session("s_cron", source="cron")
    say("s_cron", "user", "kubernetes cron report")
    session("s_tool", source="tool")
    say("s_tool", "user", "kubernetes tool integration")
    session("s_cjk")
    say("s_cjk", "user", "我们去广西桂林漓江旅游")
    say("s_cjk", "assistant", "好的 修改youer服务端")
    session("s_long")
    db.set_session_title("s_long", "Long & noisy")
    say("s_long", "user", LONG + " marker-long")
    say("s_long", "assistant", "\x1b[31mred\x1b[0m output marker-long")
    say("s_long", "user", [{"type": "text", "text": "multimodal marker-long"},
                           {"type": "image_url", "image_url": {"url": DATA_URL}}])
    say("s_long", "user", "hidden scaffold marker-long", display_kind="hidden")
    say("s_long", "user", "rewound marker-long")
    say("s_long", "user", "archived marker-long")
    session("s_empty")
    return db


def patch(path: Path):
    """Fix the clocks and mark rewound and archived rows."""
    conn = sqlite3.connect(path)
    base = 1_780_000_000
    for index, (sid,) in enumerate(conn.execute("SELECT id FROM sessions ORDER BY rowid").fetchall()):
        started = base + index * 1000
        conn.execute("UPDATE sessions SET started_at = ?, last_activity_at = ? WHERE id = ?", (started, started + 500, sid))
    for index, (mid,) in enumerate(conn.execute("SELECT id FROM messages ORDER BY id").fetchall()):
        conn.execute("UPDATE messages SET timestamp = ? WHERE id = ?", (base + index * 10, mid))
    conn.execute("UPDATE messages SET active = 0, compacted = 0 WHERE content = 'rewound marker-long'")
    conn.execute("UPDATE messages SET active = 0, compacted = 1 WHERE content = 'archived marker-long'")
    conn.commit()
    conn.close()


CASES = [
    ("discover_basic", {"query": "kubernetes"}, None),
    ("discover_full", {"query": "kubernetes", "detail": "full", "limit": 5}, None),
    ("discover_phrase", {"query": "\"rollout status\""}, None),
    ("discover_or_relaxed", {"query": "kubernetes banana"}, None),
    ("discover_not", {"query": "kubernetes NOT cron"}, None),
    ("discover_prefix", {"query": "kube*"}, None),
    ("discover_hyphen", {"query": "marker-long", "limit": 5, "role_filter": "user,assistant"}, None),
    ("discover_tool_role", {"query": "kubernetes", "role_filter": "tool"}, None),
    ("discover_special", {"query": "TODO: (fix) \"unbalanced"}, None),
    ("discover_none", {"query": "nonexistentterm"}, None),
    ("discover_cjk", {"query": "广西桂林"}, None),
    ("discover_cjk_short", {"query": "漓江"}, None),
    ("discover_latin_in_cjk", {"query": "youer"}, None),
    ("discover_sort_newest", {"query": "filler OR zebra", "sort": "newest"}, None),
    ("discover_sort_oldest", {"query": "filler OR zebra", "sort": "oldest"}, None),
    ("discover_title", {"query": "Alpha project"}, None),
    ("discover_title_quoted", {"query": "`Parent chat`"}, None),
    ("discover_after", {"query": "kubernetes OR zebra OR walrus", "after": "2026-05-29"}, None),
    ("discover_before", {"query": "kubernetes OR zebra OR walrus", "before": "2026-05-29T00:00:00Z"}, None),
    ("discover_bad_bound", {"query": "kubernetes", "after": "soon"}, None),
    ("discover_exclude", {"query": "kubernetes", "exclude_session_ids": ["s_alpha"]}, None),
    ("discover_current_lineage", {"query": "zebra"}, "s_child"),
    ("discover_current_same", {"query": "kubernetes"}, "s_alpha"),
    ("discover_reset_predecessor", {"query": "walrus"}, "s_reset_new"),
    ("scroll_basic", {"session_id": "s_alpha", "around_message_id": 5, "window": 2}, None),
    ("scroll_edge", {"session_id": "s_alpha", "around_message_id": 1, "window": 3}, None),
    ("scroll_clamped", {"session_id": "s_alpha", "around_message_id": 8, "window": 99}, None),
    ("scroll_bad_anchor", {"session_id": "s_alpha", "around_message_id": "x"}, None),
    ("scroll_foreign_anchor", {"session_id": "s_alpha", "around_message_id": 999}, None),
    ("scroll_current_lineage", {"session_id": "s_child", "around_message_id": 20}, "s_child"),
    ("scroll_rebind", {"session_id": "s_parent", "around_message_id": 20}, None),
    ("scroll_unknown_session", {"session_id": "nope", "around_message_id": 1}, None),
    ("read_short", {"session_id": "s_parent"}, None),
    ("read_truncated", {"session_id": "s_alpha"}, None),
    ("read_long_content", {"session_id": "s_long"}, None),
    ("read_missing", {"session_id": "nope"}, None),
    ("read_link_form", {"session_id": "custom/s_parent"}, None),
    ("read_empty", {"session_id": "s_empty"}, None),
    ("browse", {}, None),
    ("browse_limit", {"limit": 2}, None),
    ("browse_current", {}, "s_child"),
    ("browse_blank_query", {"query": "   "}, None),
]


def main():
    reference = Path(sys.argv[1]).resolve()
    output = Path(sys.argv[2]).resolve()
    home = Path(tempfile.mkdtemp())
    os.environ["HERMES_HOME"] = str(home)
    os.environ["TZ"] = "UTC"
    time.tzset()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    db = build(home)
    db.close()
    patch(home / "state.db")
    from persistence_assembly import open_persistence_reader
    from tools.session_search_tool import session_search

    conn = sqlite3.connect(home / "state.db")
    conn.row_factory = sqlite3.Row
    sessions = [dict(r) for r in conn.execute(
        "SELECT id, source, model, title, parent_session_id, started_at, last_activity_at, ended_at, end_reason, "
        "message_count, tool_call_count, model_config, session_key, archived, hidden FROM sessions ORDER BY rowid")]
    messages = [dict(r) for r in conn.execute(
        "SELECT id, session_id, role, content, tool_calls, tool_name, tool_call_id, timestamp, active, compacted, display_kind "
        "FROM messages ORDER BY id")]
    cases = []
    with open_persistence_reader(home) as reader:
        reader.ready()
        for name, args, current in CASES:
            result = session_search(persistence=reader, current_session_id=current, **args)
            cases.append({"name": name, "args": args, "current": current, "result": json.loads(result)})
    output.write_text(json.dumps({"sessions": sessions, "messages": messages, "cases": cases},
                                 ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(len(sessions), len(messages), len(cases))


main()
