#!/usr/bin/env python3
"""Record the reference's wrapper-peeling, heredoc and masking helpers for the approval port.

Usage: create_approval_peel_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. Per command it records `_mask_quoted_newlines`,
`_join_escaped_word_chars`, `strip_wrapper_prefixes`, `literal_heredoc_bodies`,
`_blank_literal_heredoc_bodies`, `dequote_rm_operands`; for the extras it also records
`_command_parser_limit_exceeded` (large inputs as generator descriptors), `_split_env_string` and
`_env_split_payload`.
"""

import json
import os
import shlex
import sys
from pathlib import Path

os.environ["HOME"] = "/home/user"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from create_approval_corpus import CORPUS  # noqa: E402
from create_approval_scan_corpus import EXTRA as SCAN_EXTRA  # noqa: E402
from create_approval_words_corpus import EXTRA as WORDS_EXTRA  # noqa: E402

EXTRA = [
    "echo 'a\nb'; rm x", "echo \"a\nsudo reboot\"", "git commit -m 'x\nsudo reboot'", "echo \"$(grep x f\nreboot)\"",
    "echo \"a\r\nb\"", "echo a\nb", "echo 'a\\'\nb", "echo \"unterminated\nreboot", "echo `a\nb`", "echo $(a\nb)", "echo \"$(a\n\"b\nc\")\"",
    "echo a\\;b \\&\\& c\\|d", "echo 'a\\;b'", "echo \"a\\;b\" \\ x\\\ty", "git push -o ci\\;skip -f", "git -C my\\ repo push -f",
    "echo \"a\\\"b\" \\'c", "echo '\\'", "echo \\\n x", "a\\", "echo \\\"; rm -rf /",
    "nice reboot", "nice -n 5 reboot", "nice -n x reboot", "nice -n5 reboot", "timeout 5 reboot", "timeout -s KILL 5 reboot", "timeout -- 5 reboot",
    "timeout 1.5s reboot", "timeout 0x10 ls", "timeout +inf ls", "timeout -k 1 2 ls", "command -v reboot", "command -pv reboot", "command reboot",
    "sudo -u root reboot", "sudo -ul reboot", "sudo -nl reboot", "sudo -H reboot", "sudo -h host reboot", "sudo --user=root reboot", "sudo --us root reboot",
    "sudo --chd=/ reboot", "sudo -C 3 reboot", "sudo -C x reboot", "sudo -- reboot", "sudo -E env A=1 reboot", "doas -u root reboot", "env -i reboot",
    "env A=1 B=2 reboot", "env -u A reboot", "env -S 'nice reboot'", "env --split-string='nice reboot' x", "env -- reboot", "exec -a x reboot", "nohup reboot",
    "setsid reboot", "time -f %e reboot", "stdbuf -o0 reboot", "ionice -c 2 reboot", "ionice -p 1", "xargs -n 1 reboot", "xargs -I{} rm {}", "builtin reboot",
    "busybox reboot", "busybox --help reboot", "toybox rm -rf /", "busybox", "A=1 nice reboot", "2>/dev/null reboot", "2>/dev/null nice reboot",
    "nice 2>/dev/null reboot", ">out nice reboot", "<in cat", ">>(x) y", "nice", "A=1", "nice;reboot", "a; nice reboot; b", "a && sudo reboot || doas reboot",
    "(nice reboot)", "{ nice reboot; }", "if true; then nice reboot; fi", "echo $(nice reboot)", "n\\ice reboot", "'nice' reboot", "$(echo nice) reboot",
    "/usr/bin/nice reboot", "NICE reboot", "env nice timeout 5 sudo -u x reboot", "chroot / reboot", "taskset 1 reboot", "chrt 1 reboot",
    "cat <<'EOF'\nsudo reboot\nEOF", "cat <<'EOF' > f\nrm -rf /\nEOF\nls", "cat <<\"EOF\"\nx\nEOF", "cat <<EOF\nx\nEOF", "cat <<-'EOF'\n\tx\n\tEOF",
    "tee f <<'EOF'\nx\nEOF", "bash <<'EOF'\nrm -rf /\nEOF", "cat <<'EOF' | sh\nrm -rf /\nEOF", "cat <<'EOF' \\\n| sh\nx\nEOF", "cat <<'EOF'\nunterminated",
    "a; cat <<'E1'\nx\nE1\ncat <<'E2'\ny\nE2", "cat <<'EOF'\n<<'EOF'\nx\nEOF\nEOF", "echo hi <<'EOF'\nx\nEOF", "/bin/cat <<'EOF'\nx\nEOF", "CAT <<'EOF'\nx\nEOF",
    "x | cat <<'EOF'\nrm -rf /\nEOF", "cat <<'EOF'; rm -rf /\nx\nEOF", "cat <<'A-B.c'\nx\nA-B.c", "cat <<'EOF' \\\n\\\n\\\n\\\n\\\n\\\n\\\n\\\n\\\n x\nEOF",
    "rm -rf \"/\"", "rm -rf \"$HOME\"", "rm -rf \"/\"./*", "rm -rf \"$HOME\"/\"*\"", "rm -rf '/etc'", "rm -rf \"/etc;backup\"", "rm -rf \"a b\" c", "rm \"x\" 'y' z",
    "RM -rf \"/\"", "sudo rm -rf \"/\"", "a; rm -rf \"/\"; rm \"b\"", "rm -rf \"/\" \"/etc\" \"/usr\"", "rm -rf \"\"", "rm -rf '\"'", "rm -rf $'/'",
    "echo hi", "\"rm\" -rf /", "rm -f -- \"-x\"",
]
ENV_SPLIT_STRINGS = [
    "nice reboot", "a b  c", "'a b' c", "\"a b\" c", "a\\_b c", "a\\ b", "a\\nb", "\\c", "a \\c b", "'\\c'", "a\\xb", "\\q", "'a\\qb'", "'a\\'b'",
    "'a\\\\b'", "\"a\\$b\"", "a$b", "'a$b'", "a #comment", "a#b", "# x", "\\", "'unterminated", "\"unterminated", "\\_a", "a\\_", "  ", "", "a\tb\nc",
    "-S x", "'' x", "a ''", "A=1 b",
]
ENV_COMMANDS = [
    "env -S 'nice reboot'", "env -S'nice reboot'", "env --split-string='nice reboot'", "env --split-string 'a b' c d", "env -iS'a b'", "env -uS x y",
    "env -u X -S 'a b'", "env -S", "env -S ''", "env -- x", "env x", "env -i x", "env --unset X y", "env --unset=X y", "env -S '-S reboot'",
    "env -S '-i -S reboot' z", "env -C / -S 'a'", "env --chdir=/ -S a", "env -a x -S b", "env -S 'a\\_b'", "env -S 'a $B'", "env -S 'x' 'y z'",
]
LARGE = [("chars", 128001), ("chars", 128000), ("nosep", 4097), ("nosep", 4096), ("depth", 33), ("depth", 32), ("seps", 25000), ("seps", 24999),
         ("depthq", 40), ("nl", 25000)]


def build(kind, n):
    if kind == "chars":
        return "a" * (n - 1) + ";"
    if kind == "nosep":
        return "a" * n
    if kind == "depth":
        return "echo " + "$(" * n + ")" * n
    if kind == "depthq":
        return "echo " + "\\$(" * n
    if kind == "seps":
        return ";" * n
    if kind == "nl":
        return "\n" * n
    raise ValueError(kind)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools import approval_detection as d  # noqa: E402
    from tools import approval_env_split as e  # noqa: E402
    from tools import approval_wrappers as w  # noqa: E402
    from tools.approval_quoted_newlines import _mask_quoted_newlines  # noqa: E402

    commands = list(dict.fromkeys(
        [c for rows in CORPUS.values() for c in rows] + WORDS_EXTRA + SCAN_EXTRA + EXTRA))
    rows = []
    for command in commands:
        rows.append({
            "command": command,
            "mask_nl": _mask_quoted_newlines(command),
            "join": d._join_escaped_word_chars(command),
            "peel": w.strip_wrapper_prefixes(command),
            "heredocs": [list(s) for s in w.literal_heredoc_bodies(command)],
            "blank": w._blank_literal_heredoc_bodies(command),
            "dequote": w.dequote_rm_operands(command),
            "limit": d._command_parser_limit_exceeded(command),
        })
    large = [{"kind": k, "n": n, "limit": d._command_parser_limit_exceeded(build(k, n))} for k, n in LARGE]
    splits = [{"payload": p, "args": e._split_env_string(p)} for p in ENV_SPLIT_STRINGS]
    envs = []
    for command in ENV_COMMANDS:
        tokens = shlex.split(command)
        envs.append({"tokens": tokens, "payload": e._env_split_payload(tokens)})
    out = {"commands": rows, "large": large, "splits": splits, "envs": envs}
    Path(sys.argv[2]).write_text(json.dumps(out, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    print(f"{len(rows)} commands, {len(large)} large, {len(splits)} splits, {len(envs)} envs")


if __name__ == "__main__":
    main()
