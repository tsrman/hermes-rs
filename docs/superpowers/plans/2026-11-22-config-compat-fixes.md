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
5. **(headers done; TLS options deliberately not) `providers.*.extra_headers`, `model.default_headers`, `ssl_verify`, `ssl_ca_cert`, `session_affinity_header`** were ignored
   [keys]. Headers carry credentials for some gateways; never log them. Plan: send, redact in journal.
6. **(transports done, fallbacks not) `extra_body` for non-chat transports and for fallbacks**; the reference applies
   `request_overrides` to every transport.
7. **(compression done) `auxiliary.<task>.*` is not read at all** (`provider`, `model`, `base_url`, `api_key`, `timeout`,
   `extra_body`, `reasoning_effort`) [keys]. The port runs compression on the main route. Plan: read
   `auxiliary.compression` and route the summary call through it; other tasks (vision, approval, title…)
   have no counterpart in the port yet and are listed under P3.

### P2: documented settings that change behaviour the port already has

8. **Compression keys.** The port reads `enabled`, `threshold`, `threshold_tokens`, `protect_first_n`,
   `protect_last_n`, `checkpoint_required`, `in_place`, `model.context_length`. Not read [keys]:
   `target_ratio`, `tail_mode`, `min_tail_user_messages`, `max_attempts`, `abort_on_summary_failure`,
   `model_thresholds`, `proactive_prune_*`, `micro_compact*`, `idle_compact_after_seconds`. Each needs a
   check of whether the reference code path exists in a one-shot CLI before porting.
9. **(provider window done; scoping deliberately not) `model.context_length` scoping.** The reference drops it when the startup runtime differs from the
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
12. **(done) `tool_output.max_bytes/max_lines/max_line_length`**, **`file_read_max_chars`**, **`context_file_max_chars`
    / `context_file_read_timeout`** [keys, the last is read]: the port's limits are constants.
13. **(switch done; PII and phone options not) `security.redact_secrets`, `privacy.redact_pii`, `security.redact_phone_numbers`.** The port always
    redacts secrets and has no switch [keys]. Decide whether to add the switch; defaulting to the
    reference's default is mandatory either way.
14. **`terminal.*`** read: `backend`, `env_passthrough`, timeouts for code. Not read [keys]: `cwd`,
    `timeout`, `home_mode`, `shell_init_files`, `auto_source_bashrc`, `persistent_shell`, docker/ssh/modal
    settings (remote backends do not exist in the port).
15. **`approvals.*`** read: `mode`, `timeout`, `allow`, `deny`. Not read [keys]: `cron_mode`,
    `unattended_mode`, `smart_policy`, `denial_breaker_threshold`, `approvers`, `command_allowlist`
    (top-level). `command_allowlist` is the persistent allow list the reference writes when the user
    answers "always"; the port keeps grants only for the session.
16. **(route done) `delegation.*`** read: `max_concurrent_children`, `max_iterations`, `oneshot_max_children`,
    `subagent_auto_approve`. Not read [keys]: `model`, `provider`, `base_url`, `api_key`, `api_mode`,
    `request_overrides`, `reasoning_effort`, `child_timeout_seconds`, `max_summary_chars`,
    `max_spawn_depth`, `orchestrator_enabled`, `inherit_mcp_toolsets`. Children therefore always use the
    parent's route.
17. **`toolsets` / `platform_toolsets` / `agent.disabled_toolsets`**: the port reads `platform_toolsets`
    and `agent.disabled_toolsets`; top-level `toolsets` is not read [keys].
18. **`skills.*`**: only part of the settings are read; `external_dirs`, `project_discovery`,
    `trusted_project_dirs`, `auto_load`, `template_vars`, `inline_shell` are not [keys].
19. **(prefill and terminal.timeout done) Top-level `prefill_messages_file`, `quick_commands`, `personalities`, `display.personality`,
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

Status 2026-11-22: steps 1 and 2 are done (see the tables above; fallbacks and TLS options excepted, and noted
as such). Next is step 3.

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

## Notes on headers and transports (done)

- `extra_headers` of the first enabled entry for the endpoint URL, and `model.default_headers` beneath it
  (the entry wins per header name), go on every request of every transport (chat completions, Responses,
  Anthropic). Values are converted like the reference's `str()` (numbers and booleans as text, null
  dropped). They are never written to the debug journal or printed; a test checks both.
- `extra_body` reaches the Responses and Anthropic transports too, gated like the reference on a `custom`
  provider (or a named provider). Side requests (the compression summary) carry the headers but not the
  body fields, on all three transports.
- **Not done on purpose:** `ssl_verify: false` and `ssl_ca_cert`. The port's rule is to verify TLS
  against the system roots for every API request (stage 2 of the migration plan). Honouring
  `ssl_verify: false` would weaken that for a config key; it needs an explicit decision, and a
  `ssl_ca_cert` option needs the same review. `session_affinity_header` is not read either.
- **Not done:** fallback routes get no `extra_body`/headers (the reference's rule for fallbacks was not
  recorded yet).

## Notes on `auxiliary.compression` and context windows (done)

- `auxiliary.compression` (`provider`, `model`, `base_url`, `api_key`, `key_env`/`api_key_env`,
  `api_mode`, `timeout`, `extra_body`) decides where the compression summary is made. Nothing set, or
  `provider`/`model: auto`, means the main route with the main model, as in the reference. Only a
  `base_url` means `custom`. A summary on another endpoint than the main one needs `model` (the port
  has no per-provider default auxiliary model table); on the same endpoint the key, URL and transport
  are inherited. Its `extra_body` goes only to the summary request, never the provider entry's, and
  matching `extra_headers` apply. Other tasks (`vision`, `approval`, `title_generation`, ...) have no
  counterpart in the port yet.
- `providers.<entry>.context_length` (entry-wide) and `providers.<entry>.models.<model>.context_length`
  (per model, name compared as written, case-sensitive like the reference) now set the compression
  window when `model.context_length` is not set. Checked against `get_custom_provider_context_length`.
- **Not done on purpose:** dropping `model.context_length` when the run uses another model than
  `model.default` (`--model`, `/model`). The reference then looks the window up from model metadata;
  the port has none, and without any window compression would turn off entirely, overflow recovery
  included. The configured window is kept instead; a `providers` entry window for the active model
  is only used when `model.context_length` is absent.
- Still open in item 8: `target_ratio`, `tail_mode`, `min_tail_user_messages`, `max_attempts`,
  `abort_on_summary_failure`, `model_thresholds`, proactive pruning and micro-compaction. Each changes
  which messages are summarized, so each needs the reference's boundary selection recorded first.

## Finding while recording the default request (item 11): the default system prompt was a different, much shorter text (fixed, see the end of this section)

Recorded by running the reference (`compat/run_reference_turn.py`, model `fixture`, CLI platform) and the
port against the same local server, then diffing the first request's system message. The reference sends
about 8200 characters; the port sends about 1340 [run]. The port's identity, tool and platform paragraphs
are hand-written early text; only the memory paragraph and (when present) skills, SOUL, project
instructions and memory blocks come from the reference.

What the reference adds that the port lacks (in its order):
1. Identity paragraph (`Be direct: match the length of your reply…`) instead of the port's one line.
2. Docs pointer ("For help with Hermes Agent itself…").
3. `# Finishing the job`, `# Parallel tool calls` (gated by `agent.task_completion_guidance`,
   `agent.parallel_tool_call_guidance`), the `skill_manage` workflow hint.
4. `## Mid-turn user steering` (only when the surface can steer; the reference omits it for one-shot runs).
   The port has no steering channel, so this must stay out.
5. The coding-agent block ("You are a coding agent pairing with the user…": gather context, change through
   tools, verify and know when to stop, respect the repo), then the **Workspace** snapshot (root, branch,
   status, recent commits, project manifest, context file names) taken from git at session start.
6. The CLI platform hint (the longer "plain terminal" text), the active-profile line, the conversation
   date, `Model:`, `Provider:`, `Platform:`, and the `# Hermes runtime environment` block (host kind such
   as WSL, home, cwd, scratch directory).
7. Model-gated paragraphs: `TOOL_USE_ENFORCEMENT_GUIDANCE` (`agent.tool_use_enforcement`, `auto` picks by
   model name) and the execution guidance (`agent.execution_guidance`, `auto` includes DeepSeek, Kimi and
   Qwen class models) with its Google/Gemma extra.

Why it matters: the live suite passes with the short prompt, but the reference behaviour (verify before
claiming done, never fabricate tool output, batch independent calls, stop after three failed lint fixes)
comes from these paragraphs, so a profile moved from Python to Rust would behave differently.

Plan (a project of its own, about 2700 lines of reference code in `agent/system_prompt.py` and
`agent/prompt_builder.py`):
1. A fixture generator that records each static paragraph and each gate result from the reference for a
   grid of (model, provider, platform, tool set, config) and the assembled prompts for sample cases.
2. Port the paragraphs and gates into `hermes-core/src/prompt.rs` and `hermes-runtime/src/prompt.rs`,
   keeping the resume rule (a saved session keeps its prompt bytes).
3. Workspace snapshot: git probe with the reference's limits and caching, honouring
   `agent.coding_context`.
4. Runtime environment block (WSL/host detection, scratch directory under the profile cache).
5. Switch the default prompt over in one commit, update the tests that quote prompt text, re-run the live
   suite on `deepseek-flash-1017` to check nothing regressed.

Decision needed before step 5: the port's prompt is currently shorter, which also costs fewer tokens per
turn; matching the reference adds about 2000 tokens to every request (more with the workspace block).

### Result: the system prompt is now assembled like the reference (done)

- `crates/hermes-runtime/src/system_prompt.rs` builds the three tiers (stable, context, volatile) in the
  reference's order. The paragraphs are recorded from the reference (`compat/create_prompt_fixture.py` writes
  `src/prompt_text.json` and `tests/fixtures/prompt_cases.json`); the module holds the conditions.
- Gates ported and checked against the reference's own functions on 260 (setting, model) pairs, 9 tool sets
  for the execution guidance, and 20 models for the coding brief: `agent.tool_use_enforcement`,
  `agent.execution_guidance` (booleans, boolean words, model lists, `auto`), `agent.task_completion_guidance`,
  `agent.parallel_tool_call_guidance` (Python truthiness), `agent.coding_context` (`auto`/`on`/`off`; `focus`
  acts as `auto`, its toolset collapse is not ported), `agent.coding_instructions`, `agent.environment_hint`
  and `HERMES_ENVIRONMENT_HINT`, `platform_hints.cli` (`replace`/`append`), `timezone` and `HERMES_TIMEZONE`.
- The workspace snapshot (root, branch with upstream and ahead/behind, linked worktree note, status counts,
  last three commits, manifests and package managers, verify commands, context files) runs `git` hardened for
  untrusted repositories: no prompts, no repository-configured fsmonitor, hooks, pagers or external diff, every
  `filter.*` command overridden (the status probe is skipped when filters cannot be discovered), the user's
  `safe.directory` entries kept, 2.5 s limit.
- Runtime environment block: host (WSL, Linux with kernel release, macOS), home, working directory, the profile
  scratch directory `cache/scratch` (owner-only, entries idle for 24 h removed at most hourly) which commands now
  really get as `TMPDIR`/`TMP`/`TEMP`, the WSL hint and the embedder hint. The memory guidance now has the
  `skill_manage` variant ("Skills come first") when that tool is offered, as recorded from the reference.
- **Differential check** `compat/compare_prompt.py <reference> <reference python> [--model M] [--layout L]`
  runs the reference agent and the CLI in the same temporary git project against the local server and diffs
  the first system message after removing the steering section and the profile path: identical for the models
  `fixture`, `deepseek-flash-1017`, `gpt-5`, `claude-sonnet-5-5`, `gemini-2.5-pro`, `qwen3-coder` and the
  layouts repo, plain directory, dirty repo; with a Node/Make/pytest project only the `AGENTS.md` context
  differs (the reference runner skips context files). Live suite on `deepseek-flash-1017`: 14 of 14 and about
  as fast as before (the prompt costs about 2000 more input tokens per request).
- **Not ported (intended):** the mid-turn steering section (no steering channel; the reference drops it for
  one-shot runs too); the Python toolchain probe line (`agent.environment_probe`); Bot Mode and Bot Screen
  blocks; remote-backend environment text; plugin sections; pinned `skills.auto_load` skills; the dynamic
  `context_file_max_chars` cap derived from the model window (the port keeps 20000 unless configured); the
  rule that `Conversation started` keeps the session's original date when a compaction rebuilds the prompt
  (the port uses the rebuild day).

## Notes on output limits and the redaction switch (done)

- `tool_output.max_bytes` (default 50000), `max_lines` (2000), `max_line_length` (2000) and
  `file_read_max_chars` (100000) are read with the reference's coercion (`int(value) >= 1`, else the default;
  `file_read_max_chars` only for positive numbers; 16 inputs checked against `_coerce_positive_int`).
  `max_lines` and `max_line_length` and `file_read_max_chars` shape `read_file`; `max_bytes` shapes terminal
  output.
- **Terminal output now truncates like the reference** (it did not before: 16 KiB head and 48 KiB tail of
  bytes with a different notice). Characters, not bytes: 40% from the start and 60% from the end of the limit,
  with `... [OUTPUT TRUNCATED - N chars omitted out of T total] ...` between. 35 cases (ASCII, lines, Cyrillic,
  emoji, short text; budgets from 10 to 5000) from `truncate_head_tail`, fed in chunks of 1 to 100000 bytes,
  give identical text. The wrapper's final-directory line is cut off before the count, so it never counts as
  output. Not ported: the spill file with the omitted middle, and the same limit for `execute_code` output
  and MCP results (they keep their own limits).
- `security.redact_secrets` and `HERMES_REDACT_SECRETS` (environment, then profile `.env`, then config; true
  unless it says otherwise, parsed as `1/true/yes/on`) switch redaction of terminal output, file reads and
  search results off. The stored-conversation text of `session_search`, the debug journal and CDP results stay
  redacted, as the reference's forced path does. A warning is printed at startup when it is off.
  `privacy.redact_pii` and `security.redact_phone_numbers` are still not read.

## Notes on `delegation.*` route (done) and the approvals finding (open, needs a decision)

- `delegation.model`, `provider`, `base_url`, `api_key`, `api_mode` pick the route subagents run on, with the
  reference's rules: only `model` keeps the parent's endpoint and key; a bare `base_url` (provider empty,
  `custom` or `auto`) is a direct endpoint that keeps the parent's key unless `api_key` is set; a named
  provider resolves its own key; another endpoint without `model` is refused with a clear message. A pinned
  route never borrows the parent's fallback chain (`delegation.fallback_providers` is not read). The result
  of `delegate_task` names the child model. Live check: parent `deepseek-flash-1017`, subagent
  `claude-haiku-5-5` on the gateway. Still not read: `request_overrides`, `reasoning_effort`,
  `child_timeout_seconds`, `max_summary_chars`, `max_spawn_depth`, `orchestrator_enabled`,
  `inherit_mcp_toolsets`.
- **Approvals (item 15) is a design question, not a missing key.** The reference asks for approval only for
  commands its detector flags (`tools/approval_detection.py`: dangerous patterns and a hard-line list, and
  `command_allowlist` holds the pattern keys a person approved permanently). The port asks for every
  command and file change unless `approvals.allow`, the small built-in list or a session grant covers it.
  Porting the detector would let ordinary commands (`pip install`, `python script.py`, builds, tests) run
  without a question, which is how the reference feels in practice but is a weaker default than the port has
  now. Decision needed before any work: keep the stricter default and add `command_allowlist` as persistent
  `approvals.allow` entries, or port the detector and its patterns (large, security-critical, needs a recorded
  corpus like the redactor's).

## Notes on prefill messages, terminal.timeout, terminal.cwd (done / not applicable)

- `prefill_messages_file` (`HERMES_PREFILL_MESSAGES_FILE` first, then the top-level key, then
  `agent.prefill_messages_file`; a relative path from the profile directory, `~/` expanded) is a JSON array
  of message objects sent right after the system prompt on every request and never saved in the session,
  as in the reference. A missing file, a non-array or invalid JSON only prints a warning. Subagents get none.
- `terminal.timeout` (`TERMINAL_TIMEOUT` first) is the foreground timeout when a call gives none (1 to 600).
  The reference promotes an over-cap foreground timeout to a background run; the port still refuses it.
- `terminal.cwd` is not read on purpose: in the reference the local CLI leaves `TERMINAL_CWD` unset and uses
  the launch directory; the key is bridged only for the gateway and cron.
- `quick_commands` (user-defined slash commands that run shell scripts) and `personalities` /
  `display.personality` (an ephemeral system prompt chosen by name) are interactive-surface features. They
  belong with the REPL slash commands; `quick_commands` runs commands without a model call and so needs its own
  approval decision before it is ported.
