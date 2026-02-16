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

### P0 — TypeORM `synchronize` idempotency failure

- Symptom: `relation "... already exists"` during second initialize.
- Root layer: `Executor` (`src/sql/executor/select/analyzed/mod.rs`) scalar-in-FROM branch.
- Cause: FROM-function path returned `NULL` for zero-arg scalar functions (e.g. `current_schema()`, `current_database()`), causing ORM introspection mismatch.
- Direction: evaluate a real `FunctionCall` typed expression in scalar-in-FROM path.

### P0 — Legacy FROM-function inconsistency for `CURRENT_DATABASE`

- Symptom: non-deterministic database-name behavior across execution paths.
- Root layer: `Executor` (`src/sql/executor/table_utils.rs`).
- Cause: hardcoded `"testdb"` in legacy FROM-function handling.
- Direction: resolve from task-local/query context, no hardcoded DB name.

### P1 — TypeORM test-suite cross contamination

- Symptom: `typeorm_embeddings already exists` across suites.
- Root layer: test lifecycle hygiene in `orm-tests/typeorm/*.test.ts`.
- Cause: missing teardown for `typeorm_embeddings` in multiple suites using `synchronize: true`.
- Direction: each suite must drop all objects it creates (explicit cleanup in `afterAll`).

### P2 — `information_schema.columns.data_type` precision for `text` vs `varchar`

- Symptom: ORM expectations vary on `text`/`character varying`.
- Root layer: `Catalog` type model (`DataType` currently lacks independent `Varchar` variant).
- Cause: single `DataType::Text` currently serves multiple SQL declarations.
- Direction: solve via type-model evolution; do not patch via test-only masking.

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
- **Before changing any `.expected`, `.errors`, or `.assert` file, you MUST run the corresponding `.sql` against real PostgreSQL 17.7 and verify the new expected output matches PG's actual output.** No exceptions — guessing what PG returns is not acceptable.

## Child AGENTS.md

- `src/sql/AGENTS.md` - SQL execution details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details
