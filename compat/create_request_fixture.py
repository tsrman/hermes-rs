#!/usr/bin/env python3
"""Record the request body the Python reference builds for given `agent.*` settings.

Usage: create_request_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. For every case the reference agent (CLI platform, `custom`
provider, a loopback base URL) builds the arguments of its first request with `_build_api_kwargs`;
the fixture keeps every top-level field except `messages`, `tools` and `stream_options`. The
reasoning config is resolved the way the CLI does it (`resolve_reasoning_config` over the loaded
config), the service tier through `_parse_service_tier_config`. Everything is synthetic.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from create_reference_fixture import REFERENCE_COMMIT

# (agent section as YAML, model)
CASES = [
    ("{}", "fixture"),
    ("{reasoning_effort: high}", "fixture"),
    ("{reasoning_effort: low}", "fixture"),
    ("{reasoning_effort: minimal}", "fixture"),
    ("{reasoning_effort: medium}", "fixture"),
    ("{reasoning_effort: xhigh}", "fixture"),
    ("{reasoning_effort: max}", "fixture"),
    ("{reasoning_effort: ultra}", "fixture"),
    ("{reasoning_effort: HIGH}", "fixture"),
    ("{reasoning_effort: ' High '}", "fixture"),
    ("{reasoning_effort: none}", "fixture"),
    ("{reasoning_effort: 'false'}", "fixture"),
    ("{reasoning_effort: false}", "fixture"),
    ("{reasoning_effort: disabled}", "fixture"),
    ("{reasoning_effort: off}", "fixture"),
    ("{reasoning_effort: 'off'}", "fixture"),
    ("{reasoning_effort: no}", "fixture"),
    ("{reasoning_effort: Off}", "fixture"),
    ("{reasoning_effort: [high]}", "fixture"),
    ("{reasoning_effort: {effort: High}}", "fixture"),
    ("{reasoning_effort: {effort: ultra}}", "fixture"),
    ("{reasoning_effort: {effort: 0}}", "fixture"),
    ("{reasoning_effort: {enabled: no, effort: high}}", "fixture"),
    ("{reasoning_overrides: {fixture: 'off'}}", "fixture"),
    ("{reasoning_overrides: {fixture: off}}", "fixture"),
    ("{reasoning_effort: bogus}", "fixture"),
    ("{reasoning_effort: ''}", "fixture"),
    ("{reasoning_effort: null}", "fixture"),
    ("{reasoning_effort: true}", "fixture"),
    ("{reasoning_effort: 3}", "fixture"),
    ("{reasoning_effort: {effort: fast}}", "fixture"),
    ("{reasoning_effort: {effort: high}}", "fixture"),
    ("{reasoning_effort: {enabled: false}}", "fixture"),
    ("{reasoning_effort: {enabled: true}}", "fixture"),
    ("{reasoning_overrides: {fixture: low}}", "fixture"),
    ("{reasoning_overrides: {fixture: low}, reasoning_effort: high}", "fixture"),
    ("{reasoning_overrides: {other: low}, reasoning_effort: high}", "fixture"),
    ("{reasoning_overrides: {fixture: none}, reasoning_effort: high}", "fixture"),
    ("{reasoning_overrides: {fixture: bogus}, reasoning_effort: high}", "fixture"),
    ("{reasoning_overrides: {fixture: bogus}}", "fixture"),
    ("{reasoning_overrides: {fixture: {effort: fast}}}", "fixture"),
    ("{reasoning_overrides: {'vendor/fixture': low}}", "fixture"),
    ("{reasoning_overrides: {'openai/fixture': low}}", "fixture"),
    ("{reasoning_overrides: {'openrouter/openai/fixture': low}}", "fixture"),
    ("{reasoning_overrides: {'glm-5-2': low}}", "glm-5.2"),
    ("{reasoning_overrides: {'glm-5.2': low}}", "glm-5-2"),
    ("{reasoning_overrides: {'glm-5.2': low}}", "zai/glm-5.2"),
    ("{reasoning_overrides: {'zai/glm-5.2': low}}", "glm-5.2"),
    ("{reasoning_overrides: {'glm-5.2': low}}", "GLM-5.2"),
    ("{reasoning_overrides: {'anthropic/claude-sonnet-4.5': high}}", "claude-sonnet-4-5"),
    ("{reasoning_overrides: {'ollama-local/qwen3.6:27b': none}}", "qwen3.6:27b"),
    ("{reasoning_overrides: [fixture]}", "fixture"),
    ("{reasoning_overrides: null, reasoning_effort: low}", "fixture"),
    ("{service_tier: fast}", "fixture"),
    ("{service_tier: auto}", "fixture"),
    ("{service_tier: cold}", "fixture"),
    ("{text_verbosity: high}", "fixture"),
    ("{reasoning_echo: true}", "fixture"),
    ("{stall_guards: false}", "fixture"),
    ("{verify_on_stop: true}", "fixture"),
    ("{image_input_mode: text}", "fixture"),
    ("{run_budget_seconds: 600}", "fixture"),
]
# The same settings on the Responses transport (`api_mode: codex_responses`).
RESPONSES_CASES = [
    ("{}", "fixture"),
    ("{text_verbosity: high}", "fixture"),
    ("{text_verbosity: low}", "fixture"),
    ("{text_verbosity: medium}", "fixture"),
    ("{text_verbosity: HIGH}", "fixture"),
    ("{text_verbosity: bogus}", "fixture"),
    ("{text_verbosity: ''}", "fixture"),
    ("{service_tier: fast}", "fixture"),
    ("{reasoning_effort: high}", "fixture"),
]
DROPPED = {"messages", "tools", "stream_options", "input", "instructions", "tool_choice", "parallel_tool_calls"}


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=reference, text=True).strip()
    if head != REFERENCE_COMMIT:
        raise SystemExit(f"expected upstream commit {REFERENCE_COMMIT}, got {head}")
    home = Path(tempfile.mkdtemp())
    os.environ["HERMES_HOME"] = str(home)
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import hermes_yaml as yaml
    from hermes_constants import resolve_reasoning_config
    from cli import _parse_service_tier_config
    from run_agent import AIAgent

    def record(cases, api_mode):
        rows = []
        for section, model in cases:
            config = {"agent": yaml.safe_load(section)}
            (home / "config.yaml").write_text(f"agent: {section}\n", encoding="utf-8")
            agent_section = config["agent"] if isinstance(config["agent"], dict) else {}
            agent = AIAgent(
                base_url="http://127.0.0.1:9/v1", api_key="fixture", provider="custom", api_mode=api_mode,
                model=model, platform="cli", quiet_mode=True, skip_memory=True, skip_context_files=True,
                skip_background_review=True,
                reasoning_config=resolve_reasoning_config(config, model),
                service_tier=_parse_service_tier_config(agent_section.get("service_tier", "")),
            )
            try:
                kwargs = agent._build_api_kwargs([{"role": "user", "content": "hi"}], tools_for_api=[])
                body = {k: v for k, v in kwargs.items() if k not in DROPPED}
                rows.append({"agent": section, "model": model, "body": json.loads(json.dumps(body, default=str))})
            finally:
                agent.close()
        return rows

    result = {"chat_completions": record(CASES, "chat_completions"),
              "codex_responses": record(RESPONSES_CASES, "codex_responses")}
    output.write_text(json.dumps(result, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print({key: len(rows) for key, rows in result.items()})


if __name__ == "__main__":
    main()
