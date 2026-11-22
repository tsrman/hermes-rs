#!/usr/bin/env python3
"""Compare the first system message of the reference agent and of the Rust CLI in the same project.

Usage: compare_prompt.py <reference checkout> <reference python> [--model NAME]
Both run against the local model server in a temporary git repository with a Cargo.toml (so the
coding posture and the workspace snapshot appear). Known, intended differences are removed before
the comparison: the mid-turn steering section (the CLI has no steering channel) and the profile
home path. Prints a unified diff and exits 1 when anything else differs.
"""
import argparse
import difflib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parent.parent
BINARY = Path(os.environ.get("HERMES_TEST_BINARY", ROOT / "target/debug/hermes"))


def first_system_message(server_args, run):
    server = subprocess.Popen([sys.executable, str(ROOT / "compat/model_server.py"), "--scenario", "text"],
                              stdout=subprocess.PIPE, text=True)
    try:
        url = server.stdout.readline().strip()
        run(url)
        with urlopen(url + "/requests", timeout=5) as response:
            return json.load(response)[0]["messages"][0]["content"]
    finally:
        server.kill()
        server.wait()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("reference")
    parser.add_argument("python")
    parser.add_argument("--model", default="fixture")
    parser.add_argument("--layout", choices=("repo", "plain", "dirty", "node"), default="repo")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp())
    project = work / "project"
    project.mkdir()
    if args.layout != "plain":
        (project / "Cargo.toml").write_text("[package]\nname = 'demo'\n")
        (project / "main.rs").write_text("fn main() {}\n")
        env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
               "GIT_COMMITTER_EMAIL": "t@t"}
        if args.layout == "node":
            (project / "package.json").write_text(json.dumps({"scripts": {"test": "x", "lint": "y", "dev": "z"}}))
            (project / "yarn.lock").write_text("")
            (project / "Makefile").write_text("build:\n\tcc\ncheck : build\n")
            (project / "pytest.ini").write_text("")
            (project / "AGENTS.md").write_text("Be nice.\n")
        for command in (["git", "init", "-q", "-b", "main"], ["git", "add", "."],
                        ["git", "commit", "-qm", "first commit"]):
            subprocess.run(command, cwd=project, env=env, check=True)
        if args.layout == "dirty":
            (project / "main.rs").write_text("fn main() { }\n")
            (project / "new.rs").write_text("")
            (project / "staged.rs").write_text("")
            subprocess.run(["git", "add", "staged.rs"], cwd=project, env=env, check=True)

    reference_home = work / "reference-home"

    def reference(url):
        subprocess.run([args.python, str(ROOT / "compat/run_reference_turn.py"), args.reference,
                        str(reference_home), url + "/v1", "--model", args.model], cwd=project, check=True, capture_output=True)

    port_home = work / "port-home"
    port_home.mkdir()
    (port_home / "config.yaml").write_text(
        "memory:\n  memory_enabled: false\n  user_profile_enabled: false\n")

    def port(url):
        subprocess.run([str(BINARY), "chat", "-q", "Привет", "--oneshot", "--yolo", "--base-url", url + "/v1",
                        "--model", args.model], cwd=project, check=True, capture_output=True,
                       env={**os.environ, "HERMES_HOME": str(port_home), "HERMES_API_KEY": "fixture"})

    expected = first_system_message(None, reference)
    actual = first_system_message(None, port)

    def normalize(text, home):
        text = text.replace(str(home), "<HOME>").replace(str(project), "<PROJECT>")
        text = re.sub(r"\n\n## Mid-turn user steering\n.*?\(replayed copies in earlier history are already handled\)\.\n",
                      "\n", text, flags=re.S)
        text = re.sub(r"Conversation started: .*", "Conversation started: <DATE>", text)
        text = re.sub(r"(Host|Model|Provider): .*", r"\1: <X>", text)
        return text

    left = normalize(expected, reference_home).splitlines()
    right = normalize(actual, port_home).splitlines()
    diff = list(difflib.unified_diff(left, right, "reference", "rust", lineterm="", n=1))
    print("\n".join(diff) if diff else "identical after normalisation")
    sys.exit(1 if diff else 0)


if __name__ == "__main__":
    main()
