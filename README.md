# hermes-rs

A fast, standalone Rust implementation of [Hermes Agent](https://github.com/NousResearch/hermes-agent) — a single native CLI binary (`hsr`) with no Python runtime dependency, designed to live cleanly side-by-side with Python `hermes` using `~/.hsr` by default (override with `HSR_HOME` or `HERMES_HOME`) while staying drop-in compatible with existing `~/.hermes` profiles, `config.yaml`, `.env`, `auth.json`, skills, cron jobs, and SQLite `state.db` history.

> Current releases: CLI **`v0.7.17`** (Linux x86_64/arm64 GNU & musl, macOS Apple Silicon & Intel, Windows x64) · Desktop GUI **`gui-v0.7.17`** (standalone `hsr-gui` for Linux GNU/musl & Windows x64; GUI versioning was synchronized with CLI starting at `0.7.14`, up from `gui-v0.4.11`)

## Highlights

- **Zero-dependency native binary**: Fast startup, low memory footprint, and static musl builds for Linux containers without Python.
- **Interactive terminal UX**:
  - **Full-screen TUI mode** (default on TTY, or `--ui tui` / `/ui tui`): header status bar (profile, model, context usage), scrollable Markdown feed, tool cards, command palette, multiline composer with persisted Up/Down history, and interactive approval, clarify, and `/sessions` picker modals.
  - **Inline mode** (`--ui inline` or `/ui inline`): streaming Markdown with syntax highlighting, live tool badges and spinners, `/` slash-command autocompletion, and native terminal scrollback.
  - **Plain & JSON modes**: automatic plain fallback for pipes/`NO_COLOR`, plus versioned NDJSON streaming via `--events-json` for programmatic integrations.
- **Drop-in profile & database compatibility**:
  - Uses `~/.hsr` by default (or `HSR_HOME` / `HERMES_HOME`) and reads existing `~/.hermes` profile layouts (`config.yaml`, `.env`, `SOUL.md`, `memories/{MEMORY,USER}.md`, `skills/`, `auth.json`, `cron/jobs.json`, `state.db`).
  - Safe copy-based migration for early SQLite schemas (`v1..v32 → v33`, covering Hermes v0.15+ and legacy FTS layouts `v0/v1/v2 → v3`) with byte-for-byte history verification.
- **Broad provider & multimodal support**:
  - OpenAI-compatible Chat Completions (streaming & non-streaming), OpenAI Responses API, Anthropic Messages (API key & OAuth), AWS Bedrock (Converse REST + SigV4/Bearer + Guardrails + Mantle Responses), Google Vertex AI (Service Account RS256 JWT & ADC), NVIDIA NIM, 32 named key-based providers, and OAuth refresh (`openai-codex`, `nous`, `xai-oauth`, `qwen-oauth`, `minimax-oauth`).
  - Multimodal image input across OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and AWS Bedrock Converse.
- **Built-in tools & MCP**:
  - Workspace-scoped file tools (`read_file` with image detection, document text extraction, and symlink/reparse checks, `search_files`, atomic `write_file` & `patch` on Linux, macOS, and Windows), local `terminal` (Linux/macOS with persistent shell state & `process_manage`/PTY; Windows foreground `cmd.exe` with Win32 Job Object process-tree cleanup, without `background`/`process_manage` or persistent shell env between calls), embedded RustPython `execute_code` kernel with native pure-Rust Office helpers (`xlsx_read`/`xlsx_write`, `docx_read`/`docx_write`, `pptx_read`/`pptx_write` — no external Python or MS Office required), `delegate_task` subagents (with nested depth, `list`/`steer`/`stop`/`continue`), `memory`, `todo_list`, `session_search`, `clarify`, `skills_list`/`skill_view`/`skill_manage`, `cronjob_manage`, `web_search`/`web_extract` (10 backends + keyless failover + website blocklist), `browser_cdp`, `vision_analyze`, `image_generate` (FAL.ai), `text_to_speech` (HTTP & local command), and `x_search` (xAI Responses).
  - Full MCP client (`stdio`, Streamable HTTP, and legacy `sse`) with cancellation (`notifications/cancelled`), session recovery, resources, prompts, media caching, disk spillover, and OAuth bearer tokens.
- **Security & approvals**:
  - Dangerous-command detector, allow/deny policies, hardline safety floor, secret redaction before persistence, SSRF protection (`url_safety`), and configurable `security.website_blocklist`.

## Installation

### Prebuilt Binaries (GitHub Releases)

Download the archive and `.sha256` checksum for your platform from [Releases](https://github.com/tsrman/hermes-rs/releases):

| Platform | Target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` (glibc), `x86_64-unknown-linux-musl` (Alpine / static) |
| Linux arm64 | `aarch64-unknown-linux-gnu` (glibc), `aarch64-unknown-linux-musl` (Alpine / static) |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x64 | `x86_64-pc-windows-msvc` |

**Linux / macOS:**

```bash
tag=v0.7.17
target=x86_64-unknown-linux-gnu
archive="hsr-${tag#v}-$target.tar.gz"
url="https://github.com/tsrman/hermes-rs/releases/download/$tag"
(
  set -e
  tmp="$(mktemp -d)"
  cd "$tmp"
  curl -fsSLO "$url/$archive"
  curl -fsSLO "$url/$archive.sha256"
  if command -v sha256sum >/dev/null; then
    sha256sum --check "$archive.sha256"
  else
    shasum -a 256 -c "$archive.sha256"
  fi
  tar -xzf "$archive" hsr
  mkdir -p "$HOME/.local/bin"
  install -m 755 hsr "$HOME/.local/bin/hsr"
) && hsr --help
```

**Windows (PowerShell):**

```powershell
$Tag = 'v0.7.17'
$Archive = "hsr-$($Tag.Substring(1))-x86_64-pc-windows-msvc.zip"
$Url = "https://github.com/tsrman/hermes-rs/releases/download/$Tag"
Invoke-WebRequest "$Url/$Archive" -OutFile $Archive
Invoke-WebRequest "$Url/$Archive.sha256" -OutFile "$Archive.sha256"
$Expected = (Get-Content "$Archive.sha256").Split(' ')[0]
if ((Get-FileHash $Archive -Algorithm SHA256).Hash -ne $Expected) { throw 'SHA-256 mismatch' }
$InstallDir = Join-Path $env:LOCALAPPDATA "hermes-rs\$Tag"
Expand-Archive -LiteralPath $Archive -DestinationPath $InstallDir
& (Join-Path $InstallDir 'hsr.exe') --help
```

### Build from Source

Requires Rust `1.95+` (release builds use Rust `1.100.0`):

```bash
cargo build --release --locked -p hermes-cli
./target/release/hsr --help
```

## Quick Start

```bash
# Run the interactive setup wizard (creates all profile directories, starter files, config.yaml, .env, state.db)
hsr setup

# Or quick-setup via Nous Portal OAuth / download official bundled skills / switch models / diagnose
hsr setup --portal
hsr skills sync
hsr model
hsr doctor

# Start an interactive conversation in full-screen TUI mode (uses ~/.hsr by default; `hsr chat` is equivalent)
hsr

# Start in inline streaming mode
hsr --ui inline

# Run a single non-interactive query and save the session
hsr chat --oneshot -q "Summarize the files in this directory" --save-session

# Resume the last CLI session or a specific session ID
hsr --continue
hsr --resume <session-id>

# Use a separate profile directory (under ~/.hsr/profiles/work)
hsr -p work

# Stream machine-readable NDJSON events (version: 1)
hsr chat --events-json -q "Read README.md"
```

### Migrating an Existing Python Hermes Profile

Before switching an existing `~/.hermes` profile (including older `state.db` schemas from v0.15+), inspect and migrate a verified copy:

```bash
# Check profile inventory and schema version (Linux)
hsr profile check

# Migrate a copy of an older state.db (v1..v32 -> v33 with FTS5 v3)
hsr sessions migrate --output /path/to/migrated-state.db
```

### Custom Endpoints and `extra_body`

Extra request fields for a `custom` endpoint (for example `chat_template_kwargs`) are read from the matching `providers.<name>.extra_body` (or `custom_providers[].extra_body`) entry, not from `model:` or the top level:

```yaml
model:
  provider: custom
  default: qwenflash
  base_url: https://example.invalid/v1
providers:
  myendpoint:
    base_url: https://example.invalid/v1
    extra_body:
      chat_template_kwargs: {enable_thinking: true}
```

An entry applies only when its `base_url` equals the active base URL (a trailing `/` is ignored), so overriding the URL with `--base-url`, `HERMES_BASE_URL` or a different `model.base_url` (for example a logging proxy) disables it. If the entry has a `model`, it must match the active model. Reasoning effort is set with `agent.reasoning_effort`. `hsr` warns at startup, and `hsr doctor` reports, when an `extra_body` is misplaced or not applied; `hsr --version` prints the build.

## CLI Commands

| Command | Description |
| --- | --- |
| `hsr setup [--portal] [--non-interactive]` | Interactive setup wizard: initializes profile directories, starter files, `config.yaml`, `.env`, `state.db`, provider, and skills. |
| `hsr model` | Show or switch the default LLM provider and model. |
| `hsr doctor [--json] [--fix]` | Diagnose profile directories, `config.yaml`, credentials, database, local owner, and `extra_body` matching. |
| `hsr skills [sync\|list]` | Download, sync, or inspect bundled Markdown skills in the selected profile. |
| `hsr auth <login\|status\|refresh\|logout>` | Authenticate with OAuth providers (PKCE / device code) and inspect stored credentials. |
| `hsr browser <connect\|status\|disconnect>` | Connect to, inspect, or disconnect a live local CDP browser session. |
| `hsr chat [OPTIONS]` | Interactive REPL/TUI or single-turn `-q` chat (`--ui`, `--model`, `--provider`, `--base-url`, `--save-session`, `--resume`, `--continue`, `--yolo`, `--oneshot`, `--events-json`, `--max-turns`). |
| `hsr sessions <list\|show\|search\|migrate>` | Inspect, full-text search, or migrate SQLite `state.db` sessions (`v1..v32 → v33`). |
| `hsr profile <check\|migrate>` | Inspect a profile directory and prepare a verified migration copy (Linux). |
| `hsr cron <list\|add\|edit\|pause\|resume\|run\|cancel\|remove\|status\|runs\|incidents\|notepad\|doctor\|tick\|preview>` | Manage and execute local scheduled jobs in the shared `cron/jobs.json` / `executions.db` / `notepad.db` format. |
| `hsr config set <key> <value>` | Update nested keys in the selected profile's `config.yaml`. |
| `hsr owner` | Run the local session authority daemon for the selected profile (Unix socket / Windows loopback TCP). |

Inside interactive `hsr chat`, available slash commands include `/help`, `/new`, `/sessions`, `/resume`, `/model`, `/compress` (`/compact`), `/yolo`, `/retry`, `/ui [inline|tui|plain]`, `/browser`, `/skills`, `/<skill>`, `/clear`, `/reset`, `/undo`, `/copy`, and `/exit`, alongside `@` context refs (`@file`, `@folder`, `@diff`, `@staged`, `@git`, `@url`) and local `!<cmd>`.

## Workspace Architecture

```text
hermes-rs/
├── crates/
│   ├── hermes-core/     # Protocol primitives (UTF-8 chunk-safe SSE decoder)
│   ├── hermes-runtime/  # Config, providers, OAuth, SQLite storage, compaction, tools, MCP, cron, turn loop
│   ├── hermes-cli/      # `hsr` executable, REPL/TUI, approvals, session owner, subcommands
│   └── hermes-gui/      # Standalone GPUI desktop app (`hsr-gui`) with embedded owner & Python worker
├── compat/              # Local fixture model server, Python end-to-end compat suite, release packaging
├── docs/                # Extension interface docs and migration plans
└── examples/            # Python SDK wrapper (`--events-json`) and MCP plugin example
```

## Development & Testing

```bash
# Check formatting across all workspace crates (including hermes-gui)
cargo fmt --all -- --check

# Lint and test CLI crates (default workspace members; requires loopback networking)
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked

# Lint and test the desktop GUI crate (requires GPUI system libs on Linux)
cargo clippy -p hermes-gui --all-targets --locked -- -D warnings
cargo test -p hermes-gui --locked

# Build CLI and run Python compatibility suite (requires loopback networking)
cargo build --locked -p hermes-cli
python3 -m unittest discover -s compat -p 'test_*.py' -v
```

For quick local experiments without an external API key:

```bash
python3 compat/model_server.py --scenario tool
# Use the printed address:
HERMES_API_KEY=test target/debug/hsr chat --oneshot \
  -q 'Read fixture.txt' --base-url http://127.0.0.1:<port>/v1 --model fixture
```

## Documentation

- **Compatibility Matrix**: [compat/matrix.md](compat/matrix.md) — detailed feature parity status against upstream `NousResearch/hermes-agent`.
- **Compatibility & Release Guide**: [compat/README.md](compat/README.md) — reference fixtures, installation, profile migration, and rollback procedures.
- **Extensions & Python Integration**: [docs/extensions.md](docs/extensions.md) — Markdown skills, MCP servers, `execute_code`, and `--events-json` SDK usage.
- **Windows Code Signing**: [docs/windows-code-signing.md](docs/windows-code-signing.md) — generating a self-signed Authenticode certificate on Linux, configuring GitHub Actions secrets, and installing the public `.cer` on Windows.
- **Repository Guidelines**: [AGENTS.md](AGENTS.md) — build, style, testing, and security conventions.
