# db9-server Code Wiki

> **db9-server** is a PostgreSQL-compatible distributed SQL database built on TiKV. It accepts standard PostgreSQL wire protocol connections, parses and analyzes SQL, optimizes queries through a cost-based optimizer, and executes them against a distributed TiKV key-value store.

---

## Technology Stack

| Layer | Technology |
|-------|-----------|
| Language | Rust (edition 2021) |
| Async runtime | Tokio (multi-threaded) |
| SQL parser | `sqlparser-rs` 0.40 |
| Storage engine | TiKV (via vendored `tikv-client`) |
| Wire protocol | pgwire (PostgreSQL v3 protocol) |
| TLS | `tokio-rustls` |
| Observability | `tracing` + `tracing-subscriber` |

---

## Wiki Pages

### Architecture

- [Architecture Overview](Architecture-Overview.md) -- System design, execution pipeline, module boundaries, and key invariants.

### Getting Started

- [Getting Started](Getting-Started.md) -- Build instructions, configuration reference, connecting via psql, and running tests.

### SQL Engine

- [Parser](SQL-Engine/Parser.md) -- SQL parsing layer
- [Analyzer](SQL-Engine/Analyzer.md) -- Semantic analysis, TypedExpr, scope
- [Optimizer](SQL-Engine/Optimizer.md) -- CBO pipeline
- [Executor](SQL-Engine/Executor.md) -- DDL/DML dispatch, SELECT execution
- [Operators](SQL-Engine/Operators.md) -- Physical operators (Volcano model)
- [Expression System](SQL-Engine/Expression-System.md) -- Expression evaluation, function categories
- [Type System](SQL-Engine/Type-System.md) -- Type inference, coercion
- [Catalog Views](SQL-Engine/Catalog-Views.md) -- pg_catalog / information_schema
- [DDL](SQL-Engine/DDL.md) -- CREATE/ALTER/DROP
- [DML](SQL-Engine/DML.md) -- INSERT/UPDATE/DELETE, foreign keys
- [Planner and Index Selection](SQL-Engine/Planner-and-Index-Selection.md) -- Index selection, scan strategy
- [Session and GUC](SQL-Engine/Session-and-GUC.md) -- Session state, SET/SHOW
- [PL/pgSQL](SQL-Engine/Advanced-SQL/PL-pgSQL.md) -- Procedural language
- [Sequences](SQL-Engine/Advanced-SQL/Sequences.md) -- SEQUENCE management
- [Triggers](SQL-Engine/Advanced-SQL/Triggers.md) -- Trigger subsystem
- [Full-Text Search](SQL-Engine/Advanced-SQL/Full-Text-Search.md) -- GIN, tsvector/tsquery
- [Rewriter](SQL-Engine/Advanced-SQL/Rewriter.md) -- SQL rewriter

### Diagrams

- [System Architecture](Diagrams/system-architecture.md) -- System panorama diagram
- [Query Execution Flow](Diagrams/query-execution-flow.md) -- Query execution sequence diagram
- [Module Dependencies](Diagrams/module-dependencies.md) -- Module dependency graph
- [Optimizer Pipeline](Diagrams/optimizer-pipeline.md) -- CBO pipeline detail
- [Storage Key Layout](Diagrams/storage-key-layout.md) -- Key encoding layout

---

## Repository Structure (Top Level)

```
db9-server/
  src/              -- Rust source (~118K lines in the SQL layer alone)
    sql/            -- SQL engine (analyzer, optimizer, executor, operators, catalog)
    protocol/       -- pgwire protocol handler
    storage/        -- TiKV storage layer (key encoding, schema, indexes)
    worker/         -- Unified async task engine (cron, triggers, auto-analyze)
    cron/           -- pg_cron-compatible scheduler
    extensions/     -- HTTP extensions, fs9 file system
    auth/           -- Authentication and RBAC
    model/          -- Core data model types (DataType, Value, Row, TableSchema)
    txn/            -- Transaction state and savepoints
    main.rs         -- Server entry point
    cli.rs          -- CLI argument parser
    config.rs       -- Server configuration
  docs/             -- Architecture docs, contracts, user guides
  tests/            -- SQL integration tests (287 test files)
  orm-tests/        -- ORM compatibility tests (TypeORM, Prisma, Sequelize)
  scripts/          -- Build, test, and operational scripts
  vendor/           -- Vendored dependencies (tikv-client)
```

---

## Design Principles

1. **PostgreSQL is the specification.** SQL semantics, type coercion, catalog behavior, error codes, and wire protocol responses align with PostgreSQL.
2. **Single-path deterministic execution.** Every query follows exactly one code path through the system. No hidden fallbacks.
3. **Correctness first, then speed.** Fixes target the root layer where an invariant is broken.
4. **Clear module boundaries.** Each module has a single source of truth for its domain.
5. **Multi-tenancy isolation is non-negotiable.** All persistent data is isolated per keyspace.

---

## Quick Reference

| I want to... | Start here |
|--------------|-----------|
| Understand the execution pipeline | [Architecture Overview](Architecture-Overview.md) |
| Build and run the server | [Getting Started](Getting-Started.md) |
| Add a SQL function | `src/sql/expr/functions/` + `src/sql/types/registry/` |
| Fix type inference | `src/sql/types/coercion.rs` |
| Fix the analyzer | `src/sql/analyzer/` |
| Modify optimizer rules | `src/sql/optimizer/` |
| Add a physical operator | `src/sql/operators/` |
| Add a catalog view | `src/sql/catalog/` |
| Change key encoding | `src/storage/encoding/` |
| Fix wire protocol behavior | `src/protocol/handler/` |
| Add a DDL statement | `src/sql/ddl/` |
| Fix DML / foreign keys | `src/sql/dml/` |
| Work on triggers | `src/sql/triggers/` |
| Work on cron scheduling | `src/cron/` |
| Work on background tasks | `src/worker/` |

---

## Documentation Tiers

| Tier | Location | Purpose |
|------|----------|---------|
| Normative contracts | `docs/sot/` | MUST/SHOULD/MAY rules; updated with every behavior change |
| Descriptive architecture | `docs/architecture/` + `docs/ARCHITECTURE.md` | How-it-works explanations and diagrams |
| Navigation | `src/*/AGENTS.md` | File paths, entry points, function signatures |
| User guides | `docs/*.md` | Configuration, features, extensions |

---

*Source files: `src/main.rs`, `src/cli.rs`, `src/config.rs`, `CLAUDE.md`, `docs/ARCHITECTURE.md`, `src/sql/AGENTS.md`*
