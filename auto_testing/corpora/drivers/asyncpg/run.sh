#!/usr/bin/env bash
# Reusable asyncpg (Python async PostgreSQL driver) compatibility runner for db9 — lane A-DRIVERS.
#
# Runs asyncpg's OWN test suite against db9 — the connect-path surface (extended
# protocol, BINARY type/OID codecs, prepared statements, COPY, cursors,
# introspection) from a THIRD independent implementation after psycopg and pgx.
# asyncpg is binary-protocol-heavy, so it stresses binary codecs + type
# introspection harder than psycopg.
#
# IMPORTANT — issue #2721: asyncpg's graceful Connection.close() HANGS against db9
# (db9 doesn't close the socket on the Terminate 'X' message). Every test's
# teardown calls close(), so without a workaround the suite cannot complete. This
# runner installs a conftest.py that makes close() abrupt (terminate) so teardowns
# finish and the suite produces numbers. #2721 is itself a logged db9 gap; the
# workaround does NOT mask query semantics, only the connection-teardown path.
#
# Usage:
#   bash run.sh                         # smoke set (representative DB-behavior files)
#   bash run.sh full                    # all tests/test_*.py (slow; on-demand)
#   bash run.sh tests/test_codecs.py    # explicit file(s)
#
# Requires: a db9 at $DB9_HOST:$DB9_PORT, python3 + python3-dev, git, psql, a C compiler.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

APG_DIR="${APG_DIR:-/tmp/asyncpg}"
APG_TAG="${APG_TAG:-v0.30.0}"
VENV="${APG_VENV:-/tmp/apgvenv}"
DB9_HOST="${DB9_HOST:-127.0.0.1}"
DB9_PORT="${DB9_PORT:-5455}"
DB9_USER="${DB9_USER:-admin}"
export PGPASSWORD="${DB9_PASSWORD:-admin}"
TESTDB="${TESTDB:-asyncpg_test}"
PYBIN="${PYBIN:-python3}"

log(){ echo "[asyncpg-bank] $*"; }

# 1. checkout asyncpg (submodules: pgproto is a submodule; the C extensions need it)
if [ ! -d "$APG_DIR/.git" ]; then
  log "cloning asyncpg @ $APG_TAG -> $APG_DIR (with submodules)"
  git clone --depth 1 --recurse-submodules -b "$APG_TAG" https://github.com/MagicStack/asyncpg "$APG_DIR" || exit 1
fi

# 2. venv + build the C extensions in-place
if [ ! -x "$VENV/bin/python" ]; then
  log "creating venv $VENV"
  "$PYBIN" -m venv "$VENV" || exit 1
fi
# setuptools<81: pkg_resources was removed in newer setuptools (py3.12) and asyncpg's
# build + _testbase still import it. Cython + the in-place build are required because
# asyncpg ships .pyx that must be compiled against the pgproto submodule.
"$VENV/bin/pip" install -q "setuptools<81" wheel Cython pytest pytest-timeout pytest-json-report 2>&1 | tail -1
( cd "$APG_DIR" && "$VENV/bin/python" setup.py build_ext --inplace >/tmp/apg_build.log 2>&1 ) \
  || { log "build_ext failed (need python3-dev / compiler) — see /tmp/apg_build.log"; tail -5 /tmp/apg_build.log; exit 1; }
"$VENV/bin/pip" install -q -e "$APG_DIR" 2>&1 | tail -1

# 3. db9 reachable?
if ! psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -tAc "select 1" >/dev/null 2>&1; then
  echo "ERROR: db9 not reachable at $DB9_HOST:$DB9_PORT"; exit 1
fi
psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -c "CREATE DATABASE $TESTDB" >/dev/null 2>&1 || true

# 4. conftest.py — workaround for #2721 (make graceful close() abrupt so teardowns finish)
cat > "$APG_DIR/conftest.py" <<'PY'
# db9 workaround (logged gap #2721): db9 doesn't close the socket on Terminate, so
# asyncpg's graceful Connection.close() hangs in every test's teardown. Make close()
# abrupt (terminate) so the suite can run. Does NOT mask query semantics.
import asyncpg.connection
async def _close(self, *, timeout=None):
    self.terminate()
asyncpg.connection.Connection.close = _close
try:
    import asyncpg.pool
    async def _pclose(self, *, timeout=None):
        self.terminate()
    asyncpg.pool.Pool.close = _pclose
except Exception:
    pass
PY

# 5. run pytest per-file with json-report (each file isolated; --timeout catches any
#    residual op-level hang). The smoke set is the DB-behavior core; pass explicit
#    files as args to widen.
SMOKE="test_prepare test_execute test_cursor test_codecs test_copy test_introspection test_record test_pool"
RESULTS="$HERE/results"; mkdir -p "$RESULTS"; rm -f "$RESULTS"/*.json
export PGHOST="$DB9_HOST" PGPORT="$DB9_PORT" PGUSER="$DB9_USER" PGDATABASE="$TESTDB"
case "${1:-smoke}" in
  smoke) TARGETS=""; for f in $SMOKE; do TARGETS="$TARGETS tests/$f.py"; done ;;
  full)  TARGETS="$(cd "$APG_DIR" && ls tests/test_*.py)" ;;        # all test files (slow)
  *)     TARGETS="$*" ;;                                            # explicit file(s)
esac
log "running asyncpg $APG_TAG vs db9=$DB9_HOST:$DB9_PORT (scope: ${1:-smoke})"
for t in $TARGETS; do
  base="$(basename "$t" .py)"
  [ -f "$APG_DIR/$t" ] || continue
  ( cd "$APG_DIR" && timeout "${FILE_TIMEOUT:-300}" "$VENV/bin/python" -m pytest "$t" \
      --timeout="${TEST_TIMEOUT:-30}" --timeout-method=thread -q --no-header \
      --json-report --json-report-file="$RESULTS/$base.json" >/dev/null 2>&1 )
  log "ran $base"
done

# 6. classify -> bank
"${VENV}/bin/python" "$HERE/classify_failures.py" "$RESULTS" "$HERE/asyncpg-bank.md" \
  || "$PYBIN" "$HERE/classify_failures.py" "$RESULTS" "$HERE/asyncpg-bank.md"
log "done — report: $HERE/asyncpg-bank.md"
