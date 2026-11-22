#!/usr/bin/env python3
"""Record the reference's `classify_osascript` for a corpus of command strings.

Usage: create_approval_osascript_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. Output rows: `{"command", "found": [description, ...]}` (sorted).
"""

import json
import os
import sys
from pathlib import Path

os.environ["HOME"] = "/home/user"

COMMANDS = [
    "osascript -e 'get name of first application process whose frontmost is true'", "osascript -e 'get the name of every disk'", "osascript -e 'activate'",
    "osascript -e 'display notification \"done\"'", "osascript -e 'tell application \"Finder\" to get name of startup disk'", "osascript -e 'tell app \"Notes\" to quit'",
    "osascript -e 'tell application \"Keynote\" to close active presentation saving no'", "osascript -e 'Quit without saving?'", "osascript -e 'do shell script \"ls\"'",
    "osascript -e 'do   shell   script \"ls\"'", "osascript -e 'tell application \"System Events\" to keystroke \"a\"'", "osascript -e 'tell application \"System Events\" to key code 36'",
    "osascript -e 'tell application \"Finder\" to delete file \"x\"'", "osascript -e 'tell application \"Finder\" to move file \"x\" to folder \"y\"'",
    "osascript -e 'set the text of paragraph 1 to \"x\"'", "osascript -e 'set contents of x to 1'", "osascript -e 'set the first word of d to \"y\"'", "osascript -e 'set myvar to 1'",
    "osascript -e 'tell application \"Safari\" to click button 1'", "osascript -e 'tell application \"Mail\" to empty trash'", "osascript -e 'shut down'", "osascript -e 'shutdown'",
    "osascript -e 'log out'", "osascript -e 'logout'", "osascript -e 'restart'", "osascript -e 'eject disk \"x\"'", "osascript -e 'perform action \"AXPress\"'",
    "osascript -e 'tell app \"X\" to close every window without saving'", "osascript -e 'open for access file \"x\"'", "osascript -e 'set eof of f to 0'", "osascript -e 'run script \"x\"'",
    "osascript -e 'load script file \"x\"'", "osascript -e 'store script x'", "osascript -e 'do javascript \"alert(1)\"'", "osascript -e 'execute'", "osascript -e '«event aevtquit»'",
    "osascript -e 'do ¬\nshell script \"ls\"'", "osascript -e 'do (* x *) shell script \"ls\"'", "osascript -e 'do -- c\nshell script \"ls\"'", "osascript -e 'do # c\nshell script \"ls\"'",
    "osascript -e 'display dialog \"a -- b (* c\" & quit'", "osascript -e '-- quit'", "osascript -e '(* quit'", "osascript -e '(* (* nested *) quit *) get name'", "osascript -e 'get |quit|'",
    "osascript -e 'get name' -e 'quit'", "osascript -e 'get name' -e 'get id'", "osascript -e 'get name' arg1 arg2", "osascript -e get", "osascript -eget", "osascript -e'quit'", "osascript -e", "osascript -e ''",
    "osascript -l JavaScript -e 'Application(\"Finder\").name()'", "osascript -lJavaScript -e 'x'", "osascript -l AppleScript -e 'get name'", "osascript -l applescript -e 'get name'", "osascript -l",
    "osascript -s h -e 'get name'", "osascript -s o -e 'get name'", "osascript script.scpt", "osascript ./script.scpt arg", "osascript -", "osascript", "osascript -i", "osascript -i -e 'get name'",
    "osascript < script.scpt", "osascript <script.scpt", "cat s.scpt | osascript", "osascript -e 'get name' < /dev/null", "osascript -e 'get name' > out.txt", "osascript -e 'get name' >> out.txt",
    "osascript -e 'get name' 2>&1", "osascript -e 'get name' 2>/dev/null", "osascript -e 'get name' &>/dev/null", "osascript -e 'get name' >| out", "osascript -e 'get name' > \"$HOME/x\"",
    "osascript -e 'get name' > $(echo x)", "osascript -e 'get name' > `echo x`", "osascript -e \"$SCRIPT\"", "osascript -e $(cat x)", "osascript -e `cat x`", "osascript -e 'get $x'", "osascript -e \"get name of $app\"",
    "osascript <<< 'get name'", "osascript <<< 'quit'", "osascript <<< \"$x\"", "osascript <<EOF\nget name\nEOF", "osascript <<'EOF'\nquit\nEOF", "osascript <<\"EOF\"\nget name\nEOF",
    "osascript <<EOF\nget $x\nEOF", "osascript <<'EOF'\nget $x\nEOF", "osascript <<EOF\nget \\$x\nEOF", "osascript <<EOF\nget \\\\$x\nEOF", "osascript <<-EOF\n\tget name\n\tEOF", "osascript << EOF\nget name\nEOF",
    "osascript <<EOF\nget name", "osascript <<EOF\nget name\nEOF\nls", "osascript <<EOF\nget name\nEOF\nosascript -e quit", "osascript <<A <<B\nx\nA\nB", "osascript <<'E OF'\nx\nE OF", "osascript <<\nx", "osascript <<$X\nx\n$X",
    "osacompile -e 'get name' -o a.app", "osacompile -e 'quit' -o a.app", "osacompile -l JavaScript -e x", "osacompile -o out.scpt -e 'get name'", "osacompile -t x -e 'get name'", "osacompile -c x -e 'get name'",
    "/usr/bin/osascript -e 'quit'", "OSASCRIPT -e 'quit'", "osascript.exe -e 'quit'", "o\\sascript -e 'quit'", "osa''script -e 'quit'", "'osascript' -e 'quit'", "\"osascript\" -e 'quit'", "$'osascript' -e 'quit'",
    "$'\\157sascript' -e quit", "osa\\\nscript -e quit", "osascript \\\n-e 'quit'", "osascript -e 'get name' \\\n  -e 'quit'", "sudo osascript -e 'quit'", "env X=1 osascript -e 'quit'", "nohup osascript -e 'quit' &",
    "time osascript -e 'get name'", "echo hi; osascript -e 'quit'", "echo hi && osascript -e 'get name'", "(osascript -e 'quit')", "osascript -e 'get name' | cat", "cat x | osascript -e 'get name'",
    "sh -c 'osascript -e quit'", "bash -c \"osascript -e 'get name'\"", "eval 'osascript -e quit'", "eval osascript -e quit", "xargs osascript -e quit", "ssh host osascript -e quit", "ssh host 'osascript -e quit'",
    "watch osascript -e quit", "find . -exec osascript {} \;", "find . -name osascript", "env -S 'osascript -e quit'", "env osascript -e quit", "python3 -c 'import os; os.system(\"osascript -e quit\")'",
    "perl -e 'system(\"osascript -e quit\")'", "node -e 'x=\"osascript\"'", "ruby -e 'system \"osascript\"'", "su -c 'osascript -e quit' x", "doas osascript -e quit", "script -q /dev/null osascript -e quit",
    "launchctl submit -l x -- osascript -e quit", "caffeinate osascript -e quit", "arch -arm64 osascript -e quit", "sandbox-exec -p x osascript -e quit", "parallel osascript ::: a", "automator osascript",
    "echo osascript", "grep osascript notes.txt", "cat osascript-notes.md", "python test_osascript_bridge.py", "ls osascript.d", "git commit -m 'use osascript -e quit'", "echo 'osascript -e quit'", "echo \"osascript\"",
    "X=osascript; $X -e quit", "X=osascript; \"$X\" -e quit", "`which osascript` -e quit", "$(which osascript) -e quit", "${OSA:-osascript} -e quit", "$OSA -e quit", "cmd=osascript\n$cmd -e quit",
    "type osascript", "which osascript", "command -v osascript", "man osascript", "osascript --help", "osascript -h", "osascript -V",
    "osascript -e 'tell application \"Finder\"' -e 'get name of home' -e 'end tell'", "osascript -e 'tell application \"Finder\"' -e 'delete x' -e 'end tell'",
    "osascript -e 'set x to \"quit\"'", "osascript -e 'display dialog \"Close the door\"'", "osascript -e 'get the name of the front window'", "osascript -e 'tell app \"Music\" to play'",
    "osascript -e 'beep'", "osascript -e 'say \"hi\"'", "osascript -e 'tell application \"Terminal\" to do script \"ls\"'", "osascript -e 'set volume output volume 50'", "osascript -e 'get volume settings'",
    "osascript -e 'tell application \"Safari\" to get URL of front document'", "osascript -e 'tell application \"Safari\" to set URL of front document to \"x\"'",
    "osascript -e 'tell application \"Finder\" to make new folder'", "osascript -e 'tell application \"Finder\" to duplicate file x'", "osascript -e 'tell application \"Finder\" to open file x'",
    "osascript -e 'tell application \"System Events\" to get name of every process'", "osascript -e 'tell application \"System Events\" to click menu item \"x\"'",
    "osascript -e 'KEYSTROKE'", "osascript -e 'Keystroke \"a\"'", "osascript -e 'don\\'t save'", "osascript -e \"don't save\"", "osascript -e 'dont save'", "osascript -e 'DON T SAVE'",
    "osascript\t-e\t'quit'", "osascript  -e  'quit'", "osascript -e 'quit'\nls", "ls\nosascript -e 'quit'\nls", "osascript -e 'get name'; osascript -e 'quit'", "osascript -e 'get name' && ls",
    "osascript 'unterminated", "osascript -e 'unterminated", "osascript \"-e\" 'quit'", "osascript '-e' 'quit'", "osascript -e\"quit\"", "osascript $'-e' quit", "osascript -e $'quit'",
    "osascript -e 'get name' # quit", "osascript -e 'get name' #quit\nls", "# osascript -e quit", "echo x # osascript -e quit",
    "osascript -e 'get name' --", "osascript -- -e quit", "osascript -e 'x' -- y", "osascript -e x y z", "osascript x -e quit",
    "OSACOMPILE -e quit", "osacompile.exe -e quit", "C:\\Windows\\osascript.exe -e quit", "osascript-helper -e quit", "xosascript -e quit", "osascript2 -e quit",
]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools.approval_applescript import classify_osascript  # noqa: E402

    rows = [{"command": c, "found": sorted(classify_osascript(c))} for c in dict.fromkeys(COMMANDS)]
    body = ",\n".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) for r in rows)
    Path(sys.argv[2]).write_text("[\n" + body + "\n]\n", encoding="utf-8")
    print(f"{len(rows)} commands, {sum(bool(r['found']) for r in rows)} flagged")


if __name__ == "__main__":
    main()
