#!/usr/bin/env bash
set -euo pipefail

OUT="${1:-artifacts/coverage/line_coverage.json}"
mkdir -p "$(dirname "$OUT")"

# Ensure user-local cargo bin is visible in non-login shells.
export PATH="$HOME/.cargo/bin:$PATH"

if command -v cargo-llvm-cov >/dev/null 2>&1; then
  cargo llvm-cov --json --output-path "$OUT"
  exit 0
fi

if cargo llvm-cov --version >/dev/null 2>&1; then
  cargo llvm-cov --json --output-path "$OUT"
  exit 0
fi

cat >"$OUT" <<JSON
{
  "status": "unavailable",
  "reason": "cargo-llvm-cov not installed",
  "global_ratio": null,
  "modules": {}
}
JSON

echo "WARN: cargo-llvm-cov not found; wrote unavailable line coverage to $OUT" >&2
