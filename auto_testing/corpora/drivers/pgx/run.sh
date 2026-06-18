#!/usr/bin/env bash
# Reusable pgx (Go PostgreSQL driver) compatibility runner for db9 — lane A-DRIVERS.
#
# Runs pgx's OWN test suite against db9 — the connect-path surface (extended
# protocol, binary type/OID codecs, COPY, pipelining) from a different (Go)
# implementation than psycopg. Re-runnable: clones+pins pgx on first run.
#
# Usage:
#   bash run.sh                  # full suite (go test ./...)
#   bash run.sh ./pgtype         # explicit package(s)
#
# Requires: a db9 at $DB9_HOST:$DB9_PORT, go, git, psql.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PGX_DIR="${PGX_DIR:-/tmp/pgx}"
PGX_TAG="${PGX_TAG:-v5.10.0}"
DB9_HOST="${DB9_HOST:-127.0.0.1}"
DB9_PORT="${DB9_PORT:-5455}"
DB9_USER="${DB9_USER:-admin}"
export PGPASSWORD="${DB9_PASSWORD:-admin}"
TESTDB="${TESTDB:-pgx_test}"

log(){ echo "[pgx-bank] $*"; }

if [ ! -d "$PGX_DIR/.git" ]; then
  log "cloning pgx @ $PGX_TAG -> $PGX_DIR"
  git clone --depth 1 -b "$PGX_TAG" https://github.com/jackc/pgx "$PGX_DIR" || exit 1
fi
( cd "$PGX_DIR" && go mod download 2>&1 | tail -1 )

if ! psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -tAc "select 1" >/dev/null 2>&1; then
  echo "ERROR: db9 not reachable at $DB9_HOST:$DB9_PORT"; exit 1
fi
psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -c "DROP DATABASE IF EXISTS $TESTDB" >/dev/null 2>&1
psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -c "CREATE DATABASE $TESTDB" >/dev/null 2>&1

TARGETS="${*:-./...}"
export PGX_TEST_DATABASE="postgres://$DB9_USER:$PGPASSWORD@$DB9_HOST:$DB9_PORT/$TESTDB"
RESULTS="$HERE/results"; mkdir -p "$RESULTS"
log "running pgx $PGX_TAG vs db9=$DB9_HOST:$DB9_PORT (targets: $TARGETS)"
# go test isolates per-package (each package is its own test binary) — a failure
# in one package can't lose the others. -json -> machine-readable for the classifier.
( cd "$PGX_DIR" && timeout "${RUN_TIMEOUT:-1800}" go test $TARGETS -count=1 -json > "$RESULTS/pgx.json" 2>"$RESULTS/pgx.err" )
log "go test done (rc may be 1 on failures — expected)"

"${PYTHON:-python3}" "$HERE/classify_failures.py" "$RESULTS/pgx.json" "$HERE/pgx-bank.md"
log "done — report: $HERE/pgx-bank.md"
