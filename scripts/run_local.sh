#!/bin/bash
# Builds and runs the collector (default config/local.toml). With LIGHTER_KEY_FILE pointing at a JSON file holding
# "private_key", the Lighter API key reaches the process environment only (not argv, not disk).
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -n "${LIGHTER_KEY_FILE:-}" ]]; then
  LIGHTER_API_PRIVATE_KEY="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["private_key"])' "$LIGHTER_KEY_FILE")"
  export LIGHTER_API_PRIVATE_KEY
fi
cargo build --release -q
exec ./target/release/pair_monitor "${1:-config/local.toml}"
