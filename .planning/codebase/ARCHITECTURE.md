# Architecture

**Analysis Date:** 2026-03-17

## Pattern Overview

**Overall:** PostgreSQL-compatible distributed SQL database — layered pipeline architecture with Volcano-model physical execution.

**Key Characteristics:**
- Single deterministic execution path: Analyzer → Typed IR → Optimizer (CBO) → Physical Operators → TiKV
- No hidden fallback paths: the `db9.use_optimizer` GUC is always-on; SET to off logs a notice and is ignored
- Multi-tenant: every persistent key is keyspace-scoped (`d_{db_id}_*` v2 key format); process-level state is limited to in-memory caches/config/logging
- Async Rust (Tokio multi-thread) throughout; deep async chains use boxed futures to bound stack depth

## Layers

**Protocol Layer:**
- Purpose: pgwire protocol handling — authentication, Simple Query, Extended Query, COPY
- Location: `src/protocol/`
- Contains: `DynamicPgHandler` (per-connection state), `DynamicHandlerFactory` (factory), query parser, value encoding, portal state
- Depends on: SQL layer (`Executor`, `Session`), auth, pool
- Used by: TCP accept loop in `src/main.rs`

**SQL Execution Layer:**
- Purpose: SQL parsing, semantic analysis, planning, and execution
- Location: `src/sql/`
- Contains: analyzer, optimizer, operators, executor, catalog, DDL, DML, functions, triggers, sessions
- Depends on: storage layer (`TikvStore`), model types, auth
- Used by: protocol layer

**Storage Layer:**
- Purpose: All TiKV interactions — CRUD for rows, indexes, schemas, sequences, stats, cron, worker tasks
- Location: `src/storage/`
- Contains: `TikvStore` (main store), key encoding, value serialization, backpressure
- Depends on: `tikv_client` crate
- Used by: SQL executor, worker engine, auth

**Worker / Background Tasks Layer:**
- Purpose: Unified async task engine for Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql
- Location: `src/worker/`
- Contains: engine, GC, metrics, types, config
- Depends on: storage layer (system keyspace TiKV store), pool
- Used by: `src/main.rs` at startup; triggered by SQL executor

**Auth Layer:**
- Purpose: Password authentication and RBAC
- Location: `src/auth/`
- Contains: `AuthManager`, RBAC, password hashing
- Depends on: storage layer
- Used by: protocol startup handler, executor privilege checks

**Extensions Layer:**
- Purpose: HTTP extensions and fs9 file system operations
- Location: `src/extensions/`
- Contains: `fs/` (fs9 backend, decoders, streaming, glob, WebSocket server), `parquet/`
- Depends on: storage layer
- Used by: SQL functions (`src/sql/expr/functions/fs9.rs`), main.rs for WebSocket listener

## Data Flow

**SELECT Query — primary path:**

1. TCP socket accepted in `src/main.rs` accept loop; `DynamicHandlerFactory` creates `DynamicPgHandler`
2. `DynamicPgHandler` (startup.rs) authenticates via `AuthManager` and initializes `Executor` + `Session`
3. Simple Query arrives at `DynamicPgHandler::do_query` (`protocol/handler/dynamic/query.rs`)
4. `Executor::execute` (`src/sql/executor/core/dispatch/mod.rs`) splits multi-statement batches
5. `execute_single` phases: (a) failed-txn precheck → (b) raw instrumented dispatch → (c) raw passthrough → (d) parse + AST dispatch
6. `dispatch_parsed_statements` classifies each `Statement` (DDL/DML/Query/RBAC)
7. For `Statement::Query` → `execute_query_statement` → `execute_query` → `try_execute_analyzed`
8. `try_execute_analyzed` (`src/sql/executor/select/analyzed/mod.rs`):
   - Expands views via `view_rewrite`
   - Materializes CTEs
   - Calls `Analyzer` with `CatalogSnapshot` to produce `AnalyzedQuery`
   - Calls `execute_via_optimizer` if applicable, or builds operator tree directly
9. Optimizer pipeline (`src/sql/optimizer/`): `AnalyzedQuery → LogicalPlan → (rewrite) → PhysicalPlan → BoxedOperator`
10. Volcano iterator model: root operator `open()` → `next()` loop → `close()`; leaf `TableScanOperator` / `IndexScanOperator` reads rows from TiKV
11. Results streamed back through `DynamicPgHandler` as pgwire `DataRow` messages

**DML (INSERT/UPDATE/DELETE):**

1. Parsed `Statement::Insert/Update/Delete` dispatched to `execute_dml_statement` (`src/sql/executor/core/stmt_dml.rs`)
2. Routes to `src/sql/executor/dml_analyzed/` for analyzed INSERT/UPDATE/DELETE
3. FK constraints validated via `src/sql/dml/foreign_keys.rs`
4. Row defaults applied via `src/sql/dml/defaults.rs`
5. TiKV writes via `src/storage/tikv_store/`; index keys written alongside data keys
6. After-statement triggers enqueued via `src/sql/triggers/`

**Extended Query (Prepared Statements):**

1. Parse message → `Db9QueryParser` calls `Analyzer` for type inference; stores `PreparedStatement`
2. Bind message → parameter types resolved; `Portal` created with bound parameter values
3. Execute message → portal executes through same `try_execute_analyzed` path as Simple Query

**Background Task Execution:**

1. `WorkerEngine` polls TiKV global task queue (system keyspace)
2. Acquires pessimistic lock on task; dispatches to handler (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql)
3. BgSql and trigger execution use `Executor` directly with an internal session (no current_role)
4. GC periodically cleans up completed task records

**State Management:**
- Per-connection: `Session` struct in `src/sql/session/` holds transaction state, GUC overrides, prepared statements, sequence session
- Task-local: `src/session_context.rs` uses `tokio::task_local!` for timezone, search_path, keyspace, DML limits
- Process-level caches: `TriggerBodyCache`, `RlsPolicyCache`, `TableStatsCache`, HNSW LRU cache (all in-memory, per-tenant)
- Tenant pool: `src/pool.rs` — `TikvClientPool` manages per-keyspace `TikvStore` handles with idle eviction

## Key Abstractions

**`AnalyzedQuery` / `TypedExpr` (Typed IR):**
- Purpose: SQL AST after name resolution, scope checking, and type inference
- Location: `src/sql/analyzer/types/`
- Pattern: Every expression node carries resolved `DataType`; column references are positional indices; no runtime name lookups needed downstream

**`BoxedOperator` (Physical Operator):**
- Purpose: Volcano-model iterator — `open()` / `next()` / `close()`
- Location: `src/sql/operators/executor.rs` (trait), operator impls in `src/sql/operators/`
- Pattern: Trait object (`Box<dyn Operator>`); tree built synchronously from `PhysicalPlan` by `build::BuildContext`

**`TikvStore`:**
- Purpose: All persistent access; keyspace-isolated
- Location: `src/storage/tikv_store/mod.rs`
- Pattern: Wraps `tikv_client::Transaction`; all key construction goes through `src/storage/encoding/` — never construct raw keys outside this module

**`Executor`:**
- Purpose: Central SQL execution engine for a single tenant keyspace
- Location: `src/sql/executor/core/` (the `Executor` struct)
- Pattern: Holds `Arc<TikvStore>`, observability, trigger cache, stats cache, memory accountant; shared across concurrent sessions via `Arc`

**`Session`:**
- Purpose: Per-connection mutable state
- Location: `src/sql/session/mod.rs`
- Pattern: Holds `TransactionState` (Idle / Active(txn) / Failed(txn)), GUC settings, prepared statements, savepoints; lives in `Arc<Mutex<Session>>` in the handler

**`CatalogSnapshot`:**
- Purpose: Pre-fetched immutable view of catalog for the Analyzer (sync, no async)
- Location: `src/sql/analyzer/catalog.rs`
- Pattern: Built async before `Analyzer::analyze()` is called; `Analyzer` receives it as `&dyn Catalog`

**`LogicalPlan` / `PhysicalPlan`:**
- Purpose: Intermediate representations in the CBO pipeline
- Location: `src/sql/optimizer/logical_plan/`, `src/sql/optimizer/physical_plan/`
- Pattern: `LogicalPlanner` maps `AnalyzedQuery` → `LogicalPlan` (pure structural); `rewrite/` pass (decorrelation, predicate pushdown, join reorder) mutates; `PhysicalPlanner` maps to `PhysicalPlan`

## Entry Points

**TCP Accept Loop:**
- Location: `src/main.rs` (`async_main`)
- Triggers: `tokio::net::TcpListener::accept`
- Responsibilities: TLS negotiation, connection semaphore enforcement (max connections), spawn per-connection task with `DynamicHandlerFactory`

**pgwire Handler:**
- Location: `src/protocol/handler/dynamic/`
  - `startup.rs` — authentication and executor initialization
  - `query.rs` — simple query and extended query protocol
  - `copy.rs` — COPY FROM STDIN / COPY TO STDOUT
- Triggers: pgwire library callbacks (`SimpleQueryHandler`, `ExtendedQueryHandler`, `CopyHandler` traits)
- Responsibilities: Parse protocol messages, delegate to `Executor`, encode results back to pgwire wire format

**SQL Executor Entry:**
- Location: `src/sql/executor/core/dispatch/mod.rs` (`Executor::execute`)
- Triggers: Called by `DynamicPgHandler` query handlers
- Responsibilities: Statement splitting, transaction state precheck, raw-SQL dispatch, AST parse + dispatch

**WorkerEngine:**
- Location: `src/worker/engine.rs`
- Triggers: Spawned at startup via `tokio::spawn`; also woken by `worker::wake_worker()`
- Responsibilities: Polls TiKV task queue, dispatches background jobs, manages pessimistic locks

**fs9 WebSocket Server:**
- Location: `src/extensions/fs/ws/`
- Triggers: `TcpListener` on `FS9_WS_PORT` (default separate port)
- Responsibilities: Binary WebSocket protocol for SDK file operations (read, write, list, etc.)

## Error Handling

**Strategy:** `anyhow::Result` throughout the SQL layer; `SqlError` enum in `src/sql/error.rs` carries SQLSTATE codes; mapped to pgwire `ErrorResponse` at the protocol boundary in `src/protocol/handler/errors.rs`

**Patterns:**
- `SqlError::InFailedTransaction` returned immediately by failed-txn precheck gate
- TiKV write conflicts (`is_retryable_tikv_error`) trigger autocommit retry with exponential backoff (`retry.rs`)
- Statement timeout via `tokio::time::timeout` wrapper in `core/timeout.rs`; surfaces as `SQLSTATE 57014`
- Stack overflow prevention: `recursion_limit = 256` on crate root; deep async chains use `Box::pin(async move { ... })`

## Cross-Cutting Concerns

**Logging:** `tracing` crate with `EnvFilter` from `RUST_LOG`; configured in `src/observability.rs` and `src/main.rs`

**Validation:** All SQL semantic validation in `src/sql/analyzer/`; type coercion rules follow PostgreSQL specification; `src/sql/types/` for type inference and CAST

**Authentication:** Per-connection in `protocol/handler/dynamic/startup.rs`; RBAC privilege checks scattered in executor via `require_privilege()` / `require_table_privilege()`

**Multi-tenancy isolation:** Username parsed as `<keyspace>.<user>` by `src/protocol/handler/tenant.rs`; all TiKV keys scoped to `db_id` within a keyspace; `session_context.rs` task-locals ensure keyspace propagates through async chains

**Observability:** `src/observability.rs` — `TenantObservability` tracks statement count, error count, latency per tenant; `ConnectionGuard` tracks live connections; observability user (`_db9_obs_user`) bypasses statement recording
