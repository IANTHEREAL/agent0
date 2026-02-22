# SQL Module

SQL parsing, semantic analysis, optimization, and execution. Single-path architecture: all queries flow through `Analyzer -> Typed IR -> Optimizer (CBO) -> Executor/Operators`. ~118K lines across 323 files.

## Layout

```
src/sql/
├── analyzer/                  # Semantic analysis -> AnalyzedQuery/TypedExpr (~11,400 lines)
│   ├── mod.rs                 # Analyzer struct + main entry points
│   ├── catalog.rs             # CatalogSnapshot: table/column/function resolution
│   ├── scope.rs               # Scope chain for nested queries + correlated subqueries
│   ├── dml.rs                 # DML-specific analysis (INSERT/UPDATE/DELETE type checking)
│   ├── literal.rs             # Literal value parsing and type inference
│   ├── error.rs               # AnalyzerError enum with diagnostic messages
│   ├── types/                 # Typed IR definitions
│   │   ├── mod.rs             # TypedExpr, TypedExprKind, AnalyzedQuery, AnalyzedStatement
│   │   └── display.rs         # Display impls for typed IR
│   ├── expr/                  # Expression analysis
│   │   ├── mod.rs             # Top-level expression resolution
│   │   ├── coercion.rs        # Expression-level type coercion
│   │   ├── functions.rs       # Function call analysis + overload resolution
│   │   ├── literals.rs        # Literal expression analysis
│   │   └── operators.rs       # Binary/unary operator analysis
│   ├── query/                 # Query-level analysis
│   │   ├── mod.rs             # SELECT/INSERT/UPDATE/DELETE analysis
│   │   ├── from_clause.rs     # FROM clause: tables, joins, subqueries
│   │   ├── group_by.rs        # GROUP BY validation
│   │   ├── grouping_rewrite.rs# Grouping expression rewrite
│   │   ├── projection.rs      # SELECT list projection analysis
│   │   └── set_expr.rs        # UNION/INTERSECT/EXCEPT analysis
│   └── tests.rs               # Comprehensive test suite
│
├── optimizer/                 # CBO pipeline (~16,300 lines)
│   ├── mod.rs                 # Re-exports
│   ├── logical_plan.rs        # LogicalPlan IR: logical operators before costing
│   ├── physical_plan.rs       # PhysicalPlan IR: costed physical operators
│   ├── statistics.rs          # ColumnStatistics, TableStatistics structures
│   ├── eligibility.rs         # Query eligibility checks for optimizer paths
│   ├── join_keys.rs           # Join key extraction and analysis
│   ├── window_rewrite.rs      # Window function plan rewriting
│   ├── logical_planner/       # AnalyzedQuery -> LogicalPlan
│   │   ├── mod.rs             # Main logical planner
│   │   ├── nodes.rs           # Logical node construction
│   │   └── tests.rs
│   ├── physical_planner/      # LogicalPlan -> PhysicalPlan
│   │   ├── mod.rs             # Cost-based physical operator selection
│   │   └── tests.rs
│   ├── build/                 # PhysicalPlan -> BoxedOperator (~3,500 lines)
│   │   ├── mod.rs             # Operator tree construction
│   │   ├── scan.rs            # Scan operator building (table/index)
│   │   ├── join.rs            # Join operator building (NLJ/hash/semi)
│   │   ├── aggregate.rs       # Aggregate operator building
│   │   ├── utils.rs           # Build utilities
│   │   └── tests.rs
│   ├── rewrite/               # Plan rewrite passes (~2,500 lines)
│   │   ├── mod.rs             # Predicate pushdown, cross-join elimination
│   │   ├── decorrelate.rs     # Subquery decorrelation (EXISTS→SemiJoin, NOT EXISTS→AntiJoin)
│   │   └── tests.rs
│   ├── join_reorder/          # Cost-based join reordering (~2,600 lines)
│   │   ├── mod.rs             # Join graph construction
│   │   ├── algorithms.rs      # DPccp algorithm implementation
│   │   ├── cost.rs            # Join cost model
│   │   ├── predicates.rs      # Join predicate analysis
│   │   └── tests.rs
│   └── selectivity/           # Selectivity estimation (~2,300 lines)
│       ├── mod.rs             # Selectivity from column statistics
│       └── tests.rs
│
├── operators/                 # Physical operators — Volcano iterator model (~8,300 lines)
│   ├── mod.rs                 # PhysicalOperator trait: open(), next(), close()
│   ├── scan.rs                # TableScan, IndexScan (TiKV row retrieval)
│   ├── filter.rs              # Filter: WHERE clause predicate evaluation
│   ├── project.rs             # Project: column selection + expression evaluation
│   ├── join.rs                # NestedLoopJoin: streaming outer, materializes right
│   ├── hash_join/             # HashJoin: equi-join with hash table
│   │   ├── mod.rs             # Hash join operator
│   │   ├── hash_table.rs      # In-memory hash table
│   │   └── tests.rs
│   ├── hash_semi_join.rs      # HashSemiJoin: semi/anti-join for EXISTS decorrelation
│   ├── aggregate.rs           # HashAggregate: GROUP BY with incremental aggregation
│   ├── sort.rs                # Sort: ORDER BY (full materialization)
│   ├── distinct.rs            # Distinct: UNION/DISTINCT deduplication
│   ├── window/                # Window functions (~1,500 lines)
│   │   ├── mod.rs             # Window operator core
│   │   ├── access.rs          # Row access patterns (lead, lag, first_value)
│   │   ├── aggregates.rs      # Window aggregate functions
│   │   ├── ranking.rs         # Ranking functions (row_number, rank, dense_rank)
│   │   └── tests.rs
│   ├── limit.rs               # Limit: LIMIT/OFFSET
│   ├── set_operation.rs       # UNION/INTERSECT/EXCEPT operators
│   ├── cte.rs                 # CTE (Common Table Expression) iteration
│   ├── table_function.rs      # Table-generating functions (UNNEST, generate_series)
│   ├── planner.rs             # Operator planner: builds operator tree from logical plan
│   ├── context.rs             # ExecutionContext: per-operator state
│   └── executor.rs            # Executor trait for operators
│
├── executor/                  # DDL/DML dispatch + SELECT execution (~25,400 lines)
│   ├── core/                  # Statement dispatch + infrastructure
│   │   ├── mod.rs             # Main Executor impl
│   │   ├── dispatch/          # Statement routing (~2,000 lines)
│   │   │   ├── mod.rs         # Main statement dispatcher
│   │   │   ├── prepared.rs    # Prepared statement dispatch
│   │   │   └── utils.rs       # Dispatch utilities
│   │   ├── view_rewrite/      # View expansion (~1,400 lines)
│   │   │   ├── mod.rs         # View rewrite orchestrator
│   │   │   ├── expr.rs        # Expression-level view rewriting
│   │   │   ├── query.rs       # Query-level view rewriting
│   │   │   └── table.rs       # Table reference resolution
│   │   ├── catalog_prefetch/  # Batch catalog lookups (~1,100 lines)
│   │   │   ├── mod.rs         # Prefetch orchestrator
│   │   │   ├── extraction.rs  # Catalog reference extraction from AST
│   │   │   ├── resolution.rs  # Batch resolution against TiKV
│   │   │   └── tests.rs
│   │   ├── stmt_ddl.rs        # DDL statement execution
│   │   ├── stmt_dml.rs        # DML statement execution
│   │   ├── stmt_query.rs      # Query statement execution (SELECT/EXPLAIN)
│   │   ├── stmt_rbac.rs       # RBAC statement execution (GRANT/REVOKE)
│   │   ├── statement.rs       # Common statement helpers + privilege checks
│   │   ├── analyze.rs         # ANALYZE command (statistics collection)
│   │   ├── analyze_rewrite.rs # Analyze rewrite entry point
│   │   ├── settings_tableless.rs # GUC / tableless settings queries
│   │   ├── prepared_analysis.rs # Prepared statement analysis
│   │   ├── prepared_stmt.rs   # Prepared statement management
│   │   ├── observability.rs   # Query observability hooks
│   │   ├── guc.rs             # GUC handling
│   │   ├── copy.rs            # COPY statement handling
│   │   ├── misc.rs            # Miscellaneous statement handlers
│   │   └── tests.rs
│   ├── select/                # SELECT-specific execution (~3,100 lines)
│   │   ├── mod.rs             # SELECT entry point
│   │   └── analyzed/          # Single-path analyzed SELECT executor
│   │       ├── mod.rs         # Main orchestrator
│   │       ├── pipeline.rs    # Query execution pipeline
│   │       ├── pre_materialize.rs # CTE/subquery pre-materialization
│   │       ├── postprocess.rs # Result post-processing (DISTINCT, ORDER BY)
│   │       ├── rewrite.rs     # Expression simplification
│   │       ├── expr_runtime.rs# Expression evaluation in analyzed path
│   │       ├── materialize.rs # Row materialization (KV -> typed values)
│   │       ├── materialize_catalog.rs # Catalog query materialization
│   │       ├── joins.rs       # Join handling for analyzed queries
│   │       └── subquery/      # Subquery execution (mod.rs, tests.rs)
│   ├── dml_analyzed/          # Analyzed INSERT/UPDATE/DELETE
│   │   ├── mod.rs             # DML orchestrator
│   │   ├── insert.rs          # INSERT execution
│   │   ├── update.rs          # UPDATE execution
│   │   └── delete.rs          # DELETE execution
│   ├── procedure/             # Stored procedures + materialized views
│   ├── table_utils/           # Table function helpers (generate_series)
│   ├── cte.rs                 # CTE (WITH clause) pre-materialization
│   ├── ddl.rs                 # DDL dispatch layer
│   ├── triggers.rs            # Trigger dispatch (BEFORE/AFTER DDL)
│   ├── default_privileges.rs  # DEFAULT PRIVILEGES management
│   ├── user_function.rs       # User-defined function execution
│   ├── bg_sql.rs              # Background SQL execution (worker integration)
│   ├── database.rs            # Database management (CREATE/DROP DATABASE)
│   ├── cron.rs                # Cron job management
│   ├── extensions.rs          # Extension management
│   └── udt.rs                 # User-defined type management
│
├── expr/                      # Expression evaluation + functions (~12,200 lines)
│   ├── mod.rs                 # Expression module exports
│   ├── typed_eval/            # Runtime evaluator (~3,400 lines)
│   │   ├── mod.rs             # eval_typed_expr(expr, row, ctx) → Value
│   │   ├── arithmetic.rs      # Arithmetic expression evaluation
│   │   ├── helpers.rs         # Evaluation helper functions
│   │   └── tests.rs
│   ├── traverse/              # Expression tree traversal (~1,000 lines)
│   │   ├── mod.rs             # Visitor and walker utilities
│   │   └── tests.rs
│   ├── classify.rs            # Expression classification (aggregate, window, volatile)
│   ├── static_eval.rs         # Compile-time constant expression evaluation
│   ├── typed_fold.rs          # AST transformation: fold expr tree
│   ├── typed_rewrite.rs       # Expression rewriting (constant folding)
│   ├── typed_visit.rs         # AST visitor pattern
│   ├── operators.rs           # Binary/unary operator implementations
│   ├── numeric.rs             # Numeric type arithmetic + overflow handling
│   ├── bridge.rs              # Legacy bridge (DML only)
│   └── functions/             # 14 categories of SQL functions (~5,500 lines)
│       ├── array.rs           # array_agg, array_concat, array_slice
│       ├── datetime.rs        # now, date_trunc, extract, age
│       ├── encoding.rs        # encode, decode, convert_from
│       ├── fs9.rs             # fs9_read, fs9_write, fs9_exists, fs9_size
│       ├── fts.rs             # Full-text search functions
│       ├── json.rs            # jsonb_build_object, json_agg, json_array_elements
│       ├── math.rs            # abs, ceil, floor, round, power, sqrt
│       ├── misc.rs            # version(), uuid_generate_v4()
│       ├── pg_compat.rs       # PostgreSQL compatibility functions
│       ├── regex.rs           # regexp_split_to_array, substring
│       ├── string.rs          # substr, length, upper, lower, ltrim, rtrim
│       ├── uuid.rs            # UUID functions
│       └── vector.rs          # Vector operations (embeddings)
│
├── catalog/                   # information_schema + pg_catalog + cron (40+ views, ~4,600 lines)
│   ├── mod.rs                 # CatalogRegistry (dispatcher for all virtual tables)
│   ├── helpers.rs             # Common utility functions
│   ├── virtual_tables.rs      # VirtualTable trait
│   ├── pg_class.rs            # pg_catalog.pg_class
│   ├── pg_attribute.rs        # pg_catalog.pg_attribute
│   ├── pg_type.rs             # pg_catalog.pg_type
│   ├── pg_index.rs            # pg_catalog.pg_index
│   ├── pg_indexes.rs          # pg_catalog.pg_indexes
│   ├── pg_namespace.rs        # pg_catalog.pg_namespace
│   ├── pg_constraint.rs       # pg_catalog.pg_constraint
│   ├── pg_proc.rs             # pg_catalog.pg_proc
│   ├── pg_roles.rs            # pg_catalog.pg_roles
│   ├── pg_trigger.rs          # pg_catalog.pg_trigger
│   ├── pg_views.rs            # pg_catalog.pg_views
│   ├── pg_description.rs      # pg_catalog.pg_description
│   ├── pg_database.rs         # pg_catalog.pg_database
│   ├── pg_extension.rs        # pg_catalog.pg_extension
│   ├── pg_depend.rs           # pg_catalog.pg_depend
│   ├── pg_am.rs               # pg_catalog.pg_am
│   ├── pg_collation.rs        # pg_catalog.pg_collation
│   ├── pg_enum.rs             # pg_catalog.pg_enum
│   ├── pg_range.rs            # pg_catalog.pg_range
│   ├── pg_sequence.rs         # pg_catalog.pg_sequence
│   ├── pg_attrdef.rs          # pg_catalog.pg_attrdef
│   ├── pg_db_role_setting.rs  # pg_catalog.pg_db_role_setting
│   ├── pg_tables.rs           # pg_catalog.pg_tables
│   ├── tables.rs              # information_schema.tables
│   ├── columns.rs             # information_schema.columns
│   ├── schemata.rs            # information_schema.schemata
│   ├── sequences.rs           # information_schema.sequences
│   ├── routines.rs            # information_schema.routines
│   ├── key_column_usage.rs    # information_schema.key_column_usage
│   ├── table_constraints.rs   # information_schema.table_constraints
│   ├── constraint_column_usage.rs # information_schema.constraint_column_usage
│   ├── check_constraints.rs   # information_schema.check_constraints
│   ├── referential_constraints.rs # information_schema.referential_constraints
│   ├── table_privileges.rs    # information_schema.table_privileges
│   ├── cron_job.rs            # cron.job virtual table
│   ├── cron_job_run_details.rs# cron.job_run_details virtual table
│   └── cron_running_jobs.rs   # cron.running_jobs virtual table
│
├── types/                     # Type inference + coercion (~4,000 lines)
│   ├── mod.rs                 # Module exports
│   ├── infer.rs               # TypeInferrer: core type inference
│   ├── coercion.rs            # common_type() + comparison_target_type()
│   ├── mapping.rs             # PostgreSQL DataType <-> internal Value
│   ├── context.rs             # TypeContext: per-query type state
│   ├── error.rs               # TypeError enum
│   ├── cast/                  # CAST between types
│   │   ├── mod.rs             # Cast logic
│   │   └── tests.rs
│   └── registry/              # FunctionRegistry: 200+ builtin function signatures
│       ├── mod.rs             # Registry core
│       ├── aggregate_window.rs# Aggregate + window function signatures
│       ├── json.rs            # JSON function signatures
│       ├── math.rs            # Math function signatures
│       ├── misc.rs            # Miscellaneous function signatures
│       ├── string.rs          # String function signatures
│       ├── system.rs          # System function signatures
│       └── temporal.rs        # Date/time function signatures
│
├── ddl/                       # DDL: CREATE/ALTER/DROP (~4,300 lines)
│   ├── mod.rs                 # DDL dispatch
│   ├── alter_table.rs         # ALTER TABLE (add/drop column, rename, FK management)
│   ├── create_index.rs        # CREATE INDEX (btree, GIN, expression, partial)
│   ├── create_table.rs        # CREATE TABLE (constraints, defaults, sequences)
│   ├── drop.rs                # DROP TABLE/INDEX/VIEW/SCHEMA/FUNCTION
│   ├── view.rs                # CREATE/ALTER VIEW
│   └── tests.rs
│
├── dml/                       # DML helpers (~2,300 lines)
│   ├── mod.rs                 # DML dispatch
│   ├── defaults.rs            # Default value evaluation
│   ├── foreign_keys.rs        # FK validation + cascade operations
│   ├── insert.rs              # INSERT helpers
│   ├── update.rs              # UPDATE helpers
│   ├── delete.rs              # DELETE helpers
│   └── tests.rs
│
├── session/                   # Per-session state (~2,500 lines)
│   ├── mod.rs                 # Session struct: database, search_path, timezone
│   ├── settings.rs            # GUC settings (SET/SHOW/RESET)
│   ├── transaction.rs         # Transaction state management
│   └── tests.rs
│
├── planner/                   # Index selection + scan strategy (~3,700 lines)
│   ├── mod.rs                 # Planner entry point
│   ├── index_selection.rs     # choose_best_access_path() for btree/GIN
│   ├── predicate.rs           # Predicate analysis for index applicability
│   ├── scan_type.rs           # ScanType enum (TableScan, IndexScan, GinScan)
│   └── tests.rs
│
├── explain/                   # EXPLAIN output (~1,400 lines)
│   ├── mod.rs                 # EXPLAIN entry point (uses analyzed pipeline)
│   ├── format.rs              # PostgreSQL-compatible EXPLAIN formatting
│   ├── transform.rs           # Plan tree → EXPLAIN tree transformation
│   └── tests.rs
│
├── triggers/                  # Trigger subsystem (~1,900 lines)
│   ├── mod.rs                 # Module declarations
│   ├── cache.rs               # TriggerBodyCache, CompiledTriggerBody
│   ├── before.rs              # prefetch_trigger_functions, apply_before_triggers_with_cache
│   ├── rewrite.rs             # substitute_row_references, value_to_sql_literal
│   ├── enqueue.rs             # enqueue_after_triggers (→ worker engine)
│   ├── execute.rs             # execute_trigger_body (PL/pgSQL subset)
│   └── queue.rs               # TriggerEvent types
│
├── sequences/                 # SEQUENCE management (~1,600 lines)
│   ├── mod.rs                 # NEXTVAL, SETVAL, CURRVAL
│   ├── ddl.rs                 # CREATE/ALTER/DROP SEQUENCE
│   ├── eval.rs                # Sequence expression evaluation
│   ├── index_helpers.rs       # Sequence-index helpers
│   ├── replace.rs             # Sequence reference replacement
│   └── tests.rs
│
├── rewriter/                  # SQL rewriter (~1,600 lines)
│   ├── mod.rs                 # Rewriter entry point
│   ├── flatten.rs             # Nested query flattening
│   ├── remap.rs               # Column remapping
│   └── tests.rs
│
├── plpgsql/                   # PL/pgSQL (~1,700 lines)
│   ├── mod.rs                 # Module exports
│   ├── parser.rs              # PL/pgSQL parser (DECLARE/BEGIN/END/IF/FOR/LOOP/EXIT)
│   ├── executor.rs            # PL/pgSQL executor (SELECT INTO, RAISE, RETURN)
│   ├── utils.rs               # PL/pgSQL utilities
│   └── tests.rs
│
├── parser/                    # SQL parser wrapper (~1,800 lines)
│   ├── mod.rs                 # Parser entry point (sqlparser-rs wrapper)
│   ├── operator_rewrite.rs    # Operator rewriting (parse-compat only)
│   ├── preprocess.rs          # SQL preprocessing (dollar quoting, etc.)
│   ├── tokenizer.rs           # Custom tokenizer extensions
│   └── tests.rs
│
├── binder/                    # Legacy name binding (mostly superseded by Analyzer)
│   ├── mod.rs                 # Binder core
│   ├── walk.rs                # AST walker
│   └── tests.rs
│
├── stats.rs                   # TableStatsCache: row count + column statistics (per-tenant)
├── gin.rs                     # GIN index (full-text search) support
├── fts.rs                     # Full-text search tsquery/tsvector matching
├── fts_tokenizers.rs          # FTS tokenizer implementations (Chinese)
├── rbac.rs                    # Role-based access control helpers
├── wildcard.rs                # SELECT * expansion for USING/NATURAL joins
├── error.rs                   # SqlError enum with PostgreSQL error codes (SQLSTATE)
│
│ # Utility modules
├── aggregate.rs               # Aggregate function runtime
├── alter_owner.rs             # ALTER OWNER implementation
├── alter_sequence_owned_by.rs # ALTER SEQUENCE ... OWNED BY
├── bytea.rs                   # Bytea type handling
├── catalog_oids.rs            # Catalog OID constants
├── check_constraints.rs       # CHECK constraint validation
├── comment_on.rs              # COMMENT ON implementation
├── ddl_export.rs              # DDL export (pg_dump-compatible)
├── default_privileges.rs      # DEFAULT PRIVILEGES runtime
├── index_consistency.rs       # Index consistency checks
├── index_helpers.rs           # Index operation helpers
├── information_schema.rs      # information_schema constants
├── jsonb.rs                   # JSONB operations
├── names.rs                   # Name resolution utilities
├── pg_numeric.rs              # PostgreSQL numeric type
├── pg_types.rs                # PostgreSQL type constants
├── projection.rs              # Projection helpers
├── query_context.rs           # Query execution context
├── quoting.rs                 # SQL identifier quoting
├── raw_sql.rs                 # Raw SQL handling
├── result.rs                  # SQL result types
├── role_settings.rs           # Role settings management
├── stack_safety.rs            # Stack overflow prevention
├── statement_time.rs          # Statement timing
├── table_functions.rs         # Table function runtime
├── timezone.rs                # Timezone handling
├── udt.rs                     # User-defined type helpers
├── value_coercion.rs          # Value-level type coercion
└── value_key.rs               # Value key encoding helpers
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `expr/functions/` + type signatures in `types/registry/` |
| Fix analyzer / name resolution | `analyzer/expr/`, `analyzer/query/`, `analyzer/scope.rs` |
| Fix type inference | `types/infer.rs`, `types/coercion.rs` |
| Add/modify optimizer rules | `optimizer/logical_planner/`, `optimizer/physical_planner/` |
| Join reordering | `optimizer/join_reorder/` (algorithms.rs = DPccp, cost.rs, predicates.rs) |
| Subquery decorrelation | `optimizer/rewrite/decorrelate.rs` |
| Predicate pushdown | `optimizer/rewrite/mod.rs` |
| Build physical operators | `optimizer/build/` (scan.rs, join.rs, aggregate.rs) |
| Add physical operator | `operators/*.rs` (implement PhysicalOperator trait) |
| Add expression/operator eval | `expr/typed_eval/`, `expr/operators.rs` |
| Modify SELECT behavior | `executor/select/analyzed/mod.rs`, `executor/select/analyzed/pipeline.rs` |
| Fix INSERT/UPDATE/DELETE | `executor/dml_analyzed/` (insert.rs, update.rs, delete.rs) |
| Hash join planning/execution | `planner/` + `operators/hash_join/` |
| Semi/anti-join (EXISTS) | `optimizer/rewrite/decorrelate.rs` + `operators/hash_semi_join.rs` |
| USING/NATURAL `SELECT *` shaping | `wildcard.rs` + `executor/select/` |
| DDL behavior | `ddl/` (alter_table.rs, create_index.rs, create_table.rs, drop.rs, view.rs) |
| FK validation/cascade | `dml/foreign_keys.rs` + `ddl/alter_table.rs` |
| Add catalog view | `catalog/pg_*.rs` (implement VirtualTable trait) |
| Statistics / ANALYZE | `stats.rs` + `executor/core/analyze.rs` + `optimizer/statistics.rs` |
| Triggers | `triggers/` (cache, before, rewrite, enqueue, execute) + `executor/triggers.rs` (DDL) |
| Views | `executor/core/view_rewrite/` (expr.rs, query.rs, table.rs) |
| Privileges | `executor/core/statement.rs` + `rbac.rs` |
| EXPLAIN | `executor/core/stmt_query.rs` + `explain/` (SELECT/WITH uses analyzed pipeline) |
| Session / GUCs | `session/` (settings.rs, transaction.rs) |
| PL/pgSQL | `plpgsql/` (parser.rs, executor.rs) |
| Sequences | `sequences/` (mod.rs, ddl.rs, eval.rs, replace.rs) |
| Background SQL | `executor/bg_sql.rs` (worker engine integration) |

## Key Architectural Contracts

1. **Single-path SELECT**: All SELECTs flow through Analyzer -> TypedExpr. No legacy fallback.
2. **TypedExpr carries type**: Every `TypedExpr` node has a resolved `DataType`. All column refs -> positional indices. No unresolved names escape the Analyzer.
3. **Optimizer is single-path ON**: CBO pipeline is always on for execution. `tipg.use_optimizer` is retained only as compatibility/readback GUC (accepted as no-op).
4. **Volcano iterator model**: Operators implement `open() -> next() -> close()` lifecycle for streaming execution.
5. **EXPLAIN matches execution for SELECT/WITH**: both paths share analyze+rewrite entry before optimizer planning.
6. **Type coercion dual rules**: `common_type()` (Text wins for mixed types) vs `comparison_target_type()` (non-Text wins for comparisons). Both intentional, both in `coercion.rs`.
7. **Parser rewrites are parse-compat only**: semantic rewrite authority is post-Analyzer.
8. **Decorrelation contract**: EXISTS/NOT EXISTS subqueries are decorrelated into SemiJoin/AntiJoin in the optimizer rewrite phase, not in the executor.

## Tests

| Kind | Location |
|------|----------|
| Unit tests | `src/sql/**/tests.rs` and module `#[cfg(test)]` blocks |
| SQL regressions | `tests/*.sql` + `tests/*.expected` (557 test files) |

## Commands

```bash
cargo test
./scripts/regression_gate.sh --skip-orm
```
