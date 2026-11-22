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


# --- harder tasks

def t_debug():
    files = {
        "shop/__init__.py": "",
        "shop/pricing.py": "def apply_discount(price, percent):\n    \"\"\"Price after taking percent off.\"\"\"\n    return price * percent / 100\n",
        "shop/inventory.py": "class Inventory:\n    def __init__(self, stock):\n        self.stock = dict(stock)\n\n    def reserve(self, sku, qty):\n        if self.stock.get(sku, 0) > qty:\n            self.stock[sku] -= qty\n            return True\n        return False\n",
        "shop/cart.py": "from shop.pricing import apply_discount\n\nclass Cart:\n    def __init__(self, inventory):\n        self.inventory = inventory\n        self.lines = []\n\n    def add(self, sku, qty, price):\n        if self.inventory.reserve(sku, qty):\n            self.lines.append((sku, qty, price))\n            return True\n        return False\n\n    def total(self, discount=0):\n        cents = sum(qty * price for _, qty, price in self.lines)\n        return int(apply_discount(cents, discount) * 100) / 100\n",
        "tests/__init__.py": "",
        "tests/test_shop.py": (
            "import unittest\nfrom shop.cart import Cart\nfrom shop.inventory import Inventory\nfrom shop.pricing import apply_discount\n\n"
            "class Shop(unittest.TestCase):\n"
            "    def test_discount(self):\n        self.assertEqual(apply_discount(200, 25), 150)\n"
            "    def test_reserve_all_stock(self):\n        inv = Inventory({'a': 3})\n        self.assertTrue(inv.reserve('a', 3))\n        self.assertFalse(inv.reserve('a', 1))\n"
            "    def test_cart_total(self):\n        cart = Cart(Inventory({'a': 5, 'b': 5}))\n        self.assertTrue(cart.add('a', 2, 3.33))\n        self.assertTrue(cart.add('b', 1, 1.10))\n        self.assertEqual(cart.total(), 7.76)\n"
            "    def test_cart_discount(self):\n        cart = Cart(Inventory({'a': 5}))\n        cart.add('a', 4, 2.5)\n        self.assertEqual(cart.total(10), 9.0)\n"
            "    def test_out_of_stock(self):\n        cart = Cart(Inventory({'a': 1}))\n        self.assertFalse(cart.add('a', 2, 1))\n"
            "if __name__ == '__main__':\n    unittest.main()\n"),
    }
    import hashlib
    digest = hashlib.sha256(files["tests/test_shop.py"].encode()).hexdigest()
    return files, "Тесты в tests/ падают (запуск: python3 -m unittest discover). Найди все причины и исправь код. Сами тесты не меняй.", \
        lambda p, out: (sh(p, "python3 -m unittest discover").returncode == 0
                        and hashlib.sha256((p / "tests/test_shop.py").read_bytes()).hexdigest() == digest, "")


def t_bigfile():
    lines = ['"""Generated operations."""', ""]
    for i in range(60):
        lines += [f"def op_{i}(x):", f'    """Operation number {i}."""', f"    y = x + {i}", f"    # step {i}: keep the value as is", "    return y", "", ""]
    original = "\n".join(lines)

    def check(p, out):
        text = (p / "ops.py").read_text()
        ns = {}
        exec(compile(text, "ops.py", "exec"), ns)
        good = ns["op_17"](2) == 34 and ns["op_23"](2, scale=3) == 75 and ns["op_23"](2) == 25 and ns["op_5"](1) == 6 and ns["op_59"](1) == 60
        before, after = set(original.splitlines()), text.splitlines()
        changed = sum(1 for line in after if line not in before)
        return good and changed <= 8, f"changed lines: {changed}"
    return {"ops.py": original}, "В ops.py: функцию op_17 сделай умножающей (return x * 17), а в op_23 добавь параметр scale=1, на который умножается результат. Остальные функции не трогай, файл не переписывай целиком.", check


def t_git():
    files = {"calc.py": "def add(a, b):\n    return a + b\n", "test_calc.py": "import unittest\nfrom calc import add, mul\nclass T(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n    def test_mul(self):\n        self.assertEqual(mul(4, 5), 20)\nunittest.main()\n"}

    def prepare(project):
        run = lambda c: subprocess.run(c, shell=True, cwd=project, check=True, capture_output=True)
        run("git init -q -b main && git config user.email t@example.com && git config user.name Tester")
        run("git add -A && git commit -q -m 'add calc'")
        (project / "calc.py").write_text("def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a * b\n")
        run("git add -A && git commit -q -m 'add mul'")
        (project / "calc.py").write_text("def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a + b\n")
        run("git add -A && git commit -q -m 'tidy helpers'")
        (project / "notes.txt").write_text("todo\n")
        run("git add -A && git commit -q -m 'notes'")
        return {"first": run("git rev-list --max-parents=0 HEAD").stdout.decode().strip(), "head": run("git rev-parse HEAD").stdout.decode().strip()}

    def check(p, out, state=None):
        git = lambda c: sh(p, c).stdout.strip()
        ok = (sh(p, "python3 test_calc.py").returncode == 0 and git("git branch --show-current") == "fix/regression"
              and git("git rev-list --count HEAD") == "5" and sh(p, f"git merge-base --is-ancestor {state['head']} HEAD").returncode == 0
              and git("git status --porcelain --untracked-files=no") == "")
        return ok, ""
    return files, "Тесты падают. Через историю git найди коммит, который сломал mul, исправь, и закоммить исправление в новой ветке fix/regression. Историю не переписывай.", check, prepare


def t_resume():
    files = {"config.ini": "[server]\nport = 7741\nhost = localhost\n"}

    def between(project):
        (project / "config.ini").write_text("[server]\nport = 1234\nhost = localhost\n")

    def check(p, out, state=None):
        return "7741" in out and "port = 9090" in (p / "config.ini").read_text(), ""
    return files, ["Прочитай config.ini и запомни значение port. Ничего не меняй.",
                   "Какой port ты видел в config.ini в самом начале? Назови число. Затем открой файл и замени текущий port на 9090."], check, None, between


def journal_events(project):
    path = project.parent / "journal.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def t_delegate():
    files = {"a/__init__.py": "", "a/m.py": "def double(x):\n    return x + x + 1\n", "a/test_m.py": "import unittest\nfrom a.m import double\nclass T(unittest.TestCase):\n    def test(self):\n        self.assertEqual(double(4), 8)\nunittest.main()\n",
             "b/__init__.py": "", "b/m.py": "def last(items):\n    return items[len(items)]\n", "b/test_m.py": "import unittest\nfrom b.m import last\nclass T(unittest.TestCase):\n    def test(self):\n        self.assertEqual(last([1, 2, 3]), 3)\nunittest.main()\n"}

    def check(p, out):
        events = journal_events(p)
        called = any(e["event"] == "tool" and e["name"] == "delegate_task" and e.get("child") is None for e in events)
        children = {e["child"] for e in events if e.get("child") is not None}
        passed = sh(p, "PYTHONPATH=. python3 a/test_m.py").returncode == 0 and sh(p, "PYTHONPATH=. python3 b/test_m.py").returncode == 0
        return called and children >= {0, 1} and passed, f"called={called} children={sorted(children)}"
    return files, "В каталогах a и b по одному падающему тесту (PYTHONPATH=. python3 a/test_m.py, PYTHONPATH=. python3 b/test_m.py). Используй инструмент delegate_task: один подагент на каталог, параллельно. Затем проверь оба теста сам и кратко отчитайся.", check


def t_execute_code():
    files = {f"data/part{i}.txt": "".join(f"{i}-{j}: {'ERROR' if (i * 7 + j) % 5 == 0 else 'ok'}\n" for j in range(40)) for i in range(12)}

    def check(p, out):
        events = journal_events(p)
        called = any(e["event"] == "tool" and e["name"] == "execute_code" for e in events)
        expected = sum(1 for i in range(12) for j in range(40) if (i * 7 + j) % 5 == 0)
        return called and str(expected) in out, f"called={called} expected={expected}"
    return files, "В каталоге data 12 файлов part*.txt. Подсчитай, сколько всего строк со словом ERROR. Обязательно используй инструмент execute_code (один вызов, цикл по файлам), в ответе назови число.", check


TASKS = {f.__name__[2:]: f for f in (t_fix, t_rename, t_newdir, t_search, t_longout, t_cd, t_indent, t_deny, t_debug, t_bigfile, t_git, t_resume, t_delegate, t_execute_code)}


def run_task(name, binary, model, env, timeout, provider="custom"):
    spec = TASKS[name]()
    files, prompts, check = spec[:3]
    prepare = spec[3] if len(spec) > 3 else None
    between = spec[4] if len(spec) > 4 else None
    prompts = [prompts] if isinstance(prompts, str) else prompts
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
    state = prepare(project) if prepare else None
    started = time.monotonic()
    session, out, code = None, "", 0
    for index, prompt in enumerate(prompts):
        if index and between:
            between(project)
        command = [binary, "chat", "--oneshot", "--yolo", "--debug-log", str(journal), "-q", prompt]
        if len(prompts) > 1:
            command += ["--resume", session] if session else ["--save-session"]
        try:
            run = subprocess.run(command, cwd=project, env=run_env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout)
            out, code = run.stdout, run.returncode
        except subprocess.TimeoutExpired as error:
            out, code = (error.stdout or b"").decode() if isinstance(error.stdout, bytes) else (error.stdout or ""), "timeout"
            break
        marker = [line for line in (out + "\n" + run.stderr).splitlines() if line.startswith("Session: ")]
        if marker:
            session = marker[-1].split(": ", 1)[1].strip()
    seconds = time.monotonic() - started
    events = [json.loads(l) for l in journal.read_text().splitlines()] if journal.exists() else []
    tools = [e for e in events if e["event"] == "tool"]
    errors = [e for e in events if e["event"] == "provider_error"]
    usage = [e["usage"] for e in events if e["event"] == "response" and e.get("usage")]
    try:
        ok, note = check(project, out, state) if prepare else check(project, out)
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
