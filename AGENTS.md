# Repository Guidelines

## Project Structure and Modules

This repository contains a Rust rewrite of Hermes Agent. The Cargo workspace has four crates under `crates/` sharing one `Cargo.lock`. `hermes-core` contains shared primitives such as the SSE decoder. `hermes-runtime` owns configuration, provider integration, and the agent turn loop. `hermes-cli` contains the `hsr` executable and embeds its runtime/owner logic as a library. `hermes-gui` contains the standalone GPUI desktop app (`hsr-gui`). Default workspace members are `hermes-core`, `hermes-runtime`, and `hermes-cli` so headless builds do not require GUI system libraries. Rust integration tests live in each crate's `tests/` directory, while unit tests can live beside their modules.

`compat/` contains the local model server, Python integration tests, reference-fixture tools, and the compatibility matrix. The migration plan is in `docs/superpowers/plans/`.

## Build and Development Commands

- `cargo build --locked` builds the default CLI crates (`hermes-core`, `hermes-runtime`, `hermes-cli`) using `Cargo.lock`.
- `cargo test --locked` runs Rust unit and integration tests for the CLI crates. The provider tests require loopback networking.
- `cargo fmt --all -- --check` verifies Rust formatting across all crates including `hermes-gui`. Run `cargo fmt --all` to apply it.
- `cargo clippy --all-targets --locked -- -D warnings` checks the CLI crates; run `cargo clippy -p hermes-gui --all-targets --locked -- -D warnings` and `cargo test -p hermes-gui --locked` to check and test the GUI crate.
- `cargo build --locked -p hermes-cli` followed by `python3 -m unittest discover -s compat -p 'test_*.py' -v` runs the CLI and model-server compatibility tests. These tests require loopback networking.

For local experiments, start `python3 compat/model_server.py --scenario tool` and use its printed address with `target/debug/hsr chat --oneshot -q 'Read fixture.txt' --base-url <address>/v1 --model fixture`. Set `HERMES_API_KEY` in the environment and create `fixture.txt` in the working directory. Without `-q`, `hsr chat` starts the interactive conversation (`compat/test_repl.py` covers it). `compat/live_suite.py --model deepseek-flash-1017` runs the real-provider task suite; it reads the gateway settings from the environment or the untracked `.env` and never prints the key.

## Coding and Testing

Use standard Rust formatting and four-space indentation. Name Rust modules and functions in `snake_case`, types in `PascalCase`, and constants in `SCREAMING_SNAKE_CASE`. Keep provider behavior in `hermes-runtime` and reusable protocol primitives in `hermes-core`. Python compatibility scripts use four-space indentation and `test_*.py` test files.

Add targeted tests for behavior changes and regression cases. Exercise streaming boundaries, cancellation, profile isolation, and tool results when relevant. Use temporary profiles and fixtures. See `compat/README.md` for Python reference setup and comparison commands.

## Commits and Pull Requests

Write concise, imperative commit summaries describing the change. In pull requests, explain the behavior change, list verification commands and results, and link the relevant issue when available. Describe compatibility differences and migration effects explicitly.

## Configuration and Secrets

Keep credentials in the selected profile's `.env` or environment variables. Use disposable profiles and synthetic credentials for tests, and never commit secrets or real session databases. The local model server records complete requests in memory, so use synthetic prompts and tool data.
