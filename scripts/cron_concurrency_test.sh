#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PG_HOST="${PG_HOST:-127.0.0.1}"
PG_PORT_A="${PG_PORT_A:-5433}"
PG_PORT_B="${PG_PORT_B:-5434}"
PG_USER="${PG_USER:-default.admin}"
PG_PASSWORD="${PG_PASSWORD:-admin}"
PG_DB="${PG_DB:-postgres}"
PD_ENDPOINTS="${PD_ENDPOINTS:-127.0.0.1:2379}"
CRON_POLL_MS="${DB9_CRON_POLL_MS:-10000}"
CRON_ORPHAN_TIMEOUT_SEC="${DB9_CRON_ORPHAN_TIMEOUT_SEC:-30}"
WAIT_SECONDS="${WAIT_SECONDS:-190}"
READY_TIMEOUT_SEC="${READY_TIMEOUT_SEC:-90}"

DB9_PID_A=""
DB9_PID_B=""
LOG_A="/tmp/db9-cron-concurrency-a.log"
LOG_B="/tmp/db9-cron-concurrency-b.log"

TEST_DB_CONCURRENCY=""
TEST_DB_NOEXT=""
TEST_DB_ORPHAN=""
CONCURRENCY_JOB_ID=""
ORPHAN_JOB_ID=""

TOTAL_TESTS=0
FAILED_TESTS=0

usage() {
  cat <<'EOF'
Usage: bash scripts/cron_concurrency_test.sh [options]

Fully-automated pg_cron distributed concurrency and edge-case validation.

Options:
  --pd-endpoints <addr>         TiKV PD endpoints (default: $PD_ENDPOINTS or 127.0.0.1:2379)
  --port-a <port>               db9-server instance A port (default: 5433)
  --port-b <port>               db9-server instance B port (default: 5434)
  --host <host>                 pg host for psql (default: 127.0.0.1)
  --wait-seconds <sec>          Concurrency wait duration (default: 190)
  --cron-poll-ms <ms>           DB9_CRON_POLL_MS (default: 10000)
  --orphan-timeout-sec <sec>    DB9_CRON_ORPHAN_TIMEOUT_SEC (default: 30)
  -h, --help                    Show this help

Environment overrides:
  PD_ENDPOINTS, PG_PORT_A, PG_PORT_B, PG_HOST, PG_USER, PG_PASSWORD,
  DB9_CRON_POLL_MS, DB9_CRON_ORPHAN_TIMEOUT_SEC, WAIT_SECONDS
EOF
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "ERROR: required command not found: $cmd" >&2
    exit 2
  fi
}

log() {
  printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"
}

pass() {
  log "PASS: $*"
}

fail() {
  log "FAIL: $*"
  FAILED_TESTS=$((FAILED_TESTS + 1))
}

run_test() {
  local name="$1"
  shift
  TOTAL_TESTS=$((TOTAL_TESTS + 1))
  log "--- Running test: $name ---"
  if "$@"; then
    pass "$name"
  else
    fail "$name"
  fi
}

psql_exec() {
  local port="$1"
  local db="$2"
  local sql="$3"
  PGPASSWORD="$PG_PASSWORD" psql \
    -h "$PG_HOST" \
    -p "$port" \
    -U "$PG_USER" \
    -d "$db" \
    -v ON_ERROR_STOP=1 \
    -Atq \
    -c "$sql"
}

wait_for_ready() {
  local port="$1"
  local pid="$2"
  local timeout_sec="$3"
  local elapsed=0

  while (( elapsed < timeout_sec )); do
    if psql_exec "$port" "$PG_DB" "SELECT 1;" >/dev/null 2>&1; then
      return 0
    fi

    if ! kill -0 "$pid" 2>/dev/null; then
      return 1
    fi

    sleep 1
    elapsed=$((elapsed + 1))
  done

  return 1
}

drop_database_if_exists() {
  local port="$1"
  local dbname="$2"
  if [[ -z "$dbname" ]]; then
    return 0
  fi

  psql_exec "$port" "$PG_DB" "
    SELECT pg_terminate_backend(pid)
    FROM pg_stat_activity
    WHERE datname = '$dbname' AND pid <> pg_backend_pid();
  " >/dev/null 2>&1 || true

  psql_exec "$port" "$PG_DB" "DROP DATABASE IF EXISTS $dbname;" >/dev/null 2>&1 || true
}

cleanup() {
  set +e
  log "Starting cleanup"

  if [[ -n "$TEST_DB_CONCURRENCY" ]]; then
    drop_database_if_exists "$PG_PORT_B" "$TEST_DB_CONCURRENCY"
    drop_database_if_exists "$PG_PORT_A" "$TEST_DB_CONCURRENCY"
  fi
  if [[ -n "$TEST_DB_NOEXT" ]]; then
    drop_database_if_exists "$PG_PORT_B" "$TEST_DB_NOEXT"
    drop_database_if_exists "$PG_PORT_A" "$TEST_DB_NOEXT"
  fi
  if [[ -n "$TEST_DB_ORPHAN" ]]; then
    drop_database_if_exists "$PG_PORT_B" "$TEST_DB_ORPHAN"
    drop_database_if_exists "$PG_PORT_A" "$TEST_DB_ORPHAN"
  fi

  if [[ -n "$DB9_PID_A" ]] && kill -0 "$DB9_PID_A" 2>/dev/null; then
    log "Stopping instance A (PID=$DB9_PID_A)"
    kill "$DB9_PID_A" 2>/dev/null || true
    wait "$DB9_PID_A" 2>/dev/null || true
  fi

  if [[ -n "$DB9_PID_B" ]] && kill -0 "$DB9_PID_B" 2>/dev/null; then
    log "Stopping instance B (PID=$DB9_PID_B)"
    kill "$DB9_PID_B" 2>/dev/null || true
    wait "$DB9_PID_B" 2>/dev/null || true
  fi

  log "Cleanup complete"
}
trap cleanup EXIT

while [[ $# -gt 0 ]]; do
  case "$1" in
    --pd-endpoints)
      PD_ENDPOINTS="${2:-}"
      shift 2
      ;;
    --port-a)
      PG_PORT_A="${2:-}"
      shift 2
      ;;
    --port-b)
      PG_PORT_B="${2:-}"
      shift 2
      ;;
    --host)
      PG_HOST="${2:-}"
      shift 2
      ;;
    --wait-seconds)
      WAIT_SECONDS="${2:-}"
      shift 2
      ;;
    --cron-poll-ms)
      CRON_POLL_MS="${2:-}"
      shift 2
      ;;
    --orphan-timeout-sec)
      CRON_ORPHAN_TIMEOUT_SEC="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

start_instances() {
  require_cmd cargo
  require_cmd psql

  log "Building db9-server (release)"
  (cd "$ROOT_DIR" && cargo build --release)

  log "Starting instance A on ${PG_HOST}:${PG_PORT_A}"
  (
    cd "$ROOT_DIR" && \
    PD_ENDPOINTS="$PD_ENDPOINTS" \
    PG_PORT="$PG_PORT_A" \
    DB9_CRON_POLL_MS="$CRON_POLL_MS" \
    DB9_CRON_ORPHAN_TIMEOUT_SEC="$CRON_ORPHAN_TIMEOUT_SEC" \
    ./target/release/db9-server
  ) >"$LOG_A" 2>&1 &
  DB9_PID_A=$!

  log "Starting instance B on ${PG_HOST}:${PG_PORT_B}"
  (
    cd "$ROOT_DIR" && \
    PD_ENDPOINTS="$PD_ENDPOINTS" \
    PG_PORT="$PG_PORT_B" \
    DB9_CRON_POLL_MS="$CRON_POLL_MS" \
    DB9_CRON_ORPHAN_TIMEOUT_SEC="$CRON_ORPHAN_TIMEOUT_SEC" \
    ./target/release/db9-server
  ) >"$LOG_B" 2>&1 &
  DB9_PID_B=$!

  log "Waiting for instance A readiness"
  if ! wait_for_ready "$PG_PORT_A" "$DB9_PID_A" "$READY_TIMEOUT_SEC"; then
    log "Instance A failed to become ready; tailing log"
    tail -n 200 "$LOG_A" || true
    return 1
  fi

  log "Waiting for instance B readiness"
  if ! wait_for_ready "$PG_PORT_B" "$DB9_PID_B" "$READY_TIMEOUT_SEC"; then
    log "Instance B failed to become ready; tailing log"
    tail -n 200 "$LOG_B" || true
    return 1
  fi

  log "Both instances are ready"
}

make_test_db_name() {
  local prefix="$1"
  printf '%s_%s' "$prefix" "$(date +%s)"
}

test_extension_not_installed_error() {
  TEST_DB_NOEXT="$(make_test_db_name cron_noext)"
  psql_exec "$PG_PORT_A" "$PG_DB" "CREATE DATABASE $TEST_DB_NOEXT;" >/dev/null

  local out
  local rc
  set +e
  out=$(PGPASSWORD="$PG_PASSWORD" psql \
    -h "$PG_HOST" \
    -p "$PG_PORT_A" \
    -U "$PG_USER" \
    -d "$TEST_DB_NOEXT" \
    -v ON_ERROR_STOP=1 \
    -Atq \
    -c "SELECT cron.schedule('* * * * *', 'SELECT 1');" 2>&1)
  rc=$?
  set -e

  if [[ "$rc" -eq 0 ]]; then
    log "Expected failure without pg_cron extension, but command succeeded"
    return 1
  fi

  local lowered
  lowered="${out,,}"
  if [[ "$lowered" == *"extension"* ]] || [[ "$lowered" == *"not installed"* ]] || [[ "$lowered" == *"schema \"cron\" does not exist"* ]] || [[ "$lowered" == *"function cron.schedule"* ]]; then
    return 0
  fi

  log "Unexpected error output: $out"
  return 1
}

test_concurrency_single_execution_per_tick() {
  TEST_DB_CONCURRENCY="$(make_test_db_name cron_concurrency)"
  psql_exec "$PG_PORT_A" "$PG_DB" "CREATE DATABASE $TEST_DB_CONCURRENCY;" >/dev/null

  psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "CREATE EXTENSION IF NOT EXISTS pg_cron;" >/dev/null
  psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "SELECT cron.unschedule(jobid) FROM cron.job;" >/dev/null || true

  CONCURRENCY_JOB_ID="$(psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "SELECT cron.schedule('* * * * *', 'SELECT 1');")"
  if [[ -z "$CONCURRENCY_JOB_ID" ]]; then
    log "Failed to create cron job"
    return 1
  fi

  log "Scheduled job_id=$CONCURRENCY_JOB_ID on instance A; waiting ${WAIT_SECONDS}s for multiple ticks"
  sleep "$WAIT_SECONDS"

  local has_scheduled_time
  has_scheduled_time="$(psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "
    SELECT EXISTS (
      SELECT 1
      FROM information_schema.columns
      WHERE table_schema='cron'
        AND table_name='job_run_details'
        AND column_name='scheduled_time'
    );
  ")"

  local duplicate_buckets
  if [[ "$has_scheduled_time" == "t" ]]; then
    duplicate_buckets="$(psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "
      SELECT count(*)
      FROM (
        SELECT scheduled_time, count(*) AS cnt
        FROM cron.job_run_details
        WHERE jobid = $CONCURRENCY_JOB_ID
        GROUP BY scheduled_time
        HAVING count(*) > 1
      ) dup;
    ")"
  else
    duplicate_buckets="$(psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "
      SELECT count(*)
      FROM (
        SELECT date_trunc('minute', start_time) AS scheduled_minute, count(*) AS cnt
        FROM cron.job_run_details
        WHERE jobid = $CONCURRENCY_JOB_ID
        GROUP BY date_trunc('minute', start_time)
        HAVING count(*) > 1
      ) dup;
    ")"
  fi

  local total_runs
  total_runs="$(psql_exec "$PG_PORT_A" "$TEST_DB_CONCURRENCY" "SELECT count(*) FROM cron.job_run_details WHERE jobid = $CONCURRENCY_JOB_ID;")"

  log "Observed runs for job_id=$CONCURRENCY_JOB_ID: total_runs=$total_runs, duplicate_buckets=$duplicate_buckets"

  if [[ "$duplicate_buckets" != "0" ]]; then
    log "Detected duplicate executions per scheduled bucket"
    return 1
  fi

  if [[ "$total_runs" -lt 1 ]]; then
    log "Expected at least one execution but got zero"
    return 1
  fi

  return 0
}

test_orphan_recovery_failover_smoke() {
  TEST_DB_ORPHAN="$(make_test_db_name cron_orphan)"
  psql_exec "$PG_PORT_A" "$PG_DB" "CREATE DATABASE $TEST_DB_ORPHAN;" >/dev/null

  psql_exec "$PG_PORT_A" "$TEST_DB_ORPHAN" "CREATE EXTENSION IF NOT EXISTS pg_cron;" >/dev/null
  psql_exec "$PG_PORT_A" "$TEST_DB_ORPHAN" "SELECT cron.unschedule(jobid) FROM cron.job;" >/dev/null || true

  ORPHAN_JOB_ID="$(psql_exec "$PG_PORT_A" "$TEST_DB_ORPHAN" "SELECT cron.schedule('* * * * *', 'SELECT 1');")"
  if [[ -z "$ORPHAN_JOB_ID" ]]; then
    log "Failed to create orphan recovery job"
    return 1
  fi

  local initial_runs
  initial_runs="$(psql_exec "$PG_PORT_A" "$TEST_DB_ORPHAN" "SELECT count(*) FROM cron.job_run_details WHERE jobid = $ORPHAN_JOB_ID;")"

  log "Orphan/failover concept: claim ownership is persisted in TiKV; after owner death and timeout (${CRON_ORPHAN_TIMEOUT_SEC}s), another instance can continue scheduling."
  log "Simulating abrupt owner loss by SIGKILL on instance A."

  if [[ -n "$DB9_PID_A" ]] && kill -0 "$DB9_PID_A" 2>/dev/null; then
    kill -9 "$DB9_PID_A" 2>/dev/null || true
    wait "$DB9_PID_A" 2>/dev/null || true
  fi
  DB9_PID_A=""

  local deadline=$(( $(date +%s) + CRON_ORPHAN_TIMEOUT_SEC + 120 ))
  local current_runs="$initial_runs"
  while (( $(date +%s) < deadline )); do
    current_runs="$(psql_exec "$PG_PORT_B" "$TEST_DB_ORPHAN" "SELECT count(*) FROM cron.job_run_details WHERE jobid = $ORPHAN_JOB_ID;")"
    if [[ "$current_runs" -gt "$initial_runs" ]]; then
      break
    fi
    sleep 5
  done

  log "Orphan/failover smoke counts: before=$initial_runs after=$current_runs"

  if [[ "$current_runs" -le "$initial_runs" ]]; then
    log "No post-failover executions observed within timeout window"
    return 1
  fi

  return 0
}

main() {
  log "pg_cron distributed concurrency test"
  log "Configuration: PD_ENDPOINTS=$PD_ENDPOINTS host=$PG_HOST portA=$PG_PORT_A portB=$PG_PORT_B poll_ms=$CRON_POLL_MS orphan_timeout_sec=$CRON_ORPHAN_TIMEOUT_SEC"

  start_instances

  run_test "extension-not-installed error path" test_extension_not_installed_error
  run_test "multi-instance concurrency: single execution per tick" test_concurrency_single_execution_per_tick
  run_test "edge-case orphan/failover recovery smoke" test_orphan_recovery_failover_smoke

  log "=== Test Summary ==="
  log "Total: $TOTAL_TESTS"
  log "Failed: $FAILED_TESTS"
  log "Passed: $((TOTAL_TESTS - FAILED_TESTS))"

  if [[ "$FAILED_TESTS" -ne 0 ]]; then
    log "Overall result: FAIL"
    return 1
  fi

  log "Overall result: PASS"
  return 0
}

main "$@"
