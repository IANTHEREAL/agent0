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

## Current Problems (Active)

### P1 — JOIN executor fully materializes intermediate rows (#56)

- Symptom: OOM on large joins; LIMIT applied after full materialization.
- Root layer: `Executor` (`src/sql/operators/join.rs`) — NLJ calls `collect_all()` on both sides.
- PostgreSQL behavior: NLJ streams the outer side; never materializes both.
- Direction: stream probe side via `next()`, materialize only build side.

### P1 — RBAC: SELECT privilege not enforced (#609)

- Symptom: any authenticated user can SELECT from any table.
- Root layer: `Executor` (`src/sql/executor/select/analyzed/mod.rs`) — no privilege gate.
- PostgreSQL behavior: SELECT requires SELECT privilege on each referenced base table.
- Direction: extract base tables from AnalyzedQuery, call `require_table_privilege()` per table.
- Note: DDL/DML privilege checks are fully wired (24+ call sites in `statement.rs`).

### P1 — COPY CSV parsing bugs (#634, #635)

- Symptom: DELIMITER/NULL parsing order-dependent; ESCAPE character not escaped.
- Root layer: `src/sql/executor/core/copy.rs`.
- PostgreSQL behavior: COPY options are order-independent; ESCAPE within data is always escaped.
- Direction: fix parsing to match PostgreSQL COPY semantics.

### P2 — Planner dual-path: AST vs TypedExpr access-path selection

- Root layer: `src/sql/planner.rs` — two functions: `choose_best_access_path_for_filter` (AST) and `choose_best_access_path_for_typed_filter` (TypedExpr). Typed version lacks GIN/expression-index support.
- Direction: port all capabilities to typed path, remove AST path. Blocks optimizer (Task 2).
- Deferred to Phase 2A (pre-optimizer cleanup).

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
