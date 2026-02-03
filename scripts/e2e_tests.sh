#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: bash scripts/e2e_tests.sh [suite]

Suites:
  gorm_smoke   Run Go/GORM smoke test (e2e/gorm_smoke)
  all          Run all e2e suites (default)

Environment:
  PG_DSN       PostgreSQL DSN (e.g. postgres://admin:admin@127.0.0.1:5433/postgres?sslmode=disable)
EOF
}

suite="${1:-all}"

if ! command -v go >/dev/null 2>&1; then
  echo "ERROR: go is not installed (required for gorm_smoke)" >&2
  exit 1
fi

run_gorm_smoke() {
  echo "=== e2e: gorm_smoke ==="
  (cd "$ROOT_DIR/e2e/gorm_smoke" && go test ./... -count=1)
}

case "$suite" in
  all)
    run_gorm_smoke
    ;;
  gorm_smoke|gorm)
    run_gorm_smoke
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

