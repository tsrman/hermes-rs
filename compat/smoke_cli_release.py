"""Verify a packaged CLI and its system TLS roots in a clean Linux container."""

import argparse
import hashlib
import json
import subprocess
import tarfile
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path

from compat.test_tls import create_test_certificates

BASE_URL = "https://127.0.0.1:8011/v1"
FIXTURE = Path(__file__).with_name("model_server.py").resolve()


def mount(source, target):
    return ["--mount", f"type=bind,source={source},target={target},readonly"]


def unpack(archive, destination):
    with archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    checksum = archive.with_name(archive.name + ".sha256").read_text(encoding="ascii")
    if checksum != f"{digest}  {archive.name}\n":
        raise ValueError(f"checksum mismatch for {archive.name}")
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


def smoke(archive, image, work):
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("image")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="hermes-cli-smoke-") as directory:
        smoke(args.archive.resolve(strict=True), args.image, Path(directory))


if __name__ == "__main__":
    main()
