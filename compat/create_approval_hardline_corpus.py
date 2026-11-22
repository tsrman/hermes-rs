#!/usr/bin/env python3
"""Record the reference's hardline verdicts for verbs crossed with execution contexts.

Usage: create_approval_hardline_corpus.py <reference checkout> <output.json>

Strings only: nothing is executed. Every verb below is a command the floor must refuse (or a near
miss it must not); every context wraps it the way an agent might (wrappers, quoting, carriers,
heredocs, substitutions, obfuscation). The output is `[{"command", "hardline": [bool, description]}]`.
"""

import json
import os
import sys
from pathlib import Path

os.environ["HOME"] = "/home/user"
os.environ.pop("HERMES_HOME", None)
os.environ.pop("SUDO_PASSWORD", None)

VERBS = [
    "rm -rf /", "rm -rf /*", "rm -rf ~", "rm -rf $HOME", "rm -rf \"$HOME\"", "rm -rf /etc", "rm -rf /usr/", "rm -rf /home/user", "rm -fr /", "rm --recursive --force /",
    "rm -rf /tmp/x", "rm -rf ./build", "rm file", "mkfs.ext4 /dev/sda1", "mkfs /dev/sdb", "dd if=/dev/zero of=/dev/sda", "dd if=a of=b", "echo x > /dev/sda", "cat f > /dev/nvme0n1",
    ":(){ :|:& };:", "kill -9 -1", "kill -9 123", "shutdown -h now", "reboot", "halt", "poweroff", "init 0", "init 6", "systemctl poweroff", "systemctl status x", "telinit 6",
    "rm -rf C:/", "rm -rf C:\\", "Remove-Item -Recurse -Force C:\\", "Format-Volume -DriveLetter C", "Clear-Disk -Number 0", "Stop-Computer", "Restart-Computer -WhatIf", "format c:",
    "rd /s /q C:\\", "del /s /q C:\\*", "echo reboot", "ls /reboot",
]
CONTEXTS = [
    "{v}", "sudo {v}", "sudo -u root {v}", "nice {v}", "timeout 5 {v}", "env X=1 {v}", "env -i {v}", "bash -c '{v}'", "sh -c \"{v}\"", "bash -lc '{v}'",
    "echo ok; {v}", "echo ok && {v}", "false || {v}", "({v})", "{{ {v}; }}", "echo $({v})", "echo `{v}`", "eval '{v}'", "eval {v}", "echo '{v}'", "echo \"{v}\"",
    "git commit -m '{v}'", "grep '{v}' f", "cat <<'EOF'\n{v}\nEOF", "cat <<EOF\n{v}\nEOF", "python <<'EOF'\n{v}\nEOF", "bash <<'EOF'\n{v}\nEOF", "if true; then {v}; fi",
    "xargs {v}", "nohup {v} &", "doas {v}", "command {v}", "busybox {v}", "env -S '{v}'", "su -c '{v}' root", "pwsh -c \"{v}\"", "powershell -Command {v}", "{v} # note",
    "echo a\n{v}", "echo 'a\n{v}'", "x=1 {v}", "2>/dev/null {v}", "sudo nice timeout 5 {v}", "sh -c 'sh -c \"{v}\"'", "ssh host '{v}'", "watch {v}", "time {v}", "exec {v}",
]
OBFUSCATED = [
    "r''m -rf /", "r\\m -rf /", "\"rm\" -rf /", "'rm' -rf '/'", "$'rm' -rf /", "$'\\x72\\x6d' -rf /", "rm${IFS}-rf${IFS}/", "rm$IFS-rf$IFS/", "{rm,-rf,/}", "$(echo rm) -rf /",
    "/bin/rm -rf /", "RM -RF /", "rm -r -f /", "rm -rf -- /", "rm -rf //", "rm -rf /./", "rm -rf /../", "rm -rf \"/\"", "rm -rf '/'", "rm -rf /\"\"", "rm -rf \"/\"etc",
    "rm -rf \"/\"\"etc\"", "rm -rf /etc/..", "rm -rf /etc/../*", "rm \\\n-rf /", "rm -rf \\\n/", "sh\\\nutdown now", "shut''down now", "re\"boot\"", "r\\eboot", "$'reboot'", "\"reboot\"",
    "mkfs.\\ext4 /dev/sda", "d''d if=/dev/zero of=/dev/sda", "dd if=/dev/zero \"of=/dev/sda\"", "kill -9 -1;", "k''ill -9 -1", "'kill' '-9' '-1'", "sudo -S reboot", "sudo -n reboot",
    "ls; reboot", "ls\nreboot", "ls;\n\nreboot", "ls;reboot", "ls&&reboot", "ls||reboot", "ls|reboot", "echo hi | sudo reboot", "a=$(reboot)", "a=`reboot`", "echo \"$(reboot)\"",
    "echo '$(reboot)'", "echo \\$(reboot)", "echo \"a\\\"; reboot\"", "echo 'it'\"'\"'s'; reboot", "cd /; rm -rf *", "cd / && rm -rf .", "rm -rf /home/*", "rm -rf ~/*", "rm -rf ${HOME}/",
    "rm -rf $HOME/", "rm -rf ~/ ", "rm -rf ~/.cache", "rm -rf ~user", "echo y | rm -rf /", "yes | rm -rf /", "find / -delete", "rm -rf / --no-preserve-root", "rm --no-preserve-root -rf /",
    "sudo rm -rf --no-preserve-root /", "bash -c \"$(echo 'rm -rf /')\"", "echo cm0gLXJmIC8= | base64 -d | sh", "exec bash -c 'reboot'", "source <(echo reboot)", ". /dev/stdin <<< reboot",
]

TARGETED = [
    "cat \"f\\\"n.txt\"; rm -rf /", "echo \"a\\\"b\"; reboot", "echo 'a'\\''b'; rm -rf /", "git push -o ci\\;skip -f; reboot", "git -C my\\ repo push -f && reboot",
    "echo \"unterminated; reboot", "echo 'x; reboot", "echo x\\; reboot", "echo x \\\n; reboot", "x=\"a b\" reboot", "FOO='a;b' reboot", "grep -P 'a|reboot' f", "grep -P 'a' f; reboot",
    "grep -o \"[^\\\"]*\" f", "echo \"$(grep -P 'x' f | cut -d: -f1)\" ; reboot", "ls -l C:\\Users\\me; rm -rf C:\\", "cmd /c rd /s /q C:\\", "cmd.exe /c \"rd /s /q C:\\\"",
    "powershell -NoProfile -Command \"Remove-Item -Recurse -Force C:\\\"", "pwsh -c 'ri -r -fo C:/'", "pwsh -c \"Stop-Computer -WhatIf\"", "pwsh -c \"Stop-Computer\"", "pwsh -c \"Clear-Disk 0 -WhatIf; Format-Volume C\"",
    "pwsh -File Stop-Computer.ps1", "echo Stop-Computer", "git commit -m \"never format c:\"", "echo \"format c:\"", "format c:", "a; format d: /q", "rm -rf C:\\build", "rm -rf C:/*", "rm -rf \"C:\\\"", "rd /s /q \"C:\\\"",
    "eval eval eval eval eval eval eval eval eval 'reboot'", "eval \"$(echo reboot)\"", "eval 'eval \"eval reboot\"'", "bash -c 'bash -c \"bash -c reboot\"'", "bash -c 'bash -c \"bash -c \\\"bash -c reboot\\\"\"'",
    "env -S 'nice reboot'", "env -S'bash -c reboot'", "su -c 'rm -rf /' root", "runuser -u x -c 'reboot'", "su root -c reboot", "su - -c \"reboot\"", "xargs -I{} sh -c 'reboot'", "find . -exec sh -c 'reboot' \\;",
    "cat > j.sh <<'EOF'\nreboot\nEOF\nbash j.sh", "cat <<'EOF' | bash\nreboot\nEOF", "cat <<'EOF'\nit's\nEOF\nreboot", "cat <<'EOF'\nit's\nEOF\nls; reboot", "python3 <<'EOF'\nimport os\nos.system('reboot')\nEOF",
    "python -c \"import os; os.system('reboot')\"", "node -e 'x'; reboot", "perl -e 'system(\"reboot\")'", "(reboot)", "((reboot))", "{ reboot; }", "! reboot", "if true; then reboot; fi", "while true; do reboot; done",
    "for i in 1; do reboot; done", "case x in x) reboot;; esac", "true &&\nreboot", "true ||\n\n  reboot", "echo a |\nreboot", "echo a &\nreboot", "ls ;; reboot", "ls ;&; reboot", "\treboot", "  reboot  ", "\n\nreboot",
    "r$()eboot", "r${x}eboot", "r$(echo)eboot", "$(echo re)boot", "`echo re`boot", "re''boot", "re\"\"boot", "REBOOT", "Reboot", "/sbin/reboot", "./reboot", "C:/Windows/System32/shutdown.exe /s", "shutdown.exe /r", "shutdown-helper", "reboot.sh",
    "echo \"reboot\" | sh", "printf reboot | bash", "echo reboot | sudo bash", "bash <(echo reboot)", "sh < <(echo reboot)", "sh <<< reboot", "sh <<< 'rm -rf /'",
    "rm -rf /ho''me", "rm -rf /h\\ome", "rm -rf '/'etc", "rm -rf \"/\"/etc", "rm -rf /etc\"\"", "rm -rf $'/etc'", "rm -rf ${HOME}", "rm -rf $HOME/..", "rm -rf ~/.", "rm -rf ~/..", "rm -rf ~root/", "rm -rf /home/user/..",
    "dd of=/dev/sda if=/dev/zero", "dd if=/dev/zero bs=1M of=/dev/sda1", "dd if=/dev/zero of=/dev/null", "echo x >/dev/sda", "echo x>/dev/sda", "echo x 2>/dev/sda", "cat f >> /dev/sda", "echo 'x > /dev/sda'", "echo \"cat f > /dev/sda\"", "bash -c 'cat f > /dev/sda'",
    ":(){ :|: & };:", "echo ':(){ :|:& };:'", "bash -c ':(){ :|:& };:'", "f(){ f|f& };f", "kill -1", "kill -9 -1", "kill -KILL -1", "kill -s KILL -1", "echo 'kill -9 -1'", "kill -9 -- -1", "pkill -9 -1",
]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.path.insert(0, str(Path(sys.argv[1]).resolve()))
    from tools.approval_detection import detect_hardline_command  # noqa: E402

    commands = []
    for verb in VERBS:
        for context in CONTEXTS:
            commands.append(context.replace("{{", "\x00").replace("}}", "\x01").replace("{v}", verb).replace("\x00", "{").replace("\x01", "}"))
    commands += OBFUSCATED
    commands += TARGETED
    for verb in OBFUSCATED[:12]:
        for context in CONTEXTS[:12]:
            commands.append(context.replace("{{", "\x00").replace("}}", "\x01").replace("{v}", verb).replace("\x00", "{").replace("\x01", "}"))
    commands = list(dict.fromkeys(commands))
    rows = []
    for command in commands:
        blocked, description = detect_hardline_command(command)
        rows.append({"command": command, "hardline": [bool(blocked), description]})
    body = ",\n".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) for r in rows)
    Path(sys.argv[2]).write_text("[\n" + body + "\n]\n", encoding="utf-8")
    print(f"{len(rows)} commands, {sum(r['hardline'][0] for r in rows)} blocked")


if __name__ == "__main__":
    main()
