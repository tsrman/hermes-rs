# Config handling: divergences from the reference and the plan to fix them

Source of truth: the reference checkout (`/tmp/hermes-main`), `hermes_cli/config.py` (`DEFAULT_CONFIG`,
`load_config`), `hermes_cli/config_providers.py`, `hermes_cli/runtime_provider.py`, `agent/agent_init.py`.
The port reads `config.yaml` in `crates/hermes-runtime/src/config.rs` and the `from_config` functions of the
modules that own a setting.

How each item was established is marked: **[run]** = reproduced by running the port, **[code]** = read
in both code bases, **[keys]** = only the key is absent from the port (`grep`), behaviour not compared yet.
Every fix below starts with a reference fixture (`compat/create_*.py`) or a reference run, then a port test.

## Already fixed (commit 41afbef)

| Item | Result |
|---|---|
| `model.api_key` was ignored; without `HERMES_API_KEY` the port stopped with "API key is required" | Read. Order: `--api-key` > `model.api_key` > `HERMES_API_KEY` > vendor env key. The reference ranks `model.api_key` above env keys too [run]. |
| `providers.<name>.extra_body` / `custom_providers[].extra_body` was ignored | Merged into chat-completions bodies for `provider: custom`. Matching as in `_custom_provider_extra_body_for_agent`: same base URL (trailing `/` ignored), entry `model`/`default_model` equals the model or is in `models`, entry without a model is the fallback, `enabled: false` skipped. Nested objects merge, other values replace. |
| Compression summary must not carry the provider `extra_body` | The reference sends only `auxiliary.compression.extra_body` there [code]. The port's summary call uses `complete_plain`. The iteration-limit summary is a main-agent call and keeps it. |

| `${VAR}` / `${env:VAR}` only in `mcp_servers` | Expanded in every string value of `config.yaml` (not keys), from the profile `.env`, then the process environment. Unset names, `${vault:X}` and empty names stay literal. Checked against `_expand_env_vars` on 11 inputs (unit test) and end to end (`base_url: ${SERVER}/v1`). |
| Named providers refused (`provider: llm-api`, `custom:llm-api`) | Resolved from `providers:` / `custom_providers:` by key or `name` (lower case, spaces as hyphens). The entry's `base_url` (also `url`, `api`), `api_key`, `key_env`/`api_key_env`, `api_mode`/`transport` (with the reference's aliases) and `model`/`default_model` are used, and win over `model.base_url`/`model.api_key`. `enabled: false` is an error. 15 cases recorded from `resolve_runtime_provider` (`compat/create_named_provider_fixture.py`, `tests/fixtures/named_providers.json`). Items 1–4 below are done. |

Known limits of that fix: only the Chat Completions transport gets `extra_body` (Responses and Anthropic
routes ignore it); `auxiliary.compression.extra_body` is not read; fallback routes get no `extra_body`.

## Divergences, by priority

### P1: a valid config silently does the wrong thing or is refused

1. **(done) Named providers are refused.** `model.provider: llm-api` or `custom:llm-api` (a key of `providers:`)
   fails with `provider '…' is not supported` [run]. The reference resolves them from
   `providers`/`custom_providers` (base URL, key, `api_mode`, model). Plan: resolve the name against the
   compat list built by `custom_providers()`; take `base_url`, `model`, `api_key`/`key_env`/`key_cmd`,
   `api_mode`/`transport` from the entry; fixture from `resolve_runtime_provider`.
2. **(done) `${VAR}` / `${env:VAR}` is expanded only in `mcp_servers`.** The reference expands every string value
   in the loaded config [code], so `model.api_key: ${MY_KEY}` or `base_url: ${HOST}/v1` reaches the port
   as literal text. Plan: expand once after parsing, for all values (not keys), from the profile `.env`
   then the process environment; unset variables stay literal as in the reference; test against
   `_expand_env_vars`.
3. **(done except `key_cmd`) `api_key_env` / `key_env` / `key_cmd` of the primary model and of `providers:` entries** are not
   read for the primary route (only for `fallback_providers`) [code]. Same fix as item 1.
4. **(done) `model.api_mode`** is honoured, but `providers.*.api_mode`/`transport` is not, so a Responses-only
   endpoint declared under `providers:` is called with Chat Completions [code].
5. **`providers.*.extra_headers`, `ssl_verify`, `ssl_ca_cert`, `session_affinity_header`** are ignored
   [keys]. Headers carry credentials for some gateways; never log them. Plan: send, redact in journal.
6. **`extra_body` for non-chat transports and for fallbacks** (see limits above); the reference applies
   `request_overrides` to every transport.
7. **`auxiliary.<task>.*` is not read at all** (`provider`, `model`, `base_url`, `api_key`, `timeout`,
   `extra_body`, `reasoning_effort`) [keys]. The port runs compression on the main route. Plan: read
   `auxiliary.compression` and route the summary call through it; other tasks (vision, approval, title…)
   have no counterpart in the port yet and are listed under P3.

### P2: documented settings that change behaviour the port already has

8. **Compression keys.** The port reads `enabled`, `threshold`, `threshold_tokens`, `protect_first_n`,
   `protect_last_n`, `checkpoint_required`, `in_place`, `model.context_length`. Not read [keys]:
   `target_ratio`, `tail_mode`, `min_tail_user_messages`, `max_attempts`, `abort_on_summary_failure`,
   `model_thresholds`, `proactive_prune_*`, `micro_compact*`, `idle_compact_after_seconds`. Each needs a
   check of whether the reference code path exists in a one-shot CLI before porting.
9. **`model.context_length` scoping.** The reference drops it when the startup runtime differs from the
   configured default (`_scope_context_length_to_default_runtime`), and reads `providers.*.context_length`
   [code]. The port applies it unconditionally and ignores the per-provider value.
10. **`model.max_concurrency`** is ignored. Harmless today (one model request at a time except
    `delegate_task` children); revisit when children are throttled per endpoint.
11. **`agent.*` prompt and loop settings** not read [keys]: `tool_use_enforcement`, `execution_guidance`,
    `task_completion_guidance`, `verify_guidance`, `verify_on_stop`, `max_verify_nudges`,
    `environment_hint`, `reasoning_overrides`, `reasoning_echo`, `service_tier`, `text_verbosity`,
    `run_budget_seconds`, `stall_guards`, `image_input_mode`. They change the system prompt or the request,
    so the first check is a recorded request from the reference for the default values (the defaults must
    match what the port sends now).
12. **`tool_output.max_bytes/max_lines/max_line_length`**, **`file_read_max_chars`**, **`context_file_max_chars`
    / `context_file_read_timeout`** [keys, the last is read]: the port's limits are constants.
13. **`security.redact_secrets`, `privacy.redact_pii`, `security.redact_phone_numbers`.** The port always
    redacts secrets and has no switch [keys]. Decide whether to add the switch; defaulting to the
    reference's default is mandatory either way.
14. **`terminal.*`** read: `backend`, `env_passthrough`, timeouts for code. Not read [keys]: `cwd`,
    `timeout`, `home_mode`, `shell_init_files`, `auto_source_bashrc`, `persistent_shell`, docker/ssh/modal
    settings (remote backends do not exist in the port).
15. **`approvals.*`** read: `mode`, `timeout`, `allow`, `deny`. Not read [keys]: `cron_mode`,
    `unattended_mode`, `smart_policy`, `denial_breaker_threshold`, `approvers`, `command_allowlist`
    (top-level). `command_allowlist` is the persistent allow list the reference writes when the user
    answers "always"; the port keeps grants only for the session.
16. **`delegation.*`** read: `max_concurrent_children`, `max_iterations`, `oneshot_max_children`,
    `subagent_auto_approve`. Not read [keys]: `model`, `provider`, `base_url`, `api_key`, `api_mode`,
    `request_overrides`, `reasoning_effort`, `child_timeout_seconds`, `max_summary_chars`,
    `max_spawn_depth`, `orchestrator_enabled`, `inherit_mcp_toolsets`. Children therefore always use the
    parent's route.
17. **`toolsets` / `platform_toolsets` / `agent.disabled_toolsets`**: the port reads `platform_toolsets`
    and `agent.disabled_toolsets`; top-level `toolsets` is not read [keys].
18. **`skills.*`**: only part of the settings are read; `external_dirs`, `project_discovery`,
    `trusted_project_dirs`, `auto_load`, `template_vars`, `inline_shell` are not [keys].
19. **Top-level `prefill_messages_file`, `quick_commands`, `personalities`, `display.personality`,
    `timezone`** (the last is used for the prompt date, the others not) [keys].
20. **`model_overrides`, `model_catalog`, `models_dev`** are not read [keys]; they feed context-window and
    pricing lookup.

### P3: outside the one-shot CLI, listed so they are not mistaken for gaps in the port

Gateway, dashboard, webapp, voice/TTS/STT, wake word, kanban, cron schedule options, Discord/Slack/Telegram
sections, `display.*` (the port prints plain text), `plugins`, `hooks`, `vault`/`secrets`, `lsp`,
`computer_use`, `proxy`, `desktop`, `monitoring`, `updates`, `onboarding`, `local_runtime`, `bedrock`,
`vertex`, `nous`. If a future port step adds one of these features, its config keys come with it.

## Validation behaviour

The reference merges `DEFAULT_CONFIG` under the user's file, migrates old shapes (`_config_version`),
warns about unknown or camelCase provider keys, and tolerates malformed `custom_providers` (skips the
legacy list). The port parses what it needs and returns an error for a malformed `model:` section.
Plan: decide per case between "error" and "warn and continue" after recording the reference's behaviour
for these inputs: `model:` as a list, `providers:` as a list, `custom_providers:` as a string, an
`extra_body` that is not a mapping, an `enabled` flag written as a string.

## Order of work

1. Items 2 and 1/3/4 together (env expansion, then named providers with their key, URL and mode); one
   reference fixture of `resolve_runtime_provider` over about 20 config shapes.
2. Item 5 (headers, TLS options) and item 6 (`extra_body` on every transport and on fallbacks).
3. Item 7 for `auxiliary.compression`, then items 8 and 9.
4. Items 11 to 13 after recording the reference's default request and system prompt, so a default config
   changes nothing.
5. Items 14 to 20 one by one, each starting with "does the reference code path run in a one-shot CLI".
6. The validation table.

A default config (no keys) must keep producing the requests it produces now; the existing recorded
fixtures in `crates/hermes-runtime/tests/fixtures` and the 134 compat tests guard that.

## Notes on the named-provider work

- `key_cmd` (a command that prints the key) is not run; an entry that relies on it has no key.
- For `api_mode: anthropic_messages` on a named provider the port selects its Anthropic transport and
  names the provider `anthropic` in the system prompt; the reference calls it `custom`. Check the prompt
  text if that matters for such a setup.
- The reference refuses an unknown provider with its own message; the port says `provider '…' is not
  supported`. Both are errors; only that an error occurs is compared.
