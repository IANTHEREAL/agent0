#!/usr/bin/env bash
# Reusable psycopg (PG driver) compatibility runner for db9 — lane A-DRIVERS.
#
# Runs psycopg3's OWN test suite against a running db9. psycopg is the dominant
# Python PostgreSQL driver; its suite hammers the CONNECT-PATH surface that the
# pure-SQL (pg_regress) and ORM (Django) lanes barely touch: extended-protocol
# parameter type inference, binary type/OID codecs, SQLSTATE fidelity, COPY,
# transaction control, catalog introspection.
#
# Re-runnable: clones+pins psycopg + builds a venv on first run, reuses after.
#
# Usage:
#   bash run.sh                  # smoke tier (core DB-behavior files)
#   bash run.sh smoke|full       # named tiers
#   bash run.sh tests/test_x.py  # explicit targets
#
# Requires: a db9 at $DB9_HOST:$DB9_PORT, python3 (+venv), git, psql.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PSYCOPG_DIR="${PSYCOPG_DIR:-/tmp/psycopg}"
PSYCOPG_TAG="${PSYCOPG_TAG:-3.3.4}"          # pinned release (has matching binary wheel)
VENV="${VENV:-/tmp/pgvenv}"
DB9_HOST="${DB9_HOST:-127.0.0.1}"
DB9_PORT="${DB9_PORT:-5455}"
DB9_USER="${DB9_USER:-admin}"
export PGPASSWORD="${DB9_PASSWORD:-admin}"
TESTDB="${TESTDB:-psycopg_test}"

# Core DB-behavior files (smoke). Excludes tooling: test_typing.py is mypy static
# checks; lint/packaging tests need dev tools — none are db9 gaps.
SMOKE_FILES="tests/test_connection.py tests/test_cursor.py tests/test_cursor_common.py \
tests/test_adapt.py tests/test_copy.py tests/test_column.py tests/test_sql.py \
tests/test_conninfo.py tests/test_connection_info.py tests/test_transaction.py \
tests/test_capabilities.py tests/test_pipeline.py"
# Marker exclusions: slow=long, subprocess=spawns mypy/etc, flakey/timing=nondeterministic.
MARKERS="not slow and not subprocess and not flakey and not timing"

log(){ echo "[psycopg-bank] $*"; }

# 1. acquire psycopg (pinned) on first run
if [ ! -d "$PSYCOPG_DIR/tests" ]; then
  log "cloning psycopg @ $PSYCOPG_TAG -> $PSYCOPG_DIR"
  git clone --depth 1 -b "$PSYCOPG_TAG" https://github.com/psycopg/psycopg "$PSYCOPG_DIR" || exit 1
fi

# 2. venv + deps on first run. NON-editable install (an editable install leaves a
#    namespace finder the bare project dir shadows -> broken `import psycopg`).
if [ ! -x "$VENV/bin/python" ] || ! "$VENV/bin/python" -c "import psycopg, pytest_jsonreport" 2>/dev/null; then
  log "creating venv $VENV + installing psycopg[test,binary] (non-editable)"
  python3 -m venv "$VENV" || { echo "need python3-venv"; exit 1; }
  "$VENV/bin/pip" install -q --upgrade pip
  "$VENV/bin/pip" install -q "$PSYCOPG_DIR/psycopg[test,binary]" pytest-json-report || exit 1
fi

# 3. preflight + fresh test db
if ! psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -tAc "select 1" >/dev/null 2>&1; then
  echo "ERROR: db9 not reachable at $DB9_HOST:$DB9_PORT"; exit 1
fi
psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -c "DROP DATABASE IF EXISTS $TESTDB" >/dev/null 2>&1
psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -c "CREATE DATABASE $TESTDB" >/dev/null 2>&1

# 4. resolve target files
case "${1:-smoke}" in
  smoke) FILES="$SMOKE_FILES" ;;
  full)  FILES="$(cd "$PSYCOPG_DIR" && ls tests/test_*.py | grep -v test_typing.py)" ;;
  *)     FILES="$*" ;;
esac

export PSYCOPG_TEST_DSN="host=$DB9_HOST port=$DB9_PORT user=$DB9_USER password=$PGPASSWORD dbname=$TESTDB"
RESULTS="$HERE/results"; rm -f "$RESULTS"/*.json "$RESULTS"/*.crashed 2>/dev/null; mkdir -p "$RESULTS"
log "tier=${1:-smoke} db9=$DB9_HOST:$DB9_PORT psycopg=$PSYCOPG_TAG"
# Per-file isolation: a crash/hang in one file must not lose every other file's
# results (pytest-json-report only writes on a clean session end). One report per file.
for f in $FILES; do
  base="$(basename "$f" .py)"
  # --cache-clear: psycopg's conftest caches a "segfault" flag and refuses to run if
  # a PRIOR run segfaulted; clearing per file stops one file's segfault (itself a db9
  # finding) from blocking every later file.
  ( cd "$PSYCOPG_DIR" && timeout "${FILE_TIMEOUT:-600}" "$VENV/bin/python" -m pytest "$f" \
      -m "$MARKERS" -q --no-header --cache-clear \
      --json-report --json-report-file="$RESULTS/$base.json" >/dev/null 2>&1 )
  rc=$?
  if [ -f "$RESULTS/$base.json" ]; then
    echo "  $base: $("$VENV/bin/python" -c "import json,sys; s=json.load(open('$RESULTS/$base.json'))['summary']; print('pass=%d fail=%d err=%d skip=%d'%(s.get('passed',0),s.get('failed',0),s.get('error',0),s.get('skipped',0)))" 2>/dev/null)"
  else
    echo "$base" > "$RESULTS/$base.crashed"
    echo "  $base: NO REPORT (rc=$rc — crashed/hung; recorded as a db9 gap)"
  fi
done

# 5. classify all per-file reports -> regenerate the bank
"$VENV/bin/python" "$HERE/classify_failures.py" "$RESULTS" "$PSYCOPG_DIR" "$HERE/psycopg-bank.md"
log "done — report: $HERE/psycopg-bank.md"
