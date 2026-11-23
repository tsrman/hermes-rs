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

8. **(done: read or refused, see "Notes on the config keys of items 8, 11 and 13") Compression keys.** The port reads `enabled`, `threshold`, `threshold_tokens`, `protect_first_n`,
   `protect_last_n`, `checkpoint_required`, `in_place`, `model.context_length`. Not read [keys]:
   `target_ratio`, `tail_mode`, `min_tail_user_messages`, `max_attempts`, `abort_on_summary_failure`,
   `model_thresholds`, `proactive_prune_*`, `micro_compact*`, `idle_compact_after_seconds`. Each needs a
   check of whether the reference code path exists in a one-shot CLI before porting.
9. **(provider window done; scoping deliberately not) `model.context_length` scoping.** The reference drops it when the startup runtime differs from the
   configured default (`_scope_context_length_to_default_runtime`), and reads `providers.*.context_length`
   [code]. The port applies it unconditionally and ignores the per-provider value.
10. **`model.max_concurrency`** is ignored. Harmless today (one model request at a time except
    `delegate_task` children); revisit when children are throttled per endpoint.
11. **(done: read or refused, see the same notes) `agent.*` prompt and loop settings** not read [keys]: `tool_use_enforcement`, `execution_guidance`,
    `task_completion_guidance`, `verify_guidance`, `verify_on_stop`, `max_verify_nudges`,
    `environment_hint`, `reasoning_overrides`, `reasoning_echo`, `service_tier`, `text_verbosity`,
    `run_budget_seconds`, `stall_guards`, `image_input_mode`. They change the system prompt or the request,
    so the first check is a recorded request from the reference for the default values (the defaults must
    match what the port sends now).
12. **(done) `tool_output.max_bytes/max_lines/max_line_length`**, **`file_read_max_chars`**, **`context_file_max_chars`
    / `context_file_read_timeout`** [keys, the last is read]: the port's limits are constants.
13. **(done: switch, phone option; PII refused as gateway-only) `security.redact_secrets`, `privacy.redact_pii`, `security.redact_phone_numbers`.** The port always
    redacts secrets and has no switch [keys]. Decide whether to add the switch; defaulting to the
    reference's default is mandatory either way.
14. **(init files done; `persistent_shell` refused, see the notes) `terminal.*`** read: `backend`, `env_passthrough`, `timeout`, `shell_init_files`,
    `auto_source_bashrc`, timeouts for code. Not read [keys]: `cwd`, `home_mode`, `persistent_shell`,
    docker/ssh/modal settings (remote backends do not exist in the port). See the notes below.
15. **(detector and `command_allowlist` done) `approvals.*`** read: `mode`, `timeout`, `allow`, `deny`, and
    the top-level `command_allowlist`. Not read [keys]: `cron_mode`, `unattended_mode`, `smart_policy`,
    `denial_breaker_threshold`, `approvers`; `mode` is only read as `off` (no `smart`). See the section
    "Approvals: the reference's detector" below.
16. **(route done) `delegation.*`** read: `max_concurrent_children`, `max_iterations`, `oneshot_max_children`,
    `subagent_auto_approve`. Not read [keys]: `model`, `provider`, `base_url`, `api_key`, `api_mode`,
    `request_overrides`, `reasoning_effort`, `child_timeout_seconds`, `max_summary_chars`,
    `max_spawn_depth`, `orchestrator_enabled`, `inherit_mcp_toolsets`. Children therefore always use the
    parent's route.
17. **(done: no runtime effect) `toolsets` / `platform_toolsets` / `agent.disabled_toolsets`**: the port reads
    `platform_toolsets` and `agent.disabled_toolsets`. The top-level `toolsets` list is not used to pick the
    CLI's tools in the reference either: `_get_platform_tools` reads only `platform_toolsets`, and `toolsets`
    appears in `dump.py`, `debug_summary.py`, `left_core_migration.py` and a kanban default check
    (`tools_config.py:637`) [code]. Nothing to port.
18. **(done except `inline_shell`) `skills.*`**: `external_dirs`, `project_discovery`, `trusted_project_dirs`,
    `create_dir`, `disabled`, `index_desc_limit` were read already. `auto_load`, `template_vars` and the
    `skills.config.*` values are read now; `inline_shell` is read but deliberately never run (see the notes
    below).
19. **(prefill and terminal.timeout done) Top-level `prefill_messages_file`, `quick_commands`, `personalities`, `display.personality`,
    `timezone`** (the last is used for the prompt date, the others not) [keys].
20. **(`model_overrides.context_window` done) `model_overrides`, `model_catalog`, `models_dev`**: they feed
    context-window, capability and pricing lookup. The port has no model catalog and no pricing, so only
    `model_overrides.<provider>.<model>.context_window` (and `_default`) is read. Order for the compression
    window, as in `_config_override_context_length`: `model.context_length`, an explicit `model_overrides`
    entry (exact model name, then case-insensitive), the matching `providers` entry, then a `_default`
    (the provider's, then the global one; the reference applies it only on a catalog miss, which is every
    lookup here). Values are coerced like `int()` and must be positive. The provider key is the route's
    name (`custom` for custom and named endpoints); models.dev id aliases are not mapped. `supports_*`,
    `model_family` and the catalog keys change nothing the port does.

### P3: outside the one-shot CLI, listed so they are not mistaken for gaps in the port

Gateway, dashboard, webapp, voice/TTS/STT, wake word, kanban, cron schedule options, Discord/Slack/Telegram
sections, `display.*` (the port prints plain text), `plugins`, `hooks`, `vault`/`secrets`, `lsp`,
`computer_use`, `proxy`, `desktop`, `monitoring`, `updates`, `onboarding`, `local_runtime`, `bedrock`,
`vertex`, `nous`. If a future port step adds one of these features, its config keys come with it.

## Validation behaviour (recorded 2026-11-22, partly done)

Reference behaviour recorded with `~/ref/hermes-reference-venv` (Python 3.14, the reference's direct
dependencies; `uv` installed it) over 26 malformed shapes, and compared with the port:

| Input | Reference | Port now |
|---|---|---|
| `enabled: 'false'` / `'no'` / `0` / empty | provider disabled (string false words, else Python truthiness) | same (done) |
| `baseUrl`, `apiKey`, `apiMode`, `keyEnv`, `apiKeyEnv`, `defaultModel`, `contextLength` in a provider entry | mapped to the snake_case key when it is absent, with a log warning | mapped (done; no warning printed) |
| `provider: custom` with a base URL and no key | request carries the placeholder `no-key-required` (keyless local servers) | same (done); vendor providers still stop with "API key is required" |
| `providers:` a list or string, `custom_providers:` a string, `approvals:` a list, `compression:` a string, `agent:` null, unknown top-level keys | ignored, the run goes on | same |
| `providers.<name>: 'https://…'` (entry not a mapping), `custom:<name>` not defined | error: unknown provider | same (an error) |
| `model:` a string or null or a list | no usable provider: error | error |
| `base_url: 5` | crashes (`AttributeError`) | error "model base URL is required" |
| `extra_body: [1]` | ignored | ignored |
| config file not valid YAML, tabs, or not a mapping | **runs on default settings** with a warning and saves a copy of the broken file | **refuses with an error, on purpose** |

The last row stays different: silently dropping `approvals.deny`, `command_allowlist` and the provider
settings on a typo is a worse failure than a refusal, and the message names the file.

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
  blocks; remote-backend environment text; plugin sections; the dynamic
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
  `privacy.redact_pii` is refused and `security.redact_phone_numbers` is read (see "Notes on the config keys of
  items 8, 11 and 13").

## Notes on `delegation.*` route (done)

- `delegation.model`, `provider`, `base_url`, `api_key`, `api_mode` pick the route subagents run on, with the
  reference's rules: only `model` keeps the parent's endpoint and key; a bare `base_url` (provider empty,
  `custom` or `auto`) is a direct endpoint that keeps the parent's key unless `api_key` is set; a named
  provider resolves its own key; another endpoint without `model` is refused with a clear message. A pinned
  route never borrows the parent's fallback chain (`delegation.fallback_providers` is not read). The result
  of `delegate_task` names the child model. Live check: parent `deepseek-flash-1017`, subagent
  `claude-haiku-5-5` on the gateway. Still not read: `request_overrides`, `reasoning_effort`,
  `child_timeout_seconds`, `max_summary_chars`, `max_spawn_depth`, `orchestrator_enabled`,
  `inherit_mcp_toolsets`.
## Approvals: the reference's detector (done)

Decision (2026-11-22): the default is the reference's. A command needs an answer only when the
dangerous-command detector flags it; everything else (`pip install`, `python script.py`, builds, tests)
runs. The detector is `crates/hermes-runtime/src/approval_detect/` (about 20 modules), built bottom-up
against corpora recorded from the reference by `compat/create_approval_*.py` and checked by the
`approval_*_corpus` tests (7 suites, about 9 thousand recorded rows, plus mutation checks). Pattern
tables are generated from the reference by `compat/generate_approval_tables.py`.

Order of decisions for a command (`approval_policy.rs`): `approvals.deny` globs; the hardline floor and
the `sudo -S` guard (refused even under `--yolo` and `approvals.mode: off`); `--yolo`/`off`
(refused instead when the scan could not read the command to the end); `command_allowlist` and
`approvals.allow` (single plain commands only); then the detector's findings. An answer of `y` covers
once, `s` the pattern keys for the conversation, `a` also writes them to `command_allowlist` in
`config.yaml` (key aliases of old entries resolve both ways).

Where the port is stricter than the reference, on purpose:

- every matching pattern key needs consent, not only the first match plus the content rules (an
  approved `rm -rf` no longer waves a `git push --force` in the same line through);
- `approvals.deny` globs match the command text as written, not every normalized spelling of it;
- an `approvals.allow` pattern or `command_allowlist` glob covers only a single plain command (the
  reference's quote-aware operator check is not ported).

Runs with nobody to answer (decided 2026-11-22: as in the reference). The terminal is offered to every run,
`-q` and cron included; an unflagged command runs, a flagged one is refused (`approval_unavailable`)
unless the run's mode approves: `approvals.single_query_mode` for a `-q` run with no terminal or with
`--oneshot`, `approvals.cron_mode` for a cron job. `approve`, `off`, `allow` and `yes` mean approve; any
other value, `true` included, and an absent key mean deny. The floors and a command the scanner could not
read are refused whatever the mode says, and `command_allowlist` entries still approve. File writes and
`execute_code` stay hidden without a person (code is approved one run at a time). A `-q` run at a
terminal still asks, where the reference's `-q` never asks; `--oneshot` is the port's no-person switch.
`approvals.unattended_mode` (webhook platforms) has no counterpart.

The macOS `osascript` classifier (`approval_applescript.py`) is ported and checked against 229
recorded commands; its two findings are answered for that one command only (no session or "always"
grant, as the reference's manual-only descriptions). Not ported yet: the
`cron.lifecycle_guard` splice check (a prompt-only layer for gateway lifecycle verbs; the port has no gateway), Windows path spellings (the detector is POSIX only), the guardian
(`smart`) mode and `cron_mode`/`unattended_mode`. `config set` rewrites `config.yaml` without keeping
comments, which also applies to the `command_allowlist` write.

## Notes on the terminal's init files (done)

`terminal.shell_init_files` (a list; `~` and `${VAR}` expanded, missing files dropped) and
`terminal.auto_source_bashrc` (default true, read like Python's `bool(...)`) work as in the reference:
a login `bash` sources `~/.profile`, `~/.bash_profile` and `~/.bashrc` (or the explicit list, which
replaces them), so that `PATH` entries added by nvm, pyenv or cargo reach commands, and the directories of
the original `PATH` that the profile dropped are put back at the end. Differences, on purpose:

- the shell runs once per process, not per session, and only the variables a command may see anyway are
  taken from it (the default names and `terminal.env_passthrough`); credentials an rc file exports and
  the rest of the login environment stay out, which keeps the "minimal environment" rule. `HOME`, `USER`,
  `LOGNAME`, `SHELL`, `TERM`, `TZ` and `TMPDIR` are never replaced;
- commands still run under `/bin/sh -c`, not `bash`, and the exported environment and shell state do not
  persist between calls (`persistent_shell` is not read), only the working directory does.

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

## Notes on `skills.auto_load`, `template_vars`, `inline_shell` and the skills index (done)

- `skills.auto_load` (a list of names; strings only, trimmed, repeats dropped) pins skills into the **stable**
  tier of the system prompt, right after the guidance paragraphs and before the coding brief, as the
  reference does. A block is the activation note, the SKILL.md text, `[Skill directory: ...]` and its
  note, `[Skill config (from <home>/config.yaml): ...]` for `metadata.hermes.config` values (`skills.config.<key>`
  or the declared default, `(not set)` when empty, `~`/`$VAR` expanded), the setup note when required
  environment variables are missing, and the supporting-file list with the `skill_view` footer. Blocks are
  joined with a blank line. Names that cannot be loaded (unknown, ambiguous, disabled) are skipped with one
  stderr warning. Nothing is pinned without the skills toolset or when `HERMES_IGNORE_RULES` is truthy. The
  text is rendered once per process (`TurnHistory.pinned`) and reused when compaction rebuilds the prompt;
  a resumed session keeps the stored prompt as before.
- Checked byte for byte against the reference (`compare_prompt.py --auto-load`: a pinned skill with config
  values, a missing credential, supporting files in four directories, a disabled name, a missing name, a
  duplicate and a non-string entry, plus the `hermes-agent` skill). Five Python tests and three Rust tests
  cover the rest (`PinnedSkillsTest`).
- `skills.template_vars` (default on) substitutes `${HERMES_SKILL_DIR}`; `${HERMES_SESSION_ID}` is replaced only
  when the session already has an id. **Divergence:** the port gives a new session its id on the first save,
  after the prompt is built, so a pinned skill keeps the literal token in that session (the reference has the
  id from the start). The compare script normalizes this one token.
- **`skills.inline_shell` is not executed, on purpose.** The reference expands `` !`cmd` `` snippets of a
  skill with `bash -c` while it builds the message (and in `skill_view`), outside the approval policy; a
  skill file would become a command source. The port prints a warning when the key is on and leaves the text as
  written. It needs the same decision as `quick_commands`.
- Absolute skill paths as `auto_load` identifiers are not resolved (the reference maps them under the skills
  directory); names and `category/name` work.
- Found while comparing: the `## Skills` wrapper of the index was the port's own short text, never the
  reference's. Now both variants are recorded: the long guidance for a conversation (with `web_search or
  terminal` or just `terminal` by the tools offered), and the short one for `chat -q` (the reference's
  `HERMES_SINGLE_QUERY_SESSION`). `ChatRequest.single_query` carries that; a REPL turn is never one.
  Not ported: categories demoted to names only in a coding session, and the note for unloadable duplicates.
- Found while comparing: the prompt's own gates (the `hermes-agent` help paragraph, memory and skill
  guidance) saw a tool list without `skill_view`/`skills_list`, because those tools are offered only once the
  prompt carries the index. The index is now computed first, so the gates see the same names as the model.

## Notes on the config keys of items 8, 11 and 13 (done, 2026-11-23)

Recorded with `compat/create_config_keys_fixture.py` (`tests/fixtures/config_keys.json`: 40 compression
triggers through a real `ContextCompressor`, 31 `max_attempts`/`protect_*` values, 28 `config.yaml` files read by
`chat_keeps_phone_numbers`, 16 reply scrubs, 6 `_should_redact_pii` rows) and `compat/create_request_fixture.py`
(`tests/fixtures/agent_requests.json`: 62 chat-completions and 9 Responses request bodies from
`_build_api_kwargs`, and 99 `CustomProfile.build_api_kwargs_extras` mappings). Rust tests are in `crates/hermes-runtime/tests/config_keys.rs`, end-to-end ones in
`compat/test_cli.py`. Every test was checked with a mutation.

| Key | Result | Why |
|---|---|---|
| `compression.model_thresholds` | implemented | longest matching substring wins, `provider:` scope outranks a bare key of the same length, the greater key text breaks ties; replaces the ratio before the 0.75 floor, `threshold_tokens` still caps. Matched against the effective model and provider after `--model` (`CompactionSettings::set_route`). Values that are not numbers are dropped, as in the reference |
| `compression.max_attempts` | implemented | caps how often one turn compresses its own steps (default 3, below 1 means 3, at most 10). In the reference the same counter also covers preflight and overflow retries, which the port does not have |
| `compression.protect_first_n` / `protect_last_n` | coercion fixed | negatives clamp to 0, floats truncate, numeric strings read (the port fell back to 3 for a negative first count) |
| `compression.abort_on_summary_failure` | not read, default differs | the port never drops the middle of a history on a failed summary: it keeps everything, which is the reference's `true`. The reference's default `false` replaces the middle with a placeholder |
| `compression.target_ratio`, `tail_mode`, `min_tail_user_messages` | implemented (backlog 3) | `target_ratio` is read as `float()` and clamped to 0.1..0.8 by the compressor, `tail_mode` is trimmed and lower-cased and anything but `legacy` is `lean`, `min_tail_user_messages` goes through `_parse_config_int` with a floor of 1. They drive the tail: `lean` keeps 2.5% of the window between 10K and 25K tokens (never more than half the trigger), `legacy` keeps `target_ratio` of the trigger, both held to a fifth of the window. Recorded: 35 values and 1288 planned windows (`compat/create_compression_window_corpus.py`, `tests/compression_window.rs`). **Difference:** a value the reference refuses at start-up (`target_ratio: abc`, `null`, a list) reads as the default here. The window rules and the differences that remain are in the migration plan (Russian), under the compression item |
| `proactive_prune_*`, `micro_compact*`, `idle_compact_after_seconds`, `prepare_ahead` | refused | opt-in features, off by default in the reference, with no counterpart in the port; defaults match. Gateway (`hygiene_*`), Codex (`codex_*`) and timeout keys do not apply to a one-shot CLI |
| `agent.reasoning_effort`, `agent.reasoning_overrides` | implemented for `custom` chat completions | found while recording the default request: the reference sends `reasoning_effort: medium` on the custom route, the port sent nothing. Now: config value or per-model override (spelling-tolerant: dots and dashes, provider prefixes, reverse lookup of prefixed keys), `none`/`false`/`disabled` send `none`, `ultra` becomes `max`, unknown words fall back to `medium`. `extra_body` of the endpoint still wins. The profile's host rules are ported and recorded (`build_api_kwargs_extras` over 11 endpoints x 9 configs): Groq takes only `default` or `none`, Ollama gets `think: false` when reasoning is off. **Not ported:** the catalog check that skips models marked non-reasoning (recorded: `get_model_capabilities` returns nothing for the custom provider from a fresh profile, so it only acts once the reference has cached a models catalog), named providers (they keep their profile's default body), the Responses `reasoning` object, fallback routes and subagents |
| default `reasoning_effort` on must-echo routes | deliberate difference | **the port's own safety choice, not recorded reference behaviour.** For models containing `deepseek` or `mimo`, Kimi, DeepSeek and MiMo hosts, and Ollama-looking endpoints (port 11434, an `ollama` host label, ollama.com), the port sends nothing when no effort is configured (the reference probes `/api/show` for Ollama; the port cannot). The reference sends `medium` and pads tool-call turns with `reasoning_content: " "`; the port does not replay reasoning, so asking those routes to think could end a tool turn in a 400. A configured level is sent. Re-check with `compat/live_suite.py` on `deepseek-flash-1017` before relying on it |
| `agent.text_verbosity` | implemented for Responses | `text: {verbosity}` for low/medium/high (case-insensitive), nothing otherwise, nothing on xAI. Checked against the recorded Responses bodies at settings level, and end to end against the model server, which records (and refuses) `/v1/responses`. The xAI skip follows the reference (`xai` provider or host `api.x.ai`). The rest of the reference's Responses body (`prompt_cache_key`, `reasoning`, `include`) is still absent |
| `agent.service_tier` | refused | recorded: no change to a custom chat-completions body; the reference applies it on the Codex backend and Responses (priority tiers) only |
| `agent.reasoning_echo` | refused | needs stored reasoning replay (`reasoning_content` carriers), which the port does not have; see the must-echo row |
| `agent.stall_guards` | refused, **default differs** | default `true` in the reference: an identical-call notice on a tool result after three identical calls and results, and a re-prompt when the model says it will continue but takes no action (`agent/tool_guardrails.py`, about 660 lines). The port has neither |
| `agent.verify_on_stop`, `verify_guidance`, `max_verify_nudges` | refused | `verify_on_stop` is `false` by default, so defaults match; `true` is not honoured (no evidence ledger, no refusal of a final answer) |
| `agent.run_budget_seconds` | refused | `null` by default. The reference adds a wrap-up notice to the newest tool result at 80% of the budget and caps provider timeouts; neither exists in the port |
| `agent.image_input_mode` | not applicable | the one-shot CLI attaches no images; recorded: no change to the request body |
| `tool_use_enforcement`, `execution_guidance`, `task_completion_guidance`, `parallel_tool_call_guidance`, `environment_hint` | already done | see "the system prompt is now assembled like the reference" |
| `security.redact_phone_numbers` | implemented | only a YAML false keeps `+` numbers; anything else (including `'false'`, `0`, `null`) keeps them masked. The reference reads YAML 1.1, so bare `no`, `off`, `n` count as false; the port reads them as text and treats the bare words the same. **Difference:** a quoted `'no'` or `'off'` is false here and masked in the reference |
| stored assistant replies | **new behaviour, found while doing the above** | the reference scrubs every stored assistant reply (tool-call turns too) with `redact_sensitive_text`, which follows `security.redact_secrets`, so the model sees masked text on its next request and a resumed session replays it masked. The port stored replies raw. Now scrubbed at both places in `turn.rs`; the text printed to the person and a subagent's summary stay raw, as in the reference. The reference also strips think blocks and whitespace first; not ported |
| `privacy.redact_pii` | ignored | gateway only: it hashes user and chat ids in the session-context prompt of PII-safe messaging platforms. Recorded `_should_redact_pii`: false for the `local` platform whatever the setting |
| `terminal.persistent_shell` | ignored, **default differs** | the reference reads it for the SSH backend only (`TERMINAL_PERSISTENT_SHELL` feeds `ssh_persistent`); the local backend has its own switch, `TERMINAL_LOCAL_PERSISTENT`, default on, which carries exported variables and shell state from one command to the next. The port has no remote backends and keeps only the working directory between commands, which equals the reference with `TERMINAL_LOCAL_PERSISTENT=false`. Persisting shell state needs a snapshot taken after every command; not simple, so not done |

Window selection (backlog 3) changed what a default config does: the head is the system prompt plus three rows, the tail is a token budget (10K tokens by default, as in the reference) and the history is only compressed when it has more than `head + 4` rows, so short histories are no longer cut at `protect_last_n` rows. The port still does not prune old tool results before it plans, which the reference does; see the migration plan.

Default divergences left after this work (a default config does not match the reference): `agent.stall_guards`
(notices and re-prompt), `compression.abort_on_summary_failure` (the port always aborts), local shell state
persistence, and the Responses request body.

