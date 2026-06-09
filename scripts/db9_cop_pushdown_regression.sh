#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

PG_DSN="${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}"

cd "$REPO_ROOT"

mapfile -t SQL_TESTS < <(
  awk '
    /^\[sql\]$/ { in_sql = 1; next }
    /^\[/ { in_sql = 0 }
    in_sql && NF && $0 !~ /^#/ { print }
  ' scripts/regression_gate_pushdown.list
)

python3 scripts/integration_test.py --dsn "$PG_DSN" "${SQL_TESTS[@]}"
