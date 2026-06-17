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
  pg_regress       PG 17.10 regress differential gate, SMOKE subset (auto_testing/corpora/pg_regress_17_10)
  pg_regress_full  PG 17.10 regress differential gate, FULL corpus (slow; nightly/on-demand)
  sqllogictest     sqllogictest output-equivalence gate vs PG 17.10, SMOKE (auto_testing/corpora/sqllogictest)
  sqllogictest_full sqllogictest output-equivalence gate, FULL corpus (slow; nightly/on-demand)

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

run_pg_regress() {
  require_python
  local scope="${1:-smoke}"
  local dsn="${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}"
  local py; py="$(command -v python3 || command -v python)"
  echo "=== e2e: pg_regress ($scope) ==="
  # dedicated, freshly-reset gate db (clean state each run -> deterministic ratchet)
  "$py" "$ROOT_DIR/auto_testing/corpora/_engine/run_corpus.py" \
    --corpus pg_regress_17_10 --dsn "$dsn" --db-name pgcompat_regress --reset-db \
    --check-baseline --"$scope"
}

run_slt() {
  require_python
  local scope="${1:-smoke}"
  local dsn="${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}"
  local py; py="$(command -v python3 || command -v python)"
  echo "=== e2e: sqllogictest ($scope) ==="
  # dedicated, freshly-reset gate db (separate from the pg_regress one)
  "$py" "$ROOT_DIR/auto_testing/corpora/_engine/run_corpus.py" \
    --corpus sqllogictest --dsn "$dsn" --db-name pgcompat_slt --reset-db \
    --check-baseline --"$scope"
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
  pg_regress)
    run_pg_regress smoke
    ;;
  pg_regress_full)
    run_pg_regress full
    ;;
  sqllogictest)
    run_slt smoke
    ;;
  sqllogictest_full)
    run_slt full
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
