# Dead Code Detection — Worklog

## 2026-02-11

### Task
Systematic dead code detection across TiPG codebase (221 files, ~97K lines).

### Approach
7-phase analysis executed in parallel where possible:
1. Compiler `dead_code` lint — added `#![warn(dead_code)]` to main.rs, ran `cargo check`
2. Clippy dead-code-adjacent lints — `unnecessary_wraps`, `unused_self`, `redundant_clone`, `unused_async`
3. `#[allow(dead_code)]` audit — categorized all 74 annotations
4. Unreachable code patterns — `unreachable!()`, `todo!()`, `_ => {}`, dead-after-return
5. Unused dependencies — checked all 35 deps against actual usage
6. `pub` visibility over-exposure — counted pub vs pub(crate) items
7. Consolidated report

### Key Findings
- **3 compiler dead_code warnings** (unsuppressed) — gin.rs functions + FsBackend::exists()
- **74 `#[allow(dead_code)]` annotations** — 33 keep, 36 wire-in, 5 remove
- **402 clippy warnings** (project code) — 73 unnecessary_wraps, 19 unused_self, 11 redundant_clone
- **0 unused dependencies** — all 35 deps are actively used
- **618 over-exposed `pub` items** (73% of all visibility-annotated items)
- **~440 lines safely removable** without feature impact
- **~3,500 additional lines removable** pending roadmap decisions (type inference, RBAC, operator framework)

### Decisions
- Report produced as `dead_code_report.md` — categorized by phase with dispositions
- No code changes made in this pass (detection only)
- Recommended: tighten `pub` → `pub(crate)` as highest-leverage ongoing improvement

### Verification Deep-Dives (4 parallel investigations)

Verified every `#[allow(dead_code)]` item against actual call sites. Major corrections:

- **11 misleading annotations found** — items marked dead but actively used in production:
  - `GinTokens::iter_hashes()`, `into_scan_hashes()` — called in gin.rs scan path
  - `FsBackend::exists()` — implemented and tested
  - `list_materialized_views()` — called in drop_schema()
  - `replace_procedure()` — called in CREATE OR REPLACE PROCEDURE
  - `Executor::auth_manager()` — 6 call sites in default_privileges.rs
  - `VirtualTable::schema_name()` — 41 implementations
  - `find_keyword_outside_strings()` — called in parser.rs
  - `Session::session_user()` — called in dispatch.rs
  - `NestedLoopJoinOperator` — production join execution
  - `IndexEqScanOperator::new_with_scan_limit()` — production planner

- **RBAC module: 60% integrated** — DDL/auth/metadata fully wired, enforcement layer (`check_privilege()`) completely missing
- **Type inference: 90% integrated** — core module in production (62 call sites), only convenience APIs unwired
- **Operator framework: 70% integrated** — joins + scans active, EXPLAIN introspection methods test-only
- **Truly dead code reduced to ~433 lines** (down from initial ~440 estimate, but with different items)
- **Truly abandoned code: ~335 lines** (OperatorBuilder + extract_tsquery_gin_tokens)

### Deliverable
- `dead_code_report.md` — verified categorized report with corrected dispositions

---

# Issue #633: COPY TO STDOUT FORMAT binary silently treated as text; option chars lossy-cast

## 2026-02-11

### Root Cause Analysis

**Problem 1: Unsupported FORMAT silently ignored**
- Location: `src/protocol/copy_format.rs:48-61` (`CopyOptions::from_copy_options`)
- The FORMAT option only checks for `"CSV"` and sets `CopyFormat::Csv`. All other values — including `"BINARY"`, `"FOO"` — fall through silently and keep the default `CopyFormat::Text`.
- PostgreSQL behavior: `COPY ... WITH (FORMAT binary)` uses a completely different binary wire protocol. Silently downgrading to text breaks clients expecting binary frames.

**Problem 2: Delimiter/quote/escape chars lossy-cast via `as u8`**
- Location: `src/protocol/copy_format.rs:62-75`
- `char as u8` truncates to the low byte of the Unicode scalar value (e.g., `'€'` U+20AC → `0xAC`).
- PostgreSQL requires these to be single-byte characters and rejects multi-byte.

### Fix Plan
1. Change `from_copy_options` return type to `Result<Self, String>` for validation errors.
2. Validate FORMAT: accept TEXT/CSV, reject BINARY (not supported), reject unknown.
3. Validate delimiter/quote/escape: require `is_ascii()`, reject non-ASCII.
4. Update caller at `dynamic.rs:619` to propagate error via ErrorInfo.
5. Add tests for new validation paths.

### Error Codes (PostgreSQL-compatible)
- `0A000` (feature_not_supported) for all COPY option errors (matches PostgreSQL)

---

# Issues #630, #631, #632: COPY format encoding fixes

## 2026-02-11

### #630 (P0): CSV NULL vs empty string conflation
- **Root cause**: `encode_csv_value` only quoted values containing delimiter/quote/newline/CR. Empty string and custom NULL sentinel were never force-quoted.
- **Fix**: Added `tmp == opts.null_string.as_bytes()` to `needs_quote` condition in `encode_csv_value`.
- **Also fixed**: `encode_value_raw` for `Value::Bytes` — was falling through to `encode_value` which emits `\\x` (text-mode double escaping). Now emits `\x` directly.

### #631 (P1): Text mode custom delimiter/NULL not round-trippable
- **Root cause 1**: `escape_text` was hardcoded to escape only TAB/newline/CR/backslash. Custom delimiters (e.g. `|`) in values were not escaped.
- **Root cause 2**: Non-NULL values matching null_string were output identically to NULL.
- **Fix 1**: Added `delimiter` parameter to `escape_text` and `encode_value`. Custom delimiter byte is now escaped with backslash prefix.
- **Fix 2**: Post-encoding check in text mode: if encoded bytes match null_string, insert backslash before first byte. On import, `\X` unescapes to `X`, recovering the original value.

### #632 (P1): HEADER row bypasses format encoding
- **Root cause**: `handle_copy_to_stdout` emitted header by raw-concatenating column names with delimiter. Column names with delimiter/quote/newline produced invalid output.
- **Fix**: Wrap column names as `Value::Text` and route through `encode_row_with_options`.

### Verification
- 1064 tests pass (27 copy_format tests including 16 new). Zero regressions.

---

# Issue #657: Error Masking Audit — RFC Design

## 2026-02-11

### Task
Design the comprehensive solution for fixing 62+ high-risk error-masking sites (issue #657).

### Approach
- Read issues #657, #656, #649 for full context
- Deep codebase audit: read all files referenced in the issue + grep for actual site counts
- Verified every site count against actual code (found discrepancies)

### Key Findings
- **Issue's comparison site count is wrong**: Found 20 `compare_values().unwrap_or()` sites (issue claims 8). Missed: IN list (evaluator.rs:547), BETWEEN (evaluator.rs:575-576), CASE WHEN simple form (evaluator.rs:656), JSON containment (evaluator.rs:1111), array overlap (operators.rs:187, mod.rs:1913,1927), array functions (array.rs:99,156), duplicate NULLIF/GREATEST/LEAST in mod.rs
- **`sql_datatype_to_internal` cascade is tiny**: Only 2 callers in infer.rs:102,103
- **`infer_expr_type` cascade is ~35 callers**: via projection.rs:104 wrapper
- **Duplicate implementations**: NULLIF/GREATEST/LEAST in both expr/mod.rs and functions/misc.rs

### Design Decisions
1. Compiler-driven migration (change return types → compiler finds all sites)
2. `Result<DataType, TypeError>` for type inference (preserves error context)
3. `sort_by_fallible` utility for sort closures
4. Scan all rows (not just first) for CTE/VALUES type inference
5. 9 INTENTIONAL sites documented and exempted from CI lint
6. 3 PRs: comparison+parse → type-inference cascade → CI lint

### Review Corrections (from https://github.com/c4pt0r/tipg/issues/657#issuecomment-3886963477)
1. `infer_expr_type` callers: ~35 → ~46 (missed 11 test callers in projection.rs)
2. 3 "phantom" sort-closure sites were `compare_values` direct, not `compare_order_by_values`
3. evaluator.rs:1111 is JSON IN LIST, not @> containment
4. NULLIF/GREATEST/LEAST match arms in expr/mod.rs:501-529 are dead code (registry intercepts) → delete
5. Custom closure signature → `Result<DataType, TypeError>` in PR 2

### Deliverable
- RFC posted: https://github.com/c4pt0r/tipg/issues/657#issuecomment-3886889639
- Corrections posted: https://github.com/c4pt0r/tipg/issues/657#issuecomment-3886986649

---

# Issue #657: PR 1 Implementation — Comparison + Parse

## 2026-02-11

### Changes Made (local implementation)

**New error variant (`error.rs`):**
- Added `NumericValueOutOfRange { detail: String }` with SQLSTATE 22003

**New utility (`operators.rs`):**
- Added `sort_by_fallible<T>()` — fallible sort using first-error-capture pattern

**Dead code deletion (`expr/mod.rs`):**
- Deleted NULLIF/GREATEST/LEAST match arms (lines 501-529) — dead code behind registry intercept

**13 comparison sites fixed with `?`:**
- `functions/misc.rs`: NULLIF (line 24), GREATEST (line 36), LEAST (line 48)
- `functions/array.rs`: array_position (line 99), array_remove (lines 154-157 rewritten as loop)
- `evaluator.rs`: IN list (547), BETWEEN (575-576), CASE WHEN (656), JSON IN LIST (1111)
- `operators.rs`: PGOverlap (187)
- `expr/mod.rs`: @> (1913), <@ (1927) — rewritten as explicit loops for error propagation

**`compare_order_by_values` signature change (`operators.rs:934`):**
- Changed return type from `Ordering` to `Result<Ordering>`
- Updated re-export in `expr/mod.rs`

**5 `compare_order_by_values` framework callers:**
- `operators/sort.rs`: `compare_keys()` → `Result<Ordering>`, `open()` uses `sort_by_fallible`
- `operators/window.rs`: `order_by_values_are_peers()` → `Result<bool>`, sort uses `sort_by_fallible`
- `operators/aggregate.rs`: sort uses `sort_by_fallible`
- `executor/select/join/lateral.rs`: sort uses `sort_by_fallible`

**3 direct `compare_values` sort-closure sites:**
- `executor/operators.rs:3214`: wrapped with `sort_by_fallible`
- `executor/select/order.rs:184,206`: wrapped with `sort_by_fallible`
- `apply_order_by_for_aggregate` return type changed to `Result<Vec<Row>>` (5 callers updated)

**6 parse sites fixed:**
- `expr/mod.rs:260`: interval parse → `InvalidInputSyntax { type_name: "integer" }`
- `expr/mod.rs:595,598`: FORMAT arg_pos/width parse → `InvalidInputSyntax`
- `expr/mod.rs:619`: FORMAT width_digits parse → `InvalidInputSyntax`
- `expr/mod.rs:876`: FORMAT_TYPE OID parse → `InvalidInputSyntax { type_name: "oid" }`
- `functions/pg_compat.rs:96`: format_type OID parse → `InvalidInputSyntax { type_name: "oid" }`
- `executor/core/query_exec.rs:224`: pg_sleep seconds parse → `InvalidInputSyntax { type_name: "double precision" }`

**2 numeric conversion sites fixed (`numeric.rs`):**
- `to_int64()` → `Result<i64>` with `NumericValueOutOfRange`
- `to_decimal()` → `Result<Decimal>` with `NumericValueOutOfRange`

**Module visibility:**
- `expr/mod.rs`: changed `mod operators` → `pub(crate) mod operators` for `sort_by_fallible` access

**Test updates:**
- `expr/tests.rs`: Added `.unwrap()` to `compare_order_by_values` test calls
- `operators/sort.rs`: Added `.unwrap()` to `compare_keys` test calls

### Verification
- `cargo check`: compiles clean (0 errors)
- `cargo test`: 1106 passed, 0 failed, 0 ignored
- `cargo clippy`: no new warnings (299 pre-existing)
- Lint grep: 0 remaining `compare_values().unwrap_or()`, 0 remaining `parse().unwrap_or(0)` in target scope

---

# Issue #657: PR 2 Implementation — Type Inference Cascade

## 2026-02-11

### Changes Made (local implementation)

**§4.1: `sql_datatype_to_internal` returns Result (`mapping.rs`):**
- Changed `sql_datatype_to_internal` from infallible `-> DataType` to `-> Result<DataType>` (was wrapping `unwrap_or(DataType::Text)`)
- Renamed `try_sql_datatype_to_internal` → `sql_datatype_to_internal_strict` (DDL validation path)
- Updated re-export in `types/mod.rs`

**§4.2: `infer_expr_type` returns Result (`types/mod.rs`, `projection.rs`):**
- Changed `infer_expr_type` return type from `DataType` to `Result<DataType, TypeError>`
- Deleted `try_infer_expr_type` and `infer_expr_type_join` (consolidated into single fallible API)
- Changed `projection.rs::infer_expr_type` wrapper to return `Result<DataType, TypeError>`

**§4.3: Function arg inference (`infer.rs`):**
- Changed arg type collection from `filter_map` with `unwrap_or(DataType::Text)` to `collect::<Result<Vec<_>, _>>()?`

**§4.4: Registry SameAsArg/FirstNonNull (`registry.rs`):**
- Changed `args.first().cloned().unwrap_or(DataType::Text)` to `args.first().cloned()?` (returns `None`)
- Changed `args.iter().find(...).cloned().unwrap_or(DataType::Text)` to `args.iter().find(...).cloned()?`

**Compiler-driven cascade — 37+ callers fixed:**

| File | Sites | Pattern |
|---|---|---|
| `ddl.rs` | 5 | Renamed import + call sites; error-masking `.unwrap_or()` → `collect::<Result>` |
| `udt.rs` | 2 | Renamed import + call site |
| `settings_tableless.rs` | 1 | Renamed call |
| `index_helpers.rs` | 1 | Renamed call |
| `evaluator.rs` | 4 | Renamed `try_infer_expr_type` → `infer_expr_type` |
| `expr/mod.rs` | 1 | Renamed `try_sql_datatype_to_internal` |
| `executor/operators.rs` | 22 | `?` on all infer calls; 3 functions changed return types |
| `dml.rs` | 2 | Added `?` to infer calls |
| `query_exec.rs` | 2 | `matches!()` pattern → `Ok()` wrapping; scan-all-rows |
| `join/aggregate.rs` | 5 | Added `?` to infer calls |
| `join/lateral.rs` | 2 | Rewrote `.map()` closure to for loop; scan-all-rows |
| `join/window.rs` | 1 | Added `?` to caller |
| `operator_tree/projection.rs` | 9 | Added `?`; rewrote `.map()` closure to for loop |
| `projection.rs` (tests) | 10 | Wrapped assertions in `Ok()` |

**Function return type changes in executor/operators.rs:**
- `add_pg_get_indexdef_support_to_group_by` → `Result<()>`
- `extract_group_by_info` → `Result<(Vec<Expr>, Vec<String>, Vec<DataType>)>`
- `infer_window_func_type` → `Result<DataType>`
- `extract_window_function_exprs` → `Result<(Vec<WindowFunctionExpr>, HashMap<String, String>)>`
- `collect_window_funcs_in_expr` → `Result<()>` (recursive, ~15 call sites updated)

**§4.6: CTE/VALUES scan-all-rows (7 scan sites + 4 index guards):**
- Added `infer_column_types_from_rows()` helper to `types/mod.rs` — scans all rows for first non-NULL type per column
- Replaced first-row-only type inference in: `cte.rs` (2), `table_utils.rs` (2), `query_exec.rs` (1), `lateral.rs` (1), `result.rs` (1)
- Added INTENTIONAL markers to 4 index-guard `.get(idx).cloned().unwrap_or()` sites

**§4.7: Remaining type inference sites:**
- `user_function.rs:289` — INTENTIONAL (no RETURNS clause defaults to Text)
- `operator_tree/mod.rs:1009` — INTENTIONAL (all-NULL subquery column defaults to Text)

**§4.8: INTENTIONAL markers (11 sites total):**
- `types/mod.rs:279` — empty array element type
- `protocol/handler/mod.rs:1192,1502,1508` — wire protocol encoding
- `protocol/handler/encode/result.rs:80` — wire protocol encoding
- `types/infer.rs:221` — unhandled expression default
- `types/infer.rs:300` — unknown function default
- `registry.rs:190` — ARRAY_AGG arg guard
- `registry.rs:865` — UNNEST non-array input
- `user_function.rs:289` — no RETURNS clause
- `operator_tree/mod.rs:1009` — all-NULL subquery column

**§4.5: Custom closure signature — deferred:**
- Custom closures in registry.rs already have INTENTIONAL markers
- Changing `fn(&[DataType]) -> DataType` to `fn(&[DataType]) -> Result<DataType, TypeError>` would be a larger API change with minimal benefit since the sites are already documented

### Verification
- `cargo check`: compiles clean (0 errors)
- `cargo test`: 1106 passed, 0 failed, 0 ignored
- `cargo clippy`: no new warnings
