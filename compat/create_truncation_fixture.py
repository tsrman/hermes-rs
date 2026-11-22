#!/usr/bin/env python3
"""Record the reference's head/tail truncation of tool output.

Usage: create_truncation_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. Each case is (text, max_chars) and the text the reference's
`truncate_head_tail` returns, including multi-byte characters and the notice wording.
"""
import json
import os
import sys
import tempfile
from pathlib import Path

reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
os.environ["HERMES_HOME"] = tempfile.mkdtemp()
sys.path.insert(0, str(reference))
os.chdir(reference)
from tools.tool_output_truncate import truncate_head_tail

texts = {
    "ascii": "".join(chr(97 + (i * 7) % 26) for i in range(1000)),
    "lines": "".join(f"line {i}\n" for i in range(400)),
    "cyrillic": "".join("Пример строки " + str(i) + "\n" for i in range(300)),
    "emoji": "a😀б" * 200,
    "short": "short output",
}
cases = []
for name, text in texts.items():
    for budget in (10, 50, 99, 100, 500, 1000, 5000):
        cases.append({"name": name, "text": text, "max": budget, "expected": truncate_head_tail(text, budget)})
from tools.tool_output_limits import _coerce_positive_int

values = [12, 12.7, "12", " 12 ", "12.5", True, False, 0, -5, None, [5], {"a": 1}, 1000.0, "0", "abc", ""]
limits = [{"value": value, "expected": _coerce_positive_int(value, 50000)} for value in values]
cases = {"truncation": cases, "limits": limits}
output.write_text(json.dumps(cases, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
print("wrote", output)
