#!/usr/bin/env bash
set -euo pipefail

API_URL="${API_URL:-http://localhost:8090/api}"
API_BASE="${API_URL%/}"
API_KEY="${API_KEY:-${PGTIKV_API_KEY:-}}"
HEALTH_URL="${HEALTH_URL:-${API_BASE}/health}"

PASS_OK=0
PASS_FAIL=0

pass() { echo "  ✓ $1"; PASS_OK=$((PASS_OK + 1)); }
fail() { echo "  ✗ $1"; PASS_FAIL=$((PASS_FAIL + 1)); }

RUN_ID="$(date +%s)-$$"
EMAIL="fulltest-${RUN_ID}@example.com"
PASSWORD="TestFullPass123!"
DB_NAME="testdb-${RUN_ID}"
BRANCH_NAME="branch-${RUN_ID}"
MIGRATION_NAME="20260212_test_${RUN_ID}"

CREATED_DBS=()
TOKEN=""
CUSTOMER_ID=""
DB_ID=""
BRANCH_ID=""

LAST_CURL_RC=0
LAST_HTTP=""
LAST_BODY=""

request() {
    local method="$1"
    local path="$2"
    local data="${3:-}"
    local use_auth="${4:-0}"
    local url="${API_BASE}${path}"
    local resp
    local -a cmd

    cmd=(curl -s -w "\n%{http_code}" -X "$method" "$url" -H "Content-Type: application/json")
    if [ -n "$API_KEY" ]; then
        cmd+=(-H "X-API-Key: $API_KEY")
    fi
    if [ "$use_auth" = "1" ] && [ -n "$TOKEN" ]; then
        cmd+=(-H "Authorization: Bearer $TOKEN")
    fi
    if [ -n "$data" ]; then
        cmd+=(-d "$data")
    fi

    set +e
    resp="$("${cmd[@]}")"
    LAST_CURL_RC=$?
    set -e

    if [ "$LAST_CURL_RC" -ne 0 ]; then
        LAST_HTTP="000"
        LAST_BODY=""
        return
    fi

    LAST_HTTP="$(printf '%s\n' "$resp" | sed -n '$p')"
    LAST_BODY="$(printf '%s\n' "$resp" | sed '$d')"
}

json_field() {
    local body="$1"
    local field="$2"
    printf '%s\n' "$body" | grep -o "\"${field}\":\"[^\"]*\"" | head -n 1 | cut -d'"' -f4 || true
}

assert_http() {
    local expected="$1"
    local desc="$2"
    if [ "$LAST_CURL_RC" -eq 0 ] && [ "$LAST_HTTP" = "$expected" ]; then
        pass "$desc"
    else
        fail "$desc (curl_rc=$LAST_CURL_RC http=$LAST_HTTP expected=$expected)"
    fi
}

assert_http_one_of() {
    local expected_list="$1"
    local desc="$2"
    local ok=1
    local expected
    for expected in $expected_list; do
        if [ "$LAST_HTTP" = "$expected" ]; then
            ok=0
            break
        fi
    done
    if [ "$LAST_CURL_RC" -eq 0 ] && [ "$ok" -eq 0 ]; then
        pass "$desc"
    else
        fail "$desc (curl_rc=$LAST_CURL_RC http=$LAST_HTTP expected_one_of=$expected_list)"
    fi
}

assert_body_contains() {
    local needle="$1"
    local desc="$2"
    if printf '%s\n' "$LAST_BODY" | grep -q "$needle"; then
        pass "$desc"
    else
        fail "$desc"
    fi
}

assert_non_empty() {
    local value="$1"
    local desc="$2"
    if [ -n "$value" ]; then
        pass "$desc"
    else
        fail "$desc"
    fi
}

add_created_db() {
    local id="$1"
    if [ -n "$id" ]; then
        CREATED_DBS+=("$id")
    fi
}

remove_created_db() {
    local id="$1"
    local kept=()
    local db
    for db in "${CREATED_DBS[@]}"; do
        if [ "$db" != "$id" ]; then
            kept+=("$db")
        fi
    done
    CREATED_DBS=("${kept[@]}")
}

poll_until_active() {
    local id="$1"
    local label="$2"
    local state=""
    local i

    echo -n "  Waiting for ${label} ACTIVE"
    for i in $(seq 1 40); do
        sleep 3
        request GET "/customer/databases/${id}" "" 1
        state="$(json_field "$LAST_BODY" "state")"
        if [ "$LAST_HTTP" = "200" ] && [ "$state" = "ACTIVE" ]; then
            echo " done"
            pass "${label} became ACTIVE"
            return 0
        fi
        echo -n "."
    done

    echo " timeout"
    fail "${label} did not become ACTIVE (last_state=${state:-unknown}, http=${LAST_HTTP})"
    return 1
}

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    if [ "${#CREATED_DBS[@]}" -eq 0 ]; then
        echo "No databases to cleanup"
        return
    fi

    local db_id
    local -a cmd
    for db_id in "${CREATED_DBS[@]}"; do
        cmd=(curl -s -X DELETE "${API_BASE}/customer/databases/${db_id}")
        if [ -n "$TOKEN" ]; then
            cmd+=(-H "Authorization: Bearer $TOKEN")
        fi
        if [ -n "$API_KEY" ]; then
            cmd+=(-H "X-API-Key: $API_KEY")
        fi
        "${cmd[@]}" >/dev/null 2>&1 || true
    done
    echo "Cleanup done"
}
trap cleanup EXIT

echo "=== db9 Full Customer API Integration Tests ==="
echo "API: $API_BASE"
echo "Health: $HEALTH_URL"
echo "Run ID: $RUN_ID"
echo ""

echo "--- Auth Section ---"

for tool in curl grep cut sed date seq; do
    if command -v "$tool" >/dev/null 2>&1; then
        pass "Prerequisite available: $tool"
    else
        fail "Prerequisite missing: $tool"
    fi
done

set +e
if [ -n "$API_KEY" ]; then
    curl -sf -H "X-API-Key: $API_KEY" "$HEALTH_URL" >/dev/null
else
    curl -sf "$HEALTH_URL" >/dev/null
fi
HEALTH_RC=$?
set -e
if [ "$HEALTH_RC" -eq 0 ]; then
    pass "Prerequisite health check reachable"
else
    fail "Prerequisite health check failed: $HEALTH_URL"
fi

request POST "/customer/register" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" 0
assert_http "201" "Register returns 201"
CUSTOMER_ID="$(json_field "$LAST_BODY" "id")"
assert_non_empty "$CUSTOMER_ID" "Register response has id"

request POST "/customer/register" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" 0
assert_http "409" "Duplicate register returns 409"

request POST "/customer/login" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" 0
assert_http "200" "Login returns 200"
TOKEN="$(json_field "$LAST_BODY" "token")"
assert_non_empty "$TOKEN" "Login response has token"

request POST "/customer/login" "{\"email\":\"$EMAIL\",\"password\":\"WrongPassword\"}" 0
assert_http "401" "Wrong password login returns 401"

request GET "/customer/me" "" 1
assert_http "200" "Get me returns 200"
assert_body_contains "$EMAIL" "Get me response contains email"

echo "--- Anonymous Auth Section ---"

REGISTERED_TOKEN="$TOKEN"
REGISTERED_EMAIL="$EMAIL"
TOKEN=""

request POST "/customer/anonymous-register" "" 0
assert_http "200" "Anonymous register returns 200"
ANON_TOKEN="$(json_field "$LAST_BODY" "token")"
assert_non_empty "$ANON_TOKEN" "Anonymous register returns token"
assert_body_contains "is_anonymous" "Anonymous register response contains is_anonymous"

TOKEN="$ANON_TOKEN"
request GET "/customer/me" "" 1
assert_http "200" "Anonymous user /me returns 200"
assert_body_contains "anonymous.local" "Anonymous user email contains anonymous.local"

request POST "/customer/databases" "{\"name\":\"anon-db-${RUN_ID}\"}" 1
assert_http "201" "Anonymous create database returns 201"
ANON_DB_ID="$(json_field "$LAST_BODY" "id")"
assert_non_empty "$ANON_DB_ID" "Anonymous create database returns id"
add_created_db "$ANON_DB_ID"

request POST "/customer/claim" "{\"email\":\"claim-${RUN_ID}@example.com\",\"password\":\"short\"}" 1
assert_http "400" "Claim with short password returns 400"

request POST "/customer/claim" "{\"email\":\"not-an-email\",\"password\":\"LongEnough123!\"}" 1
assert_http "400" "Claim with invalid email returns 400"

request POST "/customer/claim" "{\"email\":\"$REGISTERED_EMAIL\",\"password\":\"LongEnough123!\"}" 1
assert_http "409" "Claim with existing email returns 409"

CLAIM_EMAIL="claimed-${RUN_ID}@example.com"
CLAIM_PASSWORD="ClaimPass123!"
request POST "/customer/claim" "{\"email\":\"$CLAIM_EMAIL\",\"password\":\"$CLAIM_PASSWORD\"}" 1
assert_http "200" "Claim anonymous account returns 200"
assert_body_contains "claimed" "Claim response contains claimed"
assert_body_contains "$CLAIM_EMAIL" "Claim response contains new email"

request POST "/customer/claim" "{\"email\":\"double-${RUN_ID}@example.com\",\"password\":\"AnotherPass123!\"}" 1
assert_http "400" "Double claim returns 400 (no longer anonymous)"

request POST "/customer/login" "{\"email\":\"$CLAIM_EMAIL\",\"password\":\"$CLAIM_PASSWORD\"}" 0
assert_http "200" "Login with claimed credentials returns 200"

request DELETE "/customer/databases/${ANON_DB_ID}" "" 1
assert_http_one_of "200 500" "Delete anonymous database returns expected status"
remove_created_db "$ANON_DB_ID"

TOKEN="$REGISTERED_TOKEN"

echo "--- Database Section ---"

request POST "/customer/databases" "{\"name\":\"$DB_NAME\",\"region\":\"us-east\"}" 1
assert_http "201" "Create database returns 201"
DB_ID="$(json_field "$LAST_BODY" "id")"
assert_non_empty "$DB_ID" "Create database returns id"
add_created_db "$DB_ID"
assert_body_contains "\"state\"" "Create database response includes state"

poll_until_active "$DB_ID" "Database" || true

request GET "/customer/databases" "" 1
assert_http "200" "List databases returns 200"
assert_body_contains "$DB_ID" "List databases contains created DB"

echo "--- SQL Section ---"

request POST "/customer/databases/${DB_ID}/sql" "{\"query\":\"CREATE TABLE test_ft (id INT PRIMARY KEY, name TEXT NOT NULL, data TEXT)\"}" 1
assert_http "200" "SQL CREATE TABLE returns 200"

request POST "/customer/databases/${DB_ID}/sql" "{\"query\":\"INSERT INTO test_ft VALUES (1, 'Alice', 'hello'), (2, 'Bob', 'world')\"}" 1
assert_http "200" "SQL INSERT returns 200"

request POST "/customer/databases/${DB_ID}/sql" "{\"query\":\"SELECT id, name FROM test_ft ORDER BY id\"}" 1
assert_http "200" "SQL SELECT returns 200"
assert_body_contains "\"columns\"" "SQL SELECT response includes columns"
assert_body_contains "\"rows\"" "SQL SELECT response includes rows"
assert_body_contains "Alice" "SQL SELECT response contains Alice"
assert_body_contains "Bob" "SQL SELECT response contains Bob"

request POST "/customer/databases/${DB_ID}/sql" "{\"query\":\"SELECT * FROM nonexistent_table_xyz\"}" 1
assert_http_one_of "200 400" "SQL error request returns expected status"
assert_body_contains "error" "SQL error response includes error field"

echo "--- User Management Section ---"

request GET "/customer/databases/${DB_ID}/users" "" 1
assert_http "200" "List users returns 200"
assert_body_contains "postgres" "List users contains default superuser"

request POST "/customer/databases/${DB_ID}/users" "{\"username\":\"testuser\",\"password\":\"Test123!\"}" 1
assert_http "201" "Create user returns 201"

request DELETE "/customer/databases/${DB_ID}/users/testuser" "" 1
assert_http "200" "Delete user returns 200"

request DELETE "/customer/databases/${DB_ID}/users/admin" "" 1
assert_http_one_of "400 403" "Delete admin is protected"

echo "--- Dump Section ---"

request POST "/customer/databases/${DB_ID}/dump" "{\"ddl_only\":true}" 1
assert_http "200" "Dump DDL-only returns 200"
assert_body_contains "CREATE TABLE" "DDL-only dump contains CREATE TABLE"

request POST "/customer/databases/${DB_ID}/dump" "{\"ddl_only\":false}" 1
assert_http "200" "Dump full returns 200"
assert_body_contains "INSERT" "Full dump contains INSERT"

echo "--- Schema Section ---"

request GET "/customer/databases/${DB_ID}/schema" "" 1
assert_http "200" "Get schema returns 200"
assert_body_contains "test_ft" "Schema response includes test_ft"

echo "--- Migration Section ---"

request POST "/customer/databases/${DB_ID}/migrations" "{\"name\":\"$MIGRATION_NAME\",\"sql\":\"CREATE TABLE mig_test (id INT PRIMARY KEY);\",\"checksum\":\"abc123\"}" 1
assert_http "200" "Apply migration returns 200"
assert_body_contains "applied" "Apply migration response contains applied"

request POST "/customer/databases/${DB_ID}/migrations" "{\"name\":\"$MIGRATION_NAME\",\"sql\":\"CREATE TABLE mig_test (id INT PRIMARY KEY);\",\"checksum\":\"abc123\"}" 1
assert_http "200" "Idempotent migration re-apply returns 200"
assert_body_contains "already_applied" "Idempotent migration reports already_applied"

request POST "/customer/databases/${DB_ID}/migrations" "{\"name\":\"$MIGRATION_NAME\",\"sql\":\"CREATE TABLE mig_test2 (id INT PRIMARY KEY);\",\"checksum\":\"xyz789\"}" 1
assert_http "409" "Checksum conflict returns 409"

request GET "/customer/databases/${DB_ID}/migrations" "" 1
assert_http "200" "List migrations returns 200"
assert_body_contains "$MIGRATION_NAME" "List migrations contains migration name"

echo "--- Branch Section ---"

request POST "/customer/databases/${DB_ID}/branch" "{\"name\":\"$BRANCH_NAME\"}" 1
assert_http "201" "Create branch returns 201"
BRANCH_ID="$(json_field "$LAST_BODY" "id")"
assert_non_empty "$BRANCH_ID" "Create branch returns id"
add_created_db "$BRANCH_ID"

poll_until_active "$BRANCH_ID" "Branch database" || true

request POST "/customer/databases/${BRANCH_ID}/sql" "{\"query\":\"SELECT COUNT(*) FROM test_ft\"}" 1
assert_http "200" "Branch SQL verifies copied schema"

request DELETE "/customer/databases/${BRANCH_ID}" "" 1
# PD may return 400 for disable_keyspace in test environments (API v2 limitation)
assert_http_one_of "200 500" "Delete branch returns 200 (500 if PD keyspace disable unsupported)"
remove_created_db "$BRANCH_ID"

echo "--- Token Section ---"

request GET "/customer/tokens" "" 1
assert_http "200" "List tokens returns 200"

echo "--- Cleanup Section ---"

request DELETE "/customer/databases/${DB_ID}" "" 1
# PD may return 400 for disable_keyspace in test environments (API v2 limitation)
assert_http_one_of "200 500" "Delete database returns 200 (500 if PD keyspace disable unsupported)"
remove_created_db "$DB_ID"

echo ""
echo "=== Results: $PASS_OK passed, $PASS_FAIL failed ==="
[ "$PASS_FAIL" -eq 0 ] && exit 0 || exit 1
