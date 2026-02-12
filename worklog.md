# Worklog

## 2026-02-12 — Issue #651 Phase 1: Post-Commit Trigger Activation

### Problem
`mark_active()` was called **inside** the DML transaction (before commit). If the trigger worker scanned before the transaction committed, it found no events and could deactivate the keyspace, leaving committed events invisible.

### Fix
Deferred `mark_active()` to **after** successful commit using a `Mutex<HashSet<String>>` accumulator on `Executor`:

1. **`src/sql/executor/core/mod.rs`** — Added `pending_trigger_activations` field and three methods:
   - `schedule_trigger_activation(keyspace)` — accumulates during DML
   - `flush_trigger_activations()` — calls `mark_active` after commit
   - `clear_trigger_activations()` — discards on rollback

2. **`src/sql/trigger_worker.rs`** — Replaced `worker.mark_active(keyspace)` with `executor.schedule_trigger_activation(keyspace)` in `enqueue_after_triggers`

3. **`src/sql/executor/core/dispatch.rs`** — Added post-commit/rollback hooks at all 8 paths:
   - Observability + regular user explicit COMMIT (with failed-txn→rollback handling)
   - Observability + regular user explicit ROLLBACK
   - SET ROLE autocommit success/failure
   - Statement timeout abort
   - DML autocommit: observability rollback, success commit, failure rollback

### Design decisions
- Used `Executor` (not Session) because `enqueue_after_triggers` already takes `&Executor`
- `std::sync::Mutex` (not tokio) — lock held only for insert/drain/clear, never across await
- Kept idle grace + CAS mitigation as defense-in-depth

### Verification
- `cargo check` / `cargo clippy` — no new warnings
- `grep mark_active` — only called from `flush_trigger_activations` and internal trigger_worker methods

---

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

# Issue #657: Error Masking Audit

## PR 1: Comparison + Parse (branch: fix/657-error-masking-pr1-comparison-parse)
**Commits**: `cc3e2c6`, `c383b76` | **PR**: #664 (MERGED)

### Changes
- Added `NumericValueOutOfRange` to SqlError with SQLSTATE 22003
- Added `sort_by_fallible` utility for fallible sort closures
- Deleted dead NULLIF/GREATEST/LEAST match arms (expr/mod.rs:501-529)
- Fixed 13 comparison sites: `compare_values().unwrap_or()` → `?`
- Changed `compare_order_by_values` → `Result<Ordering>`
- Fixed 5 `compare_order_by_values` callers
- Fixed 3 direct `compare_values` sort-closure sites via `sort_by_fallible`
- Fixed 6 parse sites: `parse().unwrap_or(0)` → proper error propagation
- Fixed 2 numeric conversion sites
- Fixed ORDER BY tie-breaker (#666): replaced `unwrap_or(0)` with `match` that skips unorderable columns
- Added 10 new error path tests (jsonb/vector comparison errors, parse errors, overflow, coercion guard)

### Verification
- 1116 tests pass (1106 existing + 10 new), 19 files changed
- `cargo clippy` clean (no new warnings in changed files)
- Zero `compare_values().unwrap_or()` remaining in production code
- CI lint (PR 3) compatible

## PR 2: Type Inference Cascade (branch: fix/657-error-masking-pr2-type-inference-cascade)
**Commit**: `d32f42c`

### Changes
- Merged `sql_datatype_to_internal` return type → `Result<DataType>` (removed internal `unwrap_or`)
- Renamed `try_sql_datatype_to_internal` → `sql_datatype_to_internal_strict` (DDL validation path)
- Changed `infer_expr_type` return: `DataType` → `Result<DataType, TypeError>`
- Deleted redundant `try_infer_expr_type` and `infer_expr_type_join`
- Fixed function arg inference: `collect::<Result<Vec<_>, _>>()?`
- Fixed registry `SameAsArg`/`FirstNonNull`: `unwrap_or` → `?` (returns None)
- Added `infer_column_types_from_rows()` helper — scans ALL rows for first non-NULL type
- Replaced 7 first-row-only type inference blocks with `infer_column_types_from_rows()`
- Added 11 INTENTIONAL markers to document deliberate Text fallbacks
- Changed 4 function return types in operators.rs to propagate errors
- Changed `collect_window_funcs_in_expr` → `Result<()>` with `?` on ~15 recursive calls
- §4.5 (Custom closure signature) deferred — minimal benefit with existing INTENTIONAL markers

### Verification
- 1106 tests pass, 26 files changed, 371 insertions, 211 deletions

## PR 3: CI Lint (branch: fix/657-error-masking-pr3-ci-lint)
**Commit**: `a91b363`

### Changes
- Added `scripts/lint_error_masking.sh` — lint guard to prevent reintroduction of error masking
- Three rules:
  1. `unwrap_or(DataType::Text)` without `// INTENTIONAL:` marker (context-aware, 5-line lookback)
  2. `compare_values()` with `unwrap_or` (should use `?` or `sort_by_fallible`)
  3. `parse().unwrap_or(0)` without `// INTENTIONAL:` marker
- Context-aware: checks preceding lines for INTENTIONAL markers and error-construction context
- Excludes error-message construction sites (Err/anyhow/InvalidCast) — not error masking
- RFC design used single-line grep; updated to multi-line context check for robustness

### Verification
- Lint correctly fails on unpatched master (catches all 3 categories)
- Lint passes on combined PR 1 + PR 2 + PR 3 codebase

---

# Refactor: Ownership-Based Cache Lifecycle (#650 follow-up)

## 2026-02-12

### Objective
Move process-global caches (`COMPILED_BODY_CACHE`, `TABLE_STATS`) from `static LazyLock<DashMap<String, HashMap<K, V>>>` into per-tenant `DashMap<K, V>` owned by `TenantEntry`. When the reaper drops a `TenantEntry`, its caches are dropped automatically via Rust's `Drop`. No manual eviction API needed.

### Context
- Issue #650 identified process-global caches as cross-tenant leakage risk
- PR #660 fixed isolation by adding keyspace as outer key + manual evict APIs
- This refactor: structural improvement — move caches into per-tenant ownership

### Plan
- New structs: `TriggerBodyCache`, `TableStatsCache`
- Ownership chain: `TenantEntry` → `Arc<TriggerBodyCache>` + `Arc<TableStatsCache>`
- ~10 files affected
- Delete global statics + eviction APIs

### Implementation (Local)

**Files changed (10):**

1. `src/sql/stats.rs` — Replaced `static TABLE_STATS: LazyLock<DashMap<String, HashMap<...>>>` with `TableStatsCache` struct containing `DashMap<(u64, u64), usize>`. Methods: `update_estimate`, `bump_estimate`, `get_estimate`. Deleted `evict_keyspace_stats()`. Tests rewritten to use instances.

2. `src/sql/triggers.rs` — Replaced `static COMPILED_BODY_CACHE: LazyLock<DashMap<String, HashMap<...>>>` with `TriggerBodyCache` struct containing `DashMap<(u64, u32), CompiledTriggerBody>`. Changed `prefetch_trigger_functions`, `apply_before_triggers_with_cache`, `execute_trigger_function`, `execute_trigger_body_cached` to take `&TriggerBodyCache` instead of `keyspace: &str`. Deleted `evict_compiled_bodies()`. Tests rewritten to use instances.

3. `src/sql/mod.rs` — Changed `mod triggers` to `pub(crate) mod triggers` for cross-module visibility.

4. `src/pool.rs` — Added `trigger_cache: Arc<TriggerBodyCache>` and `stats_cache: Arc<TableStatsCache>` to `TenantEntry`. Created in `TenantEntry::new()`. Added `trigger_cache()` and `stats_cache()` accessors to `TenantHandle`. Updated test helper.

5. `src/sql/executor/core/mod.rs` — Added `trigger_cache: Arc<TriggerBodyCache>` and `stats_cache: Arc<TableStatsCache>` to `Executor`. Extended `Executor::new()`. Added accessor methods.

6. `src/protocol/handler/dynamic.rs` — Extracts caches from `TenantHandle` and passes to `Executor::new()`. Non-pool path creates fresh cache instances.

7. `src/sql/trigger_worker.rs` — Changed `process_keyspace()` from `pool.get_client()` to `pool.acquire()` for cache access + reaper protection. Passes caches to `Executor::new()`.

8. `src/sql/executor/dml.rs` — 7 call sites updated: 3× `prefetch_trigger_functions` → `self.trigger_cache()`, 3× `apply_before_triggers_with_cache` → `self.trigger_cache()`, 1× `bump_row_count_estimate` → `self.stats_cache().bump_estimate()`.

9. `src/sql/executor/core/scan.rs` — `update_row_count_estimate` → `self.stats_cache().update_estimate()`.

10. `src/sql/executor/operators.rs` + `src/sql/executor/select/mod.rs` — 3× `get_row_count_estimate` → `self.stats_cache().get_estimate()`.

**What got deleted:**
- `static COMPILED_BODY_CACHE` (global)
- `static TABLE_STATS` (global)
- `evict_compiled_bodies()` function
- `evict_keyspace_stats()` function
- All `#[allow(dead_code)]` annotations for eviction APIs
- `keyspace: &str` parameter from 5 trigger functions

### Verification
- `cargo test -p pg-tikv`: **1116 passed**, 0 failed
- `cargo clippy -p pg-tikv`: no new warnings (only pre-existing vendored tikv-client warnings)
- `cargo fmt -p pg-tikv --check`: clean

---

# Issue #662: Stale Trigger Body Cache After CREATE OR REPLACE FUNCTION

## 2026-02-12

### Problem
After `CREATE OR REPLACE FUNCTION` modifies a trigger function body, the `TriggerBodyCache` still serves the old compiled body.

### Design Decision
DDL-side invalidation (cold path) over per-execution body hash checks (hot path). DDL is rare, trigger execution is frequent. Appropriate for greenfield project.

### Implementation
- Added `TriggerBodyCache::invalidate_db(db_id)` in `src/sql/triggers.rs`
- Wired into `execute_create_function_cmd` (OR REPLACE) and `execute_drop_function_cmd` in `src/sql/executor/triggers.rs`

### PR
- PR #671: `fix/662-stale-trigger-body-cache` — squashed to single commit

---

# PR #583 Review + Merge: Trigger Worker Race Condition (#457)

## 2026-02-12

### Changes Reviewed
- CAS-style `remove_active_if_unchanged()` prevents clobbering concurrent `mark_active()` calls
- Consolidated `DashSet + DashMap` into single `DashMap<String, Instant>`
- Configurable `idle_grace_ms` (default: `max(10 * poll_interval, 1000ms)`)

### Merge Work
- Rebased onto current master (including #650 cache refactor + #667/#668 binder changes)
- Fixed pre-existing `cargo fmt` issues from #668 merge (5 files: binder/mod.rs, binder/tests.rs, ddl.rs, tikv_store/mod.rs, tikv_store/views.rs)
- CI all green, squash-merged

### Verification
- All CI checks pass: lint, test, gorm-smoke, regression-gate, sqlalchemy-smoke

---

# PR #571 Review + Merge: Quarantine Corrupt Trigger Queue Entries (#22)

## 2026-02-12

### Changes Reviewed
- `TriggerQueueTxn` trait for testable transaction abstraction
- `quarantine_corrupt_trigger_queue_entry` — moves corrupt entries to DLQ with diagnostics (base64 preview, sha256, decode error)
- `ClaimEventsResult` — tracks quarantine count for proper commit/rollback in `process_keyspace()`
- `MemTxn` test mock + 2 thorough tests

### Merge Work
- Rebased onto latest master (including #583 CAS race fix + #650 cache refactor)
- Resolved 3 conflicts: imports (kept `DashMap` + added `async_trait`), 2 × error handler (quarantine replaces error log)
- `cargo check` clean, CI all green, squash-merged

### Verification
- All 7 CI checks pass

---

# Issue #652 PR 1 — Unified Cast Function with CastContext

## 2026-02-12

### Objective
Unify `cast_value_to_type()` (explicit casts) and `coerce_value_for_column()` (assignment coercion) into a single `cast(value, target, context)` function with a `CastContext` enum.

### Files
- NEW: `src/sql/types/cast.rs` — CastContext enum + unified `cast()` function
- MODIFY: `src/sql/types/mod.rs` — register module
- MODIFY: `src/sql/expr/mod.rs` — delegate to unified cast, remove moved code
- MODIFY: `src/sql/value_coercion.rs` — delegate to unified cast, remove moved code

### Key Decisions
- CastContext: Explicit, Assignment, Implicit (Implicit wired in follow-up PR)
- 7 divergence points: Float64→Int32, Float64→Int64, Numeric→Int32, Numeric→Int64, Numeric→Float64, Bool→Int32, Int/Float/Numeric→Bool, catch-all
- `cast_to_bytea()` made `pub(crate)` — used by both `cast()` and `cast_custom_type()`
- `compare_values()` deferred to follow-up PR

### Implementation (Local)

**Commit**: `8721ac2` on branch `feat/652-unified-cast-context`

**Files changed (4 modified, 1 created):**

1. `src/sql/types/cast.rs` (NEW, 460 lines) — `CastContext` enum, unified `cast()` function, `cast_to_bytea()` (pub(crate)), `round_half_away_from_zero()`, `value_is_compatible_with_column_type()`, 19 unit tests covering all 7 divergence points.

2. `src/sql/types/mod.rs` (+2 lines) — registered `cast` module, re-exported `CastContext`.

3. `src/sql/expr/mod.rs` (-186 lines) — Deleted `round_half_away_from_zero`, `cast_to_bytea`, `cast_value_to_type`. Delegated `cast_value()` to `cast(..., Explicit)`. Widened `parse_interval_string`/`parse_timestamp_string` to `pub(crate)`.

4. `src/sql/value_coercion.rs` (-322 lines) — Replaced entire `coerce_value_for_column` body with single-line delegation to `cast(..., Assignment)`. Deleted `value_is_compatible_with_column_type`.

### Verification
- `cargo check`: 0 errors, no new warnings
- `cargo test`: **1140 passed** (1121 existing + 19 new), 0 failed
- `cargo fmt` / `cargo clippy` / `lint_error_masking.sh`: all clean
