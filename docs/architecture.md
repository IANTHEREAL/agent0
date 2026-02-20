# pg-tikv Architecture Design

> PostgreSQL-compatible distributed SQL database on TiKV.
> Last updated: 2026-02-17

## 1. Design Principles

These principles govern every design decision, code change, and review.

### 1.1 PostgreSQL is the Specification

All SQL semantics, type coercion rules, catalog behavior, error codes, and wire protocol responses must align with PostgreSQL. When in doubt, test against real PostgreSQL 17 and match its behavior. Never invent custom semantics.

### 1.2 Single-Path, Deterministic Execution

Every query follows exactly one code path. No hidden fallback, no runtime "try-new-then-old". If a path exists, it is **the** path. If the new path cannot handle a query shape, the system returns an error — it does not silently fall back to legacy code.

### 1.3 Correctness First, Then Speed

Fix at the root layer where the invariant is broken. Never use wire/test-layer masking to hide internal inconsistency. Never hack test expectations to match broken behavior.

### 1.4 Clear Module Boundaries

Each behavior has a clear module boundary and a single source of truth:
- **Analyzer** owns name resolution, type inference, and scope checking.
- **Optimizer** owns plan selection and cost estimation.
- **Executor/Operators** own physical execution.
- **Catalog** owns metadata queries.
- **Storage** owns key encoding and TiKV interaction.

No module reaches into another's responsibilities.

### 1.5 Structural Fixes Over Tactical Patches

Deliver structurally correct fixes, not tactical patches. If a fix requires touching 3+ callsites for the same symptom, the abstraction is wrong — fix the abstraction.

### 1.6 Multi-tenancy Isolation is Non-Negotiable

All persistent data must be isolated per keyspace. Process-level global state is limited to in-memory caches, configuration, and logging.

---

## 2. System Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    PostgreSQL Clients                        │
│          (psql, pgcli, ORMs, applications, agents)          │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ PostgreSQL Wire Protocol (pgwire)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                       pg-tikv Server                         │
│  ┌───────────────────────────────────────────────────────┐  │
│  │               Protocol Layer (pgwire)                  │  │
│  │  • Simple Query Handler    • Extended Query Handler    │  │
│  │  • Startup/Auth Handler    • COPY Handler              │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │                    SQL Layer                            │  │
│  │  ┌─────────┐  ┌──────────┐  ┌──────────┐             │  │
│  │  │ Parser  │→ │ Analyzer │→ │Optimizer │             │  │
│  │  │(sqlparser│  │(TypedExpr│  │  (CBO)   │             │  │
│  │  │   -rs)  │  │ /Analyzed│  │LogicalPlan│             │  │
│  │  │         │  │  Query)  │  │→Physical  │             │  │
│  │  └─────────┘  └──────────┘  └──────────┘             │  │
│  │       ↓              ↓             ↓                   │  │
│  │  ┌──────────────────────────────────────┐             │  │
│  │  │     Executor + Physical Operators     │             │  │
│  │  │  scan | filter | project | sort | agg │             │  │
│  │  │  join | hash_join | window | limit    │             │  │
│  │  └──────────────────────────────────────┘             │  │
│  │       ↓                                                │  │
│  │  ┌──────────────────────────────────────┐             │  │
│  │  │  Catalog (35+ pg_catalog/info_schema) │             │  │
│  │  └──────────────────────────────────────┘             │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │                  Storage Layer                         │  │
│  │  • Key Encoding        • Schema Management             │  │
│  │  • Index Management    • Transaction Wrapper           │  │
│  │  • Keyspace Isolation  • Statistics Persistence        │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ gRPC (TiKV Client Protocol)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                       TiKV Cluster                           │
│    TiKV nodes (Raft consensus) + PD (Placement Driver)      │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. Execution Pipeline

### 3.1 The Single Path

Every query follows this pipeline. There are no alternative paths.

```
Client SQL string
    │
    ▼
┌─ Parser (sqlparser-rs) ──────────────────────────────────────┐
│  SQL text → AST (Vec<Statement>)                             │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Dispatcher (executor/core/dispatch.rs) ─────────────────────┐
│  Route AST node to handler:                                   │
│  ├─ DDL (CREATE/ALTER/DROP) → ddl.rs                         │
│  ├─ DML (INSERT/UPDATE/DELETE) → dml_analyzed.rs             │
│  ├─ SELECT/VALUES/SET-OP → Analyzer pipeline (below)         │
│  ├─ Transaction control (BEGIN/COMMIT/ROLLBACK)              │
│  ├─ Settings (SET/SHOW/RESET)                                │
│  └─ Special (EXPLAIN, ANALYZE, COPY, CALL)                   │
└──────────────────────────────────────────────────────────────┘
    │ (SELECT path)
    ▼
┌─ View Expansion ─────────────────────────────────────────────┐
│  expand_views_in_query(): recursively inline view definitions │
│  (happens before Analyzer, so views are transparent)          │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Privilege Check ────────────────────────────────────────────┐
│  require_table_privilege(Select) on every base table          │
│  (uses CatalogSnapshot::base_table_full_names())              │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Analyzer (src/sql/analyzer/) ───────────────────────────────┐
│  AST → AnalyzedQuery / TypedExpr                              │
│  • Name resolution: column refs → positional indices          │
│  • Type inference: every node carries resolved DataType       │
│  • Scope checking: correlated subqueries tracked              │
│  • GROUP BY compliance validation                             │
│  • Function/aggregate resolution                              │
│  OUTPUT: AnalyzedQuery with output_schema                     │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Optimizer Pipeline (Always On) ─────────────────────────────┐
│  ├─ LogicalPlanner: AnalyzedQuery → LogicalPlan              │
│  ├─ Load table statistics from TableStatsCache               │
│  ├─ PhysicalPlanner: LogicalPlan → PhysicalPlan              │
│  │   (selectivity estimation, cardinality propagation)       │
│  ├─ build.rs: PhysicalPlan → BoxedOperator                   │
│  └─ Execute operator tree → results                          │
│  NOTE: tipg.use_optimizer is compatibility/readback only.    │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ QueryPlan (executor/select/analyzed/query_plan.rs) ─────────┐
│  AnalyzedQuery → QueryPlan (all routing decisions in 1 place) │
│  • ExecutionPath: SetOperation / Values / Tableless /         │
│                   SingleTable / Join                          │
│  • WhereStrategy: AllSync / AllAsync / Split / None           │
│  • OrderByStrategy: Inline / Deferred / None                 │
│  • ProjectionStrategy: Sync / NeedsMaterialization           │
│  • DistinctStrategy: None / Distinct / DistinctOn            │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Physical Execution (Volcano iterator model) ────────────────┐
│  Operator tree: each operator has open() → next() → close()  │
│                                                               │
│  Example pipeline for: SELECT a FROM t WHERE b > 5 ORDER BY a│
│                                                               │
│  LimitOperator                                                │
│    └─ SortOperator (ORDER BY a)                               │
│        └─ ProjectOperator (SELECT a)                          │
│            └─ FilterOperator (WHERE b > 5)                    │
│                └─ TableScanOperator (FROM t)                  │
│                    └─ TiKV (scan rows)                        │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
  ExecuteResult::Select { columns, column_types, rows }
    │
    ▼
  pgwire response → Client
```

### 3.2 CTE Pre-materialization

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

### 3.3 DML Path

INSERT, UPDATE, DELETE also flow through the Analyzer:

```
INSERT/UPDATE/DELETE
    → Analyzer::analyze_{insert,update,delete}()
    → TypedExpr for values/conditions
    → executor/dml_analyzed.rs: execute with typed expressions
    → Trigger activation (BEFORE inline, AFTER deferred to commit)
```

### 3.4 EXPLAIN Path

EXPLAIN uses the same analyzed pipeline as execution to guarantee consistency:

```
EXPLAIN query
    → expand_views_in_query()
    → Analyzer::analyze_query()
    → generate_plan_from_analyzed()
        (same index selection logic as runtime: choose_best_access_path)
    → Format to PostgreSQL-compatible EXPLAIN output
```

---

## 4. Module Architecture

### 4.1 Analyzer (`src/sql/analyzer/`)

**Purpose**: Transform raw SQL AST into typed, resolved intermediate representation.

**Key contract**: Every `TypedExpr` node carries a resolved `DataType`. All column references are resolved to positional indices. No unresolved names escape the Analyzer.

| File | Purpose |
|------|---------|
| `types.rs` | Typed IR definitions: `TypedExpr`, `TypedExprKind`, `AnalyzedQuery`, `AnalyzedStatement` |
| `query.rs` | Query-level analysis: SELECT/INSERT/UPDATE/DELETE, GROUP BY validation |
| `expr.rs` | Expression analysis: name resolution, type inference for all expression kinds |
| `scope.rs` | Scope chain for nested queries, correlated subquery tracking |
| `catalog.rs` | `CatalogSnapshot`: table/column/function resolution interface |
| `dml.rs` | DML-specific analysis (INSERT/UPDATE/DELETE type checking) |
| `literal.rs` | Literal value parsing and type inference |

### 4.2 Optimizer (`src/sql/optimizer/`)

**Purpose**: Cost-based query optimization. Transforms `AnalyzedQuery` into an optimized physical plan.

**Current status**: Always ON single path. Covers single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, and DISTINCT ON. Includes selectivity estimation and index selection. `tipg.use_optimizer` remains compatibility/readback only.

```
AnalyzedQuery
    → LogicalPlanner::build()     → LogicalPlan (abstract tree)
    → PhysicalPlanner::plan()     → PhysicalPlan (with cost estimates)
    → PhysicalPlan::build_operators() → BoxedOperator (executable)
```

| File | Purpose |
|------|---------|
| `logical_planner.rs` | AnalyzedQuery → LogicalPlan conversion |
| `logical_plan.rs` | LogicalPlan IR definition |
| `physical_planner.rs` | LogicalPlan → PhysicalPlan with operator selection |
| `build.rs` | PhysicalPlan → BoxedOperator, wires execution context |
| `selectivity.rs` | Selectivity estimation from column statistics |
| `statistics.rs` | `ColumnStatistics`, `TableStatistics` structures |

**Roadmap**:
- Phase 3: Join reordering, subquery decorrelation, predicate pushdown (#705)
- Phase 4: Plan cache for prepared statement reuse (#707)

### 4.3 Physical Operators (`src/sql/operators/`)

**Purpose**: Streaming execution via Volcano iterator model. Each operator implements `open() → next() → close()`.

| Operator | File | Description |
|----------|------|-------------|
| TableScan / IndexScan | `scan.rs` | TiKV row retrieval (full scan or index lookup) |
| Filter | `filter.rs` | WHERE clause predicate evaluation |
| Project | `project.rs` | Column selection + expression evaluation |
| NestedLoopJoin | `join.rs` | Streaming outer, materializes right side only |
| HashJoin | `hash_join.rs` | Equi-join with in-memory hash table |
| HashAggregate | `aggregate.rs` | GROUP BY with incremental aggregation |
| Sort | `sort.rs` | ORDER BY (full materialization required) |
| Distinct | `distinct.rs` | DISTINCT / UNION deduplication |
| Window | `window.rs` | Window functions (OVER clauses) |
| Limit | `limit.rs` | LIMIT / OFFSET |
| SetOperation | `set_operation.rs` | UNION / INTERSECT / EXCEPT |
| CTE | `cte.rs` | Common Table Expression iteration |
| TableFunction | `table_function.rs` | UNNEST, generate_series, etc. |

### 4.4 Executor (`src/sql/executor/`)

**Purpose**: Statement dispatch, DDL/DML execution, and SELECT orchestration.

```
executor/
├── core/                  # Statement dispatch + infrastructure
│   ├── dispatch.rs        # Route statements to handlers
│   ├── statement.rs       # Privilege checks + common helpers
│   ├── analyze.rs         # ANALYZE command (statistics collection)
│   ├── view_rewrite.rs    # View expansion + privilege enforcement
│   └── catalog_prefetch.rs# Batch catalog lookups
├── select/
│   └── analyzed/          # Single-path SELECT executor
│       ├── mod.rs         # Main orchestrator (try_execute_analyzed)
│       ├── query_plan.rs  # QueryPlan routing decisions
│       ├── rewrite.rs     # Expression simplification
│       └── subquery.rs    # Subquery execution
├── cte.rs                 # CTE pre-materialization
├── dml_analyzed.rs        # Analyzed INSERT/UPDATE/DELETE
├── ddl.rs                 # DDL dispatch
├── triggers.rs            # Trigger dispatch
└── procedure.rs           # CALL (stored procedures)
```

### 4.5 Expression System (`src/sql/expr/`)

**Purpose**: Runtime evaluation of typed expressions against rows.

| File | Purpose |
|------|---------|
| `typed_eval.rs` | Core evaluator: `eval_typed_expr(expr, row, ctx) → Value` (~2,800 lines) |
| `typed_fold.rs` | AST tree transformation |
| `typed_rewrite.rs` | Constant folding, expression rewriting |
| `operators.rs` | Binary/unary operator implementations |
| `numeric.rs` | Numeric arithmetic with overflow handling |
| `functions/` | 14 categories: array, datetime, encoding, fs9, fts, json, math, misc, pg_compat, regex, string, uuid, vector |

### 4.6 Type System (`src/sql/types/`)

**Purpose**: Type inference, coercion, and PostgreSQL type mapping.

| File | Purpose |
|------|---------|
| `registry.rs` | `FunctionRegistry`: 200+ builtin function signatures |
| `infer.rs` | `TypeInferrer`: core type inference logic |
| `coercion.rs` | `common_type()` (Text wins) + `comparison_target_type()` (non-Text wins) |
| `cast.rs` | CAST between any two types |
| `mapping.rs` | PostgreSQL DataType <-> internal Value types |

**Key design**: Two coercion functions with intentionally different behavior:
- `common_type(T1, T2)` → common supertype for operators (Text wins for mixed types)
- `comparison_target_type(T1, T2)` → coercion target for comparisons (non-Text wins)

### 4.7 Catalog (`src/sql/catalog/`)

**Purpose**: PostgreSQL system catalog compatibility. 35+ virtual table implementations for `pg_catalog` and `information_schema`.

Implements: `pg_class`, `pg_attribute`, `pg_type`, `pg_index`, `pg_namespace`, `pg_constraint`, `pg_proc`, `pg_roles`, `pg_trigger`, `pg_views`, `pg_description`, `pg_database`, `pg_extension`, `pg_depend`, `pg_am`, `pg_collation`, `pg_enum`, `pg_range`, `pg_sequence`, `pg_attrdef`, `pg_db_role_setting`, `tables`, `columns`, `schemata`, `sequences`, `routines`, `key_column_usage`, `table_constraints`, `constraint_column_usage`, `check_constraints`, `referential_constraints`, `table_privileges`, `pg_indexes`, and more.

### 4.8 Index Planning (`src/sql/planner.rs`)

**Purpose**: Access path selection. Given a query's WHERE predicates and available indexes, choose the best scan strategy.

Supports:
- B-tree index scans (equality, range, prefix)
- GIN index scans (full-text search, array containment)
- Expression indexes (indexes on computed expressions)
- Partial indexes (indexes with WHERE predicates)
- Multi-column index prefix matching

### 4.9 Statistics (`src/sql/stats.rs` + `optimizer/statistics.rs`)

**Purpose**: Per-tenant table statistics for cost-based optimization.

```
ANALYZE table_name
    → Scan table rows
    → Compute per-column: distinct count, null fraction, MCV, histogram
    → Store in TableStatsCache (per-tenant, in-memory)
    → Used by PhysicalPlanner for selectivity estimation
```

`TableStatsCache` is owned by `TenantEntry` in the connection pool — when the tenant entry is reaped, statistics are dropped automatically.

---

## 5. Protocol Layer

### 5.1 Connection Lifecycle

```
1. Client connects via TCP
2. pgwire Startup message received
3. parse_tenant_username("tenant.user") → (keyspace, username)
4. Authentication challenge (CleartextPassword)
5. Verify credentials against TiKV-stored auth data
6. init_executor(): acquire TiKV client from pool (keyspace-isolated)
7. Session ready for queries
```

### 5.2 Query Handling

| Protocol | Handler | Description |
|----------|---------|-------------|
| Simple Query | `on_query()` | Direct SQL text, supports `;`-separated statements |
| Extended Query | `on_bind()` + `on_execute()` | Prepared statements with parameter binding |
| COPY | `on_copy_data()` + `on_copy_done()` | Bulk data loading |
| Describe | `on_describe_statement()` | Return parameter types + result columns |

### 5.3 Handler Decomposition

The protocol handler (`src/protocol/handler/`) is decomposed into focused modules:

| Module | Purpose |
|--------|---------|
| `dynamic.rs` | `DynamicPgHandler`: main pgwire handler |
| `view_infer.rs` | View definition inference for Describe |
| `schema_resolve.rs` | Schema resolution for extended protocol |
| `type_infer.rs` | Result field type inference |
| `encode/types.rs` | DataType → PostgreSQL OID mapping |

---

## 6. Storage Layer

### 6.1 Key Layout

All keys are keyspace-prefixed by TiKV for multi-tenant isolation.

| Key Pattern | Content |
|-------------|---------|
| `_sys_next_table_id` | Auto-increment counter for table IDs |
| `_sys_schema_{table}` | `TableSchema` (bincode serialized) |
| `_sys_view_{name}` | View SQL definition |
| `_sys_matview_{name}` | Materialized view SQL definition |
| `_sys_proc_{name}` | Stored procedure SQL definition |
| `t_{table_id}_{pk_values}` | Data row (bincode serialized) |
| `i_{table_id}_{idx_id}_{vals}` | Index entry (unique: pk value, non-unique: empty) |

### 6.2 Transaction Model

- Pessimistic transactions only (`TikvStore::begin()`)
- Snapshot Isolation (prevents dirty reads, non-repeatable reads, write-skew for indexed columns)
- Autocommit: automatic retry (10 attempts, exponential backoff) on TiKV conflict errors
- Explicit transactions: no automatic retry (application must handle conflicts)

### 6.3 Multi-tenancy

```
Username format: "tenant.user" or "tenant:user"
    → parse_tenant_username() extracts keyspace
    → pool.acquire(keyspace) returns tenant-isolated TiKV client
    → All KV operations scoped to keyspace prefix
    → Stats cache, schema cache isolated per tenant
```

---

## 7. Transaction & Session Management

### 7.1 Session State (`src/sql/session.rs`)

Per-connection state:
- Current database, search path, timezone
- Transaction state (idle, active, failed)
- Sequence value cache (NEXTVAL/CURRVAL)
- GUC settings (SET/SHOW/RESET)
- Statement timeout

### 7.2 Transaction Flow

```
Autocommit (single statement):
    begin() → execute_statement() → commit()
    On conflict: retry up to 10x with exponential backoff

Explicit transaction:
    BEGIN → execute_statements... → COMMIT/ROLLBACK
    SAVEPOINT/RELEASE/ROLLBACK TO for partial rollback
    No automatic retry — application handles conflicts
```

### 7.3 Trigger Execution

```
BEFORE triggers: inline during DML execution (src/sql/triggers.rs)
    → Body compiled and cached in TriggerCache
    → Evaluated synchronously before row modification

AFTER triggers: deferred to commit (src/sql/trigger_worker.rs)
    → Queued during DML execution
    → Fired asynchronously after successful commit
    → Background tokio task processes queue
```

---

## 8. Current State & Roadmap

### Completed

| Milestone | Description | Key Files |
|-----------|-------------|-----------|
| Analyzer pipeline | Single-path typed IR for all SELECT queries | `analyzer/` |
| CBO optimizer | LogicalPlan → PhysicalPlan → BoxedOperator pipeline (default ON, multi-table + set ops + CTEs + window + DISTINCT ON) | `optimizer/` |
| ANALYZE + stats | Selectivity estimation + stats cache + warm-up | `optimizer/selectivity.rs`, `stats.rs` |
| Legacy cleanup | Removed 10 legacy code items, single execution path | All |
| Privilege enforcement | SELECT privilege on every base table | `executor/core/statement.rs` |
| 35+ catalog views | pg_catalog + information_schema compatibility | `catalog/` |
| Full-text search | GIN indexes + Chinese tokenizer | `gin.rs`, `fts.rs` |
| SQL Rewriter phase | Shared analyzed rewrite entry for execution/EXPLAIN, no SELECT/WITH runtime fallback | `executor/core/analyze_rewrite.rs`, `rewriter.rs`, `parser.rs` |

### In Progress

| Phase | Description | Issue |
|-------|-------------|-------|
| CBO Phase 3 | Join reordering, subquery decorrelation, predicate pushdown | #705 |
| CBO Phase 4 | Plan cache for prepared statement optimization | #707 |

### Future

| Feature | Description | Issue |
|---------|-------------|-------|
| Parallel execution | Distributed query execution across TiKV regions | #708 |

---

## 9. Invariants (Must Always Hold)

1. **Single execution path**: No hidden fallback. If the Analyzer succeeds, execution uses typed expressions only.
2. **TypedExpr completeness**: Every expression node carries a resolved `DataType`. No unresolved names escape the Analyzer.
3. **EXPLAIN = execution**: EXPLAIN uses the same index selection logic as runtime. They cannot diverge.
4. **Keyspace isolation**: All persistent data is scoped to the tenant's keyspace. No cross-tenant data access is possible.
5. **View transparency**: Views are expanded before the Analyzer runs. The rest of the pipeline never sees view references.
6. **CTE materialization before main query**: All CTEs in a WITH clause are fully executed and materialized before the main query begins.
7. **Privilege check before scan**: SELECT privilege is enforced before any table data is read.
8. **Autocommit retry is safe**: Only autocommit (implicit) transactions are retried. Explicit transactions never retry automatically.
9. **Trigger ordering**: BEFORE triggers execute inline (blocking). AFTER triggers execute asynchronously after commit.
10. **Test expectations match PostgreSQL**: No `.expected` file may be updated without verifying against real PostgreSQL 17.7 output.
