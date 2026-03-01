#!/usr/bin/env bash
set -euo pipefail

# Safe HNSW stress harness for local verification.
# Goals:
# 1) exercise concurrent UPDATE patterns relevant to issue #1278/#1284
# 2) avoid machine/service hard hangs via strict timeouts and phased load

HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-55435}"
USER_NAME="${USER_NAME:-admin}"
DB_NAME="${DB_NAME:-postgres}"
PGPASSWORD_VALUE="${PGPASSWORD:-admin}"
TABLE_NAME="${TABLE_NAME:-hnsw_stress_safe}"

# Global guardrail for the whole script (seconds).
TOTAL_TIMEOUT_SEC="${TOTAL_TIMEOUT_SEC:-180}"
# Per-statement timeout in shell (seconds).
SQL_TIMEOUT_SEC="${SQL_TIMEOUT_SEC:-15}"
# SQL-level statement timeout (milliseconds).
STMT_TIMEOUT_MS="${STMT_TIMEOUT_MS:-8000}"

# Load profile knobs.
SEED_ROWS="${SEED_ROWS:-300}"
PHASE1_ITERS="${PHASE1_ITERS:-10}"   # single-worker non-vector updates
PHASE2_ITERS="${PHASE2_ITERS:-10}"   # single-worker vector updates
PHASE3_ITERS="${PHASE3_ITERS:-20}"   # dual-worker mixed updates

export PGPASSWORD="$PGPASSWORD_VALUE"
PSQL_BASE=(psql -h "$HOST" -p "$PORT" -U "$USER_NAME" -d "$DB_NAME" -v ON_ERROR_STOP=1 -X -q)

log() { printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*"; }

cleanup_children() {
  pkill -P $$ >/dev/null 2>&1 || true
}
trap cleanup_children EXIT

run_sql() {
  local sql="$1"
  timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" -c "$sql" >/dev/null
}

health_check() {
  timeout "$SQL_TIMEOUT_SEC" pg_isready -h "$HOST" -p "$PORT" >/dev/null
  timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" -c "SELECT 1;" >/dev/null
}

setup_table() {
  log "setup: create table/index and seed rows"
  timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" <<SQL >/dev/null
SET statement_timeout = ${STMT_TIMEOUT_MS};
DROP TABLE IF EXISTS ${TABLE_NAME};
CREATE TABLE ${TABLE_NAME}(
  id SERIAL PRIMARY KEY,
  note TEXT,
  embedding VECTOR(3)
);
INSERT INTO ${TABLE_NAME}(note, embedding)
SELECT
  'seed-' || g,
  format('[%s,%s,%s]', (g%13)::float/13.0, ((g+1)%13)::float/13.0, ((g+2)%13)::float/13.0)::vector(3)
FROM generate_series(1, ${SEED_ROWS}) g;
CREATE INDEX idx_${TABLE_NAME}_hnsw ON ${TABLE_NAME} USING hnsw (embedding vector_l2_ops);
SQL
  health_check
}

phase_single_non_vector() {
  log "phase1: non-vector UPDATE x${PHASE1_ITERS}"
  local i
  for i in $(seq 1 "$PHASE1_ITERS"); do
    run_sql "SET statement_timeout = ${STMT_TIMEOUT_MS}; UPDATE ${TABLE_NAME} SET note='nv-${i}' WHERE id BETWEEN 1 AND 250;"
    health_check
  done
}

phase_single_vector() {
  log "phase2: vector UPDATE x${PHASE2_ITERS}"
  local i
  for i in $(seq 1 "$PHASE2_ITERS"); do
    run_sql "SET statement_timeout = ${STMT_TIMEOUT_MS}; UPDATE ${TABLE_NAME} SET embedding='[0.91,0.11,0.21]' WHERE id=1;"
    health_check
  done
}

phase_mixed_concurrent() {
  log "phase3: mixed concurrent UPDATE x${PHASE3_ITERS} per worker"

  worker_non_vector() {
    local i
    for i in $(seq 1 "$PHASE3_ITERS"); do
      timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" -c "SET statement_timeout = ${STMT_TIMEOUT_MS}; UPDATE ${TABLE_NAME} SET note='mix-nv-${i}' WHERE id BETWEEN 1 AND 250;" >/dev/null || {
        log "worker_non_vector failed at iteration ${i}"
        return 1
      }
    done
  }

  worker_vector() {
    local i
    for i in $(seq 1 "$PHASE3_ITERS"); do
      timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" -c "SET statement_timeout = ${STMT_TIMEOUT_MS}; UPDATE ${TABLE_NAME} SET embedding='[0.91,0.11,0.21]' WHERE id=1;" >/dev/null || {
        log "worker_vector failed at iteration ${i}"
        return 1
      }
    done
  }

  worker_non_vector &
  local pid1=$!
  worker_vector &
  local pid2=$!

  local rc=0
  wait "$pid1" || rc=1
  wait "$pid2" || rc=1

  health_check
  return "$rc"
}

final_check() {
  log "final check"
  timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" -c "SELECT COUNT(*) AS row_count FROM ${TABLE_NAME};"
  timeout "$SQL_TIMEOUT_SEC" "${PSQL_BASE[@]}" -c "SELECT id, note FROM ${TABLE_NAME} ORDER BY id LIMIT 3;"
}

main() {
  log "safe stress start (total timeout=${TOTAL_TIMEOUT_SEC}s)"
  setup_table
  phase_single_non_vector
  phase_single_vector
  phase_mixed_concurrent
  final_check
  log "safe stress completed"
}

if [[ "${1:-}" == "__run" ]]; then
  main
  exit 0
fi

# Global timeout wrapper to guarantee recoverability.
timeout "$TOTAL_TIMEOUT_SEC" "$0" __run || {
  rc=$?
  if [[ $rc -eq 124 ]]; then
    log "aborted by TOTAL_TIMEOUT_SEC=${TOTAL_TIMEOUT_SEC}"
  fi
  exit "$rc"
}
