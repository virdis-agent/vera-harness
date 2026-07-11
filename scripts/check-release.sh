#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$ROOT"

command -v cargo >/dev/null 2>&1 || { echo "cargo is required" >&2; exit 1; }
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release --target aarch64-apple-darwin

BINARY="target/aarch64-apple-darwin/release/vera"
if [ ! -f "$BINARY" ]; then BINARY="target/release/vera"; fi
SIZE="$(stat -f%z "$BINARY" 2>/dev/null || stat -c%s "$BINARY")"
if [ "$SIZE" -gt 15728640 ]; then
  echo "release binary is ${SIZE} bytes; limit is 15728640" >&2
  exit 1
fi
echo "release gates passed; stripped binary size=${SIZE} bytes"

