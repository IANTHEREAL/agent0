#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLUSTER_NAME="test-$(date +%s)"
PG_PORT=${PG_PORT:-15433}
PG_USER=${PG_USER:-admin}
PG_PASSWORD=${PG_PASSWORD:-admin}

PGTIKV_PID=""

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    if [ -n "$PGTIKV_PID" ] && kill -0 "$PGTIKV_PID" 2>/dev/null; then
        echo "Stopping pg-tikv (PID: $PGTIKV_PID)..."
        kill "$PGTIKV_PID" 2>/dev/null || true
        wait "$PGTIKV_PID" 2>/dev/null || true
    fi
    echo "Stopping TiKV cluster '$CLUSTER_NAME'..."
    uv run "$SCRIPT_DIR/scripts/tikv_admin.py" stop --name "$CLUSTER_NAME" 2>/dev/null || true
    uv run "$SCRIPT_DIR/scripts/tikv_admin.py" clean --name "$CLUSTER_NAME" 2>/dev/null || true
    echo "Cleanup complete"
}
trap cleanup EXIT

echo "=== pg-tikv Full Test Suite ==="
echo ""

echo "[1/5] Starting TiKV cluster '$CLUSTER_NAME'..."
TIKV_OUTPUT=$(uv run "$SCRIPT_DIR/scripts/tikv_admin.py" start --name "$CLUSTER_NAME" 2>&1)
echo "$TIKV_OUTPUT"

PD_PORT=$(echo "$TIKV_OUTPUT" | grep -oP 'PD_ENDPOINTS=127\.0\.0\.1:\K\d+')
if [ -z "$PD_PORT" ]; then
    echo "ERROR: Failed to get PD port"
    exit 1
fi
echo "PD endpoint: 127.0.0.1:$PD_PORT"
echo "Waiting for TiKV to be fully ready..."
sleep 5
echo ""

echo "[2/5] Building pg-tikv..."
cargo build --release --quiet
echo ""

echo "[3/5] Starting pg-tikv on port $PG_PORT..."
PD_ENDPOINTS="127.0.0.1:$PD_PORT" PG_PORT="$PG_PORT" "$SCRIPT_DIR/target/release/pg-tikv" > /tmp/pgtikv-test.log 2>&1 &
PGTIKV_PID=$!

for i in $(seq 1 30); do
    if pg_isready -h 127.0.0.1 -p "$PG_PORT" -U "$PG_USER" -q 2>/dev/null; then
        break
    fi
    if ! kill -0 "$PGTIKV_PID" 2>/dev/null; then
        echo "ERROR: pg-tikv failed to start"
        cat /tmp/pgtikv-test.log
        exit 1
    fi
    sleep 1
done

if ! pg_isready -h 127.0.0.1 -p "$PG_PORT" -U "$PG_USER" -q 2>/dev/null; then
    echo "ERROR: pg-tikv not ready after 30s"
    cat /tmp/pgtikv-test.log
    exit 1
fi

PG_DSN="postgres://$PG_USER:$PG_PASSWORD@127.0.0.1:$PG_PORT/postgres"
echo "pg-tikv ready (PID: $PGTIKV_PID)"
echo "PG_DSN: $PG_DSN"
echo ""

echo "[4/5] Running integration tests..."
uv run "$SCRIPT_DIR/scripts/integration_test.py" --dsn "$PG_DSN" "$@"
INTEGRATION_EXIT=$?
echo ""

echo "[5/5] Running ORM tests..."
cd "$SCRIPT_DIR/orm-tests"
if [ ! -d "node_modules" ]; then
    echo "Installing dependencies..."
    npm install --silent
fi
if [ ! -d "node_modules/.prisma" ]; then
    echo "Generating Prisma client..."
    npx prisma generate --silent
fi
PG_DSN="$PG_DSN" npm test
ORM_EXIT=$?
cd "$SCRIPT_DIR"
echo ""

echo "=== Test Summary ==="
echo "Integration tests: $([ $INTEGRATION_EXIT -eq 0 ] && echo 'PASSED' || echo 'FAILED')"
echo "ORM tests: $([ $ORM_EXIT -eq 0 ] && echo 'PASSED' || echo 'FAILED')"

if [ $INTEGRATION_EXIT -ne 0 ] || [ $ORM_EXIT -ne 0 ]; then
    exit 1
fi
