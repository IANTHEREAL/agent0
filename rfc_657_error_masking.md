# RFC #657: Error-Masking Elimination — Final Design (Post-Review)

**Issue:** #657 (Phase 1 / Dev A)
**Status:** Approved with corrections applied
**GitHub thread:**
- RFC: https://github.com/c4pt0r/tipg/issues/657#issuecomment-3886889639
- Review: https://github.com/c4pt0r/tipg/issues/657#issuecomment-3886963477
- Corrections: https://github.com/c4pt0r/tipg/issues/657#issuecomment-3886986649

---

## 1. Scope Summary

| Category | Sites | Fix mechanism |
|---|---|---|
| `compare_values().unwrap_or()` in Result-returning fn | 14 | Direct `?` |
| `compare_values().unwrap_or(0)` in sort closures (direct) | 3 | `sort_by_fallible` + `compare_values()?` |
| `compare_order_by_values` signature change | 1 | `-> Result<Ordering>` |
| `compare_order_by_values` callers (operator framework) | 5 | `sort_by_fallible` wrapper |
| Dead code deletion (NULLIF/GREATEST/LEAST match arms) | 3 | Delete |
| `parse().unwrap_or(0)` / numeric overflow | 8 | `map_err` + `?` |
| `sql_datatype_to_internal` merge | 7 | Rename + `?` |
| `infer_expr_type` callers (production) | ~35 | `?` |
| `infer_expr_type` callers (test) | ~11 | `.unwrap()` |
| Function arg inference (infer.rs:263,268) | 2 | `collect::<Result>` |
| Registry SameAsArg/FirstNonNull | 2 | Return `None` instead of Text |
| Custom closure signature | ~6 | `-> Result<DataType, TypeError>` |
| CTE/VALUES scan-all-rows | 12 | `find_map` over all rows |
| Remaining type inference sites | 3 | Propagate error |
| INTENTIONAL markers | 9 | Add comments |
| CI lint | 1 script | New file |
| **Total** | **~77** | |

---

## 2. New Error Variant

```rust
// src/sql/error.rs — add variant:
#[error("numeric value out of range{}", if detail.is_empty() { String::new() } else { format!(": {}", detail) })]
NumericValueOutOfRange { detail: String },
// SQLSTATE: 22003

// Add to sqlstate() match:
SqlError::NumericValueOutOfRange { .. } => "22003",
```

---

## 3. PR 1: Comparison + Parse (self-contained, no cascading signatures)

### 3.1 New utility: `sort_by_fallible`

**File:** `src/sql/expr/operators.rs` (or a new `src/sql/utils.rs`)

```rust
/// Sort with fallible comparison. Propagates the first comparison error.
/// After the first error, remaining comparisons short-circuit to Equal
/// and the partially-sorted result is discarded.
///
/// NOTE: Rust's `slice::sort_by` (merge sort) always terminates even when
/// the comparator returns Equal for error pairs.
pub fn sort_by_fallible<T>(
    items: &mut [T],
    mut cmp: impl FnMut(&T, &T) -> Result<std::cmp::Ordering>,
) -> Result<()> {
    let mut first_error: Option<anyhow::Error> = None;
    items.sort_by(|a, b| {
        if first_error.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match cmp(a, b) {
            Ok(ord) => ord,
            Err(e) => {
                first_error = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
```

### 3.2 Delete dead code: NULLIF/GREATEST/LEAST match arms

**File:** `src/sql/expr/mod.rs`
**Lines:** 501-529

The function registry (`functions::get_registry()`) at line 496 intercepts NULLIF, GREATEST, LEAST
before the match at line 500. These match arms are unreachable dead code.

**Action:** Delete the three match arms. The `functions/misc.rs` implementations are canonical.

### 3.3 Fix 14 comparison sites with `?` (Result-returning functions)

All one-line changes: `.unwrap_or(N)` → `?`

| # | File | Line | Before | After |
|---|---|---|---|---|
| 1 | `expr/functions/misc.rs` | 24 | `compare_values(&args[0], &args[1]).unwrap_or(1) == 0` | `compare_values(&args[0], &args[1])? == 0` |
| 2 | `expr/functions/misc.rs` | 36 | `compare_values(&val, &max).unwrap_or(0) > 0` | `compare_values(&val, &max)? > 0` |
| 3 | `expr/functions/misc.rs` | 48 | `compare_values(&val, &min).unwrap_or(0) < 0` | `compare_values(&val, &min)? < 0` |
| 4 | `expr/functions/array.rs` | 99 | `compare_values(v, &elem).unwrap_or(1) == 0` | `compare_values(v, &elem)? == 0` |
| 5 | `expr/functions/array.rs` | 156 | `compare_values(v, &elem).unwrap_or(1) != 0` | `compare_values(v, &elem)? != 0` |
| 6 | `expr/evaluator.rs` | 547 | `compare_values(&val, &item_val).unwrap_or(1) == 0` | `compare_values(&val, &item_val)? == 0` |
| 7 | `expr/evaluator.rs` | 575 | `compare_values(&val, &low_val).unwrap_or(-1) >= 0` | `compare_values(&val, &low_val)? >= 0` |
| 8 | `expr/evaluator.rs` | 576 | `compare_values(&val, &high_val).unwrap_or(1) <= 0` | `compare_values(&val, &high_val)? <= 0` |
| 9 | `expr/evaluator.rs` | 656 | `compare_values(&op_val, &cond_val).unwrap_or(1) == 0` | `compare_values(&op_val, &cond_val)? == 0` |
| 10 | `expr/evaluator.rs` | 1111 | `compare_values(&json_result, &item_val).unwrap_or(1) == 0` | `compare_values(&json_result, &item_val)? == 0` |
| 11 | `expr/operators.rs` | 187 | `compare_values(lv, rv).unwrap_or(1) == 0` | `compare_values(lv, rv)? == 0` |
| 12 | `expr/mod.rs` | 1913 | `compare_values(l, r).unwrap_or(1) == 0` | `compare_values(l, r)? == 0` |
| 13 | `expr/mod.rs` | 1927 | `compare_values(l, r).unwrap_or(1) == 0` | `compare_values(l, r)? == 0` |

NOTE: expr/mod.rs:502,513,524 are deleted in §3.2 (dead code). That's why count is 13, not 16.
Evaluator site at line 1111 is JSON IN LIST check (not @> containment).

### 3.4 Change `compare_order_by_values` signature

**File:** `src/sql/expr/operators.rs:934`

```rust
// Before
pub fn compare_order_by_values(
    left: &Value, right: &Value, asc: bool, nulls_first: bool,
) -> std::cmp::Ordering

// After
pub fn compare_order_by_values(
    left: &Value, right: &Value, asc: bool, nulls_first: bool,
) -> Result<std::cmp::Ordering>
```

NULL handling stays infallible (wrapped in `Ok`). The final branch changes from
`compare_values(left, right).unwrap_or(0)` to `compare_values(left, right)?`.

**Re-export in `expr/mod.rs:1884`:** Update return type to match.

### 3.5 Fix `compare_order_by_values` callers (5 sites in operator framework)

| # | File | Line | Change |
|---|---|---|---|
| 1 | `operators/sort.rs` | 113-124 | `compare_keys` → `Result<Ordering>`; `open()` at line 148 uses `sort_by_fallible` |
| 2 | `operators/window.rs` | 72 | Add `?` (Result-returning context) |
| 3 | `operators/window.rs` | 187 | Use `sort_by_fallible` |
| 4 | `operators/aggregate.rs` | 230 | Use `sort_by_fallible` |
| 5 | `executor/select/join/lateral.rs` | 542 | Use `sort_by_fallible` |

### 3.6 Fix 3 direct `compare_values` sort-closure sites

These bypass `compare_order_by_values` entirely — they call `compare_values` directly in sort closures.

| # | File | Line | Fix |
|---|---|---|---|
| 1 | `executor/operators.rs` | 3214 | Wrap sort closure with `sort_by_fallible`, use `compare_values()?` |
| 2 | `executor/select/order.rs` | 184 | Same |
| 3 | `executor/select/order.rs` | 206 | Same |

### 3.7 Fix 6 parse sites

| # | File | Line | Pattern | Error |
|---|---|---|---|---|
| 1 | `expr/mod.rs` | 260 | `s.parse().unwrap_or(0)` | `InvalidInputSyntax { type_name: "integer", value: s }` |
| 2 | `expr/mod.rs` | 618 | `.parse::<usize>().unwrap_or(0)` | `InvalidInputSyntax { type_name: "integer", value }` |
| 3 | `expr/mod.rs` | 639 | `.parse::<usize>().unwrap_or(0)` | `InvalidInputSyntax { type_name: "integer", value }` |
| 4 | `expr/mod.rs` | 881 | `.parse::<i64>().unwrap_or(0)` | `InvalidInputSyntax { type_name: "oid", value }` |
| 5 | `expr/functions/pg_compat.rs` | 96 | `.parse::<i64>().unwrap_or(0)` | `InvalidInputSyntax { type_name: "oid", value }` |
| 6 | `executor/core/query_exec.rs` | 224 | `s.parse::<f64>().unwrap_or(0.0)` | `InvalidInputSyntax { type_name: "double precision", value }` |

### 3.8 Fix 2 numeric conversion sites

**File:** `src/sql/expr/numeric.rs`

| # | Line | Before | After |
|---|---|---|---|
| 1 | 41 | `fn to_int64(&self) -> i64` | `fn to_int64(&self) -> Result<i64>` |
| 2 | 60 | `fn to_decimal(&self) -> Decimal` | `fn to_decimal(&self) -> Result<Decimal>` |

Callers within numeric.rs arithmetic ops already return `Result`.

### 3.9 Tests for PR 1

Each error path gets at least one test:

```sql
-- Comparison errors
SELECT GREATEST('abc', 123);           -- ERROR: cannot compare
SELECT LEAST('abc', 123);              -- ERROR: cannot compare
SELECT NULLIF(123, 'abc');             -- ERROR: cannot compare
SELECT 1 IN ('a', 'b');               -- ERROR: cannot compare
SELECT 1 BETWEEN 'a' AND 'z';         -- ERROR: cannot compare
SELECT CASE 1 WHEN 'a' THEN 'x' END; -- ERROR: cannot compare
-- ORDER BY with mixed types           -- ERROR: cannot compare

-- Parse errors
SELECT INTERVAL 'abc hours';           -- ERROR: invalid input syntax for type integer
SELECT format_type('not_oid', NULL);   -- ERROR: invalid input syntax for type oid

-- Numeric overflow
SELECT 99999999999999999999999::numeric % 1;  -- ERROR: numeric value out of range
```

---

## 4. PR 2: Type Inference Cascade

### 4.1 Merge `sql_datatype_to_internal`

**File:** `src/sql/types/mapping.rs`

```rust
// KEEP (rename from try_sql_datatype_to_internal):
pub(crate) fn sql_datatype_to_internal_strict(sql_type: &SqlDataType) -> Result<DataType> {
    sql_datatype_to_internal_impl(sql_type, UnknownCustomMode::Text, true)
}

// CHANGE (remove .unwrap_or(DataType::Text)):
pub(crate) fn sql_datatype_to_internal(sql_type: &SqlDataType) -> Result<DataType> {
    sql_datatype_to_internal_impl(sql_type, UnknownCustomMode::UserDefined, false)
}

// DELETE: try_sql_datatype_to_internal (now redundant)
```

**File:** `src/sql/types/mod.rs` — update re-export:
```rust
pub(crate) use mapping::{sql_datatype_to_internal, sql_datatype_to_internal_strict};
```

**Callers:**

| File:Line | Before | After |
|---|---|---|
| `infer.rs:102` | `Ok(super::sql_datatype_to_internal(dt))` | `super::sql_datatype_to_internal(dt).map_err(\|e\| TypeError::UnsupportedExpression(e.to_string()))` |
| `infer.rs:103` | same | same |
| `ddl.rs:57` | `try_sql_datatype_to_internal(...)` | `sql_datatype_to_internal_strict(...)` |
| `ddl.rs:70` | `try_sql_datatype_to_internal(...)` | `sql_datatype_to_internal_strict(...)` |
| `ddl.rs:75` | `try_sql_datatype_to_internal(...)` | `sql_datatype_to_internal_strict(...)` |
| `ddl.rs:839` | `try_sql_datatype_to_internal(...).unwrap_or(DataType::Text)` | `sql_datatype_to_internal_strict(...)?` |
| `expr/mod.rs:1179` | `try_sql_datatype_to_internal(...)` | `sql_datatype_to_internal_strict(...)` |
| `udt.rs:48` | `try_sql_datatype_to_internal(...)` | `sql_datatype_to_internal_strict(...)` |
| `settings_tableless.rs:293` | `try_sql_datatype_to_internal(...)` | `sql_datatype_to_internal_strict(...)` |

### 4.2 Merge `infer_expr_type`

**File:** `src/sql/types/mod.rs`

```rust
// CHANGE (remove .unwrap_or(DataType::Text)):
pub fn infer_expr_type(expr: &Expr, schema: &TableSchema) -> Result<DataType, TypeError> {
    let ctx = TypeContext::single(schema);
    let mut inferrer = TypeInferrer::new(ctx);
    inferrer.infer(expr)
}

// DELETE: try_infer_expr_type (now redundant)
// DELETE: infer_expr_type_join (dead code, #[allow(dead_code)])
```

**File:** `src/sql/projection.rs:104`

```rust
pub fn infer_expr_type(expr: &Expr, schema: &TableSchema) -> Result<DataType, TypeError> {
    super::types::infer_expr_type(expr, schema)
}
```

**Production callers (~35 sites) — all add `?`:**

| File | Lines |
|---|---|
| `executor/operators.rs` | 1102, 1428, 1464, 1501, 1597, 1818, 1823, 1998, 2067, 2279, 2612, 2624, 2697, 2770, 2776, 3875, 3960, 3966 |
| `executor/select/join/operator_tree/projection.rs` | 100, 106, 187, 193, 317, 323, 407, 413, 504 |
| `executor/select/join/aggregate.rs` | 65, 152, 354, 359 |
| `executor/select/join/lateral.rs` | 592 |
| `executor/core/query_exec.rs` | 846 |
| `dml.rs` | 202, 203 |

**Test callers (~11 sites in projection.rs) — add `.unwrap()`:**

| File | Lines |
|---|---|
| `projection.rs` | 127, 185, 224, 263, 303, 347, 368, 389, 407, 425 |

### 4.3 Fix function arg inference

**File:** `src/sql/types/infer.rs:258-272`

```rust
// Before
let arg_types: Vec<DataType> = f.args.iter()
    .filter_map(|arg| match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
            Some(self.infer(expr).unwrap_or(DataType::Text))
        }
        FunctionArg::Named { arg: FunctionArgExpr::Expr(expr), .. } => {
            Some(self.infer(expr).unwrap_or(DataType::Text))
        }
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Some(DataType::Int64),
        _ => None,
    })
    .collect();

// After
let arg_types: Vec<DataType> = f.args.iter()
    .filter_map(|arg| match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(self.infer(expr)),
        FunctionArg::Named { arg: FunctionArgExpr::Expr(expr), .. } => Some(self.infer(expr)),
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Some(Ok(DataType::Int64)),
        _ => None,
    })
    .collect::<Result<Vec<_>, _>>()?;
```

### 4.4 Fix registry SameAsArg/FirstNonNull

**File:** `src/sql/types/registry.rs:95-104`

```rust
// Before
ReturnType::SameAsArg(idx) => arg_types.get(*idx).cloned().unwrap_or(DataType::Text),
ReturnType::FirstNonNull => arg_types.first().cloned().unwrap_or(DataType::Text),

// After — return None (resolve_return_type already returns Option<DataType>)
ReturnType::SameAsArg(idx) => arg_types.get(*idx).cloned()?,
ReturnType::FirstNonNull => arg_types.first().cloned()?,
```

### 4.5 Change Custom closure signature

**File:** `src/sql/types/registry.rs`

```rust
// Before
Custom(fn(&[DataType]) -> DataType),

// After
Custom(fn(&[DataType]) -> Result<DataType, TypeError>),
```

Update `resolve_return_type`:
```rust
// Before
ReturnType::Custom(f) => f(arg_types),

// After (resolve_return_type returns Option<DataType> → need to handle Result)
ReturnType::Custom(f) => match f(arg_types) {
    Ok(dt) => dt,
    Err(_) => return None,
},
```

OR change `resolve_return_type` to return `Result<Option<DataType>, TypeError>`. Decide during implementation.

**Affected Custom registrations:** SUM, AVG, ARRAY_AGG, UNNEST — wrap returns in `Ok(...)`.

### 4.6 CTE/VALUES scan-all-rows (12 sites)

**Pattern:**

```rust
// Before
first_row.values.iter()
    .map(|v| v.data_type().unwrap_or(DataType::Text))
    .collect()

// After — scan all rows for first non-NULL type per column
(0..col_count).map(|col_idx| {
    rows.iter()
        .find_map(|row| row.values.get(col_idx).and_then(|v| v.data_type()))
        .unwrap_or(DataType::Text)
        // INTENTIONAL: all-NULL column defaults to Text (PostgreSQL-compatible)
}).collect()
```

**Sites:**

| File | Lines |
|---|---|
| `executor/cte.rs` | 70, 86, 196, 209 |
| `executor/table_utils.rs` | 681, 754, 772, 821, 989, 1004 |
| `executor/core/query_exec.rs` | 832 |
| `executor/select/join/lateral.rs` | 196 |

### 4.7 Remaining type inference sites

| File:Line | Fix |
|---|---|
| `executor/user_function.rs:289` | Propagate unknown type as error when no RETURNS clause |
| `executor/select/join/operator_tree/mod.rs:1009` | Propagate from `infer_expr_type` (now returns Result) |
| `ddl.rs:839` | Already fixed in §4.1 (`sql_datatype_to_internal_strict()?`) |

### 4.8 INTENTIONAL markers (9 sites)

| # | File:Line | Comment |
|---|---|---|
| 1 | `types/mod.rs:279` | `// INTENTIONAL: empty array defaults element type to Text (PG-compatible)` |
| 2 | `protocol/handler/mod.rs:1192` | `// INTENTIONAL: wire protocol encoding — Text OID is universally safe` |
| 3 | `protocol/handler/mod.rs:1502` | same |
| 4 | `protocol/handler/mod.rs:1508` | same |
| 5 | `protocol/handler/encode/result.rs:80` | same |
| 6 | `types/infer.rs:219` | `// INTENTIONAL: conservative default for unhandled expression types` |
| 7 | `types/infer.rs:300` | `// INTENTIONAL: registry doesn't include UDFs — Text is safe fallback` |
| 8 | `registry.rs:190` | `// INTENTIONAL: unreachable after arg validation (min_args=1)` |
| 9 | `registry.rs:865` | `// INTENTIONAL: non-array input to UNNEST — best-effort type inference` |

---

## 5. PR 3: CI Lint

**File:** `scripts/lint_error_masking.sh`

```bash
#!/bin/bash
set -euo pipefail

FAIL=0

# 1. unwrap_or(DataType::Text) without INTENTIONAL marker
if grep -rn 'unwrap_or(DataType::Text)' src/sql/ src/protocol/ src/types/ \
    | grep -v '// INTENTIONAL:' \
    | grep -v '_test\.rs\|/tests/' \
    | grep -v 'error_message\|err_msg\|format!(' ; then
    echo "FAIL: unwrap_or(DataType::Text) without INTENTIONAL marker"
    FAIL=1
fi

# 2. compare_values with unwrap_or
if grep -rn 'compare_values.*\.unwrap_or' src/ \
    | grep -v '_test\.rs\|/tests/' ; then
    echo "FAIL: compare_values() with unwrap_or — use ? or sort_by_fallible"
    FAIL=1
fi

# 3. parse().unwrap_or(0) in SQL engine
if grep -rn '\.parse.*\.unwrap_or(0' src/sql/ \
    | grep -v '// INTENTIONAL:' \
    | grep -v '_test\.rs\|/tests/' ; then
    echo "FAIL: parse().unwrap_or(0) without INTENTIONAL marker"
    FAIL=1
fi

if [ $FAIL -ne 0 ]; then
    echo ""
    echo "Error masking lint failed. See https://github.com/c4pt0r/tipg/issues/657"
    exit 1
fi

echo "Error masking lint passed."
```

---

## 6. Known Behavioral Regressions (Document in PR Descriptions)

1. `SELECT CASE 1 WHEN '1' THEN 'match' END` — works in PostgreSQL (implicit coercion), will error after this fix. Tracked under #652 (Type System Fragmentation).

2. Queries with mixed types in ORDER BY that previously returned rows in arbitrary order will now error.

3. `CREATE TABLE t (x unknowntype)` with truly unsupported type names will error instead of silently using Text.

---

## 7. Implementation Checklist

### PR 1: Comparison + Parse
- [ ] Add `NumericValueOutOfRange` to SqlError + SQLSTATE
- [ ] Add `sort_by_fallible` utility
- [ ] Delete dead NULLIF/GREATEST/LEAST match arms (expr/mod.rs:501-529)
- [ ] Fix 13 comparison sites with `?` (§3.3)
- [ ] Change `compare_order_by_values` → `Result<Ordering>` (§3.4)
- [ ] Fix 5 `compare_order_by_values` callers (§3.5)
- [ ] Fix 3 direct `compare_values` sort-closure sites (§3.6)
- [ ] Fix 6 parse sites (§3.7)
- [ ] Fix 2 numeric conversion sites (§3.8)
- [ ] New tests for each error path (§3.9)
- [ ] `cargo test` passes
- [ ] `cargo clippy` clean

### PR 2: Type Inference Cascade
- [ ] Merge `sql_datatype_to_internal` (§4.1)
- [ ] Merge `infer_expr_type` (§4.2)
- [ ] Fix function arg inference (§4.3)
- [ ] Fix registry SameAsArg/FirstNonNull (§4.4)
- [ ] Change Custom closure signature (§4.5)
- [ ] Fix CTE/VALUES scan-all-rows (§4.6)
- [ ] Fix remaining sites (§4.7)
- [ ] Add INTENTIONAL markers (§4.8)
- [ ] Update test assertions
- [ ] `cargo test` passes
- [ ] `cargo clippy` clean

### PR 3: CI Lint
- [ ] Add `scripts/lint_error_masking.sh`
- [ ] Integrate into CI pipeline
- [ ] Verify lint passes on current codebase
