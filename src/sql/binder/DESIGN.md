# Binder Design — Final RFC (Post-Review)

Issue: #659
Fixes: #643, #644, #653, #654, #645, #638, #641

## Current Code Being Replaced

On `master` (post PR #623 merge), `ddl.rs` contains:

- `relation_matches()` at lines 1534–1546 (13 lines)
- `view_references_any()` + `ScopeAwareChecker` at lines 1548–1657 (110 lines)
- `drop_dependent_views()` at lines 1707–1764 (no `search_path` param)
- 3 call sites: lines 1315, 1418, 1509

### Bugs in current code

1. **#654 — Eager CTE scoping**: `pre_visit_query` (line 1583–1608) adds ALL CTE
   names to shadow set at once. SQL CTE visibility is sequential — a CTE can't
   see later CTEs. `WITH a AS (SELECT * FROM t), t AS (SELECT 1) SELECT * FROM a`
   incorrectly treats `FROM t` inside `a` as CTE reference.

2. **#643 — Recursive CTE false-positive**: `body_refs_same` heuristic (line 1591–1603)
   excludes recursive CTEs from shadow set. For `WITH RECURSIVE t AS (... FROM t ...)`
   the main query's `FROM t` is treated as real table — but it's the CTE.

3. **#653 — No search_path**: `relation_matches` (line 1539) only matches unqualified
   names against `view_schema`, missing cross-schema deps.

4. **#644 — Nested WITH collision**: All CTEs at a Query level added at once — inner
   scopes can mask outer CTE body dependencies.

---

## Data Structures

### BindScope

```rust
pub(crate) struct BindScope {
    /// CTE names visible at this scope level.
    /// Built incrementally during CTE traversal.
    ctes: HashSet<String>,
}
```

### RelationDep

```rust
pub(crate) enum RelationDep {
    Qualified { schema: String, name: String },
    Unqualified { name: String },
}
```

### Binder

```rust
pub(crate) struct Binder {
    scopes: Vec<BindScope>,    // last = innermost
    deps: HashSet<RelationDep>,
}
```

---

## Core Algorithm — CTE Ordering

```
walk_query(query):
    push_scope()

    if query.with exists:
        for each cte in query.with.cte_tables (declaration order):
            name = normalize(cte.alias)

            if query.with.recursive AND cte_body_references_name(cte.body, name):
                // Recursive self-referencing: name visible in own body
                current_scope().ctes.insert(name)
                walk_query(cte.body)
            else:
                // Non-recursive: name NOT visible in own body
                walk_query(cte.body)
                current_scope().ctes.insert(name)

    walk_set_expr(query.body)
    walk_order_by(query.order_by)
    walk_expr_opt(query.limit)
    walk_offset_opt(query.offset)
    walk_fetch_opt(query.fetch)

    pop_scope()
```

### Why this fixes each bug

- **#654**: CTEs added sequentially → when `a`'s body walks, later CTE `t` not yet in scope
- **#643**: Recursive CTE name added BEFORE body walk → `FROM t` resolves to CTE
- **#644**: Each Query gets its own BindScope frame → inner names don't leak to outer
- **#653**: `dep_matches_target` checks all search_path schemas (not just view_schema)

---

## Relation Checking

```rust
fn check_relation(name: &ObjectName) {
    let parts = normalize(name);

    // Only 1-part names can be CTE-shadowed. Qualified names never are.
    if parts.len() == 1 {
        for scope in scopes.iter().rev() {
            if scope.ctes.contains(&parts[0]) {
                return;  // shadowed
            }
        }
    }

    // Record dependency
    match parts.len() {
        1 => Unqualified { name }
        2 => Qualified { schema: parts[0], name: parts[1] }
        n >= 3 => Qualified { schema: parts[n-2], name: parts[n-1] }
    }
}
```

---

## cte_body_references_name

Own implementation using `normalize_ident` (not reusing cte.rs which uses
`.to_lowercase()`). Performs a full walk with a clean scope (no outer CTEs
pre-loaded), so inner WITH clauses correctly shadow the name within their own
subquery scope without hiding top-level self-references.

---

## AST Walker Coverage (sqlparser 0.40.0)

### walk_statement
- Only enters CREATE VIEW / SELECT-like statements
- For Statement::Query → walk_query

### walk_query
- CTE processing (algorithm above)
- query.body → walk_set_expr
- query.order_by → walk_expr for each OrderByExpr.expr
- query.limit → walk_expr_opt
- query.offset → walk_expr on Offset.value
- query.fetch → walk_expr_opt on Fetch.quantity

### walk_set_expr
- Select(select) → walk_select
- Query(query) → walk_query
- SetOperation { left, right } → walk_set_expr both
- Values(values) → walk_expr for each value
- Insert(stmt) → walk_statement
- Update(stmt) → walk_statement
- Table(_) → no-op

### walk_select
- select.from → walk_table_with_joins for each
- select.selection (WHERE) → walk_expr_opt
- select.projection → walk_select_item for each
- select.group_by → walk_expr for each (if Expressions)
- select.having → walk_expr_opt
- select.qualify → walk_expr_opt
- select.named_window → walk window specs
- select.lateral_views → walk_expr for each
- select.distinct → walk_expr for each if Distinct::On

### walk_table_with_joins
- twj.relation → walk_table_factor
- twj.joins → walk_table_factor(join.relation) + walk_join_constraint

### walk_table_factor (8 variants)
- Table { name, args, with_hints } → **check_relation(name)** + walk args + walk hints
- Derived { subquery } → walk_query(subquery)
- TableFunction { expr } → walk_expr(expr)
- Function { args } → walk args ONLY (NO check_relation — functions are not table deps)
- UNNEST { array_exprs } → walk_expr for each
- NestedJoin { table_with_joins } → walk_table_with_joins
- Pivot { table, aggregate_function } → walk_table_factor(table) + walk_expr
- Unpivot { table } → walk_table_factor(table)

### walk_expr — subquery variants
- Subquery(query) → walk_query
- Exists { subquery } → walk_query
- InSubquery { expr, subquery } → walk_expr + walk_query
- ArraySubquery(query) → walk_query
- Function(f) → walk f.args, f.filter, f.over window spec
- All other compound variants → recurse on child Expr nodes

---

## Dependency Matching

```rust
fn dep_matches_target(dep, target, view_schema, search_path) -> bool {
    let (target_schema, target_name) = target.split_once('.') or ("public", target);

    match dep {
        Qualified { schema, name } => schema == target_schema && name == target_name,
        Unqualified { name } => {
            name == target_name && (
                if search_path.is_empty() {
                    target_schema == view_schema || target_schema == "public"
                } else {
                    search_path.contains(target_schema)
                }
            )
        }
    }
}
```

---

## Public API

```rust
pub(crate) fn extract_dependencies(sql: &str) -> Result<HashSet<RelationDep>>
pub(crate) fn view_references_any(
    view_sql: &str, view_schema: &str,
    search_path: &[String], targets: &[String],
) -> bool
```

---

## ddl.rs Integration

1. Thread `search_path: &[String]` to `drop_dependent_views()` — update signature
   and all 3 call sites (lines 1315, 1418, 1509)

2. Replace internal calls:
   ```rust
   // Before:
   view_references_any(&view.query, &view.schema, &pending)
   // After:
   binder::view_references_any(&view.query, &view.schema, search_path, &pending)
   ```

3. Delete from ddl.rs:
   - `relation_matches` (lines 1534–1546)
   - `view_references_any` + `ScopeAwareChecker` (lines 1548–1657)
   - Related unit tests that test the old functions directly
   Total: ~123 lines removed + old tests replaced by binder/tests.rs

---

## Module Structure

```
src/sql/binder/
├── mod.rs       Binder, BindScope, RelationDep, public API, dep matching
├── walk.rs      AST walker: walk_query, walk_select, walk_expr, etc.
├── tests.rs     19 unit tests
└── DESIGN.md    This file
```

Register in src/sql/mod.rs:
```rust
pub(crate) mod binder;
```

---

## Test Plan

### Unit Tests (src/sql/binder/tests.rs)

Each test calls `extract_dependencies(sql)` and asserts the returned HashSet<RelationDep>.

| #  | Name                         | Issue | SQL | Expected |
|----|------------------------------|-------|-----|----------|
| 1  | basic_dep                    | —     | `SELECT * FROM t` | `{U("t")}` |
| 2  | schema_qualified             | —     | `SELECT * FROM s.t` | `{Q("s","t")}` |
| 3  | cte_shadow_simple            | #638  | `WITH t AS (SELECT 42) SELECT * FROM t` | `{}` |
| 4  | cte_wrapping_real_table      | —     | `WITH t AS (SELECT * FROM t) SELECT * FROM t` | `{U("t")}` |
| 5  | recursive_cte_self_ref       | #643  | `WITH RECURSIVE t AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n<10) SELECT * FROM t` | `{}` |
| 6  | later_cte_shadows            | #654  | `WITH a AS (SELECT * FROM t), t AS (SELECT 1) SELECT * FROM a` | `{U("t")}` |
| 7  | nested_with_reuse            | #644  | `WITH a AS (SELECT * FROM t) SELECT * FROM (WITH a AS (SELECT 1) SELECT * FROM a) sub` | `{U("t")}` |
| 8  | subquery_dep                 | —     | `SELECT * FROM (SELECT * FROM t) sub` | `{U("t")}` |
| 9  | expr_subquery                | —     | `SELECT (SELECT x FROM t) FROM s` | `{U("t"), U("s")}` |
| 10 | exists_subquery              | —     | `SELECT * FROM a WHERE EXISTS (SELECT 1 FROM b)` | `{U("a"), U("b")}` |
| 11 | non_recursive_in_recursive   | —     | `WITH RECURSIVE a AS (SELECT 1), b AS (SELECT 1 UNION ALL SELECT n FROM b) SELECT * FROM a, b` | `{}` |
| 12 | qualified_not_shadowed       | —     | `WITH t AS (SELECT 1) SELECT * FROM public.t` | `{Q("public","t")}` |
| 13 | self_referential_view        | #639  | `SELECT * FROM v` | `{U("v")}` |
| 14 | join_deps                    | —     | `SELECT * FROM a JOIN b ON a.id = b.id` | `{U("a"), U("b")}` |
| 15 | cte_plus_real_table          | —     | `WITH c AS (SELECT 1) SELECT * FROM c, real_t` | `{U("real_t")}` |
| 16 | function_in_from             | —     | `SELECT * FROM generate_series(1, 10) AS g` | `{}` |
| 17 | cte_in_join                  | —     | `WITH c AS (SELECT 1 AS id) SELECT * FROM t JOIN c ON t.id = c.id` | `{U("t")}` |
| 18 | cte_in_union                 | —     | `WITH c AS (SELECT * FROM t) SELECT * FROM c UNION SELECT * FROM s` | `{U("t"), U("s")}` |
| 19 | values_clause                | —     | `SELECT * FROM (VALUES (1), (2)) AS v(id)` | `{}` |

### dep_matches_target tests

| #  | Name                         | dep | target | view_schema | search_path | expected |
|----|------------------------------|-----|--------|-------------|-------------|----------|
| 20 | qualified_match              | Q("public","t") | "public.t" | "public" | [] | true |
| 21 | qualified_no_match           | Q("s1","t") | "public.t" | "public" | [] | false |
| 22 | unqualified_same_schema      | U("t") | "public.t" | "public" | [] | true |
| 23 | unqualified_cross_schema     | U("t") | "s1.t" | "public" | ["s1","public"] | true |
| 24 | unqualified_not_on_path      | U("t") | "s1.t" | "public" | ["public"] | false |

### Integration Tests (extend tests/221_drop_cascade_views.sql)

4 new tests for #643, #654, #644, #653.

---

## Implementation Sequence

1. [x] Create `src/sql/binder/` directory
2. [x] Write `mod.rs` — BindScope, RelationDep, Binder struct, public API, dep matching
3. [x] Write `walk.rs` — exhaustive AST walker
4. [x] Write `tests.rs` — all 24 tests
5. [x] Register `pub(crate) mod binder;` in `src/sql/mod.rs`
6. [x] `cargo test binder` — all 24 tests pass
7. [x] Thread `search_path` through `drop_dependent_views()` in ddl.rs
8. [x] Replace old `view_references_any()` calls with `binder::view_references_any()`
9. [x] Delete old `relation_matches`, `ScopeAwareChecker`, `view_references_any` from ddl.rs
10. [x] Delete old unit tests for removed functions (~255 lines, 14 old tests)
11. [x] Add integration tests to `tests/221_drop_cascade_views.sql` (4 new tests: #9-#12)
12. [x] `cargo build` — clean compilation
13. [x] `cargo test` — 1116 tests pass, zero regressions
