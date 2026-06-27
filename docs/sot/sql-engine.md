# sql-engine — SQL parsing/planning/execution semantics (current behavior)

## Scope
- SQL parsing, analysis, rewrite, planning, and execution semantics.
- Transaction/failure behavior, retries, and statement timeouts.
- Trigger execution model and SQL-visible observability pseudo-tables.
- Session-local prepared execution behavior, including prepared-plan caching.

## Non-goals
- Persistent key layout and storage serialization (authoritative: `./storage-format.md`).
- pgwire framing and startup/authentication messages (authoritative: `./protocol-pgwire.md`).
- RBAC policy definitions and bootstrap rules (authoritative: `./auth-rbac.md`).
- Virtual catalog coverage contracts (authoritative: `./catalog-introspection.md`).

## External Contracts
- **[Stable] PostgreSQL is the default semantic target**
  - SQL-visible behavior MUST target PostgreSQL parity by default.
  - Intentional divergence MUST be explicit in SoT with rationale, user value, scope, and verification.
  - Cross-link: `docs/sot/README.md` section `Compatibility Strategy: PG-Compatible by Default, DB9-Better by Explicit Design`.

- **[Stable] PostgreSQL dialect parsing and multi-statement execution**
  - SQL text MUST be parsed with the PostgreSQL dialect in `sqlparser`.
  - Simple-query multi-statement input MUST execute statements in order and return a result stream for each statement.
  - Evidence: `src/sql/parser/mod.rs`, `src/sql/executor/core/dispatch/mod.rs`, `tests/05_transaction.sql`.

- **[Stable] Narrow single-path contract for analyzed execution**
  - Once a statement enters analyzed `SELECT/WITH`, analyzed DML, or analyzed prepared execution, the engine MUST stay on that semantic path and MUST NOT silently try a legacy planner/executor because analysis failed.
  - Explicit compatibility shims are allowed only at documented boundaries, including:
    - protocol/parser utility acceptance when `sqlparser` cannot parse a supported raw-SQL utility shape;
    - prepared execution reparse for schema drift or currently unsupported prepared recursive CTE execution (tracked in `#1516`).
  - Evidence: `src/sql/executor/core/analyze_rewrite.rs`, `src/protocol/handler/query_parser.rs`, `src/sql/raw_sql.rs`, `src/sql/executor/core/dispatch/prepared.rs`.

- **[Stable] Canonical `SELECT/WITH` semantic pipeline**
  - `analyze_then_rewrite_query()` is the canonical semantic entrypoint for analyzed query execution and `EXPLAIN SELECT/WITH`.
  - The current pipeline is:
    1. view expansion,
    2. catalog snapshot build,
    3. base-table `SELECT` privilege checks,
    4. Analyzer,
    5. post-analysis rewriter.
  - Evidence: `src/sql/executor/core/analyze_rewrite.rs`, `src/sql/rewriter/mod.rs`.

- **[Stable] Failed-transaction state**
  - When a statement fails inside an explicit transaction, subsequent non-empty statements MUST fail with PostgreSQL failed-transaction behavior until the transaction ends or is repaired by `ROLLBACK TO SAVEPOINT`.
  - Evidence: `src/sql/error.rs` (`InFailedTransaction`), `src/sql/executor/core/dispatch/mod.rs`, `src/sql/executor/core/dispatch/prepared.rs`, `src/protocol/handler/portal.rs`.

- **[Stable] Retry envelope for retryable TiKV conflicts**
  - Retryable write conflicts are retried only when it is safe to restart the statement:
    - autocommit statements;
    - the first statement in an explicit transaction, before any prior statement in that transaction has completed.
  - Later statements in an explicit transaction execute once and then follow failed-transaction semantics on error.
  - The session default is `db9.retry_max_attempts = 64` and `db9.retry_timeout = 0` (no wall-clock timeout).
  - Evidence: `src/sql/executor/core/dispatch/transaction.rs`, `src/sql/executor/core/dispatch/prepared.rs`, `src/sql/session/settings.rs`.

- **[Stable] Intentional divergence: honest `transaction_isolation` labeling**
  - db9 exposes the engine's TiKV snapshot isolation honestly as `repeatable read` on the `transaction_isolation` surface, instead of pretending to provide PostgreSQL `read committed`.
  - Requests for `read committed`, `read uncommitted`, or `serializable` on this surface currently resolve to `repeatable read`.
  - Why not PostgreSQL here: TiKV snapshot isolation does not match PostgreSQL `read committed`, and db9 does not implement PostgreSQL `serializable`.
  - User value: avoids falsely advertising weaker or stronger isolation guarantees than the engine actually provides.
  - Scope: `SHOW transaction_isolation`, `SET transaction_isolation = ...`, and `BEGIN/START TRANSACTION ISOLATION LEVEL ...`.
  - Governance / history: `#608`, PR `#617`.
  - Evidence: `src/sql/session/settings.rs`, `src/sql/executor/core/dispatch/utils.rs`.

- **[Experimental] Statement timeout handling**
  - When a statement timeout fires inside an explicit transaction, current behavior aborts the transaction rather than leaving it open in failed state.
  - PostgreSQL-parity follow-up is tracked in `#1519`.
  - Treat this as experimental until dedicated PG-parity coverage exists.
  - Evidence: `src/sql/executor/core/dispatch/mod.rs`, `src/sql/error.rs`.

- **[Experimental] Transaction rollback for regular `SET`**
  - Transaction rollback currently does not restore regular (non-`LOCAL`) `SET` values changed inside an explicit transaction, even for built-in typed settings such as `timezone`.
  - This affects both full `ROLLBACK` and `ROLLBACK TO SAVEPOINT`.
  - PostgreSQL-parity follow-up is tracked in `#1520`.
  - Treat this as experimental until rollback-aware session-state undo exists for regular `SET`.
  - Evidence: `src/sql/session/transaction.rs`, `src/sql/session/settings.rs`.

- **[Experimental] Role identity semantics around `SET ROLE`**
  - `SESSION_USER` currently follows the effective role after `SET ROLE`, instead of remaining the authenticated login role.
  - SQL value function `CURRENT_ROLE` is currently not implemented correctly and errors as an unresolved column reference.
  - db9 does not currently expose PostgreSQL's SQL-visible `role` surface coherently: `SHOW role` errors, and `current_setting('role', true)` stays `NULL` even after `SET ROLE`.
  - PostgreSQL also supports `SET role = ...` and transaction-local `SET LOCAL role = ...` forms on that same effective-role surface, while db9 currently returns SQL parse errors on those variants.
  - Role-state changes made through `SET ROLE` / `RESET ROLE` inside an explicit transaction are currently not undone by `ROLLBACK` or `ROLLBACK TO SAVEPOINT`.
  - PostgreSQL-parity follow-up is tracked in `#1521`.
  - Treat this as experimental until session/effective role state is separated in expression evaluation, `CURRENT_ROLE` exists as a real SQL value function, and rollback-aware role-state undo exists.
  - Evidence: `src/sql/executor/core/dispatch/roles.rs`, `src/sql/session/mod.rs`, `src/sql/query_context.rs`, `src/sql/expr/typed_eval/helpers.rs`.

- **[Experimental] Transaction read-only semantics**
  - `SET TRANSACTION READ ONLY` / `BEGIN READ ONLY` currently do not provide a real PostgreSQL `transaction_read_only` surface.
  - Plain `SET transaction_read_only = ...` and `set_config('transaction_read_only', ..., false)` currently route through generic GUC storage instead of a real transaction-local state model.
  - `SET LOCAL transaction_read_only = ...` and tableless `set_config('transaction_read_only', ..., true)` can already fabricate `SHOW transaction_read_only = on`, but writes still succeed inside the supposedly read-only transaction.
  - As a result, db9 can expose both sticky session-visible and transaction-local-looking `transaction_read_only` readback without PostgreSQL-compatible scope or write enforcement.
  - `SET default_transaction_read_only = on` currently only changes session readback; it does not make subsequent explicit transactions or autocommit statements read-only.
  - Current transaction-access-mode behavior mutates `default_transaction_read_only`, leaks across `ROLLBACK`, and does not prevent writes inside the supposedly read-only transaction.
  - PostgreSQL-parity follow-ups are tracked in `#1522` and `#1524`.
  - Treat this as experimental until session-default read-only state, transaction-local read-only state, readback, and write enforcement all exist and are wired together.
  - Evidence: `src/sql/executor/core/dispatch/utils.rs`, `src/sql/executor/core/dispatch/ast.rs`, `src/sql/session/settings.rs`.

- **[Experimental] `SET TRANSACTION` context rules**
  - Plain `SET TRANSACTION ...` currently does not enforce PostgreSQL's context rules.
  - Outside an explicit transaction block, current behavior succeeds and can mutate session-visible state instead of emitting PostgreSQL's warning/no-op behavior.
  - Inside an explicit transaction, `SET TRANSACTION ISOLATION LEVEL ...` is currently accepted even after earlier queries in the same transaction.
  - The current handler also discards the parser's `session` flag, so `SET TRANSACTION` and `SET SESSION CHARACTERISTICS AS TRANSACTION` do not yet have clearly separated scope rules.
  - PostgreSQL-parity follow-up is tracked in `#1525`.
  - Treat this as experimental until transaction-context validation and session-vs-transaction scope handling are enforced explicitly.
  - Evidence: `src/sql/executor/core/dispatch/ast.rs`, `src/sql/executor/core/dispatch/utils.rs`, `src/sql/session/mod.rs`.

- **[Experimental] Transaction deferrable surfaces**
  - db9 does not implement a real PostgreSQL `transaction_deferrable` / `default_transaction_deferrable` contract.
  - PostgreSQL transaction-mode statement forms such as `BEGIN DEFERRABLE`, `START TRANSACTION DEFERRABLE`, `SET TRANSACTION DEFERRABLE`, and `SET SESSION CHARACTERISTICS AS TRANSACTION DEFERRABLE` are also currently absent; db9 returns SQL parse errors on those forms.
  - Current generic GUC compatibility handling can accept these parameter names and expose misleading readback without PostgreSQL-compatible scope or behavior.
  - PostgreSQL-parity follow-up is tracked in `#1529`.
  - Treat this as experimental until db9 either implements a coherent contract or rejects these surfaces explicitly.
  - Evidence: `src/sql/session/settings.rs`, `src/sql/executor/core/settings_tableless.rs`, `src/sql/executor/core/dispatch/ast.rs`.

- **[Experimental] Reserved session pseudo-GUC surfaces**
  - db9 still accepts reserved PostgreSQL session/privilege pseudo-GUC names such as `is_superuser` and `session_authorization` through the generic compatibility-GUC path.
  - db9 also hijacks the dotted name `session.authorization` as if it were an alias for `session_authorization`, while PostgreSQL treats it as an ordinary custom-GUC name.
  - The same generic path can also fabricate a sticky or transaction-local-looking `role` surface through `set_config('role', ...)`, even though db9 has no real authoritative `SHOW role` / `current_setting('role')` implementation and does not change `current_user` / `session_user` accordingly.
  - `SET LOCAL session_authorization = ...` and tableless `set_config('session_authorization', ..., true)` are also currently accepted in db9 without changing the authoritative login-role readback, while PostgreSQL temporarily changes `SHOW session_authorization` within the transaction and restores it on rollback.
  - Generic `RESET` is also unsafe on these names today: db9 silently accepts `RESET is_superuser` and `RESET session.authorization`, while PostgreSQL errors on the former and rejects the dotted form in `RESET` syntax.
  - `SHOW` / `current_setting()` readback still comes from authoritative session identity hooks, so `SET` / `set_config()` can appear to succeed while silently preserving the old value.
  - PostgreSQL-parity follow-up is tracked in `#1530`.
  - Treat this as experimental until db9 either gives these names dedicated semantics or rejects generic `SET` / `set_config()` on them explicitly.
  - Evidence: `src/sql/session/mod.rs`, `src/sql/session/settings.rs`, `src/sql/executor/core/settings_tableless.rs`, `src/sql/executor/core/dispatch/ast.rs`.

- **[Experimental] `session_replication_role`**
  - PostgreSQL's `session_replication_role` surface has real trigger-behavior semantics; `replica` mode suppresses row-level trigger execution.
  - db9 currently has no authoritative `session_replication_role` contract: `SHOW session_replication_role` errors until a generic `SET` fabricates the value through compatibility-GUC storage.
  - After `SET session_replication_role = replica`, current db9 can read back `replica`, but BEFORE triggers still execute normally.
  - PostgreSQL-parity follow-up is tracked in `#1535`.
  - Treat this as experimental until db9 either implements explicit trigger-mode semantics or rejects this surface instead of faking it.
  - Evidence: `src/sql/session/settings.rs`, `src/sql/executor/core/dispatch/ast.rs`, `src/sql/executor/core/settings_tableless.rs`, `src/sql/executor/dml_analyzed/insert.rs`.

- **[Experimental] `check_function_bodies`**
  - PostgreSQL's `check_function_bodies` surface has real `CREATE FUNCTION` semantics; it changes whether function bodies are validated at creation time.
  - db9 currently exposes `SET` / `RESET` / `SHOW` readback for `check_function_bodies`, but the function-creation path does not consume that setting.
  - As a result, current db9 behaves as if body validation were always disabled, even while `SHOW check_function_bodies` reports `on` by default.
  - PostgreSQL-parity follow-up is tracked in `#1536`.
  - Treat this as experimental until db9 either implements explicit body-validation semantics or narrows/rejects the surface instead of faking it.
  - Evidence: `src/sql/session/settings.rs`, `src/sql/executor/triggers.rs`.

- **[Experimental] `standard_conforming_strings`**
  - PostgreSQL's `standard_conforming_strings` surface has real parser-visible semantics; when it is `off`, ordinary string literals interpret backslash escapes and PostgreSQL emits a warning for nonstandard escape usage.
  - db9 currently exposes `SET` / `RESET` / `SHOW` readback for `standard_conforming_strings`, but ordinary string-literal semantics do not change with that setting.
  - As a result, current db9 behaves as if standard-conforming string parsing were always enabled, even while `SHOW standard_conforming_strings` can report `off`.
  - PostgreSQL-parity follow-up is tracked in `#1537`.
  - Treat this as experimental until db9 either implements explicit parser semantics or narrows/rejects the surface instead of faking it.
  - Evidence: `src/sql/session/settings.rs`, `src/sql/parser/mod.rs`, `src/protocol/handler/server_params.rs`.

- **[Experimental] `bytea_output`**
  - PostgreSQL's `bytea_output` surface has real text-result formatting semantics; switching between `hex` and `escape` changes how `bytea` values are rendered.
  - db9 currently exposes `SET` / `RESET` / `SHOW` readback for `bytea_output`, but text output for `bytea` values remains fixed.
  - As a result, current db9 behaves as if hex output were always enabled, even while `SHOW bytea_output` can report `escape`.
  - PostgreSQL-parity follow-up is tracked in `#1538`.
  - Treat this as experimental until db9 either implements explicit output-format semantics or narrows/rejects the surface instead of faking it.
  - Evidence: `src/sql/session/settings.rs`, `src/protocol/handler/encode/`, `src/sql/expr/functions/encoding.rs`.

- **[Experimental] `SESSION AUTHORIZATION` statements**
  - db9 exposes `SHOW session_authorization` / `current_setting('session_authorization')`, but does not currently implement PostgreSQL's dedicated `SET SESSION AUTHORIZATION ...` or `RESET SESSION AUTHORIZATION` statements.
  - Current behavior is a SQL parse error rather than a coherent session-identity contract.
  - PostgreSQL-parity follow-up is tracked in `#1533`.
  - Treat this as experimental until db9 either implements those statement surfaces explicitly or rejects them in a deliberate documented way.
  - Evidence: `src/sql/parser/preprocess.rs`, `src/sql/executor/core/dispatch/ast.rs`, `src/sql/session/mod.rs`.

- **[Experimental] `set_config()` outside the tableless fast path**
  - db9 currently gives `set_config(name, value, is_local)` real side effects only on the dedicated tableless `SELECT set_config(...)` fast path.
  - In general expression contexts, `set_config()` is still exposed as a regular scalar function but currently returns an empty string and does not mutate session state.
  - PostgreSQL-parity follow-up is tracked in `#1532`.
  - Treat this as experimental until `set_config()` executes through one authoritative path across tableless and analyzed expression contexts, or unsupported contexts are rejected explicitly.
  - Evidence: `src/sql/executor/core/settings_tableless.rs`, `src/sql/expr/typed_eval/helpers.rs`, `src/sql/types/registry/system.rs`, `src/sql/query_context.rs`.

- **[Experimental] `default_transaction_isolation` SET semantics**
  - `SET default_transaction_isolation = ...` is currently accepted but does not change `SHOW default_transaction_isolation` or future transaction behavior.
  - PostgreSQL-parity follow-up is tracked in `#1523`.
  - Treat this as experimental until db9 either implements real semantics for future transactions or rejects the setting explicitly.
  - Evidence: `src/sql/session/settings.rs`, `src/sql/executor/core/dispatch/guc.rs`.

- **[Stable] Session-local prepared plan cache**
  - Eligible analyzed prepared queries can promote into a per-session prepared-plan cache after repeated executions.
  - Cache keys include normalized SQL, parameter types, database, search path, and resolved table IDs.
  - Cache entries are invalidated on schema-version drift; unsupported prepared recursive CTEs (tracked in `#1516`) and schema-drift recovery explicitly reparse SQL text instead of reusing stale analyzed state.
  - Evidence: `src/sql/executor/core/plan_cache.rs`, `src/sql/executor/core/dispatch/prepared.rs`, `src/sql/session/mod.rs`, `src/sql/session/settings.rs`.

- **[Stable] Planner-driven access paths include B-tree, GIN, and HNSW**
  - The optimizer/runtime can emit B-tree, GIN, and HNSW scans when the corresponding predicates/orderings are eligible.
  - GIN semantics are authoritative in `./extensions-gin.md`.
  - HNSW scans load the base graph plus visible delta entries at execution time; `hnsw.ef_search` is a supported session GUC.
  - Evidence: `src/sql/planner/index_selection.rs`, `src/sql/optimizer/physical_planner/mod.rs`, `src/sql/optimizer/build/scan.rs`, `src/sql/operators/hnsw_scan.rs`, `src/sql/hnsw/storage.rs`.

- **[Experimental] Observability sys pseudo-tables**
  - `_DB9_SYS_OBSERVABILITY` and `_DB9_SYS_QUERY_SAMPLES` are SQL-visible pseudo-tables with a deliberately restricted fast path.
  - Evidence: `src/sql/executor/table_utils/mod.rs`, `src/sql/catalog/virtual_tables.rs`, `src/observability.rs`.

## Data Model & Invariants
- **Transaction state model**: session state, savepoints, and failed-transaction behavior are owned by the SQL engine layer.
  - Evidence: `src/sql/session/mod.rs`, `src/sql/executor/core/dispatch/mod.rs`.
- **Stored values conform to schema types**: analyzed DML MUST either coerce values safely or fail atomically before persistence.
  - Evidence: `src/sql/types/coercion.rs`, `src/sql/dml/mod.rs`, `tests/155_dml_type_coercion_invariant_issue407.sql`.
- **Trigger model**: BEFORE triggers run inline; AFTER triggers enqueue async work for the worker engine after commit.
  - Evidence: `src/sql/triggers/mod.rs`, `src/sql/triggers/queue.rs`, `src/sql/triggers/worker.rs`, `src/worker/engine.rs`, `tests/53_trigger_execution.sql`, `tests/86_async_triggers.sql`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/sql/parser/mod.rs`
- `src/sql/executor/core/analyze_rewrite.rs`
- `src/sql/rewriter/mod.rs`
- `src/sql/executor/core/dispatch/mod.rs`
- `src/sql/executor/core/dispatch/transaction.rs`
- `src/sql/executor/core/dispatch/prepared.rs`
- `src/sql/executor/core/plan_cache.rs`
- `src/sql/executor/dml_analyzed/`
- `src/sql/optimizer/physical_planner/mod.rs`
- `src/sql/optimizer/build/scan.rs`
- `src/sql/operators/hnsw_scan.rs`
- `src/sql/hnsw/storage.rs`
- `src/sql/triggers/mod.rs`
- `src/observability.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/regression-gate`, `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/05_transaction.sql`

## Change Management
- Any SQL-visible semantic change to parsing, analyze/rewrite, retry behavior, failure handling, prepared execution, trigger model, or access-path selection MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking SQL-visible changes require DR/ADR per #368 rules (impact surface, migration, rollback, and verification updates).
- Reference: https://github.com/c4pt0r/db9/issues/368
