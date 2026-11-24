# Hermes GUI (`hsr-gui`)

Standalone desktop frontend for Hermes Agent built with [GPUI](https://gpui.rs/).

This crate lives outside the root Cargo workspace (`exclude = ["crates/hermes-gui"]`) so CLI builds and musl releases stay free of graphics/windowing dependencies.

## Architecture

- `hsr-gui` connects to the shared local session owner (`hsr owner`, implemented on `hermes-runtime::local_owner`).
- **Unix (Linux/macOS):** Unix domain socket `<profile>/rust-owner/owner.sock` with profile-scoped `owner.lock`.
- **Windows:** Authenticated loopback TCP (`127.0.0.1:<port>` stored in `<profile>/rust-owner/owner.addr` with a 256-bit per-owner token).
- Protocol v2 supports profile discovery, session list/search, transcript loading, streaming turn execution with sequenced event replay (`SubscribeTurn { after_seq }`), cancellation, and interactive tool approval survival across UI reconnects.
- Sessions created in `hsr-gui` are written to the same `<profile>/state.db` and can be resumed directly in `hsr chat --resume <id>`.

## System Dependencies

### Linux (Debian / Ubuntu)

```bash
sudo apt-get install -y \
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

### Windows

- Standard MSVC toolchain (`x86_64-pc-windows-msvc`) with Windows SDK (DirectX / DirectWrite / Win32).

## Building and Running

First build the CLI binary (used as the local session owner process):

```bash
cargo build --locked -p hermes-cli
```

Then build and run the GUI:

```bash
# Default (Wayland + X11 on Linux, Win32 on Windows)
cargo run --manifest-path crates/hermes-gui/Cargo.toml --locked

# Linux Wayland-only (when libxkbcommon-x11-dev is not installed)
cargo run --manifest-path crates/hermes-gui/Cargo.toml --no-default-features --features wayland --locked
```

Run tests and lints:

```bash
cargo fmt --manifest-path crates/hermes-gui/Cargo.toml --all -- --check
cargo clippy --manifest-path crates/hermes-gui/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path crates/hermes-gui/Cargo.toml --locked
```
