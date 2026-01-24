# Volcano-Style SQL Engine Refactoring

**Started:** 2026-01-23
**Status:** Phase 1-5, 6 (DISTINCT) Complete. Phases 6.2 (Set Operations), 7 (CTEs) Deferred.

## Overview

Refactoring pg-tikv's SQL execution engine to use the Volcano iterator model. The goal is to replace the current monolithic executor with a composable operator tree that enables:

1. **Better modularity** - Each operator is self-contained
2. **Streaming execution** - Process rows one at a time without full materialization
3. **Easier optimization** - Operator reordering, pushdowns
4. **Cleaner code** - Separation of concerns

## Current State Analysis

### Existing Volcano Framework (`src/sql/operators/`)

| Operator | Status | Notes |
|----------|--------|-------|
| `TableScanOperator` | ✅ Complete | Full table scan from TiKV |
| `IndexScanOperator` | ✅ Complete | Index-based row lookup |
| `FilterOperator` | ✅ Complete | Row filtering with predicates |
| `ProjectOperator` | ✅ Complete | Column projection |
| `SortOperator` | ✅ Complete | ORDER BY (materializes all rows) |
| `LimitOperator` | ✅ Complete | LIMIT/OFFSET streaming |
| `HashAggregateOperator` | ✅ Complete | GROUP BY with hash table |
| `NestedLoopJoinOperator` | ✅ Complete | All join types (materializes) |
| `HashJoinOperator` | 🔶 Skeleton | Has code but never used |
| `DistinctOperator` | ✅ Complete | Integrated for SELECT DISTINCT |
| `WindowOperator` | 🔶 Skeleton | Has code but never used |
| `SetOperationOperator` | 🔶 Skeleton | Has code but never used |
| `CTEScanOperator` | 🔶 Skeleton | Has code but never used |

### Current Executor Architecture (`src/sql/executor*.rs`)

The current execution path is in `executor_select.rs` and related files:
- `execute_query_with_ctes()` - Main entry point
- `execute_simple_select()` - Single table queries
- `execute_join_query_with_ctes()` - JOIN queries
- Complex logic for aggregation, window functions, subqueries

### Key Insight

The Volcano operators are **already functional** but **not integrated** into the main execution path. The goal is to gradually route queries through the operator framework.

## Refactoring Plan

### Phase 1: Infrastructure & Testing (Current)
**Goal:** Ensure foundation is solid, add comprehensive operator tests

- [x] Remove dead code warnings (allow unused for WIP code)
- [ ] Add integration tests that use operators directly
- [ ] Ensure `execute_operator_tree()` works correctly
- [ ] Add `EXPLAIN` support that shows operator tree

### Phase 2: Simple SELECT via Operators ✅ COMPLETED
**Goal:** Route simple SELECT queries through Volcano framework

Target queries:
```sql
SELECT * FROM table;
SELECT * FROM table WHERE condition;
SELECT * FROM table ORDER BY col LIMIT n;
SELECT col1, col2 FROM table;  -- projection ← NOW SUPPORTED
```

Tasks:
- [x] Extended `is_simple_operator_query()` to accept column projections
- [x] Added `is_simple_projection_expr()` helper for expression validation
- [x] Added `validate_projection_columns()` for early column existence check
- [x] Modified `execute_with_operators()` to apply projection after scan
- [x] All 107 integration tests pass
- [x] All 584 ORM tests pass

### Phase 3: Aggregation via Operators ✅ COMPLETED
**Goal:** Route GROUP BY queries through `HashAggregateOperator`

Target queries:
```sql
SELECT COUNT(*) FROM table;
SELECT category, SUM(amount) FROM table GROUP BY category;
SELECT category, COUNT(*) FROM table GROUP BY category HAVING COUNT(*) > 5;
```

Tasks:
- [x] Extend planner to build aggregate operator tree (already done)
- [x] Handle HAVING clause (filter after aggregate)
- [x] Integrate into execution path (already done)
- [x] Run all tests (107 integration + 584 ORM tests pass)

### Phase 4: JOIN via Operators ✅ COMPLETED
**Goal:** Route JOIN queries through `NestedLoopJoinOperator`

Target queries:
```sql
SELECT * FROM a JOIN b ON a.id = b.a_id;
SELECT * FROM a LEFT JOIN b ON a.id = b.a_id;
SELECT * FROM a, b WHERE a.id = b.a_id;  -- implicit join
```

**Status:** ✅ Completed - Simple single-JOIN queries now route through operator framework.

Tasks:
- [x] Implement `execute_simple_join_with_operators()` function
- [x] Implement `is_simple_join_operator_query()` predicate  
- [x] Implement `try_execute_simple_join_with_operators()` helper
- [x] Fix table alias resolution in combined schema
- [x] All 107 integration tests + 584 ORM tests pass

### Phase 5: Window Functions via Operators ✅ COMPLETED
**Goal:** Migrate window function execution to `WindowOperator`

Target queries:
```sql
SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM table;
SELECT id, SUM(amount) OVER (PARTITION BY category) FROM table;
```

**Status:** ✅ Completed. Window function queries now route through `WindowOperator` when they match the `is_window_operator_query()` predicate.

Tasks:
- [x] Parse window functions from SELECT to build WindowFunctionExpr
- [x] Integrate WindowOperator with execution path  
- [x] Run window function tests (107 integration + 584 ORM tests pass)

### Phase 6: DISTINCT and Set Operations ✅ PARTIAL (DISTINCT completed)
**Goal:** Complete remaining operators

Target queries:
```sql
SELECT DISTINCT col1, col2 FROM table;
SELECT DISTINCT * FROM table;
SELECT col FROM a UNION SELECT col FROM b;
```

Tasks:
- [x] Integrate `DistinctOperator` for SELECT DISTINCT
- [ ] Integrate `SetOperationOperator` (UNION, INTERSECT, EXCEPT) - deferred, complex recursive tree building
- [x] All 107 integration tests + 584 ORM tests pass

**Status:** ✅ DISTINCT completed. Set operations deferred due to complexity (requires recursive operator tree building for both sides of the operation).

### Phase 7: CTE Support ⏸️ DEFERRED
**Goal:** Route CTE queries through operator framework

**Status:** Deferred. Current CTE implementation works well. `CTEScanOperator` exists but integration requires CTE materialization and reference tracking.

Tasks:
- [ ] Implement CTE materialization in operator framework
- [ ] Handle recursive CTEs
- [ ] Run CTE tests

### Phase 8: Cleanup & Optimization
**Goal:** Remove old code paths, optimize

Tasks:
- [ ] Remove legacy execution code
- [ ] Add HashJoin for equi-joins
- [ ] Add cost-based join order optimization
- [ ] Profile and optimize hot paths

## Progress Log

### 2026-01-23

**Analysis Complete:**
- Reviewed all operator implementations
- Identified which operators are complete vs skeleton
- Mapped current executor flow
- Created this refactoring plan

**Key Finding:** The operators are actually well-implemented AND partially integrated!

**Already Working (via `PGTIKV_USE_OPERATORS=true`, default on):**
- `SELECT * FROM table WHERE ... ORDER BY ... LIMIT ...`
- `SELECT COUNT(*), SUM(x), ... FROM table GROUP BY ...`

**Current Integration Point:** `executor_select.rs` lines 344-420
- Checks `is_simple_operator_query()` → routes to `execute_with_operators()`
- Checks `is_aggregate_operator_query()` → routes to `execute_aggregate_with_operators()`

**What's Missing:**
1. ~~Column projection: `SELECT col1, col2 FROM table`~~ ✅ DONE
2. ~~HAVING clause: `SELECT ... GROUP BY ... HAVING COUNT(*) > 5`~~ ✅ DONE
3. Function calls: `SELECT UPPER(name) FROM table`
4. ~~JOIN queries (code exists but not wired)~~ ⚠️ BLOCKED (alias resolution)
5. DISTINCT, Window functions, CTEs

**Phase 2 Completed:**
- Extended `is_simple_operator_query()` to allow identifier projections
- Added column validation before execution
- Modified projection to use `eval_expr` for each column

**Phase 3 Completed:**
- Removed HAVING restriction from `is_aggregate_operator_query()` 
- Added `eval_having_expr_for_operators()` to evaluate HAVING expressions against aggregated rows
- Added `find_matching_aggregate()` to match aggregate functions in HAVING to computed columns
- All 107 integration tests + 584 ORM tests pass

**Phase 4 Completed:**
- Implemented `execute_simple_join_with_operators()` and helper functions
- Enabled integration at `executor_select.rs` line 148
- Added `rewrite_join_expr_with_aliases()` to transform expressions with qualified column names
- Added `NestedLoopJoinOperator::with_schema()` to accept pre-built combined schema
- Fixed `eval_expr` to look up columns by qualified name first (e.g., `User.id`)
- All 107 integration tests + 584 ORM tests pass

**Phase 5 Completed:**
- Added `is_window_operator_query()` predicate to detect eligible window function queries
- Implemented `extract_window_function_exprs()` to parse window functions from SELECT clause
- Implemented `execute_window_with_operators()` to build operator tree: Scan → Window → Sort → Limit
- Added helper functions for projection mapping (`ProjectionSource`, `project_window_results()`)
- Integrated at `executor_select.rs` line ~472
- All 107 integration tests + 584 ORM tests pass

**Phase 6 Completed (DISTINCT only):**
- Updated `is_simple_operator_query()` to allow plain DISTINCT (only rejects DISTINCT ON)
- Added `distinct: bool` parameter to `execute_with_operators()`
- Fixed operator ordering: Scan → Project → Distinct → Sort → Limit (not Scan → Distinct → Project)
- Used `ProjectOperator` to narrow columns BEFORE DistinctOperator
- All 107 integration tests + 584 ORM tests pass
- SetOperationOperator deferred (requires recursive operator tree building)

## Testing Strategy

After each phase:
1. Run `cargo test` - unit tests
2. Run `./run_tests.sh` - full integration + ORM tests

**Critical:** Never break existing functionality. The old path must remain until the new path is proven.

## Lessons Learned

### Phase 2: Column Projection Support

1. **Validate columns before execution, not during**: When routing queries through the operator path, the projection happens AFTER rows are fetched. If a column doesn't exist, the error would only surface if there are rows. Adding `validate_projection_columns()` catches this early.

2. **Don't silently convert errors to NULL**: The initial implementation used `.unwrap_or(Value::Null)` for projection errors. This caused `SELECT nonexistent_col FROM table` to return empty results instead of an error. Always propagate errors properly.

3. **Keep the old path as fallback**: The `is_simple_operator_query()` function acts as a gatekeeper. Queries that don't match the pattern fall through to the legacy executor. This allows incremental migration without breaking complex queries.

4. **Expression validation must be recursive**: `is_simple_projection_expr()` and `validate_projection_columns()` need to handle nested expressions (CAST, CASE, binary ops) to properly validate complex projections like `SELECT id + 1 FROM table`.

### Phase 3: HAVING Clause Support

1. **HAVING expressions reference computed aggregates, not raw columns**: When evaluating `HAVING COUNT(*) > 5`, the `COUNT(*)` is not a function to execute - it's a reference to a value that was already computed during the aggregation phase. Need a special evaluator that maps aggregate function calls to their corresponding column indices.

2. **Clone aggregate expressions before passing to operator**: The `HashAggregateOperator` takes ownership of `agg_exprs`. If we need them later for HAVING evaluation, clone them first.

3. **Match aggregates by function name AND arguments**: Two different `COUNT(*)` calls should resolve to the same column, but `COUNT(id)` and `COUNT(*)` are different. The `find_matching_aggregate()` function compares function name, arguments (as strings), and DISTINCT flag.

4. **Group-by column count matters**: The aggregated row has columns in order `[group_by_cols..., agg_cols...]`. When looking up an aggregate, its index in `agg_exprs` must be offset by `group_by_count` to get the correct column in the row.

### Phase 4: JOIN Operator Support

1. **Table alias resolution is critical**: When JOIN conditions use aliases like `c.id = o.customer_id`, the expression evaluator needs to map these to the correct columns in the combined schema. Simple `column_index(col_name)` lookup fails when both tables have columns with the same name.

2. **Solution: Qualified column names in combined schema**: Create combined schema with table-qualified column names (`User.id`, `posts.authorId`) and rewrite all expressions to use qualified names. The key changes were:
   - `rewrite_join_expr_with_aliases()` transforms `User.id` → `User.id` (preserving alias)
   - `NestedLoopJoinOperator::with_schema()` accepts pre-built combined schema
   - `eval_expr` now tries qualified lookup first: `column_index("User.id")` before `column_index("id")`

3. **Rewriting must be thorough**: All expressions that reference columns need rewriting:
   - JOIN condition (`ON a.id = b.a_id`)
   - WHERE filter
   - ORDER BY expressions
   - Projection expressions (non-wildcard SELECT columns)
   The `rewrite_join_expr_with_aliases()` function recursively handles BinaryOp, UnaryOp, Function, Cast, Case, InList, Between, etc.

4. **The fix in eval_expr was minimal**: Just 3 lines changed to try qualified lookup first:
   ```rust
   let qualified_name = format!("{}.{}", table_part, col_name);
   schema.column_index(&qualified_name).or_else(|| schema.column_index(col_name))
   ```
   This backward-compatible change works for both JOIN (qualified) and non-JOIN (unqualified) queries.

### Phase 5: Window Operator Support

1. **Window functions require separate projection handling**: The output schema from WindowOperator is `[input_columns..., window_columns...]`. When projecting the final result, we need to track whether each projection item comes from:
   - Input columns (index 0..input_col_count)
   - Window function results (index input_col_count + window_idx)
   - Other expressions (evaluate against input portion of row)

2. **Window function type inference**: Each window function has a specific output type:
   - `row_number`, `rank`, `dense_rank` → Int64
   - `sum`, `avg` → Float64
   - `count` → Int64
   - `min`, `max`, `first_value`, `last_value` → Same as argument type
   - `lag`, `lead` → Same as argument type (nullable)

3. **Operator ordering for window queries**: The correct order is `Scan → Window → Sort → Limit`. The WindowOperator handles its own internal ordering for PARTITION BY and ORDER BY within each window function.

4. **Keep existing window.rs as fallback**: The `is_window_operator_query()` predicate only routes simple window queries through the operator path. Complex cases (multiple tables, CTEs, subqueries) fall back to the existing `compute_window_functions()` in `src/sql/window.rs`.

### Phase 6: DISTINCT Operator Support

1. **DISTINCT must operate on projected columns, not source table columns**: When executing `SELECT DISTINCT col1, col2 FROM table`, the deduplication must happen on `(col1, col2)` pairs, not on the full row. If the table has a primary key, all rows would be "distinct" when looking at the full row.

2. **Operator ordering matters for DISTINCT**: The correct order is:
   - For DISTINCT with column projection: `Scan → Project → Distinct → Sort → Limit`
   - For DISTINCT with wildcard (`SELECT DISTINCT *`): `Scan → Distinct → Sort → Limit`
   
   The initial implementation had `Scan → Sort → Limit → Distinct → Project` which was wrong.

3. **Use ProjectOperator to narrow columns before Distinct**: Rather than applying projection after operator execution, inject a `ProjectOperator` into the operator tree when DISTINCT is present. This ensures DISTINCT sees only the projected columns.

4. **Allow plain DISTINCT but reject DISTINCT ON**: The `is_simple_operator_query()` predicate was updated to:
   - Allow `SELECT DISTINCT ...` (routes through operator path with DistinctOperator)
   - Reject `SELECT DISTINCT ON (col) ...` (falls back to legacy executor)
   
   `DISTINCT ON` has ordering semantics that require different handling.

---

## Technical Notes

### Operator Execution Flow

```
SQL Query
    → Parser (sqlparser-rs)
    → PhysicalPlanner.plan_*()
    → Operator Tree (BoxedOperator)
    → execute_operator_tree()
        → op.open()
        → loop: op.next() until None
        → op.close()
    → ExecuteResult
```

### Key Files

| File | Purpose |
|------|---------|
| `src/sql/operators/mod.rs` | PhysicalOperator trait, BoxedOperator |
| `src/sql/operators/planner.rs` | PhysicalPlanner, OperatorBuilder |
| `src/sql/operators/executor.rs` | execute_operator_tree() |
| `src/sql/operators/context.rs` | ExecutionContext |

### Expression Evaluation

Operators use `eval_expr()` from `src/sql/expr.rs`. This is shared with the current executor, so expression evaluation doesn't need to change.

### Schema Handling

Each operator exposes `schema()` which returns the output schema. This is used by parent operators to know column types and names.
