#!/usr/bin/env python3
"""Create a disposable Hermes profile using the pinned Python storage API."""

import argparse
import sqlite3
import subprocess
import sys
from pathlib import Path

REFERENCE_COMMIT = "2f8c1e7ed051c311af4f3b3ca0f8976b9c40839b"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path, help="Pinned Hermes Python checkout")
    parser.add_argument("profile", type=Path, help="New empty directory for fixture data")
    args = parser.parse_args()

    checkout = args.checkout.resolve(strict=True)
    head = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=checkout, text=True
    ).strip()
    if head != REFERENCE_COMMIT:
        parser.error(f"expected upstream commit {REFERENCE_COMMIT}, got {head}")
    profile = args.profile.resolve()
    profile.mkdir(parents=True, exist_ok=False)
    sys.path.insert(0, str(checkout))

    from persistence.conversation.messages import NewMessage
    from persistence.conversation.sessions import NewSession
    from persistence_assembly import open_persistence, profile_scope

    with open_persistence(profile_scope(home=profile)) as store:
        store.sessions.create(NewSession(session_id="compat-session", source="cli"))
        store.messages.append(
            "compat-session",
            NewMessage.from_live({"role": "user", "content": "Привет, Hermes"}),
        )
        store.messages.append(
            "compat-session",
            NewMessage.from_live({"role": "assistant", "content": "Здравствуйте!"}),
        )

    with sqlite3.connect(f"file:{profile / 'state.db'}?mode=ro", uri=True) as db:
        rows = db.execute(
            "SELECT role, content FROM messages WHERE session_id = ? ORDER BY id",
            ("compat-session",),
        ).fetchall()
        assert rows == [("user", "Привет, Hermes"), ("assistant", "Здравствуйте!")]
        assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
    print(profile / "state.db")


if __name__ == "__main__":
    main()
