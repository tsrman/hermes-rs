#!/usr/bin/env python3
"""Record LocalEnvironment persistent shell state behaviour from the Python reference.

Usage: create_local_persistent_corpus.py <reference checkout> <output.json>
Run with the reference interpreter (e.g. ~/ref/hermes-reference-venv/bin/python).
"""

import json
import os
import sys
import tempfile
from pathlib import Path

ROOT_TOKEN = "<ROOT>"

BOOL_VALUES = [
    None, "", "true", "TRUE", "True", "1", "yes", "YES", "Yes",
    "false", "FALSE", "0", "no", "NO", "off", "on", "2", "garbage",
]

SCENARIOS = [
    {
        "name": "export_overwrite_and_unset",
        "persistent": True,
        "steps": [
            {"command": "export DEMO_VAR=hello"},
            {"command": 'echo "v=$DEMO_VAR"'},
            {"command": "export DEMO_VAR=world"},
            {"command": 'echo "v=$DEMO_VAR"'},
            {"command": "unset DEMO_VAR"},
            {"command": 'echo "v=${DEMO_VAR:-unset}"'},
        ],
    },
    {
        "name": "special_chars_and_multiline",
        "persistent": True,
        "steps": [
            {"command": """export SPECIAL="a'b\\"c \\$x \\`echo no\\`" """},
            {"command": 'printf "%s\\n" "$SPECIAL"'},
            {"command": "export MULTI=$'line1\\nline2\\nline3'"},
            {"command": 'printf "%s\\n" "$MULTI"'},
        ],
    },
    {
        "name": "unexported_var_does_not_persist",
        "persistent": True,
        "steps": [
            {"command": 'LOCAL_ONLY=local_val; echo "same=$LOCAL_ONLY"'},
            {"command": 'echo "next=${LOCAL_ONLY:-gone}"'},
        ],
    },
    {
        "name": "path_modification_persists",
        "persistent": True,
        "steps": [
            {"command": 'export PATH="/tmp/hermes-custom-bin-test:$PATH"'},
            {"command": 'case ":$PATH:" in *:/tmp/hermes-custom-bin-test:*) echo yes;; *) echo no;; esac'},
        ],
    },
    {
        "name": "nonzero_exit_preserves_exports_and_exit_code",
        "persistent": True,
        "steps": [
            {"command": "export AFTER_FAIL=kept; (exit 7)"},
            {"command": 'echo "after=$AFTER_FAIL"'},
        ],
    },
    {
        "name": "explicit_exit_skips_dump_keeps_prior_state",
        "persistent": True,
        "steps": [
            {"command": "export BEFORE_EXIT=before"},
            {"command": "export DURING_EXIT=during; exit 5"},
            {"command": 'echo "before=$BEFORE_EXIT during=${DURING_EXIT:-gone}"'},
        ],
    },
    {
        "name": "set_eu_does_not_poison_next_command",
        "persistent": True,
        "steps": [
            {"command": "set -eu; export STRICT_OK=yes"},
            {"command": 'echo "strict=$STRICT_OK undef=${UNDEF_VAR}"'},
        ],
    },
    {
        "name": "excluded_session_and_harness_vars",
        "persistent": True,
        "steps": [
            {
                "command": (
                    "export HERMES_SESSION_ID=leaked_sid HERMES_RPC_TOKEN=leaked_rpc "
                    "HERMES_UI_SESSION_ID=ui1 HERMES_DELEGATED_CHILD_CONTEXT=1 "
                    "HERMES_CRON_SESSION=1 HERMES_KERNEL_DIR=/tmp/k "
                    "AI_AGENT=custom_agent HERMES_AGENT=custom_hermes "
                    "NORMAL_VAR=kept_normal"
                )
            },
            {
                "command": (
                    'echo "sid=${HERMES_SESSION_ID:-none} rpc=${HERMES_RPC_TOKEN:-none} '
                    'ui=${HERMES_UI_SESSION_ID:-none} child=${HERMES_DELEGATED_CHILD_CONTEXT:-none} '
                    'cron=${HERMES_CRON_SESSION:-none} kdir=${HERMES_KERNEL_DIR:-none} '
                    'ai=${AI_AGENT:-none} ha=${HERMES_AGENT:-none} norm=${NORMAL_VAR:-none}"'
                )
            },
        ],
    },
    {
        "name": "secret_shaped_export_persists_in_session",
        "persistent": True,
        "steps": [
            {"command": "export MY_CUSTOM_TOKEN=tok_value_998877"},
            {"command": 'echo "len=${#MY_CUSTOM_TOKEN} prefix=${MY_CUSTOM_TOKEN:0:4}"'},
        ],
    },
    {
        "name": "persistent_false_drops_exports_keeps_cwd",
        "persistent": False,
        "steps": [
            {"command": "mkdir -p sub && cd sub && export NO_PERSIST=nope && pwd"},
            {"command": 'echo "var=${NO_PERSIST:-gone}"; pwd'},
        ],
    },
    {
        "name": "persistent_true_with_cd",
        "persistent": True,
        "steps": [
            {"command": "mkdir -p sub && cd sub && export IN_SUB=yes"},
            {"command": 'echo "var=$IN_SUB"; pwd'},
        ],
    },
    {
        "name": "heredoc_and_no_trailing_newline",
        "persistent": True,
        "steps": [
            {"command": "printf 'no-newline'"},
            {"command": "cat <<'EOF'\nhello from heredoc\nEOF\nexport AFTER_HD=ok"},
            {"command": 'echo "hd=$AFTER_HD"'},
        ],
    },
    {
        "name": "function_defined_in_command_does_not_persist",
        "persistent": True,
        "steps": [
            {"command": "my_tmp_fn() { echo hi; }; my_tmp_fn"},
            {"command": "type my_tmp_fn >/dev/null 2>&1 && echo exists || echo missing"},
        ],
    },
    {
        "name": "exported_function_does_not_persist",
        "persistent": True,
        "steps": [
            {"command": "my_exp_fn() { echo hi-exp; }; export -f my_exp_fn; my_exp_fn"},
            {"command": "type my_exp_fn >/dev/null 2>&1 && echo exists || echo missing"},
        ],
    },
]


def main() -> int:
    if len(sys.argv) != 3:
        sys.stderr.write("Usage: create_local_persistent_corpus.py <reference checkout> <output.json>\n")
        return 2
    ref = Path(sys.argv[1]).resolve()
    out_path = Path(sys.argv[2]).resolve()
    sys.path.insert(0, str(ref))

    with tempfile.TemporaryDirectory() as home_dir:
        os.environ["HERMES_HOME"] = home_dir
        os.environ["TERMINAL_AUTO_SOURCE_BASHRC"] = "false"
        os.environ.pop("TERMINAL_SHELL_INIT_FILES", None)

        from tools.terminal_tool_config import _tenv_bool
        from tools.environments.local import LocalEnvironment

        bool_cases = []
        for raw in BOOL_VALUES:
            if raw is None:
                os.environ.pop("TERMINAL_LOCAL_PERSISTENT", None)
            else:
                os.environ["TERMINAL_LOCAL_PERSISTENT"] = raw
            bool_cases.append({
                "value": raw,
                "expected": _tenv_bool("TERMINAL_LOCAL_PERSISTENT", "true"),
            })
        os.environ.pop("TERMINAL_LOCAL_PERSISTENT", None)

        recorded_scenarios = []
        for scenario in SCENARIOS:
            with tempfile.TemporaryDirectory() as workdir:
                root = str(Path(workdir).resolve())
                env = LocalEnvironment(cwd=root, timeout=15, persistent=scenario["persistent"])
                try:
                    steps_out = []
                    for step in scenario["steps"]:
                        res = env.execute(step["command"], timeout=15)
                        output = (res.get("output") or "").replace(root, ROOT_TOKEN)
                        cwd = (env.cwd or "").replace(root, ROOT_TOKEN)
                        steps_out.append({
                            "command": step["command"],
                            "output": output,
                            "returncode": int(res.get("returncode", -1)),
                            "cwd": cwd,
                        })
                    recorded_scenarios.append({
                        "name": scenario["name"],
                        "persistent": scenario["persistent"],
                        "steps": steps_out,
                    })
                finally:
                    env.cleanup()

    payload = {
        "bool_cases": bool_cases,
        "scenarios": recorded_scenarios,
    }
    out_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Wrote {len(bool_cases)} bool cases and {len(recorded_scenarios)} scenarios to {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
