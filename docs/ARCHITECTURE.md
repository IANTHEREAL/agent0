# db9-server Architecture

> PostgreSQL-compatible distributed SQL database on TiKV.

## 1. Design Principles

These principles govern every design decision, code change, and review.

### 1.1 PostgreSQL is the Specification

All SQL semantics, type coercion rules, catalog behavior, error codes, and wire protocol responses must align with PostgreSQL. When in doubt, test against real PostgreSQL 17 and match its behavior. Never invent custom semantics.

### 1.2 Single-Path, Deterministic Execution

Every query follows exactly one code path. No hidden fallback, no runtime "try-new-then-old". If a path exists, it is **the** path. If the new path cannot handle a query shape, the system returns an error — it does not silently fall back to legacy code.

### 1.3 Correctness First, Then Speed

Fix at the root layer where the invariant is broken. Never use wire/test-layer masking to hide internal inconsistency. Never hack test expectations to match broken behavior.

### 1.4 Clear Module Boundaries

Each behavior has a clear module boundary and a single source of truth:
- **Analyzer** owns name resolution, type inference, and scope checking.
- **Optimizer** owns plan selection and cost estimation.
- **Executor/Operators** own physical execution.
- **Catalog** owns metadata queries.
- **Storage** owns key encoding and TiKV interaction.

No module reaches into another's responsibilities.

### 1.5 Structural Fixes Over Tactical Patches

Deliver structurally correct fixes, not tactical patches. If a fix requires touching 3+ callsites for the same symptom, the abstraction is wrong — fix the abstraction.

### 1.6 Multi-tenancy Isolation is Non-Negotiable

All persistent data must be isolated per keyspace. Process-level global state is limited to in-memory caches, configuration, and logging.

---

## 2. System Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    PostgreSQL Clients                        │
│          (psql, pgcli, ORMs, applications, agents)          │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ PostgreSQL Wire Protocol (pgwire)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                       db9-server Server                         │
│  ┌───────────────────────────────────────────────────────┐  │
│  │               Protocol Layer (pgwire)                  │  │
│  │  • Simple Query Handler    • Extended Query Handler    │  │
│  │  • Startup/Auth Handler    • COPY Handler              │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │                    SQL Layer                            │  │
│  │  ┌─────────┐  ┌──────────┐  ┌──────────┐             │  │
│  │  │ Parser  │→ │ Analyzer │→ │Optimizer │             │  │
│  │  │(sqlparser│  │(TypedExpr│  │  (CBO)   │             │  │
│  │  │   -rs)  │  │ /Analyzed│  │LogicalPlan│             │  │
│  │  │         │  │  Query)  │  │→Physical  │             │  │
│  │  └─────────┘  └──────────┘  └──────────┘             │  │
│  │       ↓              ↓             ↓                   │  │
│  │  ┌──────────────────────────────────────┐             │  │
│  │  │     Executor + Physical Operators     │             │  │
│  │  │  scan | filter | project | sort | agg │             │  │
│  │  │  join | hash_join | hash_semi_join     │             │  │
│  │  │  window | limit | set_operation       │             │  │
│  │  └──────────────────────────────────────┘             │  │
│  │       ↓                                                │  │
│  │  ┌──────────────────────────────────────┐             │  │
│  │  │  Catalog (37 pg_catalog/info_schema)  │             │  │
│  │  └──────────────────────────────────────┘             │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │                  Storage Layer                         │  │
│  │  • Key Encoding (v2)   • Schema Management             │  │
│  │  • Index Management    • Transaction Wrapper           │  │
│  │  • Keyspace Isolation  • Statistics Persistence        │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │              Background Services                       │  │
│  │  • Worker Engine (cron, triggers, auto-analyze, bg-sql)│  │
│  │  • Cron Scheduler (pg_cron-compatible expressions)     │  │
│  │  • Worker GC (orphan recovery, DLQ cleanup)            │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ gRPC (TiKV Client Protocol)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                       TiKV Cluster                           │
│    TiKV nodes (Raft consensus) + PD (Placement Driver)      │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. Execution Pipeline

Every query follows this pipeline. There are no alternative paths.

```
Client SQL string
    │
    ▼
┌─ Parser (sqlparser-rs) ──────────────────────────────────────┐
│  SQL text → AST (Vec<Statement>)                             │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Dispatcher (executor/core/dispatch.rs) ─────────────────────┐
│  Route AST node to handler:                                   │
│  ├─ DDL (CREATE/ALTER/DROP) → ddl.rs                         │
│  ├─ DML (INSERT/UPDATE/DELETE) → dml_analyzed.rs             │
│  ├─ SELECT/VALUES/SET-OP → Analyzer pipeline (below)         │
│  ├─ Transaction control (BEGIN/COMMIT/ROLLBACK)              │
│  ├─ Settings (SET/SHOW/RESET)                                │
│  └─ Special (EXPLAIN, ANALYZE, COPY, CALL)                   │
└──────────────────────────────────────────────────────────────┘
    │ (SELECT path)
    ▼
┌─ View Expansion ─────────────────────────────────────────────┐
│  expand_views_in_query(): recursively inline view definitions │
│  (happens before Analyzer, so views are transparent)          │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Privilege Check ────────────────────────────────────────────┐
│  require_table_privilege(Select) on every base table          │
│  (uses CatalogSnapshot::base_table_full_names())              │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Analyzer (src/sql/analyzer/) ───────────────────────────────┐
│  AST → AnalyzedQuery / TypedExpr                              │
│  • Name resolution: column refs → positional indices          │
│  • Type inference: every node carries resolved DataType       │
│  • Scope checking: correlated subqueries tracked              │
│  • GROUP BY compliance validation                             │
│  • Function/aggregate resolution                              │
│  OUTPUT: AnalyzedQuery with output_schema                     │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Optimizer Pipeline (Always On) ─────────────────────────────┐
│  ├─ LogicalPlanner: AnalyzedQuery → LogicalPlan              │
│  ├─ Load table statistics from TableStatsCache               │
│  ├─ PhysicalPlanner: LogicalPlan → PhysicalPlan              │
│  │   (selectivity estimation, cardinality propagation)       │
│  ├─ build.rs: PhysicalPlan → BoxedOperator                   │
│  └─ Execute operator tree → results                          │
│  NOTE: db9.use_optimizer is compatibility/readback only.    │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ QueryPlan (executor/select/analyzed/query_plan.rs) ─────────┐
│  AnalyzedQuery → QueryPlan (all routing decisions in 1 place) │
│  • ExecutionPath: SetOperation / Values / Tableless /         │
│                   SingleTable / Join                          │
│  • WhereStrategy: AllSync / AllAsync / Split / None           │
│  • OrderByStrategy: Inline / Deferred / None                 │
│  • ProjectionStrategy: Sync / NeedsMaterialization           │
│  • DistinctStrategy: None / Distinct / DistinctOn            │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌─ Physical Execution (Volcano iterator model) ────────────────┐
│  Operator tree: each operator has open() → next() → close()  │
│                                                               │
│  Example pipeline for: SELECT a FROM t WHERE b > 5 ORDER BY a│
│                                                               │
│  LimitOperator                                                │
│    └─ SortOperator (ORDER BY a)                               │
│        └─ ProjectOperator (SELECT a)                          │
│            └─ FilterOperator (WHERE b > 5)                    │
│                └─ TableScanOperator (FROM t)                  │
│                    └─ TiKV (scan rows)                        │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
  ExecuteResult::Select { columns, column_types, rows }
    │
    ▼
  pgwire response → Client
```

For CTE pre-materialization, DML path, and EXPLAIN path details, see [architecture/sql-engine.md](architecture/sql-engine.md).

---

## 4. Module Map

| Module | Deep-dive | Contracts (SoT) | Navigation | User Guide |
|--------|-----------|------------------|------------|------------|
| SQL Engine | [architecture/sql-engine.md](architecture/sql-engine.md) | [sot/sql-engine.md](sot/sql-engine.md) | [src/sql/AGENTS.md](../src/sql/AGENTS.md) | [sql-reference.md](sql-reference.md) |
| Protocol | [architecture/protocol.md](architecture/protocol.md) | [sot/protocol-pgwire.md](sot/protocol-pgwire.md) | [src/protocol/AGENTS.md](../src/protocol/AGENTS.md) | [prepared-statement-contract.md](prepared-statement-contract.md) |
| Storage | [architecture/storage.md](architecture/storage.md) | [sot/storage-format.md](sot/storage-format.md) | [src/storage/AGENTS.md](../src/storage/AGENTS.md) | — |
| Transactions | [architecture/transactions.md](architecture/transactions.md) | [sot/sql-engine.md](sot/sql-engine.md) | — | — |
| Multi-Tenancy | [architecture/multi-tenancy.md](architecture/multi-tenancy.md) | [sot/multi-tenancy.md](sot/multi-tenancy.md) | — | [multi-tenancy.md](multi-tenancy.md) |
| Worker/Cron | [architecture/worker.md](architecture/worker.md) | [sot/worker-cron.md](sot/worker-cron.md) | — | [worker.md](worker.md) |
| Auth/RBAC | — | [sot/auth-rbac.md](sot/auth-rbac.md) | — | [authentication.md](authentication.md) |
| Catalog | — | [sot/catalog-introspection.md](sot/catalog-introspection.md) | — | — |
| Extensions | — | [sot/extensions-gin.md](sot/extensions-gin.md) | — | [extensions.md](extensions.md), [fs9_extension.md](fs9_extension.md) |
| Config | — | [sot/ops-config.md](sot/ops-config.md) | — | [configuration.md](configuration.md) |
| Testing | — | [sot/testing-gates.md](sot/testing-gates.md) | — | [testing.md](testing.md) |
| Invariants | — | [sot/invariants.md](sot/invariants.md) | — | — |

---

## 5. Current State & Roadmap

### Completed

| Milestone | Description | Key Files |
|-----------|-------------|-----------|
| Analyzer pipeline | Single-path typed IR for all SELECT queries | `analyzer/` |
| CBO optimizer | LogicalPlan → PhysicalPlan → BoxedOperator pipeline (default ON, multi-table + set ops + CTEs + window + DISTINCT ON) | `optimizer/` |
| ANALYZE + stats | Selectivity estimation + stats cache + warm-up | `optimizer/selectivity/`, `stats.rs` |
| CBO Phase 3 | Join reordering (DPccp), subquery decorrelation (EXISTS→SemiJoin/AntiJoin), predicate pushdown, cross-join elimination, hash join selection | `optimizer/join_reorder/`, `optimizer/rewrite/` |
| Legacy cleanup | Removed 10 legacy code items, single execution path | All |
| Privilege enforcement | SELECT privilege on every base table | `executor/core/statement.rs` |
| 37 catalog views | pg_catalog + information_schema + cron compatibility | `catalog/` |
| Full-text search | FTS functions + GIN tokenization/storage (GIN planner access path currently disabled) + Chinese tokenizer | `gin.rs`, `fts.rs`, `planner/index_selection.rs` |
| Worker engine | Unified async task engine: cron, async triggers, auto-analyze, bg-sql, bg-ddl | `src/worker/` |
| Cron scheduler | pg_cron-compatible cron expressions, job management, virtual tables | `src/cron/`, `catalog/cron_*.rs` |
| Prepared statement unification | Analyzer-backed Describe, TypedExpr visitor, error unification | `protocol/handler/` |
| Protocol hardening | SQLSTATE mapping, memory/stack guards, handler decomposition | `protocol/handler/` |
| FK correctness | ref_columns validation, NULL MATCH SIMPLE, stale snapshots, self-referential | `sql/dml/foreign_keys.rs` |
| GUC + SHOW | SET LOCAL, current_setting(), SHOW ALL, PostgreSQL-compatible GUC defaults | `sql/session/` |
| SQL parity | UNNEST as JOIN, JSONB canonicalization, SQLSTATE 42725 parity | `sql/analyzer/`, `sql/executor/` |

### In Progress

| Phase | Description | Issue |
|-------|-------------|-------|
| Plan cache | Plan cache for prepared statement optimization | #707 |

### Future

| Feature | Description | Issue |
|---------|-------------|-------|
| Parallel execution | Distributed query execution across TiKV regions | #708 |

---

## 6. Invariants

11 system-wide invariants that must always hold. See [docs/sot/invariants.md](sot/invariants.md) for the full normative specification.

---

## 7. Documentation Guide

### Three-Tier Documentation System

| Tier | Location | Content | Update Rule |
|------|----------|---------|-------------|
| **Normative** | `docs/sot/` | Contracts (MUST/SHOULD/MAY), invariants, gates | PR changing behavior MUST update |
| **Descriptive** | `docs/architecture/` + this file | How-it-works explanations, diagrams, module deep-dives | Should be updated when architecture changes; may drift |
| **Navigation** | `src/*/AGENTS.md` | File paths, entry points, function signatures | Updated when file structure changes |

### How to Update

- **Changing behavior/contracts?** → Update `docs/sot/` first, then descriptive docs.
- **Changing architecture?** → Update `docs/architecture/` deep-dive, then this entry point.
- **Changing file structure?** → Update `src/*/AGENTS.md` navigation docs.
- **Adding new documentation?** → Use the decision tree:
  - Binding rule (MUST/MUST NOT)? → `docs/sot/`
  - Explaining how the system works? → `docs/architecture/`
  - How-to for users? → `docs/*.md` (user guide)
  - Design decision/ADR? → `docs/design/`
  - Process/governance? → `docs/governance/`
