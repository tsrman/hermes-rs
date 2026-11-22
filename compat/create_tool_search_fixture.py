#!/usr/bin/env python3
"""Record the tool-search bridge (tool_search / tool_describe / tool_call) from the Python reference.

Usage: create_tool_search_fixture.py <reference checkout> <registry.json> <cases.json>
Run it with the reference interpreter. `registry.json` holds what the bridge reads from the
registry (each tool's toolset, the core tool names, the GUI-surface toolsets); `cases.json` holds,
for sample configurations over the tools the Rust CLI implements, the assembled bridge schemas,
search and describe results, and the outcome of tool_call resolution and validation.
"""

import json
import os
import sys
import tempfile
from pathlib import Path

# Tools the Rust CLI can offer when everything is switched on.
IMPLEMENTED = {"clarify", "read_file", "write_file", "patch", "search_files", "terminal", "skills_list",
               "skill_view", "web_search", "web_extract", "memory", "session_search", "todo_list",
               "process_manage", "skill_manage", "execute_code", "delegate_task"}
# Tools whose schema the Rust CLI records from the reference verbatim: only these may be deferred in
# the cases, since the catalog quotes their descriptions.
EVERY_TOOL = ["clarify", "memory", "process_manage", "session_search", "todo_list", "web_extract", "web_search"]

CONFIGS = [
    ("default", None),
    ("everything_deferred", {"defer": EVERY_TOOL}),
    ("some_deferred", {"defer": ["memory", "web_search", "todo_list"]}),
    ("nothing_deferred", {"defer": []}),
    ("disabled", {"enabled": "off"}),
    ("legacy_false", False),
    ("listing_off", {"listing": "off"}),
    ("tight_budget_names", {"defer": EVERY_TOOL, "listing_max_tokens": 200}),
    ("tiny_budget", {"defer": EVERY_TOOL, "listing_max_tokens": 200, "threshold_pct": 0.1}),
    ("scalar_defer", {"defer": "todo_list"}),
]

SEARCH_QUERIES = [
    ["web search"], ["terminal process"], ["past sessions history"], ["write file"], ["kill process"],
    ["memory"], ["skills"], ["todo list"], ["zzzqq"], ["search"], ["read a file from disk"],
    ["run shell command execute code python"], ["web", "file", "session"], ["  Web_Search  "],
    ["searching sessions", "tasks"], ["fts5 conversations"], ["a"], ["list"], ["wait for output"],
]
SEARCH_ARGS = [{"queries": q} for q in SEARCH_QUERIES] + [
    {"queries": ["web"], "limit": 1}, {"queries": ["file"], "limit": 100}, {"queries": ["file"], "limit": 0},
    {"queries": ["file"], "limit": "2"}, {"queries": ["file"], "limit": "x"}, {"queries": ["file"], "limit": 2.9},
    {"queries": ["file"], "limit": None}, {"queries": "web search"}, {"queries": []}, {"queries": [" ", ""]},
    {"queries": [None, 0, "web"]}, {"queries": ["a"] * 8}, {"queries": "x" * 3}, {"queries": 5}, {}, {"query": "web"},
]
DESCRIBE_ARGS = [
    {"names": ["todo_list"]}, {"names": "session_search"}, {"names": ["todo_list", "todo_list", "process_manage"]},
    {"names": ["clarify"]}, {"names": ["memory"]}, {"names": ["zzz"]}, {"names": ["delegate_task"]},
    {"names": ["vision_analyze", "todo_list", "nope"]}, {"names": ["connectors__gmail__send"]},
    {"names": ["send_message"]}, {"names": ["tool_search"]}, {"names": []}, {"names": [" "]}, {"names": 5}, {},
    {"names": [f"n{i}" for i in range(11)]}, {"names": ["computer_use"]}, {"names": ["browser_vault"]},
    {"names": ["kanban_show"]}, {"names": ["web_search", "terminal"]},
]
CALL_ARGS = [
    {"calls": [{"name": "todo_list", "arguments": {}}]},
    {"calls": [{"name": "todo_list", "arguments": {"todos": [{"id": "1", "content": "a", "status": "pending"}]}}]},
    {"calls": [{"name": "todo_list", "arguments": {"todos": [{"id": "1", "content": "a", "status": "done"}]}}]},
    {"calls": [{"name": "todo_list", "arguments": {"todos": [{"id": "1", "content": "a"}]}}]},
    {"calls": [{"name": "todo_list", "arguments": {"todos": "[{\"id\": \"1\", \"content\": \"a\", \"status\": \"pending\"}]"}}]},
    {"calls": [{"name": "todo_list", "arguments": {"todos": 5}}]},
    {"calls": [{"name": "todo_list", "arguments": {"merge": "true"}}]},
    {"calls": [{"name": "todo_list", "arguments": {"merge": "maybe"}}]},
    {"calls": [{"name": "todo_list", "arguments": "{\"merge\": true}"}]},
    {"calls": [{"name": "todo_list", "arguments": "not json"}]},
    {"calls": [{"name": "todo_list", "arguments": [1]}]},
    {"calls": [{"name": "todo_list"}]},
    {"calls": [{"name": "process_manage", "arguments": {}}]},
    {"calls": [{"name": "process_manage", "arguments": {"action": "list"}}]},
    {"calls": [{"name": "process_manage", "arguments": {"action": "explode"}}]},
    {"calls": [{"name": "process_manage", "arguments": {"action": "wait", "session_id": "x", "timeout": 0}}]},
    {"calls": [{"name": "process_manage", "arguments": {"action": "wait", "session_id": "x", "timeout": "5"}}]},
    {"calls": [{"name": "process_manage", "arguments": {"action": "wait", "session_id": "x", "timeout": "five"}}]},
    {"calls": [{"name": "process_manage", "arguments": {"action": "wait", "session_id": "x", "timeout": 2.5}}]},
    {"calls": [{"name": "session_search", "arguments": {"query": "x", "limit": "3", "sort": "newest"}}]},
    {"calls": [{"name": "session_search", "arguments": {"sort": "sideways"}}]},
    {"calls": [{"name": "clarify", "arguments": {}}]},
    {"calls": [{"name": "memory", "arguments": {}}]},
    {"calls": [{"name": "delegate_task", "arguments": {}}]},
    {"calls": [{"name": "nope", "arguments": {}}]},
    {"calls": [{"name": "tool_search", "arguments": {}}]},
    {"calls": [{"name": "connectors__gmail__send", "arguments": {}}]},
    {"calls": [{"name": "connectors__a__b", "arguments": {}}, {"name": "connectors__c__d", "arguments": {}}]},
    {"calls": [{"name": "todo_list", "arguments": {}}, {"name": "process_manage", "arguments": {"action": "list"}}]},
    {"calls": [{"name": "todo_list", "arguments": {}}, {"name": "todo_list", "arguments": {}}]},
    {"calls": []}, {"calls": "x"}, {"calls": "[{\"name\": \"todo_list\", \"arguments\": {}}]"},
    {"calls": {"name": "todo_list", "arguments": {}}}, {"calls": [5]}, {"calls": [{"arguments": {}}]},
    {"name": "todo_list", "arguments": {}}, {"name": "", "arguments": {}}, {},
    {"calls": [{"name": "computer_use", "arguments": {}}]}, {"calls": [{"name": "kanban_show", "arguments": {}}]},
]


def main():
    reference = Path(sys.argv[1]).resolve()
    registry_out = Path(sys.argv[2]).resolve()
    cases_out = Path(sys.argv[3]).resolve()
    home = tempfile.mkdtemp()
    os.environ["HERMES_HOME"] = home
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import model_tools
    from hermes_cli.tools_config import _get_platform_tools
    from tools import tool_search as ts
    from tools.registry import registry, tool_error
    from tools.tool_search_validation import validate_deferred_call_args
    import toolsets

    registry_out.write_text(json.dumps({
        "toolsets": {name: registry.get_entry(name).toolset for name in sorted(registry.get_all_tool_names())},
        "core": list(toolsets._HERMES_CORE_TOOLS),
        "direct_surface": sorted(ts._DIRECT_SURFACE_TOOLSETS),
    }, indent=1) + "\n", encoding="utf-8")

    enabled = sorted(_get_platform_tools({}, "cli"))
    all_defs = model_tools.get_tool_definitions(enabled_toolsets=enabled, quiet_mode=True,
                                                skip_tool_search_assembly=True)
    defs = [d for d in all_defs if d["function"]["name"] in IMPLEMENTED]
    assert {d["function"]["name"] for d in defs} == IMPLEMENTED, "reference lacks some implemented tools"

    def configure(raw):
        text = "" if raw is None else "tools:\n  tool_search: %s\n" % json.dumps(raw)
        (Path(home) / "config.yaml").write_text(text)
        try:
            from hermes_cli import config as cfg
            cfg.reset_config_cache() if hasattr(cfg, "reset_config_cache") else None
        except Exception:
            pass
        return ts.ToolSearchConfig.from_raw(raw)

    cases = {"assemble": [], "search": [], "describe": [], "call": []}
    for name, raw in CONFIGS:
        config = configure(raw)
        assembly = ts.assemble_tool_defs(defs, config=config)
        cases["assemble"].append({
            "name": name, "config": raw, "activated": assembly.activated, "tier": assembly.tier,
            "listing_form": assembly.listing_form, "deferred_count": assembly.deferred_count,
            "tools": assembly.tool_defs})

    for scenario, raw in (("everything_deferred", {"defer": EVERY_TOOL}), ("default", None),
                          ("limits", {"defer": EVERY_TOOL, "search_default_limit": 2, "max_search_limit": 3})):
        config = configure(raw)
        for args in SEARCH_ARGS:
            result = ts.dispatch_tool_search(args, current_tool_defs=defs, config=config)
            cases["search"].append({"config": raw, "arguments": args, "result": json.loads(result)})
        for args in DESCRIBE_ARGS:
            result = ts.dispatch_tool_describe(args, current_tool_defs=defs, config=config)
            cases["describe"].append({"config": raw, "arguments": args, "result": json.loads(result)})
        for args in CALL_ARGS:
            names = ts.session_tool_names(defs)
            name, call_args, err = ts.resolve_underlying_call(args, names)
            if err or not name:
                outcome = json.loads(tool_error(err or "tool_call could not be resolved"))
            elif name == ts.CONNECTOR_BATCH_SENTINEL:
                outcome = json.loads(tool_error("Connectors are not available in this session."))
            elif name not in ts.scoped_deferrable_names(defs):
                outcome = json.loads(tool_error(ts.scope_block_reason(name)))
            else:
                probe = ts.validate_deferred_call_args(name, call_args)
                outcome = json.loads(probe) if probe is not None else {"call": name, "arguments": call_args}
            cases["call"].append({"config": raw, "arguments": args, "outcome": outcome})
    cases_out.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print("wrote", cases_out)


if __name__ == "__main__":
    main()
