# SQL Engine Architecture

> **Contracts**: See [docs/sot/sql-engine.md](../sot/sql-engine.md) for normative specifications.
> **Navigation**: See [src/sql/AGENTS.md](../../src/sql/AGENTS.md) for detailed code paths and symbols.
> **Invariants**: See [docs/sot/invariants.md](../sot/invariants.md) for cross-module invariants #1–6.

## CTE Pre-materialization

CTEs are fully materialized before the main query executes:

```
WITH clause processing (executor/cte.rs):
    For each CTE:
    ├─ Non-recursive: execute_query_with_outer_ctes() → materialize rows
    └─ Recursive: execute_recursive_cte()
        ├─ Execute base expression → seed rows
        ├─ Loop (max 1000 iterations):
        │   ├─ Execute recursive expression with current rows as CTE
        │   └─ Append new rows; stop when empty
        └─ Deduplicate if UNION (not UNION ALL)
    Result: HashMap<String, (TableSchema, Vec<Row>)>
```

## DML Path

INSERT, UPDATE, DELETE also flow through the Analyzer:

```
INSERT/UPDATE/DELETE
    → Analyzer::analyze_{insert,update,delete}()
    → TypedExpr for values/conditions
    → executor/dml_analyzed.rs: execute with typed expressions
    → Trigger activation (BEFORE inline, AFTER deferred to commit)
```

## EXPLAIN Path

EXPLAIN uses the same analyzed pipeline as execution to guarantee consistency:

```
EXPLAIN query
    → expand_views_in_query()
    → Analyzer::analyze_query()
    → generate_plan_from_analyzed()
        (same index selection logic as runtime: choose_best_access_path)
    → Format to PostgreSQL-compatible EXPLAIN output
```

## Analyzer (`src/sql/analyzer/`)

Transforms raw SQL AST into typed, resolved intermediate representation.

> **Contract**: See [docs/sot/sql-engine.md](../sot/sql-engine.md) — every `TypedExpr` node carries a resolved `DataType`. All column references are resolved to positional indices. No unresolved names escape the Analyzer.

| File | Purpose |
|------|---------|
| `types.rs` | Typed IR definitions: `TypedExpr`, `TypedExprKind`, `AnalyzedQuery`, `AnalyzedStatement` |
| `query.rs` | Query-level analysis: SELECT/INSERT/UPDATE/DELETE, GROUP BY validation |
| `expr.rs` | Expression analysis: name resolution, type inference for all expression kinds |
| `scope.rs` | Scope chain for nested queries, correlated subquery tracking |
| `catalog.rs` | `CatalogSnapshot`: table/column/function resolution interface |
| `dml.rs` | DML-specific analysis (INSERT/UPDATE/DELETE type checking) |
| `literal.rs` | Literal value parsing and type inference |

## Optimizer (`src/sql/optimizer/`)

Cost-based query optimization. Transforms `AnalyzedQuery` into an optimized physical plan. Always ON (single path). Covers single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, and DISTINCT ON. Includes selectivity estimation, index selection, join reordering (DPccp), subquery decorrelation (EXISTS/NOT EXISTS → SemiJoin/AntiJoin), and predicate pushdown. `db9.use_optimizer` remains compatibility/readback only.

```
AnalyzedQuery
    → LogicalPlanner::build()     → LogicalPlan (abstract tree)
    → Rewrite passes (decorrelate, predicate pushdown, join reorder)
    → PhysicalPlanner::plan()     → PhysicalPlan (with cost estimates)
    → PhysicalPlan::build_operators() → BoxedOperator (executable)
```

| Module | Purpose |
|--------|---------|
| `logical_planner/` | AnalyzedQuery → LogicalPlan conversion |
| `logical_plan.rs` | LogicalPlan IR definition |
| `physical_planner/` | LogicalPlan → PhysicalPlan with operator selection |
| `physical_plan.rs` | PhysicalPlan IR definition |
| `build/` | PhysicalPlan → BoxedOperator (scan, join, aggregate, utils) |
| `selectivity/` | Selectivity estimation from column statistics |
| `statistics.rs` | `ColumnStatistics`, `TableStatistics` structures |
| `rewrite/` | Plan rewrite passes: decorrelation (EXISTS→SemiJoin), predicate pushdown |
| `join_reorder/` | Cost-based join reordering via DPccp algorithm |
| `join_keys.rs` | Join key extraction and analysis |
| `eligibility.rs` | Query eligibility checks for optimizer paths |
| `window_rewrite.rs` | Window function plan rewriting |

## Physical Operators (`src/sql/operators/`)

Streaming execution via Volcano iterator model. Each operator implements `open() → next() → close()`.

| Operator | File | Description |
|----------|------|-------------|
| TableScan / IndexScan | `scan.rs` | TiKV row retrieval (full scan or index lookup) |
| Filter | `filter.rs` | WHERE clause predicate evaluation |
| Project | `project.rs` | Column selection + expression evaluation |
| NestedLoopJoin | `join.rs` | Streaming outer, materializes right side only |
| HashJoin | `hash_join/` | Equi-join with in-memory hash table (mod.rs, hash_table.rs) |
| HashSemiJoin | `hash_semi_join.rs` | Semi/anti-join for EXISTS/NOT EXISTS decorrelation |
| HashAggregate | `aggregate.rs` | GROUP BY with incremental aggregation |
| Sort | `sort.rs` | ORDER BY (full materialization required) |
| Distinct | `distinct.rs` | DISTINCT / UNION deduplication |
| Window | `window/` | Window functions (mod.rs, access.rs, aggregates.rs, ranking.rs) |
| Limit | `limit.rs` | LIMIT / OFFSET |
| SetOperation | `set_operation.rs` | UNION / INTERSECT / EXCEPT |
| CTE | `cte.rs` | Common Table Expression iteration |
| TableFunction | `table_function.rs` | UNNEST, generate_series, etc. |

### JOIN Type Support

| JOIN Type | Supported | Operator | Notes |
|-----------|-----------|----------|-------|
| INNER JOIN | Yes | NLJ, HashJoin | Default join type |
| LEFT OUTER JOIN | Yes | NLJ, HashJoin | NULL-padded right rows for unmatched left |
| RIGHT OUTER JOIN | Yes | NLJ, HashJoin | NULL-padded left rows for unmatched right |
| FULL OUTER JOIN | Yes | NLJ, HashJoin | Both-side NULL padding for unmatched rows |
| CROSS JOIN | Yes | NLJ | Cartesian product |
| SEMI JOIN | Yes | HashSemiJoin | EXISTS decorrelation |
| ANTI JOIN | Yes | HashSemiJoin | NOT EXISTS decorrelation |

## Executor (`src/sql/executor/`)

Statement dispatch, DDL/DML execution, and SELECT orchestration.

```
executor/
├── core/                      # Statement dispatch + infrastructure
│   ├── dispatch/              # Statement routing (mod.rs, prepared.rs, utils.rs)
│   ├── statement.rs           # Privilege checks + common helpers
│   ├── analyze.rs             # ANALYZE command (statistics collection)
│   ├── view_rewrite/          # View expansion + privilege enforcement (mod.rs, expr.rs, query.rs, table.rs)
│   ├── catalog_prefetch/      # Batch catalog lookups (mod.rs, extraction.rs, resolution.rs)
│   ├── stmt_ddl.rs            # DDL statement execution
│   ├── stmt_dml.rs            # DML statement execution
│   ├── stmt_query.rs          # Query statement execution
│   ├── stmt_rbac.rs           # RBAC statement execution
│   ├── settings_tableless.rs  # GUC / tableless settings queries
│   └── observability.rs       # Query observability hooks
├── select/
│   └── analyzed/              # Single-path SELECT executor
│       ├── mod.rs             # Main orchestrator
│       ├── pipeline.rs        # Query execution pipeline
│       ├── pre_materialize.rs # CTE/subquery pre-materialization
│       ├── postprocess.rs     # Result post-processing
│       ├── rewrite.rs         # Expression simplification
│       ├── subquery/          # Subquery execution (mod.rs, tests.rs)
│       └── materialize_catalog.rs # Catalog query materialization
├── dml_analyzed/              # Analyzed INSERT/UPDATE/DELETE (mod.rs, insert.rs, update.rs, delete.rs)
├── procedure/                 # CALL + materialized views
├── table_utils/               # Table function helpers (generate_series)
├── cte.rs                     # CTE pre-materialization
├── ddl.rs                     # DDL dispatch
├── triggers.rs                # Trigger dispatch
├── default_privileges.rs      # DEFAULT PRIVILEGES management
├── user_function.rs           # User-defined function execution
└── bg_sql.rs                  # Background SQL execution (worker integration)
```

### Materialized Views (`executor/procedure/materialized_views.rs`)

Three-phase lifecycle:

1. **CREATE MATERIALIZED VIEW**: Execute the query, auto-generate schema with synthetic `_mv_rowid` primary key, persist metadata and dependency graph, create backing table and seed with initial rows.
2. **REFRESH MATERIALIZED VIEW**: Re-execute stored query against current data, truncate and reload backing table. Supports `CONCURRENTLY` keyword which enqueues a `BgDdl` task for async refresh via the worker engine.
3. **DROP MATERIALIZED VIEW**: Remove metadata, triggers, sequences, and backing table. Supports `CASCADE`.

## Expression System (`src/sql/expr/`)

Runtime evaluation of typed expressions against rows.

| Module | Purpose |
|--------|---------|
| `typed_eval/` | Core evaluator: `eval_typed_expr(expr, row, ctx) → Value` (mod.rs, arithmetic.rs, helpers.rs) |
| `typed_fold.rs` | AST tree transformation |
| `typed_rewrite.rs` | Constant folding, expression rewriting |
| `traverse/` | Expression tree traversal and visitor utilities |
| `classify.rs` | Expression classification (aggregate, window, volatile) |
| `static_eval.rs` | Compile-time constant expression evaluation |
| `operators.rs` | Binary/unary operator implementations |
| `numeric.rs` | Numeric arithmetic with overflow handling |
| `functions/` | 13 categories: array, datetime, encoding, fs9, fts, json, math, misc, pg_compat, regex, string, uuid, vector |

## Type System (`src/sql/types/`)

Type inference, coercion, and PostgreSQL type mapping.

| Module | Purpose |
|--------|---------|
| `registry/` | `FunctionRegistry`: 200+ builtin function signatures (aggregate_window, json, math, misc, string, system, temporal) |
| `infer.rs` | `TypeInferrer`: core type inference logic |
| `coercion.rs` | `common_type()` (Text wins) + `comparison_target_type()` (non-Text wins) |
| `cast/` | CAST between any two types (mod.rs, tests.rs) |
| `mapping.rs` | PostgreSQL DataType <-> internal Value types |

Two coercion functions with intentionally different behavior:
- `common_type(T1, T2)` → common supertype for operators (Text wins for mixed types)
- `comparison_target_type(T1, T2)` → coercion target for comparisons (non-Text wins)

## Catalog (`src/sql/catalog/`)

PostgreSQL system catalog compatibility. 37 virtual table implementations for `pg_catalog`, `information_schema`, and `cron`.

Implements: `pg_class`, `pg_attribute`, `pg_type`, `pg_index`, `pg_namespace`, `pg_constraint`, `pg_proc`, `pg_roles`, `pg_trigger`, `pg_views`, `pg_description`, `pg_database`, `pg_extension`, `pg_depend`, `pg_am`, `pg_collation`, `pg_enum`, `pg_range`, `pg_sequence`, `pg_attrdef`, `pg_db_role_setting`, `pg_tables`, `pg_indexes`, `tables`, `columns`, `schemata`, `sequences`, `routines`, `key_column_usage`, `table_constraints`, `constraint_column_usage`, `check_constraints`, `referential_constraints`, `table_privileges`, `cron_job`, `cron_job_run_details`, `cron_running_jobs`, and more.

## Index Planning (`src/sql/planner/`)

Access path selection. Given a query's WHERE predicates and available indexes, choose the best scan strategy.

Supports:
- B-tree index scans (equality, range, prefix)
- Expression indexes (indexes on computed expressions)
- Partial indexes (indexes with WHERE predicates)
- Multi-column index prefix matching

Note: `ScanType::GinIndexScan` is reserved for planned future support, but GIN access-path selection is currently disabled so EXPLAIN and runtime behavior stay aligned.

## Statistics (`src/sql/stats.rs` + `optimizer/statistics.rs`)

Per-tenant table statistics for cost-based optimization.

```
ANALYZE table_name
    → Scan table rows
    → Compute per-column: distinct count, null fraction, MCV, histogram
    → Store in TableStatsCache (per-tenant, in-memory)
    → Used by PhysicalPlanner for selectivity estimation
```

`TableStatsCache` is owned by `TenantEntry` in the connection pool — when the tenant entry is reaped, statistics are dropped automatically.
