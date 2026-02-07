# Sequelize relation failures root-cause memo

**Date**: 2026-02-06  
**Git**: `93fc478` (workspace dirty: `src/sql/executor/select.rs` + `src/sql/executor/operators.rs` have debug-only WARN logs)

## Symptom (Sequelize-only rerun)

- ORM scope: `orm-tests` → only `sequelize/`
- Result: **93 passed / 5 failed** (98 total), exit code 1
- All failures in `orm-tests/sequelize/relation.test.ts`:
  - many-to-many relations (4 fails): load/add/remove/query-through
  - nested includes (1 fail): load multiple levels
  - Error messages: `Column 'id' not found` / `Column 'PostId' not found`

Artifacts:

- `orm-tests/test-results.json`
- pg-tikv log: `/tmp/pgtikv-sequelize.log`

## High-confidence root cause

Sequelize’s many-to-many + nested include queries generate a **parenthesized JOIN** shape like:

```sql
... FROM "sequelize_posts" AS "Post"
LEFT OUTER JOIN (
  "sequelize_post_tags" AS "tags->sequelize_post_tags"
  INNER JOIN "sequelize_tags" AS "tags"
    ON "tags"."id" = "tags->sequelize_post_tags"."TagId"
)
ON "Post"."id" = "tags->sequelize_post_tags"."PostId"
...
```

In `sqlparser-rs`, the `(... JOIN ...)` part becomes `TableFactor::NestedJoin`.

**pg-tikv currently handles `TableFactor::NestedJoin` by materializing it into a derived table** (default alias `nested_join`). This collapses/hides the inner aliases (`tags`, `tags->sequelize_post_tags`) from the outer query’s scope.

But the outer query continues to reference inner aliases in both:

- SELECT projection: `"tags"."id" AS "tags.id"`, `"tags->sequelize_post_tags"."PostId" AS ...`
- JOIN condition: `... ON "Post"."id" = "tags->sequelize_post_tags"."PostId"`

After materialization, the outer scope only “sees” the derived table alias (`nested_join`) and its columns (which are then further prefixed into the join output schema as `nested_join.<col>`). The inner alias names are no longer valid identifiers, so expression evaluation fails with:

- `Column 'id' not found` (when evaluating `"tags"."id"`)
- `Column 'PostId' not found` (when evaluating `"tags->sequelize_post_tags"."PostId"`)

Important nuance (rules out “missing column”):

- The derived table output *does* contain `id` / `PostId` (see evidence below), but the outer query cannot resolve them via the *old* inner-alias-qualified names.

## Direct evidence (from `/tmp/pgtikv-sequelize.log`)

The debug instrumentation in `src/sql/executor/select.rs` prints when `NestedJoin` is materialized.

### Example: `Column 'id' not found`

From `/tmp/pgtikv-sequelize.log`:

> 791: `WARN Materializing nested join as derived table; inner table aliases will not be visible to the outer query alias=nested_join nested_join="sequelize_post_tags" AS "tags->sequelize_post_tags" JOIN "sequelize_tags" AS "tags" ON "tags"."id" = "tags->sequelize_post_tags"."TagId"`  
> 792: `WARN Nested join derived table output schema alias=nested_join derived_cols=["createdAt", "updatedAt", "PostId", "TagId", "id", "name", "color"]`  
> 793: `ERROR Query execution error: Column 'id' not found`

This shows:

1) The exact `NestedJoin` shape (`tags->...` JOIN `tags`) was materialized.  
2) The derived table schema *includes* `id`.  
3) The query still fails resolving `id` (alias-scope issue, not missing data/DDL).

### Example: `Column 'PostId' not found`

> 858: `WARN Materializing nested join as derived table; ...`  
> 859: `WARN ... derived_cols=["createdAt", "updatedAt", "PostId", "TagId", "id", "name", "color"]`  
> 860: `ERROR Query execution error: Column 'PostId' not found`

Again: derived table contains `PostId`, but the outer query cannot resolve `"tags->sequelize_post_tags"."PostId"` after materialization.

### Nested includes shows the same pattern

> 886: `WARN Materializing nested join as derived table ... nested_join="sequelize_post_tags" AS "posts->tags->sequelize_post_tags" JOIN "sequelize_tags" AS "posts->tags" ...`  
> 887: `WARN ... derived_cols=[...]`  
> 888: `ERROR Query execution error: Column 'id' not found`

## Where this happens in code (mechanism)

- `src/sql/executor/select.rs`: `resolve_join_table_factor` → `TableFactor::NestedJoin` arm materializes nested join as a derived table (default alias `nested_join`).
  - The WARN logs above are emitted here (added only for diagnosis).
- `src/sql/executor/table_utils.rs`: `execute_derived_table()` builds a schema from the subquery’s output columns (`derived_cols` in the logs).
- `src/sql/executor/select.rs`: operator multi-join path builds the *outer join output schema* by prefixing every input schema with its alias:
  - derived-table columns become `nested_join.id`, `nested_join.PostId`, ...
- Expression evaluation (`eval_expr` + `SingleTableContext::resolve_compound_identifier`) resolves `table.col` via an exact schema lookup; when the query still uses `"tags"."id"` / `"tags->..."."PostId"`, those names are absent after materialization → column-not-found.

## Why other Sequelize relation tests pass

- one-to-many / many-to-one queries do **not** create `NestedJoin` table factors; they use simpler JOIN shapes where table aliases remain visible at the correct scope (so rewrite + evaluation can resolve identifiers).

## Fix recommendation (most stable, minimal regression risk)

There are two plausible directions:

1) **Full semantic support (flatten NestedJoin)**: treat alias-less `NestedJoin` as pure join-grouping and expand it into the outer join list so inner aliases stay visible “naturally”. This is the cleanest long-term model but touches join planning + schema build + rewrite + potentially more surfaces → higher regression risk.

2) **Transparent materialization (surgical fix)**: keep today’s `NestedJoin -> derived table` fallback (no JOIN algorithm changes), but make it *semantically transparent* **only for `alias == None`** by fixing alias visibility at the rewrite layer, with strong guard-rails: “宁可不支持，也不返回错结果”.

Given the current goal (**fix Sequelize failures with lowest regression risk**), I recommend **(2) Transparent materialization** as the first fix. It is narrowly scoped to the `NestedJoin` fallback and does not alter HashJoin/NLJ operators or the operator framework.

### Semantic boundary (must match PostgreSQL)

Handle `TableFactor::NestedJoin` by alias presence:

- **`( ... )` with `alias == None`** (Sequelize’s shape): parentheses are only join grouping / precedence; **inner table aliases must remain visible** to the outer query scope.
- **`( ... ) AS t`** (or any explicit alias): this becomes a derived-table namespace; **only `t` is visible** outside; inner aliases must *not* leak (keep current behavior).

### Recommended fix (Phase 1): “Transparent derived table” mapping

Keep materialization, but restore alias visibility via expression rewrite:

1) **Materialize alias-less NestedJoin using a unique internal alias**
   - Instead of fixed `nested_join`, generate `__tipg_nested_join_0`, `__tipg_nested_join_1`, … to avoid collisions with user aliases.

2) **Collect “inner visible aliases” from the NestedJoin**
   - Walk `NestedJoin.table_with_joins` and collect each inner `TableFactor::Table`/`Derived` alias (including Sequelize’s `"a->b"` style).
   - This set defines the aliases that PostgreSQL would keep visible outside the parentheses.

3) **Inject an inner-alias → derived-alias mapping into `table_aliases` (rewrite layer)**
   - In `try_execute_simple_join_with_operators()`, when building `table_aliases: Vec<(String, TableSchema)>` for `rewrite_expr_for_multi_join`, add synthetic entries:
     - Pair alias: the internal derived alias (`__tipg_nested_join_N`)
     - Schema name: the inner alias (e.g. `tags`, `tags->sequelize_post_tags`)
     - Schema columns: **empty** (critical)
   - Effect: `rewrite_expr_for_multi_join`’s `CompoundIdentifier` branch matches `schema.name == inner_alias` and rewrites:
     - `"tags"."id"` → `__tipg_nested_join_N."id"`
     - `"tags->...". "PostId"` → `__tipg_nested_join_N."PostId"`
   - Because columns are empty, this does **not** affect unqualified identifiers (`Expr::Identifier`) or ambiguity logic.

This is exactly the missing piece causing today’s `Column 'id' not found` / `Column 'PostId' not found`.

### Correctness guard-rails (must-have)

To keep risk low and avoid silent wrong results:

1) **Duplicate column names in derived output**
   - Materialization uses `SELECT *`, so duplicate column names are possible.
   - pg-tikv’s `TableSchema::column_index()` returns the **first match**, which can silently map `x.id` and `y.id` to the same derived `id`.
   - Guard-rail: if the derived schema contains duplicate column names **for any column referenced via an inner alias mapping**, return a clear `Unsupported` (or `AmbiguousColumn`) error instead of executing. (Failing is strictly better than wrong results.)

2) **Qualified wildcards (`inner_alias.*`)**
   - `SelectItem::QualifiedWildcard` currently resolves against `tables` (not `table_aliases`), so `tags.*` will still fail even if expression rewrite works.
   - Guard-rail: if outer query contains `QualifiedWildcard` for any inner alias coming from a transparent NestedJoin, return `Unsupported` with an explicit message. (Supporting this requires deeper lineage-aware wildcard expansion and is no longer “minimal”.)

3) **Strict scoping**
   - Apply mapping **only** to alias-less `NestedJoin`. Never leak inner aliases when an explicit alias is present.

### Verification / regression tests (what to run after implementing)

- Re-run Sequelize-only suite: `orm-tests/sequelize/relation.test.ts` should go from 5 fails → 0 fails.
- Add a minimal SQL integration regression case reproducing the Sequelize shape:
  - `LEFT JOIN (A AS "x->t" INNER JOIN B AS "x" ON ...) ON ...` with outer references to `"x"` / `"x->t"`.
  - Include at least one projection and one outer ON condition that references inner aliases.

### Ordering: bug fix vs refactor

Fix this bug first, then refactor. This Sequelize NestedJoin case is a high-signal “red line” for correctness in JOIN scoping; getting it green first reduces refactor risk.

### Q&A (for the earlier discussion)

- **1) “是不是 select.rs 实现很糟糕导致的？”**  
  本质不是“代码写得糟”，而是 **语义缺口**：alias-less `NestedJoin` 被当作 derived table 处理，导致 PostgreSQL 作用域规则被破坏。

- **2) “是不是新老机制混合？”**  
  是的：operator JOIN 路径为主，但 `NestedJoin` 目前走“物化 derived table”的 fallback，这相当于把一个“分组括号语义”强行变成“子查询语义”，属于机制混用导致的语义偏差。

- **3) “这个问题容易修复吗？”**  
  如果采用本方案（透明物化 + rewrite 映射 + guard-rails），属于 **中等偏易**（改动集中，回归面小）。真正的“根治”（全面 flatten + wildcard lineage + 重复列可区分）工作量更大，适合后续迭代。
