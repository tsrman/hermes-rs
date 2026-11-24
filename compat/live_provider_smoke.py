#!/usr/bin/env python3
"""Cloud provider live smoke runner for `hsr`.

Exercises each configured API mode (OpenAI Chat Completions, OpenAI/Codex Responses, Anthropic
Messages, AWS Bedrock Converse, Google Vertex AI, NVIDIA NIM, xAI, Nous, or local gateway) with
a single minimal `--oneshot` turn against an isolated temporary profile.

Credentials are read from the process environment or the untracked repository `.env` and are
NEVER printed.

Examples:
    python3 compat/live_provider_smoke.py --dry-run
    python3 compat/live_provider_smoke.py --provider anthropic --provider openai-codex
    python3 compat/live_provider_smoke.py --mock-url http://127.0.0.1:8080/v1 --model fixture
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = ROOT / "target" / "debug" / "hsr"

# (name, provider_flag, default_model, env_keys_any, extra_args)
PROVIDER_MATRIX = [
    (
        "gateway",
        "custom",
        "deepseek-flash-1017",
        ("LLM_GATEWAY_URL", "GATEWAY_API_KEY"),
        [],
    ),
    (
        "openai",
        "openai",
        "gpt-4o-mini",
        ("OPENAI_API_KEY",),
        [],
    ),
    (
        "openai-codex",
        "openai-codex",
        "gpt-5.3-codex",
        ("CODEX_HOME",),
        [],
    ),
    (
        "anthropic",
        "anthropic",
        "claude-sonnet-4-5",
        ("ANTHROPIC_API_KEY", "ANTHROPIC_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"),
        [],
    ),
    (
        "bedrock",
        "bedrock",
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ("AWS_BEARER_TOKEN_BEDROCK", "AWS_ACCESS_KEY_ID"),
        [],
    ),
    (
        "vertex",
        "vertex",
        "gemini-2.5-flash",
        ("VERTEX_ACCESS_TOKEN", "VERTEX_CREDENTIALS_PATH", "GOOGLE_APPLICATION_CREDENTIALS"),
        [],
    ),
    (
        "nvidia",
        "nvidia",
        "nvidia/llama-3.1-nemotron-70b-instruct",
        ("NVIDIA_API_KEY",),
        [],
    ),
    (
        "xai",
        "xai",
        "grok-3-mini",
        ("XAI_API_KEY",),
        [],
    ),
]


def load_env() -> dict[str, str]:
    env = dict(os.environ)
    env_file = ROOT / ".env"
    if env_file.is_file():
        for line in env_file.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#") or "=" not in stripped:
                continue
            k, v = stripped.split("=", 1)
            env.setdefault(k.strip(), v.strip().strip("'\""))
    return env


def has_credentials(name: str, env_keys: tuple[str, ...], env: dict[str, str]) -> bool:
    if name == "gateway":
        return bool(env.get("LLM_GATEWAY_URL") and env.get("GATEWAY_API_KEY"))
    if name == "openai-codex":
        codex_auth = Path(env.get("CODEX_HOME") or (Path.home() / ".codex")) / "auth.json"
        hsr_auth = Path(env.get("HSR_HOME") or (Path.home() / ".hsr")) / "auth.json"
        if codex_auth.is_file():
            return True
        if hsr_auth.is_file():
            try:
                data = json.loads(hsr_auth.read_text(encoding="utf-8"))
                return bool(data.get("providers", {}).get("openai-codex"))
            except Exception:
                return False
        return False
    return any(bool(env.get(k, "").strip()) for k in env_keys)


def run_smoke_turn(
    binary: Path,
    name: str,
    provider: str,
    model: str,
    env: dict[str, str],
    base_url: str | None = None,
    timeout: int = 90,
) -> tuple[bool, str]:
    with tempfile.TemporaryDirectory(prefix=f"hsr-smoke-{name}-") as tmp:
        profile = Path(tmp) / "profile"
        work = Path(tmp) / "work"
        profile.mkdir()
        work.mkdir()
        (profile / "config.yaml").write_text(
            f"model:\n  provider: {provider}\n  default: {model}\n",
            encoding="utf-8",
        )
        run_env = dict(env)
        run_env["HSR_HOME"] = str(profile)
        cmd = [
            str(binary),
            "chat",
            "--oneshot",
            "-q",
            "Reply with exactly: OK",
            "--model",
            model,
        ]
        if base_url:
            cmd.extend(["--base-url", base_url])
        elif name == "gateway" and env.get("LLM_GATEWAY_URL"):
            gw = env["LLM_GATEWAY_URL"].rstrip("/")
            if not gw.endswith("/v1"):
                gw += "/v1"
            cmd.extend(["--base-url", gw])
            run_env["HERMES_API_KEY"] = env.get("GATEWAY_API_KEY", "")

        proc = subprocess.run(
            cmd,
            cwd=work,
            env=run_env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        out = proc.stdout.strip()
        if proc.returncode == 0 and out:
            preview = out.splitlines()[0][:80]
            return True, f"OK ({preview})"
        err = (proc.stderr.strip() or out or f"exit {proc.returncode}").splitlines()[-1][:160]
        return False, f"FAIL (rc={proc.returncode}): {err}"


def main() -> int:
    parser = argparse.ArgumentParser(description="Run live cloud-provider smoke checks for hsr.")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY, help="Path to hsr binary.")
    parser.add_argument(
        "--provider",
        action="append",
        dest="providers",
        help="Run only specified provider(s) (repeatable).",
    )
    parser.add_argument("--model", help="Override model name for selected provider(s).")
    parser.add_argument(
        "--mock-url",
        help="Run a single deterministic smoke check against a local OpenAI-compatible mock URL.",
    )
    parser.add_argument("--dry-run", action="store_true", help="List detected providers without calling APIs.")
    parser.add_argument("--timeout", type=int, default=90, help="Per-provider timeout in seconds.")
    args = parser.parse_args()

    if not args.binary.is_file():
        print(f"Binary not found: {args.binary}. Build with `cargo build --locked -p hermes-cli`.", file=sys.stderr)
        return 2

    env = load_env()
    if args.mock_url:
        env = dict(env)
        env["HERMES_API_KEY"] = env.get("HERMES_API_KEY") or "smoke-mock-key"
        model = args.model or "fixture"
        if args.dry_run:
            print(f"[dry-run] mock -> {args.mock_url} ({model})")
            return 0
        ok, note = run_smoke_turn(
            args.binary,
            "mock",
            "custom",
            model,
            env,
            base_url=args.mock_url,
            timeout=args.timeout,
        )
        print(f"mock ({model}): {note}")
        return 0 if ok else 1

    wanted = set(args.providers or [])
    ran = 0
    failures = 0
    for name, provider_flag, default_model, env_keys, _ in PROVIDER_MATRIX:
        if wanted and name not in wanted:
            continue
        ready = has_credentials(name, env_keys, env)
        model = args.model or default_model
        if not ready:
            if wanted:
                print(f"{name}: SKIP (no credentials configured)")
                failures += 1
            else:
                print(f"{name}: skip (no credentials)")
            continue
        if args.dry_run:
            print(f"[dry-run] {name}: ready (provider={provider_flag}, model={model})")
            ran += 1
            continue
        ran += 1
        ok, note = run_smoke_turn(
            args.binary,
            name,
            provider_flag,
            model,
            env,
            timeout=args.timeout,
        )
        print(f"{name} ({model}): {note}")
        if not ok:
            failures += 1

    if ran == 0 and failures == 0:
        print("No cloud provider credentials detected; nothing executed.")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
