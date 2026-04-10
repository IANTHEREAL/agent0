#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: bash scripts/e2e_tests.sh [suite]

Suites:
  all              Run all e2e suites (default)
  gorm_smoke       Run Go/GORM smoke test (e2e/gorm_smoke)
  sqlalchemy_smoke Run Python SQLAlchemy smoke suite (e2e/sqlalchemy_smoke)
  dify_sqlalchemy_compat Run Dify SQLAlchemy compat suite (e2e/dify_sqlalchemy_compat)

Aliases:
  gorm             Alias for gorm_smoke
  sqlalchemy       Alias for sqlalchemy_smoke
  dify             Alias for dify_sqlalchemy_compat

Environment:
  PG_DSN           PostgreSQL DSN (e.g. postgres://admin:admin@127.0.0.1:5433/postgres?sslmode=disable)
  DB9_RUN_COP_PUSHDOWN_TESTS
                   Opt in to pushdown-specific smoke tests that require a DB9-cop-capable CSE
  DB9_E2E_IGNORE_COP_PUSHDOWN_TESTS
                   Force-skip pushdown-specific smoke tests even when DB9_RUN_COP_PUSHDOWN_TESTS=1

Examples:
  PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres bash scripts/e2e_tests.sh sqlalchemy_smoke
  PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres DB9_RUN_COP_PUSHDOWN_TESTS=1 \
    bash scripts/e2e_tests.sh sqlalchemy_smoke
  PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres bash scripts/e2e_tests.sh dify_sqlalchemy_compat
  PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres bash scripts/e2e_tests.sh all
EOF
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "ERROR: '$cmd' is required but was not found in PATH." >&2
    exit 2
  fi
}

require_python() {
  if command -v python >/dev/null 2>&1; then
    return 0
  fi
  if command -v python3 >/dev/null 2>&1; then
    return 0
  fi
  echo "ERROR: python is required but neither 'python' nor 'python3' was found in PATH." >&2
  exit 2
}

run_gorm_smoke() {
  require_cmd go

  echo "=== e2e: gorm_smoke ==="
  (cd "$ROOT_DIR/e2e/gorm_smoke" && go test ./... -count=1)
}

run_sqlalchemy_smoke() {
  require_cmd uv
  require_python

  local entrypoint="$ROOT_DIR/scripts/e2e_sqlalchemy_smoke.sh"
  if [[ ! -f "$entrypoint" ]]; then
    echo "ERROR: missing entrypoint: $entrypoint" >&2
    exit 2
  fi

  echo "=== e2e: sqlalchemy_smoke ==="
  bash "$entrypoint"
}

run_dify_sqlalchemy_compat() {
  require_cmd uv
  require_python

  local entrypoint="$ROOT_DIR/scripts/e2e_dify_sqlalchemy_compat.sh"
  if [[ ! -f "$entrypoint" ]]; then
    echo "ERROR: missing entrypoint: $entrypoint" >&2
    exit 2
  fi

  echo "=== e2e: dify_sqlalchemy_compat ==="
  bash "$entrypoint"
}

suite="${1:-all}"

case "$suite" in
  all)
    run_gorm_smoke
    echo ""
    run_sqlalchemy_smoke
    echo ""
    run_dify_sqlalchemy_compat
    ;;
  gorm_smoke|gorm)
    run_gorm_smoke
    ;;
  sqlalchemy_smoke|sqlalchemy)
    run_sqlalchemy_smoke
    ;;
  dify_sqlalchemy_compat|dify)
    run_dify_sqlalchemy_compat
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    echo "Unknown suite: $suite" >&2
    usage >&2
    exit 2
    ;;
esac
