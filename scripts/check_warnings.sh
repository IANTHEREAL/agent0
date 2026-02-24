#!/bin/bash
set -euo pipefail

# Preflight: jq is required for JSON filtering.
if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required but not found. Install with: apt install jq / brew install jq" >&2
  exit 1
fi

JSON_OUT=$(mktemp)
STDERR_OUT=$(mktemp)
trap 'rm -f "$JSON_OUT" "$STDERR_OUT"' EXIT

# Step 1: Run cargo check. JSON goes to stdout→file, diagnostics to stderr→file.
if ! cargo check -p db9-server --all-targets --all-features \
       --message-format=json >"$JSON_OUT" 2>"$STDERR_OUT"; then
  echo "ERROR: cargo check failed:" >&2
  cat "$STDERR_OUT" >&2
  exit 1
fi

# Step 2: Extract db9-server warnings via package_id (matches #db9-server@ fragment).
# This catches ALL targets within the db9-server package.
PG_WARNINGS=$(jq -r '
  select(.reason == "compiler-message")
  | select(.package_id | test("#db9-server@"))
  | select(.message.level == "warning")
  | .message.rendered
' "$JSON_OUT")

if [ -n "$PG_WARNINGS" ]; then
  COUNT=$(echo "$PG_WARNINGS" | grep -c '.' || true)
  echo "ERROR: db9-server has $COUNT warning(s) (must be 0):" >&2
  echo "$PG_WARNINGS" >&2
  exit 1
fi
echo "OK: db9-server has 0 warnings."
