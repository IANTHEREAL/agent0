#!/usr/bin/env bash
set -euo pipefail

OUT="${1:-artifacts/coverage/line_coverage.json}"
mkdir -p "$(dirname "$OUT")"

# Ensure user-local cargo bin is visible in non-login shells.
export PATH="$HOME/.cargo/bin:$PATH"

run_cov() {
  # Default to frozen mode to avoid network/index access in restricted CI/dev envs.
  # Set COVERAGE_ALLOW_NETWORK=1 to disable --frozen fallback behavior.
  if [[ "${COVERAGE_ALLOW_NETWORK:-0}" == "1" ]]; then
    cargo llvm-cov --locked --json --output-path "$OUT"
  else
    cargo llvm-cov --frozen --json --output-path "$OUT"
  fi
}

if command -v cargo-llvm-cov >/dev/null 2>&1 || cargo llvm-cov --version >/dev/null 2>&1; then
  if run_cov; then
    exit 0
  fi
  if [[ -s "$OUT" ]]; then
    echo "WARN: cargo-llvm-cov failed; keeping existing line coverage at $OUT" >&2
    exit 0
  fi
  cat >"$OUT" <<JSON
{
  "status": "unavailable",
  "reason": "cargo-llvm-cov failed",
  "global_ratio": null,
  "modules": {}
}
JSON
  echo "WARN: cargo-llvm-cov failed; wrote unavailable line coverage to $OUT" >&2
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
