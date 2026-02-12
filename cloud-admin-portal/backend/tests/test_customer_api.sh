#!/usr/bin/env bash
set -euo pipefail

API_URL="${API_URL:-http://localhost:8090/api}"
PASS_OK=0
PASS_FAIL=0

pass() { echo "  ✓ $1"; PASS_OK=$((PASS_OK + 1)); }
fail() { echo "  ✗ $1"; PASS_FAIL=$((PASS_FAIL + 1)); }

echo "=== db9 Customer API Integration Tests ==="
echo "API: $API_URL"
echo ""

# 1. Register
echo "--- Register ---"
RESP=$(curl -s -w "\n%{http_code}" -X POST "$API_URL/customer/register" \
  -H "Content-Type: application/json" \
  -d '{"email":"test-'"$RANDOM"'@example.com","password":"SecurePass123!"}')
HTTP_CODE=$(echo "$RESP" | tail -1)
BODY=$(echo "$RESP" | sed '$d')
[ "$HTTP_CODE" = "201" ] && pass "Register returns 201" || fail "Register returns $HTTP_CODE"
echo "$BODY" | grep -q '"id"' && pass "Response has id" || fail "Response missing id"

EMAIL="inttest-$(date +%s)-$$@example.com"
PASSWORD="TestPassword123!"

# Register with known email
curl -s -X POST "$API_URL/customer/register" \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" > /dev/null

# 2. Duplicate register
echo "--- Duplicate Register ---"
RESP=$(curl -s -w "\n%{http_code}" -X POST "$API_URL/customer/register" \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")
HTTP_CODE=$(echo "$RESP" | tail -1)
[ "$HTTP_CODE" = "409" ] && pass "Duplicate register returns 409" || fail "Duplicate register returns $HTTP_CODE"

# 3. Login
echo "--- Login ---"
RESP=$(curl -s -w "\n%{http_code}" -X POST "$API_URL/customer/login" \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")
HTTP_CODE=$(echo "$RESP" | tail -1)
BODY=$(echo "$RESP" | sed '$d')
[ "$HTTP_CODE" = "200" ] && pass "Login returns 200" || fail "Login returns $HTTP_CODE"
TOKEN=$(echo "$BODY" | grep -o '"token":"[^"]*"' | cut -d'"' -f4)
[ -n "$TOKEN" ] && pass "Token received" || fail "No token in response"

# 4. Invalid login
echo "--- Invalid Login ---"
RESP=$(curl -s -w "\n%{http_code}" -X POST "$API_URL/customer/login" \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"$EMAIL\",\"password\":\"WrongPassword\"}")
HTTP_CODE=$(echo "$RESP" | tail -1)
[ "$HTTP_CODE" = "401" ] && pass "Invalid login returns 401" || fail "Invalid login returns $HTTP_CODE"

# 5. Get me
echo "--- Get Me ---"
RESP=$(curl -s -w "\n%{http_code}" -H "Authorization: Bearer $TOKEN" "$API_URL/customer/me")
HTTP_CODE=$(echo "$RESP" | tail -1)
BODY=$(echo "$RESP" | sed '$d')
[ "$HTTP_CODE" = "200" ] && pass "Get me returns 200" || fail "Get me returns $HTTP_CODE"
echo "$BODY" | grep -q "$EMAIL" && pass "Response has correct email" || fail "Response missing email"

# 6. No auth
echo "--- No Auth ---"
RESP=$(curl -s -w "\n%{http_code}" "$API_URL/customer/me")
HTTP_CODE=$(echo "$RESP" | tail -1)
[ "$HTTP_CODE" = "401" ] && pass "No auth returns 401" || fail "No auth returns $HTTP_CODE"

# 7. List tokens
echo "--- List Tokens ---"
RESP=$(curl -s -w "\n%{http_code}" -H "Authorization: Bearer $TOKEN" "$API_URL/customer/tokens")
HTTP_CODE=$(echo "$RESP" | tail -1)
[ "$HTTP_CODE" = "200" ] && pass "List tokens returns 200" || fail "List tokens returns $HTTP_CODE"

# Summary
echo ""
echo "=== Results: $PASS_OK passed, $PASS_FAIL failed ==="
[ "$PASS_FAIL" -eq 0 ] && exit 0 || exit 1
