# Hermes GUI (`hsr-gui`)

Standalone desktop frontend for Hermes Agent built with [GPUI](https://gpui.rs/).

This crate is a member of the root Cargo workspace (sharing the root `Cargo.lock`), but is omitted from `default-members` so headless CLI builds and static musl releases stay free of graphics/windowing dependencies.

## Architecture

- `hsr-gui` is a standalone binary that embeds `hermes-cli` (`owner` and `__embedded-python` subcommands) and connects to the shared local session owner (`hermes-runtime::local_owner`) without requiring a separate `hsr` binary.
- **Unix (Linux/macOS):** Unix domain socket `<profile>/rust-owner/owner.sock` with profile-scoped `owner.lock`.
- **Windows:** Authenticated loopback TCP (`127.0.0.1:<port>` stored in `<profile>/rust-owner/owner.addr` with a 256-bit per-owner token).
- Protocol v2 supports profile discovery, session list/search/delete, transcript loading, streaming turn execution with sequenced event replay (`SubscribeTurn { after_seq }`), cancellation, and interactive tool approval survival across UI reconnects.
- Sessions created in `hsr-gui` are written to the same `<profile>/state.db` and can be resumed directly in `hsr chat --resume <id>`.
- Built-in desktop features include Setup & Settings (OAuth login and bundled skills sync), project folder picker, file and clipboard image attachments, `@`/`/` autocomplete, extended Markdown (GFM tables, task lists, footnotes, callouts, `<details>`), native LaTeX math rendering, native Mermaid vector diagrams, and quote-in-reply (`Ctrl+Shift+Q`).

## System Dependencies

### Linux (Debian / Ubuntu — glibc)

```bash
sudo apt-get install -y \
  libasound2-dev \
  libfontconfig1-dev \
  libfreetype6-dev \
  libvulkan-dev \
  libwayland-dev \
  libxcb1-dev \
  libxcb-render0-dev \
  libxcb-shape0-dev \
  libxcb-xfixes0-dev \
  libxkbcommon-dev \
  libxkbcommon-x11-dev \
  pkg-config
```

### Linux (Alpine Linux — musl)

Build dependencies (dynamic CRT linking `-C target-feature=-crt-static` is required because GPUI dynamically links Wayland/X11/Vulkan/Fontconfig shared libraries):

```bash
apk add --no-cache \
  build-base pkgconf alsa-lib-dev fontconfig-dev freetype-dev \
  wayland-dev libxkbcommon-dev libxcb-dev vulkan-loader-dev vulkan-headers
```

Runtime packages on a fresh Alpine desktop:

```bash
apk add --no-cache \
  fontconfig freetype wayland-libs-client libxkbcommon libxkbcommon-x11 \
  libxcb vulkan-loader mesa-vulkan-swrast
```

Build on Alpine musl:

```bash
RUSTFLAGS="-C target-feature=-crt-static" \
  cargo build -p hermes-gui --release --locked --target x86_64-unknown-linux-musl
```

### Windows

- Standard MSVC toolchain (`x86_64-pc-windows-msvc`) with Windows SDK (DirectX / DirectWrite / Win32).

## Building and Running

Build and run the standalone GUI directly from the workspace root:

```bash
# Default (Wayland + X11 on Linux, Win32 on Windows)
cargo run -p hermes-gui --locked

# Linux Wayland-only (when libxkbcommon-x11-dev is not installed)
cargo run -p hermes-gui --no-default-features --features wayland --locked
```

Run tests and lints:

```bash
cargo fmt --all -- --check
cargo clippy -p hermes-gui --all-targets --locked -- -D warnings
cargo test -p hermes-gui --locked
```
