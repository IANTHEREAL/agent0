#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

timestamp() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log() {
  printf '[%s] %s\n' "$(timestamp)" "$*"
}

die() {
  printf '[%s] ERROR: %s\n' "$(timestamp)" "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

CURRENT_USER="${USER:-$(id -un)}"

MODE="both"
START_STACK=1
RUN_PREPARE=1
RUN_EXECUTE=1
RUN_CORRECTNESS_CHECK=1
KEEP_STACK=0
TPCH_SF=10
TPCC_WAREHOUSES=""
TPCH_THREADS=1
TPCC_THREADS=32
TPCH_TIME="30m"
TPCC_TIME="30m"
TPCC_DB9_DB="tpcc10g_db9"
TPCC_PG_DB="tpcc10g_pg18"
TPCH_DB9_DB="tpch10g_db9"
TPCH_PG_DB="tpch10g_pg18"

DB9_BIN_DEFAULT="/Users/chenhuansheng/Documents/GitHub/worktrees/db9-server-pr2402/target/release/db9-server"
CSE_TIKV_BIN_DEFAULT="/Users/chenhuansheng/Documents/GitHub/worktrees/cloud-storage-engine-pr4921/target/release/tikv-server"

DB9_BIN="${DB9_BIN:-$DB9_BIN_DEFAULT}"
CSE_TIKV_BIN="${CSE_TIKV_BIN:-$CSE_TIKV_BIN_DEFAULT}"

PG18_HOST="${PG18_HOST:-127.0.0.1}"
PG18_PORT="${PG18_PORT:-5432}"
PG18_USER="${PG18_USER:-$CURRENT_USER}"
PG18_PASSWORD="${PG18_PASSWORD:-}"

DB9_HOST="${DB9_HOST:-127.0.0.1}"
DB9_PORT="${DB9_PORT:-5433}"
DB9_USER="${DB9_USER:-admin}"
DB9_PASSWORD="${DB9_PASSWORD:-admin}"
DB9_METRICS_PORT="${DB9_METRICS_PORT:-9090}"
DB9_REDIS_URL="${DB9_REDIS_URL:-redis://127.0.0.1:6379/0}"
DB9_TENANT_MEMORY_QUOTA_BYTES="${DB9_TENANT_MEMORY_QUOTA_BYTES:-0}"
DB9_STATEMENT_TIMEOUT_MS="${DB9_STATEMENT_TIMEOUT_MS:-0}"
DB9_STATEMENT_TIMEOUT_HARD_CAP_MS="${DB9_STATEMENT_TIMEOUT_HARD_CAP_MS:-0}"

TIUP_TAG="${TIUP_TAG:-db9-local-tpc-compare}"
PORT_OFFSET="${PORT_OFFSET:-21000}"
CSE_PD_PORT="${CSE_PD_PORT:-23379}"
CSE_KV_PORT="${CSE_KV_PORT:-41160}"
CSE_STATUS_PORT="${CSE_STATUS_PORT:-41180}"

REPORT_DIR_DEFAULT="$ROOT_DIR/benchmarks/side/local_tiup_tpc_compare/$(date -u +%Y%m%dT%H%M%SZ)"
REPORT_DIR="${REPORT_DIR:-$REPORT_DIR_DEFAULT}"

usage() {
  cat <<'EOF'
Usage:
  scripts/run_local_tiup_tpc_compare.sh [options]

Runs local TPC-C / TPC-H comparison tests against:
  - db9-server PR #2402
  - cloud-storage-engine PR #4921
  - local PostgreSQL 18.3

Options:
  --mode {tpcc|tpch|both}        Benchmark family to run. Default: both
  --tpch-sf N                    TPC-H scale factor. Default: 10
  --tpcc-warehouses N            TPC-C warehouse count. Required when mode includes tpcc
  --tpch-threads N               TPC-H client threads. Default: 1
  --tpcc-threads N               TPC-C client threads. Default: 32
  --tpch-time DUR                TPC-H run duration. Default: 30m
  --tpcc-time DUR                TPC-C run duration. Default: 30m
  --report-dir PATH              Output directory for logs, raw results, and report
  --skip-stack-start             Reuse existing local db9/CSE/Redis stack
  --skip-prepare                 Skip data preparation and only execute workload
  --skip-run                     Prepare data but do not execute workload
  --skip-correctness-check       Skip TPC-C correctness validation after prepare/run
  --keep-stack                   Leave started db9/CSE/Redis processes running
  -h, --help                     Show this help

Environment overrides:
  DB9_BIN, CSE_TIKV_BIN
  DB9_HOST, DB9_PORT, DB9_USER, DB9_PASSWORD, DB9_METRICS_PORT, DB9_REDIS_URL
  DB9_TENANT_MEMORY_QUOTA_BYTES, DB9_STATEMENT_TIMEOUT_MS,
  DB9_STATEMENT_TIMEOUT_HARD_CAP_MS
  PG18_HOST, PG18_PORT, PG18_USER, PG18_PASSWORD
  TIUP_TAG, PORT_OFFSET, CSE_PD_PORT, CSE_KV_PORT, CSE_STATUS_PORT
  REPORT_DIR
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode) MODE="$2"; shift 2 ;;
    --tpch-sf) TPCH_SF="$2"; shift 2 ;;
    --tpcc-warehouses) TPCC_WAREHOUSES="$2"; shift 2 ;;
    --tpch-threads) TPCH_THREADS="$2"; shift 2 ;;
    --tpcc-threads) TPCC_THREADS="$2"; shift 2 ;;
    --tpch-time) TPCH_TIME="$2"; shift 2 ;;
    --tpcc-time) TPCC_TIME="$2"; shift 2 ;;
    --report-dir) REPORT_DIR="$2"; shift 2 ;;
    --skip-stack-start) START_STACK=0; shift ;;
    --skip-prepare) RUN_PREPARE=0; shift ;;
    --skip-run) RUN_EXECUTE=0; shift ;;
    --skip-correctness-check) RUN_CORRECTNESS_CHECK=0; shift ;;
    --keep-stack) KEEP_STACK=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

case "$MODE" in
  tpcc|tpch|both) ;;
  *) die "--mode must be one of: tpcc, tpch, both" ;;
esac

if [[ "$MODE" == "tpcc" || "$MODE" == "both" ]]; then
  [[ -n "$TPCC_WAREHOUSES" ]] || die "--tpcc-warehouses is required when mode includes tpcc"
fi

require_cmd tiup
require_cmd psql
require_cmd pg_isready
require_cmd curl
require_cmd python3

[[ -x "$DB9_BIN" ]] || die "db9 binary not found or not executable: $DB9_BIN"
[[ -x "$CSE_TIKV_BIN" ]] || die "cse tikv binary not found or not executable: $CSE_TIKV_BIN"

mkdir -p "$REPORT_DIR"
RAW_DIR="$REPORT_DIR/raw"
LOG_DIR="$REPORT_DIR/logs"
mkdir -p "$RAW_DIR" "$LOG_DIR"

DB9_PID=""
REDIS_PID=""
TIUP_SESSION_PID=""

cleanup() {
  set +e
  if [[ "$KEEP_STACK" -eq 0 ]]; then
    if [[ -n "$DB9_PID" ]] && kill -0 "$DB9_PID" >/dev/null 2>&1; then
      log "stopping db9-server pid=$DB9_PID"
      kill "$DB9_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$REDIS_PID" ]] && kill -0 "$REDIS_PID" >/dev/null 2>&1; then
      log "stopping local redislite pid=$REDIS_PID"
      kill "$REDIS_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$TIUP_SESSION_PID" ]] && kill -0 "$TIUP_SESSION_PID" >/dev/null 2>&1; then
      log "stopping tiup playground pid=$TIUP_SESSION_PID"
      kill -INT "$TIUP_SESSION_PID" >/dev/null 2>&1 || true
    fi
  fi
}
trap cleanup EXIT

wait_for_port() {
  local host="$1"
  local port="$2"
  local timeout_secs="${3:-60}"
  python3 - "$host" "$port" "$timeout_secs" <<'PY'
import socket, sys, time
host = sys.argv[1]
port = int(sys.argv[2])
timeout = int(sys.argv[3])
deadline = time.time() + timeout
while time.time() < deadline:
    s = socket.socket()
    s.settimeout(0.5)
    try:
        s.connect((host, port))
        print(f"ready {host}:{port}")
        raise SystemExit(0)
    except Exception:
        time.sleep(1)
    finally:
        s.close()
print(f"timeout waiting for {host}:{port}", file=sys.stderr)
raise SystemExit(1)
PY
}

wait_for_http_ok() {
  local url="$1"
  local timeout_secs="${2:-60}"
  python3 - "$url" "$timeout_secs" <<'PY'
import sys, time, urllib.request
url = sys.argv[1]
timeout = int(sys.argv[2])
deadline = time.time() + timeout
while time.time() < deadline:
    try:
        with urllib.request.urlopen(url, timeout=2) as resp:
            if 200 <= resp.status < 300:
                print(f"ready {url}")
                raise SystemExit(0)
    except Exception:
        time.sleep(1)
print(f"timeout waiting for {url}", file=sys.stderr)
raise SystemExit(1)
PY
}

psql_cmd() {
  local host="$1"
  local port="$2"
  local user="$3"
  local db="$4"
  local sql="$5"
  local password="${6:-}"
  if [[ -n "$password" ]]; then
    PGPASSWORD="$password" psql -h "$host" -p "$port" -U "$user" -d "$db" -v ON_ERROR_STOP=1 -Atqc "$sql"
  else
    psql -h "$host" -p "$port" -U "$user" -d "$db" -v ON_ERROR_STOP=1 -Atqc "$sql"
  fi
}

ensure_redis() {
  if python3 - "$DB9_REDIS_URL" <<'PY'
import socket, sys, urllib.parse
u = urllib.parse.urlparse(sys.argv[1])
host = u.hostname or "127.0.0.1"
port = u.port or 6379
s = socket.socket()
s.settimeout(0.5)
try:
    s.connect((host, port))
    print("redis reachable")
    raise SystemExit(0)
except Exception:
    raise SystemExit(1)
finally:
    s.close()
PY
  then
    log "redis already reachable at $DB9_REDIS_URL"
    return
  fi

  log "starting local redislite on 127.0.0.1:6379"
  python3 - <<'PY' >"$LOG_DIR/redislite.log" 2>&1 &
import redislite, signal, sys, time
r = redislite.Redis(serverconfig={'port':'6379', 'bind':'127.0.0.1', 'save':'', 'appendonly':'no'})
print('redislite_ready', r.ping(), flush=True)
signal.signal(signal.SIGTERM, lambda *args: sys.exit(0))
signal.signal(signal.SIGINT, lambda *args: sys.exit(0))
while True:
    time.sleep(60)
PY
  REDIS_PID="$!"
  wait_for_port 127.0.0.1 6379 60 >/dev/null
}

start_cse_stack() {
  local cfg="$REPORT_DIR/cse-v2.toml"
  cat >"$cfg" <<EOF
[storage]
api-version = 2
enable-ttl = true
reserve-raft-space = 0
reserve-space = 0
EOF

  log "starting tiup playground with PR #4921 tikv-server"
  tiup playground \
    --mode tikv-slim \
    --tag "$TIUP_TAG" \
    --host 127.0.0.1 \
    --without-monitor \
    --port-offset "$PORT_OFFSET" \
    --kv.config "$cfg" \
    --kv.binpath "$CSE_TIKV_BIN" \
    >"$LOG_DIR/tiup-playground.log" 2>&1 &
  TIUP_SESSION_PID="$!"

  wait_for_port 127.0.0.1 "$CSE_PD_PORT" 120 >/dev/null
  wait_for_port 127.0.0.1 "$CSE_KV_PORT" 120 >/dev/null
  wait_for_port 127.0.0.1 "$CSE_STATUS_PORT" 120 >/dev/null
  wait_for_http_ok "http://127.0.0.1:${CSE_STATUS_PORT}/metrics" 120 >/dev/null
}

start_db9() {
  log "starting db9-server PR #2402"
  REDIS_URL="$DB9_REDIS_URL" \
  DB9_DEV=1 \
  DB9_DEV_ADMIN_PASSWORD="$DB9_PASSWORD" \
  DB9_AUTO_ANALYZE_ENABLED=false \
  DB9_TENANT_MEMORY_QUOTA_BYTES="$DB9_TENANT_MEMORY_QUOTA_BYTES" \
  DB9_STATEMENT_TIMEOUT_MS="$DB9_STATEMENT_TIMEOUT_MS" \
  DB9_STATEMENT_TIMEOUT_HARD_CAP_MS="$DB9_STATEMENT_TIMEOUT_HARD_CAP_MS" \
  DB9_METRICS_PORT="$DB9_METRICS_PORT" \
  PD_ENDPOINTS="127.0.0.1:${CSE_PD_PORT}" \
  PG_PORT="$DB9_PORT" \
  "$DB9_BIN" >"$LOG_DIR/db9-server.log" 2>&1 &
  DB9_PID="$!"
  wait_for_port "$DB9_HOST" "$DB9_PORT" 120 >/dev/null
  wait_for_http_ok "http://${DB9_HOST}:${DB9_METRICS_PORT}/health" 120 >/dev/null
}

reset_db() {
  local host="$1"
  local port="$2"
  local user="$3"
  local db="$4"
  local password="${5:-}"
  local sql_drop="DROP DATABASE IF EXISTS ${db};"
  local sql_create="CREATE DATABASE ${db};"
  log "reset database ${db} on ${host}:${port}"
  psql_cmd "$host" "$port" "$user" postgres "$sql_drop" "$password"
  psql_cmd "$host" "$port" "$user" postgres "$sql_create" "$password"
}

run_db9_metrics_snapshot() {
  local output="$1"
  curl -sS --max-time 10 "http://${DB9_HOST}:${DB9_METRICS_PORT}/internal/metrics" >"$output"
}

run_tpch_prepare() {
  local host="$1" port="$2" user="$3" password="$4" db="$5"
  log "tpch prepare on ${db}@${host}:${port}"
  tiup bench tpch prepare \
    -d postgres \
    -H "$host" \
    -P "$port" \
    -U "$user" \
    ${password:+-p "$password"} \
    -D "$db" \
    --conn-params sslmode=disable \
    --sf "$TPCH_SF" \
    --dropdata \
    --analyze \
    >"$LOG_DIR/tpch_prepare_${db}.log" 2>&1
}

run_tpch_execute() {
  local label="$1" host="$2" port="$3" user="$4" password="$5" db="$6"
  log "tpch run on ${label}"
  tiup bench tpch run \
    -d postgres \
    -H "$host" \
    -P "$port" \
    -U "$user" \
    ${password:+-p "$password"} \
    -D "$db" \
    --conn-params sslmode=disable \
    --sf "$TPCH_SF" \
    --time "$TPCH_TIME" \
    --output json \
    >"$RAW_DIR/tpch_${label}.json" 2>"$LOG_DIR/tpch_${label}.log"
}

run_tpcc_prepare() {
  local host="$1" port="$2" user="$3" password="$4" db="$5"
  log "tpcc prepare on ${db}@${host}:${port}"
  tiup bench tpcc prepare \
    -d postgres \
    -H "$host" \
    -P "$port" \
    -U "$user" \
    ${password:+-p "$password"} \
    -D "$db" \
    --conn-params sslmode=disable \
    --warehouses "$TPCC_WAREHOUSES" \
    --dropdata \
    --no-check \
    >"$LOG_DIR/tpcc_prepare_${db}.log" 2>&1
}

run_tpcc_execute() {
  local label="$1" host="$2" port="$3" user="$4" password="$5" db="$6"
  log "tpcc run on ${label}"
  tiup bench tpcc run \
    -d postgres \
    -H "$host" \
    -P "$port" \
    -U "$user" \
    ${password:+-p "$password"} \
    -D "$db" \
    --conn-params sslmode=disable \
    --warehouses "$TPCC_WAREHOUSES" \
    -T "$TPCC_THREADS" \
    --time "$TPCC_TIME" \
    --output json \
    >"$RAW_DIR/tpcc_${label}.json" 2>"$LOG_DIR/tpcc_${label}.log"
}

run_tpcc_correctness_check() {
  local label="$1" host="$2" port="$3" user="$4" password="$5" db="$6" phase="$7"
  log "tpcc correctness check on ${label} (${phase})"
  python3 "$ROOT_DIR/scripts/tpcc_correctness_check.py" \
    --host "$host" \
    --port "$port" \
    --user "$user" \
    --password "$password" \
    --db "$db" \
    --label "$label" \
    --phase "$phase" \
    --output "$RAW_DIR/tpcc_${label}_${phase}_correctness.json" \
    >"$LOG_DIR/tpcc_${label}_${phase}_correctness.log"
}

write_report() {
  local report="$REPORT_DIR/report.md"
  cat >"$report" <<EOF
# Local TPC Compare Report

Generated: $(timestamp)

Scope:

- db9-server binary: \`$DB9_BIN\`
- cse tikv binary: \`$CSE_TIKV_BIN\`
- PostgreSQL 18.3: \`${PG18_HOST}:${PG18_PORT}\`
- db9 pgwire: \`${DB9_HOST}:${DB9_PORT}\`
- db9 metrics: \`http://${DB9_HOST}:${DB9_METRICS_PORT}/internal/metrics\`
- cse PD: \`127.0.0.1:${CSE_PD_PORT}\`
- cse TiKV: \`127.0.0.1:${CSE_KV_PORT}\`

db9 local runtime knobs:

- tenant memory quota bytes: \`$DB9_TENANT_MEMORY_QUOTA_BYTES\`
- statement timeout ms: \`$DB9_STATEMENT_TIMEOUT_MS\`
- statement timeout hard cap ms: \`$DB9_STATEMENT_TIMEOUT_HARD_CAP_MS\`

Requested benchmark sizes:

- TPC-H: scale factor \`$TPCH_SF\` (targeting the 10GB class)
- TPC-C: warehouses \`${TPCC_WAREHOUSES:-not-run}\`

Artifacts:

- raw JSON: \`$RAW_DIR\`
- logs: \`$LOG_DIR\`

Commands were executed with:

- driver: \`postgres\`
- conn params: \`sslmode=disable\`

Notes:

- This script uses \`tiup bench\` with PostgreSQL wire.
- For TPC-C, correctness JSON artifacts are emitted after \`prepare\` and after
  \`run\` unless \`--skip-correctness-check\` is used.
- For TPC-C, “10GB” is approximated by the chosen warehouse count and should be
  calibrated on PostgreSQL using \`pg_database_size()\`.
- This report is an execution wrapper and artifact index. Interpret throughput
  and latency from the raw \`tiup bench\` JSON outputs.
EOF
  log "wrote report to $report"
}

if [[ "$START_STACK" -eq 1 ]]; then
  ensure_redis
  start_cse_stack
  start_db9
fi

if [[ "$MODE" == "tpch" || "$MODE" == "both" ]]; then
  reset_db "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$TPCH_PG_DB" "$PG18_PASSWORD"
  reset_db "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$TPCH_DB9_DB" "$DB9_PASSWORD"
  if [[ "$RUN_PREPARE" -eq 1 ]]; then
    run_tpch_prepare "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$PG18_PASSWORD" "$TPCH_PG_DB"
    run_tpch_prepare "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$DB9_PASSWORD" "$TPCH_DB9_DB"
  fi
  if [[ "$RUN_EXECUTE" -eq 1 ]]; then
    run_db9_metrics_snapshot "$RAW_DIR/tpch_db9_before.prom"
    run_tpch_execute "pg18" "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$PG18_PASSWORD" "$TPCH_PG_DB"
    run_tpch_execute "db9" "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$DB9_PASSWORD" "$TPCH_DB9_DB"
    run_db9_metrics_snapshot "$RAW_DIR/tpch_db9_after.prom"
  fi
fi

if [[ "$MODE" == "tpcc" || "$MODE" == "both" ]]; then
  reset_db "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$TPCC_PG_DB" "$PG18_PASSWORD"
  reset_db "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$TPCC_DB9_DB" "$DB9_PASSWORD"
  if [[ "$RUN_PREPARE" -eq 1 ]]; then
    run_tpcc_prepare "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$PG18_PASSWORD" "$TPCC_PG_DB"
    run_tpcc_prepare "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$DB9_PASSWORD" "$TPCC_DB9_DB"
    if [[ "$RUN_CORRECTNESS_CHECK" -eq 1 ]]; then
      run_tpcc_correctness_check "pg18" "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$PG18_PASSWORD" "$TPCC_PG_DB" "after_prepare"
      run_tpcc_correctness_check "db9" "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$DB9_PASSWORD" "$TPCC_DB9_DB" "after_prepare"
    fi
  fi
  if [[ "$RUN_EXECUTE" -eq 1 ]]; then
    run_db9_metrics_snapshot "$RAW_DIR/tpcc_db9_before.prom"
    run_tpcc_execute "pg18" "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$PG18_PASSWORD" "$TPCC_PG_DB"
    run_tpcc_execute "db9" "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$DB9_PASSWORD" "$TPCC_DB9_DB"
    run_db9_metrics_snapshot "$RAW_DIR/tpcc_db9_after.prom"
    if [[ "$RUN_CORRECTNESS_CHECK" -eq 1 ]]; then
      run_tpcc_correctness_check "pg18" "$PG18_HOST" "$PG18_PORT" "$PG18_USER" "$PG18_PASSWORD" "$TPCC_PG_DB" "after_run"
      run_tpcc_correctness_check "db9" "$DB9_HOST" "$DB9_PORT" "$DB9_USER" "$DB9_PASSWORD" "$TPCC_DB9_DB" "after_run"
    fi
  fi
fi

write_report
log "done"
