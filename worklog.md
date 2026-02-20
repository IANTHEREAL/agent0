# Worklog

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
