#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLUSTER_NAME="test-$(date +%s)"
PG_PORT=${PG_PORT:-15433}
PG_USER=${PG_USER:-admin}
PG_PASSWORD=${PG_PASSWORD:-admin}

# ORM selection — normalize env var to 0/1
_raw_prisma=${WITH_PRISMA:-0}
case "$_raw_prisma" in
    1|true|yes|on)  INCLUDE_PRISMA=1 ;;
    0|false|no|off) INCLUDE_PRISMA=0 ;;
    *) echo "ERROR: invalid WITH_PRISMA value '$_raw_prisma' (expected 1/true/yes/on or 0/false/no/off)"; exit 1 ;;
esac

# Extract script-only flags from args; pass the rest to integration_test.py.
INTEGRATION_ARGS=()
for arg in "$@"; do
    case "$arg" in
        --with-prisma)
            INCLUDE_PRISMA=1
            ;;
        --skip-prisma)
            INCLUDE_PRISMA=0
            ;;
        *)
            INTEGRATION_ARGS+=("$arg")
            ;;
    esac
done

# Report file
REPORT_DIR="$SCRIPT_DIR/test-reports"
REPORT_TIMESTAMP=$(date +%Y%m%d-%H%M%S)
GIT_SHA="$(git rev-parse HEAD 2>/dev/null || echo "unknown")"
GIT_SHA_SHORT="$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")"
REPORT_FILE="$REPORT_DIR/test-report-$REPORT_TIMESTAMP-$GIT_SHA_SHORT.md"
mkdir -p "$REPORT_DIR"

# Timing
START_TIME=$(date +%s)
PGTIKV_PID=""
ORM_DATABASE=""

# Test results
INTEGRATION_EXIT=0
ORM_EXIT=0
INTEGRATION_OUTPUT=""
ORM_OUTPUT=""

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    if [ -n "$ORM_DATABASE" ] && [ -n "$PGTIKV_PID" ] && kill -0 "$PGTIKV_PID" 2>/dev/null; then
        echo "Dropping isolated ORM database '$ORM_DATABASE'..."
        PGPASSWORD="$PG_PASSWORD" psql -X -q \
            -h 127.0.0.1 -p "$PG_PORT" -U "$PG_USER" -d postgres \
            -v ON_ERROR_STOP=1 \
            -c "DROP DATABASE IF EXISTS \"$ORM_DATABASE\"" 2>/dev/null || true
    fi
    if [ -n "$PGTIKV_PID" ] && kill -0 "$PGTIKV_PID" 2>/dev/null; then
        echo "Stopping pg-tikv (PID: $PGTIKV_PID)..."
        kill "$PGTIKV_PID" 2>/dev/null || true
        wait "$PGTIKV_PID" 2>/dev/null || true
    fi
    echo "Cleaning TiKV cluster '$CLUSTER_NAME'..."
    uv run "$SCRIPT_DIR/scripts/tikv_admin.py" clean --name "$CLUSTER_NAME" 2>/dev/null || true
    echo "Cleanup complete"
}
trap cleanup EXIT

# Helper to log to both stdout and report
log() {
    echo "$1"
    echo "$1" >> "$REPORT_FILE"
}

# Initialize report
cat > "$REPORT_FILE" << EOF
# pg-tikv Test Report

**Generated**: $(date '+%Y-%m-%d %H:%M:%S')
**Host**: $(hostname)
**Git**: ${GIT_SHA} (${GIT_SHA_SHORT})
**Rust**: $(rustc --version 2>/dev/null || echo "unknown")
**Node**: $(node --version 2>/dev/null || echo "unknown")

---

## Test Execution

EOF

echo "=== pg-tikv Full Test Suite ==="
echo ""

echo "[1/5] Starting TiKV cluster '$CLUSTER_NAME'..."
TIKV_OUTPUT=$(uv run "$SCRIPT_DIR/scripts/tikv_admin.py" start --name "$CLUSTER_NAME" 2>&1)
echo "$TIKV_OUTPUT"

PD_PORT=$(echo "$TIKV_OUTPUT" | grep -oP 'PD_ENDPOINTS=127\.0\.0\.1:\K\d+')
if [ -z "$PD_PORT" ]; then
    echo "ERROR: Failed to get PD port"
    echo "### Error: Failed to start TiKV cluster" >> "$REPORT_FILE"
    exit 1
fi
echo "PD endpoint: 127.0.0.1:$PD_PORT"
echo "Waiting for TiKV to be fully ready..."
sleep 5
echo ""

cat >> "$REPORT_FILE" << EOF
### Environment

| Component | Value |
|-----------|-------|
| TiKV Cluster | $CLUSTER_NAME |
| PD Endpoint | 127.0.0.1:$PD_PORT |
| pg-tikv Port | $PG_PORT |
| User | $PG_USER |
| ORM Database | isolated (auto-created per run) |
| Prisma | $([ "$INCLUDE_PRISMA" -eq 1 ] && echo "enabled" || echo "skipped") |

EOF

echo "[2/5] Building pg-tikv..."
BUILD_START=$(date +%s)
cargo build --release --quiet
BUILD_END=$(date +%s)
BUILD_TIME=$((BUILD_END - BUILD_START))
echo "Build completed in ${BUILD_TIME}s"
echo ""

echo "### Build" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"
echo "- Duration: ${BUILD_TIME}s" >> "$REPORT_FILE"
echo "- Mode: release" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"

echo "[3/5] Starting pg-tikv on port $PG_PORT..."
PD_ENDPOINTS="127.0.0.1:$PD_PORT" \
PG_PORT="$PG_PORT" \
PGTIKV_BOOTSTRAP_ADMIN_USER="$PG_USER" \
PGTIKV_BOOTSTRAP_ADMIN_PASSWORD="$PG_PASSWORD" \
PGTIKV_INSECURE=1 \
"$SCRIPT_DIR/target/release/pg-tikv" > /tmp/pgtikv-test.log 2>&1 &
PGTIKV_PID=$!

for i in $(seq 1 30); do
    if pg_isready -h 127.0.0.1 -p "$PG_PORT" -U "$PG_USER" -q 2>/dev/null; then
        break
    fi
    if ! kill -0 "$PGTIKV_PID" 2>/dev/null; then
        echo "ERROR: pg-tikv failed to start"
        cat /tmp/pgtikv-test.log
        echo "### Error: pg-tikv failed to start" >> "$REPORT_FILE"
        exit 1
    fi
    sleep 1
done

if ! pg_isready -h 127.0.0.1 -p "$PG_PORT" -U "$PG_USER" -q 2>/dev/null; then
    echo "ERROR: pg-tikv not ready after 30s"
    cat /tmp/pgtikv-test.log
    echo "### Error: pg-tikv not ready after 30s" >> "$REPORT_FILE"
    exit 1
fi

PG_DSN="postgres://$PG_USER:$PG_PASSWORD@127.0.0.1:$PG_PORT/postgres"
echo "pg-tikv ready (PID: $PGTIKV_PID)"
echo "PG_DSN: $PG_DSN"
echo ""

echo "[4/5] Running integration tests..."
INTEGRATION_START=$(date +%s)
INTEGRATION_OUTPUT=$(uv run "$SCRIPT_DIR/scripts/integration_test.py" --dsn "$PG_DSN" "$SCRIPT_DIR/tests/" "${INTEGRATION_ARGS[@]}" 2>&1) || INTEGRATION_EXIT=$?
INTEGRATION_END=$(date +%s)
INTEGRATION_TIME=$((INTEGRATION_END - INTEGRATION_START))
echo "$INTEGRATION_OUTPUT"
echo ""

# Parse integration test results
INTEGRATION_PASSED=$(echo "$INTEGRATION_OUTPUT" | grep -oP '\d+(?= passed)' | tail -1 || echo "0")
INTEGRATION_FAILED=$(echo "$INTEGRATION_OUTPUT" | grep -oP '\d+(?= failed)' | tail -1 || echo "0")

cat >> "$REPORT_FILE" << EOF
### Integration Tests

- **Duration**: ${INTEGRATION_TIME}s
- **Status**: $([ $INTEGRATION_EXIT -eq 0 ] && echo '✅ PASSED' || echo '❌ FAILED')
- **Passed**: $INTEGRATION_PASSED
- **Failed**: $INTEGRATION_FAILED

<details>
<summary>Output</summary>

\`\`\`
$INTEGRATION_OUTPUT
\`\`\`

</details>

EOF

echo "[5/5] Running ORM tests..."
ORM_DATABASE="orm_tests_${REPORT_TIMESTAMP//-/_}"
echo "Preparing isolated ORM database '$ORM_DATABASE'..."
PGPASSWORD="$PG_PASSWORD" psql -X -q \
    -h 127.0.0.1 -p "$PG_PORT" -U "$PG_USER" -d postgres \
    -v ON_ERROR_STOP=1 \
    -c "DROP DATABASE IF EXISTS \"$ORM_DATABASE\"" \
    -c "CREATE DATABASE \"$ORM_DATABASE\""
ORM_DSN="postgres://$PG_USER:$PG_PASSWORD@127.0.0.1:$PG_PORT/$ORM_DATABASE"

cd "$SCRIPT_DIR/orm-tests"
if [ ! -d "node_modules" ]; then
    echo "Installing dependencies..."
    npm install --silent
fi

ORM_SUITES=(typeorm/ sequelize/ knex/ drizzle/ pg-client/)
if [ "$INCLUDE_PRISMA" -eq 1 ]; then
    if [ -d "prisma/" ] && compgen -G "prisma/*.test.ts" > /dev/null 2>&1; then
        ORM_SUITES+=(prisma/)
        if [ -f "schema.prisma" ] || [ -f "prisma/schema.prisma" ]; then
            if [ ! -d "node_modules/.prisma" ]; then
                echo "Generating Prisma client..."
                npx prisma generate --no-hints
            fi
        else
            echo "Prisma schema not found; skipping prisma generate"
        fi
    else
        echo "WARNING: --with-prisma requested but orm-tests/prisma/ suite not found; skipping."
    fi
else
    echo "Prisma tests are skipped by default; use --with-prisma or WITH_PRISMA=1 to include them."
fi

ORM_START=$(date +%s)
ORM_OUTPUT=$(PG_DSN="$ORM_DSN" timeout 300 npm test -- "${ORM_SUITES[@]}" 2>&1) || ORM_EXIT=$?
ORM_END=$(date +%s)
ORM_TIME=$((ORM_END - ORM_START))
echo "$ORM_OUTPUT"
cd "$SCRIPT_DIR"
echo ""

# Parse ORM test results
ORM_PASSED=$(echo "$ORM_OUTPUT" | grep -oP '\d+(?= passed)' | tail -1 || echo "0")
ORM_FAILED=$(echo "$ORM_OUTPUT" | grep -oP '\d+(?= failed)' | tail -1 || echo "0")
ORM_SKIPPED=$(echo "$ORM_OUTPUT" | grep -oP '\d+(?= skipped)' | tail -1 || echo "0")

cat >> "$REPORT_FILE" << EOF
### ORM Tests

- **Duration**: ${ORM_TIME}s
- **Status**: $([ $ORM_EXIT -eq 0 ] && echo '✅ PASSED' || echo '❌ FAILED')
- **Suites**: ${ORM_SUITES[*]}
- **Passed**: $ORM_PASSED
- **Failed**: $ORM_FAILED
- **Skipped**: $ORM_SKIPPED

<details>
<summary>Output</summary>

\`\`\`
$ORM_OUTPUT
\`\`\`

</details>

EOF

# Calculate totals
END_TIME=$(date +%s)
TOTAL_TIME=$((END_TIME - START_TIME))
TOTAL_PASSED=$((INTEGRATION_PASSED + ORM_PASSED))
TOTAL_FAILED=$((INTEGRATION_FAILED + ORM_FAILED))

# Summary
echo "=== Test Summary ==="
echo "Integration tests: $([ $INTEGRATION_EXIT -eq 0 ] && echo 'PASSED' || echo 'FAILED') (${INTEGRATION_TIME}s)"
echo "ORM tests: $([ $ORM_EXIT -eq 0 ] && echo 'PASSED' || echo 'FAILED') (${ORM_TIME}s)"
echo "Total time: ${TOTAL_TIME}s"
echo ""
echo "Report saved to: $REPORT_FILE"

cat >> "$REPORT_FILE" << EOF
---

## Summary

| Suite | Status | Passed | Failed | Duration |
|-------|--------|--------|--------|----------|
| Integration | $([ $INTEGRATION_EXIT -eq 0 ] && echo '✅ PASSED' || echo '❌ FAILED') | $INTEGRATION_PASSED | $INTEGRATION_FAILED | ${INTEGRATION_TIME}s |
| ORM | $([ $ORM_EXIT -eq 0 ] && echo '✅ PASSED' || echo '❌ FAILED') | $ORM_PASSED | $ORM_FAILED | ${ORM_TIME}s |
| **Total** | $([ $INTEGRATION_EXIT -eq 0 ] && [ $ORM_EXIT -eq 0 ] && echo '✅ PASSED' || echo '❌ FAILED') | **$TOTAL_PASSED** | **$TOTAL_FAILED** | **${TOTAL_TIME}s** |

---

*Report generated by \`run_tests.sh\`*
EOF

if [ $INTEGRATION_EXIT -ne 0 ] || [ $ORM_EXIT -ne 0 ]; then
    exit 1
fi
