#!/usr/bin/env python3
"""Record the plain API-key provider profiles of the Python reference.

Usage: create_provider_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. Only profiles whose request handling is the stock
OpenAI-compatible path are kept: those that override message preparation, extra request
bodies or API kwargs need behaviour this runtime does not have.
"""

import json
import os
import sys
import tempfile
from pathlib import Path

SUPPORTED_MODES = {"chat_completions", "codex_responses"}
HOOKS = ("prepare_messages", "build_extra_body", "build_api_kwargs_extras")


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
        if any(getattr(cls, hook, None) is not getattr(ProviderProfile, hook, None) for hook in HOOKS):
            continue
        if profile.auth_type != "api_key" or profile.api_mode not in SUPPORTED_MODES or not profile.base_url:
            continue
        envs = list(profile.env_vars)
        rows.append({
            "name": profile.name,
            "aliases": list(profile.aliases),
            "api_mode": profile.api_mode,
            "base_url": profile.base_url,
            "key_envs": [e for e in envs if not e.endswith("_BASE_URL")],
            "base_url_envs": [e for e in envs if e.endswith("_BASE_URL")],
        })
    rows.sort(key=lambda row: row["name"])
    output.write_text(json.dumps(rows, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print([row["name"] for row in rows])


if __name__ == "__main__":
    main()
