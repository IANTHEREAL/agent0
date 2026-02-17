# Worklog

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
