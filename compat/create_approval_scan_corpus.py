#!/usr/bin/env python3
"""Record the reference's shell-scanner primitives for the approval detector's port.

Usage: create_approval_scan_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. For every command of `create_approval_corpus.py` plus the extras
below it records `expand_ansi_c_quotes`, `substitution_depth` and the `_scan_shell` steps under four
option sets (steps are skipped above 400 chars), so the Rust scanner is checked below the verdict level.
"""

import json
import os
import sys
from pathlib import Path

os.environ["HOME"] = "/home/user"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from create_approval_corpus import CORPUS  # noqa: E402

EXTRA = [
    "echo $'\\x72\\x6d' -rf /", "$'rm' -rf $'/'", "echo $'it\\'s'; ls", "echo $\"locale\"", "echo $$'x'",
    "echo \"$'not ansi'\"", "echo \"$(echo $'\\x41')\"", "echo `echo $'\\x41'`", "x=$'\\u00e9\\U0001F600\\ud800'",
    "echo $'\\0abc'", "echo $'\\400'", "echo $'\\cA\\c'", "echo $'\\q\\?\\\"'", "echo $'unterminated",
    "echo $\\\n'x'", "echo $'a\\\nb'", "echo # comment $'x'\nls", "echo a#b $'x'", "echo \\# $'x'",
    "echo 'a'\"b\"`c`$(d)${e}", "echo \"a $(b \"c\") d\"", "echo `a\\`b`", "echo `unterminated", "echo $(unterminated",
    "echo ${unterminated", "echo '$(x)' \"$(y)\" `z`", "echo $(( 1 + (2) ))", "echo $(a $(b $(c)))",
    "cat <<EOF\n$(x)\nEOF\nls", "cat <<'EOF'\n$(x $(y))\nEOF\n$(z)", "cat <<-\"EOF\"\n\t$(x)\n\tEOF", "cat <<\\EOF\n$(a)\nEOF",
    "cat <<A <<'B'\n$(1)\nA\n$(2 $(3))\nB", "cat <<E\na\\\nE\n$(x)\nE", "echo <<< $(x)", "echo $((1<<'E'))",
    "echo $(echo 'a)b') c", "echo $(echo \"a)b\") c", "echo $(echo \\) ) c", "echo $(# c )\n)", "echo a;#b\necho c",
    "echo \u00e9\u4e2d\U0001F600 'ü' \"ß\"", "echo \u2028 # x", "a\\\nb", "echo \\", "'", "\"", "`", "$(", "${",
    "echo \"\\\"\" '\\' \\' ", "ls $(pwd)/reboot", "echo $(printf '%s' ')') ls",
]
OPTION_SETS = {
    "uq_brace": dict(subst="uq", brace=True),
    "q_naive": dict(subst="q", naive_backtick=True),
    "comments": dict(subst="", comments=True),
    "u_stop": dict(subst="u", stop_unterminated=True),
}
MAX_STEP_LEN = 400


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools.approval_ansi_c import expand_ansi_c_quotes  # noqa: E402
    from tools.approval_parser_budget import _scan_shell, substitution_depth  # noqa: E402

    commands = list(dict.fromkeys(c for rows in CORPUS.values() for c in rows)) + EXTRA
    rows = []
    for command in commands:
        row = {
            "command": command,
            "ansi_c": expand_ansi_c_quotes(command),
            "depth": substitution_depth(command),
        }
        if len(command) <= MAX_STEP_LEN:
            row["steps"] = {
                name: [list(step) for step in _scan_shell(command, **options)]
                for name, options in OPTION_SETS.items()
            }
        rows.append(row)
    body = ",\n".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) for r in rows)
    Path(sys.argv[2]).write_text("[\n" + body + "\n]\n", encoding="utf-8")
    print(f"{len(rows)} commands")


if __name__ == "__main__":
    main()
