#!/usr/bin/env python3
"""Run one reference-agent turn against the local model server."""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit

from create_reference_fixture import REFERENCE_COMMIT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("profile", type=Path)
    parser.add_argument("base_url")
    parser.add_argument("--max-turns", type=int, default=sys.maxsize)
    args = parser.parse_args()
    checkout = args.checkout.resolve(strict=True)
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=checkout, text=True).strip()
    if head != REFERENCE_COMMIT:
        parser.error(f"expected upstream commit {REFERENCE_COMMIT}, got {head}")
    url = urlsplit(args.base_url)
    if url.scheme != "http" or url.hostname != "127.0.0.1" or url.path != "/v1":
        parser.error("base_url must be http://127.0.0.1:<port>/v1")
    profile = args.profile.resolve()
    profile.mkdir(parents=True, exist_ok=False)
    os.environ["HERMES_HOME"] = str(profile)
    sys.path.insert(0, str(checkout))

    from run_agent import AIAgent

    agent = AIAgent(
        base_url=args.base_url,
        api_key="fixture",
        provider="custom",
        api_mode="chat_completions",
        model="fixture",
        platform="cli",
        max_iterations=args.max_turns,
        quiet_mode=True,
        skip_memory=True,
        skip_context_files=True,
        skip_background_review=True,
    )
    try:
        result = agent.run_conversation("Привет")
        print(json.dumps({
            "final_response": result["final_response"],
            "completed": result["completed"],
            "failed": result["failed"],
            "api_calls": result["api_calls"],
            "turn_exit_reason": result.get("turn_exit_reason"),
        }, ensure_ascii=False))
    finally:
        agent.close()


if __name__ == "__main__":
    main()
