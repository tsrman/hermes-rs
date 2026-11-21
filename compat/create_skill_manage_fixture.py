#!/usr/bin/env python3
"""Record skill_manage behaviour from the Python reference.

Usage: create_skill_manage_fixture.py <reference checkout> <schema.json> <cases.json>
Run it with the reference interpreter. Each scenario seeds a skills directory, runs `skill_manage`
calls through the reference handler, and records every result with the visible file tree after it
(hidden files such as the usage sidecar and locks are left out; the skills directory is replaced by
`<skills>`). Writes the tool schema too, with the dynamic description of a wider index.
"""

import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path

SKILL = """---
name: {name}
description: {description}
---

# {title}

## When to Use

Use it when {name} is needed.

Steps:
1. Do the first thing.
2. Do the second thing.
"""
FULL = """---
name: {name}
description: Use when testing. Does the thing.
version: 1.0.0
author: Hermes Agent
license: MIT
metadata:
  hermes:
    tags: [test]
---

# {name}

## When to Use

Use it.

Run `grep` to look, then `cat` the file. See references/guide.md and references/missing.md.
"""


def skill(name, description="Use when testing. Does the thing.", title=None):
    return SKILL.format(name=name, description=description, title=title or name)


def create(name, content=None, **extra):
    return {"action": "create", "name": name, "content": content if content is not None else skill(name), **extra}


def batch(*ops, **top):
    return {"operations": list(ops), **top}


LONG_DESC = "Use when " + "x" * 80 + "."
# name, seed {relative path: text}, config yaml text, [calls]
SCENARIOS = [
    ("create_basic", {}, "", [
        batch(create("alpha")),
        batch(create("beta", skill("beta"), category="devops")),
        batch(create("gamma", FULL.format(name="gamma"))),
        batch(create("delta")),
        {"action": "create", "name": "flat-one", "content": skill("flat-one")},
        {"action": "create", "name": "flat-two", "content": skill("flat-two"), "category": "misc"},
    ]),
    ("create_errors", {"existing/SKILL.md": skill("existing")}, "", [
        batch(create("")),
        batch(create("Bad Name")),
        batch(create("UPPER")),
        batch(create("-lead")),
        batch(create("x" * 65)),
        batch(create("ok-name", category="a/b")),
        batch(create("ok-name", category="Bad")),
        batch(create("ok-name", "no frontmatter")),
        batch(create("ok-name", "---\nname: ok-name\n")),
        batch(create("ok-name", "---\n- a\n- b\n---\nbody\n")),
        batch(create("ok-name", "---\nname: ok-name\n---\nbody\n")),
        batch(create("ok-name", "---\ndescription: d\n---\nbody\n")),
        batch(create("ok-name", "---\nname: ok-name\ndescription: d\n---\n\n")),
        batch(create("ok-name", "   ")),
        batch(create("ok-name", skill("ok-name", description=LONG_DESC))),
        batch(create("ok-name", skill("ok-name") + "y" * 100001)),
        batch(create("existing")),
        batch({"action": "create", "name": "ok-name"}),
        batch({"action": "create", "name": "ok-name", "content": ""}),
        batch({"action": "create", "name": "ok-name", "new_string": "text", "file_content": "text"}),
        {"action": "create", "name": "ok-name"},
        {"action": "create", "name": ""},
    ]),
    ("wide_index", {}, "skills:\n  index_desc_limit: 120\n", [
        batch(create("wide", skill("wide", description=LONG_DESC))),
        batch(create("wide2", skill("wide2", description="Use when " + "x" * 200 + "."))),
    ]),
    ("patch", {"mine/SKILL.md": FULL.format(name="mine"), "mine/references/guide.md": "line one\nline two\nline two\n"}, "", [
        batch({"action": "patch", "name": "mine", "old_string": "Use it.", "new_string": "Use it well."}),
        batch({"action": "patch", "name": "mine", "old_string": "Use it well.", "new_string": ""}),
        batch({"action": "patch", "name": "mine", "old_string": "nothing like this", "new_string": "x"}),
        batch({"action": "patch", "name": "mine", "old_string": "line two", "new_string": "LINE", "file_path": "references/guide.md"}),
        batch({"action": "patch", "name": "mine", "old_string": "line two", "new_string": "LINE", "file_path": "references/guide.md", "replace_all": True}),
        batch({"action": "patch", "name": "mine", "old_string": "line one", "new_string": "first", "file_path": "references/guide.md"}),
        batch({"action": "patch", "name": "mine", "old_string": "x", "new_string": "y", "file_path": "references/absent.md"}),
        batch({"action": "patch", "name": "mine", "old_string": "x", "new_string": "y", "file_path": "../escape.md"}),
        batch({"action": "patch", "name": "mine", "old_string": "x", "new_string": "y", "file_path": "other/file.md"}),
        batch({"action": "patch", "name": "mine", "old_string": "x"}),
        batch({"action": "patch", "name": "mine", "new_string": "x"}),
        batch({"action": "patch", "name": "mine", "content": skill("mine"), "old_string": "a", "new_string": "b"}),
        batch({"action": "patch", "name": "mine", "content": skill("mine", description="Use when rewriting. Rewritten.")}),
        batch({"action": "patch", "name": "mine", "old_string": "---\nname: mine", "new_string": "name: mine"}),
        batch({"action": "patch", "name": "mine", "old_string": "Use when", "new_string": "Use " + "w" * 90 + " when"}),
        batch({"action": "patch", "name": "nope", "old_string": "a", "new_string": "b"}),
        {"action": "patch", "name": "mine", "old_string": "Rewritten", "new_string": "Changed"},
        {"action": "patch", "name": "mine", "content": skill("mine", description="Use when flat. Flat.")},
        {"action": "edit", "name": "mine", "content": skill("mine", description="Use when edited. Edited.")},
        {"action": "edit", "name": "mine", "content": "bad"},
        {"action": "edit", "name": "mine"},
        {"action": "patch", "name": "mine"},
    ]),
    ("files", {"tool/SKILL.md": skill("tool"), "tool/references/a.md": "a", "tool/scripts/run.sh": "#!/bin/sh\n"}, "", [
        batch({"action": "write_file", "name": "tool", "file_path": "references/new.md", "file_content": "hello"}),
        batch({"action": "write_file", "name": "tool", "file_path": "templates/deep/er/t.txt", "file_content": "deep"}),
        batch({"action": "write_file", "name": "tool", "file_path": "references/a.md", "file_content": "overwritten"}),
        batch({"action": "write_file", "name": "tool", "file_path": "references/empty.md", "file_content": ""}),
        batch({"action": "write_file", "name": "tool", "file_path": "notes/x.md", "file_content": "x"}),
        batch({"action": "write_file", "name": "tool", "file_path": "references", "file_content": "x"}),
        batch({"action": "write_file", "name": "tool", "file_path": "../x.md", "file_content": "x"}),
        batch({"action": "write_file", "name": "tool", "file_path": "references/../../x.md", "file_content": "x"}),
        batch({"action": "write_file", "name": "tool", "file_path": "", "file_content": "x"}),
        batch({"action": "write_file", "name": "tool", "file_path": "references/n.md"}),
        batch({"action": "write_file", "name": "tool", "file_path": "SKILL.md", "file_content": skill("tool", "Use when rewritten. Done.")}),
        batch({"action": "write_file", "name": "tool", "file_path": "tool/SKILL.md", "file_content": skill("tool", "Use when again. Done.")}),
        batch({"action": "write_file", "name": "tool", "file_path": "references/big.md", "file_content": "z" * 100001}),
        batch({"action": "write_file", "name": "missing", "file_path": "references/n.md", "file_content": "x"}),
        batch({"action": "remove_file", "name": "tool", "file_path": "references/new.md"}),
        batch({"action": "remove_file", "name": "tool", "file_path": "templates/deep/er/t.txt"}),
        batch({"action": "remove_file", "name": "tool", "file_path": "references/gone.md"}),
        batch({"action": "remove_file", "name": "tool", "file_path": "../x"}),
        batch({"action": "remove_file", "name": "tool"}),
        batch({"action": "remove_file", "name": "ghost", "file_path": "references/a.md"}),
        {"action": "write_file", "name": "tool", "file_path": "assets/flat.txt", "file_content": "flat"},
        {"action": "remove_file", "name": "tool", "file_path": "assets/flat.txt"},
        {"action": "write_file", "name": "tool", "file_path": "assets/flat.txt"},
    ]),
    ("batches", {"one/SKILL.md": skill("one"), "two/SKILL.md": skill("two")}, "", [
        batch(create("fresh"), {"action": "write_file", "name": "fresh", "file_path": "references/r.md", "file_content": "r"},
              {"action": "patch", "name": "one", "old_string": "first thing", "new_string": "1st thing"}),
        batch({"action": "patch", "name": "one", "old_string": "second thing", "new_string": "2nd thing"},
              {"action": "patch", "name": "two", "old_string": "absent", "new_string": "x"}),
        batch(create("will-vanish"), {"action": "write_file", "name": "will-vanish", "file_path": "bad/x.md", "file_content": "x"}),
        batch({"action": "write_file", "name": "one", "file_path": "references/x.md", "file_content": "1"},
              {"action": "write_file", "name": "one", "file_path": "references/x.md", "file_content": "2"}),
        batch({"action": "patch", "name": "one", "content": skill("one", "Use when a. A.")},
              {"action": "patch", "name": "one", "content": skill("one", "Use when b. B.")}),
        batch({"action": "patch", "name": "one", "old_string": "1st", "new_string": "first"},
              {"action": "patch", "name": "one", "old_string": "first", "new_string": "1st"}),
        batch({"action": "patch", "name": "one", "file_path": " references/p.md", "old_string": "a", "new_string": "b"}),
        batch(create("late"), create("late")),
        batch({"action": "write_file", "name": "later", "file_path": "references/a.md", "file_content": "1"}, create("later")),
        batch(*[{"action": "write_file", "name": "one", "file_path": f"references/f{i}.md", "file_content": "x"} for i in range(21)]),
        batch(*[{"action": "write_file", "name": "one", "file_path": f"references/f{i}.md", "file_content": "x"} for i in range(20)]),
        batch(),
        {"operations": "x"},
        batch("not a dict"),
        batch({"name": "one"}),
        batch({"action": "explode", "name": "one"}),
        batch({"action": "edit", "name": "one", "content": skill("one")}),
        batch({"action": "patch", "old_string": "a", "new_string": "b"}),
        batch({"action": "patch", "name": "two", "old_string": "a", "new_string": "b"}, {"action": "create", "content": "x"}),
        batch({"action": "patch", "old_string": "first", "new_string": "b"}, name="one"),
        batch(create("catted", category="bad/cat")),
        batch({"action": "write_file", "name": "one", "file_path": "references/misfiled.md", "content": "text"}),
        batch({"action": "create", "name": "misfiled", "file_content": "text"}),
        batch({"action": "patch", "name": "one", "old_string": "a", "file_content": "text"}),
    ]),
    ("delete", {"cat/old/SKILL.md": skill("old"), "plain/SKILL.md": skill("plain"), "hermes-agent/SKILL.md": skill("hermes-agent"),
                ".usage.json": json.dumps({"pinned-skill": {"pinned": True}, "locked-skill": {"locked": True}}),
                "pinned-skill/SKILL.md": skill("pinned-skill"), "locked-skill/SKILL.md": skill("locked-skill"),
                "umbrella/SKILL.md": skill("umbrella"), "gone/SKILL.md": skill("gone")}, "", [
        batch({"action": "delete", "name": "old"}),
        batch({"action": "delete", "name": "plain"}),
        batch({"action": "delete", "name": "plain"}),
        batch({"action": "delete", "name": "hermes-agent"}),
        batch({"action": "delete", "name": "pinned-skill"}),
        batch({"action": "delete", "name": "locked-skill"}),
        batch({"action": "delete", "name": "gone", "absorbed_into": "gone"}),
        batch({"action": "delete", "name": "gone", "absorbed_into": "nowhere"}),
        batch({"action": "delete", "name": "gone", "absorbed_into": "umbrella"}),
        batch({"action": "delete", "name": "umbrella"}, {"action": "delete", "name": "x"}),
        batch({"action": "delete"}),
        batch({"action": "delete", "name": "../etc"}),
        {"action": "delete", "name": "missing-flat"},
        {"action": "delete", "name": ""},
    ]),
    ("locks_and_lookup", {"cat/found/SKILL.md": skill("found"), "dirname/SKILL.md": skill("display-name"),
                          "twin-a/SKILL.md": skill("twin"), "twin-b/SKILL.md": skill("twin"),
                          "locked/SKILL.md": skill("locked"), ".usage.json": json.dumps({"locked": {"locked": True}})}, "", [
        batch({"action": "patch", "name": "found", "old_string": "first thing", "new_string": "1st"}),
        batch({"action": "patch", "name": "cat/found", "old_string": "second thing", "new_string": "2nd"}),
        batch({"action": "patch", "name": "display-name", "old_string": "first thing", "new_string": "1st"}),
        batch({"action": "patch", "name": "twin", "old_string": "first thing", "new_string": "1st"}),
        batch({"action": "patch", "name": "locked", "old_string": "first thing", "new_string": "1st"}),
        batch({"action": "write_file", "name": "locked", "file_path": "references/a.md", "file_content": "x"}),
        batch(create("locked")),
        {"action": "edit", "name": "locked", "content": skill("locked")},
        batch({"action": "patch", "name": "dirname", "old_string": "first thing", "new_string": "1st"}),
    ]),
]


def tree(root: Path):
    out = {}
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root)
        if any(part.startswith(".") for part in rel.parts) or not path.is_file():
            continue
        out[rel.as_posix()] = path.read_text(encoding="utf-8")
    return out


def normalize(value, root):
    if isinstance(value, dict):
        return {k: normalize(v, root) for k, v in value.items()}
    if isinstance(value, list):
        return [normalize(v, root) for v in value]
    if isinstance(value, str):
        return value.replace(str(root), "<skills>")
    return value


def main():
    reference = Path(sys.argv[1]).resolve()
    schema_out = Path(sys.argv[2]).resolve()
    cases_out = Path(sys.argv[3]).resolve()
    home = Path(tempfile.mkdtemp())
    os.environ["HERMES_HOME"] = str(home)
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import model_tools  # noqa: F401  (registers the tools)
    from tools import skill_manager_tool as smt
    from tools.registry import registry

    schema = registry.get_schema("skill_manage")
    schema_out.write_text(json.dumps(schema, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    wide = smt._skill_manage_description(120)

    cases = []
    skills = home / "skills"
    for name, seed, config, calls in SCENARIOS:
        shutil.rmtree(skills, ignore_errors=True)
        for rel, text in seed.items():
            target = skills / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8")
        (home / "config.yaml").write_text(config)
        try:
            from hermes_cli import config as cfg
            for attr in ("reset_config_cache", "clear_config_cache"):
                getattr(cfg, attr, lambda: None)()
        except Exception:
            pass
        steps = []
        for args in calls:
            raw = smt._skill_manage_from(dict(args))
            steps.append({"arguments": args, "result": normalize(json.loads(raw), skills), "tree": tree(skills)})
        cases.append({"name": name, "seed": seed, "config": config, "steps": steps})
    cases_out.write_text(json.dumps({"wide_description": wide, "cases": cases}, indent=1, ensure_ascii=False) + "\n",
                         encoding="utf-8")
    print("wrote", cases_out)


if __name__ == "__main__":
    main()
