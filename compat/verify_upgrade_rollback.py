"""Verify CLI upgrade (from -> to) and rollback (to -> from) across releases.

Checks SHA-256 checksums, unpacks both release archives (or uses local binaries via
`--local-from`/`--local-to`), creates a session with `from`, upgrades to `to` and resumes the same
session ID, then rolls back to `from` and resumes again—confirming all three turns survive in
`state.db` and remain searchable via FTS5.

Supports both clean Linux containers (without Python, via `--image`) and native execution on
Linux, macOS, and Windows.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile
from contextlib import contextmanager
from http.server import ThreadingHTTPServer
from pathlib import Path

from compat.model_server import Handler
from compat.package_cli_release import checksum
from compat.smoke_cli_release import BASE_URL, model_fixture, mount
from compat.test_tls import create_test_certificates

BIN_NAMES = ("hsr.exe", "hsr", "hermes.exe", "hermes")


def unpack_verified(archive: Path, destination: Path) -> Path:
    sha_file = archive.with_name(archive.name + ".sha256")
    if not sha_file.is_file():
        raise FileNotFoundError(f"missing checksum file {sha_file}")
    expected = sha_file.read_text(encoding="ascii").strip().split()[0].lower()
    actual = checksum(archive).strip().split()[0].lower()
    if expected != actual:
        raise ValueError(f"checksum mismatch for {archive.name}: expected {expected}, got {actual}")

    destination.mkdir(parents=True, exist_ok=True)
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as contents:
            names = contents.namelist()
            if len(names) != 1 or names[0] not in BIN_NAMES:
                raise ValueError(f"unexpected archive contents in {archive.name}: {names}")
            contents.extract(names[0], destination)
            return destination / names[0]

    with tarfile.open(archive, "r:gz") as contents:
        members = contents.getmembers()
        if len(members) != 1 or members[0].name not in BIN_NAMES or not members[0].isfile():
            raise ValueError(
                f"unexpected archive contents in {archive.name}: {[m.name for m in members]}"
            )
        contents.extractall(destination, filter="data")
        return destination / members[0].name


@contextmanager
def native_model():
    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        server.scenario = "tool"
        server.requests = []
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield server, f"http://127.0.0.1:{server.server_port}/v1"
        finally:
            server.shutdown()
            thread.join()


def verify_linux_container(from_bin: Path, to_bin: Path, image: str, work: Path) -> None:
    create_test_certificates(work)
    with model_fixture(work) as fixture:
        docker = [
            "docker",
            "run",
            "--rm",
            "--network",
            f"container:{fixture}",
            "--user",
            "1000:1000",
            "--env",
            "HOME=/tmp",
            "--env",
            "HERMES_HOME=/tmp/hermes-profile",
            "--env",
            "HERMES_API_KEY=fixture",
            *mount(from_bin, "/usr/local/bin/hsr-from"),
            *mount(to_bin, "/usr/local/bin/hsr-to"),
            *mount(work / "trusted.pem", "/etc/ssl/certs/ca-certificates.crt"),
        ]
        result = subprocess.check_output(
            [
                *docker,
                image,
                "sh",
                "-ec",
                r'''
                if command -v python || command -v python3; then exit 1; fi
                cd /tmp
                hsr-from --help >/dev/null
                hsr-to --help >/dev/null
                answer=$(hsr-from chat --oneshot --save-session --model fixture --base-url "$1" \
                    -q 'upgrade rollback step one' 2>/tmp/step1.log)
                test "$answer" = 'Ответ модели'
                session=$(sed -n 's/^Session: //p' /tmp/step1.log)
                test -n "$session"

                answer=$(hsr-to chat --oneshot --resume "$session" --model fixture --base-url "$1" \
                    -q 'upgrade rollback step two')
                test "$answer" = 'Ответ модели'

                answer=$(hsr-from chat --oneshot --resume "$session" --model fixture --base-url "$1" \
                    -q 'upgrade rollback step three')
                test "$answer" = 'Ответ модели'

                hsr-to sessions search '"upgrade rollback step one"' >/tmp/s1.json
                hsr-to sessions search '"upgrade rollback step two"' >/tmp/s2.json
                hsr-from sessions search '"upgrade rollback step three"' >/tmp/s3.json
                cat /tmp/s1.json
                printf '\n---SPLIT---\n'
                cat /tmp/s2.json
                printf '\n---SPLIT---\n'
                cat /tmp/s3.json
                ''',
                "hermes-upgrade-rollback",
                BASE_URL,
            ],
            text=True,
            timeout=120,
        )
        parts = [json.loads(chunk) for chunk in result.split("\n---SPLIT---\n")]
        assert len(parts) == 3
        for idx, needle in enumerate(("step one", "step two", "step three")):
            if not any(needle in hit.get("snippet", "") for hit in parts[idx]):
                raise RuntimeError(f"missing '{needle}' in FTS results: {parts[idx]}")
    print(f"upgrade/rollback passed in {image} ({from_bin.name} <-> {to_bin.name})")


def verify_native(from_bin: Path, to_bin: Path, work: Path) -> None:
    home = work / "user home"
    profile = home / ".hsr" / "profiles" / "upgrade-check"
    profile.mkdir(parents=True)
    (profile / "config.yaml").write_text("model: fixture\n", encoding="utf-8")
    project = work / "project dir"
    project.mkdir()
    (project / "fixture.txt").write_text("upgrade rollback payload", encoding="utf-8")

    env = {
        key: os.environ[key]
        for key in ("SystemRoot", "WINDIR", "TEMP", "TMP", "COMSPEC")
        if key in os.environ
    }
    env.update(HOME=str(home), HERMES_API_KEY="fixture", PATH=str(work / "empty-path"))
    if os.name == "nt":
        env.update(USERPROFILE=str(home), HOME=str(work / "unused-shell-home"))

    def cli(executable: Path, *args: str) -> subprocess.CompletedProcess[str]:
        res = subprocess.run(
            [str(executable), *args],
            cwd=project,
            env=env,
            capture_output=True,
            encoding="utf-8",
            timeout=45,
        )
        if res.returncode != 0:
            raise RuntimeError(
                f"{executable.name} {' '.join(args)} exited with {res.returncode}: {res.stderr}"
            )
        return res

    cli(from_bin, "--help")
    cli(to_bin, "--help")

    with native_model() as (_server, base_url):
        base = ("-p", "upgrade-check", "chat", "--oneshot", "--model", "fixture", "--base-url", base_url)
        step1 = cli(from_bin, *base, "--save-session", "-q", "upgrade rollback step one")
        assert step1.stdout.strip() == "Ответ модели", step1.stdout
        session = next(
            line.removeprefix("Session: ").strip()
            for line in step1.stderr.splitlines()
            if line.startswith("Session: ")
        )
        assert session, f"missing session id in stderr: {step1.stderr}"

        step2 = cli(to_bin, *base, "--resume", session, "-q", "upgrade rollback step two")
        assert step2.stdout.strip() == "Ответ модели", step2.stdout

        step3 = cli(from_bin, *base, "--resume", session, "-q", "upgrade rollback step three")
        assert step3.stdout.strip() == "Ответ модели", step3.stdout

        shown = json.loads(cli(to_bin, "-p", "upgrade-check", "sessions", "show", session).stdout)
        user_texts = [
            msg.get("content", "")
            for msg in shown.get("messages", [])
            if msg.get("role") == "user"
        ]
        for needle in ("step one", "step two", "step three"):
            if not any(needle in text for text in user_texts):
                raise RuntimeError(f"missing '{needle}' in resumed transcript: {user_texts}")

        for binary, phrase in (
            (to_bin, '"upgrade rollback step one"'),
            (to_bin, '"upgrade rollback step two"'),
            (from_bin, '"upgrade rollback step three"'),
        ):
            hits = json.loads(cli(binary, "-p", "upgrade-check", "sessions", "search", phrase).stdout)
            if not hits:
                raise RuntimeError(f"FTS search returned no hits for {phrase} on {binary.name}")

    print(f"native upgrade/rollback passed ({from_bin.name} <-> {to_bin.name})")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--from-archive", type=Path, help="Older release archive (.tar.gz or .zip)")
    parser.add_argument("--to-archive", type=Path, help="Newer release archive (.tar.gz or .zip)")
    parser.add_argument("--local-from", type=Path, help="Local older CLI binary (for local testing)")
    parser.add_argument("--local-to", type=Path, help="Local newer CLI binary (for local testing)")
    parser.add_argument("--image", help="Docker image for Linux container verification without Python")
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="hermes-upgrade-rollback-") as tmp:
        work = Path(tmp)
        if args.local_from and args.local_to:
            from_bin = args.local_from.resolve()
            to_bin = args.local_to.resolve()
        elif args.from_archive and args.to_archive:
            from_bin = unpack_verified(args.from_archive.resolve(), work / "from")
            to_bin = unpack_verified(args.to_archive.resolve(), work / "to")
        else:
            parser.error("provide either (--from-archive and --to-archive) or (--local-from and --local-to)")

        if args.image:
            verify_linux_container(from_bin, to_bin, args.image, work)
        else:
            verify_native(from_bin, to_bin, work)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
