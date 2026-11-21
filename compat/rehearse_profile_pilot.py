#!/usr/bin/env python3
"""Rehearse interrupted Python-to-Rust handoff on a disposable Linux profile."""

import argparse
import fcntl
import json
import os
import select
import signal
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import closing, contextmanager
from pathlib import Path
from urllib.request import urlopen

from create_reference_fixture import REFERENCE_MESSAGES

COMPAT = Path(__file__).resolve().parent
SESSION = "compat-session"


def wait_for(predicate, description, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise RuntimeError(f"timed out waiting for {description}")


@contextmanager
def child(command, **kwargs):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, **kwargs)
    try:
        yield process
    finally:
        if process.poll() is None:
            process.kill()
        process.communicate(timeout=10)


def interrupt(process, signum=signal.SIGKILL, status=None):
    process.send_signal(signum)
    _, error = process.communicate(timeout=10)
    expected = -signum if status is None else status
    assert process.returncode == expected, (process.returncode, error)


@contextmanager
def model_server(scenario="stall_on_resume"):
    with child([sys.executable, str(COMPAT / "model_server.py"),
                "--scenario", scenario]) as server:
        if not select.select([server.stdout], [], [], 10)[0]:
            raise RuntimeError("model fixture did not start")
        base = server.stdout.readline().strip()
        if not base.startswith("http://127.0.0.1:"):
            raise RuntimeError("model fixture is unavailable")
        yield base


def release_model(base):
    with urlopen(base + "/release", timeout=3) as response:
        response.read()


def model_has_request(base):
    with urlopen(base + "/requests", timeout=3) as response:
        return bool(json.load(response))


def reference_worker(control, base):
    from persistence_assembly import open_persistence, profile_scope
    from run_agent import AIAgent

    stopping = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stopping.set())
    with open_persistence(profile_scope(home=Path(os.environ["HERMES_HOME"]))) as persistence:
        agent = AIAgent(
            base_url=base + "/v1", api_key="fixture", provider="custom",
            api_mode="chat_completions", model="fixture", platform="cli", session_id=SESSION,
            persistence=persistence, enabled_toolsets=["file"], quiet_mode=True, skip_memory=True,
            skip_context_files=True, skip_background_review=True,
        )
        try:
            result = agent.run_conversation("hold resume", conversation_history=[
                {"role": role, "content": content} for role, content in REFERENCE_MESSAGES
            ])
            assert result["completed"] and result["final_response"] == "Ответ модели", result
            (control / "drained").touch()
            if not stopping.wait(timeout=25):
                raise RuntimeError("missing stop request")
            (control / "stopping").touch()
            # Hold shutdown open for the stop-phase interruption.
            time.sleep(25)
        finally:
            agent.close()


def drain_and_stop(python, checkout, control, env):
    worker = [python, __file__, "--reference-worker", str(control)]
    watcher = [sys.executable, "-c", """
import sys, time
from pathlib import Path
until = Path(sys.argv[1])
print("waiting", flush=True)
while not until.exists():
    time.sleep(0.05)
""", str(control / "drained")]
    with model_server() as base, child([*worker, base], cwd=control,
                                      env={**env, "PYTHONPATH": checkout}) as old:
        wait_for(lambda: model_has_request(base), "Python request in flight")
        with child(watcher) as observer:
            assert select.select([observer.stdout], [], [], 5)[0]
            assert observer.stdout.readline().strip() == "waiting"
            interrupt(observer)
        assert old.poll() is None
        assert not (control / "drained").exists()
        release_model(base)
        wait_for((control / "drained").exists, "settled Python turn")
        print("drain: interrupted wait, then settled the same Python turn", flush=True)

        old.terminate()
        wait_for((control / "stopping").exists, "Python shutdown")
        interrupt(old)
        print("stop: interrupted shutdown after the saved turn", flush=True)


def acquire_lock(path):
    lock = path.open("a")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        lock.close()
        raise
    return lock


def recover_lock(path):
    holder = [sys.executable, "-c", """
import fcntl, signal, sys
with open(sys.argv[1], "a") as lease:
    fcntl.flock(lease, fcntl.LOCK_EX)
    print("locked", flush=True)
    signal.pause()
""", str(path)]
    with child(holder) as operator:
        assert select.select([operator.stdout], [], [], 5)[0]
        assert operator.stdout.readline().strip() == "locked"
        try:
            with acquire_lock(path):
                raise AssertionError("a second operator acquired an occupied lock")
        except BlockingIOError:
            pass
        interrupt(operator)
    lock = acquire_lock(path)
    print("lock: competing operator rejected, then reacquired after interruption", flush=True)
    return lock


def migrate_after_interruption(rust, source, destination, env):
    command = [rust, "profile", "migrate", "--output", str(destination)]
    # A rollback-journal write lock keeps the migration at the source read.
    with closing(sqlite3.connect(source / "state.db")) as writer:
        assert writer.execute("PRAGMA journal_mode=DELETE").fetchone() == ("delete",)
        original = (source / "state.db").read_bytes()
        writer.execute("BEGIN EXCLUSIVE")
        with child(command, env=env) as interrupted:
            wait_for(lambda: any(destination.parent.glob(".hermes-migrate-*")),
                     "private migration staging directory", timeout=5)
            interrupt(interrupted)
        writer.rollback()
    assert not destination.exists()
    leftovers = list(destination.parent.glob(".hermes-migrate-*"))
    assert len(leftovers) == 1 and stat.S_IMODE(leftovers[0].stat().st_mode) == 0o700
    assert (source / "state.db").read_bytes() == original
    report = json.loads(subprocess.check_output(command, env=env, text=True, timeout=20))
    assert report["database"]["history_verified"] and report["database"]["fts_integrity"]
    copy = (destination / "state.db").read_bytes()
    repeated = subprocess.run(command, env=env, text=True, capture_output=True, timeout=15)
    assert repeated.returncode == 1 and "already exists" in repeated.stderr
    assert (destination / "state.db").read_bytes() == copy
    assert (source / "state.db").read_bytes() == original
    print("snapshot: retried before publication and preserved a published copy on retry", flush=True)
    return original


def socket_ready(profile):
    with socket.socket(socket.AF_UNIX) as connection:
        try:
            connection.connect(str(profile / "rust-owner" / "owner.sock"))
        except (FileNotFoundError, ConnectionRefusedError):
            return False
    return True


def cli_json(rust, env, *args):
    return json.loads(subprocess.check_output([rust, *args], env=env, text=True, timeout=15))


def resume_after_interruption(rust, destination, env, cwd, old_history):
    owner_command = [rust, "owner"]
    with child(owner_command, env=env) as first:
        wait_for(lambda: socket_ready(destination), "first Rust owner")
        interrupt(first)
    with child(owner_command, env=env) as owner, model_server() as base:
        wait_for(lambda: socket_ready(destination), "replacement Rust owner")
        print("owner: restarted after interruption before the first turn", flush=True)
        chat = [rust, "chat", "--oneshot", "--resume", SESSION, "--model", "fixture",
                "--base-url", base + "/v1", "-q"]
        with child([*chat, "hold resume"], env=env, cwd=cwd) as client:
            wait_for(lambda: model_has_request(base), "Rust request in flight")
            interrupt(client, signal.SIGINT, 130)
        assert cli_json(rust, env, "sessions", "show", SESSION)["messages"] == old_history
        release_model(base)
        result = subprocess.run([*chat, "pilot continuation"], env=env, cwd=cwd, text=True,
                                capture_output=True, timeout=15, check=True)
        assert result.stdout.strip() == "Ответ модели"
        assert f"Session: {SESSION}" in result.stderr
        history = cli_json(rust, env, "sessions", "show", SESSION)["messages"]
        assert history[:len(old_history)] == old_history
        assert [message["content"] for message in history[len(old_history):]] == [
            "pilot continuation", "Ответ модели",
        ]
        search = cli_json(rust, env, "sessions", "search", '"pilot continuation"')
        assert len(search) == 1 and search[0]["session_id"] == SESSION
        print("resume: cancelled pending turn, retried once and found the result in FTS", flush=True)
        interrupt(owner, signal.SIGTERM, 0)


def rehearse(rust, python, checkout, work):
    source, destination = work / "python-profile", work / "rust-profile"
    env = {"PATH": os.environ["PATH"], "HOME": str(work), "LANG": "C.UTF-8",
           "HERMES_HOME": str(source), "HERMES_API_KEY": "fixture", "PYTHONNOUSERSITE": "1"}
    subprocess.run([python, str(COMPAT / "create_reference_fixture.py"), checkout, str(source)],
                   env=env, cwd=work, stdout=subprocess.DEVNULL, check=True, timeout=20)
    (source / "config.yaml").write_text("model: fixture\n", encoding="utf-8")
    credentials = source / ".env"
    credentials.write_text("HERMES_API_KEY=fixture\n", encoding="utf-8")
    credentials.chmod(0o600)
    (work / "fixture.txt").write_text("Synthetic pilot fixture", encoding="utf-8")
    drain_and_stop(python, checkout, work, env)
    old_history = cli_json(rust, env, "sessions", "show", SESSION)["messages"]
    assert sum(message["content"] == "hold resume" for message in old_history) == 1
    assert old_history[-1]["content"] == "Ответ модели"
    env_new = {**env, "HERMES_HOME": str(destination)}
    with recover_lock(work / "pilot.lock"):
        original = migrate_after_interruption(rust, source, destination, env)
        assert (destination / ".env").read_bytes() == credentials.read_bytes()
        assert cli_json(rust, env_new, "sessions", "show", SESSION)["messages"] == old_history
        resume_after_interruption(rust, destination, env_new, work, old_history)
        assert (source / "state.db").read_bytes() == original


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--reference-worker":
        reference_worker(Path(sys.argv[2]), sys.argv[3])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rust_binary", type=Path)
    parser.add_argument("python_binary", type=Path)
    parser.add_argument("python_checkout", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="hermes-pilot-") as directory:
        rehearse(str(args.rust_binary.resolve(strict=True)), str(args.python_binary.absolute()),
                 str(args.python_checkout.resolve(strict=True)), Path(directory))


if __name__ == "__main__":
    main()
