#!/bin/bash
set -euo pipefail

# Scope: src/ and tests/ (examples/ has no Rust; benches/ does not exist).

# Search function: uses rg if available, otherwise grep.
search_rust() {
  local pattern="$1"
  shift
  if command -v rg >/dev/null 2>&1; then
    rg -n --type rust "$pattern" "$@" || true
  else
    grep -rn --include='*.rs' -E "$pattern" "$@" || true
  fi
}

# Step 1: Ban multiline #[allow( — any line with #[allow( that doesn't close on same line.
BAD_ML=$(search_rust '#\[allow\(' src/ tests/ | grep -v ')]\|)\]' || true)
if [ -n "$BAD_ML" ]; then
  echo "ERROR: multiline #[allow(...)] found — must be single-line:" >&2
  echo "$BAD_ML" >&2
  exit 1
fi

# Step 2: Every single-line #[allow(...dead_code...)] must have a justification tag.
BAD=$(search_rust '#\[allow\(.*dead_code.*\)\]' src/ tests/ \
  | grep -v '// forward-compat:\|// serde:\|// framework:\|// test:' || true)
if [ -n "$BAD" ]; then
  echo "ERROR: bare #[allow(dead_code)] without justification tag:" >&2
  echo "$BAD" >&2
  exit 1
fi

echo "OK: all #[allow(dead_code)] annotations are single-line with justification tags."
