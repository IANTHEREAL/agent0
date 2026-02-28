# Module Dependencies

This page documents the dependency relationships between db9-server's major modules. Understanding these dependencies is critical for maintaining clean module boundaries and avoiding circular references.

For the full system overview, see the [System Architecture](./system-architecture.md) and [Architecture Overview](../Architecture-Overview.md).

## Layered Architecture

The system follows a strict layered architecture where higher layers depend on lower layers, but not the reverse.

```mermaid
graph TD
    subgraph L1["Layer 1: Entry Point"]
        MAIN[main.rs<br/>Server Bootstrap]
    end

    subgraph L2["Layer 2: Protocol"]
        PROTO[protocol<br/>pgwire Handler]
        PROTO_ENC[protocol::encode<br/>Value Encoding]
        PROTO_DYN[protocol::dynamic<br/>Query / Copy / Startup]
    end

    subgraph L3["Layer 3: SQL Engine"]
        EXEC[executor<br/>Statement Dispatch]
        ANALYZER[analyzer<br/>Name Resolution + Types]
        OPTIMIZER[optimizer<br/>CBO Pipeline]
        OPERATORS[operators<br/>Physical Operators]
        CATALOG[catalog<br/>Virtual Tables]
        DDL[ddl<br/>Schema Changes]
        DML[dml<br/>Row Mutations]
        EXPR[expr<br/>Expression Evaluation]
        SESSION[session<br/>GUC + Transaction]
    end

    subgraph L4["Layer 4: Support"]
        TRIGGERS[triggers<br/>Event System]
        PLANNER[planner<br/>Index Selection]
        TYPES[types<br/>Type Registry + Cast]
        STATS[stats<br/>Statistics Cache]
        MISC_SUPPORT[sequences / plpgsql]
    end

    subgraph L5["Layer 5: Storage"]
        STORE[storage::tikv_store<br/>KV Operations]
        ENCODING[storage::encoding<br/>Key Format]
    end

    subgraph L6["Layer 6: Infrastructure"]
        MODEL[model<br/>DataType / Value / Row]
        TXN[txn<br/>Transaction + Savepoints]
        INFRA[auth / pool / config / observability]
    end

    subgraph L7["Layer 7: Background + Extensions"]
        WORKER[worker<br/>Task Engine]
        CRON[cron<br/>Scheduler]
        EXT[extensions<br/>HTTP / fs9 / Parquet]
    end

    MAIN --> PROTO & INFRA & WORKER

    PROTO --> PROTO_DYN --> PROTO_ENC
    PROTO_DYN --> EXEC & SESSION

    EXEC --> ANALYZER & OPTIMIZER & DDL & DML & CATALOG
    EXEC --> TRIGGERS & MISC_SUPPORT
    EXEC --> STATS & EXT

    ANALYZER --> TYPES & MODEL & EXPR
    OPTIMIZER --> ANALYZER & PLANNER & STATS
    OPTIMIZER --> OPERATORS
    OPERATORS --> EXPR & STORE & MODEL
    DDL --> STORE & MODEL
    DML --> STORE & TRIGGERS & MISC_SUPPORT

    PLANNER --> STORE & MODEL
    TYPES --> MODEL
    EXPR --> TYPES & MODEL
    CATALOG --> STORE & MODEL

    STORE --> ENCODING & INFRA & TXN
    ENCODING --> MODEL

    WORKER --> STORE
    CRON --> WORKER

    TRIGGERS --> WORKER
    EXT --> STORE
```

## Key Dependency Chains

### Query Execution Chain
The critical path for SELECT queries flows through these modules in order:

```mermaid
graph LR
    P[protocol] --> E[executor]
    E --> A[analyzer]
    A --> T[types]
    E --> O[optimizer]
    O --> LP[logical_planner]
    LP --> RW[rewrite]
    RW --> PP[physical_planner]
    PP --> B[build]
    B --> OP[operators]
    OP --> S[storage]
    S --> TK[TiKV]
```

### Optimizer Internal Dependencies

```mermaid
graph TD
    OPT_MOD[optimizer::mod<br/>optimize fn]
    LOG_PLAN[logical_plan<br/>Plan IR]
    LOG_PLANNER[logical_planner<br/>AnalyzedQuery to LogicalPlan]
    REWRITE[rewrite<br/>Plan Transformations]
    DECORRELATE[rewrite::decorrelate<br/>Subquery to SemiJoin]
    PUSHDOWN[rewrite::predicate_pushdown]
    PHYS_PLAN[physical_plan<br/>Physical IR]
    PHYS_PLANNER[physical_planner<br/>Cost-Based Selection]
    JOIN_REORDER[join_reorder<br/>DPccp Algorithm]
    SELECTIVITY[selectivity<br/>Cardinality Estimation]
    BUILD[build<br/>Plan to Operators]
    STATISTICS[statistics<br/>Column Histograms]

    OPT_MOD --> LOG_PLANNER
    LOG_PLANNER --> LOG_PLAN
    OPT_MOD --> REWRITE
    REWRITE --> PUSHDOWN & DECORRELATE & JOIN_REORDER
    OPT_MOD --> PHYS_PLANNER
    PHYS_PLANNER --> PHYS_PLAN & SELECTIVITY
    SELECTIVITY --> STATISTICS
    BUILD --> PHYS_PLAN
    JOIN_REORDER --> SELECTIVITY
```

### Storage Layer Dependencies

```mermaid
graph TD
    TIKV_STORE[tikv_store<br/>Main Store API]
    TABLES[tikv_store::tables<br/>Row CRUD]
    INDEXES[tikv_store::indexes<br/>Index CRUD]
    SCHEMAS[tikv_store::schemas<br/>Schema Metadata]
    STATS_STORE[tikv_store::statistics<br/>Stats Persistence]
    WORKER_STORE[tikv_store::worker<br/>Task Queue]
    CRON_STORE[tikv_store::cron<br/>Job Metadata]

    DATA_KEYS[encoding::data_keys<br/>Row Key Format]
    META_KEYS[encoding::metadata_keys<br/>Schema Key Format]
    VALUE_ENC[encoding::value_encoding<br/>Row Serialization]

    TIKV_STORE --> TABLES & INDEXES & SCHEMAS & STATS_STORE & WORKER_STORE & CRON_STORE
    TABLES --> DATA_KEYS & VALUE_ENC
    INDEXES --> DATA_KEYS & VALUE_ENC
    SCHEMAS --> META_KEYS
    STATS_STORE --> META_KEYS
```

## Dependency Rules and Invariants

### Rule 1: No Upward Dependencies
Lower layers never depend on higher layers. Storage never imports from SQL. SQL never imports from Protocol. This ensures that each layer can be tested and reasoned about independently.

### Rule 2: Model is the Foundation
The `model` module (`src/model/`) defines the fundamental types (`DataType`, `Value`, `Row`, `TableSchema`, `ColumnDef`) that all other modules depend on. It has no dependencies on any other application module.

### Rule 3: Storage Isolation
The storage layer (`src/storage/`) is the only module that interacts with TiKV directly. All other modules access persistent data through the `TikvStore` API. This enforces keyspace isolation and ensures consistent key encoding.

### Rule 4: Analyzer is Sync, Executor is Async
The Analyzer uses a pre-fetched `CatalogSnapshot` and performs no I/O. All async operations (TiKV reads/writes) happen in the Executor and Operator layers. This separation keeps the type inference logic deterministic and testable.

### Rule 5: Single Optimizer Path
The optimizer module (`src/sql/optimizer/`) is the sole path for SELECT query planning. The `db9.use_optimizer` GUC is a compatibility no-op -- the optimizer is always active. There is no fallback to a legacy planner.

### Rule 6: Worker Independence
The worker engine (`src/worker/`) operates on a separate system keyspace (`_sys_worker`) with its own `TikvStore` instance. It does not share transactions with the main query execution path. Communication between the SQL layer and the worker happens through the TiKV task queue, not through direct function calls.

### Rule 7: Extension Isolation
Extensions (`src/extensions/`) are compiled in but their install state is per-tenant. Extension functions are registered in the `extensions` schema and do not pollute the `public` schema. Extensions access storage through the standard `TikvStore` API.
