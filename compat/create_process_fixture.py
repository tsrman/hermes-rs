#!/usr/bin/env python3
"""Record process_manage behaviour from the Python reference.

Usage: create_process_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. Each scenario starts local background processes through
the reference registry and calls `process_manage`; the output holds the steps and the normalized
results. Process ids, pids, times and the working directory are replaced by placeholders, so the
Rust test replays the same steps with its own registry. Commands start with `exec` so the login
shell the reference wraps them in prints no `logout` line.
"""

import json
import os
import re
import sys
import tempfile
import time
from pathlib import Path

SECRET = "Authorization: Bearer abcdefghijklmnop12345678"
LOOP = "i=0; while [ $i -lt 300 ]; do echo \"line $i\"; i=$((i+1)); done"
# name, [steps]; a step is {"spawn": command} or {"call": arguments, "caller": optional owner suffix}
SCENARIOS = [
    ("quick_exit", [
        {"spawn": "exec sh -c 'echo hello; echo err >&2; exit 3'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 5}},
        {"call": {"action": "poll", "session_id": "$0"}},
        {"call": {"action": "log", "session_id": "$0"}},
        {"call": {"action": "list"}},
        {"call": {"action": "write", "session_id": "$0", "data": "x"}},
        {"call": {"action": "submit", "session_id": "$0", "data": "x"}},
        {"call": {"action": "close", "session_id": "$0"}},
        {"call": {"action": "kill", "session_id": "$0"}},
    ]),
    ("running_process", [
        {"spawn": "exec sleep 30"},
        {"call": {"action": "poll", "session_id": "$0"}},
        {"call": {"action": "write", "session_id": "$0", "data": "x"}},
        {"call": {"action": "submit", "session_id": "$0"}},
        {"call": {"action": "close", "session_id": "$0"}},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 1}},
        {"call": {"action": "list"}},
        {"call": {"action": "kill", "session_id": "$0"}},
        {"call": {"action": "poll", "session_id": "$0"}},
        {"call": {"action": "kill", "session_id": "$0"}},
        {"call": {"action": "list"}},
    ]),
    ("paging", [
        {"spawn": f"exec sh -c '{LOOP}'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 10}},
        {"call": {"action": "log", "session_id": "$0"}},
        {"call": {"action": "log", "session_id": "$0", "offset": 0, "limit": 5}},
        {"call": {"action": "log", "session_id": "$0", "offset": 295, "limit": 50}},
        {"call": {"action": "log", "session_id": "$0", "offset": -3, "limit": 10}},
        {"call": {"action": "log", "session_id": "$0", "offset": 1000, "limit": 5}},
        {"call": {"action": "log", "session_id": "$0", "limit": 0}},
        {"call": {"action": "log", "session_id": "$0", "limit": 2}},
        {"call": {"action": "poll", "session_id": "$0"}},
    ]),
    ("long_output_is_tailed", [
        {"spawn": "exec sh -c 'i=0; while [ $i -lt 400 ]; do echo \"row $i xxxxxxxxxxxxxxxxxxxx\"; i=$((i+1)); done'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 10}},
        {"call": {"action": "poll", "session_id": "$0"}},
        {"call": {"action": "kill", "session_id": "$0"}},
    ]),
    ("text_handling", [
        {"spawn": "exec sh -c 'printf \"\\033[31mred\\033[0m caf\\303\\251 \\342\\202\\254\\n\"; echo " + SECRET + "'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 10}},
        {"call": {"action": "poll", "session_id": "$0"}},
        {"call": {"action": "log", "session_id": "$0"}},
        {"call": {"action": "list"}},
    ]),
    ("line_separators", [
        {"spawn": "exec sh -c 'printf \"a\\rb\\nc\\f d\\n\\nlast\"'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 10}},
        {"call": {"action": "log", "session_id": "$0"}},
        {"call": {"action": "log", "session_id": "$0", "offset": 1, "limit": 2}},
    ]),
    ("signal_exit", [
        {"spawn": "exec sh -c 'kill -9 $$'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 10}},
        {"call": {"action": "poll", "session_id": "$0"}},
    ]),
    ("ownership_and_prefixes", [
        {"spawn": "exec sh -c 'echo mine'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 10}},
        {"call": {"action": "poll", "session_id": "$0"}, "caller": "other"},
        {"call": {"action": "list"}, "caller": "other"},
        {"call": {"action": "poll", "session_id": "$0~4"}},
        {"call": {"action": "poll", "session_id": "$0~p6"}},
        {"call": {"action": "poll", "session_id": "$0~3"}},
        {"call": {"action": "poll", "session_id": "proc_zzzzzzzz"}},
        {"call": {"action": "poll", "session_id": "nope"}},
        {"call": {"action": "poll", "session_id": 12345}},
    ]),
    ("wait_limits", [
        {"spawn": "exec sh -c 'echo quick'"},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 999}},
        {"call": {"action": "wait", "session_id": "$0", "timeout": 0}},
        {"call": {"action": "wait", "session_id": "$0", "timeout": -4}},
        {"call": {"action": "wait", "session_id": "$0"}},
        {"spawn": "exec sleep 30"},
        {"call": {"action": "wait", "session_id": "$1", "timeout": 1}},
        {"call": {"action": "kill", "session_id": "$1"}},
    ]),
    ("argument_errors", [
        {"call": {"action": "poll"}},
        {"call": {"action": "wait"}},
        {"call": {"action": "kill", "session_id": ""}},
        {"call": {"action": "handoff"}},
        {"call": {"action": "handoff", "session_id": "proc_abcdef123456", "data": "x"}},
        {"call": {"action": "explode", "session_id": "x"}},
        {"call": {}},
        {"call": {"action": "list"}},
    ]),
]


def normalize(value, ids, cwd):
    if isinstance(value, dict):
        out = {}
        for key, item in value.items():
            if key == "pid":
                out[key] = 0
            elif key == "uptime_seconds":
                out[key] = 0
            elif key == "started_at":
                out[key] = "<t>"
            elif key == "exited_at":
                out[key] = 0.0
            elif key == "cwd":
                out[key] = "<cwd>"
            else:
                out[key] = normalize(item, ids, cwd)
        return out
    if isinstance(value, list):
        return [normalize(item, ids, cwd) for item in value]
    if isinstance(value, str):
        for index, sid in enumerate(ids):
            value = value.replace(sid, f"<S{index}>")
        value = re.sub(r"No process with ID [0-9a-f]{3}$", "No process with ID <P3>", value)
        return re.sub(r"Uptime: \d+s", "Uptime: <n>s", value)
    return value


def substitute(value, ids):
    if isinstance(value, dict):
        return {key: substitute(item, ids) for key, item in value.items()}
    if isinstance(value, str):
        match = re.fullmatch(r"\$(\d+)(?:~(p?)(\d+))?", value)
        if match:
            sid = ids[int(match.group(1))]
            if match.group(3) is None:
                return sid
            hexpart = sid[len("proc_"):][:int(match.group(3))]
            return ("proc_" if match.group(2) else "") + hexpart
    return value


def main():
    reference = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]).resolve()
    home = tempfile.mkdtemp()
    os.environ["HERMES_HOME"] = home
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import warnings
    warnings.simplefilter("ignore")
    from hermes_state import SessionDB
    SessionDB(Path(home) / "state.db")
    from tools.process_registry import process_registry, _handle_process

    cwd = tempfile.mkdtemp()
    fixture = []
    for number, (name, steps) in enumerate(SCENARIOS):
        owner = f"t{number}"
        ids, recorded = [], []
        for step in steps:
            if "spawn" in step:
                session = process_registry.spawn_local(step["spawn"], cwd=cwd, task_id=owner, owner_task_id=owner)
                ids.append(session.id)
                recorded.append({"spawn": step["spawn"]})
                continue
            caller = owner if "caller" not in step else f"{owner}-{step['caller']}"
            raw = _handle_process(substitute(step["call"], ids), task_id=caller)
            recorded.append({"call": step["call"], "caller": step.get("caller"),
                             "result": normalize(json.loads(raw), ids, cwd)})
        fixture.append({"name": name, "owner": owner, "steps": recorded})
    process_registry.kill_all()
    out.write_text(json.dumps(fixture, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
