# Worklog

## 2026-02-21: Statement-Level FK NO ACTION/RESTRICT Deferral for DELETE

### Problem
pg-tikv treated `NO ACTION` (default FK action) and `RESTRICT` identically — both checked per-row during deletion and failed immediately if any referencing row existed. PostgreSQL differentiates: both are checked at statement end for non-deferrable constraints, meaning `DELETE FROM self_ref_table` (all rows) succeeds when all referencing rows are also deleted by the same statement. This caused 179 ORM test failures across 5 `advanced.test.ts` suites where `beforeEach` cleanup ran `DELETE FROM adv_employees` on a table with self-referential `manager_id` FK.

### Solution: Two-Phase DELETE with HashSet Statement-Level Check
1. **Two-phase DELETE** in `executor/dml_analyzed/delete.rs`: Phase 1 collects all rows matching WHERE, computes `stmt_deleting_pks` as a `HashSet<String>` (PKs serialized via `pk_to_hash_key` for O(1) lookup). Phase 2 executes deletions with the set threaded through.
2. **Separate statement-level check** in `cascade_delete_recursive`: `stmt_deleting_pks` is passed as an immutable `&HashSet<String>` alongside the existing mutable `deleted_pks`. The row loop checks `stmt_deleting_pks.contains()` (O(1)) to skip referencing rows being deleted by the same statement. `deleted_pks` retains its original per-row seeding for cascade cycle prevention.
3. This handles both NO ACTION and RESTRICT correctly — no match-arm splitting needed.

### Key Design Decision
User correction: PostgreSQL 17.7 validates that RESTRICT also allows bulk self-ref delete when all referencing rows are in the same statement. The statement-level HashSet check handles both uniformly.

### Files Modified
| File | Change |
|------|--------|
| `src/sql/executor/dml_analyzed/delete.rs` | Two-phase restructure: collect → compute PK HashSet → execute |
| `src/sql/dml/delete.rs` | Add `stmt_deleting_pks: &HashSet<String>` param to `execute_delete_row` |
| `src/sql/dml/foreign_keys.rs` | Add `pk_to_hash_key` helper; thread `stmt_deleting_pks` + `stmt_target_table` through cascade chain; O(1) skip check in row loop |
| `tests/100_fk_null_and_unique_ref.sql` | 4 new test cases (tests 24-27) |
| `tests/100_fk_null_and_unique_ref.expected` | Expected output validated against PG 17.7 |

### Verification
- `cargo build` — clean compile
- `cargo test` — all 1924 tests pass
- All `.expected` output validated against PostgreSQL 17.7 via `sudo -u postgres psql`

---

## 2026-02-21: Issue #951 — JSONB Binary Parameter Validation

### Problem
The binary JSONB parameter decode path (`src/protocol/handler/params/decode.rs:143-158`) accepted any UTF-8 bytes after stripping the version byte, without validating that the content is valid JSON. For example, `\x01not json at all` was silently accepted as `Value::Jsonb("not json at all")`. The text JSONB decode path already validated via `serde_json::from_str()`.

### Solution
Added `serde_json::from_str()` validation + `parsed.to_string()` canonicalization to the binary JSONB decode path, mirroring the text path exactly. Invalid JSON now returns SQLSTATE `22P02` via the existing `invalid_param()` error wrapper.

### Files Modified
| File | Change |
|------|--------|
| `src/protocol/handler/params/decode.rs` | Add serde_json parse + roundtrip (2 lines added) |
| `src/protocol/handler/tests.rs` | Add 3 tests: invalid JSON rejection, canonicalization, empty body after version byte |

### Verification
- `cargo build` — clean compile
- `cargo test test_decode_parameters_jsonb_binary` — all 5 tests pass (2 existing + 3 new)

---

## 2026-02-21: Issue #281 — JSON/JSONB Canonicalization

### Problem
TiPG's JSONB output was missing PostgreSQL's canonical formatting (spaces after `:` and `,`). The wire text encoder (`write_jsonb_pg()`) in `encode/value.rs` correctly handled formatting, but multiple other output paths (binary wire, COPY, cast to text/json, ALTER TYPE) bypassed it and emitted raw compact stored strings. Additionally, `JSONB_AGG` returned `Value::Json` instead of `Value::Jsonb`.

### Solution: Output-Boundary Canonicalization
Extracted the existing `write_jsonb_pg()` logic into a reusable `format_jsonb_pg_str()` helper in `src/sql/jsonb.rs`, then applied it at ALL output boundaries:

1. **`src/sql/jsonb.rs`** — Added `format_jsonb_pg()`, `format_jsonb_pg_str()`, `write_jsonb_pg()` + 8 unit tests
2. **`src/sql/mod.rs`** — Changed `mod jsonb` → `pub(crate) mod jsonb` for cross-module access
3. **`src/protocol/handler/encode/value.rs`** — Text: replaced inline formatter with shared helper. Binary: canonicalize before sending.
4. **`src/protocol/copy_format.rs`** — Split `Value::Json | Value::Jsonb` in both text and CSV paths
5. **`src/sql/types/cast/mod.rs`** — Added JSONB→text cast, fixed JSONB→JSON cast
6. **`src/sql/ddl/mod.rs`** — Fixed ALTER TYPE JSONB→text coercion path
7. **`src/sql/aggregate.rs`** — Added `JsonbAgg` variant, fixed `value_to_json_str` for `Value::Jsonb`

### What Did NOT Change
- Internal stored format: `Value::Jsonb(String)` still holds compact `serde_json::to_string()` output
- Hash/equality/group-by paths: unchanged (use raw stored string)
- JSON type: fully preserved as-is
- `Display for Value`: NOT changed (avoids cross-layer coupling)

### Verification
- `cargo build` — clean compile
- `cargo test` — all 1917 tests pass (0 failures)
- 17 new regression tests added across 4 files

---

## 2026-02-21: Issue #906 Follow-ups (3 Commits)

### Commit 1: refactor(executor): extract shared runtime helpers from dispatch paths
- Added 4 helpers to `dispatch/utils.rs`: `RuntimeSettings`, `wrap_with_runtime_context`, `apply_statement_timeout`, `autocommit_backoff`
- Updated `dispatch/prepared.rs` and `dispatch/mod.rs` to use helpers (~-40 lines dedup)
- No behavior change, pure internal cleanup

### Commit 2: perf(executor): use Cow<AnalyzedQuery> to avoid clone for simple prepared queries
- Changed `execute_via_optimizer` to take `Cow<'a, AnalyzedQuery>` instead of `mut AnalyzedQuery`
- Added `query_needs_pre_materialization` guard to skip cloning for Cow::Borrowed when no mutation needed
- Updated 3 call sites: `try_execute_analyzed` (Cow::Owned), `execute_subquery` (Cow::Borrowed), `prepared.rs` (Cow::Borrowed)
- Simple prepared SELECT and subqueries: **0 clones** (was 1 deep clone each)

### Commit 3: test(prepared): add extended-protocol schema-drift and metadata e2e tests
- New file: `orm-tests/pg-client/prepared-metadata.test.ts` (~210 lines, 7 test cases)
- Tests: column metadata OIDs, re-execute with different params, multi-param types, INSERT RETURNING metadata
- Schema drift tests: ADD COLUMN, DROP referenced column, DROP TABLE after prepare

### Verification
- `cargo check` — clean compile
- `cargo clippy` — no new warnings
- `cargo test` — all 1839 tests pass

---

## 2026-02-21: Structural Fix for Extended-Protocol Worker Stack Overflow (#907)

### Problem
tokio-runtime-worker hits stack overflow when executing prepared statements via
the extended protocol. GDB shows the `do_query` closure requests ~198 MiB on an
8 MiB worker stack. Root cause: Rust async compiler monomorphizes deeply nested
async call chains into enormous future state machines.

Critical path: `do_query → execute_prepared → execute_via_optimizer` (275-line
`async fn` with ~8 `.await` points holding large types across await boundaries).

### Solution
Convert three key `async fn` into functions returning `Pin<Box<dyn Future>>`:
1. `execute_via_optimizer` — the largest async state machine (275 lines, ~8 awaits)
2. `execute_subquery` — seals the recursion boundary
3. `try_execute_analyzed` — entry point from simple-query path

This is idiomatic Rust for recursive/large async functions, already used in 13+
places in this codebase (e.g. `build_cte_context_from_analyzed_with_base`).

### User Feedback Incorporated
1. Signature guards placed OUTSIDE `#[cfg(test)]` — checked by `cargo build`
2. Smoke test should run with `PGTIKV_TOKIO_STACK_MB=8` for deterministic signal
3. Performance: at least one `Box` per boundary + additional per subquery recursion
4. `main.rs` comment: "keeps bounded" not "is sufficient" (avoids absolute certainty)

### Files Modified
| File | Change |
|------|--------|
| `src/sql/executor/select/analyzed/mod.rs` | Convert 3 `async fn` → `fn -> Pin<Box<...>>`, add signature guards |
| `scripts/extended_protocol_smoke.py` | Add #907 regression test cases |
| `src/main.rs` | Update comment |

### Verification
- `cargo build` — compiles cleanly (signature guards pass type-check)
- `cargo test` — 1826 tests pass, 0 failures
- No semantic changes to SQL execution
- All callers unchanged (`.await` on `Pin<Box<dyn Future>>` is identical)

---

## 2026-02-20: Canonical TypedExpr Visitor/Transform API (#870)

### Summary
Extracted canonical tree-traversal primitives for `TypedExprKind` (28 variants) and migrated 12 independent walkers to use them, including the largest manual walker (`pre_materialize_async_exprs`, ~1,100 LOC). Fixed correlated-detection blind spot for nested subquery payloads.

### What Changed

**New file: `src/sql/expr/traverse.rs` (~600 LOC with tests)**
- `for_each_child`: yields references to each immediate TypedExpr child (read-only, left-to-right)
- `map_children`: transforms each immediate TypedExpr child, returns rebuilt `TypedExprKind`
- `visit_any`: stack-safe iterative predicate test (left-to-right DFS)
- `transform_bottom_up`: recursive bottom-up sync transform
- `AsyncExprTransform` trait + `map_children_async`: async child recursion for `&mut self` transforms
- 9 unit tests covering round-trip, traversal order, deep tree stack safety, subquery boundary

**Migrated walkers:**

| File | Function | Before LOC | After LOC |
|------|----------|-----------|-----------|
| `analyzer/types.rs` | `reindex_typed_expr` | 167 | 18 |
| `expr/typed_fold.rs` | `fold_typed_expr` | 238 | 72 (Case arm preserved) |
| `expr/typed_visit.rs` | `expr_any` | 131 | 6 (thin wrapper) |
| `expr/classify.rs` | `expr_any_iter` + `push_expr_children` | 158 | 0 (deleted, replaced by `visit_any`) |
| `expr/typed_rewrite.rs` | `SequenceMaterializeCtx::rewrite_expr` | 280 | 120 (via `AsyncExprTransform`) |
| `executor/select/analyzed/subquery.rs` | `has_outer_ref` | 60 | 5 (via `visit_any`) |
| `executor/select/analyzed/subquery.rs` | `substitute_outer_refs_in_expr` | 310 | 90 (explicit subquery arms + `map_children`) |
| `executor/select/analyzed/rewrite.rs` | `contains_aggregate` | 32 | 3 (via `visit_any`) |
| `executor/select/analyzed/rewrite.rs` | `collect_aggregates_from_expr` | 80 | 30 (via `for_each_child`) |
| `optimizer/window_rewrite.rs` | `contains_window` | 58 | 3 (via `visit_any`) |
| `optimizer/rewrite.rs` | `collect_column_indices_inner` | 128 | 10 (via `for_each_child`) |
| `executor/select/analyzed/mod.rs` | `pre_materialize_async_exprs` | 1,100 | 280 (via `AsyncExprTransform`) |
| `executor/select/analyzed/subquery.rs` | `is_correlated_query` + helpers | 74 | 90 (depth-parameterized, fixes blind spot) |

### Semantic Changes (Correctness Fixes)

1. **`has_outer_ref`** — now correctly traverses:
   - `escape` field in Like/SimilarTo
   - `order_by` exprs in FunctionCall/AggregateCall
   - `path` field in JsonAccess
   - `window_frame` bounds in WindowCall
   - **Nested subquery payloads** — descends into expression-level subqueries (ScalarSubquery, Exists, InSubquery, AnyAll, ArraySubquery) with incremented depth threshold to detect transitively-correlated references

2. **`substitute_outer_refs_in_expr`** — old catch-all `_ => expr.clone()` skipped recursion into SimilarTo, WindowCall, MinMax, Row, ArrayLiteral. These are now correctly substituted via `map_children`.

3. **`reindex_typed_expr`** — old catch-all skipped WindowCall, SimilarTo, Row, ArrayLiteral. These are now correctly reindexed via `map_children`.

4. **`is_correlated_query`** — now detects transitively-correlated subqueries. Previously, a query like `SELECT 1 FROM t2 WHERE t2.x = (SELECT t1.y FROM t3)` would be misclassified as uncorrelated because `has_outer_ref` treated the ScalarSubquery as an opaque leaf. Now the depth-parameterized `has_outer_ref_beyond` descends into the subquery payload and detects `scope_depth > min_depth+1`.

### Design Decisions

- `for_each_child` (borrows) vs `map_children` (rebuilds) — two primitives, not one, to avoid paying clone cost for read-only visits.
- Free functions, not methods — TypedExpr is defined elsewhere; matches existing convention.
- `map_children` returns `TypedExprKind`, not `TypedExpr` — callers control `data_type`.
- `AsyncExprTransform` trait instead of closure — Rust can't express `FnMut(&T) -> impl Future` generics cleanly.
- `visit_any` pushes children in reverse for left-to-right pop order.
- Subquery payloads are opaque at the expression level — `has_outer_ref_beyond` descends into subquery payloads with incremented depth to detect transitively-correlated references.

### Verification
- `cargo test`: 1,768 tests pass, 0 failures
- All existing tests preserved and passing
- 7 new regression tests for semantic changes
- 9 new unit tests for traverse.rs core API

### LOC Impact
- New code: ~1,000 LOC (traverse.rs with tests + PreMaterializeTransform)
- Deleted code: ~2,200 LOC (across migration targets)
- Net: ~-1,200 LOC
