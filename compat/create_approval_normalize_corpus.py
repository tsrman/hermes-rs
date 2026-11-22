#!/usr/bin/env python3
"""Record the reference's detection-time normalization for the approval detector's port.

Usage: create_approval_normalize_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. For every command of `create_approval_corpus.py` plus the extras
below it records `_normalize_command_for_detection` and `_lower_preserving_flags(normalized)` in two
environments: `plain` (HOME=/home/user, HERMES_HOME unset) and `custom` (HERMES_HOME=/opt/hermes-data).
Windows spellings are out of scope for the port, so none are recorded.
"""

import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from create_approval_corpus import CORPUS  # noqa: E402

EXTRA = [
    "\x1b[31mrm\x1b[0m -rf /", "rm\x00 -rf /", "ｒｍ -rf /", "ﬁle", "rm -rf \\\n/", "rm -rf \\\r\n/", "a\\\\\nb",
    "r\\m -rf /", "r''m", "r\"\"m", "echo ''x''", "echo \"\"\"\"", "rm${IFS}-rf${IFS}/", "rm$IFS-rf$IFS/", "rm${IFS:0:1}-rf${IFS:0:1}/",
    "echo $IFSx ${IFSx}", "echo \\ x\\", "echo a\\\nb\\\\nc", "\x1b]0;title\x07ls", "\x1b]0;title\x1b\\ls", "\x9b31mls", "\x85ls",
    "cat /home/user/.bashrc", "cat /home/user/.ssh/id_rsa", "echo x > /home/user/.hermes/config.yaml", "cat /home/user/My\\ Docs/x",
    "cat '/home/user/My Docs/x'", "cat /home/user/", "cat /home/user", "cat /home/username/x", "cat x/home/user/y", "ls /home/user ",
    "cat ~/.hermes/.env", "cat $HOME/.hermes/config.yaml", "cat ${HOME}/.hermes/.env", "cat $HERMES_HOME/.env",
    "cat /opt/hermes-data/config.yaml", "mv /opt/hermes-data /tmp/x", "mv /opt/hermes-data/ /tmp/x", "cat /opt/hermes-data-old/x",
    "cat /opt/hermes-data/profiles/dev/.env", "cat /opt/hermes-data\\ x/y", "cat /opt/hermes-data/My\\ Docs/y",
    "echo x > /home/user/.hermes/profiles/a/.env", "rm -rf /home/user/.hermes", "rm -rf /home/user/.hermes/",
    "Cat /Home/User/.bashrc", "-X --Long Flag -rF Rm /ETC", "RM -RF /Tmp -f", "ls\t-L\n-A  /X", "AéB -É", "",
]
ENVIRONMENTS = {"plain": None, "custom": "/opt/hermes-data"}


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools.approval_detection import _lower_preserving_flags, _normalize_command_for_detection  # noqa: E402

    commands = list(dict.fromkeys(c for rows in CORPUS.values() for c in rows)) + EXTRA
    rows = []
    for name, hermes_home in ENVIRONMENTS.items():
        os.environ["HOME"] = "/home/user"
        os.environ.pop("HERMES_HOME", None)
        os.environ.pop("HERMES_DATA_DIR_SUFFIX", None)
        if hermes_home:
            os.environ["HERMES_HOME"] = hermes_home
        for command in commands:
            normalized = _normalize_command_for_detection(command)
            rows.append({"env": name, "command": command, "normalized": normalized,
                         "lowered": _lower_preserving_flags(normalized)})
    body = ",\n".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) for r in rows)
    Path(sys.argv[2]).write_text("[\n" + body + "\n]\n", encoding="utf-8")
    print(f"{len(rows)} rows")


if __name__ == "__main__":
    main()
