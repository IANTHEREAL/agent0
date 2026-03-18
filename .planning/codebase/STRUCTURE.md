# Codebase Structure

**Analysis Date:** 2026-03-17

## Directory Layout

```
db9-server/
├── src/                        # All application source code
│   ├── main.rs                 # Server entry point (TCP accept loop, TLS, worker startup)
│   ├── cli.rs                  # CLI argument parser (--port, --host, --keyspace, --tls-*)
│   ├── config.rs               # ServerConfig, env helpers, auth mode
│   ├── session_context.rs      # Tokio task-local session state (timezone, search_path, keyspace)
│   ├── observability.rs        # TenantObservability, ConnectionGuard, global registry
│   ├── pool.rs                 # TikvClientPool — per-tenant TikvStore handles + idle eviction
│   ├── tls.rs                  # TLS acceptor setup (pgwire + WebSocket)
│   ├── auth/                   # Authentication and RBAC
│   ├── cron/                   # pg_cron-compatible scheduler
│   ├── extensions/             # HTTP extensions + fs9 file system
│   ├── model/                  # Core data model types
│   ├── protocol/               # pgwire protocol handler
│   ├── sql/                    # SQL engine (~118K lines)
│   ├── storage/                # TiKV storage layer
│   ├── txn/                    # Transaction savepoints
│   └── worker/                 # Unified background task engine
├── tests/                      # SQL integration tests (878 files: .sql + .expected/.errors/.assert)
├── tests_pending/              # Integration tests not yet passing
├── orm-tests/                  # TypeORM, Prisma, Sequelize compatibility tests (npm)
├── e2e/                        # End-to-end tests
├── auto_testing/               # Regression gate and automated test infrastructure
├── scripts/                    # integration_test.py and other utilities
├── docs/                       # Architecture and feature documentation
├── prds/                       # Product requirement documents
├── sdk/                        # Client SDK
├── db9-admin/                  # Admin tooling
├── cloud-admin-portal/         # Cloud admin portal (separate app)
├── crates/                     # Vendored/local crates (pgwire fork)
├── vendor/                     # Vendored Rust dependencies
├── deploy/                     # Deployment configuration
├── tools/                      # Developer tools
├── Cargo.toml                  # Workspace manifest
├── Cargo.lock                  # Locked dependency tree
├── Dockerfile                  # Production image
├── Dockerfile.dev              # Development image
├── Makefile                    # Build/test shortcuts
└── CLAUDE.md                   # Living knowledge base for this codebase
```

## Directory Purposes

**`src/sql/` — SQL Engine:**
- Purpose: Everything SQL: parsing, analysis, planning, execution, catalog, DDL, DML, functions
- Key files: `src/sql/mod.rs` exposes `Executor`, `Session`, `ExecuteResult`, `ExecuteResults`

```
src/sql/
├── analyzer/                   # Semantic analysis → AnalyzedQuery / TypedExpr
│   ├── expr/                   # Expression analysis (coercion, functions, literals, operators)
│   ├── query/                  # Query analysis (from_clause, group_by, projection, set_expr)
│   ├── types/                  # TypedExpr, AnalyzedQuery, TypedExprKind definitions
│   ├── scope.rs                # Scope chain for column resolution
│   └── catalog.rs              # Catalog trait + CatalogSnapshot
├── optimizer/                  # CBO: AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator
│   ├── logical_planner/        # AnalyzedQuery → LogicalPlan (pure structural translation)
│   ├── rewrite/                # Plan rewrites: decorrelation, predicate pushdown
│   ├── join_reorder/           # DPccp cost-based join reordering
│   ├── physical_planner/       # LogicalPlan → PhysicalPlan
│   ├── build/                  # PhysicalPlan → BoxedOperator (synchronous tree walk)
│   └── selectivity/            # Selectivity estimation from column statistics
├── operators/                  # Physical operators (Volcano iterator model)
│   ├── hash_join/              # Equi-join with hash table
│   ├── window/                 # Window functions
│   ├── hash_semi_join.rs       # Semi/anti-join for EXISTS decorrelation
│   ├── hnsw_scan.rs            # HNSW ANN scan operator
│   ├── scan.rs                 # TableScanOperator + IndexScanOperator
│   ├── aggregate.rs            # HashAggregate
│   ├── join.rs                 # NestedLoopJoin
│   └── executor.rs             # BoxedOperator trait definition
├── executor/                   # DDL/DML dispatch + SELECT execution
│   ├── core/                   # Statement dispatch infrastructure
│   │   ├── dispatch/           # execute() / execute_single() state machine
│   │   ├── view_rewrite/       # View expansion (sync, pre-analysis)
│   │   ├── catalog_prefetch/   # Batch catalog lookups before analysis
│   │   ├── plan_cache.rs       # PreparedPlanCache
│   │   ├── prepared_stmt.rs    # PreparedStatement storage
│   │   ├── statement.rs        # Statement classification + top-level dispatch
│   │   ├── stmt_ddl.rs         # DDL statement routing
│   │   ├── stmt_dml.rs         # DML statement routing
│   │   ├── stmt_query.rs       # Query statement routing
│   │   └── query_exec.rs       # execute_query entry + CTE context build
│   ├── select/analyzed/        # Single-path SELECT executor (try_execute_analyzed)
│   ├── dml_analyzed/           # Analyzed INSERT / UPDATE / DELETE
│   └── procedure/              # Stored procedures + materialized views
├── expr/                       # Expression system
│   ├── functions/              # 16 function categories (array, datetime, embedding,
│   │                           #   encoding, fs9, fts, http, json, math, misc,
│   │                           #   pg_compat, regex, string, uuid, vector)
│   ├── typed_eval/             # Runtime evaluator for TypedExpr
│   └── traverse/               # Expression tree traversal (map_children)
├── catalog/                    # information_schema + pg_catalog + cron virtual tables (54 files)
├── types/                      # Type inference, coercion, mapping
│   ├── registry/               # FunctionRegistry (aggregate_window, json, math, etc.)
│   └── cast/                   # CAST rules between types
├── ddl/                        # DDL: CREATE/ALTER/DROP
│   ├── alter_table/            # ALTER TABLE sub-operations
│   ├── create_index.rs         # CREATE INDEX (btree, GIN, HNSW)
│   ├── create_table.rs         # CREATE TABLE
│   ├── drop.rs                 # DROP TABLE/INDEX/VIEW/etc.
│   └── view.rs                 # CREATE/ALTER VIEW
├── dml/                        # DML helpers
│   ├── foreign_keys/           # FK constraint validation
│   ├── defaults.rs             # Column default evaluation
│   ├── insert.rs               # INSERT helpers
│   ├── update.rs               # UPDATE helpers
│   └── delete.rs               # DELETE helpers
├── session/                    # Per-connection state
│   ├── mod.rs                  # Session struct, TransactionState
│   └── settings.rs             # SessionSettings (GUC parameters)
├── hnsw/                       # HNSW vector index (cache, storage, usearch FFI)
├── planner/                    # Index selection, scan strategy, HNSW predicate detection
├── explain/                    # EXPLAIN output formatting
├── triggers/                   # Trigger subsystem (cache, before, rewrite, enqueue, execute)
├── sequences/                  # SEQUENCE management
├── plpgsql/                    # PL/pgSQL parser + executor
├── parser/                     # SQL parser wrappers (operator_rewrite, preprocess)
├── rls/                        # Row-Level Security (cache, policy evaluation)
└── stats.rs                    # TableStatsCache (per-tenant, in-memory)
```

**`src/protocol/` — pgwire Protocol:**
- Purpose: Implements pgwire protocol; bridges network to SQL executor
- Contains: `DynamicPgHandler` (per-connection), `DynamicHandlerFactory`, value encoding, portal state

```
src/protocol/
├── copy_format.rs              # COPY format parsing (CSV, TEXT, BINARY)
└── handler/
    ├── dynamic/                # Main handler (mod.rs, startup.rs, query.rs, copy.rs)
    ├── encode/                 # Value encoding + PostgreSQL type OID mapping
    ├── params/                 # Parameter counting + decoding for prepared statements
    ├── copy/                   # COPY context management
    ├── portal.rs               # Portal state + suspended queries (cursor support)
    ├── query_parser.rs         # Db9QueryParser (pgwire QueryParser trait)
    ├── server_params.rs        # ParameterStatus provider (server_version, etc.)
    ├── tenant.rs               # Multi-tenancy username parsing (<keyspace>.<user>)
    └── errors.rs               # SQLSTATE mapping + error helpers
```

**`src/storage/` — TiKV Storage:**
- Purpose: All TiKV reads/writes; key construction; value serialization
- Contains: `TikvStore` (main client), encoding utilities, backpressure

```
src/storage/
├── encoding/                   # Key and value encoding
│   ├── data_keys.rs            # Row and index key construction (d_{db_id}_* v2 format)
│   ├── metadata_keys.rs        # Schema, sequence, cron, worker key construction
│   ├── value_encoding.rs       # Row column value encoding
│   └── serialization.rs        # bincode/serde serialization helpers
├── tikv_store/                 # TikvStore implementation (one file per concern)
│   ├── mod.rs                  # TikvStore struct + begin()/commit()/scan() etc.
│   ├── tables.rs               # Table schema CRUD
│   ├── indexes.rs              # Index operations
│   ├── schemas.rs              # Schema (namespace) management
│   ├── sequences.rs            # Sequence value storage
│   ├── statistics.rs           # Table statistics persistence
│   ├── cron.rs                 # Cron job storage
│   ├── worker.rs               # Worker task queue
│   └── migrations.rs           # Storage migrations
├── backpressure.rs             # TiKV write backpressure control
└── kv_stats.rs                 # KV read statistics tracking (task-local)
```

**`src/worker/` — Background Tasks:**
- Purpose: Unified async task engine processing the global TiKV task queue
- Key files: `engine.rs` (main loop), `types.rs` (task types), `gc.rs` (cleanup), `config.rs`, `metrics.rs`

**`src/cron/` — pg_cron Scheduler:**
- Purpose: pg_cron-compatible scheduling integrated with worker engine
- Key files: `parser.rs`, `types.rs`, `config.rs`, `worker.rs`, `process_list.rs`

**`src/extensions/fs/` — fs9 File System:**
- Purpose: fs9 file operations with TiKV + optional S3 backend; WebSocket server for SDK
- Key files: `backend.rs`, `decoders.rs`, `streaming.rs`, `glob.rs`, `ws/` (WebSocket protocol)

**`src/auth/` — Authentication:**
- Purpose: Password auth and RBAC
- Key files: `db9_auth.rs` (AuthManager), `password.rs`, `rbac.rs`

**`src/model/` — Data Model Types:**
- Purpose: Core value types shared across all layers
- Key files: `DataType`, `Value`, `Row`, `TableSchema`

**`src/txn/` — Transaction Savepoints:**
- Purpose: PostgreSQL-compatible SAVEPOINT semantics over TiKV transactions
- Key files: `savepoints.rs` (before-image recording), `state.rs`

**`tests/` — SQL Integration Tests:**
- Purpose: SQL correctness tests against a running server
- Pattern: Each test is `<N>_<name>.sql` + `<N>_<name>.expected` (or `.errors` or `.assert`)
- Run via: `python3 scripts/integration_test.py`

**`orm-tests/` — ORM Compatibility Tests:**
- Purpose: TypeORM, Prisma, Sequelize compatibility
- Run via: `cd orm-tests && npm test`

**`auto_testing/` — Regression Gate:**
- Purpose: CI regression gate; lists which integration tests must pass
- Key file: `auto_testing/regression_gate.list`

## Key File Locations

**Entry Points:**
- `src/main.rs`: Server startup, TCP accept loop, TLS, worker/fs9 init
- `src/sql/executor/core/dispatch/mod.rs`: `Executor::execute` — SQL dispatch entry point
- `src/protocol/handler/dynamic/query.rs`: pgwire query handler entry

**Configuration:**
- `src/config.rs`: `ServerConfig` (timeouts, max connections, auth mode, embedding)
- `src/session_context.rs`: Task-local GUC propagation (timezone, search_path, keyspace)
- `src/sql/session/settings.rs`: Per-session GUC settings

**Core Logic:**
- `src/sql/analyzer/mod.rs`: `Analyzer` struct — SQL AST → TypedIR
- `src/sql/optimizer/mod.rs`: CBO pipeline entry; `optimize()` function
- `src/sql/executor/select/analyzed/mod.rs`: `try_execute_analyzed` — primary SELECT execution
- `src/storage/tikv_store/mod.rs`: `TikvStore` — all TiKV interactions
- `src/storage/encoding/data_keys.rs`: Row and index key construction

**Testing:**
- `tests/`: SQL integration tests (`.sql` + `.expected` pairs)
- `auto_testing/regression_gate.list`: Mandatory passing tests for CI
- `scripts/integration_test.py`: Integration test runner

## Naming Conventions

**Files:**
- Snake_case Rust module files: `create_table.rs`, `foreign_keys.rs`, `hash_join.rs`
- Module directories use snake_case matching the module name: `hash_join/`, `alter_table/`
- Test files named `<N>_<descriptive_name>.sql` with numeric prefix for ordering

**Directories:**
- Snake_case throughout; no camelCase directories
- Domain-named: `ddl/`, `dml/`, `catalog/`, `operators/`, `optimizer/`

**Rust types:**
- Structs/enums: PascalCase (`TikvStore`, `AnalyzedQuery`, `BoxedOperator`)
- Functions/methods: snake_case (`execute_query`, `try_execute_analyzed`)
- Constants: SCREAMING_SNAKE_CASE (`DEFAULT_STATEMENT_TIMEOUT_MS`)

**SQL test files:**
- `<N>_<name>.sql` — SQL statements to execute
- `<N>_<name>.expected` — expected stdout output (highest priority)
- `<N>_<name>.errors` — expected stderr/error output
- `<N>_<name>.assert` — custom assertion logic (lowest priority)

## Where to Add New Code

**New SQL function:**
- Implementation: `src/sql/expr/functions/<category>.rs` (pick closest category or create new)
- Registration: Call `register(&mut map)` in `src/sql/expr/functions/mod.rs`
- Type signature: `src/sql/types/registry/` (add to appropriate registry file)

**New SQL statement (DDL):**
- Parser handling: `src/sql/executor/core/stmt_ddl.rs` (add to `execute_ddl_statement` match)
- Implementation: `src/sql/ddl/` (new file or extend existing)
- Classification: `src/sql/executor/core/statement.rs` (`classify_statement`)

**New SQL statement (DML):**
- Parser handling: `src/sql/executor/core/stmt_dml.rs`
- Implementation: `src/sql/executor/dml_analyzed/` or `src/sql/dml/`

**New catalog virtual table:**
- Implementation: `src/sql/catalog/` (new file, add to `virtual_tables.rs` dispatch)
- Pattern: Return `(TableSchema, Vec<Row>)` from async function

**New physical operator:**
- Implementation: `src/sql/operators/<name>.rs`
- Register in: `src/sql/operators/mod.rs` (pub use)
- Wire into plan: `src/sql/optimizer/build/` (add PhysicalNode variant handling)

**New TiKV storage operation:**
- Implementation: `src/storage/tikv_store/<concern>.rs` (add method to `TikvStore`)
- Key construction: `src/storage/encoding/data_keys.rs` or `metadata_keys.rs` (use v2 format: `d_{db_id}_*`)

**New background task type:**
- Task type: `src/worker/types.rs`
- Handler: `src/worker/engine.rs` (add dispatch arm)

**New GUC parameter:**
- Session-scoped: `src/sql/session/settings.rs` (add to `SessionSettings`)
- Server-wide: `src/config.rs` (add to `ServerConfig`)
- SHOW/SET handling: `src/sql/executor/core/guc.rs`

## Special Directories

**`vendor/`:**
- Purpose: Vendored Rust dependencies for offline builds
- Generated: Partially (cargo vendor)
- Committed: Yes

**`crates/pgwire`:**
- Purpose: Local fork of the pgwire library crate
- Generated: No
- Committed: Yes

**`target/`:**
- Purpose: Cargo build artifacts
- Generated: Yes
- Committed: No

**`tests_pending/`:**
- Purpose: Integration tests that describe known-failing behavior; not in regression gate
- Generated: No
- Committed: Yes

**`.planning/`:**
- Purpose: GSD planning documents (architecture analysis, implementation plans)
- Generated: By GSD tooling
- Committed: Yes

---

*Structure analysis: 2026-03-17*
