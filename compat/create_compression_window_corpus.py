#!/usr/bin/env python3
"""Record which messages the Python reference keeps at the head and the tail of a compression and which
span it summarizes, for `crates/hermes-runtime/tests/compression_window.rs`.

Usage: create_compression_window_corpus.py <reference checkout> <output.json>
Run it with the reference interpreter: `<venv>/bin/python -I compat/create_compression_window_corpus.py ...`.
No model is called: the window is planned by the compressor itself (`_plan_compaction_window` and
`_scan_window_handoffs`, the two phases of `compress()` that precede the summary call). Everything is synthetic.

A case is a `compression:` section, a window, a message list and the compressor state. Rows are stored packed (`pack`/`unpack`). Message contents are
stored as `[text, pad, pad_char]` (the text followed by `pad` copies of the character) and expanded by the test, which keeps the fixture small.
Each case records:

* `head`: `_protect_head_size`, the rows kept at the head;
* `gate`: `compress()` returns the history untouched when it has at most `head + 4` rows;
* `start`, `end`: `_compress_window` on the unpruned copy, the summarized span before the handoff scan;
* `full_start`, `full_end`: the same on the pruned copy that `compress()` really uses;
* `prune_clean`: the prune changed nothing, so the two agree by construction;
* `tail_start`: where the kept tail starts after `_scan_window_handoffs` (a summary beyond `end` moves it);
* `window_rows`: how many rows the summarizer would read (0 means `empty_post_handoff_window`);
* `threshold_tokens`, `tail_token_budget`: what the reference derived from the section and the window.

The port reads the unpruned copy; `full_*` only differ from `start`/`end` when old tool results were pruned.
"""

import json
import os
import random
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

# Filled from the reference in main(): the current and an old handoff prefix and the marker texts.
MARKERS = {}
SUMMARY_PORT = "[CONTEXT SUMMARY]:\nUse this handoff as historical context. "

# (compression YAML, window or None for the reference's fallback window)
SETTINGS = [
    ("{threshold_tokens: 3000}", 128000),
    ("{threshold_tokens: 4000, protect_first_n: 1, protect_last_n: 6}", 128000),
    ("{threshold_tokens: 6000, protect_first_n: 0, protect_last_n: 2}", 128000),
    ("{threshold_tokens: 5000, protect_first_n: 5, protect_last_n: 30}", 200000),
    ("{threshold_tokens: 2500, protect_last_n: 3}", None),
    ("{threshold_tokens: 4000, tail_mode: legacy}", 128000),
    ("{threshold_tokens: 8000, tail_mode: legacy, target_ratio: 0.5}", 128000),
    ("{threshold_tokens: 8000, tail_mode: legacy, target_ratio: 0.05}", 128000),
    ("{threshold_tokens: 4000, tail_mode: legacy, target_ratio: 0.9, protect_last_n: 4}", 64000),
    ("{threshold_tokens: 4000, min_tail_user_messages: 2}", 128000),
    ("{threshold_tokens: 3000, min_tail_user_messages: 3, protect_first_n: 2}", 128000),
    ("{threshold_tokens: 6000, tail_mode: legacy, target_ratio: 0.3, min_tail_user_messages: 2}", 128000),
    ("{threshold: 0.5}", 32000),
    ("{threshold_tokens: 1500}", 100000),
    # A budget so small that the rows the walk must keep cost more than it.
    ("{threshold_tokens: 60, tail_mode: legacy}", 100000),
    ("{threshold_tokens: 300, tail_mode: legacy, protect_last_n: 8}", 100000),
    ("{threshold_tokens: 800, tail_mode: legacy, protect_first_n: 0}", 100000),
    ("{threshold_tokens: 200, protect_last_n: 8, protect_first_n: 2}", 100000),
    # The share of the trigger that caps the lean budget applies above 2000 tokens only, and a legacy tail is
    # held to a fifth of the window, which is the fallback window when none is configured.
    ("{threshold_tokens: 2000}", 100000),
    ("{threshold_tokens: 2001}", 100000),
    ("{threshold_tokens: 60000, tail_mode: legacy, target_ratio: 0.8}", None),
    # A long head: the last user turn may sit inside it.
    ("{threshold_tokens: 3000, protect_first_n: 8}", 128000),
    ("{threshold_tokens: 3000, protect_first_n: 0, min_tail_user_messages: 2}", 128000),
]

# Sections only read by the config parser, with what the compressor makes of them.
VALUE_CASES = [
    "{}", "{tail_mode: lean}", "{tail_mode: legacy}", "{tail_mode: ' LEGACY '}", "{tail_mode: Legacy}",
    "{tail_mode: bogus}", "{tail_mode: ''}", "{tail_mode: null}", "{tail_mode: 1}", "{tail_mode: true}",
    "{target_ratio: 0.2}", "{target_ratio: 0.5}", "{target_ratio: 0.05}", "{target_ratio: 0.95}",
    "{target_ratio: 1}", "{target_ratio: 0}", "{target_ratio: -1}", "{target_ratio: '0.3'}",
    "{target_ratio: true}", "{target_ratio: abc}", "{target_ratio: null}", "{target_ratio: [0.3]}",
    "{min_tail_user_messages: 1}", "{min_tail_user_messages: 2}", "{min_tail_user_messages: 5}",
    "{min_tail_user_messages: 0}", "{min_tail_user_messages: -3}", "{min_tail_user_messages: '3'}",
    "{min_tail_user_messages: ' 4 '}", "{min_tail_user_messages: 2.0}", "{min_tail_user_messages: 2.5}",
    "{min_tail_user_messages: true}", "{min_tail_user_messages: abc}", "{min_tail_user_messages: null}",
    "{min_tail_user_messages: [2]}",
]


class Builder:
    """Synthetic histories. Tool results stay at or under 200 characters unless a case asks for more, so
    the reference's prune (which only touches longer ones) leaves the sizes alone."""

    def __init__(self, rng):
        self.rng = rng
        self.calls = 0
        self.serial = 0

    def text(self, size, cyr=False):
        self.serial += 1
        if size <= 0:
            return [f"m{self.serial}", 0, "x"]
        return [f"m{self.serial} ", size, self.rng.choice(["я", "语", "é"]) if cyr else "x"]

    def user(self, size, cyr=False):
        return {"role": "user", "content": self.text(size, cyr)}

    def assistant(self, size, cyr=False):
        return {"role": "assistant", "content": self.text(size, cyr)}

    def exchange(self, parallel=1, result_size=60, assistant_text=0):
        calls, results = [], []
        for _ in range(parallel):
            self.calls += 1
            ident = f"call_{self.calls}"
            calls.append(call(self.calls))
            results.append({"role": "tool", "tool_call_id": ident, "content": [f"r{self.calls} ", result_size, "y"]})
        head = {"role": "assistant", "content": self.text(assistant_text) if assistant_text else None, "tool_calls": calls}
        return [head, *results]

    def summary(self, kind):
        """A handoff row. `content` is expanded by `expand` from `{"summary": kind, "text": ...}`."""
        self.serial += 1
        if kind == "metadata":
            return {"role": "user", "content": "plain", "_compressed_summary": True}
        role = "assistant" if kind.startswith("merged") else "user"
        return {"role": role, "content": {"summary": kind, "text": f"S{self.serial}"}}


def history(builder, shape, budget_chars):
    """A message list for one shape. `budget_chars` is about the size of the tail budget so that the
    cut falls somewhere inside the list instead of at an end."""
    rng, b = builder.rng, builder
    rows = []
    unit = max(40, budget_chars // 6)
    cyr = shape.endswith("_cyr")
    shape = shape.removesuffix("_cyr")

    def turn(tools=0, parallel=1, big=False):
        rows.append(b.user(rng.randint(10, unit) * (3 if big else 1), cyr))
        for _ in range(tools):
            rows.extend(b.exchange(parallel, rng.randint(20, 150), rng.choice([0, 0, 30])))
        rows.append(b.assistant(rng.randint(10, unit), cyr))

    if shape == "chat":
        for _ in range(rng.randint(2, 14)):
            turn()
    elif shape == "tools":
        for _ in range(rng.randint(2, 8)):
            turn(tools=rng.randint(0, 3))
    elif shape == "big_results":
        # Old tool results over 200 characters are pruned by the reference before it plans the window.
        for _ in range(rng.randint(2, 7)):
            rows.append(b.user(rng.randint(10, unit)))
            for _ in range(rng.randint(1, 3)):
                rows.extend(b.exchange(rng.randint(1, 2), rng.randint(250, max(400, budget_chars // 2))))
            rows.append(b.assistant(rng.randint(10, unit)))
    elif shape == "parallel":
        for _ in range(rng.randint(2, 6)):
            turn(tools=rng.randint(1, 2), parallel=rng.randint(2, 4))
    elif shape == "long_user":
        for index in range(rng.randint(3, 8)):
            turn(tools=rng.randint(0, 1), big=index == rng.randint(0, 5))
        rows.insert(rng.randint(0, len(rows)), b.user(budget_chars * 3, cyr))
    elif shape == "long_tail_assistant":
        for _ in range(rng.randint(2, 6)):
            turn(tools=rng.randint(0, 2))
        rows.append(b.user(30))
        rows.append(b.assistant(budget_chars * 2, cyr))
    elif shape == "many_small":
        for _ in range(rng.randint(15, 60)):
            rows.append(b.user(rng.randint(1, 40)))
            rows.append(b.assistant(rng.randint(1, 40)))
    elif shape == "summary_head":
        rows.append(b.summary(rng.choice(["port", "current", "historical", "metadata", "merged", "merged_header", "ended", "ended_live"])))
        for _ in range(rng.randint(2, 8)):
            turn(tools=rng.randint(0, 2))
    elif shape == "summary_middle":
        for _ in range(rng.randint(2, 6)):
            turn(tools=rng.randint(0, 2))
        rows.append(b.summary(rng.choice(["port", "current", "metadata", "ended", "merged"])))
        for _ in range(rng.randint(1, 6)):
            turn(tools=rng.randint(0, 2))
    elif shape == "summary_tail":
        for _ in range(rng.randint(3, 8)):
            turn(tools=rng.randint(0, 2))
        rows.append(b.summary(rng.choice(["port", "current", "ended_live"])))
        for _ in range(rng.randint(0, 2)):
            turn()
    elif shape == "developer_row":
        for _ in range(rng.randint(3, 8)):
            turn(tools=rng.randint(0, 1))
        rows.insert(rng.randint(1, len(rows) - 1), {"role": rng.choice(["system", "developer"]), "content": "note"})
    elif shape == "assistant_first":
        rows.append(b.assistant(20))
        for _ in range(rng.randint(3, 8)):
            turn(tools=rng.randint(0, 2))
    elif shape == "orphan_tool_head":
        rows.extend(b.exchange(2)[1:])
        for _ in range(rng.randint(3, 8)):
            turn(tools=rng.randint(0, 2))
    elif shape == "nudges":
        for _ in range(rng.randint(3, 7)):
            turn(tools=rng.randint(0, 2))
        rows.insert(rng.randint(1, len(rows)), {"role": "user", "content": "[IMPORTANT: Background process p1 finished]"})
        rows.insert(rng.randint(1, len(rows)), {"role": "user", "content": "   "})
    elif shape == "long_agentic":
        # One or two requests, then a long run of tool exchanges: the last user row is far from the end.
        for _ in range(rng.randint(1, 2)):
            rows.append(b.user(rng.randint(10, unit), cyr))
            for _ in range(rng.randint(4, 12)):
                rows.extend(b.exchange(rng.choice([1, 1, 2, 3]), rng.randint(20, 150), rng.choice([0, 0, 30])))
    elif shape == "agentic_text":
        # Assistant replies carry text and calls both, in a turn that outgrows the tail budget.
        rows.append(b.assistant(rng.randint(10, unit), cyr))
        rows.append(b.user(rng.randint(10, unit)))
        for _ in range(rng.randint(5, 12)):
            rows.extend(b.exchange(rng.choice([1, 2]), rng.randint(20, 150), rng.randint(unit // 2, unit * 2)))
    elif shape == "summary_then_tools":
        for _ in range(rng.randint(1, 3)):
            turn(tools=rng.randint(0, 2))
        rows.append(b.summary(rng.choice(["port", "current", "ended", "ended_live", "merged"])))
        for _ in range(rng.randint(1, 4)):
            rows.extend(b.exchange(rng.choice([1, 2]), rng.randint(20, 150)))
    elif shape == "fuzz":
        # Random rows of every kind, some of them as large as the whole budget, with handoffs anywhere.
        for _ in range(rng.randint(6, 40)):
            pick = rng.random()
            size = int(budget_chars * rng.uniform(0.5, 3)) if rng.random() < 0.06 else rng.randint(5, unit * 2)
            if pick < 0.30:
                rows.append(b.user(size, cyr))
            elif pick < 0.45:
                rows.append(b.assistant(size, cyr))
            elif pick < 0.92:
                rows.extend(b.exchange(rng.choice([1, 1, 2, 3]), rng.randint(20, 150) if size < unit * 2 else size,
                                       size if rng.random() < 0.4 else 0))
            else:
                rows.append(b.summary(rng.choice(["port", "current", "ended", "ended_live", "merged", "metadata"])))
    elif shape == "summary_sandwich":
        # Handoffs at the start, live rows, a newer handoff, then a short tail: the cut can fall between them.
        for _ in range(rng.randint(1, 3)):
            rows.append(b.summary(rng.choice(["port", "current", "ended"])))
        for _ in range(rng.randint(1, 3)):
            rows.append(rng.choice([b.user, b.assistant])(rng.randint(5, unit * 2), cyr))
        rows.append(b.summary(rng.choice(["port", "current"])))
        for _ in range(rng.randint(0, 3)):
            turn(tools=rng.randint(0, 1))
    elif shape == "handoffs_only":
        # Nothing but handoffs before a short live tail: the window the cut leaves may hold only handoffs.
        for _ in range(rng.randint(3, 6)):
            rows.append(b.summary(rng.choice(["port", "current", "ended", "metadata", "historical", "merged_empty", "merged_header"])))
        for _ in range(rng.randint(1, 3)):
            rows.append(rng.choice([b.user, b.assistant])(rng.randint(5, unit), cyr))
    elif shape == "tool_user_gap":
        # A user message in the middle of a tool run (an interjection) and tool rows that end the list.
        for _ in range(rng.randint(2, 5)):
            turn(tools=rng.randint(1, 2))
        rows.append(b.user(rng.randint(10, unit)))
        rows.extend(b.exchange(rng.randint(1, 3)))
    return rows


SHAPES = ["chat", "tools", "parallel", "long_user", "long_tail_assistant", "many_small", "summary_head",
          "summary_middle", "summary_tail", "developer_row", "assistant_first", "orphan_tool_head", "nudges",
          "tool_user_gap", "big_results", "long_agentic", "agentic_text", "summary_then_tools", "summary_sandwich", "handoffs_only", "fuzz", "chat_cyr", "tools_cyr", "parallel_cyr"]

# How a case ends: a pending user query (preflight, the query is the last row), the user's query followed by
# tool exchanges (mid-turn), or the history as it stands.
ENDINGS = ["query", "turn_open", "turn_open_big", "as_is"]


def expand(content):
    if isinstance(content, dict) and "summary" in content:
        kind, text, m = content["summary"], content["text"], MARKERS
        if kind == "port":
            return SUMMARY_PORT + text
        body = m["historical"] if kind == "historical" else m["current"]
        if kind.startswith("merged"):
            prior = {"merged": "kept request", "merged_header": m["header"] + "\nkept request",
                     "merged_empty": m["header"]}[kind]
            return prior + "\n\n" + m["delimiter"] + "\n" + body + " " + text
        if kind == "ended":
            return body + " " + text + "\n\n" + m["end"]
        if kind == "ended_live":
            return body + " " + text + "\n\n" + m["end"] + "\nlive follow-up"
        return body + " " + text
    if isinstance(content, list) and len(content) == 3 and isinstance(content[1], int):
        return content[0] + content[2] * content[1]
    return content


ROLE_CODES = {"u": "user", "a": "assistant", "s": "system", "d": "developer"}


def call(number):
    """The tool call `Builder.exchange` makes for call number `number`."""
    return {"id": f"call_{number}", "type": "function",
            "function": {"name": "read_file", "arguments": json.dumps({"path": f"f{number}.txt"})}}


def pack(row):
    """A message row in the fixture's compact form; the test unpacks it the same way (`unpack`)."""
    keys = set(row)
    codes = {role: code for code, role in ROLE_CODES.items()}
    if row["role"] in codes and keys == {"role", "content"}:
        return [codes[row["role"]], row["content"]]
    if row["role"] == "assistant" and keys == {"role", "content", "tool_calls"}:
        numbers = [int(c["id"].removeprefix("call_")) for c in row["tool_calls"]]
        if row["tool_calls"] == [call(n) for n in numbers]:
            return ["a", row["content"], numbers]
    if row["role"] == "tool" and keys == {"role", "content", "tool_call_id"}:
        return ["t", row["content"], int(row["tool_call_id"].removeprefix("call_"))]
    return ["x", row]


def unpack(item):
    code = item[0]
    if code == "x":
        return item[1]
    if code == "t":
        return {"role": "tool", "tool_call_id": f"call_{item[2]}", "content": item[1]}
    row = {"role": ROLE_CODES[code], "content": item[1]}
    if len(item) == 3:
        row["tool_calls"] = [call(n) for n in item[2]]
    return row


def expanded(rows):
    out = []
    for row in rows:
        row = dict(row)
        row["content"] = expand(row.get("content"))
        out.append(row)
    return out


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    from agent.agent_init import _parse_compression_config
    from agent.context_compressor import ContextCompressor, _estimate_msg_budget_tokens
    from agent.model_metadata import DEFAULT_FALLBACK_CONTEXT
    import hermes_yaml as yaml
    import session_rules.turns as turns

    MARKERS.update(current=turns.SUMMARY_PREFIX, historical=turns._HISTORICAL_SUMMARY_PREFIXES[2],
                   end=turns._SUMMARY_END_MARKER, delimiter=turns._MERGED_SUMMARY_DELIMITER,
                   header=turns._MERGED_PRIOR_CONTEXT_HEADER)

    def compressor_for(section, window):
        config = {"compression": yaml.safe_load(section)}
        agent = SimpleNamespace(model="m", provider="custom")
        cs = _parse_compression_config(agent, config)
        return ContextCompressor(
            model="m", threshold_percent=cs.threshold, protect_first_n=cs.protect_first, protect_last_n=cs.protect_last,
            summary_target_ratio=cs.target_ratio, quiet_mode=True, config_context_length=window, provider="custom",
            threshold_tokens_cap=cs.threshold_tokens, min_tail_user_messages=cs.min_tail_users, tail_mode=cs.tail_mode)

    values = []
    for section in VALUE_CASES:
        config = {"compression": yaml.safe_load(section)}
        try:
            cs = _parse_compression_config(SimpleNamespace(model="m", provider="custom"), config)
            comp = ContextCompressor(model="m", summary_target_ratio=cs.target_ratio, quiet_mode=True,
                                     config_context_length=128000, provider="custom",
                                     min_tail_user_messages=cs.min_tail_users, tail_mode=cs.tail_mode)
            result = {"tail_mode": comp.tail_mode, "target_ratio": comp.summary_target_ratio,
                      "min_tail_users": comp.min_tail_user_messages}
        except Exception as error:  # the reference refuses a config it cannot coerce
            result = {"error": type(error).__name__}
        values.append({"compression": section, **result})

    cases = []
    rng = random.Random(20261123)
    for section, window in SETTINGS:
        effective_window = window or DEFAULT_FALLBACK_CONTEXT
        probe = compressor_for(section, effective_window)
        budget = probe.tail_token_budget
        budget_chars = budget * 4
        for shape in SHAPES:
            for repeat in range({"fuzz": 8, "summary_sandwich": 6, "handoffs_only": 6}.get(shape, 2)):
                builder = Builder(rng)
                rows = history(builder, shape, budget_chars)
                ending = rng.choice(ENDINGS)
                if ending == "query":
                    rows.append(builder.user(rng.randint(5, 200)))
                elif ending.startswith("turn_open"):
                    rows.append(builder.user(rng.randint(5, 200) * (30 if ending.endswith("big") else 1)))
                    for _ in range(rng.randint(1, 6)):
                        rows.extend(builder.exchange(rng.choice([1, 1, 2]), rng.randint(20, 150)))
                system = [{"role": "system", "content": ["system prompt", rng.choice([0, 400]), "s"]}]
                messages = system + rows
                after_pass = rng.random() < 0.15
                case = {"compression": section, "window": window, "ending": ending, "shape": shape,
                        "after_pass": after_pass, "messages": [pack(row) for row in messages]}
                assert [unpack(item) for item in case["messages"]] == messages
                record(case, section, effective_window, compressor_for, expanded(messages), after_pass, SimpleNamespace(
                    estimate=_estimate_msg_budget_tokens))
                if not case["blank_echoes"]:  # the reference would renumber rows; leave such cases out
                    cases.append(case)
    output.write_text(json.dumps({
        "markers": MARKERS,
        "cases": cases,
        "values": values,
        "fallback_window": DEFAULT_FALLBACK_CONTEXT,
        "prune_cases": record_prune_cases(compressor_for),
        "placement_cases": record_placement_cases(ContextCompressor),
    }, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    kinds = {}
    for case in cases:
        key = "noop" if case["full_noop"] else "window"
        kinds[key] = kinds.get(key, 0) + 1
    print({"cases": len(cases), "values": len(values), "outcomes": kinds,
           "pruned_differs": sum(1 for c in cases if not c["prune_clean"] and (c["start"], c["end"]) != (c["full_start"], c["full_end"]))})


def record_prune_cases(compressor_for):
    """Record `_prune_old_tool_results` outputs across tool types, dedup, spared pending rounds, and pass-4 pressure."""
    long_text = "alpha line\n" * 30  # > 200 chars

    def make_pair(cid, name, args, content):
        return [
            {"role": "assistant", "content": None, "tool_calls": [
                {"id": cid, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}
            ]},
            {"role": "tool", "tool_call_id": cid, "content": content},
        ]

    # 1. Per-tool stub shapes + dedup + protected vs unprotected skill_view + refused calls.
    msgs = [{"role": "system", "content": "sys"}, {"role": "user", "content": "start"}]
    msgs += make_pair("c1", "read_file", {"path": "src/a.rs", "offset": 10}, long_text)
    msgs += make_pair("c2", "read_file", {"path": "src/a.rs", "offset": 10}, long_text)  # duplicate of c1 -> c1 deduped
    msgs += make_pair("c3", "write_file", {"path": "out.txt", "content": "a\nb\nc"}, json.dumps({"bytes_written": 300, "padding": "w" * 220}))
    msgs += make_pair("c4", "write_file", {"path": "bad.txt", "content": "a"}, json.dumps({"error": "permission denied", "padding": "e" * 220}))
    msgs += make_pair("c5", "terminal", {"command": "cargo test --workspace"}, json.dumps({"exit_code": 0, "output": "ok\n" * 100}))
    msgs += make_pair("c6", "terminal", {"command": "false && echo nope"}, json.dumps({"exit_code": 1, "output": "line1\nFAILED: assertion error\n" + " " * 200}))
    msgs += make_pair("c7", "search_files", {"pattern": "foo", "path": "crates", "target": "content"}, json.dumps({"total_count": 7, "matches": "s" * 220}))
    msgs += make_pair("c8", "patch", {"path": "lib.rs", "mode": "replace"}, json.dumps({"ok": True, "diff": "p" * 220}))
    msgs += make_pair("c9", "web_search", {"query": "rust async"}, json.dumps({"results": "ws" * 120}))
    msgs += make_pair("c10", "web_extract", {"urls": ["https://a.example", "https://b.example"]}, json.dumps({"text": "we" * 120}))
    msgs += make_pair("c11", "delegate_task", {"goal": "audit module"}, json.dumps({"summary": "d1" * 120}))
    msgs += make_pair("c12", "delegate_task", {"tasks": [{"goal": "first"}, {"goal": "second"}]}, json.dumps({"results": "d2" * 120}))
    msgs += make_pair("c13", "execute_code", {"code": "print('hello')\n" * 6}, "out\n" * 80)
    msgs += make_pair("c14", "skill_view", {"name": "old-skill"}, "skill body " + "sk" * 120)
    msgs += make_pair("c15", "skills_list", {"category": "dev"}, json.dumps({"count": 4, "skills": "sl" * 120}))
    msgs += make_pair("c16", "skill_manage", {"operations": [{"action": "create", "name": "s1"}, {"action": "patch", "name": "s2"}]}, json.dumps({"ok": True, "pad": "sm" * 120}))
    msgs += make_pair("c17", "memory", {"action": "add", "target": "user"}, json.dumps({"ok": True, "pad": "mm" * 120}))
    msgs += make_pair("c18", "todo_list", {"todos": []}, json.dumps({"todos": "td" * 120}))
    msgs += make_pair("c19", "clarify", {"question": "Pick"}, json.dumps({"responses": [{"status": "answered", "user_response": "Option A"}], "pad": "cl" * 120}))
    msgs += make_pair("c20", "process_manage", {"action": "poll", "session_id": "p1"}, json.dumps({"exit_code": 2, "output": "pm" * 120}))
    msgs += make_pair("c21", "cronjob_manage", {"action": "run"}, json.dumps({"success": True, "job": {"execution_error": "boom"}, "pad": "cj" * 120}))
    msgs += make_pair("c22", "browser_navigate", {"url": "https://example.test"}, "html " + "bn" * 120)
    msgs += make_pair("c23", "custom_mcp", {"a": "val1", "b": 42}, json.dumps({"result": "cm" * 120}))
    msgs += make_pair("c24", "terminal", {"command": "rm -rf /"}, json.dumps({"status": "blocked", "error": "BLOCKED: NOT consented " + "bl" * 120}))
    msgs += make_pair("c25", "skill_view", {"name": "recent-skill"}, "recent skill body " + "z" * 220)
    # Tail turns (recent-skill is within last 10 messages so it stays protected).
    for k in range(3):
        msgs.append({"role": "user", "content": f"tail user {k}"})
        msgs.append({"role": "assistant", "content": f"tail reply {k}"})

    # 2. Spared pending tool round vs oversized pending round + pass-4 pressure demotion.
    pressure_msgs = [{"role": "system", "content": "sys"}, {"role": "user", "content": "u0"}]
    for k in range(5):
        pressure_msgs += make_pair(f"p{k}", "read_file", {"path": f"p{k}.txt"}, f"body-{k} " + "q" * 1600)
    pressure_msgs.append({"role": "assistant", "content": "done"})
    pressure_msgs.append({"role": "user", "content": "next"})

    pending_spared = [{"role": "system", "content": "sys"}, {"role": "user", "content": "u0"}]
    pending_spared += make_pair("s0", "read_file", {"path": "old.txt"}, "old " + "a" * 800)
    pending_spared += make_pair("s1", "read_file", {"path": "pending.txt"}, "pending " + "b" * 800)

    pending_huge = [{"role": "system", "content": "sys"}, {"role": "user", "content": "u0"}]
    pending_huge += make_pair("h0", "read_file", {"path": "old.txt"}, "old " + "a" * 400)
    pending_huge += make_pair("h1", "read_file", {"path": "huge.txt"}, "huge " + "c" * 12000)

    out = []
    for label, section, window, case_msgs in [
        ("all_tools_and_dedup", "{threshold_tokens: 200, tail_mode: legacy, protect_last_n: 6}", 128000, msgs),
        ("pressure_tail_demote", "{threshold_tokens: 400, tail_mode: legacy, target_ratio: 0.5, protect_last_n: 8}", 8000, pressure_msgs),
        ("pending_round_spared", "{threshold_tokens: 400, tail_mode: legacy, target_ratio: 0.25, protect_last_n: 2}", 8000, pending_spared),
        ("pending_round_oversized", "{threshold_tokens: 400, tail_mode: legacy, target_ratio: 0.25, protect_last_n: 2}", 4000, pending_huge),
    ]:
        comp = compressor_for(section, window)
        pruned, count = comp._prune_old_tool_results(
            case_msgs, protect_tail_count=comp.protect_last_n, protect_tail_tokens=comp.tail_token_budget,
        )
        out.append({
            "label": label,
            "compression": section,
            "window": window,
            "messages": case_msgs,
            "pruned_count": count,
            "contents": [m.get("content") for m in pruned],
        })
    return out


def record_placement_cases(cls):
    """Record `_summary_placement` and `_merge_summary_into_tail_row` across head/tail role shapes."""
    u = lambda text="hi": {"role": "user", "content": text}
    a = lambda text="ok": {"role": "assistant", "content": text}
    s = lambda text="sys": {"role": "system", "content": text}
    at = lambda: {"role": "assistant", "content": None, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]}
    t = lambda: {"role": "tool", "tool_call_id": "c1", "content": "res"}

    cases = []
    grids = [
        ("empty_head_user_tail", [], [u()], 0),
        ("empty_head_asst_tail", [], [a(), u()], 0),
        ("sys_head_user_tail", [s()], [u()], 1),
        ("sys_head_asst_tail", [s()], [a(), u()], 1),
        ("sys_head_exempt_then_user", [s()], [at(), t(), u()], 1),
        ("sys_head_exempt_only", [s()], [at(), t()], 1),
        ("user_head_user_tail", [s(), u()], [u()], 2),
        ("user_head_asst_tail", [s(), u()], [a(), u()], 2),
        ("user_head_exempt_then_asst", [s(), u()], [at(), t(), a(), u()], 2),
        ("user_head_exempt_only", [s(), u()], [at(), t()], 2),
        ("asst_head_user_tail", [s(), u(), a()], [u()], 3),
        ("asst_head_asst_tail", [s(), u(), a()], [a(), u()], 3),
        ("asst_head_exempt_then_asst", [s(), u(), a()], [at(), t(), a(), u()], 3),
        ("asst_head_exempt_only", [s(), u(), a()], [at(), t()], 3),
        ("exempt_head_user_tail", [at(), t()], [u()], 2),
        ("exempt_head_asst_tail", [at(), t()], [a()], 2),
        ("no_user_anywhere", [s(), a()], [a()], 2),
        ("blank_user_only", [s(), u("   ")], [a()], 2),
        ("user_head_empty_tail", [s(), u()], [], 2),
        ("asst_head_empty_tail", [s(), u(), a()], [], 3),
    ]
    comp = cls.__new__(cls)
    comp._summary_has_user_turn = True
    for label, head, tail, start in grids:
        role, merge, force_lead, first_vis = cls._summary_placement(head, tail, start)
        merged_preview = None
        if merge and tail:
            target_idx = first_vis if force_lead and first_vis is not None else 0
            copy = dict(tail[target_idx])
            comp._merge_summary_into_tail_row(copy, "SUM", role, force_lead)
            merged_preview = {"target_idx": target_idx, "role": copy["role"], "content": copy["content"]}
        cases.append({
            "label": label,
            "head": head,
            "tail": tail,
            "compress_start": start,
            "summary_role": role,
            "merge_into_tail": merge,
            "force_user_leading": force_lead,
            "first_tail_visible_idx": first_vis,
            "merged_preview": merged_preview,
        })
    return cases


def record(case, section, window, compressor_for, messages, after_pass, _helpers):
    comp = compressor_for(section, window)
    if after_pass:
        comp.compression_count = 1
    case["threshold_tokens"] = comp.threshold_tokens
    case["tail_token_budget"] = comp.tail_token_budget
    case["head"] = comp._protect_head_size(messages)
    case["gate"] = len(messages) <= case["head"] + 4
    unpruned = comp._drop_blank_echoes(messages)
    start, end = comp._compress_window(unpruned)
    plan, pruned_count, full_start, full_end = comp._plan_compaction_window(messages)
    case["start"], case["end"] = start, end
    case["full_start"], case["full_end"] = full_start, full_end
    case["prune_clean"] = pruned_count == 0 and len(plan) == len(messages)
    case["blank_echoes"] = len(unpruned) != len(messages)
    case["noop"] = True
    case["tail_start"] = case["window_rows"] = None
    case["full_noop"] = True
    case["full_tail_start"] = case["full_window_rows"] = None
    if case["gate"] or case["blank_echoes"]:
        return
    if start < end:
        scan = comp._scan_window_handoffs(unpruned, start, end, unpruned[start:end])
        case["tail_start"] = scan.tail_start
        case["window_rows"] = len(scan.turns_to_summarize)
        case["noop"] = not scan.turns_to_summarize
    if full_start < full_end:
        full_comp = compressor_for(section, window)
        if after_pass:
            full_comp.compression_count = 1
        full_scan = full_comp._scan_window_handoffs(plan, full_start, full_end, plan[full_start:full_end])
        case["full_tail_start"] = full_scan.tail_start
        case["full_window_rows"] = len(full_scan.turns_to_summarize)
        case["full_noop"] = not full_scan.turns_to_summarize


if __name__ == "__main__":
    main()
