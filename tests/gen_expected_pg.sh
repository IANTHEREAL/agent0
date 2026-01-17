#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TESTS_DIR="$ROOT_DIR/tests"
PG_BIN="${PG_BIN:-/usr/lib/postgresql/16/bin}"
PORT="${PG_PORT:-55432}"
DATA_DIR="${PG_DATA_DIR:-/tmp/pgdata}"
LOG_DIR="${PG_LOG_DIR:-/tmp}"

export PATH="$PG_BIN:$PATH"
export PGOPTIONS="${PGOPTIONS:-} -c client_min_messages=warning"

PSQL_ARGS=(
  -X
  -q
  -P pager=off
  -P format=unaligned
  -P fieldsep="|"
  -P null="NULL"
  -h 127.0.0.1
  -p "$PORT"
  -U postgres
  -d postgres
)

cleanup() {
  if [[ -n "${PG_PID:-}" ]] && kill -0 "$PG_PID" 2>/dev/null; then
    pg_ctl -D "$DATA_DIR" -m fast stop > "$LOG_DIR/pg_stop.log" 2>&1 || true
  fi
}
trap cleanup EXIT

rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"
initdb -D "$DATA_DIR" -A trust -U postgres > "$LOG_DIR/initdb.log"

postgres -D "$DATA_DIR" -k /tmp -p "$PORT" > "$LOG_DIR/postgres.log" 2>&1 &
PG_PID=$!

for _ in $(seq 1 30); do
  if pg_isready -h 127.0.0.1 -p "$PORT" -U postgres >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

if ! pg_isready -h 127.0.0.1 -p "$PORT" -U postgres >/dev/null 2>&1; then
  echo "Postgres failed to start" >&2
  exit 1
fi

mapfile -t SQL_LIST < <(find "$TESTS_DIR" -type f -name "*.sql" | sort)

for sql in "${SQL_LIST[@]}"; do
  if [[ "$sql" == *_setup.sql ]]; then
    base="${sql%_setup.sql}.sql"
    if [[ -f "$base" ]]; then
      continue
    fi
  fi

  dir="$(dirname "$sql")"
  base="$(basename "$sql" .sql)"
  setup="$dir/${base}_setup.sql"
  load="$dir/${base}_load.py"
  out="$dir/${base}.expected"
  work_sql="$sql"
  tmp_resolved=""

  if [[ "$sql" == */dvdrental/restore.sql ]]; then
    tmp_resolved="$(mktemp /tmp/restore.resolved.XXXX.sql)"
    sed "s|\\$\\$PATH\\$\\$|$dir|g" "$sql" > "$tmp_resolved"
    work_sql="$tmp_resolved"
  fi

  if [[ -f "$setup" ]]; then
    psql "${PSQL_ARGS[@]}" -f "$setup" > "$LOG_DIR/psql_setup.out" 2>&1 || true
  fi

  if [[ -f "$load" ]]; then
    python3 "$load" --port "$PORT" --user postgres --password "" > "$LOG_DIR/psql_load.out" 2>&1 || true
  fi

  psql "${PSQL_ARGS[@]}" -f "$work_sql" > "$out" 2>&1 || true

  if [[ -n "$tmp_resolved" ]]; then
    rm -f "$tmp_resolved"
  fi
done
