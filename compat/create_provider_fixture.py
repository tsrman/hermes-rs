#!/usr/bin/env python3
"""Record the plain API-key provider profiles of the Python reference.

Usage: create_provider_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. A profile is kept when its request handling is the stock
OpenAI-compatible path plus what its hooks add with no reasoning setting (the port reads no
`agent.reasoning_effort`): `build_api_kwargs_extras(reasoning_config=None)` is recorded as the
profile's default `extra_body` and `api_kwargs` (both land at the top level of the request body).
`model_dependent` marks profiles whose extras change with the model name; their model-specific
extras are not sent. Profiles that override `prepare_messages`, or whose extra body hook needs
per-request state (a session id), are kept without that part and say so in `unsent`.
"""

import json
import os
import sys
import tempfile
from pathlib import Path

SUPPORTED_MODES = {"chat_completions", "codex_responses"}
PROBE_MODELS = ["m", "gpt-4o", "deepseek-chat", "deepseek-reasoner", "glm-4.6", "glm-5", "kimi-k2", "kimi-k2-thinking",
                "qwen3-max", "anthropic/claude-sonnet-4", "openai/gpt-5", "gemini-2.5-pro", "llama-3.3-70b", "MiniMax-M2"]


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    from providers import list_providers
    from providers.base import ProviderProfile

    rows = []
    for profile in list_providers():
        cls = type(profile)
        overridden = {hook for hook in ("prepare_messages", "build_extra_body", "build_api_kwargs_extras")
                      if getattr(cls, hook, None) is not getattr(ProviderProfile, hook, None)}
        if "prepare_messages" in overridden:
            continue  # message rewriting is behaviour this runtime does not have
        if profile.auth_type != "api_key" or profile.api_mode not in SUPPORTED_MODES or not profile.base_url:
            continue
        unsent = []
        variants = {}
        for model in PROBE_MODELS:
            extra_body, api_kwargs = profile.build_api_kwargs_extras(reasoning_config=None, model=model)
            variants.setdefault(json.dumps([extra_body, api_kwargs], sort_keys=True), (extra_body, api_kwargs))
        baseline_body, baseline_kwargs = profile.build_api_kwargs_extras(reasoning_config=None, model="m")
        if len(variants) > 1:
            unsent.append("model-specific extras")
        if "build_extra_body" in overridden and profile.build_extra_body(session_id="s"):
            unsent.append("per-request extra body (session id)")
        envs = list(profile.env_vars)
        rows.append({
            "name": profile.name,
            "aliases": list(profile.aliases),
            "api_mode": profile.api_mode,
            "base_url": profile.base_url,
            "key_envs": [e for e in envs if not e.endswith("_BASE_URL")],
            "base_url_envs": [e for e in envs if e.endswith("_BASE_URL")],
            "extra_body": baseline_body,
            "api_kwargs": baseline_kwargs,
            "unsent": unsent,
        })
    rows.sort(key=lambda row: row["name"])
    output.write_text(json.dumps(rows, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print([row["name"] for row in rows])


if __name__ == "__main__":
    main()
