#!/usr/bin/env python3
"""Record the reference's execution-flag, grep-operand and heredoc-mask primitives.

Usage: create_approval_exec_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. Per command it records `mask_heredoc_bodies_for_shell_scan`
(both consumer modes), `_iter_top_level_shell_segments`, `_shell_segment_tokens` and
`_shell_tokens_with_spans` at every command word, `_quoted_grep_pattern_spans`,
`_grep_safe_detection_variant`, `_execution_flag_findings`, `_eval_payloads` and `_nested_payloads`.
"""

import json
import os
import sys
from pathlib import Path

os.environ["HOME"] = "/home/user"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from create_approval_corpus import CORPUS  # noqa: E402
from create_approval_peel_corpus import EXTRA as PEEL_EXTRA  # noqa: E402
from create_approval_scan_corpus import EXTRA as SCAN_EXTRA  # noqa: E402
from create_approval_words_corpus import EXTRA as WORDS_EXTRA  # noqa: E402

EXTRA = [
    "python -c 'import os'", "python3 -c 'print(1)'", "python3.11 -m http.server", "python -W ignore -c x", "python -Wonce -c x", "python -cx", "python -bc x",
    "py -c x", "python script.py", "python - <<EOF\nprint(1)\nEOF", "python <<'EOF'\nx\nEOF", "node -e 'x'", "node --eval=x", "node -p 1", "node -pe 1", "node a.js",
    "node --require ./x -e y", "perl -e 'x'", "perl -ne x", "perl -Mstrict -e x", "perl -0 -e x", "perl x.pl", "ruby -e x", "ruby -rjson -e x", "ruby -r json -e x",
    "php -r x", "php -d x=1 -r y", "php a.php", "powershell -Command Get-Date", "pwsh -c 'ls; rm x'", "pwsh -NoProfile -Command ls rm x", "pwsh -File a.ps1",
    "pwsh -ExecutionPolicy Bypass -Command x", "pwsh ls", "pwsh -c", "powershell -c", "bun -e x", "bun run a.ts", "deno eval x", "deno -q eval x", "deno run eval.ts",
    "deno -L debug eval x", "deno -e x", "bash -c 'ls'", "bash -lc 'ls'", "sh -c ls", "zsh -c x", "dash -ec x", "bash -o pipefail -c x", "bash -oc x", "bash -O extglob -c x",
    "bash --norc -c x", "bash --rcfile f -c x", "bash -c", "bash -Wc x", "bash x.sh", "bash -x x.sh", "bash -- -c x", "bash +o x -c y", "bash -s -c x", "ksh -c x",
    "sudo bash -c 'x'", "env bash -c x", "xargs sh -c 'x'", "bash -c 'a; bash -c b'", "sh -c \"echo 'x'\"", "sh -c 'unterminated", "python -c 'unterminated", "rg --pre ./x foo",
    "rg --pre=./x foo", "rg -e --pre foo", "rg --regexp --pre foo", "rg foo --pre", "rg --hostname-bin x", "sort --compress-program=x f", "sort --compress-program x f",
    "sort -k 1 f", "sort -o out f", "sort -ko x", "ag --pager less foo", "ag --pager=less x", "man --pager x ls", "man -P x ls", "man -Px ls", "man -H x ls", "man --html=x ls",
    "man ls", "man -- -P x", "rg -g '*.rs' --pre x y", "rg --max-count 1 --pre x y", "rg -A 1 --pre x y",
    "eval ls", "eval 'rm -rf /'", "eval rm -rf /", "eval -- ls", "eval -x ls", "eval", "eval eval ls", "a; eval b; c", "echo $(eval x)", "echo \"$(eval x)\" y", "eval \"$(cat f)\"",
    "eval eval eval eval eval eval eval eval eval eval ls", "FOO=1 eval x", "sudo eval x", "bash -c 'eval x'", "echo `eval y`",
    "grep foo f", "grep -P 'a+' f", "grep -P \"a+\" f", "grep -P a+ f", "grep -P -e 'a' f", "grep -Pe 'a' f", "grep -Pe'a' f", "grep --perl-regexp 'a' f",
    "grep --perl-regexp --regexp='a' f", "grep --perl-regexp --regexp 'a' f", "grep -P --file=pats f", "grep -P -f pats f", "grep -P -e", "grep -P --regexp", "grep -m",
    "grep -P 'a' 'b' c", "grep -P \"$(x)\" f", "grep -P 'a' f | grep -P 'b'", "grep -P 'a'; rm -rf /", "egrep -P 'x' f", "grep -P -- '-x' f", "grep -P", "grep -P ''",
    "grep -o \"[^\\\"]*\" f", "grep -P 'unterminated", "grep -P \"unterminated", "grep -P 'a\\'b' c", "echo \"$(grep -P 'x' f | cut -d: -f1)\" z", "sed -n \"$(grep -n X f | cut -d: -f1),+3p\" f",
    "grep -P `x` f", "grep -P 'a'b c", "grep -P x='a' f", "grep -A 3 -P 'a' f", "grep -A3 -P 'a' f", "grep -e a -e b f", "grep -P -e a -e 'b' f", "grep -- f", "grep a\\ b f",
    "cat <<'EOF'\nrm -rf /\nEOF", "cat <<'EOF'\nx\nEOF\nrm -rf /", "cat <<EOF\nx\nEOF", "cat <<'EOF' | sh\nx\nEOF", "cat <<'EOF'; ls\nx\nEOF", "python <<'EOF'\nimport os\nEOF",
    "python3 <<'EOF'\nx\nEOF", "osascript <<'EOF'\nx\nEOF", "bash <<'EOF'\nx\nEOF", "cat <<'A' <<'B'\n1\nA\n2\nB", "cat <<'A' <<B\n1\nA\n2\nB", "cat <<-'EOF'\n\tx\n\tEOF",
    "cat <<\"EOF\"\nx\nEOF", "cat <<\\EOF\nx\nEOF", "cat <<'E''F'\nx\nEF", "cat <<$'EOF'\nx\nEOF", "cat <<'EOF'\nunterminated", "cat <<", "cat << \nx", "cat <<'EOF' >f\nx\nEOF",
    "cat <<'EOF' > $(x)\nx\nEOF", "FOO=1 cat <<'EOF'\nx\nEOF", "env X=1 cat <<'EOF'\nx\nEOF", "/bin/cat <<'EOF'\nx\nEOF", "a && cat <<'EOF'\nx\nEOF", "cat <<'EOF' &\nx\nEOF",
    "cat <<'EOF' 2>&1\nx\nEOF", "cat <<<x", "echo \"<<'EOF'\"\nrm x", "cat <<'EOF'\r\nx\r\nEOF\r\n", "cat <<'EOF'\n$(x)\nEOF", "(cat <<'EOF'\nx\nEOF\n)", "x=\"a<<b\"; cat <<'EOF'\nx\nEOF",
    "cat <<'EOF'\nx\nEOF\ncat <<'F'\ny\nF\nls", "tee f <<'EOF'\nx\nEOF", "cat <<'EOF' # c\nx\nEOF", "cat a<<'EOF'\nx\nEOF",
]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools import approval_detection as d  # noqa: E402
    from tools.approval_grep_spans import _grep_safe_detection_variant, _quoted_grep_pattern_spans  # noqa: E402
    from tools.approval_shell_tokens import _shell_tokens_with_spans  # noqa: E402
    from tools.shell_heredoc import mask_heredoc_bodies_for_shell_scan  # noqa: E402

    commands = list(dict.fromkeys(
        [c for rows in CORPUS.values() for c in rows] + WORDS_EXTRA + SCAN_EXTRA + PEEL_EXTRA + EXTRA))
    rows = []
    for command in commands:
        segments = list(d._iter_top_level_shell_segments(command))
        tokens, span_tokens = [], []
        for segment in segments:
            for start, _, _ in d._iter_shell_command_word_spans(segment):
                tokens.append([segment, start, d._shell_segment_tokens(segment, start)])
                st = _shell_tokens_with_spans(segment, start)
                span_tokens.append([segment, start, None if st is None else [list(t) for t in st]])
        spans, malformed = _quoted_grep_pattern_spans(command)
        capped = []
        nested0 = [[p, n] for p, n in d._nested_payloads(command, 0, capped)]
        capped8 = []
        nested8 = [[p, n] for p, n in d._nested_payloads(command, 8, capped8)]
        rows.append({
            "command": command,
            "heredoc_data": mask_heredoc_bodies_for_shell_scan(command, code_consumers=False),
            "heredoc_code": mask_heredoc_bodies_for_shell_scan(command, code_consumers=True),
            "segments": segments,
            "tokens": tokens,
            "span_tokens": span_tokens,
            "grep": [[list(s) for s in spans], malformed],
            "grep_safe": list(_grep_safe_detection_variant(command)),
            "findings": [list(f) for f in d._execution_flag_findings(command)],
            "evals": list(d._eval_payloads(command)),
            "nested0": nested0, "capped0": capped,
            "nested8": nested8, "capped8": capped8,
        })
    Path(sys.argv[2]).write_text(json.dumps(rows, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    print(f"{len(rows)} commands")


if __name__ == "__main__":
    main()
