#!/usr/bin/env bash
# scripts/test_governance_gate_v.sh — Self-tests for Gate V enforcement (#1323)
set -euo pipefail

PASS=0; FAIL=0

# Decision function matching the CI logic:
# gate_v_check <has_bug_label:0|1> <changed_files_newline_separated>
# Returns 0 (pass) or 1 (fail).
gate_v_check() {
  local has_bug="$1"
  local changed_files="$2"
  if [[ "$has_bug" -eq 0 ]]; then
    return 0  # No bug label → no enforcement
  fi
  if echo "$changed_files" | grep -qx "scripts/regression_gate.list"; then
    return 0  # Bug label + gate list touched → pass
  fi
  return 1    # Bug label + gate list NOT touched → fail
}

run_test() {
  local name="$1" expected="$2"; shift 2
  if "$@"; then result=0; else result=1; fi
  if [[ "$result" -eq "$expected" ]]; then
    echo "  PASS: $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $name (expected=$expected got=$result)"
    FAIL=$((FAIL + 1))
  fi
}

echo "=== Gate V self-tests ==="

# --- Positive case: bug label + gate list touched → PASS (exit 0) ---
run_test "bug+gate_list_touched" 0 \
  gate_v_check 1 "$(printf 'src/sql/foo.rs\nscripts/regression_gate.list\ntests/123_foo.sql')"

# --- Negative case: bug label + gate list NOT touched → FAIL (exit 1) ---
run_test "bug+gate_list_missing" 1 \
  gate_v_check 1 "$(printf 'src/sql/foo.rs\ntests/123_foo.sql')"

# --- No bug label → PASS regardless ---
run_test "no_bug_label" 0 \
  gate_v_check 0 "$(printf 'src/sql/foo.rs')"

# --- Bug label + only gate list touched → PASS ---
run_test "bug+only_gate_list" 0 \
  gate_v_check 1 "scripts/regression_gate.list"

# --- Bug label + empty changeset → FAIL ---
run_test "bug+empty_changeset" 1 \
  gate_v_check 1 ""

# --- Regression: issue ref parsing must not truncate at 20 (#1345) ---
# Simulates the CI parsing logic: extract unique #<id> refs from PR body.
# The bug issue (9999) is placed at position 25 — must still be found.
parse_issue_refs() {
  local body="$1"
  # Match #<digits>, deduplicate, no truncation
  echo "$body" | grep -oE '#[0-9]+' | sed 's/#//' | sort -un
}

body_25_refs=""
for i in $(seq 1 24); do body_25_refs+="#${i} "; done
body_25_refs+="#9999"

refs=$(parse_issue_refs "$body_25_refs")
ref_count=$(echo "$refs" | wc -l)
has_9999=$(echo "$refs" | grep -cx "9999")

run_test "issue_refs_no_truncation_at_20 (count=$ref_count)" 0 \
  test "$ref_count" -eq 25

run_test "bug_issue_at_pos_25_still_found" 0 \
  test "$has_9999" -eq 1

echo "=== Results: $PASS passed, $FAIL failed ==="
[[ "$FAIL" -eq 0 ]] || exit 1
