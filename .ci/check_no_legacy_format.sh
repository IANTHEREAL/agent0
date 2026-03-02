#!/usr/bin/env bash
# Guard against re-introduction of legacy format code.
#
# NOTE: -e intentionally omitted. grep returns exit code 1 when no match
# is found — that IS the passing case for a legacy guard. With -e, the
# script would abort on clean code. Errors are tracked via $ERRORS counter.
set -uo pipefail

ERRORS=0

check_pattern() {
    local pattern="$1" allowed="${2:-0}" label="$3"
    local matches=""
    # || true: prevents pipefail from aborting when grep finds no matches (exit 1)
    matches=$(grep -rn --include='*.rs' "$pattern" src/ \
        | grep -v '// CI-ALLOWED' \
        || true)
    local count=0
    if [ -n "$matches" ]; then
        count=$(printf '%s\n' "$matches" | wc -l)
    fi
    if [ "$count" -gt "$allowed" ]; then
        echo "FAIL: '$label' found $count occurrences (allowed $allowed):"
        printf '%s\n' "$matches"
        ERRORS=$((ERRORS + 1))
    fi
}

check_pattern 'deserialize_v1_bincode' 0 'V1 bincode deserializer'
check_pattern 'V1Era[0-9]'            0 'V1 era structs'
check_pattern 'V1ColumnDef'           0 'V1 column def'
check_pattern 'V1IndexDef'            0 'V1 index def'
check_pattern 'USE_V2_SCHEMA_FORMAT'  0 'V2 format flag'
check_pattern 'storage_version[[:space:]]*==[[:space:]]*0'  0 'HNSW v0 branch'
check_pattern 'storage_version[[:space:]]*=[[:space:]]*0'   0 'HNSW v0 assignment'
check_pattern 'storage_version[[:space:]]*:[[:space:]]*0'   0 'HNSW v0 struct init'

if [ "$ERRORS" -gt 0 ]; then
    echo "Legacy guard failed ($ERRORS violations)"
    exit 1
fi
echo "Legacy format guard: PASS"
