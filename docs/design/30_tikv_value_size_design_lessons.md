# Design Lessons: TiKV Value Size Constraints

> **Status**: Active

**Date:** 2026-03-22
**Trigger:** #1969 — HNSW graph stored as single KV exceeds TiKV raft-entry-max-size
**Status:** Lessons documented. Remediation in progress.

## Background

The HNSW vector index serializes the entire graph (vectors + topology) as a single KV
value in TiKV. For tables with ~1,300+ rows of VECTOR(1536) embeddings, this blob
exceeds TiKV's `raft-entry-max-size` (default 8 MB). The background merge worker
enters an infinite failure/retry loop, and the oversized write can corrupt TiKV regions.

The initial design doc (27_hnsw_vector_index.md) noted "Per-node KV decomposition" as
a V2 concern for ">1M vectors". The actual threshold is ~1,300 rows for OpenAI-dimension
embeddings — off by roughly 1,000x.

This document captures the systemic lessons and establishes design invariants to prevent
the same class of issue in future features.

## Scope of Impact

An audit of all 104 `txn_put` call sites across 27 files found three risk categories:

| Risk Level | Path | Details |
|------------|------|---------|
| **Critical** | HNSW graph blob | Grows linearly with `rows × dimensions`. Exceeds 8 MB at ~1,300 rows for VECTOR(1536). Both `CREATE INDEX` and background merge are affected. |
| **High** | Table statistics (ANALYZE) | One blob per table. A 100+ column wide table with large-text MCVs can approach 8 MB. |
| **Medium** | Row data (INSERT/UPDATE) | No TOAST equivalent. A single row with a large TEXT/BYTEA column can exceed the limit. |

All other index types (BTree, GIN, expression, partial) are safe — they store one KV per
row with values of at most a few dozen bytes. The HNSW monolithic blob is the only index
storage pattern that grows with table size.

## Lessons

### 1. Quantify storage boundaries in design docs

Any feature that writes blobs to TiKV must include a **Storage Constraints** section that
answers three questions:

1. What is the maximum single-KV value size this feature can produce?
2. At what data scale does it exceed `raft-entry-max-size` (8 MB)?
3. If it can exceed the limit, what is the chunking/sharding strategy?

"Handle at scale later" is not acceptable without a concrete threshold number. Stating
">1M vectors" when the real limit is ~1,300 rows makes the risk invisible during review.

### 2. Guard `txn_put` against oversized values

There is currently no pre-write size check in `txn_put`. When a value exceeds the Raft
entry limit, TiKV returns an opaque `RaftEntryTooLarge` region error that surfaces to the
user as SQLSTATE `XX000` (internal error) with no actionable message.

**Action item:** Add a configurable soft limit check in `txn_put` (e.g., 6 MB) that
returns a clear `SqlError` with the key family and value size when exceeded. This catches
oversized writes at the application layer with a meaningful error instead of letting them
fail deep in the TiKV client.

### 3. Test at realistic data dimensions

All HNSW tests used VECTOR(3) — three-dimensional vectors, 12 bytes each. Real-world
embeddings are VECTOR(768) to VECTOR(3072), 500x–1000x larger per vector. The largest
test (5,000 vectors × 3 dimensions) produced a graph of ~200 KB, well below any limit.

**Invariant:** For any feature where storage size depends on a user-controlled parameter
(vector dimensions, column count, text length), at least one test must exercise a value
close to the storage boundary. Small functional tests prove correctness; boundary tests
prove scalability.

### 4. Evaluate library serialization APIs against storage constraints

usearch 0.21 provides only file-path-based `save(path)` / `load(path)`. There is no
buffer API, no streaming, no per-component export. This forced a temp-file roundtrip
(`save → fs::read → txn_put → txn.get → fs::write → load`) and made chunking impossible
without forking the library.

**Invariant:** When evaluating external libraries for integration with TiKV storage,
verify that the serialization API supports:
- In-memory (buffer/stream) serialization (no mandatory file I/O)
- Bounded output size or incremental output
- Ability to reconstruct from partial data (for chunked storage)

If the library only offers opaque whole-file serialization, document this as a known
limitation and plan for replacement before the feature reaches production scale.

### 5. Document TiKV constraints as first-class invariants

The following TiKV constraints affect db9-server's storage layer but were not previously
documented as design invariants:

| Constraint | Default | Enforced in db9? | Risk |
|------------|---------|------------------|------|
| `raft-entry-max-size` (max single KV value) | 8 MB | No pre-write check | Any unbounded blob can fail |
| `txn-total-size-limit` (max transaction size) | 100 MB | No tracking | Large COPY imports, bulk DML |
| Max key length | 4 KB (TiKV hard limit) | No pre-write check | Composite indexes on long TEXT columns |
| gRPC message size | 64 MB (client-configured) | Partial (scan pagination) | Low risk |

**Action item:** Add these constraints to `docs/sot/storage-format.md` as stable
invariants with enforcement requirements.

### 6. Avoid the "serialize entire structure → single KV" pattern for growable data

The HNSW issue is a specific instance of a general anti-pattern: serializing a data
structure that grows with user data into a single KV value. This pattern is safe for
fixed-size metadata (schemas, configs, definitions) but unsafe for anything that grows
with row count, column count, or user-controlled parameters.

**Safe patterns (already used in db9):**
- Per-row KV entries (BTree/GIN indexes, table rows)
- Fixed-size pages (fs9: 16 KB pages)
- Delta-log with bounded entries (HNSW deltas: one KV per vector mutation)

**Unsafe pattern (the HNSW graph blob):**
- Aggregate entire index/structure into one KV value

When a new feature needs to persist growable data, use one of the safe patterns from
the start.

## Affected Code Paths

| Path | File | Current State | Remediation |
|------|------|---------------|-------------|
| HNSW graph write (merge) | `src/worker/engine.rs:1434` | No size check | Phase 0: frozen guard; Phase 2: paged storage |
| HNSW graph write (CREATE INDEX) | `src/sql/ddl/create_index.rs:588` | No size check | Phase 0: size check + error |
| Table statistics write | `src/storage/tikv_store/statistics.rs:14` | Single blob per table | Future: per-column storage or MCV size cap |
| Row data write | `src/storage/tikv_store/tables.rs:426` | No size check | Future: row size guard |
| `txn_put` (all paths) | `src/txn/mod.rs:43` | No value size validation | Future: soft limit + clear error |

## References

- #1969 — HNSW graph exceeds raft-entry-max-size
- #1968 — txn_put size guard (parent issue)
- `docs/design/27_hnsw_vector_index.md` — original HNSW design doc
- `docs/sot/storage-format.md` — storage format invariants
