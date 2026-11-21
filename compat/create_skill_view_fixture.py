#!/usr/bin/env python3
"""Record skills_list and skill_view results from the Python reference.

Usage: create_skill_view_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. A fixed set of skills is written to a skills directory
(and a profile `.env`), then each call runs through the reference functions; the skills
directory is replaced by `<skills>` and file lists are sorted (the reference lists them in
directory order).
"""

import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

PLAIN = "---\nname: {name}\ndescription: {desc}\n{extra}---\n\n# {name}\n\nBody of {name}.\n"


def skill(name, desc="Does a thing.", extra=""):
    return PLAIN.format(name=name, desc=desc, extra=extra)


FILES = {
    "alpha/SKILL.md": skill("alpha", "Root level skill.", "version: 1.0.0\n"),
    "devops/beta/SKILL.md": skill("beta", "Category skill.", "tags: [ops, deploy]\nrelated_skills: other-one, another\n"),
    "devops/deep/gamma/SKILL.md": skill("gamma", "Nested twice.",
                                       "metadata:\n  hermes:\n    tags:\n      - nested\n      - deep\n    related_skills: [beta]\ncompatibility: Needs nothing\n"),
    "linked/SKILL.md": skill("linked", "Has linked files.", "metadata:\n  custom: {a: 1}\n"),
    "linked/references/guide.md": "# Guide\n",
    "linked/references/notes.txt": "ignored by the glob\n",
    "linked/templates/a.yaml": "a: 1\n",
    "linked/templates/sub/b.json": "{}\n",
    "linked/templates/sub/c.bin": "x",
    "linked/assets/logo.png": "png",
    "linked/assets/deep/data.csv": "1,2\n",
    "linked/scripts/run.sh": "#!/bin/sh\n",
    "linked/scripts/tool.py": "print(1)\n",
    "linked/scripts/readme.txt": "no\n",
    "env/SKILL.md": skill("env", "Needs secrets.", (
        "required_environment_variables:\n  - name: HERMES_FIXTURE_ONE\n    prompt: Enter one\n    help: Get it at example.test\n"
        "  - HERMES_FIXTURE_SET\n  - name: HERMES_FIXTURE_OPT\n    optional: true\n"
        "  - name: not a name\n  - name: HERMES_FIXTURE_ONE\n    prompt: duplicate\n"
        "setup:\n  help: Setup help\n  collect_secrets:\n    - env_var: HERMES_FIXTURE_TWO\n      prompt: Enter two\n      provider_url: https://example.test/keys\n"
        "prerequisites:\n  env_vars: [HERMES_FIXTURE_LEGACY]\n"
        "required_credential_files:\n  - creds/token.json\n")),
    "ready/SKILL.md": skill("ready", "All set.", "required_environment_variables:\n  - HERMES_FIXTURE_SET\n"),
    "winonly/SKILL.md": skill("winonly", "Another OS.", "platforms: [windows]\n"),
    "disabled-one/SKILL.md": skill("disabled-one", "Disabled by config."),
    "templated/SKILL.md": "---\nname: templated\ndescription: Uses tokens.\n---\n\nDir ${HERMES_SKILL_DIR} and ${HERMES_SESSION_ID}.\n",
    "dirname/SKILL.md": skill("display-name", "Name differs from directory."),
    "bom/SKILL.md": "﻿" + skill("bom", "Starts with a BOM."),
    "twin/one/SKILL.md": skill("twin", "First twin."),
    "twin/two/SKILL.md": skill("twin", "Second twin."),
    ".env": "HERMES_FIXTURE_SET=value\nHERMES_FIXTURE_EMPTY=\n",
}
CALLS = [
    ("list", {}), ("list_category", {"category": "devops"}), ("list_unknown_category", {"category": "nothing"}),
    ("list_empty_category", {"category": ""}),
    ("view", {"name": "alpha"}), ("view", {"name": "beta"}), ("view", {"name": "devops/beta"}), ("view", {"name": "gamma"}),
    ("view", {"name": "linked"}), ("view", {"name": "env"}), ("view", {"name": "ready"}),
    ("view", {"name": "winonly"}), ("view", {"name": "disabled-one"}), ("view", {"name": "templated"}),
    ("view", {"name": "display-name"}), ("view", {"name": "dirname"}), ("view", {"name": "bom"}),
    ("view", {"name": "twin"}), ("view", {"name": "missing"}), ("view", {"name": "/etc/passwd"}),
    ("view", {"name": "../escape"}), ("view", {"name": "a/../b"}),
    ("view", {"name": "linked", "file_path": "references/guide.md"}),
    ("view", {"name": "linked", "file_path": "templates/sub/b.json"}),
    ("view", {"name": "linked", "file_path": "references/absent.md"}),
    ("view", {"name": "linked", "file_path": "references"}),
    ("view", {"name": "linked", "file_path": "../alpha/SKILL.md"}),
    ("view", {"name": "linked", "file_path": "SKILL.md"}),
    ("view", {"name": "winonly", "file_path": "SKILL.md"}),
]


def normalize(value, root):
    if isinstance(value, dict):
        return {k: normalize(v, root) for k, v in value.items()}
    if isinstance(value, list):
        items = [normalize(v, root) for v in value]
        return sorted(items) if items and all(isinstance(i, str) for i in items) else items
    if isinstance(value, str):
        return value.replace(str(root), "<skills>")
    return value


def main():
    reference = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]).resolve()
    home = Path(tempfile.mkdtemp())
    os.environ["HERMES_HOME"] = str(home)
    os.environ.pop("HERMES_FIXTURE_ONE", None)
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import model_tools  # noqa: F401
    from tools import skills_tool as st

    skills = home / "skills"
    for rel, text in FILES.items():
        target = (home / rel) if rel == ".env" else skills / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
    config = "skills:\n  disabled: [disabled-one]\n"
    (home / "config.yaml").write_text(config)
    cases = []
    for kind, args in CALLS:
        st.clear_skills_cache()
        raw = st.skills_list(**args) if kind.startswith("list") else st.skill_view(**args)
        cases.append({"kind": "list" if kind.startswith("list") else "view", "arguments": args,
                      "result": normalize(json.loads(raw), skills)})
    # With nothing installed.
    shutil.rmtree(skills)
    skills.mkdir()
    st.clear_skills_cache()
    cases.append({"kind": "empty", "arguments": {}, "result": json.loads(st.skills_list())})
    out.write_text(json.dumps({"files": FILES, "config": config, "cases": cases}, indent=1, ensure_ascii=False) + "\n",
                   encoding="utf-8")
    print("wrote", out)


if __name__ == "__main__":
    main()
