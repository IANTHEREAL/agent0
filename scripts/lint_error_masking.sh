#!/bin/bash
# Lint guard for error-masking patterns (issue #657).
# Prevents reintroduction of silent unwrap_or fallbacks.
set -euo pipefail

FAIL=0

# ── Helper: context-aware INTENTIONAL check ──────────────────────────
# Checks whether any of the 5 preceding lines (or the match line itself)
# contains "// INTENTIONAL:" — handles markers placed above chained calls.
check_unwrap_or_text() {
    local dir_args=("$@")
    local matches
    matches=$(grep -rn 'unwrap_or(DataType::Text)' "${dir_args[@]}" \
        | grep -v '_test\.rs\|/tests/' \
        || true)

    [ -z "$matches" ] && return 0

    local violations=""
    while IFS= read -r match_line; do
        local file lineno start context content
        file=$(echo "$match_line" | cut -d: -f1)
        lineno=$(echo "$match_line" | cut -d: -f2)

        # Check preceding 5 lines + current line for INTENTIONAL marker
        # (covers multi-line chained method calls like .get(i).cloned().unwrap_or())
        start=$((lineno - 5))
        [ "$start" -lt 1 ] && start=1
        context=$(sed -n "${start},${lineno}p" "$file")
        if echo "$context" | grep -q '// INTENTIONAL:'; then
            continue
        fi

        # Exclude error-message construction (not error masking)
        content=$(sed -n "${lineno}p" "$file")
        if echo "$content" | grep -qE 'Err\(|anyhow!\(|from:.*\.data_type\(\)'; then
            continue
        fi
        # Check preceding 4 lines for error construction context
        local err_start=$((lineno - 4))
        [ "$err_start" -lt 1 ] && err_start=1
        local err_context
        err_context=$(sed -n "${err_start},${lineno}p" "$file")
        if echo "$err_context" | grep -qE 'Err\(|anyhow!\(|InvalidCast'; then
            continue
        fi

        violations="${violations}${match_line}"$'\n'
    done <<< "$matches"

    if [ -n "$violations" ]; then
        echo "$violations"
        echo "FAIL: unwrap_or(DataType::Text) without INTENTIONAL marker"
        return 1
    fi
    return 0
}

# ── Rule 1: unwrap_or(DataType::Text) ────────────────────────────────
if ! check_unwrap_or_text src/sql/ src/protocol/ src/types/; then
    FAIL=1
fi

# ── Rule 2: compare_values with unwrap_or ────────────────────────────
if grep -rn 'compare_values.*\.unwrap_or' src/ \
    | grep -v '_test\.rs\|/tests/' ; then
    echo "FAIL: compare_values() with unwrap_or — use ? or sort_by_fallible"
    FAIL=1
fi

# ── Rule 3: parse().unwrap_or(0) in SQL engine ───────────────────────
if grep -rn '\.parse.*\.unwrap_or(0' src/sql/ \
    | grep -v '// INTENTIONAL:' \
    | grep -v '_test\.rs\|/tests/' ; then
    echo "FAIL: parse().unwrap_or(0) without INTENTIONAL marker"
    FAIL=1
fi

if [ $FAIL -ne 0 ]; then
    echo ""
    echo "Error masking lint failed. See https://github.com/c4pt0r/db9/issues/657"
    exit 1
fi

echo "Error masking lint passed."
