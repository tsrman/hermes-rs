#!/usr/bin/env python3
"""Record agent.stall_guards primitives from the Python reference.

Usage: create_stall_guards_corpus.py <reference checkout> <output.json>
"""

import json
import sys
from pathlib import Path

LONG_OK = "x" * 512
SHORT_OK = "x" * 511
LONG_JSON_A = json.dumps({"b": 2, "a": "v" * 520})
LONG_JSON_B = json.dumps({"a": "v" * 520, "b": 2}, indent=2)
BIG_ARGS = {f"key_{i:02d}": f"value_{i:02d}" for i in range(12)}

OBSERVE_SEQUENCES = [
    {
        "name": "short_consecutive_streak_4",
        "calls": [
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": SHORT_OK, "id": "c1", "failed": False},
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": SHORT_OK, "id": "c2", "failed": False},
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": SHORT_OK, "id": "c3", "failed": False},
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": SHORT_OK, "id": "c4", "failed": False},
        ],
    },
    {
        "name": "long_consecutive_streak_stubs_from_2nd",
        "calls": [
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": LONG_OK, "id": "c1", "failed": False},
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": LONG_OK, "id": "c2", "failed": False},
            {"tool": "read_file", "args": {"path": "a.txt"}, "result": LONG_OK, "id": "c3", "failed": False},
        ],
    },
    {
        "name": "long_failed_result_not_stubbed_but_noticed",
        "calls": [
            {"tool": "terminal", "args": {"command": "false"}, "result": LONG_OK, "id": "c1", "failed": True},
            {"tool": "terminal", "args": {"command": "false"}, "result": LONG_OK, "id": "c2", "failed": True},
            {"tool": "terminal", "args": {"command": "false"}, "result": LONG_OK, "id": "c3", "failed": True},
        ],
    },
    {
        "name": "poller_exempt_from_notice_not_stub",
        "calls": [
            {"tool": "process_manage", "args": {"action": "poll", "session_id": "p1"}, "result": LONG_OK, "id": "p1", "failed": False},
            {"tool": "process_manage", "args": {"action": "poll", "session_id": "p1"}, "result": LONG_OK, "id": "p2", "failed": False},
            {"tool": "process_manage", "args": {"action": "poll", "session_id": "p1"}, "result": LONG_OK, "id": "p3", "failed": False},
            {"tool": "mcp__svc__job_poll", "args": {"id": 1}, "result": "running", "id": "m1", "failed": False},
            {"tool": "mcp__svc__job_poll", "args": {"id": 1}, "result": "running", "id": "m2", "failed": False},
            {"tool": "mcp__svc__job_poll", "args": {"id": 1}, "result": "running", "id": "m3", "failed": False},
            {"tool": "mcp__svc__fetch_get_result", "args": {"id": 1}, "result": "pending", "id": "g1", "failed": False},
            {"tool": "mcp__svc__fetch_get_result", "args": {"id": 1}, "result": "pending", "id": "g2", "failed": False},
            {"tool": "mcp__svc__fetch_get_result", "args": {"id": 1}, "result": "pending", "id": "g3", "failed": False},
        ],
    },
    {
        "name": "canonical_args_and_json_result_hash",
        "calls": [
            {"tool": "search_files", "args": {"b": 2, "a": 1}, "result": LONG_JSON_A, "id": "s1", "failed": False},
            {"tool": "search_files", "args": {"a": 1, "b": 2}, "result": LONG_JSON_B, "id": "s2", "failed": False},
            {"tool": "search_files", "args": {"b": 2, "a": 1}, "result": LONG_JSON_A, "id": "s3", "failed": False},
        ],
    },
    {
        "name": "execute_code_strips_volatile_metadata",
        "calls": [
            {
                "tool": "execute_code",
                "args": {"code": "print(1)"},
                "result": json.dumps({"status": "success", "output": "1\n", "duration_seconds": 0.11, "kernel": {"pid": 1, "execution_count": 1}}),
                "id": "e1",
                "failed": False,
            },
            {
                "tool": "execute_code",
                "args": {"code": "print(1)"},
                "result": json.dumps({"status": "success", "output": "1\n", "duration_seconds": 0.25, "kernel": {"pid": 1, "execution_count": 2}}),
                "id": "e2",
                "failed": False,
            },
            {
                "tool": "execute_code",
                "args": {"code": "print(1)"},
                "result": json.dumps({"status": "success", "output": "1\n", "duration_seconds": 0.39, "kernel": {"pid": 1, "execution_count": 3}}),
                "id": "e3",
                "failed": False,
            },
        ],
    },
    {
        "name": "other_tool_keeps_duration_seconds",
        "calls": [
            {"tool": "terminal", "args": {"command": "echo 1"}, "result": json.dumps({"output": "1", "duration_seconds": 0.1}), "id": "t1", "failed": False},
            {"tool": "terminal", "args": {"command": "echo 1"}, "result": json.dumps({"output": "1", "duration_seconds": 0.2}), "id": "t2", "failed": False},
            {"tool": "terminal", "args": {"command": "echo 1"}, "result": json.dumps({"output": "1", "duration_seconds": 0.3}), "id": "t3", "failed": False},
        ],
    },
    {
        "name": "long_args_preview_truncated_and_empty_id",
        "calls": [
            {"tool": "read_file", "args": BIG_ARGS, "result": LONG_OK, "id": "", "failed": False},
            {"tool": "read_file", "args": BIG_ARGS, "result": LONG_OK, "id": "second", "failed": False},
        ],
    },
    {
        "name": "cycle_period_2_fires_at_3_laps",
        "calls": [
            {"tool": "read_file", "args": {"path": "a"}, "result": "ra", "id": "1", "failed": False},
            {"tool": "read_file", "args": {"path": "b"}, "result": "rb", "id": "2", "failed": False},
            {"tool": "read_file", "args": {"path": "a"}, "result": "ra", "id": "3", "failed": False},
            {"tool": "read_file", "args": {"path": "b"}, "result": "rb", "id": "4", "failed": False},
            {"tool": "read_file", "args": {"path": "a"}, "result": "ra", "id": "5", "failed": False},
            {"tool": "read_file", "args": {"path": "b"}, "result": "rb", "id": "6", "failed": False},
            {"tool": "read_file", "args": {"path": "a"}, "result": "ra", "id": "7", "failed": False},
        ],
    },
    {
        "name": "cycle_period_3_and_period_5_ignored",
        "calls": [
            {"tool": "read_file", "args": {"p": i}, "result": f"r{i}", "id": f"p3-{k}", "failed": False}
            for k, i in enumerate([1, 2, 3, 1, 2, 3, 1, 2, 3])
        ],
    },
    {
        "name": "cycle_period_5_not_detected",
        "calls": [
            {"tool": "read_file", "args": {"p": i}, "result": f"r{i}", "id": f"p5-{k}", "failed": False}
            for k, i in enumerate([1, 2, 3, 4, 5] * 3)
        ],
    },
    {
        "name": "cycle_all_pollers_exempt_mixed_not_exempt",
        "calls": [
            {"tool": "process_manage", "args": {"id": "a"}, "result": "ok", "id": "1", "failed": False},
            {"tool": "mcp__x__status_poll", "args": {"id": "b"}, "result": "ok", "id": "2", "failed": False},
            {"tool": "process_manage", "args": {"id": "a"}, "result": "ok", "id": "3", "failed": False},
            {"tool": "mcp__x__status_poll", "args": {"id": "b"}, "result": "ok", "id": "4", "failed": False},
            {"tool": "process_manage", "args": {"id": "a"}, "result": "ok", "id": "5", "failed": False},
            {"tool": "mcp__x__status_poll", "args": {"id": "b"}, "result": "ok", "id": "6", "failed": False},
            # Now a mixed cycle (poller + read_file): NOT exempt.
            {"tool": "process_manage", "args": {"id": "a"}, "result": "ok", "id": "7", "failed": False},
            {"tool": "read_file", "args": {"path": "c"}, "result": "rc", "id": "8", "failed": False},
            {"tool": "process_manage", "args": {"id": "a"}, "result": "ok", "id": "9", "failed": False},
            {"tool": "read_file", "args": {"path": "c"}, "result": "rc", "id": "10", "failed": False},
            {"tool": "process_manage", "args": {"id": "a"}, "result": "ok", "id": "11", "failed": False},
            {"tool": "read_file", "args": {"path": "c"}, "result": "rc", "id": "12", "failed": False},
        ],
    },
    {
        "name": "ordinals_through_14",
        "calls": [
            {"tool": "read_file", "args": {"path": "x"}, "result": "same", "id": f"o{i}", "failed": False}
            for i in range(1, 15)
        ],
    },
]

CONTINUE_INTENT_CASES = [
    "",
    "   ",
    "Let me now inspect the config file.",
    "Let me now inspect the config file:",
    "Let me now inspect the config file\u2026",
    "I'll now run the unit tests.",
    "I\u2019ll now check the logs",
    "I will now patch src/main.rs.",
    "Now I'll inspect the output.",
    "Now I\u2019ll inspect the output.",
    "Now I will check the database.",
    "Next, I inspect the schema.",
    "Next: I run cargo test.",
    "Good progress. Let me now verify the build.",
    "Let me now check the file. And here is the final answer.",
    "Let me check the file.",
    "I'll check it later.",
    "Next I will do it.",
    "Let me now " + ("a" * 100) + ".",
    "Let me now " + ("a" * 101) + ".",
    ("x" * 380) + " Let me now run it.",
    ("x" * 390) + " Let me now run it.",
    "Done.\nLet me now check the status.",
    "Let me now check\nsecond line",
    "LET ME NOW RUN THE TESTS.",
    "The user asked: let me now explain?",
]

DEGENERATE_FINAL_CASES = [
    ("", "hello"),
    ("   ", "hello"),
    ("?warming", "Check the server status"),
    ("!oops", "Check the server status"),
    (",then", "Check the server status"),
    (";next", "Check the server status"),
    (":abc", "Check the server status"),
    (":8080", "What port?"),
    (")tail", "hello"),
    ("]tail", "hello"),
    ("}tail", "hello"),
    ("?warming.", "Check the server status"),
    ("?warming!", "Check the server status"),
    ("$5", "How much?"),
    ("#123", "Which issue?"),
    ("-1", "What exit code?"),
    ("/tmp", "Where?"),
    (".env", "Which file?"),
    ("(a)", "Which option?"),
    (":)", "Thanks"),
    (";;", "What separator?"),
    ("42", "What is the answer?"),
    ("SQLite", "Which DB?"),
    ("Done.", "Run it"),
    ("пар", "Run the build"),
    ("пар", "Запусти сборку"),
    ("пар.", "Run the build"),
    ("你好", "Check status"),
    ("你好", "检查状态"),
    ("你好。", "Check status"),
    ("пар1", "Run the build"),
    ("а" * 24, "Run the build"),
    ("а" * 25, "Run the build"),
]

CONFIG_VALUES = [
    ("absent", None),
    ("bool_true", True),
    ("bool_false", False),
    ("null", None),
    ("int_0", 0),
    ("int_1", 1),
    ("str_empty", ""),
    ("str_false", "false"),
    ("list_empty", []),
]

INTENT_ACK_CASES = [
    ("absent_chat", False, None, "gpt-4o", "chat_completions"),
    ("absent_responses", False, None, "gpt-5.2-codex", "codex_responses"),
    ("auto_chat", True, "auto", "gpt-4o", "chat_completions"),
    ("auto_responses", True, "auto", "gpt-5.2-codex", "codex_responses"),
    ("bool_true", True, True, "gpt-4o", "chat_completions"),
    ("bool_false", True, False, "gpt-5.2-codex", "codex_responses"),
    ("str_always", True, "always", "gpt-4o", "chat_completions"),
    ("str_yes", True, "YES", "gpt-4o", "chat_completions"),
    ("str_on", True, "on", "gpt-4o", "chat_completions"),
    ("str_never", True, "never", "gpt-5.2-codex", "codex_responses"),
    ("str_no", True, "no", "gpt-5.2-codex", "codex_responses"),
    ("str_off", True, "OFF", "gpt-5.2-codex", "codex_responses"),
    ("str_unknown_chat", True, "weird", "gpt-4o", "chat_completions"),
    ("str_unknown_responses", True, "weird", "gpt-5.2-codex", "codex_responses"),
    ("list_match", True, ["deepseek", "qwen"], "DeepSeek-V3", "chat_completions"),
    ("list_miss", True, ["deepseek", "qwen"], "gpt-5.2-codex", "codex_responses"),
    ("list_empty", True, [], "gpt-5.2-codex", "codex_responses"),
    ("null_chat", True, None, "gpt-4o", "chat_completions"),
    ("null_responses", True, None, "gpt-5.2-codex", "codex_responses"),
]


def main() -> int:
    if len(sys.argv) != 3:
        sys.stderr.write("Usage: create_stall_guards_corpus.py <reference checkout> <output.json>\n")
        return 2
    ref = Path(sys.argv[1]).resolve()
    out_path = Path(sys.argv[2]).resolve()
    sys.path.insert(0, str(ref))

    from types import SimpleNamespace
    from agent.tool_guardrails import ToolCallGuardrailController
    from agent.agent_runtime_helpers import (
        intent_ack_continuation_mode,
        looks_like_degenerate_final,
        trailing_continue_intent,
    )

    observe_out = []
    for seq in OBSERVE_SEQUENCES:
        ctrl = ToolCallGuardrailController()
        results = []
        for call in seq["calls"]:
            obs = ctrl.observe_call(
                call["tool"], call["args"], call["result"],
                tool_call_id=call["id"], failed=call["failed"],
            )
            results.append({
                "tool": call["tool"],
                "args": call["args"],
                "result": call["result"],
                "id": call["id"],
                "failed": call["failed"],
                "notice": obs.notice,
                "stub": obs.stub,
                "kind": obs.kind,
            })
        observe_out.append({"name": seq["name"], "calls": results})

    continue_out = [
        {"text": text, "expected": trailing_continue_intent(text)}
        for text in CONTINUE_INTENT_CASES
    ]
    degenerate_out = [
        {"text": text, "user_message": user, "expected": looks_like_degenerate_final(text, user_message=user)}
        for text, user in DEGENERATE_FINAL_CASES
    ]
    config_out = []
    for label, val in CONFIG_VALUES:
        section = {} if label == "absent" else {"stall_guards": val}
        config_out.append({
            "label": label,
            "present": label != "absent",
            "value": val,
            "expected": bool(section.get("stall_guards", True)),
        })

    intent_ack_out = []
    for label, present, val, model, api_mode in INTENT_ACK_CASES:
        section = {"intent_ack_continuation": val} if present else {}
        agent = SimpleNamespace(
            _intent_ack_continuation=section.get("intent_ack_continuation", "auto"),
            model=model,
            api_mode=api_mode,
        )
        intent_ack_out.append({
            "label": label,
            "present": present,
            "value": val,
            "model": model,
            "is_responses": api_mode == "codex_responses",
            "expected": intent_ack_continuation_mode(agent),
        })

    payload = {
        "observe_sequences": observe_out,
        "continue_intent_cases": continue_out,
        "degenerate_final_cases": degenerate_out,
        "config_cases": config_out,
        "intent_ack_cases": intent_ack_out,
    }
    out_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(
        f"Wrote {len(observe_out)} observe sequences, {len(continue_out)} continue-intent cases, "
        f"{len(degenerate_out)} degenerate-final cases, {len(config_out)} config cases, "
        f"{len(intent_ack_out)} intent-ack cases to {out_path}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
