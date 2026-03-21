#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

MANIFEST_PATH="${MANIFEST_PATH:-$SCRIPT_DIR/regression_gate.list}"

CLUSTER_NAME="${CLUSTER_NAME:-regression-$(date +%s)}"

PG_HOST="${PG_HOST:-127.0.0.1}"
PG_PORT_IS_EXPLICIT=0
if [[ -n "${PG_PORT+x}" ]]; then
  PG_PORT_IS_EXPLICIT=1
fi
PG_PORT="${PG_PORT:-15433}"
PG_USER="${PG_USER:-admin}"
PG_PASSWORD="${PG_PASSWORD:-admin}"
WORKER_SYSTEM_KEYSPACE="${DB9_WORKER_SYSTEM_KEYSPACE:-_sys_worker}"

DB9_PID=""
CLUSTER_STARTED=0
ORM_DATABASE=""
ORM_DSN=""

RUN_UNIT=1
START_ENV=1
VERBOSE=0
STOP_ON_ERROR=0
SKIP_ORM=0
SKIP_BUILD=0

usage() {
  cat <<'EOF'
Usage:
  scripts/regression_gate.sh [options]

Runs the fast regression gate (SQL regressions + optional ORM subset + unit tests).

By default it will:
  1) Start a fresh TiKV cluster (via scripts/tikv_admin.py)
  2) Build db9-server (release)
  3) Start db9-server
  4) Run the regression SQL pack (SSOT: scripts/regression_gate.list)
  5) (Optional) Run the regression ORM pack (SSOT: scripts/regression_gate.list)
  6) Stop & clean the cluster

Options:
  --dsn <dsn>         Use an existing running db9-server instance (skip cluster/server start)
  --no-env            Skip starting TiKV/db9-server (alias of providing --dsn)
  --manifest <path>   Override regression manifest path (default: scripts/regression_gate.list)
  --skip-orm          Skip ORM regression pack
  --skip-unit         Skip `cargo test`
  --skip-build        Skip `cargo build --release` (expects pre-built binary at target/release/db9-server)
  -v, --verbose       Pass `--verbose` to integration_test.py
  -x, --stop-on-error Pass `--stop-on-error` to integration_test.py
  -h, --help          Show help

Environment:
  PG_HOST/PG_PORT/PG_USER/PG_PASSWORD control the default DSN when --dsn is not provided.
  CLUSTER_NAME controls the TiKV cluster name when auto-starting.
  MANIFEST_PATH overrides the manifest path (same as --manifest).
EOF
}

trim_manifest_line() {
  local s="$1"
  s="${s%%#*}"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s\n' "$s"
}

build_dsn_with_database() {
  local dsn="$1"
  local database="$2"
  python3 - "$dsn" "$database" <<'PY'
import sys
from urllib.parse import quote, urlsplit, urlunsplit

dsn = sys.argv[1]
database = sys.argv[2]

parts = urlsplit(dsn)
if not parts.scheme or not parts.netloc:
    raise SystemExit(1)

new_path = "/" + quote(database, safe="")
print(urlunsplit((parts.scheme, parts.netloc, new_path, parts.query, parts.fragment)))
PY
}

dsn_database_name() {
  local dsn="$1"
  python3 - "$dsn" <<'PY'
import sys
from urllib.parse import unquote, urlsplit

dsn = sys.argv[1]
parts = urlsplit(dsn)
path = parts.path or ""
if path.startswith("/"):
    path = path[1:]
print(unquote(path))
PY
}

parse_manifest() {
  local manifest="$1"
  local section=""

  SQL_TESTS=()
  ORM_TESTS=()
  PYTHON_TESTS=()

  while IFS= read -r raw_line || [[ -n "$raw_line" ]]; do
    local line
    line="$(trim_manifest_line "$raw_line")"
    [[ -z "$line" ]] && continue

    if [[ "$line" =~ ^\[([a-z]+)\]$ ]]; then
      section="${BASH_REMATCH[1]}"
      continue
    fi

    case "$section" in
      sql) SQL_TESTS+=("$line") ;;
      orm) ORM_TESTS+=("$line") ;;
      python) PYTHON_TESTS+=("$line") ;;
      *) echo "ERROR: invalid manifest entry (outside [sql]/[orm]/[python]): $line" >&2; return 2 ;;
    esac
  done <"$manifest"

  if [[ "${#SQL_TESTS[@]}" -eq 0 ]]; then
    echo "ERROR: manifest has no [sql] entries: $manifest" >&2
    return 2
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dsn)
      PG_DSN="${2:-}"
      START_ENV=0
      shift 2
      ;;
    --no-env)
      START_ENV=0
      shift
      ;;
    --manifest)
      MANIFEST_PATH="${2:-}"
      shift 2
      ;;
    --skip-orm)
      SKIP_ORM=1
      shift
      ;;
    --skip-unit)
      RUN_UNIT=0
      shift
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    -v|--verbose)
      VERBOSE=1
      shift
      ;;
    -x|--stop-on-error)
      STOP_ON_ERROR=1
      shift
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

SQL_TESTS=()
ORM_TESTS=()
PYTHON_TESTS=()
if [[ -f "$MANIFEST_PATH" ]]; then
  parse_manifest "$MANIFEST_PATH"
else
  echo "WARN: manifest not found at '$MANIFEST_PATH'; falling back to built-in SQL list." >&2
  SQL_TESTS=(
    tests/01_ddl_basic.sql
    tests/24_json_comprehensive.sql
    tests/74_filter_clause.sql
    tests/86_async_triggers.sql
    tests/107_join_using_natural_full_right_issue86.sql
    tests/108_chained_natural_using_join_wildcard.sql
    tests/116_join_using_outer_unqualified_refs_issue86.sql
    tests/117_drop_function_trigger_deps_issue222.sql
    tests/119_upsert_do_update_update_semantics_issue196.sql
    tests/125_pr285_compat_regressions.sql
    tests/126_timestamptz_offset_input_issue270.sql
  )
fi

REGRESSION_TESTS=("${SQL_TESTS[@]}")

for test_file in "${REGRESSION_TESTS[@]}"; do
  if [[ ! -f "$ROOT_DIR/$test_file" ]]; then
    echo "ERROR: missing regression test file: $test_file" >&2
    echo "Tip: make sure the regression guards are backported to your branch before release." >&2
    exit 2
  fi
done

for test_file in "${PYTHON_TESTS[@]}"; do
  if [[ ! -f "$ROOT_DIR/$test_file" ]]; then
    echo "ERROR: missing Python test file: $test_file" >&2
    exit 2
  fi
done

is_port_free() {
  python3 - "$1" "$2" <<'PY'
import socket
import sys

host = sys.argv[1]
port = int(sys.argv[2])

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
try:
    sock.bind((host, port))
except OSError:
    sys.exit(1)
finally:
    sock.close()
sys.exit(0)
PY
}

pick_free_port() {
  python3 - "$PG_HOST" <<'PY'
import socket
import sys

host = sys.argv[1]
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.bind((host, 0))
print(sock.getsockname()[1])
sock.close()
PY
}

worker_is_enabled() {
  local raw="${DB9_WORKER_ENABLED:-1}"
  local raw_lc
  raw_lc="$(printf '%s' "$raw" | tr '[:upper:]' '[:lower:]')"
  case "$raw_lc" in
    1|true|t|yes|y|on) return 0 ;;
    0|false|f|no|n|off) return 1 ;;
    *) return 0 ;;
  esac
}

ensure_pd_keyspace() {
  local pd_endpoints="$1"
  local keyspace="$2"
  local pd_primary="${pd_endpoints%%,*}"

  python3 - "$pd_primary" "$keyspace" <<'PY'
import json
import sys
import time
import urllib.error
import urllib.request

pd = sys.argv[1]
keyspace = sys.argv[2]
base = f"http://{pd}/pd/api/v2/keyspaces"
headers = {"Content-Type": "application/json"}
create_data = json.dumps({"name": keyspace}).encode("utf-8")


def request(method: str, url: str, data=None):
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    with urllib.request.urlopen(req, timeout=5) as resp:
        body = resp.read().decode("utf-8", "replace")
        return resp.status, body


last_err = "unknown error"
for _ in range(15):
    try:
        status, body = request("POST", base, create_data)
        if status not in (200, 201, 409):
            last_err = f"POST status={status}, body={body}"
            time.sleep(1)
            continue
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        if e.code == 409 or "already exists" in body.lower():
            pass
        elif e.code in (500, 503):
            last_err = f"POST status={e.code}, body={body}"
            time.sleep(1)
            continue
        else:
            print(
                f"ERROR: failed to create keyspace '{keyspace}' on PD {pd}: "
                f"status={e.code}, body={body}",
                file=sys.stderr,
            )
            sys.exit(1)
    except Exception as e:
        last_err = f"POST error: {e}"
        time.sleep(1)
        continue

    try:
        status, body = request("GET", f"{base}/{keyspace}")
        if status == 200:
            print(f"Ensured system keyspace '{keyspace}' on PD {pd}")
            sys.exit(0)
        last_err = f"GET status={status}, body={body}"
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        last_err = f"GET status={e.code}, body={body}"
    except Exception as e:
        last_err = f"GET error: {e}"

    time.sleep(1)

print(
    f"ERROR: unable to ensure keyspace '{keyspace}' on PD {pd} after retries: {last_err}",
    file=sys.stderr,
)
sys.exit(1)
PY
}

verify_worker_startup() {
  local log_file="/tmp/db9-regression.log"

  for _ in $(seq 1 20); do
    if grep -Fq "WorkerEngine and GC started" "$log_file" 2>/dev/null \
      || grep -Fq "WorkerEngine started (cron/triggers/HNSW/DDL)" "$log_file" 2>/dev/null; then
      echo "Worker startup verified."
      return 0
    fi

    if grep -Fq "Failed to initialize system store:" "$log_file" 2>/dev/null; then
      echo "ERROR: worker enabled but system-store initialization failed." >&2
      sed -n '1,240p' "$log_file" || true
      return 1
    fi

    sleep 1
  done

  echo "ERROR: worker enabled but startup success marker was not observed in time." >&2
  sed -n '1,240p' "$log_file" || true
  return 1
}

manifest_requires_live_worker() {
  local test_file
  for test_file in "${REGRESSION_TESTS[@]}"; do
    case "$test_file" in
      tests/187_worker_cic.sql|tests/188_worker_refresh_mv.sql|tests/186_worker_bg_sql.sql)
        return 0
        ;;
    esac
  done
  return 1
}

verify_worker_runtime_with_dsn() {
  local dsn="$1"

  local task_id
  task_id="$(psql "$dsn" -Atqc "SELECT pg_background_launch('SELECT 1')" 2>/dev/null || true)"
  if [[ -z "$task_id" || ! "$task_id" =~ ^[0-9]+$ ]]; then
    echo "ERROR: failed to launch worker runtime probe via pg_background_launch()." >&2
    echo "       External DSN is reachable, but worker functions are not operational." >&2
    return 1
  fi

  local result=""
  local i
  for i in $(seq 1 30); do
    result="$(psql "$dsn" -Atqc "SELECT pg_background_result($task_id)" 2>/dev/null || true)"
    if [[ "$result" == "OK" || "$result" == ERROR:* ]]; then
      echo "Worker runtime probe verified (task_id=$task_id, result=$result)."
      return 0
    fi
    sleep 0.2
  done

  echo "ERROR: worker runtime probe did not complete in time (task_id=$task_id, last_result='${result:-<empty>}')." >&2
  echo "       Background worker appears unavailable; worker SQL cases would be unreliable." >&2
  return 1
}

cleanup() {
  echo ""
  echo "=== Regression gate cleanup ==="

  if [[ -n "$ORM_DATABASE" && -n "${PG_DSN:-}" ]]; then
    psql "$PG_DSN" -v ON_ERROR_STOP=0 \
      -c "DROP DATABASE IF EXISTS \"$ORM_DATABASE\"" >/dev/null 2>&1 || true
  fi

  if [[ -n "$DB9_PID" ]] && kill -0 "$DB9_PID" 2>/dev/null; then
    echo "Stopping db9-server (PID: $DB9_PID)..."
    kill "$DB9_PID" 2>/dev/null || true
    wait "$DB9_PID" 2>/dev/null || true
  fi

  if [[ "$CLUSTER_STARTED" -eq 1 ]]; then
    echo "Stopping TiKV cluster '$CLUSTER_NAME'..."
    python3 "$ROOT_DIR/scripts/tikv_admin.py" stop --name "$CLUSTER_NAME" 2>/dev/null || true
    python3 "$ROOT_DIR/scripts/tikv_admin.py" clean --name "$CLUSTER_NAME" 2>/dev/null || true
  fi

  echo "Cleanup complete"
}
trap cleanup EXIT

REPORT_TS="$(date +%Y%m%d-%H%M%S)"
REPORT_DIR="$ROOT_DIR/test-reports/regression-gate-$REPORT_TS"
mkdir -p "$REPORT_DIR"
UNIT_LOG="$REPORT_DIR/unit.log"
SQL_LOG="$REPORT_DIR/sql.log"
PYTHON_LOG="$REPORT_DIR/python.log"
ORM_LOG="$REPORT_DIR/orm.log"

if [[ "$START_ENV" -eq 1 ]]; then
  if ! is_port_free "$PG_HOST" "$PG_PORT"; then
    if [[ "$PG_PORT_IS_EXPLICIT" -eq 1 ]]; then
      echo "ERROR: PG_PORT ${PG_PORT} is already in use on ${PG_HOST}. Please pick another port." >&2
      exit 1
    fi
    NEW_PORT="$(pick_free_port)"
    echo "WARN: PG_PORT ${PG_PORT} is already in use on ${PG_HOST}; using free port ${NEW_PORT}"
    PG_PORT="$NEW_PORT"
  fi
fi

if [[ -z "${PG_DSN:-}" ]]; then
  PG_DSN="postgres://${PG_USER}:${PG_PASSWORD}@${PG_HOST}:${PG_PORT}/postgres"
fi

REGRESSION_DSN="$PG_DSN"
DSN_DB_NAME="$(dsn_database_name "$PG_DSN" || true)"
if [[ -n "$DSN_DB_NAME" && "$DSN_DB_NAME" != "postgres" ]]; then
  REGRESSION_DSN="$(build_dsn_with_database "$PG_DSN" "postgres")"
fi

echo "=== db9-server Regression Gate ==="
echo "PG_DSN: $PG_DSN"
if [[ "$REGRESSION_DSN" != "$PG_DSN" ]]; then
  echo "Regression SQL DSN: $REGRESSION_DSN"
fi
echo "Manifest: $MANIFEST_PATH"
echo "Report dir: $REPORT_DIR"
echo ""

TOTAL_STEPS=5
if [[ "${#PYTHON_TESTS[@]}" -gt 0 ]]; then
  TOTAL_STEPS=$((TOTAL_STEPS + 1))
fi
if [[ "$SKIP_ORM" -eq 0 && "${#ORM_TESTS[@]}" -gt 0 ]]; then
  TOTAL_STEPS=$((TOTAL_STEPS + 1))
fi

if [[ "$RUN_UNIT" -eq 1 ]]; then
  echo "[1/$TOTAL_STEPS] Running unit tests..."
  (cd "$ROOT_DIR" && cargo test) 2>&1 | tee "$UNIT_LOG"
  echo ""
else
  echo "[1/$TOTAL_STEPS] Skipping unit tests (--skip-unit)"
  echo ""
fi

if [[ "$START_ENV" -eq 1 ]]; then
  echo "[2/$TOTAL_STEPS] Starting TiKV cluster '$CLUSTER_NAME'..."
  TIKV_OUTPUT="$(python3 "$ROOT_DIR/scripts/tikv_admin.py" start --name "$CLUSTER_NAME" 2>&1)"
  echo "$TIKV_OUTPUT"

  PD_ENDPOINTS="$(echo "$TIKV_OUTPUT" | grep -oE 'PD_ENDPOINTS=[^[:space:]]+' | tail -n 1 | cut -d= -f2 || true)"
  if [[ -z "$PD_ENDPOINTS" ]]; then
    echo "ERROR: Failed to parse PD_ENDPOINTS from tikv_admin.py output" >&2
    exit 1
  fi

  CLUSTER_STARTED=1
  echo "Waiting for TiKV to be fully ready..."
  sleep 5

  if worker_is_enabled; then
    echo "Ensuring worker system keyspace '${WORKER_SYSTEM_KEYSPACE}'..."
    ensure_pd_keyspace "$PD_ENDPOINTS" "$WORKER_SYSTEM_KEYSPACE"
  else
    echo "Worker disabled (DB9_WORKER_ENABLED='${DB9_WORKER_ENABLED:-unset}'); skipping system keyspace provisioning."
  fi

  echo ""
  if [[ "${SKIP_BUILD:-0}" -eq 1 ]]; then
    if [[ -x "$ROOT_DIR/target/release/db9-server" ]]; then
      echo "[3/$TOTAL_STEPS] Using pre-built db9-server binary"
    else
      echo "ERROR: --skip-build specified but no binary at target/release/db9-server" >&2
      exit 1
    fi
  else
    echo "[3/$TOTAL_STEPS] Building db9-server..."
    (cd "$ROOT_DIR" && cargo build --release --quiet)
  fi
  echo ""

	  echo "Starting db9-server on ${PG_HOST}:${PG_PORT} (PD_ENDPOINTS=${PD_ENDPOINTS})..."
	  pushd "$ROOT_DIR" >/dev/null
	  PD_ENDPOINTS="$PD_ENDPOINTS" \
	  PG_PORT="$PG_PORT" \
	  DB9_BOOTSTRAP_ADMIN_USER="$PG_USER" \
	  DB9_BOOTSTRAP_ADMIN_PASSWORD="$PG_PASSWORD" \
	  DB9_INSECURE=1 \
	  ./target/release/db9-server > /tmp/db9-regression.log 2>&1 &
	  DB9_PID=$!
	  popd >/dev/null

  # Wait for readiness
  if command -v pg_isready >/dev/null 2>&1; then
    for i in $(seq 1 30); do
      if pg_isready -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -q 2>/dev/null; then
        break
      fi
      if ! kill -0 "$DB9_PID" 2>/dev/null; then
        echo "ERROR: db9-server exited during startup" >&2
        sed -n '1,200p' /tmp/db9-regression.log || true
        exit 1
      fi
      sleep 1
    done
  fi

  # Final check via psql
  if ! PGPASSWORD="$PG_PASSWORD" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d postgres -c "SELECT 1" >/dev/null 2>&1; then
    echo "ERROR: db9-server not ready" >&2
    sed -n '1,200p' /tmp/db9-regression.log || true
    exit 1
  fi

  echo "db9-server ready (PID: $DB9_PID)"

  if worker_is_enabled; then
    verify_worker_startup
  fi

  echo ""
else
  echo "[2/$TOTAL_STEPS] Skipping TiKV/db9-server startup (--no-env/--dsn)"
  if manifest_requires_live_worker; then
    echo "Verifying worker runtime on external DSN..."
    verify_worker_runtime_with_dsn "$PG_DSN"
  fi
  echo ""
fi

# Extended protocol smoke test — catches do_describe_statement regressions early
SMOKE_LOG="$REPORT_DIR/extended-smoke.log"
echo "[4/$TOTAL_STEPS] Running extended protocol smoke test..."
SMOKE_EXIT=0
set +e
python3 "$SCRIPT_DIR/extended_protocol_smoke.py" --dsn "$PG_DSN" 2>&1 | tee "$SMOKE_LOG"
SMOKE_EXIT=${PIPESTATUS[0]}
set -e
echo ""
if [[ "$SMOKE_EXIT" -ne 0 ]]; then
  echo "❌ Extended protocol smoke FAILED (exit=$SMOKE_EXIT). Server is fundamentally broken — skipping remaining tests."
  echo "   See: $SMOKE_LOG"
  exit 1
fi

echo "[5/$TOTAL_STEPS] Running regression SQL cases (${#REGRESSION_TESTS[@]} files)..."
args=(--dsn "$REGRESSION_DSN")
if [[ "$VERBOSE" -eq 1 ]]; then
  args+=(--verbose)
fi
if [[ "$STOP_ON_ERROR" -eq 1 ]]; then
  args+=(--stop-on-error)
fi

SQL_EXIT=0
set +e
(cd "$ROOT_DIR" && python3 scripts/integration_test.py "${args[@]}" "${REGRESSION_TESTS[@]}") 2>&1 | tee "$SQL_LOG"
SQL_EXIT=${PIPESTATUS[0]}
set -e

echo ""
if [[ "$SQL_EXIT" -ne 0 ]]; then
  echo "❌ Regression gate failed (SQL exit=$SQL_EXIT). See: $SQL_LOG"
  exit 1
fi

NEXT_STEP=6

if [[ "${#PYTHON_TESTS[@]}" -gt 0 ]]; then
  echo "[$NEXT_STEP/$TOTAL_STEPS] Running Python multi-session tests (${#PYTHON_TESTS[@]} files)..."
  PYTHON_EXIT=0
  for py_test in "${PYTHON_TESTS[@]}"; do
    echo "  -> $py_test"
    set +e
    python3 "$ROOT_DIR/$py_test" --dsn "$REGRESSION_DSN" 2>&1 | tee -a "$PYTHON_LOG"
    PY_RC=${PIPESTATUS[0]}
    set -e
    if [[ "$PY_RC" -ne 0 ]]; then
      PYTHON_EXIT=1
      if [[ "$STOP_ON_ERROR" -eq 1 ]]; then
        break
      fi
    fi
  done
  echo ""
  if [[ "$PYTHON_EXIT" -ne 0 ]]; then
    echo "❌ Regression gate failed (Python exit=$PYTHON_EXIT). See: $PYTHON_LOG"
    exit 1
  fi
  NEXT_STEP=$((NEXT_STEP + 1))
fi

if [[ "$SKIP_ORM" -eq 1 || "${#ORM_TESTS[@]}" -eq 0 ]]; then
  echo "✅ Regression gate passed"
  exit 0
fi

if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
  echo "ERROR: node/npm are required for the ORM regression pack (or pass --skip-orm)." >&2
  exit 2
fi

echo "[$NEXT_STEP/$TOTAL_STEPS] Running ORM regression pack (${#ORM_TESTS[@]} item(s))..."
ORM_FILTERS=()
for item in "${ORM_TESTS[@]}"; do
  if [[ "$item" == orm-tests/* ]]; then
    ORM_FILTERS+=("${item#orm-tests/}")
  else
    ORM_FILTERS+=("$item")
  fi
done

ORM_DATABASE="orm_regression_${REPORT_TS//[-]/_}"
ORM_DSN="$(build_dsn_with_database "$PG_DSN" "$ORM_DATABASE")"
if [[ -z "$ORM_DSN" ]]; then
  echo "ERROR: failed to derive ORM DSN from PG_DSN: $PG_DSN" >&2
  exit 2
fi

if ! psql "$PG_DSN" -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS \"$ORM_DATABASE\"" \
  -c "CREATE DATABASE \"$ORM_DATABASE\"" >/dev/null; then
  echo "ERROR: failed to create isolated ORM database '$ORM_DATABASE'." >&2
  echo "       Ensure the PG_DSN user has CREATEDB privilege." >&2
  exit 2
fi

ORM_EXIT=0
set +e
(
  cd "$ROOT_DIR/orm-tests"
  if [[ ! -d node_modules ]]; then
    npm ci
  fi
  PG_DSN="$ORM_DSN" npm test -- "${ORM_FILTERS[@]}"
) 2>&1 | tee "$ORM_LOG"
ORM_EXIT=${PIPESTATUS[0]}
set -e

echo ""
if [[ "$ORM_EXIT" -ne 0 ]]; then
  echo "❌ Regression gate failed (ORM exit=$ORM_EXIT). See: $ORM_LOG"
  exit 1
fi

echo "✅ Regression gate passed"
