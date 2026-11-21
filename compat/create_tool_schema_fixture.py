#!/usr/bin/env python3
"""Record tool definitions from the Python reference.

Usage: create_tool_schema_fixture.py <reference checkout> <schema directory> <fixture.json>
Run it with the reference interpreter. Writes one `<name>.json` per ported tool, which the
Rust runtime includes at build time, and a fixture with the full default tool list in order.
"""

import json
import os
import sys
import tempfile
from pathlib import Path

PORTED = ["memory", "web_search", "web_extract", "todo_list", "session_search", "browser_cdp"]


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
    default = model_tools.get_tool_definitions(quiet_mode=True)
    fixture.write_text(json.dumps(default, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print([tool["function"]["name"] for tool in default])


if __name__ == "__main__":
    main()
