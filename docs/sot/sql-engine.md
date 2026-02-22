# sql-engine — SQL parsing/planning/execution semantics (current behavior)

## Scope
- SQL parsing/planning/execution semantics (DDL/DML/queries).
- Expression evaluation and type/coercion behavior.
- Trigger execution model (BEFORE + async AFTER queue/worker).
- SQL-visible internal surfaces owned by the engine (e.g. observability sys tables).

## Non-goals
- Persistent storage encoding and key layout (authoritative: `./storage-format.md`).
- pgwire message framing and protocol-level behavior (authoritative: `./protocol-pgwire.md`).
- RBAC policy definitions (authoritative: `./auth-rbac.md`).
- Virtual catalog semantics (`information_schema`/`pg_catalog`) (authoritative: `./catalog-introspection.md`).

## External Contracts
- **[Stable] PostgreSQL dialect parsing + multi-statement execution**
  - The engine MUST parse SQL using `sqlparser`’s PostgreSQL dialect and MUST support multiple statements in a single query string (e.g. `BEGIN; ...; COMMIT;`).
  - When multiple statements are provided, the engine MUST execute them in order and return a result stream containing each statement’s results (Simple Query protocol compliance).
  - Evidence: `src/sql/parser.rs` (`parse_sql`), `src/sql/executor/core.rs` (`Executor::execute` docstring + statement loop), `tests/05_transaction.sql` (multi-statement examples).

- **[Stable] Failed-transaction state (PostgreSQL compatibility)**
  - When a statement fails inside an explicit transaction, subsequent non-empty statements MUST error until the transaction is ended via `ROLLBACK`/`COMMIT`/`END`.
  - Evidence: `src/sql/executor/core.rs` (`Executor::execute` early `InFailedSqlTransaction` check + `session.mark_transaction_failed()` paths).

- **[Stable] Autocommit retry on TiKV write conflicts**
  - In autocommit mode, the engine MUST retry retryable TiKV write-conflict errors with backoff (bounded attempts) to better emulate PostgreSQL’s “wait + retry” behavior under concurrent updates.
  - Evidence: `src/sql/executor/core.rs` (`is_retryable_tikv_error`, retry loop with `max_attempts = 10`), `tests/104_write_conflict_retry.sql` (infrastructure coverage).

- **[Experimental] Statement timeout behavior**
  - If a statement times out inside an explicit transaction, the engine currently aborts the transaction (ROLLBACK) to avoid leaving a partially-applied transaction open.
  - This differs from PostgreSQL’s typical “failed transaction” state handling; treat as experimental until a dedicated compatibility test exists.
  - Evidence: `src/sql/executor/core.rs` (statement timeout handling + rollback-on-timeout comment).

- **[Experimental] Observability sys pseudo-tables**
  - The engine recognizes sys pseudo-table surfaces including `_PGTIKV_SYS_OBSERVABILITY` and `_PGTIKV_SYS_QUERY_SAMPLES` and restricts the “fast path” to simple, non-nested `SELECT ... FROM <sys_table>` queries.
  - Evidence: `src/sql/executor/core.rs` (`is_observability_system_query`), `src/sql/executor/table_utils.rs` (sys table resolution paths).
  - Gap: explicit assertion-level gate coverage is tracked in `docs/sot/modules.yaml` (`sql-engine`).

## Data Model & Invariants
- **Transaction state model**: statement execution is mediated by `Session` + an active TiKV transaction; savepoints and transaction boundaries are owned by the SQL engine layer, while TiKV transaction primitives are specified in `./storage-format.md`.
  - Evidence: `src/sql/session.rs`, `src/sql/executor/core.rs` (`session.begin/commit/rollback`, `with_savepoints` wrapper).
- **[Stable] Stored values conform to schema types**: DML (INSERT/UPDATE/UPSERT) MUST NOT persist a `Value` variant that is incompatible with the declared column `DataType`. If an implicit DML cast/coercion is not supported, the statement MUST error and MUST be atomic (no partial row writes and no index corruption).
  - Evidence: `src/sql/coercion.rs` (`coerce_value_for_column`), `src/sql/dml.rs` (`coerce_row_values`, DML write helpers), `tests/155_dml_type_coercion_invariant_issue407.sql`.
- **Trigger execution model** (high level): BEFORE triggers execute in-statement; AFTER triggers are enqueued and processed asynchronously by a worker (exact queue/storage details are implementation-defined and may evolve).
  - Evidence: `src/sql/triggers.rs`, `src/sql/trigger_queue.rs`, `src/sql/trigger_worker.rs`, `src/sql/executor/triggers.rs`, `tests/53_trigger_execution.sql`.
- **Index access paths are planner-driven**: plan selection currently chooses full-table + B-tree variants based on schema/index metadata + predicates; index encoding details are specified in `./storage-format.md`.
  - Evidence: `src/sql/planner/index_selection.rs`, `src/sql/executor/select/mod.rs`.
- **[Experimental] GIN access path is currently disabled in planner/runtime**: `ScanType::GinIndexScan` is retained as a future contract shape, but current access-path selection does not emit it and runtime builders reject it if reached unexpectedly.
  - Evidence: `src/sql/planner/index_selection.rs`, `src/sql/optimizer/build/scan.rs`, `src/sql/operators/planner.rs`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`:
- `PGTIKV_MAX_GENERATE_SERIES_ROWS`
- `PGTIKV_TRIGGER_ENABLED`, `PGTIKV_TRIGGER_*`
- `PGTIKV_OBS_ENABLED`, `PGTIKV_OBS_*`

## Entrypoints
- `src/sql/parser.rs` (`parse_sql`)
- `src/sql/planner.rs` (`choose_join_algorithm`)
- `src/sql/executor/core.rs` (`Executor`)
- `src/sql/expr/mod.rs` (`eval_expr`, `eval_join_expr`)
- `src/sql/triggers.rs`
- `src/sql/trigger_queue.rs`
- `src/sql/trigger_worker.rs`
- `src/sql/executor/triggers.rs`
- `src/observability.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/regression-gate.yml/regression-gate`, `ci:.github/workflows/orm-tests.yml/test`
- Local reproduce (typical):
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/05_transaction.sql`

## Change Management
- Any change to SQL-visible semantics (parser normalization, transaction/failure rules, planner/executor behavior, trigger model, observability sys surfaces) MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Breaking changes require DR/ADR per #368 rules (impact surface + migration + rollback + verification updates).
- Reference: https://github.com/c4pt0r/tipg/issues/368
