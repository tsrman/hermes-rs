#!/usr/bin/env python3
"""Record fuzzy find-and-replace results from the Python reference.

Usage: create_fuzzy_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. Each case holds the file text, old and new strings and
replace_all, and the reference's (new content, match count, strategy, error, notes); a second
list holds the "Did you mean" snippets.
"""

import json
import sys
from pathlib import Path

PY = "def f(x):\n    if x:\n        return 1\n    return 2\n\n\ndef g(y):\n    return y + 1\n"
TABS = "class A:\n\tdef m(self):\n\t\treturn 1\n"
SMART = "title = \u201cHello\u201d \u2014 it\u2019s fine\nnote = a\u00a0b\n"
DUP = "x = 1\ny = 2\nx = 1\nz = 3\nx = 1\n"
GUT = "alpha\nbeta\ngamma\ndelta\n"
CASES = [
    ("exact", PY, "return 1", "return 10", False),
    ("exact_multi_line", PY, "    if x:\n        return 1\n", "    if not x:\n        return 0\n", False),
    ("exact_delete", PY, "    return 2\n", "", False),
    ("ambiguous", DUP, "x = 1", "x = 9", False),
    ("replace_all", DUP, "x = 1", "x = 9", True),
    ("ambiguous_many", "a\n" * 9, "a", "b", False),
    ("identical", PY, "return 1", "return 1", False),
    ("empty_old", PY, "", "x", False),
    ("whitespace_old", PY, "   ", "x", False),
    ("line_trimmed", PY, "if x:\nreturn 1", "if x:\n    return 11", False),
    ("indentation_flexible", PY, "if x:\n  return 1", "if x:\n  return 12", False),
    ("reindent_deeper", PY, "return 1", "if z:\n    return 1", False),
    ("whitespace_normalized", "a  =   1\nb = 2\n", "a = 1", "a = 5", False),
    ("whitespace_normalized_tabs", "x\t=\t1\n", "x = 1", "x = 2", False),
    ("escape_normalized", PY, "return 1\\n    return 2", "return 1\n    return 3", False),
    ("trimmed_boundary", PY, "  def g(y):\n    return y + 1  ", "def g(y):\n    return y + 2", False),
    ("unicode_normalized", SMART, 'title = "Hello" -- it\'s fine', 'title = "Hi" -- it\'s fine', False),
    ("unicode_keeps_typography", SMART, "note = a b", "note = a c", False),
    ("unicode_replace_all", SMART + SMART, 'title = "Hello"', 'title = "Bye"', True),
    ("block_anchor", "def h():\n    a = 1\n    b = 2\n    c = 3\n    return a\n", "def h():\n    a = 1\n    b = 22\n    c = 3\n    return a", "def h():\n    return 0", False),
    ("block_anchor_rejected", "def h():\n    a = 1\n    return a\n", "def h():\n    zzzzzzzz = qqqq\n    return a", "x", False),
    ("context_aware", "alpha beta gamma\nsecond line here\nthird line here\n", "alpha beta gamme\nsecond line here\nthird line herx", "REPLACED", False),
    ("similarity_replace_all", "def h():\n    a = 1\n    b = 2\n    return a\ndef h():\n    a = 1\n    b = 2\n    return a\n", "def h():\n    a = 1\n    b = 3\n    return a", "x", True),
    ("no_match", PY, "nothing like this at all", "x", False),
    ("tabs_flexible", TABS, "def m(self):\n    return 1", "def m(self):\n    return 2", False),
    ("mid_line", "foo(bar, baz)\n", "bar, baz", "qux", False),
    ("gutter_block", GUT, "2|beta\n3|gamma", "2|BETA\n3|GAMMA", False),
    ("gutter_block_plain_new", GUT, "2|beta\n3|gamma", "BETA\nGAMMA", False),
    ("gutter_bad_new", GUT, "2|beta\n3|gamma", "2|BETA\nGAMMA", False),
    ("gutter_single_line", GUT, "2|beta", "2|BETA", False),
    ("gutter_single_inside", "a = 4\n", "2|4", "5", False),
    ("gutter_not_found", GUT, "2|nothing\n3|here", "x", False),
    ("gutter_in_file", "1|real\n2|content\n", "1|real\n2|content", "x", False),
    ("gutter_non_consecutive", GUT, "2|beta\n5|gamma", "x", False),
    ("quote_drift_corrected", "say = 'hi'\n", "say = \\'hi\\'", "say = \\'yo\\'", False),
    ("quote_drift_ambiguous", "say = 'hi'\n", "say = \\'hi\\'", "say = \\\"yo\\\"", False),
    ("quote_real_backslash", "say = \\'hi\\'\n", "say = \\'hi\\'", "say = \\'yo\\'", False),
    ("backslash_doubled", "path = 'a\\\\b'\n", "path = 'a\\\\\\\\b'", "path = 'c'", False),
    ("backslash_doubled_new", "path = 'a\\\\b'\n", "path = 'a\\\\\\\\b'", "path = 'c\\\\d'", False),
    ("newline_drift", "a = 1\nb = 2\n", "a = 1\\nb = 2", "a = 1\\nb = 3", False),
    ("literal_newline_ok", "s = \"a\\n\"\n", "s = \"a\\n\"", "s = \"b\\n\"", False),
    ("tab_unescape", "a\tb\n", "a\tb", "c\\td", False),
    ("trailing_newline_pattern", PY, "return 2\n", "return 3\n", False),
    ("crlf", "a\r\nb\r\nc\r\n", "a\r\nb", "x\r\ny", False),
    ("single_char_gutter", "5\n", "1|5", "6", False),
    ("long_anchor", "\n".join(f"line {i} of a long file" for i in range(60)), "line 30 of a long file\nline 31 of a long fil", "X", False),
]
HINTS = [
    ("def m(self):\n    return 1", TABS),
    ("nothing", PY),
    ("   def g(y):", PY),
    ("\tdef m(self):", "class A:\n    def m(self):\n        return 1\n"),
    ("return 3\nreturn 4", PY),
    ("x = 1", DUP),
    ("", PY),
    ("abc", ""),
    ("\n\n  return y + 2", PY),
    ("alpha beta", "alpha  beta\nalpha betb\ngamma\n" * 40),
]


BIG = "".join(f"line {i}\n" for i in range(300))
DIFFS = [
    ("a\nb\nc\n", "a\nB\nc\n"), ("a\nb\nc", "a\nb\nc\nd"), ("", "new\n"), ("gone\n", ""), ("same\n", "same\n"),
    (BIG, BIG.replace("line 5\n", "FIVE\n").replace("line 250\n", "TWO-FIFTY\n")),
    (BIG, BIG.replace("line 100\n", "")), (BIG, BIG + "tail\n"), ("x\r\ny\r\n", "x\r\nz\r\n"),
    ("a\n" * 50, "a\n" * 25 + "b\n" + "a\n" * 25), ("one\ntwo\nthree\nfour\n", "four\nthree\ntwo\none\n"),
    ("\n".join(["same"] * 400), "\n".join(["same"] * 200 + ["diff"] + ["same"] * 199)),
]
APPLIED = [
    ("alpha beta gamma", "alpha beta", "alpha BETA gamma"), ("short", "x", "short"), ("has new text here", "old", "new text here"),
    ("x = 1 and x = 2", "x = 1", "x = 1"), ("content", "gone", "missing text here"), ("", "a", "long enough new"),
]


def main():
    reference = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]).resolve()
    sys.path.insert(0, str(reference))
    import difflib
    from tools.fuzzy_match import fuzzy_find_and_replace, find_closest_lines, format_no_match_hint, is_already_applied, boundary_note

    cases = []
    for name, content, old, new, replace_all in CASES:
        notes = []
        result = fuzzy_find_and_replace(content, old, new, replace_all, correction_notes=notes)
        cases.append({"name": name, "content": content, "old": old, "new": new, "replace_all": replace_all,
                      "result": list(result), "notes": notes})
    hints = []
    for old, content in HINTS:
        hints.append({"old": old, "content": content, "closest": find_closest_lines(old, content),
                      "hint": format_no_match_hint("Could not find a match for old_string in the file", 0, old, content)})
    diffs = [{"old": o, "new": n, "diff": "".join(difflib.unified_diff(
        o.splitlines(keepends=True), n.splitlines(keepends=True), fromfile="a/f.txt", tofile="b/f.txt"))}
        for o, n in DIFFS]
    applied = [{"content": c, "old": o, "new": n, "applied": is_already_applied(c, o, n)} for c, o, n in APPLIED]
    notes = [{"strategy": s, "note": boundary_note([s])} for s in ("exact", "block_anchor", "context_aware", None, "line_trimmed")]
    out.write_text(json.dumps({"cases": cases, "hints": hints, "diffs": diffs, "applied": applied, "notes": notes}, indent=1, ensure_ascii=False) + "\n",
                   encoding="utf-8")
    print("wrote", out, len(cases), len(hints))


if __name__ == "__main__":
    main()
