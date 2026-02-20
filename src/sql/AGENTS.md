# SQL Module

SQL parsing, semantic analysis, optimization, and execution. Single-path architecture: all queries flow through `Analyzer -> Typed IR -> Executor/Operators`.

## Layout

```
src/sql/
├── analyzer/              # Semantic analysis -> AnalyzedQuery/TypedExpr (8,400 lines)
│   ├── mod.rs             # Analyzer struct + main entry points
│   ├── types.rs           # Typed IR: TypedExpr, TypedExprKind, AnalyzedQuery
│   ├── query.rs           # Query-level analysis: SELECT/INSERT/UPDATE/DELETE + GROUP BY
│   ├── expr.rs            # Expression analysis: resolve names, infer types
│   ├── catalog.rs         # CatalogSnapshot: table/column/function resolution
│   ├── scope.rs           # Scope chain for nested queries + correlated subqueries
│   ├── dml.rs             # DML-specific analysis (INSERT/UPDATE/DELETE type checking)
│   ├── literal.rs         # Literal value parsing and type inference
│   ├── error.rs           # AnalyzerError enum with diagnostic messages
│   └── tests.rs           # Comprehensive analyzer test suite
│
├── optimizer/             # CBO pipeline (6,400 lines)
│   ├── mod.rs             # Re-exports: LogicalPlanner, PhysicalPlanner, statistics
│   ├── logical_planner.rs # AnalyzedQuery -> LogicalPlan (abstract tree)
│   ├── logical_plan.rs    # LogicalPlan IR: logical operators before costing
│   ├── physical_planner.rs# LogicalPlan -> PhysicalPlan + operator selection
│   ├── build.rs           # PhysicalPlan -> BoxedOperator (wires execution context)
│   ├── selectivity.rs     # Selectivity estimation using column statistics
│   └── statistics.rs      # ColumnStatistics, TableStatistics structures
│
├── operators/             # Physical operators — Volcano iterator model (7,800 lines)
│   ├── mod.rs             # PhysicalOperator trait: open(), next(), close()
│   ├── scan.rs            # TableScan, IndexScan (TiKV row retrieval)
│   ├── filter.rs          # Filter: WHERE clause predicate evaluation
│   ├── project.rs         # Project: column selection + expression evaluation
│   ├── join.rs            # NestedLoopJoin: streaming outer, materializes right
│   ├── hash_join.rs       # HashJoin: equi-join with hash table
│   ├── aggregate.rs       # HashAggregate: GROUP BY with incremental aggregation
│   ├── sort.rs            # Sort: ORDER BY (full materialization)
│   ├── distinct.rs        # Distinct: UNION/DISTINCT deduplication
│   ├── window.rs          # Window function: OVER clauses
│   ├── limit.rs           # Limit: LIMIT/OFFSET
│   ├── set_operation.rs   # UNION/INTERSECT/EXCEPT operators
│   ├── cte.rs             # CTE (Common Table Expression) iteration
│   ├── table_function.rs  # Table-generating functions (UNNEST, generate_series)
│   ├── planner.rs         # Operator planner: builds operator tree from logical plan
│   ├── context.rs         # ExecutionContext: per-operator state
│   └── executor.rs        # Executor trait for operators
│
├── executor/              # DDL/DML dispatch + SELECT execution
│   ├── core/              # Core dispatch + statement execution (7,000 lines)
│   │   ├── mod.rs         # Main Executor impl (execute_statement, execute_query)
│   │   ├── dispatch.rs    # Statement type dispatcher
│   │   ├── statement.rs   # Common statement helpers + privilege checks
│   │   ├── analyze.rs     # ANALYZE command (statistics collection)
│   │   ├── view_rewrite.rs# View -> base table expansion + privilege checks
│   │   ├── catalog_prefetch.rs # Batch catalog lookups for performance
│   │   └── ...
│   ├── select/            # SELECT-specific execution
│   │   ├── mod.rs         # SELECT entry point (routes to analyzed/)
│   │   └── analyzed/      # Single-path analyzed SELECT executor (6,600 lines)
│   │       ├── mod.rs     # Main analyzed SELECT executor
│   │       ├── query_plan.rs # QueryPlan: routing decisions in one place
│   │       ├── rewrite.rs # View rewrite + expression simplification
│   │       ├── subquery.rs# Subquery execution
│   │       ├── joins.rs   # Join handling for analyzed queries
│   │       ├── materialize.rs # Row materialization (KV -> typed values)
│   │       └── expr_runtime.rs # Expression evaluation in analyzed path
│   ├── cte.rs             # CTE (WITH clause) pre-materialization
│   ├── dml_analyzed.rs    # Analyzed INSERT/UPDATE/DELETE
│   ├── ddl.rs             # DDL dispatch layer
│   ├── triggers.rs        # Trigger dispatch (BEFORE/AFTER)
│   ├── procedure.rs       # Stored procedure execution (CALL)
│   ├── table_utils.rs     # Table schema helpers + column resolution
│   └── ...
│
├── expr/                  # Expression evaluation + functions
│   ├── typed_eval.rs      # CORE: Runtime eval of TypedExpr (~2,800 lines)
│   ├── typed_fold.rs      # AST transformation: fold expr tree
│   ├── typed_rewrite.rs   # Expression rewriting (constant folding)
│   ├── typed_visit.rs     # AST visitor pattern
│   ├── operators.rs       # Binary/unary operator implementations
│   ├── numeric.rs         # Numeric type arithmetic + overflow handling
│   ├── bridge.rs          # Legacy bridge (DML only)
│   └── functions/         # 14 categories of SQL functions
│       ├── array.rs       # array_agg, array_concat, array_slice
│       ├── datetime.rs    # now, date_trunc, extract, age
│       ├── encoding.rs    # encode, decode, convert_from
│       ├── fs9.rs         # fs9_read, fs9_write, fs9_exists, fs9_size
│       ├── fts.rs         # Full-text search functions
│       ├── json.rs        # jsonb_build_object, json_agg, json_array_elements
│       ├── math.rs        # abs, ceil, floor, round, power, sqrt
│       ├── misc.rs        # version(), uuid_generate_v4()
│       ├── pg_compat.rs   # PostgreSQL compatibility functions
│       ├── regex.rs       # regexp_split_to_array, substring
│       ├── string.rs      # substr, length, upper, lower, ltrim, rtrim
│       ├── uuid.rs        # UUID functions
│       └── vector.rs      # Vector operations (embeddings)
│
├── catalog/               # information_schema + pg_catalog (35+ views)
│   ├── mod.rs             # CatalogRegistry (dispatcher for all virtual tables)
│   ├── helpers.rs         # Common utility functions
│   ├── virtual_tables.rs  # VirtualTable trait
│   ├── pg_class.rs        # pg_catalog.pg_class
│   ├── pg_attribute.rs    # pg_catalog.pg_attribute
│   ├── pg_type.rs         # pg_catalog.pg_type
│   ├── pg_index.rs        # pg_catalog.pg_index
│   ├── pg_namespace.rs    # pg_catalog.pg_namespace
│   ├── pg_constraint.rs   # pg_catalog.pg_constraint
│   ├── pg_proc.rs         # pg_catalog.pg_proc
│   ├── pg_roles.rs        # pg_catalog.pg_roles
│   ├── pg_trigger.rs      # pg_catalog.pg_trigger
│   ├── pg_views.rs        # pg_catalog.pg_views
│   ├── pg_description.rs  # pg_catalog.pg_description
│   ├── pg_database.rs     # pg_catalog.pg_database
│   ├── pg_extension.rs    # pg_catalog.pg_extension
│   ├── pg_depend.rs       # pg_catalog.pg_depend
│   ├── pg_am.rs           # pg_catalog.pg_am
│   ├── pg_collation.rs    # pg_catalog.pg_collation
│   ├── tables.rs          # information_schema.tables
│   ├── columns.rs         # information_schema.columns
│   ├── schemata.rs        # information_schema.schemata
│   ├── sequences.rs       # information_schema.sequences
│   └── ...                # 10+ more virtual table implementations
│
├── types/                 # Type inference + coercion
│   ├── registry.rs        # FunctionRegistry: 200+ builtin function signatures
│   ├── infer.rs           # TypeInferrer: core type inference
│   ├── coercion.rs        # common_type() + comparison_target_type()
│   ├── cast.rs            # CAST between any two types
│   ├── mapping.rs         # PostgreSQL DataType <-> internal Value
│   ├── context.rs         # TypeContext: per-query type state
│   └── error.rs           # TypeError enum
│
├── binder/                # Legacy name binding (mostly superseded by Analyzer)
│
├── planner.rs             # Index selection, scan strategy, expression-index support
├── explain.rs             # EXPLAIN output (uses analyzed pipeline)
├── session.rs             # Per-session state: GUCs, transaction, sequence counters
├── stats.rs               # TableStatsCache: row count + column statistics (per-tenant)
├── ddl.rs                 # CREATE/ALTER/DROP TABLE/INDEX/SCHEMA/ROLE/VIEW/TRIGGER/FUNCTION
├── triggers/              # Trigger subsystem (BEFORE/AFTER)
│   ├── mod.rs             # Module declarations + re-exports
│   ├── cache.rs           # TriggerBodyCache, CompiledTriggerBody
│   ├── before.rs          # prefetch_trigger_functions, apply_before_triggers_with_cache
│   ├── rewrite.rs         # substitute_row_references, value_to_sql_literal
│   ├── queue.rs           # TriggerEvent, EventStatus, key encoding, Snowflake IDs
│   ├── worker.rs          # TriggerWorker struct, config, singleton, run()
│   ├── enqueue.rs         # enqueue_after_triggers
│   ├── execute.rs         # execute_trigger_body, PL/pgSQL subset
│   ├── claim.rs           # TriggerQueueTxn trait, claim_events, quarantine
│   └── gc.rs              # gc_loop, recover_orphans, DLQ cleanup
├── sequences.rs           # SEQUENCE management (NEXTVAL, SETVAL, CREATE, ALTER)
├── gin.rs                 # GIN index (full-text search) support
├── fts.rs                 # Full-text search tsquery/tsvector matching
├── plpgsql.rs             # PL/pgSQL procedure parsing and execution
├── rbac.rs                # Role-based access control helpers
├── wildcard.rs            # SELECT * expansion for USING/NATURAL joins
├── parser.rs              # SQL parser wrapper (sqlparser-rs)
└── error.rs               # SqlError enum with PostgreSQL error codes
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `expr/functions/` + type inference in `types/registry.rs` |
| Fix analyzer / name resolution | `analyzer/expr.rs`, `analyzer/query.rs`, `analyzer/scope.rs` |
| Fix type inference | `types/infer.rs`, `types/coercion.rs` |
| Add/modify optimizer rules | `optimizer/logical_planner.rs`, `optimizer/physical_planner.rs` |
| Add physical operator | `operators/*.rs` (implement PhysicalOperator trait) |
| Add expression/operator eval | `expr/typed_eval.rs`, `expr/operators.rs` |
| Modify SELECT behavior | `executor/select/analyzed/mod.rs` |
| Fix INSERT/UPDATE/DELETE | `executor/dml_analyzed.rs` |
| Hash join planning/execution | `planner.rs` + `operators/hash_join.rs` |
| USING/NATURAL `SELECT *` shaping | `wildcard.rs` + `executor/select/` |
| DDL behavior | `ddl.rs` / `executor/ddl.rs` |
| Add catalog view | `catalog/pg_*.rs` (implement VirtualTable trait) |
| Statistics / ANALYZE | `stats.rs` + `executor/core/analyze.rs` + `optimizer/statistics.rs` |
| Triggers | `triggers/` (cache, before, queue, worker, enqueue, execute, claim, gc) + `executor/triggers.rs` (DDL) |
| Views | `executor/core/view_rewrite.rs` |
| Privileges | `executor/core/statement.rs` + `rbac.rs` |
| EXPLAIN | `executor/core/stmt_query.rs` + `explain.rs` (SELECT/WITH uses analyzed pipeline; non-SELECT uses AST trivial plan) |

## Key Architectural Contracts

1. **Single-path SELECT**: All SELECTs flow through Analyzer -> TypedExpr. No legacy fallback.
2. **TypedExpr carries type**: Every `TypedExpr` node has a resolved `DataType`. All column refs -> positional indices. No unresolved names escape the Analyzer.
3. **Optimizer is single-path ON**: CBO pipeline is always on for execution. `tipg.use_optimizer` is retained only as compatibility/readback GUC (accepted as no-op).
4. **Volcano iterator model**: Operators implement `open() -> next() -> close()` lifecycle for streaming execution.
5. **EXPLAIN matches execution for SELECT/WITH**: both paths share analyze+rewrite entry before optimizer planning.
6. **Type coercion dual rules**: `common_type()` (Text wins for mixed types) vs `comparison_target_type()` (non-Text wins for comparisons). Both intentional, both in `coercion.rs`.
7. **Parser rewrites are parse-compat only**: semantic rewrite authority is post-Analyzer.

## Tests

| Kind | Location |
|------|----------|
| Unit tests | `src/sql/**/tests.rs` and module `#[cfg(test)]` blocks |
| SQL regressions | `tests/*.sql` + `tests/*.expected` |

## Commands

```bash
cargo test
./scripts/regression_gate.sh --skip-orm
```
