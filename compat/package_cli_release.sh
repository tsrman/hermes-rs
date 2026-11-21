#!/usr/bin/env bash
set -euo pipefail
exec python3 -m compat.package_cli_release "$@"
