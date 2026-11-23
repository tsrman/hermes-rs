#!/usr/bin/env python3
"""Record OpenAI Responses request body fields (`prompt_cache_key`, `reasoning`, `include`, `text`)
from the Python reference (`agent.transports.codex.ResponsesApiTransport.build_kwargs`).

Usage: create_responses_body_corpus.py <reference checkout> <output.json>
"""

import json
import sys
from pathlib import Path

SAMPLE_TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "zeta_tool",
            "description": "Second alphabetically",
            "parameters": {"type": "object", "properties": {"b": {"type": "string"}, "a": {"type": "integer"}}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "alpha_tool",
            "description": "First alphabetically",
            "parameters": {"type": "object", "properties": {"x": {"type": "boolean"}}},
        },
    },
]

REASONING_CASES = [
    # (label, provider, base_url, model, agent_yaml)
    ("default_custom_legacy", "custom", "http://127.0.0.1:9/v1", "fixture", "{}"),
    ("custom_effort_minimal", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: minimal}"),
    ("custom_effort_low", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: low}"),
    ("custom_effort_medium", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: medium}"),
    ("custom_effort_high", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: high}"),
    ("custom_effort_xhigh", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: xhigh}"),
    ("custom_effort_max_clamps_xhigh", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: max}"),
    ("custom_effort_ultra_clamps_xhigh", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: ultra}"),
    ("custom_effort_none", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: none}"),
    ("custom_effort_false", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: false}"),
    ("custom_effort_disabled_map", "custom", "http://127.0.0.1:9/v1", "fixture", "{reasoning_effort: {enabled: false}}"),
    ("custom_override_beats_global", "custom", "http://127.0.0.1:9/v1", "gpt-5.2-codex", "{reasoning_effort: high, reasoning_overrides: {gpt-5.2-codex: low}}"),
    # OpenAI legacy ladder (gpt-5.2-codex: none/low/medium/high/xhigh)
    ("openai_legacy_minimal_clamps_low", "openai", "https://api.openai.com/v1", "gpt-5.2-codex", "{reasoning_effort: minimal}"),
    ("openai_legacy_max_clamps_xhigh", "openai", "https://api.openai.com/v1", "gpt-5.2-codex", "{reasoning_effort: max}"),
    ("openai_legacy_ultra_clamps_xhigh", "openai", "https://api.openai.com/v1", "gpt-5.2-codex", "{reasoning_effort: ultra}"),
    ("openai_legacy_none", "openai", "https://api.openai.com/v1", "gpt-5.2-codex", "{reasoning_effort: none}"),
    # gpt-5.6 / gpt-6-sol / daybreak ladder (supports max)
    ("gpt56_effort_max", "custom", "http://127.0.0.1:9/v1", "gpt-5.6", "{reasoning_effort: max}"),
    ("gpt56_effort_ultra_clamps_max", "custom", "http://127.0.0.1:9/v1", "gpt-5.6", "{reasoning_effort: ultra}"),
    ("gpt56_effort_none", "custom", "http://127.0.0.1:9/v1", "gpt-5.6", "{reasoning_effort: none}"),
    ("gpt6_sol_effort_max", "custom", "http://127.0.0.1:9/v1", "gpt-6-sol-pro", "{reasoning_effort: max}"),
    ("daybreak_effort_max", "custom", "http://127.0.0.1:9/v1", "gpt-daybreak-blue-latest", "{reasoning_effort: max}"),
    # Astra / GPT-6.1-sol / GPT-6.2 (no-disable low..max on official OpenAI)
    ("gpt61_sol_disable_unprojectable", "openai", "https://api.openai.com/v1", "gpt-6.1-sol", "{reasoning_effort: none}"),
    ("gpt61_sol_high_all_turns", "openai", "https://api.openai.com/v1", "gpt-6.1-sol", "{reasoning_effort: high}"),
    ("gpt6_astra_openai_minimal_clamps_low", "openai", "https://api.openai.com/v1", "gpt-6-astra", "{reasoning_effort: minimal}"),
    ("gpt6_astra_openai_disable_unprojectable", "openai", "https://api.openai.com/v1", "gpt-6-astra", "{reasoning_effort: false}"),
    ("gpt6_astra_custom_relay_uses_legacy", "custom", "http://127.0.0.1:9/v1", "gpt-6-astra", "{reasoning_effort: max}"),
    ("gpt62_astra_openai_max", "openai", "https://api.openai.com/v1", "gpt-6.2-astra", "{reasoning_effort: ultra}"),
    # OpenAI api.openai.com all_turns & non-reasoning denylist
    ("openai_gpt54_all_turns", "openai", "https://api.openai.com/v1", "gpt-5.4", "{}"),
    ("openai_gpt54_mini_all_turns", "openai", "https://api.openai.com/v1", "gpt-5.4-mini", "{reasoning_effort: high}"),
    ("openai_gpt55_all_turns", "openai", "https://api.openai.com/v1", "gpt-5.5", "{}"),
    ("openai_gpt52_no_all_turns", "openai", "https://api.openai.com/v1", "gpt-5.2", "{}"),
    ("openai_o3_no_all_turns", "openai", "https://api.openai.com/v1", "o3", "{}"),
    ("custom_gpt54_no_all_turns", "custom", "http://127.0.0.1:9/v1", "gpt-5.4", "{}"),
    ("openai_gpt4o_rejects_reasoning", "openai", "https://api.openai.com/v1", "gpt-4o", "{}"),
    ("openai_gpt41_rejects_reasoning", "openai", "https://api.openai.com/v1", "gpt-4.1", "{reasoning_effort: high}"),
    ("openai_gpt35_rejects_reasoning", "openai", "https://api.openai.com/v1", "gpt-3.5-turbo", "{}"),
    ("openai_chatgpt_rejects_reasoning", "openai", "https://api.openai.com/v1", "chatgpt-4o-latest", "{}"),
    ("openai_ft_gpt4o_rejects_reasoning", "openai", "https://api.openai.com/v1", "ft:gpt-4o:org::123", "{}"),
    ("custom_relay_gpt4o_keeps_reasoning", "custom", "http://127.0.0.1:9/v1", "gpt-4o", "{}"),
    # xAI Responses
    ("xai_grok4_unlisted_no_reasoning", "xai", "https://api.x.ai/v1", "grok-4", "{}"),
    ("xai_grok3_mini_medium", "xai", "https://api.x.ai/v1", "grok-3-mini", "{}"),
    ("xai_grok43_xhigh_clamps_high", "xai", "https://api.x.ai/v1", "grok-4.3", "{reasoning_effort: xhigh}"),
    ("xai_grok46_xhigh_kept", "xai", "https://api.x.ai/v1", "grok-4.6", "{reasoning_effort: xhigh}"),
    ("xai_grok46_ultra_clamps_xhigh", "xai", "https://api.x.ai/v1", "grok-4.6-fast", "{reasoning_effort: ultra}"),
    ("xai_grok46_disabled_omits_all", "xai", "https://api.x.ai/v1", "grok-4.6", "{reasoning_effort: none}"),
    ("xai_text_verbosity_omitted", "xai", "https://api.x.ai/v1", "grok-4.6", "{text_verbosity: high}"),
    ("custom_xai_host_detected", "custom", "https://api.x.ai/v1", "grok-4.6", "{text_verbosity: high, reasoning_effort: high}"),
    # text_verbosity on non-xAI
    ("verbosity_high", "custom", "http://127.0.0.1:9/v1", "fixture", "{text_verbosity: HIGH}"),
    ("verbosity_low", "openai", "https://api.openai.com/v1", "gpt-5.4", "{text_verbosity: low}"),
]

CACHE_KEY_CASES = [
    # (label, provider, base_url, instructions, with_tools, session_id, override_key)
    ("no_session_with_tools", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", True, None, None),
    ("no_session_no_tools", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", False, None, None),
    ("with_session_and_tools", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", True, "sess-abc-123", None),
    ("with_session_no_tools", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", False, "sess-abc-123", None),
    ("cron_fire_1", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", True, "cron_job42_20261123_090000", None),
    ("cron_fire_2_same_key", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", True, "cron_job42_20261123_183045", None),
    ("override_short_kept", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", False, "sess-1", "custom-key-42"),
    ("override_long_hashed", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", False, "sess-1", "k" * 80),
    ("override_blank_dropped", "custom", "http://127.0.0.1:9/v1", "You are Hermes.", False, "sess-1", "   "),
    ("xai_with_session", "xai", "https://api.x.ai/v1", "You are Hermes.", True, "sess-xai-1", None),
    ("xai_without_session", "xai", "https://api.x.ai/v1", "You are Hermes.", True, None, None),
    ("xai_cron_session", "xai", "https://api.x.ai/v1", "You are Hermes.", False, "cron_xjob_20261123_010203", None),
]


def main() -> int:
    if len(sys.argv) != 3:
        sys.stderr.write("Usage: create_responses_body_corpus.py <reference checkout> <output.json>\n")
        return 2
    ref = Path(sys.argv[1]).resolve()
    out_path = Path(sys.argv[2]).resolve()
    sys.path.insert(0, str(ref))

    import hermes_yaml as yaml
    from hermes_constants import resolve_reasoning_config
    from agent.transports.codex import ResponsesApiTransport

    transport = ResponsesApiTransport()

    reasoning_out = []
    for label, provider, base_url, model, agent_yaml in REASONING_CASES:
        agent_section = yaml.safe_load(agent_yaml) or {}
        config = {"agent": agent_section}
        reasoning_config = resolve_reasoning_config(config, model)
        raw_verbosity = str(agent_section.get("text_verbosity") or "").strip().lower()
        text_verbosity = raw_verbosity if raw_verbosity in {"low", "medium", "high"} else None
        is_xai = provider in ("xai", "xai-oauth") or "api.x.ai" in base_url
        kwargs = transport.build_kwargs(
            model,
            [{"role": "system", "content": "sys"}, {"role": "user", "content": "hi"}],
            tools=[],
            provider=provider,
            base_url=base_url,
            is_xai_responses=is_xai,
            reasoning_config=reasoning_config,
            text_verbosity=text_verbosity,
        )
        reasoning_out.append({
            "label": label,
            "provider": provider,
            "base_url": base_url,
            "model": model,
            "agent": agent_yaml,
            "reasoning": kwargs.get("reasoning"),
            "include": kwargs.get("include"),
            "text": kwargs.get("text"),
        })

    cache_out = []
    for label, provider, base_url, instructions, with_tools, session_id, override_key in CACHE_KEY_CASES:
        is_xai = provider in ("xai", "xai-oauth") or "api.x.ai" in base_url
        overrides = {"prompt_cache_key": override_key} if override_key is not None else None
        kwargs = transport.build_kwargs(
            "fixture",
            [{"role": "system", "content": instructions}, {"role": "user", "content": "hi"}],
            tools=SAMPLE_TOOLS if with_tools else [],
            provider=provider,
            base_url=base_url,
            is_xai_responses=is_xai,
            session_id=session_id,
            request_overrides=overrides,
        )
        wire_key = (
            (kwargs.get("extra_body") or {}).get("prompt_cache_key")
            if is_xai
            else kwargs.get("prompt_cache_key")
        )
        headers = kwargs.get("extra_headers") or {}
        cache_out.append({
            "label": label,
            "provider": provider,
            "base_url": base_url,
            "instructions": instructions,
            "with_tools": with_tools,
            "session_id": session_id,
            "override_key": override_key,
            "prompt_cache_key": wire_key,
            "x_grok_conv_id": headers.get("x-grok-conv-id"),
        })

    payload = {
        "sample_tools": SAMPLE_TOOLS,
        "reasoning_cases": reasoning_out,
        "cache_key_cases": cache_out,
    }
    out_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Wrote {len(reasoning_out)} reasoning cases and {len(cache_out)} cache_key cases to {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
