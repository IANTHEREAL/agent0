# HNSW Vector Index Design Document

> **Status**: Superseded
>
> This document records the original HNSW design merged in PR `#1241`, but it is **not** the current HNSW architecture.
>
> Current shipped architecture:
> - no process-level HNSW graph cache
> - delta-log storage for DML writes
> - background sweep/merge to consolidate deltas into the base graph
>
> Current references:
> - `docs/sot/sql-engine.md`
> - `docs/ARCHITECTURE.md`
> - `docs/architecture/sql-engine.md`
> - `src/sql/hnsw/mod.rs`
> - `src/sql/hnsw/storage.rs`
> - `src/worker/gc.rs`

**Historical status**: Original implementation design (superseded)
**Author**: AI Assistant
**Date**: 2026-02-28
**PR**: [#1241](https://github.com/c4pt0r/db9-server/pull/1241)
**Issue**: [#1220](https://github.com/c4pt0r/db9-server/issues/1220)

---

## Overview

HNSW (Hierarchical Navigable Small World) vector index support for approximate nearest neighbor (ANN) search. Implements pgvector-compatible syntax: `CREATE INDEX ... USING hnsw`, three distance operator classes, k-NN queries via `ORDER BY <distance_op> LIMIT k`, and runtime tuning via `hnsw.ef_search` GUC.

## Motivation

Without ANN indexes, all k-NN queries require a full table scan with O(N) distance computations. This is acceptable for small datasets (<10K rows) but becomes a bottleneck at scale. HNSW provides sub-linear query time (O(log N) typical) with high recall.

Prior state:
- Vector type, distance operators (`<->`, `<=>`, `<#>`), and distance functions worked correctly
- No index support — all queries were full scans

## Design

### Architecture

```
CREATE INDEX → parse operator class → build usearch graph → serialize to TiKV
                                         |
SELECT ... ORDER BY <-> LIMIT k → detect pattern → load graph from cache/TiKV
                                         |
                                  usearch ANN search → batch_get rows → return
```

Historical note: the diagram above reflects the original design. The current implementation no longer uses the process-level cache described below.

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| HNSW library | `usearch` 0.21 (C++ FFI) | Only viable Rust HNSW crate with incremental insert support |
| Storage model | Whole-graph single KV in TiKV | Historical original design; current implementation moved to delta-log + background merge |
| Serialization | Temp-file bridge | `usearch` only supports file-based save/load; save to temp file -> read bytes -> store in TiKV |
| Graph cache | Process-level LRU with `Arc<RwLock<>>` | Historical original design; current implementation intentionally removed this cache |
| Precision | f64 -> f32 conversion | db9 vectors are `Vec<f64>`, usearch requires `f32`; acceptable precision loss for ANN |
| Pattern detection | `ORDER BY distance_op LIMIT k` | Matches pgvector's query pattern; detected in the physical planner |

### Components

```
src/sql/hnsw/
├── mod.rs          # Constants, process-level LRU cache, get_or_load
└── storage.rs      # TiKV persistence: HnswMeta, save/load graph, temp-file bridge

src/sql/operators/
└── hnsw_scan.rs    # HnswScanOperator (PhysicalOperator impl)

src/sql/planner/
└── hnsw_predicate.rs  # Detects ORDER BY distance() LIMIT k pattern
```

### DDL Syntax

```sql
-- Default params (m=16, ef_construction=64)
CREATE INDEX idx ON items USING hnsw (embedding vector_l2_ops);

-- Custom params
CREATE INDEX idx ON items USING hnsw (embedding vector_cosine_ops)
  WITH (m=32, ef_construction=128);

-- Operator classes → distance operators
--   vector_l2_ops      →  <->  (L2/Euclidean)
--   vector_cosine_ops  →  <=>  (Cosine)
--   vector_ip_ops      →  <#>  (Inner Product)
```

### Query Pattern

The physical planner detects the `ORDER BY <distance_op> LIMIT k` pattern and selects an HNSW scan instead of a table scan + sort:

```sql
-- Automatically uses HNSW index
SELECT * FROM items ORDER BY embedding <-> '[1,2,3]' LIMIT 10;
```

### Runtime Tuning

```sql
SET hnsw.ef_search = 100;  -- default 40, range 1-1000
SHOW hnsw.ef_search;
```

Higher `ef_search` values increase recall at the cost of latency.

### EXPLAIN Output

```
HNSW Scan using idx_hnsw on items
  Distance Metric: l2, k: 10
```

### DML Maintenance

| Operation | Behavior |
|-----------|----------|
| INSERT | New vectors automatically added to the HNSW graph in real-time |
| UPDATE | Old vector removed (lazy), new vector inserted |
| DELETE | Lazy cache invalidation — deleted vectors filtered at query time |
| DROP INDEX | Cache eviction |

### Storage Format

- **Meta key**: `hnsw_meta_{db_id}_{table_id}_{index_id}` -> `HnswMeta` (dimension, metric, m, ef_construction, count)
- **Graph key**: `hnsw_graph_{db_id}_{table_id}_{index_id}` -> serialized usearch index bytes
- Labels in the usearch graph are primary key values (u64), enabling direct row lookup after ANN search.

### Cache Design

Process-level LRU cache keyed by `(db_id, table_id, index_id)`:

1. On first query: load graph from TiKV -> deserialize via temp file -> insert into cache
2. On subsequent queries: return cached graph
3. On DROP INDEX: evict from cache
4. On INSERT: add vector to cached graph + persist updated graph to TiKV

## Modified Modules

| Module | Changes |
|--------|---------|
| `src/model/mod.rs` | `IndexDef` extended with `hnsw_m`, `hnsw_ef_construction`, `hnsw_distance_metric` |
| `src/sql/ddl/create_index.rs` | HNSW DDL: op-class parsing, WITH clause, backfill (~250 lines) |
| `src/sql/optimizer/physical_planner/mod.rs` | HNSW cost-based selection |
| `src/sql/optimizer/build/scan.rs` | HnswScanOperator construction |
| `src/sql/session/settings.rs` | `hnsw.ef_search` GUC parameter |
| `src/sql/dml/insert.rs` / `delete.rs` / `update.rs` | DML maintenance hooks |
| `src/sql/explain/` | HNSW Scan plan node formatting |
| `src/sql/parser/preprocess.rs` | CREATE INDEX WITH clause parsing |
| `src/storage/encoding/serialization.rs` | HNSW meta serialization |
| `src/storage/tikv_store/tables.rs` | HNSW graph persistence operations |

## Testing

- **Unit tests**: `cargo test` — 2755 passed, 0 failed (zero regressions)
- **SQL integration tests** (5 new files):
  - `260_hnsw_basic.sql` — CREATE INDEX, k-NN search, EXPLAIN, DROP
  - `261_hnsw_distance_metrics.sql` — L2, cosine, inner product with separate indexes
  - `262_hnsw_dml.sql` — INSERT/DELETE after index creation, NULL vectors
  - `263_hnsw_params.sql` — WITH clause (m, ef_construction), ef_search GUC
  - `264_hnsw_edge_cases.sql` — Empty table, k > n, error conditions
  - `265_vector_txn_basic.sql` — Transaction RYOW integration

## Not in Scope (V2)

- IVFFlat index
- Parallel/distributed HNSW build
- VACUUM repair of deleted nodes
- `hnsw.iterative_scan`
- Half-precision vectors (halfvec)
- Per-node KV decomposition (>1M vectors)
- REINDEX command

## References

- [pgvector HNSW](https://github.com/pgvector/pgvector#hnsw) — syntax compatibility target
- [usearch crate](https://crates.io/crates/usearch) — Rust bindings for the HNSW implementation
