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
- `Optimizer` (`src/sql/optimizer/`): `AnalyzedQuery -> LogicalPlan -> PhysicalPlan -> BoxedOperator`. Handles single-table, multi-table, set operations, CTEs, window functions, and DISTINCT ON. Always-on single path; `db9.use_optimizer` remains a compatibility no-op GUC.
- `Operators` (`src/sql/operators/`): physical operators (scan, filter, project, sort, aggregate, hash_join, NLJ, window, CTE, set_operation, table_function).
- `Executor` (`src/sql/executor/`): DDL/DML dispatch, SELECT execution (analyzed path at `executor/select/analyzed/`).
- `Catalog` (`src/sql/catalog/`): `information_schema` / `pg_catalog` compatibility surface (35+ virtual table implementations).
- `Storage`: all persistent keys must remain keyspace-isolated via `TikvStore`.

### Multi-tenancy Invariant (Critical)

- All persistent data must be isolated per keyspace (`_sys_*`, table rows, indexes, auth, and future stats).
- Process-level global state is limited to in-memory caches/config/logging.

## Completed Milestones

### Sprint 1 — Analyzer + Legacy Cleanup

- **Task 1:** Analyzer -> Typed IR pipeline (single execution path for all SELECT queries)
- **#56** NLJ streaming — NLJ now streams outer side, materializes only build side (`src/sql/operators/join.rs`)
- **#609** SELECT privilege enforcement — `require_table_privilege(Select)` on every base table
- **#634/#635** COPY CSV fixes — order-independent option parsing, ESCAPE self-escaping
- Legacy removal: `infer_expr_type`, `executor/subquery.rs`, boolean validator, comparison coercion fallback
- All 10 legacy code items fully resolved (#772)

### CBO — Planner Unification + Optimizer Pipeline (#778, #815)

- `AnalyzedQuery -> LogicalPlan -> PhysicalPlan -> BoxedOperator` pipeline implemented in `src/sql/optimizer/`.
- **Planner dual-path resolved:** TypedExpr path has full GIN + expression-index + partial-index support. EXPLAIN uses the analyzed pipeline (view expansion -> Analyzer -> typed planner). AST path retained only for non-SELECT EXPLAIN and analysis error fallback.
- Coverage: single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, DISTINCT ON.
- GUC `db9.use_optimizer` is retained for compatibility/readback only (accepted as no-op; engine remains single-path).
- Handler decomposed: `view_infer.rs`, `schema_resolve.rs`, `type_infer.rs` extracted from monolithic handler.
- Index name uniqueness enforced within schema (#777).

### CBO Phase 2 — Statistics + Selectivity Estimation (#790)

- `ANALYZE` command collects per-column statistics (distinct count, null fraction, most-common values, histograms).
- `TableStatsCache` stores full column statistics per-tenant (`src/sql/stats.rs`).
- Selectivity estimation wired into `PhysicalPlanner` (`src/sql/optimizer/selectivity.rs`).
- Cardinality estimates propagated through LogicalPlan nodes.

### Other Recent

- **fs9** file system functions: `fs9_read`, `fs9_write`, `fs9_exists`, `fs9_size`, `fs9_mtime` with global read budget for OOM protection.
- **FTS** Chinese tokenizer support for GIN full-text search (#774).
- **Admin** API: rich DbError detail extraction, 200 with error body for SQL failures.

## Current Problems (Active)

### P1 — CBO Phases 3-4

- Phase 3: Join reordering / decorrelation (#705) — multi-table cost-based join ordering.
- Phase 4: Plan cache (#707) — parameterized plan reuse.

### P2 — Architecture Gaps

- #704: No query rewrite phase — missing standard SQL Rewriter module.
- #708: No parallel / distributed query execution framework.

## Repository Layout (stable)

```
db9-server/
├── src/
│   ├── sql/
│   │   ├── analyzer/          # Semantic analysis -> AnalyzedQuery/TypedExpr
│   │   ├── optimizer/         # CBO: LogicalPlan -> PhysicalPlan -> operators
│   │   ├── operators/         # Physical operators (scan, join, sort, agg, window...)
│   │   ├── executor/          # DDL/DML dispatch + select/analyzed/ path
│   │   ├── expr/              # Expression system + functions/ (14 categories)
│   │   ├── catalog/           # information_schema + pg_catalog (35+ views)
│   │   ├── types/             # Type inference, coercion, mapping
│   │   ├── binder/            # Legacy name binding
│   │   ├── planner.rs         # Query planner (index selection, scan planning)
│   │   ├── explain.rs         # EXPLAIN (uses analyzed pipeline)
│   │   ├── triggers.rs        # BEFORE trigger body compilation + cache
│   │   ├── trigger_worker.rs  # Background async trigger processing
│   │   ├── stats.rs           # TableStatsCache (per-tenant)
│   │   └── ...
│   ├── protocol/
│   │   └── handler/           # pgwire handler (dynamic.rs + view_infer, schema_resolve, type_infer)
│   ├── storage/
│   ├── extensions/            # HTTP extensions + fs9 file operations
│   ├── auth/
│   ├── model/                 # Data model types (DataType, Value, Row, TableSchema)
│   ├── txn/                   # Transaction state + savepoints
│   ├── pool.rs
│   ├── tls.rs
│   └── main.rs
├── tests/         # SQL integration tests
├── orm-tests/     # TypeORM, Prisma, Sequelize compatibility
├── docs/          # Architecture and design documents
└── scripts/
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `src/sql/expr/functions/` (14 categories: array, datetime, encoding, fs9, fts, json, math, misc, pg_compat, regex, string, uuid, vector) |
| Add SQL statement | `src/sql/executor/` (DDL in `ddl.rs`, DML in `dml_analyzed.rs`, SELECT in `select/analyzed/`) |
| Fix type inference | `src/sql/types/infer.rs` |
| Fix analyzer / name resolution | `src/sql/analyzer/` (query.rs, expr.rs, scope.rs) |
| Optimizer / query planning | `src/sql/optimizer/` (logical_planner.rs -> physical_planner.rs -> build.rs) |
| Physical operators | `src/sql/operators/` (scan, join, hash_join, sort, aggregate, window, etc.) |
| Index planning / scan strategy | `src/sql/planner.rs` |
| EXPLAIN output | `src/sql/explain.rs` |
| Add PostgreSQL type mapping | `src/protocol/handler/encode/types.rs` |
| Change key encoding | `src/storage/encoding.rs` |
| Catalog / pg_catalog views | `src/sql/catalog/` (35+ pg_* view implementations) |
| Multi-tenancy | `src/pool.rs` + username parsing in handler |
| Triggers | `src/sql/triggers.rs` (body cache) + `src/sql/executor/triggers.rs` (DDL) + `src/sql/trigger_worker.rs` (async) |
| Transaction state | `src/txn/` (state.rs, savepoints.rs) |

## Build & Test Commands

```bash
cargo build
cargo test
PD_ENDPOINTS=127.0.0.1:2379 cargo run
python3 scripts/integration_test.py
cd orm-tests && npm test
```

## E2E Integration Testing (mandatory for cross-component changes)

Changes touching `cloud-admin-portal/`, `db9` CLI, or `fs9` (sh9 shell, fs9-server, fs9-meta, fs9-client) **must** be validated in the full E2E Docker Compose environment before merging.

**Load the `e2e-up` skill** (`.codex/skills/e2e-up/SKILL.md`) for setup instructions, then:

```bash
cd deploy/e2e
./setup.sh            # builds all 7 services, waits healthy, runs smoke tests
```

After the stack is running, verify your change end-to-end via `db9` inside the container:

```bash
docker compose exec db9-admin db9 --api-url http://localhost:8090/api <command>
```

Rebuild only the affected service after code changes:

```bash
docker compose build <service>   # db9-admin | db9-server | fs9-server | fs9-meta
docker compose up -d <service>
```

## SQL Test Contract (must follow)

- `.expected` is exclusive full-output snapshot mode.
- When `.expected` is absent, `.errors` and `.assert` MAY be combined in one test (`.errors + .assert`) to validate both expected SQL diagnostics and required output fragments.
- Always enforce deterministic output (`ORDER BY`, fixed values, no random-dependent assertions).
- Do not update expected outputs blindly; validate against real PostgreSQL first.
- **Before changing any `.expected`, `.errors`, or `.assert` file, you MUST run the corresponding `.sql` against real PostgreSQL 17.7 and verify the new expected output matches PG's actual output.** No exceptions — guessing what PG returns is not acceptable.

## Child AGENTS.md

- `src/sql/AGENTS.md` - SQL execution details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details

## Design Documents

- `docs/ARCHITECTURE.md` - Comprehensive architecture design with principles and execution pipeline
