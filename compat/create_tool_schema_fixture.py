#!/usr/bin/env python3
"""Record tool definitions from the Python reference.

Usage: create_tool_schema_fixture.py <reference checkout> <schema directory> <fixture.json>
Run it with the reference interpreter. Writes one `<name>.json` per ported tool, which the
Rust runtime includes at build time, a fixture with the full default tool list in order,
`toolsets.json` (every toolset resolved to its tools) and `tool_search_defaults.json` (the
tools deferred behind the tool-search bridge by default) beside the schema directory, and
`toolset_cases.json` next to the fixture: which tools the CLI exposes for sample configs,
and `guidance.json`: the system-prompt paragraphs that go with optional tools.
"""

import json
import os
import sys
import tempfile
from pathlib import Path

PORTED = ["memory", "web_search", "web_extract", "todo_list", "session_search", "browser_cdp", "process_manage", "skill_manage", "skill_view", "skills_list", "patch", "search_files", "clarify"]


# Tools the Rust CLI implements; the cases only record these.
RUST_TOOLS = {"read_file", "write_file", "patch", "search_files", "terminal", "skills_list", "clarify",
              "skill_view", "web_search", "web_extract", "memory", "session_search"}
TOOLSET_CASES = {
    "default": {},
    "web_only": {"platform_toolsets": {"cli": ["web"]}},
    "terminal_and_file": {"platform_toolsets": {"cli": ["terminal", "file"]}},
    "composite": {"platform_toolsets": {"cli": ["hermes-cli"]}},
    "legacy_alias": {"platform_toolsets": {"cli": ["hermes"]}},
    "safe": {"platform_toolsets": {"cli": ["safe"]}},
    "empty": {"platform_toolsets": {"cli": []}},
    "explicit_mixed": {"platform_toolsets": {"cli": ["web", "memory", "skills", "todo"]}},
    "disabled_web": {"agent": {"disabled_toolsets": ["web"]}},
    "disabled_memory_list": {"agent": {"disabled_toolsets": ["memory", "skills"]}},
    "disabled_composite": {"agent": {"disabled_toolsets": ["debugging"]}},
    "disabled_json_string": {"agent": {"disabled_toolsets": "['memory']"}},
    "other_platform_ignored": {"platform_toolsets": {"telegram": ["web"]}},
    "unknown_name": {"platform_toolsets": {"cli": ["web", "no-such-toolset"]}},
}


def main():
    reference = Path(sys.argv[1]).resolve()
    schemas = Path(sys.argv[2]).resolve()
    fixture = Path(sys.argv[3]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import model_tools
    from tools.registry import registry

    schemas.mkdir(parents=True, exist_ok=True)
    for name in PORTED:
        schema = registry.get_schema(name)
        if schema is None:
            raise SystemExit(f"reference has no tool {name!r}")
        (schemas / f"{name}.json").write_text(
            json.dumps(schema, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    import toolsets
    from hermes_cli.tools_config import _get_platform_tools

    resolved = {name: sorted(toolsets.resolve_toolset(name)) for name in sorted(toolsets.TOOLSETS)}
    (schemas.parent / "toolsets.json").write_text(
        json.dumps(resolved, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    cases = []
    for name, config in TOOLSET_CASES.items():
        enabled = _get_platform_tools(config, "cli")
        tools = model_tools.get_tool_definitions(
            enabled_toolsets=sorted(enabled),
            disabled_toolsets=(config.get("agent") or {}).get("disabled_toolsets"), quiet_mode=True)
        cases.append({"name": name, "config": config,
                      "tools": sorted(t["function"]["name"] for t in tools
                                      if t["function"]["name"] in RUST_TOOLS)})
    (fixture.parent / "toolset_cases.json").write_text(
        json.dumps(cases, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    from hermes_cli.config_defaults import DEFAULT_CONFIG

    search_defaults = DEFAULT_CONFIG["tools"]["tool_search"]
    (schemas.parent / "tool_search_defaults.json").write_text(
        json.dumps({"defer": list(search_defaults["defer"])}, indent=1) + "\n", encoding="utf-8")
    from agent import prompt_builder

    guidance = {
        "memory": {
            f"{mem}-{user}": prompt_builder.build_memory_guidance(mem, user, skill_manage_available=False)
            for mem, user in ((True, True), (False, True), (True, False), (False, False))
        },
        "skills": prompt_builder.SKILLS_GUIDANCE,
        "session_search": prompt_builder.SESSION_SEARCH_GUIDANCE,
    }
    (fixture.parent / "guidance.json").write_text(
        json.dumps(guidance, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    default = model_tools.get_tool_definitions(quiet_mode=True)
    fixture.write_text(json.dumps(default, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print([tool["function"]["name"] for tool in default])


if __name__ == "__main__":
    main()
