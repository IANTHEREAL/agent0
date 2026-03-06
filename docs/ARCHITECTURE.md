# db9-server Architecture

> PostgreSQL-compatible distributed SQL database on TiKV.

## 1. Design Principles

### 1.1 PostgreSQL is the specification

SQL semantics, type coercion, catalog behavior, SQLSTATEs, and protocol-visible behavior should follow PostgreSQL by default. Intentional divergence is allowed only when it is explicit, justified, and documented in SoT.

### 1.2 Single semantic path for analyzed execution

db9 enforces one semantic path once a statement enters analyzed query execution, analyzed DML, or analyzed prepared execution. It does not silently try a second planner/executor because analysis failed.

This invariant is deliberately narrower than "no fallback anywhere". Current code still contains explicit compatibility exceptions at documented boundaries, including:
- protocol/parser utility acceptance for some raw-SQL utility shapes;
- prepared execution reparse for schema drift;
- prepared recursive CTE fallback to text execution (tracked in `#1516`).

Those exceptions are explicit compatibility shims, not hidden alternate planners.

### 1.3 Correctness before speed

Fix the broken invariant at the owning layer. Do not hide internal inconsistency in tests, protocol formatting, or documentation.

### 1.4 Clear module boundaries

- Analyzer: name resolution, typing, and semantic validation
- Rewriter: post-analysis query normalization/rewrite
- Optimizer: logical/physical planning and access-path choice
- Executor/Operators: physical execution
- Catalog: virtual catalog surfaces
- Storage: persistent layout and TiKV interaction

### 1.5 Multi-tenancy isolation is non-negotiable

Persistent state is isolated per TiKV keyspace. Tenant-scoped in-memory state lives under tenant-owned pool entries rather than process-global mutable state.

## 2. System Overview

```
Clients / ORMs / psql
        │
        ▼
pgwire protocol
        │
        ▼
db9-server
  ├─ Protocol layer
  │   ├─ startup/auth
  │   ├─ simple query
  │   ├─ extended query
  │   └─ COPY handling
  ├─ SQL layer
  │   ├─ parser
  │   ├─ analyzer
  │   ├─ rewriter
  │   ├─ optimizer
  │   └─ executor/operators
  ├─ Catalog layer
  │   └─ pg_catalog / information_schema / cron virtual tables
  ├─ Storage layer
  │   └─ TiKV-backed metadata, rows, indexes, stats, worker state
  └─ Background services
      ├─ worker engine
      ├─ cron scheduler
      └─ worker GC + HNSW sweep
        │
        ▼
TiKV cluster
```

## 3. Execution Pipeline

### 3.1 High-level statement flow

```
client SQL
  → parser (`parse_sql`)
  → statement dispatch
    → transaction / settings / utility handlers, or
    → analyzed query path, or
    → analyzed DML path, or
    → prepared execution path
```

### 3.2 Canonical analyzed `SELECT/WITH` path

The semantic entrypoint is `analyze_then_rewrite_query()`:

```
AST query
  → view expansion
  → catalog snapshot build
  → base-table SELECT privilege checks
  → Analyzer
  → post-analysis rewriter
  → LogicalPlanner
  → PhysicalPlanner
  → operator builder
  → physical operator execution
  → result post-processing
```

This same semantic pipeline is shared by `EXPLAIN SELECT/WITH`.

### 3.3 DML path

```
INSERT / UPDATE / DELETE
  → analyzer DML entry
  → typed expressions / coercion checks
  → analyzed DML executor
  → index maintenance
  → trigger handling
  → optional background follow-up (for example HNSW merge enqueue)
```

### 3.4 Prepared execution

Prepared statements reuse analyzed/prepared structures when valid, with explicit reparse only for documented compatibility cases such as schema drift or unsupported prepared recursive CTE execution (tracked in `#1516`).

Session-local prepared plan caching is shipped for eligible prepared queries. The broader roadmap item is not "whether any plan cache exists", but whether db9 should add larger-scope parameterized/shared reuse beyond the current session-local cache.

## 4. Shipped Architectural Capabilities

- Analyzer-backed query and DML pipeline
- Always-on optimizer pipeline
- Post-analysis query rewriter
- Session-local prepared plan cache
- GIN planner/runtime access path
- HNSW nearest-neighbor access path
- HNSW delta-log storage plus background sweep/merge
- Worker engine for cron, async triggers, auto-analyze, background SQL, and background DDL
- Virtual catalog compatibility surfaces

## 5. Roadmap Boundaries

### Shipped now

- Session-local prepared plan cache for eligible prepared statements
- HNSW scan support and background merge architecture

### Still open

- Broader plan-cache work tracked by `#707`
  - larger-scope parameterized/shared reuse remains a roadmap topic
- Parallel/distributed execution framework tracked by `#708`

## 6. Module Map

| Module | Deep-dive | SoT |
|---|---|---|
| SQL Engine | [architecture/sql-engine.md](architecture/sql-engine.md) | [sot/sql-engine.md](sot/sql-engine.md) |
| Protocol | [architecture/protocol.md](architecture/protocol.md) | [sot/protocol-pgwire.md](sot/protocol-pgwire.md) |
| Storage | [architecture/storage.md](architecture/storage.md) | [sot/storage-format.md](sot/storage-format.md) |
| Multi-tenancy | [architecture/multi-tenancy.md](architecture/multi-tenancy.md) | [sot/multi-tenancy.md](sot/multi-tenancy.md) |
| Worker/Cron | [architecture/worker.md](architecture/worker.md) | [sot/worker-cron.md](sot/worker-cron.md) |
| Auth/RBAC | — | [sot/auth-rbac.md](sot/auth-rbac.md) |
| Catalog | — | [sot/catalog-introspection.md](sot/catalog-introspection.md) |
| Extensions | — | [sot/extensions-gin.md](sot/extensions-gin.md) |
| Config | — | [sot/ops-config.md](sot/ops-config.md) |
| Testing | — | [sot/testing-gates.md](sot/testing-gates.md) |
| Invariants | — | [sot/invariants.md](sot/invariants.md) |

## 7. Documentation Rules

- `docs/sot/**` is normative.
- `docs/architecture/**` and this file are descriptive and should track current architecture, but SoT remains authoritative for contracts.
- `docs/design/**` must not be treated as current architecture unless explicitly marked active.
