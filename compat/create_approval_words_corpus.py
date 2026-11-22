#!/usr/bin/env python3
"""Record the reference's command-position and shell-word primitives for the approval port.

Usage: create_approval_words_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. Records, per command, `_iter_shell_command_starts`,
`_mark_command_starts` (plain and with a skip span), `_iter_shell_command_word_spans`, and
`_shell_command_segment` at every start; plus `_deobfuscate_shell_word` for every command word and
for the extra words below, and `_literal_command_substitution_output` for the extra scripts.
"""

import json
import os
import sys
from pathlib import Path

os.environ["HOME"] = "/home/user"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from create_approval_corpus import CORPUS  # noqa: E402
from create_approval_scan_corpus import EXTRA as SCAN_EXTRA  # noqa: E402

EXTRA = [
    "sudo -u root rm -rf /", "sudo -ul reboot", "sudo -nl reboot", "sudo -l reboot", "sudo -- rm x", "sudo -E -H ls",
    "env -i A=1 rm x", "env -S 'echo hi' ls", "env --split-string='a b' c", "env -u X rm", "env -- rm", "env A=1 B=2",
    "command -v reboot", "command -pv reboot", "command -p reboot", "nice -n 5 reboot", "nice -n5 ls", "timeout 5 ls",
    "timeout -s KILL 5 reboot", "timeout -- 5 ls", "chroot / ls", "chrt 1 ls", "taskset 1 ls", "taskset -p 1 ls",
    "stdbuf -o0 -e L ls", "ionice -c 2 -n 4 ls", "ionice -p 1", "exec -a name ls", "time -f %e ls", "nohup setsid time ls",
    "2>/dev/null reboot", "> out rm x", "3>&1 ls", "<in cat", "&>log ls", ">| f ls", ">>f ls", "<<EOF cat\nx\nEOF", "<> f ls",
    "if true; then rm x; fi", "while true; do ls; done", "! rm x", "{ rm x; }", "( rm x )", "a && b || c; d & e",
    "a &&& b", "a ||| b", "a; ; b", "echo ${IFS} {rm,x}", "{rm,-rf,/}", "{ls}", "x=$(echo rm) y", "$(echo rm) -rf /",
    "`echo rm` -rf /", "echo $(rm x; ls $(a | b))", "echo \"$(ls; rm x)\"", "echo '$(rm x)'", "echo `a; b`",
    "echo a\\;b", "echo a \\&\\& b", "echo a # b; c\nd", "echo $((1;2))", "(a;(b;(c)))", "a\n\nb\n\n\nc", "  \t a", "a;\n  b",
    "cmd 2>&1 | other", "cmd &>/dev/null; z", "cmd >&2 || z", "cmd <&3 | z", "cmd >| f; z", "cmd 2>&1; z",
    "echo \"unclosed $(a; b", "echo `unclosed a; b", "sh -c 'a; b' ; c", "bash -c \"x && y\"", "/usr/bin/env rm x",
    "RM x", "\\rm x", "r\\m x", "'r'm x", "\"r\"'m' x", "$'r'm x", "r$(echo)m x", "${x:-rm} y", "${x/a/rm} y",
]
EXTRA_WORDS = [
    "rm", "'rm'", "\"rm\"", "r\\m", "r''m", "/bin/rm", "$(echo rm)", "`echo rm`", "$(printf '%s' rm)", "$(printf rm)",
    "$(echo -n rm)", "$(echo -e rm)", "$(echo -nE rm)", "$(echo 'a b')", "$(echo a b)", "$(echo $(echo rm))", "${x:-rm}",
    "${x/a/rm}", "${x:-}", "${x/a/}", "{rm,-rf,/}", "{a}", "{a,b}", "{a,'b'}", "a${x:-b}c", "\\$(echo rm)", "'$(echo rm)'",
    "\"$(echo rm)\"", "$(echo", "$(", "`", "$(echo rm", "r\\", "\\", "'", "\"\"", "''", "x\\ y", "a'b c'd", "$((1))",
    "$(echo ../x)", "$(echo 'it''s')", "$(echo \"a\\\"b\")", "$(echo a\\ b)", "$(echo 'unterminated)", "$(echo -- rm)",
    "$(printf %s rm)", "$(printf '%s' 'r m')", "$(PRINTF rm)", "$(Echo rm)", "$(echo)", "$()", "$( )", "${}", "${a:-b:-c}",
]
EXTRA_SCRIPTS = [
    "echo rm", "echo -n rm", "echo -e rm", "echo -nEe rm", "echo -x rm", "printf rm", "printf '%s' rm", "printf %s rm x",
    "printf %d 1", "echo 'a b'", "echo a b", "echo \"a\\\"b\"", "echo a\\ b", "echo 'unterminated", "echo \\", "echo \"x\\",
    "", "   ", "'", "echo ''", "echo \"\"", "echo a''b", "echo a'b'c", "ECHO rm", "printf '%s'", "printf", "echo", "ls",
    "echo a/b.c:d@e%f+g=h,i-j", "echo a;b", "echo $x", "echo a\tb", "echo a\nb", "echo 'a\\b'", "echo \"a\\b\"", "echo \"a\\$b\"",
]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools import approval_detection as d  # noqa: E402

    commands = list(dict.fromkeys(c for rows in CORPUS.values() for c in rows)) + EXTRA + SCAN_EXTRA
    commands = list(dict.fromkeys(commands))
    rows = []
    for command in commands:
        starts = list(d._iter_shell_command_starts(command))
        spans = [list(s) for s in d._iter_shell_command_word_spans(command)]
        row = {
            "command": command,
            "starts": starts,
            "mark": d._mark_command_starts(command),
            "mark_skip": d._mark_command_starts(command, skip=[(2, 5), (9, 12)]),
            "spans": spans,
            "deob": [d._deobfuscate_shell_word_for_detection(s[2]) for s in spans],
        }
        if len(command) <= 400:
            row["segments"] = [d._shell_command_segment(command, s) for s in starts]
        rows.append(row)
    words = [{"word": w, "deob": d._deobfuscate_shell_word_for_detection(w)} for w in EXTRA_WORDS]
    scripts = [{"script": s, "out": d._literal_command_substitution_output(s)} for s in EXTRA_SCRIPTS]
    out = {"commands": rows, "words": words, "scripts": scripts}
    Path(sys.argv[2]).write_text(json.dumps(out, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    print(f"{len(rows)} commands, {len(words)} words, {len(scripts)} scripts")


if __name__ == "__main__":
    main()
