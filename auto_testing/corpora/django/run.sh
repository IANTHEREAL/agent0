#!/usr/bin/env bash
# Reusable Django-compatibility runner for db9 (lane A-ORM, upstream-suite family).
#
# Runs Django's OWN test suite against a running db9 via the stock postgresql
# wire protocol + the db9_backend override package (test-harness workarounds only).
# Re-runnable: clones/pins Django + builds the venv on first use, reuses after.
#
# Usage:
#   bash run.sh                # smoke tier (default)
#   bash run.sh smoke|full     # named tiers
#   bash run.sh basic lookup   # explicit app list
#
# Requires: a db9 listening at $DB9_HOST:$DB9_PORT (see ../../../docs / the
# local-db9-standup recipe), python3, git, psql.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DJANGO_DIR="${DJANGO_DIR:-/tmp/django}"
DJANGO_TAG="${DJANGO_TAG:-stable/4.2.x}"
VENV="${VENV:-/tmp/djvenv}"
DB9_HOST="${DB9_HOST:-127.0.0.1}"
DB9_PORT="${DB9_PORT:-5455}"
DB9_USER="${DB9_USER:-admin}"
export DB9_HOST DB9_PORT DB9_USER PGPASSWORD="${DB9_PASSWORD:-admin}"

SMOKE_APPS="basic lookup queries aggregation annotations expressions dates datetimes"

log(){ echo "[django-bank] $*"; }

# 1. acquire Django (pinned) on first run
if [ ! -d "$DJANGO_DIR/tests" ]; then
  log "cloning Django $DJANGO_TAG -> $DJANGO_DIR"
  git clone --depth 1 -b "$DJANGO_TAG" https://github.com/django/django "$DJANGO_DIR" || exit 1
fi

# 2. venv + deps on first run
if [ ! -x "$VENV/bin/python" ]; then
  log "creating venv $VENV"
  python3 -m venv "$VENV" || { echo "need python3-venv (apt-get install -y python3-venv)"; exit 1; }
  "$VENV/bin/pip" install -q --upgrade pip
  "$VENV/bin/pip" install -q -e "$DJANGO_DIR" "psycopg[binary]" sqlparse tblib pyyaml pytz asgiref || exit 1
fi

# 3. resolve app set
case "${1:-smoke}" in
  smoke) APPS="$SMOKE_APPS" ;;
  full)  APPS="$(ls -d "$DJANGO_DIR"/tests/*/ | xargs -n1 basename | grep -v __pycache__)" ;;
  *)     APPS="$*" ;;
esac

# 4. preflight: db9 reachable?
if ! psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -tAc "select 1" >/dev/null 2>&1; then
  echo "ERROR: db9 not reachable at $DB9_HOST:$DB9_PORT (start db9 first)"; exit 1
fi

# 5. run each app in isolation (fresh test DBs), capture full output
export PYTHONPATH="$HERE:$DJANGO_DIR/tests"
RESULTS="$HERE/results"; mkdir -p "$RESULTS"
log "tier=${1:-smoke} apps=$(echo $APPS | wc -w) db9=$DB9_HOST:$DB9_PORT"
for app in $APPS; do
  for db in test_db9_default test_db9_other; do
    psql -h "$DB9_HOST" -p "$DB9_PORT" -U "$DB9_USER" -d postgres -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1
  done
  ( cd "$DJANGO_DIR" && timeout "${APP_TIMEOUT:-1800}" "$VENV/bin/python" tests/runtests.py \
      --settings=db9_settings --parallel=1 --verbosity=1 "$app" ) > "$RESULTS/$app.log" 2>&1
  echo "  $app: $(grep -E '^(Ran |OK|FAILED)' "$RESULTS/$app.log" | tr '\n' ' ' | sed 's/  */ /g')"
done

# 6. classify + (re)generate the report
"$VENV/bin/python" "$HERE/classify_failures.py" "$RESULTS" "$HERE/Django-bank.md"
log "done — report: $HERE/Django-bank.md"
