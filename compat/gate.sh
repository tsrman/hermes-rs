#!/usr/bin/env bash
# The checks a change must pass before it goes to main. Run from the repository root:
#   compat/gate.sh [--quick]     (--quick skips the slow Python suites)
# Needs rustup with the CI toolchain (1.95.0). The Windows check needs `ziglang` in a Python venv
# (pip install ziglang) and the target x86_64-pc-windows-gnu; it is skipped with a note when absent.
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
TOOLCHAIN="${RUST_VERSION:-1.95.0}"
step() { printf '\n== %s\n' "$*"; }

step "fmt"
cargo "+$TOOLCHAIN" fmt --all -- --check
step "clippy"
cargo "+$TOOLCHAIN" clippy --workspace --all-targets --locked -- -D warnings
step "cargo test"
cargo "+$TOOLCHAIN" test --workspace --locked
step "build hermes-cli (debug) for the Python suites"
cargo build --locked -p hermes-cli
if [[ "${1:-}" != "--quick" ]]; then
  step "python compat suites"
  python3 -m unittest discover -s compat -p 'test_*.py'
fi

step "windows cross-check (hermes-cli, x86_64-pc-windows-gnu)"
ZIG_PY="${ZIG_PYTHON:-$HOME/ref/venv/bin/python}"
if [[ -x "$ZIG_PY" ]] && "$ZIG_PY" -m ziglang version >/dev/null 2>&1 \
   && rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu; then
  W="$(mktemp -d)"
  printf '#!/bin/sh\nargs=""\nfor a in "$@"; do case "$a" in --target=*|-target=*) ;; *) args="$args $(printf %%s "$a" | sed "s/.*/'"'"'&'"'"'/")";; esac; done\neval exec %s -m ziglang cc -target x86_64-windows-gnu $args\n' "$ZIG_PY" > "$W/zigcc"
  printf '#!/bin/sh\nexec %s -m ziglang ar "$@"\n' "$ZIG_PY" > "$W/zigar"
  chmod +x "$W/zigcc" "$W/zigar"
  CC_x86_64_pc_windows_gnu="$W/zigcc" AR_x86_64_pc_windows_gnu="$W/zigar" \
    CARGO_TARGET_DIR="${WIN_TARGET_DIR:-$W/target}" \
    cargo check -p hermes-cli --target x86_64-pc-windows-gnu --locked
else
  echo "skipped: needs ziglang in $ZIG_PY and the x86_64-pc-windows-gnu target"
fi
printf '\nall gates passed\n'
