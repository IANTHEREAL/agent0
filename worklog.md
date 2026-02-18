# Worklog

## 2026-02-17 — Issue #819: Expand Optimizer Coverage & Eliminate Dual-Path Gaps

### Problem
The CBO optimizer pipeline (AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator) could
not handle several query shapes, causing them to fall back to the legacy execution path:
- Async expressions (subqueries, catalog-dependent functions)
- VALUES queries
- Tableless SELECT (empty FROM)
- Table functions in FROM (generate_series, etc.)
- Virtual catalog tables (pg_catalog, information_schema)
- Subquery FROM leaves
- FOR UPDATE/SHARE row locking

### Solution
Expanded the optimizer execution path to handle all common query shapes:

1. **Pre-materialization** of non-correlated async expressions (subqueries → constants)
   before the eligibility check, so queries that previously fell back now route through
   the optimizer.

2. **BuildContext.preloaded_rows** — virtual catalog tables, CTEs, and table functions
   are pre-loaded into a row cache that SeqScan checks before going to KV storage.

3. **Values operator** — build.rs now evaluates VALUES rows at operator build time.

4. **Table function pre-execution** — generate_series, extension functions, user
   functions are executed during context preparation and stored as preloaded rows.

5. **Post-processing pipeline** — async WHERE filter, row locks (FOR UPDATE/SHARE
   with SKIP LOCKED/NOWAIT), async projection, deferred ORDER BY + LIMIT for queries
   that need per-row async evaluation.

6. **Relaxed eligibility** — removed gates for: async expressions, VALUES body, empty
   FROM, non-table FROM refs (functions, subqueries), virtual catalog tables. Kept:
   aggregate ORDER BY/HAVING rewrite failures, window-in-DISTINCT-ON.

### Files Changed
| File | Changes |
|------|---------|
| `src/sql/optimizer/eligibility.rs` | Relaxed: removed 5 gates (async, VALUES, empty FROM, non-table leaves, catalog tables) |
| `src/sql/optimizer/build.rs` | Added preloaded_rows to BuildContext, SeqScan uses it; implemented Values + TableFunction operators |
| `src/sql/executor/select/analyzed/mod.rs` | Pre-materialization before routing; expanded execute_via_optimizer with 9-step pipeline; helper functions for passthrough projection, schema building, table function pre-loading, lock application |
| `src/sql/executor/core/statement.rs` | EXPLAIN: removed lock check from optimizer routing (locks handled by execution) |

### Verification
- `cargo check`: compiles, 6 warnings (all pre-existing)
- `cargo test`: 1575 tests pass, 0 failures

## 2026-02-17 — Issue #698: Trigger Module Restructuring

### Problem
The trigger subsystem spanned 4 flat files totaling ~3,261 lines. `trigger_worker.rs` at 2,005 lines mixed 5 distinct concerns (config, enqueue, claim, execute, GC), making navigation difficult.

### Solution
Converted 4 flat files into a `src/sql/triggers/` directory module with 10 clear file-per-concern files. Zero behavioral changes — pure file reorganization.

### Files Changed
| File | Action |
|------|--------|
| `src/sql/triggers/mod.rs` | **NEW** — module declarations + re-exports |
| `src/sql/triggers/cache.rs` | **NEW** — from `triggers.rs` (TriggerBodyCache, CompiledTriggerBody) |
| `src/sql/triggers/before.rs` | **NEW** — from `triggers.rs` (prefetch, apply) |
| `src/sql/triggers/rewrite.rs` | **NEW** — 1:1 from `trigger_rewrite.rs` |
| `src/sql/triggers/queue.rs` | **NEW** — 1:1 from `trigger_queue.rs` |
| `src/sql/triggers/worker.rs` | **NEW** — from `trigger_worker.rs` (struct, config, run) |
| `src/sql/triggers/enqueue.rs` | **NEW** — from `trigger_worker.rs` (enqueue_after_triggers) |
| `src/sql/triggers/execute.rs` | **NEW** — from `trigger_worker.rs` (body execution, PL/pgSQL) |
| `src/sql/triggers/claim.rs` | **NEW** — from `trigger_worker.rs` (TriggerQueueTxn, claim, quarantine) |
| `src/sql/triggers/gc.rs` | **NEW** — from `trigger_worker.rs` (gc_loop, recover_orphans, DLQ) |
| `src/sql/triggers.rs` | **DELETE** — replaced by `triggers/` directory |
| `src/sql/trigger_worker.rs` | **DELETE** — split into worker/enqueue/execute/claim/gc |
| `src/sql/trigger_queue.rs` | **DELETE** — moved to `triggers/queue.rs` |
| `src/sql/trigger_rewrite.rs` | **DELETE** — moved to `triggers/rewrite.rs` |
| `src/sql/mod.rs` | **EDIT** — remove 3 old module decls, add 2 compat aliases |
| `src/sql/AGENTS.md` | **EDIT** — update trigger layout section |
| `CLAUDE.md` | **EDIT** — update trigger layout + Where to Look table |

### Key Design Decisions
1. **Compat aliases** — `pub(crate) use triggers::worker as trigger_worker` + `triggers::queue as trigger_queue` in `mod.rs` absorb all external consumer references. Zero consumer file changes needed.
2. **`pub(super)` field widening** — only 4 TriggerWorker fields (`worker_id`, `active_keyspaces`, `config`, `shutdown`) widened to `pub(super)` for cross-file access. Private struct `KeyspaceBackoff` stays private to avoid `private_interfaces` lint.
3. **Tests stay in-place** — each file's `#[cfg(test)] mod tests` preserves access to private symbols. `MemTxn` mock lives at top-level of `claim.rs` with `#[cfg(test)] pub(super)` for cross-file test sharing.
4. **Absolute paths for cross-module test imports** — used `crate::sql::triggers::queue::*` instead of fragile `super::super::*`.

### Verification
- `cargo build` — compiles with same 3 warnings as baseline (no new warnings)
- `cargo test` — all 1465 tests pass
- `cargo test trigger` — all 37 trigger tests pass
- Old files gone: `trigger_queue.rs`, `trigger_worker.rs`, `trigger_rewrite.rs`, `triggers.rs` all deleted
- Old module declarations removed from `mod.rs`
- Compat aliases present: 2 `use triggers::* as` in `mod.rs`
- Consumer files unchanged: `main.rs`, `dml_analyzed.rs`, `core/mod.rs`, `table_utils.rs`, `pool.rs`, `dynamic.rs`

---

## 2026-02-17 — Query Rewriter: Subquery Flattening for Views

### Problem
View expansion replaces `FROM my_view` with `FROM (SELECT ... FROM base) AS my_view` at the AST level. After analysis, this produces `AnalyzedTableRefKind::Subquery`, which the optimizer rejects and the legacy planner treats as a join (materializes entire view). Outer predicates like `WHERE age > 30` are never pushed down.

### Solution
Added a post-analysis rewriter (`src/sql/rewriter.rs`) that flattens simple view subqueries back to direct table references. This is a pure function — no side effects, no I/O — inserted between Analyzer and Optimizer/Executor.

### Files Changed
| File | Change |
|------|--------|
| `src/sql/rewriter.rs` | **NEW** — `rewrite_query()`, `can_flatten()`, `flatten_subquery()`, `remap_column_refs()` + 16 unit tests |
| `src/sql/mod.rs` | Added `pub(crate) mod rewriter;` |
| `src/sql/executor/select/analyzed/mod.rs` | Inserted `rewrite_query()` call after analysis, before optimizer gate |
| `src/sql/executor/core/statement.rs` | Inserted `rewrite_query()` call in EXPLAIN path |

### Flattenable Criteria (conservative Step 1)
All must hold: no outer CTEs, single FROM source that is Subquery, inner has no CTEs/LIMIT/OFFSET/ORDER BY/GROUP BY/HAVING/DISTINCT, inner FROM is single Table, all inner projections are plain ColumnRef, no correlated refs in inner WHERE.

### Key Design Decisions
1. **IS TRUE guard on inner WHERE merge** — evaluator short-circuits FALSE but evaluates RHS when LHS is NULL. IS TRUE converts NULL→FALSE for correct short-circuit.
2. **Column name remapping** — inner `SELECT a AS x` → after flatten, outer ColumnRef gets `column_name: "a"` (base), not "x" (alias). Required for planner index selection.
3. **Subquery boundary in remap** — ScalarSubquery/Exists/ArraySubquery/InSubquery.subquery/AnyAll.subquery are NOT descended into (independent scope).
4. **Defensive bounds checks** — if any remapped column_index is out of mapping/base_names bounds, return query unchanged (no panic).

### Verification
- `cargo build` — compiles with no new warnings
- `cargo test` — all 1463 tests pass (16 new rewriter unit tests)
- Rewriter is transparent for non-flattenable queries (returns unchanged)

---

## Issue #819: Eliminate SELECT Dual-Path — Single CBO Architecture

### Summary
Removed the legacy SELECT execution fallback path, making the CBO optimizer the single execution path for all SELECT queries. Deleted ~1700+ lines of legacy code.

### Changes

**Step 1: Self-join keying** — `FROM t AS a JOIN t AS b` was broken because `BuildContext.table_schemas` was keyed by table name, causing overwrites. Fixed by keying all lookups (build.rs, physical_planner.rs, prepare_optimizer_contexts, EXPLAIN) by `alias.unwrap_or(table_name)`.

**Step 2: Subquery FROM schema loading** — `collect_join_tree_refs` in optimizer/mod.rs now recurses into `AnalyzedTableRefKind::Subquery` to pre-load inner table schemas.

**Step 3: JOIN ON subqueries** — Added `extract_async_join_on_predicates` to split correlated subqueries from JOIN ON conditions into post-join async WHERE filters. Reuses existing async WHERE post-processing pipeline.

**Step 4: Remove routing gate + delete legacy** — `try_execute_analyzed` and `execute_subquery` now always call `execute_via_optimizer`. Deleted 7 legacy functions (~1700 lines): `execute_analyzed_query`, `execute_analyzed_pipeline`, `execute_analyzed_join`, `execute_analyzed_values`, `execute_analyzed_tableless`, `execute_analyzed_distinct_on`, `execute_analyzed_set_op`. Deleted `query_plan.rs` entirely.

**Step 5: Unify EXPLAIN** — `execute_explain` now always uses the optimizer pipeline (removed `is_optimizer_eligible` routing check).

**Step 6: GUC deprecation** — `SET tipg.use_optimizer = off` logs NOTICE and stays ON. `SHOW tipg.use_optimizer` always returns "on".

**Step 7: Cleanup** — Removed unused imports, dead `rewrite::*` glob, `pub use planner::*`, `pub use statistics::*`. Gated test-only eligibility functions with `#[cfg(test)]`. Added `#[allow(dead_code)]` on legacy EXPLAIN helpers.

### Files Modified
| File | Action |
|------|--------|
| `src/sql/optimizer/build.rs` | Alias-aware schema/rows lookup in SeqScan/IndexScan + test fix |
| `src/sql/optimizer/mod.rs` | Subquery FROM recursion in collect_join_tree_refs, removed unused re-exports |
| `src/sql/optimizer/eligibility.rs` | Removed 3 gates, gated remaining code with `#[cfg(test)]` |
| `src/sql/optimizer/physical_planner.rs` | Alias-aware stats/schema lookup |
| `src/sql/executor/select/analyzed/mod.rs` | Single path, JOIN ON extraction, deleted ~1700 lines of legacy |
| `src/sql/executor/select/analyzed/query_plan.rs` | **DELETED** |
| `src/sql/executor/core/statement.rs` | Unified EXPLAIN (always optimizer) |
| `src/sql/session.rs` | GUC deprecation notice |
| `src/sql/query_context.rs` | `#[allow(dead_code)]` on unused `use_optimizer()` |
| `src/sql/operators/mod.rs` | Removed unused `pub use planner::*` |
| `src/sql/explain.rs` | `#[allow(dead_code)]` on legacy EXPLAIN helpers |

### Verification
- `cargo check` — zero new warnings (all pre-existing)
- `cargo test` — all 1649 tests pass
- Dead code grep: zero references to `execute_analyzed_query`, `plan_query`, `QueryPlan`
- `use_optimizer` routing branches: zero remaining (only GUC infrastructure)

### Review Fixes (P0/P1 from code review)

**P0 Fix: Outer JOIN ON semantics** — `extract_async_join_on_predicates` now gates extraction on `JoinType::Inner | JoinType::Cross` only. For LEFT/RIGHT/FULL joins, async ON predicates are left in place to preserve null-extension semantics (moving ON→WHERE converts outer joins to inner filters).

**P1 Fix: Scope-safe schema map keying** — Replaced alias-only keying (`alias.unwrap_or(name)`) with composite key via `schema_map_key(table_name, alias)` → `"table_name\0alias"`. This prevents collisions when the same alias appears in outer and inner subquery scopes for different tables (e.g., `FROM users AS t JOIN (SELECT * FROM orders AS t) AS sub`). Updated all 5 lookup sites: build.rs (SeqScan, IndexScan), physical_planner.rs (3 sites), prepare_optimizer_contexts, statement.rs EXPLAIN.

**Finding 3 (landing artifacts)**: False positive — files exist on both branch and master. No action needed.
