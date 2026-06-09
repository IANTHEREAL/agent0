#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

PG_DSN="${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}"

cd "$REPO_ROOT"

python3 scripts/check_oracle_coverage.py

bash scripts/regression_gate.sh \
  --dsn "$PG_DSN" \
  --manifest scripts/regression_gate_pushdown.list \
  --skip-orm \
  --skip-unit \
  --skip-build \
  "$@"
