# pg-tikv Knowledge Base (Living Document)

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
- `Optimizer` (`src/sql/optimizer/`): `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`. Handles single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, and DISTINCT ON. Always-on (the `tipg.use_optimizer` GUC is accepted for compatibility but is a no-op — `SET ... = off` logs a notice and is ignored; `SHOW` always returns `on`).
- `Operators` (`src/sql/operators/`): physical operators (scan, filter, project, sort, aggregate, hash_join, NLJ, window, CTE, set_operation, table_function).
- `Executor` (`src/sql/executor/`): DDL/DML dispatch, SELECT execution (analyzed path at `executor/select/analyzed/`).
- `Catalog` (`src/sql/catalog/`): `information_schema` / `pg_catalog` compatibility surface (35+ virtual table implementations).
- `Storage`: all persistent keys must remain keyspace-isolated via `TikvStore`.

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
- `tipg.use_optimizer` GUC retained for compatibility (always-on, no-op when set to off).
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

- **Prisma ORM** binary wire protocol (#841/#842).
- **PL/pgSQL** execution: SELECT INTO, FOR loops, EXIT, correlated table functions (#863).
- **Cron** runtime management: job timeout, cancel, process list (#896), status command (#913), --file + dollar-quoting (#892).
- **fs9** file system: `fs9_read`, `fs9_write`, `fs9_exists`, `fs9_size`, `fs9_mtime`, `fs9_remove`, remote backend routing, SDK API (#851/#853/#855/#878).
- **SQL functions**: json_agg/jsonb_agg, hashtext (#930), RETURNS TABLE syntax (#895).
- **FTS** Chinese tokenizer support for GIN full-text search (#774).
- **Infra**: SQL execution timeout protection (#860), stack overflow prevention (#929), information_schema.columns optimization (#936), unified SqlError with SQLSTATE (#900).

## Open Issues (Active)

### Correctness — FK cluster (shared module: `src/sql/dml/foreign_keys.rs` + `src/sql/ddl/alter_table.rs`)

All 4 FK issues touch shared validation/cascade paths. Safe execution order: (#925 + #923) first, then (#924 + #922).

- **#925** FK runtime validation ignores ref_columns, enforces parent PK only.
- **#923** FK NULL handling doesn't match PostgreSQL MATCH SIMPLE.
- **#924** FK ON DELETE path uses stale child snapshots across multiple FKs. Depends on: #925.
- **#922** FK self-referential constraints skipped in UPDATE/DELETE. Depends on: #925.

### Correctness — Analyzer / SQL semantics

- **#910** Mixed unknown binary ops miss SQLSTATE 42725 parity. → **#911** (test coverage).
- **#601** Implement SET LOCAL (transaction-scoped settings). → **#884** (`current_setting()` + SET LOCAL rollback).
- **#600** PostgreSQL-compatible defaults for SHOW on common GUCs. → **#599** (SHOW ALL).
- **#408** Support UNNEST(...) as JOIN relation. Blocks: #885.
- **#397/#396** Collated string index test mismatches.
- **#395** ALTER TYPE test mismatch.
- **#281** JSON/JSONB canonicalization mismatches (key order, whitespace). Blocks: #375.

### Usability — ORM compatibility

- **#885** Activepieces drop-in compatibility. Depends on: #408, #601, remaining syntax gaps.
- **#840** Prisma ORM remaining 6 test gaps (upsert, JSONB path, SERIALIZABLE). Depends on: Phase 1 correctness.
- **#375** Dify compatibility (introspection, RETURNING, JSONB operators). Depends on: #281.
- **#920** db9 CLI: `login --api-key` 401 chicken-and-egg auth bug. Independent.

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

- **#911** Broaden 42725 parity test coverage. Depends on: #910.
- **#898** Handler-level on_parse fallback-path tests.
- **#411** Enforce clippy policy.
- **#317** integration_test NO_COLOR + diff on golden mismatch.
- **#419** doc-lint should validate code_entrypoint symbols exist.

## Issue Resolution Map (Execution Order)

```
Phase 0 — Quick wins (start now, parallel lanes)
├── Lane T1: #898 || #317 || #419 || #411    [testing/infra, independent]
├── Lane T2: #920                              [db9 CLI auth, independent]
└── Lane T3: #700 → #699                      [admin portal: security before refactor]
    Validation: cargo test, db9 CLI smoke, admin-portal E2E

Phase 1 — Core correctness (highest priority)
├── Lane C1 (FK):       (#925 + #923) → (#924 + #922)
│   Shared: src/sql/dml/foreign_keys.rs, src/sql/ddl/alter_table.rs
│   Validation: cargo test + SQL integration (FK-specific tests)
├── Lane C2 (GUC):      #601 → #884
│   Validation: cargo test + SHOW/SET integration tests
├── Lane C3 (SHOW):     #600 → #599
│   Validation: cargo test + psql SHOW ALL comparison vs PG 17.7
├── Lane C4 (operator):  #910 → #911
│   Validation: cargo test + SQLSTATE parity tests
└── Lane C5 (SQL parity): #408 || #281 || #395 || #396 || #397
    Validation: cargo test + SQL integration + golden file diff vs PG 17.7

Phase 2 — ORM compatibility (after Phase 1)
├── #840 Prisma (depends on Phase 1 correctness lanes)
├── #375 Dify (depends on #281)
└── #885 Activepieces (depends on #840 + #375 + #408 + #601)
    Validation: cargo test + orm-tests (npm test) + Activepieces migration suite

Phase 3 — Performance (after Phase 1 stabilizes)
├── #857 → #707 (pre-materialization dedup, then plan cache)
└── #708 (parallel execution, independent long-term track)
    Validation: cargo test + cargo bench + SQL integration

Phase 4 — Architecture debt (parallel, after Phase 1 stabilizes)
├── Independent: #696 || #695 || #694 || #693
└── #779 (sequential: connection caps → tenant QPS → timeout → memory)
    Validation: cargo test + cargo clippy
```

## Repository Layout (stable)

```
pg-tikv/
├── src/
│   ├── sql/
│   │   ├── analyzer/          # Semantic analysis → AnalyzedQuery/TypedExpr
│   │   ├── optimizer/         # CBO: LogicalPlan → PhysicalPlan → operators
│   │   ├── operators/         # Physical operators (scan, join, sort, agg, window…)
│   │   ├── executor/          # DDL/DML dispatch + select/analyzed/ path
│   │   ├── expr/              # Expression system + functions/ (14 categories)
│   │   ├── catalog/           # information_schema + pg_catalog (35+ views)
│   │   ├── types/             # Type inference, coercion, mapping
│   │   ├── binder/            # Legacy name binding
│   │   ├── planner/           # Query planner (index selection, scan planning)
│   │   ├── explain/           # EXPLAIN (uses analyzed pipeline)
│   │   ├── triggers/           # Trigger subsystem (cache, before, queue, worker, enqueue, execute, claim, gc)
│   │   ├── stats.rs           # TableStatsCache (per-tenant)
│   │   └── ...
│   ├── protocol/
│   │   └── handler/           # pgwire handler (dynamic/ module: mod.rs, query.rs, copy.rs, startup.rs)
│   ├── storage/
│   ├── extensions/            # HTTP extensions + fs9 file operations
│   ├── auth/
│   ├── types/
│   ├── txn/                   # Transaction state + savepoints
│   ├── pool.rs
│   ├── tls.rs
│   └── main.rs
├── tests/         # SQL integration tests
├── orm-tests/     # TypeORM, Prisma, Sequelize compatibility
└── scripts/
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `src/sql/expr/functions/` (14 categories: array, datetime, encoding, fs9, fts, json, math, misc, pg_compat, regex, string, uuid, vector) |
| Add SQL statement | `src/sql/executor/` (DDL in `ddl.rs`, DML in `dml_analyzed/` (insert.rs, update.rs, delete.rs), SELECT in `select/analyzed/`) |
| Fix type inference | `src/sql/types/infer.rs` |
| Fix analyzer / name resolution | `src/sql/analyzer/` (query/, expr/, scope.rs) |
| Optimizer / query planning | `src/sql/optimizer/` (logical_planner/ → physical_planner/ → build/) |
| Physical operators | `src/sql/operators/` (scan, join, hash_join, sort, aggregate, window, etc.) |
| Index planning / scan strategy | `src/sql/planner/` (mod.rs, index_selection.rs, predicate.rs, scan_type.rs) |
| EXPLAIN output | `src/sql/explain/` (mod.rs, format.rs, transform.rs) |
| Add PostgreSQL type mapping | `src/protocol/handler/encode/types.rs` |
| Change key encoding | `src/storage/encoding/` (data_keys.rs, metadata_keys.rs, value_encoding.rs, serialization.rs) |
| Catalog / pg_catalog views | `src/sql/catalog/` (35+ pg_* view implementations) |
| Multi-tenancy | `src/pool.rs` + username parsing in handler |
| Triggers | `src/sql/triggers/` (cache, before, queue, worker, enqueue, execute, claim, gc) + `src/sql/executor/triggers.rs` (DDL) |
| Transaction state | `src/txn/` (state.rs, savepoints.rs) |

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

## Child AGENTS.md

- `src/sql/AGENTS.md` - SQL execution details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details
