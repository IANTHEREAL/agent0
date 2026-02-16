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
Client/ORM -> pgwire -> SQL Parser -> Analyzer -> Typed IR -> Executor/Operators -> TiKV Store -> TiKV
```

### Module Boundaries

- `Analyzer`: name resolution, scope checking, and type inference; outputs typed IR only.
- `Typed IR`: explicit types, explicit function/aggregate resolution, deterministic execution contract.
- `Executor/Operators`: physical execution (scan/filter/project/sort/aggregate/join), no hidden semantic fallback.
- `Catalog`: `information_schema` / `pg_catalog` compatibility surface and metadata contract.
- `Storage`: all persistent keys must remain keyspace-isolated via `TikvStore`.

### Multi-tenancy Invariant (Critical)

- All persistent data must be isolated per keyspace (`_sys_*`, table rows, indexes, auth, and future stats).
- Process-level global state is limited to in-memory caches/config/logging.

## Sprint 1 Completed

- **Task 1:** Analyzer → Typed IR pipeline (single execution path for all SELECT queries)
- **#56** NLJ streaming — NLJ now streams outer side, materializes only build side (`src/sql/operators/join.rs`)
- **#609** SELECT privilege enforcement — `require_table_privilege(Select)` on every base table (`mod.rs:85-99`)
- **#634/#635** COPY CSV fixes — order-independent option parsing, ESCAPE self-escaping (`copy_format.rs`)
- Legacy removal: `infer_expr_type`, `executor/subquery.rs`, boolean validator, comparison coercion fallback

## Current Problems (Active)

### P1 — Cost-Based Optimizer (#728)

- Next major milestone: `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`.
- **Planner dual-path resolved:** TypedExpr path now has full GIN + expression-index + partial-index support. EXPLAIN uses the analyzed pipeline (view expansion → Analyzer → typed planner). AST path retained only for non-SELECT EXPLAIN and analysis error fallback.
- Phase 1: LogicalPlan pipeline for single-table SELECTs (no optimizer rules, no statistics).
- Phases 2-4: Statistics (#706), join reordering/decorrelation (#705), plan cache (#707).

## Repository Layout (stable)

```
pg-tikv/
├── src/
│   ├── sql/
│   ├── protocol/
│   ├── storage/
│   ├── auth/
│   ├── types/
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
| Add SQL function | `src/sql/expr.rs` / `src/sql/expr/functions/` |
| Add SQL statement | `src/sql/executor.rs` + `src/sql/executor/` |
| Fix type inference | `src/sql/types/infer.rs` |
| Add PostgreSQL type mapping | `src/protocol/handler/encode/types.rs` |
| Change key encoding | `src/storage/encoding.rs` |
| Catalog behavior | `src/sql/catalog/` + `src/sql/information_schema.rs` |
| Multi-tenancy | `src/pool.rs` + username parsing in handler |

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

## Child AGENTS.md

- `src/sql/AGENTS.md` - SQL execution details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details
