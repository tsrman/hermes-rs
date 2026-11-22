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
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parent.parent
BINARY = Path(os.environ.get("HERMES_TEST_BINARY", ROOT / "target/debug/hermes"))


AUTO_LOAD_CONFIG = """skills:
  auto_load: [pinned-demo, " pinned-demo ", missing-one, off-one, 7]
  disabled: [off-one]
  config:
    demo:
      path: ~/data
      flag: true
      count: 0
"""


def write_pinned_skills(root):
    """A pinned skill with supporting files, config values and a missing credential, plus a disabled one."""
    skill = root / "devtools" / "pinned-demo"
    for name, text in {
        "SKILL.md": """---
name: pinned-demo
description: Demo skill pinned by config
metadata:
  hermes:
    config:
      - key: demo.path
        description: Where the data lives
        default: ~/default
      - key: demo.flag
        description: A flag
      - key: demo.count
        description: A count
      - key: demo.unset
        description: Never set
      - key: demo.fallback
        description: Falls back
        default: [a, b]
required_environment_variables:
  - name: DEMO_PINNED_SECRET_XYZ
    prompt: Secret
    help: Get one from the demo site
---

# Pinned demo

Directory ${HERMES_SKILL_DIR}, session ${HERMES_SESSION_ID}, shell !`echo hi`.
""",
        "references/a.md": "a\n", "references/notes.txt": "n\n",
        "templates/t.yaml": "t: 1\n", "scripts/run.py": "print(1)\n", "assets/pic.bin": "x",
    }.items():
        path = skill / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    helper = root / "hermes-agent"
    helper.mkdir(parents=True)
    (helper / "SKILL.md").write_text("---\nname: hermes-agent\ndescription: Help\n---\n\nBody\n")
    other = root / "devtools" / "off-one"
    other.mkdir(parents=True)
    (other / "SKILL.md").write_text("---\nname: off-one\ndescription: Disabled\n---\n\nBody\n")


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
    parser.add_argument("--interactive", action="store_true",
                        help="reference without the `chat -q` marker (long skills guidance); the port still runs -q, "
                             "so only the skills wrapper is expected to differ")
    parser.add_argument("--auto-load", action="store_true", help="pin skills with skills.auto_load")
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
    seed = work / "seed"
    seed.mkdir()
    config = "memory:\n  memory_enabled: false\n  user_profile_enabled: false\n"
    if args.auto_load:
        config += AUTO_LOAD_CONFIG
        write_pinned_skills(seed / "skills")
    (seed / "config.yaml").write_text(config)

    def reference(url):
        command = [args.python, str(ROOT / "compat/run_reference_turn.py"), args.reference,
                   str(reference_home), url + "/v1", "--model", args.model, "--seed", str(seed)]
        if not args.interactive:
            command.append("--single-query")
        if args.auto_load:
            command.append("--context-files")
        subprocess.run(command, cwd=project, check=True, capture_output=True)

    port_home = work / "port-home"
    shutil.copytree(seed, port_home)

    def port(url):
        subprocess.run([str(BINARY), "chat", "-q", "Привет", "--oneshot", "--yolo", "--base-url", url + "/v1",
                        "--model", args.model, "--save-session"], cwd=project, check=True, capture_output=True,
                       env={**os.environ, "HERMES_HOME": str(port_home), "HERMES_API_KEY": "fixture"})

    expected = first_system_message(None, reference)
    actual = first_system_message(None, port)

    def normalize(text, home):
        text = text.replace(str(home), "<HOME>").replace(str(project), "<PROJECT>")
        text = re.sub(r"\n\n## Mid-turn user steering\n.*?\(replayed copies in earlier history are already handled\)\.\n",
                      "\n", text, flags=re.S)
        # A new session has no id when the port builds its prompt, so a pinned skill keeps the token.
        text = re.sub(r"\d{8}_\d{6}_[0-9a-f]{6}|\$\{HERMES_SESSION_ID\}", "<SESSION>", text)
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
