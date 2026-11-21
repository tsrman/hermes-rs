#!/usr/bin/env python3
"""Rehearse a return to Python after Rust CLI turns on disposable profiles."""

import argparse
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import termios
from contextlib import closing
from pathlib import Path
from urllib.request import urlopen

from rehearse_profile_pilot import (
    COMPAT, SESSION, acquire_lock, child, cli_json, interrupt, model_server, socket_ready, wait_for,
)

ARTIFACT = "pilot-result.txt"
ARTIFACT_CONTENT = "Verified Rust pilot file"
RETURN_QUERY = "Continue the pilot in Python."


def python_turn(session, history, base, **storage):
    from run_agent import AIAgent

    agent = AIAgent(
        base_url=base + "/v1", api_key="fixture", provider="custom", api_mode="chat_completions",
        model="fixture", platform="cli", session_id=session, quiet_mode=True, skip_memory=True,
        skip_context_files=True, skip_background_review=True, enabled_toolsets=["file"],
        **storage,
    )
    try:
        result = agent.run_conversation(RETURN_QUERY, conversation_history=history)
        assert result["completed"] and result["final_response"] == "Ответ модели", result
    finally:
        agent.close()


def current_python(profile, base, new_session):
    from persistence_assembly import open_persistence, open_persistence_reader, profile_scope

    histories = {}
    with open_persistence_reader(profile, check_drift=True) as store:
        for session in (SESSION, new_session):
            history = store.messages.read_resume_conversations(session).model
            histories[session] = [message.as_openai_message() for message in history]
            assert [message["role"] for message in histories[session][-4:]] == [
                "user", "assistant", "tool", "assistant",
            ]
    with open_persistence(profile_scope(home=profile), eager=True) as store:
        for session, history in histories.items():
            python_turn(session, history, base, persistence=store)


def export_rust_history(profile, destination, new_session):
    from hermes_cli.session_export import to_legacy_export_dict
    from persistence_assembly import open_persistence_reader

    with open_persistence_reader(profile, check_drift=True) as store:
        sessions = [store.maintenance.export_session(session, include_inactive=True)
                    for session in (SESSION, new_session)]
        assert all(sessions)
        destination.write_text(json.dumps([to_legacy_export_dict(session) for session in sessions]),
                               encoding="utf-8")


def restore_in_v32(profile, base, export_path, new_session):
    from hermes_state import SessionDB

    sessions = json.loads(export_path.read_text(encoding="utf-8"))
    with closing(SessionDB(profile / "state.db")) as store:
        for already_imported in (False, True):
            report = store.import_sessions(sessions)
            assert report["ok"], report
            assert report["imported_ids"] == ([] if already_imported else [new_session]), report
            assert report["skipped_ids"] == ([SESSION, new_session] if already_imported else [SESSION]), report
        old = store.get_messages_as_conversation(new_session, repair_alternation=True, include_row_ids=True)
        assert [message["role"] for message in old] == ["user", "assistant", "tool", "assistant"]
        python_turn(new_session, old, base, session_db=store)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def approve_write(command, env, cwd):
    master, slave = pty.openpty()
    try:
        with child(command, cwd=cwd, env=env, stdin=slave, preexec_fn=controlling_terminal) as cli:
            prompt = bytearray()
            while b"[y/N]" not in prompt:
                if not select.select([master], [], [], 8)[0]:
                    raise RuntimeError("missing Rust approval prompt")
                prompt.extend(os.read(master, 8192))
            assert ARTIFACT_CONTENT.encode() in prompt
            assert not (cwd / ARTIFACT).exists()
            os.write(master, b"yes\n")
            stdout, stderr = cli.communicate(timeout=15)
            assert cli.returncode == 0 and f"Session: {SESSION}" in stderr, stderr
            assert stdout.strip() == "Ответ модели"
    finally:
        os.close(slave)
        os.close(master)


def query(tool, arguments):
    return json.dumps({"tool": tool, "arguments": arguments})


def run_rust(rust, profile_env, work):
    with model_server("local_tools") as base, child([rust, "owner"], env=profile_env) as owner:
        wait_for(lambda: socket_ready(Path(profile_env["HERMES_HOME"])), "Rust owner")
        chat = [rust, "chat", "--model", "fixture", "--base-url", base + "/v1"]
        approve_write(
            [*chat, "--resume", SESSION, "-q",
             query("write_file", {"path": ARTIFACT, "content": ARTIFACT_CONTENT})], profile_env, work,
        )
        created = subprocess.run(
            [*chat, "--oneshot", "--save-session", "-q", query("read_file", {"path": ARTIFACT})],
            env=profile_env, cwd=work, text=True, capture_output=True, check=True, timeout=15,
        )
        assert created.stdout.strip() == "Ответ модели"
        session_line = next(line for line in created.stderr.splitlines() if line.startswith("Session: "))
        session = session_line.removeprefix("Session: ")
        assert session and session != SESSION
        interrupt(owner, signal.SIGTERM, 0)
        return session


def transcript(rust, env, session):
    return cli_json(rust, env, "sessions", "show", session)["messages"]


def tool_exchange(messages, name):
    for index, message in enumerate(messages):
        calls = message.get("tool_calls") or []
        if calls and calls[0]["function"]["name"] == name:
            result = messages[index + 1]
            assert result["role"] == "tool" and result["tool_call_id"] == calls[0]["id"]
            return calls[0], result
    raise AssertionError(f"missing tool exchange for {name}")


def run_worker(python, checkout, mode, env, work, *args):
    subprocess.run([python, __file__, mode, *map(str, args)], cwd=work,
                   env={**env, "PYTHONPATH": checkout}, check=True, timeout=40)


def verify_python_replay(base, expected_tools):
    with urlopen(base + "/requests", timeout=3) as response:
        requests = json.load(response)
    turns = [request for request in requests if request.get("stream")]
    assert len(turns) == len(expected_tools)
    assert all(not request.get("tools") for request in requests if not request.get("stream"))
    for request, name in zip(turns, expected_tools):
        messages = request["messages"]
        assert messages[-1]["role"] == "user" and messages[-1]["content"] == RETURN_QUERY
        call, result = tool_exchange(messages, name)
        assert json.loads(call["function"]["arguments"])["path"] == ARTIFACT
        payload = json.loads(result["content"])
        if name == "write_file":
            assert payload["verified"] and payload["bytes_written"] == len(ARTIFACT_CONTENT)
        else:
            assert ARTIFACT_CONTENT in payload["content"]


def rehearse(rust, python, checkout, legacy, work):
    source, pilot, archive, restored = (work / name for name in ("source", "pilot", "archive", "restored"))
    env = {"PATH": os.environ["PATH"], "HOME": str(work), "LANG": "C.UTF-8",
           "HERMES_HOME": str(source), "HERMES_API_KEY": "fixture", "PYTHONNOUSERSITE": "1"}
    version = 32 if legacy else 33
    subprocess.run([python, str(COMPAT / "create_reference_fixture.py"), legacy or checkout,
                    str(source), "--schema-version", str(version)], env=env, cwd=work,
                   stdout=subprocess.DEVNULL, check=True, timeout=20)
    (source / "config.yaml").write_text("title_generation: {enabled: false}\n", encoding="utf-8")
    before_switch = work / "before-rust"
    if legacy:
        shutil.copytree(source, before_switch)
    original = (source / "state.db").read_bytes()
    pilot_env = {**env, "HERMES_HOME": str(pilot)}
    with acquire_lock(work / "pilot.lock"):
        cli_json(rust, env, "profile", "migrate", "--output", str(pilot))
        new_session = run_rust(rust, pilot_env, work)
        history = {session: transcript(rust, pilot_env, session) for session in (SESSION, new_session)}
        _, write_result = tool_exchange(history[SESSION], "write_file")
        assert json.loads(write_result["content"])["verified"]
        artifact = work / ARTIFACT
        assert artifact.read_text(encoding="utf-8") == ARTIFACT_CONTENT
        fingerprint = (artifact.stat().st_ino, artifact.stat().st_mtime_ns, artifact.read_bytes())

        report = cli_json(rust, pilot_env, "profile", "migrate", "--output", str(archive))
        assert report["database"]["history_verified"]
        archive_env = {**env, "HERMES_HOME": str(archive)}
        snapshot = (archive / "state.db").read_bytes()
        assert {session: transcript(rust, archive_env, session) for session in history} == history
        with model_server("text") as base:
            if legacy:
                export_path = work / "new-sessions.json"
                run_worker(python, checkout, "--export", archive_env, work, archive, export_path, new_session)
                shutil.copytree(before_switch, restored)
                restored_env = {**env, "HERMES_HOME": str(restored)}
                run_worker(python, legacy, "--legacy", restored_env, work, restored, base,
                           export_path, new_session)
                verify_python_replay(base, ["read_file"])
                with closing(sqlite3.connect(restored / "state.db")) as database:
                    assert database.execute("SELECT version FROM schema_version").fetchone() == (32,)
                assert transcript(rust, restored_env, SESSION) == transcript(rust, env, SESSION)
                resumed = transcript(rust, restored_env, new_session)
                assert resumed[-2]["content"] == RETURN_QUERY and resumed[-1]["content"] == "Ответ модели"
                final_env, expected_ids = restored_env, {new_session}
            else:
                run_worker(python, checkout, "--current", pilot_env, work, pilot, base, new_session)
                verify_python_replay(base, ["write_file", "read_file"])
                for session, before in history.items():
                    after = transcript(rust, pilot_env, session)
                    assert after[:len(before)] == before
                    assert [message["content"] for message in after[len(before):]] == [
                        RETURN_QUERY, "Ответ модели",
                    ]
                final_env, expected_ids = pilot_env, set(history)
        hits = cli_json(rust, final_env, "sessions", "search", '"Continue the pilot in Python"')
        assert len(hits) == len(expected_ids) and {hit["session_id"] for hit in hits} == expected_ids
        assert (artifact.stat().st_ino, artifact.stat().st_mtime_ns, artifact.read_bytes()) == fingerprint
        assert (source / "state.db").read_bytes() == original
        assert (archive / "state.db").read_bytes() == snapshot
    print(f"schema {version}: restored Python turns, preserved Rust history and the approved file", flush=True)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--current":
        current_python(Path(sys.argv[2]), *sys.argv[3:])
        return
    if len(sys.argv) > 1 and sys.argv[1] == "--export":
        export_rust_history(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4])
        return
    if len(sys.argv) > 1 and sys.argv[1] == "--legacy":
        restore_in_v32(Path(sys.argv[2]), sys.argv[3], Path(sys.argv[4]), sys.argv[5])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rust_binary", type=Path)
    parser.add_argument("python_binary", type=Path)
    parser.add_argument("python_checkout", type=Path)
    parser.add_argument("legacy_checkout", type=Path)
    args = parser.parse_args()
    rust = str(args.rust_binary.resolve(strict=True))
    python = str(args.python_binary.absolute())
    checkout = str(args.python_checkout.resolve(strict=True))
    legacy = str(args.legacy_checkout.resolve(strict=True))
    for previous in (None, legacy):
        with tempfile.TemporaryDirectory(prefix="hermes-rollback-") as directory:
            rehearse(rust, python, checkout, previous, Path(directory))


if __name__ == "__main__":
    main()
