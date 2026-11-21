"""Verify a packaged CLI in a Linux container or on a native runner."""

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
from compat.test_tls import create_test_certificates

BASE_URL = "https://127.0.0.1:8011/v1"
FIXTURE = Path(__file__).with_name("model_server.py").resolve()


def mount(source, target):
    return ["--mount", f"type=bind,source={source},target={target},readonly"]


def unpack(archive, destination):
    expected = archive.with_name(archive.name + ".sha256").read_text(encoding="ascii")
    if expected != checksum(archive):
        raise ValueError(f"checksum mismatch for {archive.name}")
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as contents:
            if contents.namelist() != ["hermes.exe"]:
                raise ValueError(f"unexpected archive contents in {archive.name}")
            contents.extract("hermes.exe", destination)
        return destination / "hermes.exe"
    with tarfile.open(archive, "r:gz") as contents:
        members = contents.getmembers()
        if len(members) != 1 or members[0].name != "hermes" or not members[0].isfile():
            raise ValueError(f"unexpected archive contents in {archive.name}")
        contents.extractall(destination, filter="data")
    return destination / "hermes"


@contextmanager
def model_fixture(certs):
    container = subprocess.check_output([
        "docker", "run", "-d", "--rm", "--network", "none",
        *mount(FIXTURE, "/fixture.py"),
        *mount(certs / "server.pem", "/tls/server.pem"),
        *mount(certs / "server.key", "/tls/server.key"),
        "python:3.12-slim", "python3", "/fixture.py", "--port", "8011",
        "--tls-cert", "/tls/server.pem", "--tls-key", "/tls/server.key",
    ], text=True, timeout=120).strip()
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            logs = subprocess.check_output(["docker", "logs", container], text=True, timeout=5)
            if "https://127.0.0.1:8011" in logs:
                yield container
                return
            time.sleep(0.1)
        raise RuntimeError("TLS fixture did not start")
    finally:
        subprocess.run(["docker", "stop", "-t", "0", container],
                       stdout=subprocess.DEVNULL, check=True, timeout=20)


def smoke_linux(archive, image, work):
    binary = unpack(archive, work)
    create_test_certificates(work)
    with model_fixture(work) as fixture:
        docker = [
            "docker", "run", "--rm", "--network", f"container:{fixture}", "--user", "1000:1000",
            "--env", "HOME=/tmp", "--env", "HERMES_HOME=/tmp/hermes-profile",
            "--env", "HERMES_API_KEY=fixture", *mount(binary, "/usr/local/bin/hermes"),
        ]
        rejected = subprocess.run([
            *docker, image, "hermes", "chat", "--oneshot", "--model", "fixture",
            "--base-url", BASE_URL, "-q", "release smoke untrusted",
        ], capture_output=True, text=True, timeout=120)
        if rejected.returncode != 1 or "UnknownIssuer" not in rejected.stderr:
            raise RuntimeError(f"unexpected result without the test root: {rejected.stderr}")

        root = mount(work / "trusted.pem", "/etc/ssl/certs/ca-certificates.crt")
        result = subprocess.check_output([
            *docker, *root, image, "sh", "-ec", '''
                if command -v python || command -v python3; then exit 1; fi
                cd /tmp
                hermes --help >/dev/null
                answer=$(hermes chat --oneshot --save-session --model fixture --base-url "$1" \
                    -q 'release smoke first' 2>/tmp/hermes-session.log)
                test "$answer" = 'Ответ модели'
                session=$(sed -n 's/^Session: //p' /tmp/hermes-session.log)
                test -n "$session"
                answer=$(hermes chat --oneshot --resume "$session" --model fixture --base-url "$1" \
                    -q 'release smoke resumed')
                test "$answer" = 'Ответ модели'
                hermes sessions search '"release smoke resumed"'
            ''', "hermes-smoke", BASE_URL,
        ], text=True, timeout=120)
        hits = json.loads(result)
        if len(hits) != 1 or "release smoke resumed" not in hits[0]["snippet"]:
            raise RuntimeError(f"missing resumed turn in release FTS results: {result}")
    print(f"{archive.name}: checksum, startup, system TLS, session resume and FTS passed on {image}")


@contextmanager
def native_model():
    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        server.scenario = "tool"
        server.requests = []
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            yield server, f"http://127.0.0.1:{server.server_port}/v1"
        finally:
            server.shutdown()
            thread.join()


def smoke_native(archive, work):
    first = work / "installed CLI"
    first.mkdir()
    binary = unpack(archive, first)
    second = work / "second installation"
    second.mkdir()
    replacement = second / binary.name
    shutil.copy2(binary, replacement)
    home = work / "user profile"
    profile = home / ".hermes" / "profiles" / "fixture"
    profile.mkdir(parents=True)
    (profile / "config.yaml").write_text("model: fixture\n", encoding="utf-8")
    project = work / "project folder"
    project.mkdir()
    (project / "fixture.txt").write_text("native read_file payload", encoding="utf-8")

    env = {key: os.environ[key] for key in ("SystemRoot", "WINDIR", "TEMP", "TMP") if key in os.environ}
    env.update(HOME=str(home), HERMES_API_KEY="fixture", PATH=str(work / "empty-path"))
    if os.name == "nt":
        env.update(USERPROFILE=str(home), HOME=str(work / "unused shell home"))

    def cli(executable, *args):
        result = subprocess.run([str(executable), *args], cwd=project, env=env,
                                capture_output=True, encoding="utf-8", timeout=30)
        if result.returncode:
            raise RuntimeError(f"{executable.name} exited with {result.returncode}: {result.stderr}")
        return result

    cli(binary, "--help")
    with native_model() as (server, base_url):
        chat = ("-p", "fixture", "chat", "--oneshot", "--model", "fixture", "--base-url", base_url)
        saved = cli(binary, *chat, "--save-session", "-q", "native smoke first")
        assert saved.stdout.strip() == "Ответ модели" and '"read_file"' in saved.stderr
        session = next(line.removeprefix("Session: ") for line in saved.stderr.splitlines()
                       if line.startswith("Session: "))
        assert (profile / "state.db").is_file()
        assert not (home / ".hermes" / "state.db").exists()
        tool_result = json.loads(server.requests[1]["messages"][-1]["content"])
        assert "native read_file payload" in tool_result["content"]
        resumed = cli(replacement, *chat, "--resume", session, "-q", "native smoke resumed")
        assert resumed.stdout.strip() == "Ответ модели"
        hits = json.loads(cli(binary, "-p", "fixture", "sessions", "search", '"native smoke resumed"').stdout)
        assert len(hits) == 1 and hits[0]["session_id"] == session
    print(f"{archive.name}: checksum, startup, named profile, read_file, session resume and FTS passed")
    if sys.platform == "darwin":
        subprocess.run(
            [sys.executable, "-m", "unittest", "-v", "compat.test_file_tools.TerminalToolTest"],
            env={**os.environ, "HERMES_TEST_BINARY": str(binary)},
            check=True, timeout=300,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("image", nargs="?", help="Linux container image; omit for a native smoke test")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="hermes-cli-smoke-") as directory:
        archive = args.archive.resolve(strict=True)
        work = Path(directory)
        if args.image:
            smoke_linux(archive, args.image, work)
        else:
            smoke_native(archive, work)


if __name__ == "__main__":
    main()
