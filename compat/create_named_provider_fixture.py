#!/usr/bin/env python3
"""Record how the reference resolves the runtime route for `model.provider` naming an entry of
`providers:` / `custom_providers:`.

Usage: create_named_provider_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter. Each case is a config.yaml; the environment holds MYK=from-env.
"""
import json, os, sys, tempfile

reference, output = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
sys.path.insert(0, reference)
os.chdir(reference)
CASES = {
 "named_key": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck, model: m}\n",
 "custom_colon": "model: {provider: 'custom:llm-api', default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck}\n",
 "key_env": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', key_env: MYK}\n",
 "api_key_env": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key_env: MYK}\n",
 "api_mode": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck, api_mode: codex_responses}\n",
 "transport": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck, transport: anthropic_messages}\n",
 "legacy_list": "model: {provider: 'custom:old', default: m}\ncustom_providers:\n  - {name: old, base_url: 'https://o/v1', api_key: ok}\n",
 "name_field": "model: {provider: 'custom:pretty-name', default: m}\nproviders:\n  k1: {name: Pretty Name, base_url: 'https://a/v1', api_key: ck}\n",
 "model_url_wins": "model: {provider: llm-api, default: m, base_url: 'https://override/v1'}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck}\n",
 "model_key_wins": "model: {provider: llm-api, default: m, api_key: mk}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck}\n",
 "disabled": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {enabled: false, base_url: 'https://a/v1', api_key: ck}\n",
 "unknown": "model: {provider: nothere, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck}\n",
 "url_alias": "model: {provider: llm-api, default: m}\nproviders:\n  llm-api: {api: 'https://a/v1', api_key: ck}\n",
 "entry_model_default": "model: {provider: llm-api}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck, default_model: dm}\n",
 "case": "model: {provider: LLM-API, default: m}\nproviders:\n  llm-api: {base_url: 'https://a/v1', api_key: ck}\n",
}

out = []
for name, config in CASES.items():
    home = tempfile.mkdtemp()
    os.environ["HERMES_HOME"] = home
    os.environ.pop("OPENAI_API_KEY", None)
    os.environ["MYK"] = "from-env"
    open(home + "/config.yaml", "w").write(config)
    for module in list(sys.modules):
        if module.startswith("hermes_cli"):
            del sys.modules[module]
    from hermes_cli.runtime_provider import resolve_runtime_provider
    try:
        runtime = resolve_runtime_provider(requested=None)
        expected = {key: runtime.get(key) for key in ("api_mode", "base_url", "api_key")}
    except BaseException as error:
        expected = {"error": type(error).__name__ + ": " + str(error)[:200]}
    out.append({"name": name, "config": config, "expected": expected})
with open(output, "w") as handle:
    json.dump(out, handle, indent=1)
    handle.write("\n")
print("wrote", output, len(out), "cases")
