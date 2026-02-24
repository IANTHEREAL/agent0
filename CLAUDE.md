# db9-server Knowledge Base (Living Document)

## Overview

PostgreSQL-compatible distributed SQL database on TiKV. Implements pgwire protocol and translates SQL to KV operations.

## Refactor Governance Contract (Architecture Owner)

### Identity

- Act as a senior database systems expert for architecture, execution semantics, and compatibility contracts.

### Role

- Own and enforce one clean execution model for `Analyzer -> Typed IR -> Executor`.
- Own root-cause analysis for CI/ORM regressions at the correct fault layer (`Analyzer` / `Executor` / `Catalog` / `Storage`), not symptom layers.

### Mission

- Build a clean, maintainable, debuggable database core before launch.
- Keep architecture single-path and deterministic: no hidden fallback, no runtime "try-new-then-old".
- Ensure each behavior has a clear module boundary and a single source of truth.
- Deliver structurally correct fixes, not tactical patches.

### Operating Principles

- **PostgreSQL is the specification.** All SQL semantics, type coercion rules, catalog behavior, error codes, and wire protocol responses must align with PostgreSQL. When in doubt, test against real PostgreSQL and match its behavior — do not invent custom semantics.
- Correctness first, then speed; optimize diagnosis path, not by shortcuts.
- Fix only at the root layer where the invariant is broken.
- Never use wire/test-layer masking logic to hide internal inconsistency.
- Test expectations must follow validated semantics (PostgreSQL parity where intended), not vice versa.
- If behavior changes intentionally, update contract and tests in the same change.

## Current Architecture (Latest)

### Execution Model (single path)

```
Client/ORM -> pgwire -> SQL Parser -> Analyzer -> Typed IR -> Optimizer (CBO) -> Executor/Operators -> TiKV Store -> TiKV
```

### Module Boundaries

- `Analyzer` (`src/sql/analyzer/`): name resolution, scope checking, and type inference; outputs `AnalyzedQuery` / `TypedExpr`.
- `Optimizer` (`src/sql/optimizer/`): `AnalyzedQuery → LogicalPlan → (rewrite: decorrelation, predicate pushdown, join reorder) → PhysicalPlan → BoxedOperator`. Handles single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, DISTINCT ON, SemiJoin/AntiJoin. Always-on (the `db9.use_optimizer` GUC is accepted for compatibility but is a no-op — `SET ... = off` logs a notice and is ignored; `SHOW` always returns `on`).
- `Operators` (`src/sql/operators/`): physical operators (scan, filter, project, sort, aggregate, hash_join, hash_semi_join, NLJ, window/, CTE, set_operation, table_function).
- `Executor` (`src/sql/executor/`): DDL/DML dispatch, SELECT execution (analyzed path at `executor/select/analyzed/`), background SQL (`bg_sql.rs`).
- `Catalog` (`src/sql/catalog/`): `information_schema` / `pg_catalog` / `cron` compatibility surface (40+ virtual table implementations).
- `Storage` (`src/storage/`): all persistent keys must remain keyspace-isolated via `TikvStore`. Database-scoped v2 key format (`d_{db_id}_*`).
- `Worker` (`src/worker/`): unified async task engine (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql). Global task queue in TiKV, pessimistic locking, no leader election.
- `Cron` (`src/cron/`): pg_cron-compatible scheduler integrated with worker engine.

### Multi-tenancy Invariant (Critical)

- All persistent data must be isolated per keyspace (`_sys_*`, table rows, indexes, auth, and future stats).
- Process-level global state is limited to in-memory caches/config/logging.

## Completed Milestones

### Sprint 1 — Analyzer + Legacy Cleanup

- **Task 1:** Analyzer → Typed IR pipeline (single execution path for all SELECT queries)
- **#56** NLJ streaming — NLJ now streams outer side, materializes only build side (`src/sql/operators/join.rs`)
- **#609** SELECT privilege enforcement — `require_table_privilege(Select)` on every base table
- **#634/#635** COPY CSV fixes — order-independent option parsing, ESCAPE self-escaping
- Legacy removal: `infer_expr_type`, `executor/subquery.rs`, boolean validator, comparison coercion fallback
- All 10 legacy code items fully resolved (#772)

### CBO — Planner Unification + Optimizer Pipeline (#778, #815)

- `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator` pipeline implemented in `src/sql/optimizer/`.
- **Planner dual-path resolved:** TypedExpr path has full expression-index + partial-index support. EXPLAIN uses the analyzed pipeline (view expansion → Analyzer → typed planner). AST path retained only for non-SELECT EXPLAIN and analysis error fallback.
- **GIN index status:** planner can produce GIN scan plans (visible in EXPLAIN), but the runtime operator falls back to table scan (`src/sql/optimizer/build/scan.rs`) — GIN execution operator is not yet implemented.
- Coverage: single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, DISTINCT ON.
- `db9.use_optimizer` GUC retained for compatibility (always-on, no-op when set to off).
- Handler decomposed: monolithic `dynamic.rs` split into `dynamic/` module (mod.rs, query.rs, copy.rs, startup.rs).
- Index name uniqueness enforced within schema (#777).

### CBO Phase 2–3 — Statistics + Join Optimization (#706, #705, #908, #937)

- Phase 2: Table statistics (#706) — COMPLETE (ANALYZE, persistence, warmup, invalidation).
- Phase 3: Join optimization (#705) — ALL DONE: predicate pushdown, cross-join elimination, hash join selection, cost-based join reordering via DPccp (#908), subquery decorrelation EXISTS/NOT EXISTS → SemiJoin/AntiJoin (#937).
- Index selection in optimizer path — COMPLETE (btree parity: point, range, bounded-range, in-list, expression indexes, partial indexes; GIN excluded).

### Protocol — Prepared Statement Unification + Hardening (#865, #471, #579)

- Prepared-statement semantic contract unified (#865): all 9 sub-issues closed — text substitution removed (#867), Analyzer-backed Describe (#868), god files split (#869/#875), TypedExpr visitor API (#870/#899), error handling unified (#871/#900), regex cache + NULL guard dedup (#872/#873/#880).
- Protocol production readiness (#471): secure defaults (#401), COPY txn semantics (#32), Extended Query binary Bind (#355/#403), pgwire robustness (#421 — 6 sub-tasks). All closed.
- Protocol hardening (#579): SQLSTATE mapping, memory/stack guards (#929), dynamic.rs split into sub-modules (#917/#919/#921). All closed.
- Prepared execute parity (#902/#906/#935): runtime context, role gating, timeout, recursive CTE, Cow optimization. All closed.

### Other Recent

- **Worker engine** (`src/worker/`): unified async task engine — Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql. Global TiKV queue, pessimistic locking, GC.
- **Cron scheduler** (`src/cron/`): pg_cron-compatible expressions, job management, virtual tables (`cron_job`, `cron_job_run_details`, `cron_running_jobs`).
- **Prisma ORM** binary wire protocol (#841/#842).
- **PL/pgSQL** execution: SELECT INTO, FOR loops, EXIT, correlated table functions (#863).
- **Cron** runtime management: job timeout, cancel, process list (#896), status command (#913), --file + dollar-quoting (#892).
- **fs9** file system: `fs9_read`, `fs9_write`, `fs9_exists`, `fs9_size`, `fs9_mtime`, `fs9_remove`, remote backend routing, SDK API (#851/#853/#855/#878).
- **SQL functions**: json_agg/jsonb_agg, hashtext (#930), RETURNS TABLE syntax (#895).
- **FTS** Chinese tokenizer support for GIN full-text search (#774).
- **Infra**: SQL execution timeout protection (#860), stack overflow prevention (#929), information_schema.columns optimization (#936), unified SqlError with SQLSTATE (#900).
- **Refactoring**: dispatch_raw! macro for executor dispatch (#966), deduplicated utility functions (#964), LogicalPlan::map_children() (#963), IndexScanBase coverage (#970).

### Phase 1 — Core Correctness (ALL DONE)

- **FK cluster** (#925, #923, #924, #922): ref_columns validation, NULL MATCH SIMPLE, stale snapshots, self-referential constraints — all fixed in `src/sql/dml/foreign_keys.rs`.
- **Analyzer/operators** (#910, #911): SQLSTATE 42725 parity for mixed unknown binary ops + test coverage.
- **GUC** (#601, #884): SET LOCAL (transaction-scoped), `current_setting()`, SET LOCAL rollback.
- **SHOW** (#600, #599): PostgreSQL-compatible SHOW defaults + SHOW ALL.
- **SQL parity** (#408): UNNEST(...) as JOIN relation.
- **JSONB** (#281): JSON/JSONB canonicalization (key order, whitespace).
- **CLI** (#920): db9 login --api-key 401 auth fix.

## Open Issues (Active)

### Correctness — SQL parity

- **#397/#396** Collated string index test mismatches.
- **#395** ALTER TYPE test mismatch.

### Usability — ORM compatibility

- **#885** Activepieces drop-in compatibility. Depends on: remaining syntax gaps.
- **#840** Prisma ORM remaining test gaps (upsert, JSONB path, SERIALIZABLE).
- **#375** Dify compatibility (introspection, RETURNING, JSONB operators).

### Performance (deferred)

- **#857** Pre-materialization runs unconditionally/twice on top-level SELECT. → **#707** (plan cache).
- **#707** Plan cache for prepared statements. Independent of #708.
- **#708** Parallel/distributed query execution framework (long-term, independent track).

### Architecture / Code quality

- **#696** Statement-type detection duplicated in query_parser.rs and dispatch.rs.
- **#695** Dual type module hierarchy (src/types/ vs src/sql/types/).
- **#694** bincode serialization in SQL layer couples to storage encoding.
- **#693** Version column name rewriting in wire encoding layer.
- **#779** Per-tenant resource governance: connection caps → tenant QPS → timeout enforcement → memory/backpressure.
- **#700** Admin portal credential encryption falls back to plaintext. → **#699** (tenant creation refactor).

### Testing / Infra

- **#898** Handler-level on_parse fallback-path tests.
- **#411** Enforce clippy policy.
- **#317** integration_test NO_COLOR + diff on golden mismatch.
- **#419** doc-lint should validate code_entrypoint symbols exist.

## Issue Resolution Map (Execution Order)

```
Phase 0 — Testing / Infra (parallel, independent)
├── #898 || #317 || #419 || #411
└── #700 → #699                      [admin portal: security before refactor]
    Validation: cargo test, admin-portal E2E

Phase 1 — SQL parity (remaining)
└── #395 || #396 || #397             [collated index + ALTER TYPE mismatches]
    Validation: cargo test + SQL integration + golden file diff vs PG 17.7

Phase 2 — ORM compatibility
├── #840 Prisma
├── #375 Dify
└── #885 Activepieces (depends on #840 + #375)
    Validation: cargo test + orm-tests (npm test) + Activepieces migration suite

Phase 3 — Performance
├── #857 → #707 (pre-materialization dedup, then plan cache)
└── #708 (parallel execution, independent long-term track)
    Validation: cargo test + cargo bench + SQL integration

Phase 4 — Architecture debt (parallel)
├── Independent: #696 || #695 || #694 || #693
└── #779 (sequential: connection caps → tenant QPS → timeout → memory)
    Validation: cargo test + cargo clippy
```

## Repository Layout (stable)

```
db9-server/
├── src/
│   ├── sql/                           # SQL engine (~118K lines)
│   │   ├── analyzer/                  # Semantic analysis → AnalyzedQuery/TypedExpr
│   │   │   ├── expr/                  # Expression analysis (coercion, functions, literals, operators)
│   │   │   ├── query/                 # Query analysis (from_clause, group_by, projection, set_expr)
│   │   │   └── types/                 # Typed IR definitions (TypedExpr, AnalyzedQuery)
│   │   ├── optimizer/                 # CBO: LogicalPlan → PhysicalPlan → operators
│   │   │   ├── logical_planner/       # AnalyzedQuery → LogicalPlan
│   │   │   ├── physical_planner/      # LogicalPlan → PhysicalPlan
│   │   │   ├── build/                 # PhysicalPlan → BoxedOperator (scan, join, aggregate)
│   │   │   ├── rewrite/               # Plan rewrites (decorrelation, predicate pushdown)
│   │   │   ├── join_reorder/          # Cost-based join reordering (DPccp algorithm)
│   │   │   └── selectivity/           # Selectivity estimation from column statistics
│   │   ├── operators/                 # Physical operators (Volcano iterator model)
│   │   │   ├── hash_join/             # Equi-join with hash table
│   │   │   ├── hash_semi_join.rs      # Semi/anti-join for EXISTS decorrelation
│   │   │   └── window/                # Window functions (access, aggregates, ranking)
│   │   ├── executor/                  # DDL/DML dispatch + SELECT execution
│   │   │   ├── core/                  # Statement dispatch + infrastructure
│   │   │   │   ├── dispatch/          # Statement routing (mod.rs, prepared.rs, utils.rs)
│   │   │   │   ├── view_rewrite/      # View expansion (expr.rs, query.rs, table.rs)
│   │   │   │   └── catalog_prefetch/  # Batch catalog lookups
│   │   │   ├── select/analyzed/       # Single-path SELECT executor
│   │   │   ├── dml_analyzed/          # Analyzed INSERT/UPDATE/DELETE
│   │   │   └── procedure/             # Stored procedures + materialized views
│   │   ├── expr/                      # Expression system
│   │   │   ├── typed_eval/            # Runtime evaluator (arithmetic, helpers)
│   │   │   ├── traverse/              # Expression tree traversal
│   │   │   └── functions/             # 14 categories (array…vector)
│   │   ├── catalog/                   # information_schema + pg_catalog + cron (40+ views)
│   │   ├── types/                     # Type inference, coercion, mapping
│   │   │   ├── registry/              # FunctionRegistry (aggregate_window, json, math, misc, string, system, temporal)
│   │   │   └── cast/                  # CAST between types
│   │   ├── ddl/                       # DDL: CREATE/ALTER/DROP (alter_table, create_index, create_table, drop, view)
│   │   ├── dml/                       # DML helpers: defaults, foreign_keys, insert, update, delete
│   │   ├── session/                   # Per-session state (settings, transaction)
│   │   ├── planner/                   # Index selection, scan strategy, expression-index support
│   │   ├── explain/                   # EXPLAIN output (format, transform)
│   │   ├── triggers/                  # Trigger subsystem (cache, before, rewrite, enqueue, execute)
│   │   ├── sequences/                 # SEQUENCE management (DDL, eval, replace)
│   │   ├── rewriter/                  # SQL rewriter (flatten, remap)
│   │   ├── plpgsql/                   # PL/pgSQL parser + executor
│   │   ├── parser/                    # SQL parser (operator_rewrite, preprocess, tokenizer)
│   │   ├── binder/                    # Legacy name binding
│   │   └── stats.rs                   # TableStatsCache (per-tenant)
│   ├── protocol/
│   │   ├── copy_format.rs             # COPY format parsing (CSV, TEXT, BINARY)
│   │   └── handler/                   # pgwire handler
│   │       ├── dynamic/               # DynamicPgHandler (mod.rs, query.rs, copy.rs, startup.rs)
│   │       ├── encode/                # Value encoding + type mapping
│   │       ├── params/                # Parameter counting + decoding
│   │       ├── copy/                  # COPY context management
│   │       ├── portal.rs              # Portal state + suspended queries
│   │       ├── query_parser.rs        # Db9QueryParser (pgwire QueryParser trait)
│   │       ├── server_params.rs       # ParameterStatus provider
│   │       ├── tenant.rs              # Multi-tenancy username parsing
│   │       └── errors.rs              # SQLSTATE mapping + error helpers
│   ├── storage/
│   │   ├── encoding/                  # Key encoding (data_keys, metadata_keys, value_encoding, serialization)
│   │   ├── tikv_store/               # TiKV operations (tables, indexes, schemas, sequences, cron, worker, statistics, migrations)
│   │   └── kv_stats.rs               # KV read statistics tracking (task-local)
│   ├── worker/                        # Unified async task engine (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql)
│   ├── cron/                          # pg_cron-compatible scheduler (parser, types, config, worker, process_list)
│   ├── extensions/                    # HTTP extensions + fs9 file system (backend, decoders, streaming, glob)
│   ├── auth/                          # Authentication + RBAC
│   ├── types/                         # Type system (separate from sql/types/ — debt #695)
│   ├── txn/                           # Transaction state + savepoints
│   ├── main.rs                        # Server entry point (TLS, worker/cron startup)
│   ├── cli.rs                         # CLI argument parser
│   ├── config.rs                      # Server configuration
│   ├── session_context.rs             # Tokio task-local session context
│   ├── observability.rs               # Logging, tracing, metrics
│   ├── pool.rs                        # TiKV connection pool
│   └── tls.rs                         # TLS setup
├── docs/                              # Architecture + feature docs
├── tests/                             # SQL integration tests (557 test files)
├── orm-tests/                         # TypeORM, Prisma, Sequelize compatibility
└── scripts/
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `src/sql/expr/functions/` (14 categories: array, datetime, encoding, fs9, fts, json, math, misc, pg_compat, regex, string, uuid, vector) |
| Add SQL statement | `src/sql/executor/` (DDL in `ddl.rs`/`ddl/`, DML in `dml_analyzed/`, SELECT in `select/analyzed/`) |
| Fix type inference | `src/sql/types/infer.rs` |
| Fix analyzer / name resolution | `src/sql/analyzer/` (query/, expr/, scope.rs) |
| Optimizer / query planning | `src/sql/optimizer/` (logical_planner/ → rewrite/ → physical_planner/ → build/) |
| Join reordering | `src/sql/optimizer/join_reorder/` (algorithms.rs, cost.rs, predicates.rs) |
| Subquery decorrelation | `src/sql/optimizer/rewrite/decorrelate.rs` |
| Physical operators | `src/sql/operators/` (scan, join, hash_join/, hash_semi_join, sort, aggregate, window/, etc.) |
| Index planning / scan strategy | `src/sql/planner/` (mod.rs, index_selection.rs, predicate.rs, scan_type.rs) |
| EXPLAIN output | `src/sql/explain/` (mod.rs, format.rs, transform.rs) |
| Add PostgreSQL type mapping | `src/protocol/handler/encode/types.rs` |
| Change key encoding | `src/storage/encoding/` (data_keys.rs, metadata_keys.rs, value_encoding.rs, serialization.rs) |
| Catalog / pg_catalog views | `src/sql/catalog/` (40+ virtual table implementations) |
| Multi-tenancy | `src/pool.rs` + `src/protocol/handler/tenant.rs` |
| Triggers | `src/sql/triggers/` (cache, before, rewrite, enqueue, execute) + `src/sql/executor/triggers.rs` (DDL) |
| Transaction state | `src/txn/` (state.rs, savepoints.rs) |
| Session state / GUCs | `src/sql/session/` (mod.rs, settings.rs, transaction.rs) + `src/session_context.rs` |
| Worker / background tasks | `src/worker/` (engine.rs, types.rs, config.rs, gc.rs, metrics.rs) |
| Cron scheduling | `src/cron/` (parser.rs, types.rs, config.rs, worker.rs, process_list.rs) |
| DDL (CREATE/ALTER/DROP) | `src/sql/ddl/` (alter_table.rs, create_index.rs, create_table.rs, drop.rs, view.rs) |
| DML helpers / FK validation | `src/sql/dml/` (foreign_keys.rs, defaults.rs, insert.rs, update.rs, delete.rs) |
| PL/pgSQL | `src/sql/plpgsql/` (parser.rs, executor.rs, utils.rs) |
| fs9 file operations | `src/extensions/fs/` (mod.rs, backend.rs, decoders.rs, streaming.rs, glob.rs) |

## Build & Test Commands

```bash
cargo build
cargo test
PD_ENDPOINTS=127.0.0.1:2379 cargo run
python3 scripts/integration_test.py
cd orm-tests && npm test
```

## SQL Test Contract (must follow)

- `.expected` > `.errors` > `.assert` priority; use only one validation mode per test.
- Always enforce deterministic output (`ORDER BY`, fixed values, no random-dependent assertions).
- Do not update expected outputs blindly; validate against real PostgreSQL first.
- **Before changing any `.expected`, `.errors`, or `.assert` file, you MUST run the corresponding `.sql` against real PostgreSQL 17.7 and verify the new expected output matches PG's actual output.** No exceptions — guessing what PG returns is not acceptable.

## Documentation

- `docs/architecture.md` - Full architecture design document
- `src/sql/AGENTS.md` - SQL execution layer details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details
- `docs/worker.md` - Worker engine deep dive
- `docs/extensions.md` - Extension functions
- `docs/fs9_extension.md` - fs9 filesystem extension
- `docs/authentication.md` - Auth/RBAC
- `docs/multi-tenancy.md` - Keyspace isolation
- `docs/prepared-statement-contract.md` - Prepared statement semantics
