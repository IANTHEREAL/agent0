# CBO Phase 3 — PR 1: Optimizer Entrypoint + Multi-Table Eligibility (NLJ Only)

## Detailed Implementation Plan

**Goal**: Enable the optimizer pipeline to handle multi-table JOIN queries using NLJ.
Fix the `left_col_start` index normalization bug. Extract shared utilities to neutral
layers. All changes remain behind `tipg.use_optimizer` GUC (default OFF).

---

## Step 1: Delete Stale Design Document

**File**: `docs/design/sqlalchemy-compatibility-devplan.md`

**Action**: `git rm docs/design/sqlalchemy-compatibility-devplan.md`

**Why**: Every feature it proposes is already shipped. Contains anti-patterns and wrong
file paths.

---

## Step 2: Create `src/sql/expr/classify.rs` — Expression Classification (G5)

**New file**: `src/sql/expr/classify.rs`

**Purpose**: Canonical location for expression classification functions used by both
the executor and optimizer. Prevents dependency inversion (executor must NOT import
from optimizer).

### Functions to extract:

#### 2a: `pub(crate) fn needs_async(expr: &TypedExpr) -> bool`

Currently at: `src/sql/executor/select/analyzed/expr_runtime.rs:431-433`

```rust
pub(super) fn needs_async(expr: &TypedExpr) -> bool {
    has_unresolved_subquery(expr) || has_catalog_dependent_function(expr)
}
```

**Action**: Move `needs_async` to `classify.rs` as `pub(crate)`. Also move its two
helper functions:

- `has_unresolved_subquery` — currently at `subquery.rs:539` (visibility `pub(super)`)
- `has_catalog_dependent_function` — currently at `materialize.rs:22` (visibility `pub(super)`)

Both must become `pub(crate)` in `classify.rs`.

**Original call sites to update**:
- `expr_runtime.rs:431` — change to `use crate::sql::expr::classify::needs_async;`
- `expr_runtime.rs:25-26` — remove old imports of `has_catalog_dependent_function` and
  `has_unresolved_subquery` from `super::materialize` and `super::subquery`; import from
  `crate::sql::expr::classify` instead
- `subquery.rs` — the `split_where_for_async` function at line 510 calls
  `has_unresolved_subquery` directly; update to import from `classify`
- `materialize.rs` — remove `has_catalog_dependent_function` definition (keep the rest)
- `subquery.rs` — remove `has_unresolved_subquery` definition (keep the rest)

**Register module**: Add `pub mod classify;` to `src/sql/expr/mod.rs` (after `pub mod compile;`)

### Content of `classify.rs`:

```rust
//! Expression classification helpers shared across executor and optimizer.
//!
//! Canonical home for `needs_async` (async materialization) and helper predicates.
//! Both execution routing (`executor/select/analyzed/mod.rs`) and optimizer
//! eligibility (`optimizer/eligibility.rs`) import from here — no duplication.

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};

/// Check if a TypedExpr needs async (per-row) materialization.
///
/// Returns `true` if the expression contains subquery nodes or catalog-dependent
/// functions that can't be evaluated by the pure typed evaluator.
pub(crate) fn needs_async(expr: &TypedExpr) -> bool {
    has_unresolved_subquery(expr) || has_catalog_dependent_function(expr)
}

/// Check if a TypedExpr tree contains any non-materialized subquery node.
pub(crate) fn has_unresolved_subquery(expr: &TypedExpr) -> bool {
    // ... (move full body from subquery.rs:539-600)
}

/// Return true if `expr` contains a catalog-dependent function that must be
/// resolved at executor level (not via the pure typed evaluator).
pub(crate) fn has_catalog_dependent_function(expr: &TypedExpr) -> bool {
    // ... (move full body from materialize.rs:22-120)
}
```

---

## Step 3: Extract `reindex_join_condition` to Neutral Layer (G1)

**Source**: `src/sql/executor/select/analyzed/joins.rs:330-336` (and helper
`reindex_typed_expr` at lines 340-414+)

**Target**: `src/sql/analyzer/types.rs` — where `JoinCondition` is defined.

### Why `types.rs` and not a new file:
- `JoinCondition` is defined here (line 822)
- The reindex function operates directly on `JoinCondition`
- Both executor and optimizer already import from `analyzer::types`
- No new dependency graph edges created

### Functions to add to `types.rs`:

```rust
/// Reindex a JoinCondition from global column indices to local indices.
///
/// When the analyzer builds `A JOIN (B JOIN C ON ...)`, the ON condition for
/// the inner join has column indices that are global (relative to the full
/// FROM clause). Both executor and optimizer call this to normalize to local
/// indices (starting from 0 for the join's left child).
pub fn reindex_join_condition(condition: &JoinCondition, offset: usize) -> JoinCondition {
    match condition {
        JoinCondition::On(expr) => JoinCondition::On(reindex_typed_expr(expr, offset)),
        // USING already uses local indices (computed at analysis time).
        other => other.clone(),
    }
}

/// Recursively clone a TypedExpr, subtracting `offset` from all
/// `ColumnRef.column_index` where `scope_depth == 0` (current-scope refs only).
pub fn reindex_typed_expr(expr: &TypedExpr, offset: usize) -> TypedExpr {
    // ... (move full body from joins.rs:340-414+)
}
```

### Update executor call site:

**File**: `src/sql/executor/select/analyzed/joins.rs`

- Remove `reindex_join_condition` definition (lines 330-336)
- Remove `reindex_typed_expr` definition (lines 340-414+)
- Add import: `use crate::sql::analyzer::types::reindex_join_condition;`
- Update `reindex_typed_expr` calls (if any within joins.rs outside the moved functions)
  to use `crate::sql::analyzer::types::reindex_typed_expr`

**Existing callers of `reindex_join_condition` in executor**:
- Search all uses in `executor/select/analyzed/` to ensure they still compile

---

## Step 4: Fix `LogicalPlanner::build_table_ref` — Normalize Join Indices (G1)

**File**: `src/sql/optimizer/logical_planner.rs:174-195`

**Current code** (broken):
```rust
AnalyzedTableRefKind::Join {
    left,
    right,
    join_type,
    condition,
    ..  // ← Discards left_col_start!
} => {
    // ...
    condition: condition.clone(),  // ← Global indices, not normalized
}
```

**Fixed code**:
```rust
AnalyzedTableRefKind::Join {
    left,
    right,
    join_type,
    condition,
    left_col_start,
} => {
    let left_plan = Self::build_table_ref(left);
    let right_plan = Self::build_table_ref(right);
    let mut combined_cols = left_plan.schema.columns.clone();
    combined_cols.extend(right_plan.schema.columns.clone());
    let schema = PlanSchema::from_columns(combined_cols);

    // Normalize ON condition indices from global (analyzer scope) to local
    // (relative to this join's combined schema). left_col_start is the global
    // offset where this join's left child begins.
    let normalized_condition =
        crate::sql::analyzer::types::reindex_join_condition(condition, *left_col_start);

    LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left_plan),
            right: Box::new(right_plan),
            join_type: *join_type,
            condition: normalized_condition,
        },
        schema,
    }
}
```

**Add import**: `use crate::sql::analyzer::types::reindex_join_condition;` (or inline path)

---

## Step 5: Create `src/sql/optimizer/eligibility.rs` — Shared Eligibility (G5)

**New file**: `src/sql/optimizer/eligibility.rs`

**Purpose**: Single `is_optimizer_eligible()` function shared by both execution routing
(`executor/select/analyzed/mod.rs:111`) and EXPLAIN routing (`statement.rs:1125`).

### Content:

```rust
//! Optimizer eligibility — shared by execution and EXPLAIN routing.
//!
//! Determines whether an `AnalyzedQuery` can be routed through the CBO
//! pipeline. Execution-side routing (`mod.rs:111`) adds its own additional
//! guards (locks, SELECT INTO) — this helper covers query shape + expression
//! safety only.

use crate::sql::analyzer::types::{
    AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, JoinCondition, TypedExpr,
};
use crate::sql::expr::classify::needs_async;

/// Check whether a query is eligible for the CBO optimizer pipeline.
///
/// Phase 3 eligibility:
/// - SELECT body only (no SetOperation, no VALUES)
/// - FROM has exactly one table ref (can be a join tree)
/// - All leaf table refs are Table (no subquery, no function)
/// - No expression position contains async expressions (subqueries,
///   catalog-dependent functions)
pub fn is_optimizer_eligible(analyzed: &AnalyzedQuery) -> bool {
    let select = match &analyzed.body {
        AnalyzedQueryBody::Select(select) => select,
        // SetOperation and Values not supported in optimizer build
        // (build.rs:87,95 would fail)
        _ => return false,
    };

    // Must have exactly one FROM item (can be a join tree).
    if select.from.len() != 1 {
        return false;
    }

    // All leaf table refs must be simple Tables.
    if !all_leaves_are_tables(&select.from[0]) {
        return false;
    }

    // Reject if any expression position contains async expressions.
    if has_any_async_expr(select, analyzed) {
        return false;
    }

    true
}

/// Recursively check that all leaves in a table ref tree are `Table` variants.
fn all_leaves_are_tables(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => true,
        AnalyzedTableRefKind::Join { left, right, .. } => {
            all_leaves_are_tables(left) && all_leaves_are_tables(right)
        }
        // Subquery and Function not supported
        _ => false,
    }
}

/// Check if any expression position in the query contains async expressions.
///
/// Walks: SELECT list, WHERE, HAVING, ORDER BY, all JOIN ON conditions.
fn has_any_async_expr(select: &AnalyzedSelect, query: &AnalyzedQuery) -> bool {
    // SELECT list
    for proj in &select.projection {
        if needs_async(&proj.expr) {
            return true;
        }
    }

    // WHERE
    if let Some(ref where_expr) = select.where_clause {
        if needs_async(where_expr) {
            return true;
        }
    }

    // HAVING
    if let Some(ref having_expr) = select.having {
        if needs_async(having_expr) {
            return true;
        }
    }

    // ORDER BY
    for ob in &query.order_by {
        if needs_async(&ob.expr) {
            return true;
        }
    }

    // JOIN ON conditions (recursive)
    for table_ref in &select.from {
        if table_ref_has_async_condition(table_ref) {
            return true;
        }
    }

    false
}

/// Recursively check JOIN ON conditions for async expressions.
fn table_ref_has_async_condition(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => false,
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            // Check ON condition
            if let JoinCondition::On(ref expr) = condition {
                if needs_async(expr) {
                    return true;
                }
            }
            // Recurse into children
            table_ref_has_async_condition(left) || table_ref_has_async_condition(right)
        }
        // Subquery / Function — shouldn't reach here (rejected by
        // all_leaves_are_tables), but be safe
        _ => false,
    }
}
```

### Register module:

**File**: `src/sql/optimizer/mod.rs` — add `pub mod eligibility;`

---

## Step 6: Add `optimize()` Entrypoint in `src/sql/optimizer/mod.rs`

**File**: `src/sql/optimizer/mod.rs`

**Action**: Add a top-level `optimize()` function that encapsulates the full pipeline.
This is the single entrypoint shared by both execution and EXPLAIN.

```rust
use crate::sql::analyzer::types::AnalyzedQuery;
use physical_plan::PhysicalPlan;

/// Single optimizer entrypoint: AnalyzedQuery → PhysicalPlan.
///
/// Both execution (`execute_via_optimizer`) and EXPLAIN call this function
/// to ensure they produce identical plans — no drift.
pub fn optimize(analyzed: &AnalyzedQuery, planning_ctx: &PlanningContext) -> PhysicalPlan {
    // Step 1: AnalyzedQuery → LogicalPlan
    let logical = LogicalPlanner::build(analyzed);
    // Step 2: LogicalPlan → PhysicalPlan (cost-based)
    PhysicalPlanner::plan(&logical, planning_ctx)
}
```

---

## Step 7: Update `execute_via_optimizer` — Recursive Table/Stats Collection

**File**: `src/sql/executor/select/analyzed/mod.rs:2657-2758`

### 7a: Replace inline `is_optimizer_eligible` with shared version

**Remove**: The `fn is_optimizer_eligible` definition at lines 2761-2778.

**Update routing gate** at lines 111-113:
```rust
// Before:
if crate::sql::query_context::QueryContext::use_optimizer()
    && expanded_query.locks.is_empty()
    && is_optimizer_eligible(&analyzed)

// After:
if crate::sql::query_context::QueryContext::use_optimizer()
    && expanded_query.locks.is_empty()
    && crate::sql::optimizer::eligibility::is_optimizer_eligible(&analyzed)
```

### 7b: Recursive table ref collection for stats and schemas

The current code at lines 2678-2690 only collects stats from `select.from` top-level
`Table` refs. For joins, we need to recursively walk the join tree.

**Add helper function** (in `mod.rs` or inline):

```rust
/// Recursively collect all Table refs from a join tree.
fn collect_table_refs(table_ref: &AnalyzedTableRef) -> Vec<(&str, &TableSchema, Option<&str>)> {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, schema } => {
            vec![(name.as_str(), schema, table_ref.alias.as_deref())]
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            let mut refs = collect_table_refs(left);
            refs.extend(collect_table_refs(right));
            refs
        }
        _ => vec![],
    }
}
```

**Update stats collection** (lines 2678-2690):
```rust
// Step 1.5: Build PlanningContext with table statistics.
let mut planning_ctx = PlanningContext::empty();
if let AnalyzedQueryBody::Select(select) = &analyzed.body {
    for table_ref in &select.from {
        for (name, schema, _alias) in collect_table_refs(table_ref) {
            if let Some(stats) = self.stats_cache().get_full_stats(db_id, schema.table_id) {
                planning_ctx.table_stats.insert(name.to_string(), stats);
            }
        }
    }
}
```

**Update schema collection** (lines 2697-2722):
```rust
// Step 3: Resolve table schemas for the operator bridge.
let mut build_ctx = BuildContext::new();
if let AnalyzedQueryBody::Select(select) = &analyzed.body {
    for table_ref in &select.from {
        for (name, _schema, alias) in collect_table_refs(table_ref) {
            let display_alias = alias.unwrap_or(name);
            let cte_key = name.to_lowercase();
            if let Some((cte_schema, _)) = ctes.get(&cte_key) {
                build_ctx = build_ctx.with_schema(name.to_string(), cte_schema.clone());
            } else if let Some(table_schema) =
                self.store().get_schema(txn, db_id, name).await?
            {
                let mut schema = table_schema;
                let short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                if !short.eq_ignore_ascii_case(display_alias) {
                    schema.from_alias = Some(display_alias.to_string());
                }
                build_ctx = build_ctx.with_schema(name.to_string(), schema);
            } else {
                return Err(anyhow!(
                    "Optimizer cannot resolve table schema for '{}'",
                    name
                ));
            }
        }
    }
}
```

### 7c: Use shared `optimize()` entrypoint

Replace lines 2674 and 2693:
```rust
// Before:
let logical = LogicalPlanner::build(analyzed);
// ...build planning_ctx...
let physical = PhysicalPlanner::plan(&logical, &planning_ctx);

// After:
// ...build planning_ctx...
let physical = crate::sql::optimizer::optimize(analyzed, &planning_ctx);
```

---

## Step 8: Update Imports

### `src/sql/executor/select/analyzed/mod.rs`:
- Remove: local `is_optimizer_eligible` function
- Add: `use crate::sql::optimizer::eligibility::is_optimizer_eligible;` (or inline path)
- Update optimizer imports to use `optimize` entrypoint

### `src/sql/executor/select/analyzed/expr_runtime.rs`:
- Change line 25-26 imports:
  ```rust
  // Before:
  use super::materialize::has_catalog_dependent_function;
  use super::subquery::{has_unresolved_subquery, split_where_for_async};

  // After:
  use crate::sql::expr::classify::{has_catalog_dependent_function, has_unresolved_subquery};
  use super::subquery::split_where_for_async;
  ```
- Change `needs_async` definition:
  ```rust
  // Before (line 431):
  pub(super) fn needs_async(expr: &TypedExpr) -> bool {
      has_unresolved_subquery(expr) || has_catalog_dependent_function(expr)
  }

  // After: delegate to classify
  pub(super) fn needs_async(expr: &TypedExpr) -> bool {
      crate::sql::expr::classify::needs_async(expr)
  }
  ```
  OR: remove it entirely and have callers use `classify::needs_async` directly.
  Need to check who calls `needs_async` from `expr_runtime`.

### `src/sql/executor/select/analyzed/subquery.rs`:
- Keep `split_where_for_async` and other functions
- Remove `has_unresolved_subquery` definition (moved to classify.rs)
- Update any internal calls to `has_unresolved_subquery` to use `classify::has_unresolved_subquery`

### `src/sql/executor/select/analyzed/materialize.rs`:
- Remove `has_catalog_dependent_function` definition (moved to classify.rs)
- Update any internal calls to use `classify::has_catalog_dependent_function`

### `src/sql/executor/select/analyzed/joins.rs`:
- Remove `reindex_join_condition` and `reindex_typed_expr` definitions
- Import from `crate::sql::analyzer::types::reindex_join_condition`
- Check if `reindex_typed_expr` is called independently (if so, also import it)

### `src/sql/optimizer/logical_planner.rs`:
- Add import for `reindex_join_condition` from `crate::sql::analyzer::types`

### `src/sql/expr/mod.rs`:
- Add `pub mod classify;`

### `src/sql/optimizer/mod.rs`:
- Add `pub mod eligibility;`
- Add `optimize()` function with necessary imports

---

## Step 9: Write Tests

### 9a: Integration Tests

**File**: New test SQL file(s) in `tests/` directory

Test cases (all with explicit ORDER BY per G6):

1. **Two-table inner join**: `A JOIN B ON a.id = b.a_id` — results identical
   optimizer-on vs optimizer-off
2. **Nested join**: `A JOIN B JOIN C` — ON condition indices are correct
3. **Left outer join**: `A LEFT JOIN B ON ...` — outer join semantics preserved
4. **Cross join**: `FROM a, b` — correct Cartesian product
5. **USING clause**: `A JOIN B USING (id)` — column visibility correct
6. **NATURAL JOIN + SELECT ***: shape preservation

### 9b: Unit Tests

For `eligibility.rs`:
- Single table SELECT → eligible
- Multi-table JOIN → eligible
- JOIN with subquery in ON → NOT eligible
- SetOperation → NOT eligible
- VALUES → NOT eligible
- Table function → NOT eligible

For `reindex_join_condition` (in analyzer/types.rs):
- Nested join reindexing: `A JOIN (B JOIN C)` — ON conditions reindexed correctly
- USING not affected by reindex

---

## Step 10: Verify

1. `cargo build` — compiles with no errors
2. `cargo test` — all existing tests pass (optimizer default OFF)
3. New tests pass
4. `cargo clippy` — no new warnings

---

## Files Changed Summary

| File | Action | Lines Changed |
|------|--------|---------------|
| `docs/design/sqlalchemy-compatibility-devplan.md` | DELETE | - |
| `src/sql/expr/classify.rs` | CREATE | ~150 lines |
| `src/sql/expr/mod.rs` | EDIT | +1 line |
| `src/sql/optimizer/eligibility.rs` | CREATE | ~100 lines |
| `src/sql/optimizer/mod.rs` | EDIT | ~25 lines |
| `src/sql/optimizer/logical_planner.rs` | EDIT | ~10 lines |
| `src/sql/analyzer/types.rs` | EDIT | ~100 lines (add reindex) |
| `src/sql/executor/select/analyzed/mod.rs` | EDIT | ~80 lines |
| `src/sql/executor/select/analyzed/expr_runtime.rs` | EDIT | ~10 lines |
| `src/sql/executor/select/analyzed/joins.rs` | EDIT | ~-90 lines (remove reindex) |
| `src/sql/executor/select/analyzed/subquery.rs` | EDIT | ~-65 lines (remove has_unresolved_subquery) |
| `src/sql/executor/select/analyzed/materialize.rs` | EDIT | ~-100 lines (remove has_catalog_dependent_function) |
| Tests | CREATE/EDIT | ~100 lines |

---

## Execution Order

1. Step 1 — delete stale doc (independent)
2. Step 2 — create classify.rs (enables Step 5)
3. Step 3 — extract reindex to types.rs (enables Step 4)
4. Step 4 — fix logical_planner (depends on Step 3)
5. Step 5 — create eligibility.rs (depends on Step 2)
6. Step 6 — add optimize() entrypoint
7. Step 7 — update execute_via_optimizer (depends on Steps 4, 5, 6)
8. Step 8 — update all imports (integrated with Steps 2-7)
9. Step 9 — write tests
10. Step 10 — verify

Steps 1, 2, 3 can be done in parallel. Steps 4, 5, 6 can be done in parallel after
their dependencies. Step 7 depends on all prior steps.

---

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| Moving `has_unresolved_subquery` breaks callers | Search all `has_unresolved_subquery` call sites before moving; grep for `use super::subquery::has_unresolved_subquery` |
| `reindex_typed_expr` missing a TypedExprKind variant | Copy entire function body; verify all arms present |
| Optimizer path produces wrong results for joins | All behind GUC (default OFF); integration tests compare on vs off |
| USING indices are local but ON indices are global | `reindex_join_condition` already handles this: USING → clone, ON → reindex |
