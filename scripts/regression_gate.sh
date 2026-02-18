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
WORKER_SYSTEM_KEYSPACE="${PGTIKV_WORKER_SYSTEM_KEYSPACE:-_sys_worker}"

PGTIKV_PID=""
CLUSTER_STARTED=0

RUN_UNIT=1
START_ENV=1
VERBOSE=0
STOP_ON_ERROR=0
SKIP_ORM=0

usage() {
  cat <<'EOF'
Usage:
  scripts/regression_gate.sh [options]

Runs the fast regression gate (SQL regressions + optional ORM subset + unit tests).

By default it will:
  1) Start a fresh TiKV cluster (via scripts/tikv_admin.py)
  2) Build pg-tikv (release)
  3) Start pg-tikv
  4) Run the regression SQL pack (SSOT: scripts/regression_gate.list)
  5) (Optional) Run the regression ORM pack (SSOT: scripts/regression_gate.list)
  6) Stop & clean the cluster

Options:
  --dsn <dsn>         Use an existing running pg-tikv instance (skip cluster/server start)
  --no-env            Skip starting TiKV/pg-tikv (alias of providing --dsn)
  --manifest <path>   Override regression manifest path (default: scripts/regression_gate.list)
  --skip-orm          Skip ORM regression pack
  --skip-unit         Skip `cargo test`
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

parse_manifest() {
  local manifest="$1"
  local section=""

  SQL_TESTS=()
  ORM_TESTS=()

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
      *) echo "ERROR: invalid manifest entry (outside [sql]/[orm]): $line" >&2; return 2 ;;
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
  local raw="${PGTIKV_WORKER_ENABLED:-1}"
  case "${raw,,}" in
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

cleanup() {
  echo ""
  echo "=== Regression gate cleanup ==="

  if [[ -n "$PGTIKV_PID" ]] && kill -0 "$PGTIKV_PID" 2>/dev/null; then
    echo "Stopping pg-tikv (PID: $PGTIKV_PID)..."
    kill "$PGTIKV_PID" 2>/dev/null || true
    wait "$PGTIKV_PID" 2>/dev/null || true
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

echo "=== pg-tikv Regression Gate ==="
echo "PG_DSN: $PG_DSN"
echo "Manifest: $MANIFEST_PATH"
echo "Report dir: $REPORT_DIR"
echo ""

TOTAL_STEPS=4
if [[ "$SKIP_ORM" -eq 0 && "${#ORM_TESTS[@]}" -gt 0 ]]; then
  TOTAL_STEPS=5
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
    echo "Worker disabled (PGTIKV_WORKER_ENABLED='${PGTIKV_WORKER_ENABLED:-unset}'); skipping system keyspace provisioning."
  fi

  echo ""
  echo "[3/$TOTAL_STEPS] Building pg-tikv..."
  (cd "$ROOT_DIR" && cargo build --release --quiet)
  echo ""

	  echo "Starting pg-tikv on ${PG_HOST}:${PG_PORT} (PD_ENDPOINTS=${PD_ENDPOINTS})..."
	  pushd "$ROOT_DIR" >/dev/null
	  PD_ENDPOINTS="$PD_ENDPOINTS" \
	  PG_PORT="$PG_PORT" \
	  PGTIKV_BOOTSTRAP_ADMIN_USER="$PG_USER" \
	  PGTIKV_BOOTSTRAP_ADMIN_PASSWORD="$PG_PASSWORD" \
	  PGTIKV_INSECURE=1 \
	  ./target/release/pg-tikv > /tmp/pgtikv-regression.log 2>&1 &
	  PGTIKV_PID=$!
	  popd >/dev/null

  # Wait for readiness
  if command -v pg_isready >/dev/null 2>&1; then
    for i in $(seq 1 30); do
      if pg_isready -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -q 2>/dev/null; then
        break
      fi
      if ! kill -0 "$PGTIKV_PID" 2>/dev/null; then
        echo "ERROR: pg-tikv exited during startup" >&2
        sed -n '1,200p' /tmp/pgtikv-regression.log || true
        exit 1
      fi
      sleep 1
    done
  fi

  # Final check via psql
  if ! PGPASSWORD="$PG_PASSWORD" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d postgres -c "SELECT 1" >/dev/null 2>&1; then
    echo "ERROR: pg-tikv not ready" >&2
    sed -n '1,200p' /tmp/pgtikv-regression.log || true
    exit 1
  fi

  echo "pg-tikv ready (PID: $PGTIKV_PID)"
  echo ""
else
  echo "[2/$TOTAL_STEPS] Skipping TiKV/pg-tikv startup (--no-env/--dsn)"
  echo ""
fi

echo "[4/$TOTAL_STEPS] Running regression SQL cases (${#REGRESSION_TESTS[@]} files)..."
args=(--dsn "$PG_DSN")
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

if [[ "$SKIP_ORM" -eq 1 || "${#ORM_TESTS[@]}" -eq 0 ]]; then
  echo "✅ Regression gate passed (SQL-only)"
  exit 0
fi

if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
  echo "ERROR: node/npm are required for the ORM regression pack (or pass --skip-orm)." >&2
  exit 2
fi

echo "[5/$TOTAL_STEPS] Running ORM regression pack (${#ORM_TESTS[@]} item(s))..."
ORM_FILTERS=()
for item in "${ORM_TESTS[@]}"; do
  if [[ "$item" == orm-tests/* ]]; then
    ORM_FILTERS+=("${item#orm-tests/}")
  else
    ORM_FILTERS+=("$item")
  fi
done

ORM_EXIT=0
set +e
(
  cd "$ROOT_DIR/orm-tests"
  if [[ ! -d node_modules ]]; then
    npm ci
  fi
  PG_DSN="$PG_DSN" npm test -- "${ORM_FILTERS[@]}"
) 2>&1 | tee "$ORM_LOG"
ORM_EXIT=${PIPESTATUS[0]}
set -e

echo ""
if [[ "$ORM_EXIT" -ne 0 ]]; then
  echo "❌ Regression gate failed (ORM exit=$ORM_EXIT). See: $ORM_LOG"
  exit 1
fi

echo "✅ Regression gate passed"
