#!/usr/bin/env python3
"""Live task suite: runs the hermes CLI on small real tasks against a real provider and checks the
results by running code, not by reading the model's words.

    compat/live_suite.py --model deepseek-flash-1017 [--only name,...] [--binary target/debug/hermes]

The provider comes from the environment (LLM_GATEWAY_URL, GATEWAY_API_KEY) or from the repository's
untracked .env. Each task runs in its own temporary profile and project; keys are never printed.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_env():
    env = dict(os.environ)
    path = ROOT / ".env"
    if path.exists():
        for line in path.read_text().splitlines():
            if "=" in line and not line.lstrip().startswith("#"):
                key, value = line.split("=", 1)
                env.setdefault(key.strip(), value.strip().strip("'\""))
    return env


def sh(cwd, command):
    return subprocess.run(command, shell=True, cwd=cwd, capture_output=True, text=True, timeout=60)


# --- tasks: (name, files, prompt, check(project, final_text) -> (ok, note))

def t_fix():
    files = {"pkg/__init__.py": "", "pkg/calc.py": "def average(values):\n    return sum(values) / (len(values) - 1)\n",
             "test_calc.py": "import unittest\nfrom pkg.calc import average\nclass T(unittest.TestCase):\n    def test(self):\n        self.assertEqual(average([2, 4, 6]), 4)\nunittest.main()\n"}
    return files, "Тест test_calc.py падает. Найди причину, исправь код и убедись, что тест проходит.", \
        lambda p, out: (sh(p, "python3 test_calc.py").returncode == 0, "")


def t_rename():
    files = {"app/__init__.py": "", "app/users.py": "def fetch_user(uid):\n    return {'id': uid}\n",
             "app/views.py": "from app.users import fetch_user\n\ndef show(uid):\n    return fetch_user(uid)['id']\n",
             "app/api.py": "from app import users\n\ndef get(uid):\n    return users.fetch_user(uid)\n",
             "README.md": "Use `fetch_user(uid)` to load a user.\n",
             "check.py": "from app.views import show\nfrom app.api import get\nassert show(3) == 3 and get(4)['id'] == 4\nprint('ok')\n"}
    return files, "Переименуй функцию fetch_user в load_user во всём проекте (код и документация). Затем запусти python3 check.py.", \
        lambda p, out: (sh(p, "grep -rn fetch_user . --include=*.py --include=*.md").stdout == "" and sh(p, "python3 check.py").stdout.strip() == "ok", "")


def t_newdir():
    files = {"test_slug.py": "import unittest\nfrom text.util.slug import slugify\nclass T(unittest.TestCase):\n    def test(self):\n        self.assertEqual(slugify('  Hello, World! Ünï  '), 'hello-world-uni')\n        self.assertEqual(slugify('a  b'), 'a-b')\nunittest.main()\n"}
    return files, "Реализуй slugify в новом модуле text/util/slug.py так, чтобы test_slug.py проходил (нижний регистр, латиница без диакритики, небуквенно-цифровые символы в один дефис, без дефисов по краям). Создай нужные пакеты.", \
        lambda p, out: (sh(p, "python3 test_slug.py").returncode == 0, "")


def t_search():
    files = {f"src/m{i}.py": f"def f{i}():\n    return {i}\n" for i in range(30)}
    files["src/loader.py"] = "DEFAULTS = {'retries': 3}\n\ndef parse_settings(text):\n    timeout = 47\n    return timeout\n"
    return files, "В каком файле находится функция parse_settings и какой в ней таймаут? Ответь коротко: файл и число.", \
        lambda p, out: ("loader.py" in out and "47" in out, "")


def t_longout():
    files = {"noisy.sh": "i=0; while [ $i -lt 30000 ]; do echo \"progress line $i\"; i=$((i+1)); done\necho 'FATAL: disk quota exceeded on volume vol-9137' >&2\nexit 2\n"}
    return files, "Запусти bash noisy.sh. Он завершился ошибкой; скажи точное сообщение об ошибке.", \
        lambda p, out: ("vol-9137" in out, "")


def t_cd():
    files = {"build/Makefile": "all:\n\t@echo built-$$(basename $$(pwd)) > out.txt\n", "notes.txt": "x\n"}
    return files, "Зайди в каталог build, выполни там make, затем (отдельной командой) выведи содержимое out.txt в текущем каталоге и скажи, что там.", \
        lambda p, out: ((p / "build/out.txt").exists() and "built-build" in out, "")


def t_indent():
    files = {"conf.py": "class Config:\n\tdef __init__(self):\n\t\tself.name = 'old'\n\t\tself.debug = False\n\n\tdef show(self):\n\t\tprint(self.name)\n"}
    return files, "В conf.py поменяй значение debug на True и name на 'new'. Остальное не трогай.", \
        lambda p, out: (sh(p, "python3 -c \"import conf; c=conf.Config(); assert c.debug is True and c.name=='new'\"").returncode == 0
                         and "\t\tself.debug = True" in (p / "conf.py").read_text(), "")


def t_deny():
    files = {"keep/data.txt": "important\n"}
    return files, "Удали каталог keep командой rm -rf keep и скажи, получилось ли.", \
        lambda p, out: ((p / "keep/data.txt").exists(), "rm -rf must be blocked by approvals.deny")


TASKS = {f.__name__[2:]: f for f in (t_fix, t_rename, t_newdir, t_search, t_longout, t_cd, t_indent, t_deny)}


def run_task(name, binary, model, env, timeout, provider="custom"):
    files, prompt, check = TASKS[name]()
    base = Path(tempfile.mkdtemp(prefix=f"live-{name}-"))
    project, profile = base / "proj", base / "profile"
    project.mkdir(); profile.mkdir()
    for rel, text in files.items():
        (project / rel).parent.mkdir(parents=True, exist_ok=True)
        (project / rel).write_text(text)
    (profile / "config.yaml").write_text(
        f"model: {{default: {model}, provider: {provider}, base_url: '{env['LLM_GATEWAY_URL'].rstrip('/')}/v1'}}\n"
        "approvals:\n  deny: ['git push*', 'rm -rf*']\n")
    journal = base / "journal.jsonl"
    run_env = {k: v for k, v in os.environ.items() if k not in ("LLM_GATEWAY_URL", "GATEWAY_API_KEY")}
    run_env.update(HERMES_HOME=str(profile), HERMES_API_KEY=env["GATEWAY_API_KEY"])
    started = time.monotonic()
    try:
        run = subprocess.run([binary, "chat", "--oneshot", "--yolo", "--debug-log", str(journal), "-q", prompt],
                             cwd=project, env=run_env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout)
        out, code = run.stdout, run.returncode
    except subprocess.TimeoutExpired as error:
        out, code = (error.stdout or b"").decode() if isinstance(error.stdout, bytes) else (error.stdout or ""), "timeout"
    seconds = time.monotonic() - started
    events = [json.loads(l) for l in journal.read_text().splitlines()] if journal.exists() else []
    tools = [e for e in events if e["event"] == "tool"]
    errors = [e for e in events if e["event"] == "provider_error"]
    usage = [e["usage"] for e in events if e["event"] == "response" and e.get("usage")]
    try:
        ok, note = check(project, out)
    except Exception as error:  # a broken check is a failed task, not a crashed suite
        ok, note = False, f"check raised {error!r}"
    return {"task": name, "ok": bool(ok), "exit": code, "seconds": round(seconds, 1), "requests": sum(e["event"] == "request" for e in events),
            "tool_calls": len(tools), "tool_failures": sum(not t["succeeded"] for t in tools),
            "tools": [t["name"] for t in tools], "provider_errors": len(errors),
            "input_tokens": sum(u["input_tokens"] for u in usage), "output_tokens": sum(u["output_tokens"] for u in usage), "note": note, "dir": str(base)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--binary", default=str(ROOT / "target/debug/hermes"))
    parser.add_argument("--only", default="")
    parser.add_argument("--provider", default="custom", help="custom (chat completions) or anthropic (messages API)")
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--json", default="")
    args = parser.parse_args()
    env = load_env()
    if "GATEWAY_API_KEY" not in env or "LLM_GATEWAY_URL" not in env:
        sys.exit("LLM_GATEWAY_URL and GATEWAY_API_KEY are required")
    names = [n for n in args.only.split(",") if n] or list(TASKS)
    results = []
    for name in names:
        result = run_task(name, args.binary, args.model, env, args.timeout, args.provider)
        results.append(result)
        print(f"{'PASS' if result['ok'] else 'FAIL'} {name:8} {result['seconds']:6}s req={result['requests']} tools={result['tool_calls']} "
              f"failed={result['tool_failures']} err={result['provider_errors']} in={result['input_tokens']} out={result['output_tokens']} {result['note']}", flush=True)
    print(f"{sum(r['ok'] for r in results)}/{len(results)} passed on {args.model} via {args.provider}")
    if args.json:
        Path(args.json).write_text(json.dumps(results, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()
