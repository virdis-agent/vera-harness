#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"

cargo build --release >/dev/null
BINARY="target/release/vera"

SIZE="$(stat -f%z "$BINARY" 2>/dev/null || stat -c%s "$BINARY")"
"$BINARY" --version >/dev/null
STARTUP_MS="$( { /usr/bin/time -p "$BINARY" --version >/dev/null; } 2>&1 | awk '$1 == "real" { printf "%d", $2 * 1000 }' )"
SCHEMA_OUTPUT="$($BINARY inspect)"
SCHEMA_TOKENS="$(printf '%s\n' "$SCHEMA_OUTPUT" | awk -F': ' '/^tool schema tokens:/ {print $2}')"
SCHEMA_BYTES="$(printf '%s\n' "$SCHEMA_OUTPUT" | awk -F': ' '/^tool schema bytes:/ {print $2}')"

if /usr/bin/time -l "$BINARY" --version >/dev/null 2>"$ROOT/target/vera-memory-benchmark.txt"; then
  RSS="$(awk '$2 == "maximum" && $3 == "resident" && $4 == "set" && $5 == "size" {print $1; found=1} END {if (!found) print "unavailable"}' "$ROOT/target/vera-memory-benchmark.txt")"
else
  RSS="unavailable"
fi

AGENT_SECONDS="$( { /usr/bin/time -p cargo test --locked subagents::tests::runs_tasks_and_enforces_four_agent_concurrency -- --nocapture >/dev/null; } 2>&1 | awk '$1 == "real" { print $2 }' )"

echo "release binary bytes: $SIZE"
echo "warm version startup ms: $STARTUP_MS"
echo "maximum resident set size: $RSS"
echo "tool schema tokens: ${SCHEMA_TOKENS:-unavailable}"
echo "tool schema bytes: ${SCHEMA_BYTES:-unavailable}"
echo "four-agent fixture wall seconds: ${AGENT_SECONDS:-unavailable}"
