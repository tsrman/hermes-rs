#!/usr/bin/env python3
"""Record how the Python reference reads the `compression.*`, `security.redact_phone_numbers` and
`privacy.redact_pii` settings, for the Rust tests in `crates/hermes-runtime/tests/config_keys.rs`.

Usage: create_config_keys_fixture.py <reference checkout> <output.json>
Run it with the reference interpreter: `<venv>/bin/python -I compat/create_config_keys_fixture.py ...`.
Everything below is synthetic. Each section records the reference's own answer:

* `thresholds`: a `compression:` YAML section goes through `_parse_compression_config`, then a real
  `ContextCompressor` for (model, provider, window); the trigger in tokens is what the port must reach.
* `compression_values`: what the same parse gives for `max_attempts`, `protect_first_n`, `protect_last_n`.
* `phone_policy`: a real `config.yaml` per case, read by `chat_keeps_phone_numbers` in a fresh process.
* `reply_scrub`: the stored assistant reply's scrub, `redact_assistant_reply` with the opt-in on and off,
  and with `HERMES_REDACT_SECRETS=false`.
* `pii`: `_should_redact_pii` for the CLI platform.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

A = "abcdefghijklmnop1234567890"

THRESHOLD_CASES = [
    # (compression YAML, model, provider, window)
    ("{}", "glm-5.2", "custom", 128000),
    ("{}", "glm-5.2", "custom", 1000000),
    ("{threshold: 0.9}", "gpt-5", "openrouter", 128000),
    ("{threshold: 0.9}", "gpt-5", "openrouter", 2000000),
    ("{model_thresholds: {glm-5.2: 0.4}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm-5.2: 0.4}}", "glm-5.2", "custom", 128000),
    ("{model_thresholds: {glm-5.2: 0.4}}", "gpt-5", "custom", 1000000),
    ("{model_thresholds: {glm: 0.4, glm-5: 0.6}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 0.4, glm-5: 0.6}}", "zai/glm-5.2", "openrouter", 1000000),
    ("{model_thresholds: {'openrouter:glm': 0.3, glm: 0.6}}", "glm-5.2", "openrouter", 1000000),
    ("{model_thresholds: {'openrouter:glm': 0.3, glm: 0.6}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {'openrouter:glm': 0.3, glm: 0.6}}", "glm-5.2", "OpenRouter", 1000000),
    ("{model_thresholds: {' OpenRouter :glm': 0.3}}", "glm-5.2", "openrouter", 1000000),
    ("{model_thresholds: {'openrouter:glm': 0.3, 'glm': 0.3}}", "glm-5.2", "openrouter", 1000000),
    ("{model_thresholds: {'openrouter:gl': 0.3, 'glm': 0.7}}", "glm-5.2", "openrouter", 1000000),
    # The scoped key sorts before the bare one as text, so only its rank lets it win.
    ("{model_thresholds: {'a:glm': 0.3, glm: 0.6}}", "glm-5.2", "a", 1000000),
    ("{model_thresholds: {'a:glm': 0.3, glm: 0.6}}", "glm-5.2", "b", 1000000),
    ("{model_thresholds: {GLM: 0.4}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 0.4, 'x:y': 0.2}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {'': 0.3}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 1.5}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 1}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 0}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: -0.5}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 0.1}}", "glm-5.2", "custom", 100000),
    ("{model_thresholds: {glm: '0.4'}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: true}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: null}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 0.4, other: bad}}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: [glm]}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: glm}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {5: 0.4}}", "gpt-5.2", "custom", 1000000),
    ("{threshold: 0.3, model_thresholds: {glm: 0.4}, threshold_tokens: 150000}", "glm-5.2", "custom", 1000000),
    ("{threshold: 0.3, model_thresholds: {glm: 0.4}, threshold_tokens: 500000}", "glm-5.2", "custom", 1000000),
    ("{model_thresholds: {glm: 0.4}, threshold_tokens: 70000}", "glm-5.2", "custom", 1000000),
    ("{threshold_tokens: 60000}", "glm-5.2", "custom", 128000),
    ("{threshold_tokens: 0}", "glm-5.2", "custom", 128000),
    ("{threshold_tokens: -5}", "glm-5.2", "custom", 128000),
    ("{threshold_tokens: '80000'}", "glm-5.2", "custom", 128000),
    ("{threshold_tokens: null}", "glm-5.2", "custom", 128000),
]

VALUE_CASES = [
    "{}",
    "{max_attempts: 1}", "{max_attempts: 6}", "{max_attempts: 10}", "{max_attempts: 11}", "{max_attempts: 99}",
    "{max_attempts: 0}", "{max_attempts: -2}", "{max_attempts: '7'}", "{max_attempts: ' 4 '}",
    "{max_attempts: true}", "{max_attempts: false}", "{max_attempts: 2.0}", "{max_attempts: 2.5}",
    "{max_attempts: abc}", "{max_attempts: null}", "{max_attempts: '3.0'}", "{max_attempts: [5]}",
    "{protect_first_n: 0}", "{protect_first_n: 1}", "{protect_first_n: 7}", "{protect_first_n: -1}",
    "{protect_first_n: '2'}", "{protect_first_n: 2.9}", "{protect_first_n: true}", "{protect_first_n: -2.5}",
    "{protect_last_n: 0}", "{protect_last_n: 5}", "{protect_last_n: '8'}", "{protect_last_n: 3.7}",
    "{protect_last_n: -4}",
]

REPLY_TEXTS = [
    "call +15551234567 now",
    "+4915112345678 and +1555123",
    "two: +15551234567, +442071838750.",
    "version +1.2.3 build +123456",
    "id 12345678 +",
    "(+15551234567)",
    "+15551234567abc",
    "x+15551234567",
    f"the key is sk-{A} and the phone is +15551234567",
    f"export OPENAI_API_KEY={A}",
    f"Authorization: Bearer {A}",
    f"token ghp_{A}",
    "plain text without anything",
    "",
    "Телефон +79161234567 и ключ " + f"sk-{A}",
    "https://user:hunter2hunter2@example.test/path call +15551234567",
]

PHONE_CONFIGS = [
    "security: {redact_phone_numbers: false}",
    "security: {redact_phone_numbers: true}",
    "security: {redact_phone_numbers: no}",
    "security: {redact_phone_numbers: off}",
    "security: {redact_phone_numbers: yes}",
    "security: {redact_phone_numbers: No}",
    "security: {redact_phone_numbers: NO}",
    "security: {redact_phone_numbers: Off}",
    "security: {redact_phone_numbers: OFF}",
    "security: {redact_phone_numbers: False}",
    "security: {redact_phone_numbers: FALSE}",
    "security: {redact_phone_numbers: n}",
    "security: {redact_phone_numbers: N}",
    "security: {redact_phone_numbers: 'no'}",
    "security: {redact_phone_numbers: 'off'}",
    "security: {redact_phone_numbers: On}",
    "security: {redact_phone_numbers: 'false'}",
    "security: {redact_phone_numbers: 'False'}",
    "security: {redact_phone_numbers: 0}",
    "security: {redact_phone_numbers: 1}",
    "security: {redact_phone_numbers: null}",
    "security: {redact_phone_numbers: ''}",
    "security: {redact_phone_numbers: [false]}",
    "security: {}",
    "security: null",
    "security: [false]",
    "privacy: {redact_pii: true}",
    "",
]

PHONE_PROBE = """
import json, sys
sys.path.insert(0, sys.argv[1])
from agent.redact_phone_policy import chat_keeps_phone_numbers
print(json.dumps(chat_keeps_phone_numbers()))
"""

DISABLED_PROBE = """
import json, sys
sys.path.insert(0, sys.argv[1])
from agent.redact_phone_policy import redact_assistant_reply
print(json.dumps([redact_assistant_reply(t) for t in json.loads(sys.argv[2])]))
"""


def run_probe(reference, code, args, env_extra, home):
    env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": home, **env_extra}
    done = subprocess.run([sys.executable, "-I", "-c", code, str(reference), *args], cwd=reference, env=env,
                          capture_output=True, text=True, timeout=120)
    if done.returncode != 0:
        raise SystemExit(f"probe failed: {done.stderr[-800:]}")
    return json.loads(done.stdout.strip().splitlines()[-1])


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    import hermes_yaml as yaml
    from agent.agent_init import _parse_compression_config
    from agent.context_compressor import ContextCompressor
    from agent.redact import redact_sensitive_text

    def parse(section):
        config = {"compression": yaml.safe_load(section)}
        return config, SimpleNamespace(model="m", provider="custom")

    thresholds = []
    for section, model, provider, window in THRESHOLD_CASES:
        config, agent = parse(section)
        agent.model, agent.provider = model, provider
        try:
            cs = _parse_compression_config(agent, config)
            compressor = ContextCompressor(
                model=model, threshold_percent=cs.threshold, protect_first_n=cs.protect_first,
                protect_last_n=cs.protect_last, quiet_mode=True, config_context_length=window,
                provider=provider, model_thresholds=cs.model_thresholds, threshold_tokens_cap=cs.threshold_tokens)
            compressor.update_model(model, window, provider=provider)
            result = {"threshold_tokens": compressor.threshold_tokens}
        except Exception as error:  # the reference refuses a config it cannot coerce
            result = {"error": type(error).__name__}
        thresholds.append({"compression": section, "model": model, "provider": provider, "window": window, **result})

    values = []
    for section in VALUE_CASES:
        config, agent = parse(section)
        try:
            cs = _parse_compression_config(agent, config)
            result = {"max_attempts": cs.max_attempts, "protect_first": cs.protect_first,
                      "protect_last": cs.protect_last}
        except Exception as error:
            result = {"error": type(error).__name__}
        values.append({"compression": section, **result})

    phone_policy = []
    for yaml_text in PHONE_CONFIGS:
        home = tempfile.mkdtemp()
        Path(home, "config.yaml").write_text(yaml_text + "\n", encoding="utf-8")
        keeps = run_probe(reference, PHONE_PROBE, [], {}, home)
        phone_policy.append({"yaml": yaml_text, "keeps": keeps})

    reply = {"on": [], "keep_phone": [], "disabled": []}
    for text in REPLY_TEXTS:
        reply["on"].append({"input": text, "output": redact_sensitive_text(text)})
        reply["keep_phone"].append({"input": text, "output": redact_sensitive_text(text, keep_phone_numbers=True)})
    home = tempfile.mkdtemp()
    disabled = run_probe(reference, DISABLED_PROBE, [json.dumps(REPLY_TEXTS)], {"HERMES_REDACT_SECRETS": "false"}, home)
    reply["disabled"] = [{"input": t, "output": o} for t, o in zip(REPLY_TEXTS, disabled)]

    from gateway.config import Platform
    from gateway.session import _should_redact_pii
    pii = [{"platform": platform.value, "enabled": enabled, "redacts": _should_redact_pii(platform, enabled)}
           for platform in (Platform.LOCAL, Platform.TELEGRAM, Platform.DISCORD) for enabled in (True, False)]

    output.write_text(json.dumps({"thresholds": thresholds, "compression_values": values,
                                  "phone_policy": phone_policy, "reply_scrub": reply, "pii": pii},
                                 indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print({"thresholds": len(thresholds), "values": len(values), "phone_policy": len(phone_policy),
           "reply": len(REPLY_TEXTS), "pii": len(pii)})


if __name__ == "__main__":
    main()
