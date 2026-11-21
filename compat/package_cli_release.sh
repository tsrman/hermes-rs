#!/usr/bin/env bash
set -euo pipefail

target=${1:?usage: package_cli_release.sh TARGET OUTPUT_DIRECTORY}
output=${2:?usage: package_cli_release.sh TARGET OUTPUT_DIRECTORY}

case "$target" in
  x86_64-unknown-linux-gnu)
    ;;
  x86_64-unknown-linux-musl)
    export CC_x86_64_unknown_linux_musl=${CC_x86_64_unknown_linux_musl:-musl-gcc}
    ;;
  *)
    printf 'Unsupported release target %s\n' "$target" >&2
    exit 1
    ;;
esac

version=$(cargo metadata --locked --no-deps --format-version 1 |
  jq -er '.packages[] | select(.name == "hermes-cli") | .version')
if [[ ${GITHUB_REF_TYPE:-} == tag && ${GITHUB_REF_NAME:-} != "v$version" ]]; then
  printf 'Expected tag v%s for hermes-cli, got %s\n' "$version" "${GITHUB_REF_NAME:-}" >&2
  exit 1
fi

target_directory=${CARGO_TARGET_DIR:-target}
cargo build --release --locked -p hermes-cli --target "$target" --target-dir "$target_directory"
mkdir -p "$output"
archive="hermes-$version-$target.tar.gz"
tar --mtime=@0 --owner=0 --group=0 --numeric-owner \
  -C "$target_directory/$target/release" -czf "$output/$archive" hermes
(
  cd "$output"
  sha256sum "$archive" > "$archive.sha256"
)
