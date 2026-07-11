#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release >/dev/null
BINARY="target/release/vera"
"$BINARY" --version >/dev/null
ELAPSED_MS="$( { /usr/bin/time -p "$BINARY" --version >/dev/null; } 2>&1 | awk '$1 == "real" { printf "%d", $2 * 1000 }' )"
echo "warm version startup: ${ELAPSED_MS}ms"
if [ "$ELAPSED_MS" -gt 75 ]; then exit 1; fi
