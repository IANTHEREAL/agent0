# Worklog — HNSW Vector Index Fixes (6 Findings + 2 Critical Bugs)

## Date: 2026-02-28

## Summary

Round 1: Implemented 6 targeted fixes for HNSW vector index support from PR #1241 review.
Round 2: Fixed 2 critical bugs found in Round 1 implementation.

---

## Round 1 — Original 6 Findings

### Finding 1 — P0: Multi-tenant cache isolation (SUPERSEDED by Round 2)

**Problem**: `HNSW_CACHE` keyed by `(db_id, table_id, index_id)`. Two tenants with identical tuples share cached graphs.

**Original fix**: Added `store_id: u64` to `TikvStore`. Changed cache key to 4-tuple.
**Superseded**: Round 2 removed the process-level cache entirely, making `store_id` unnecessary.

### Finding 2 — P0: HNSW visibility not transaction-safe (SUPERSEDED by Round 2)

**Problem**: INSERT mutated shared in-memory graph before txn commits. Rollback corrupts cache.

**Original fix**: Load graph from TiKV, write to txn buffer, invalidate cache.
**Superseded**: Round 2 removed cache; all reads load directly from TiKV.

### Finding 3 — P1: UPDATE does not maintain HNSW indexes (SUPERSEDED by Round 2)

**Problem**: `is_index_materializable` returns false for HNSW, so UPDATE silently skips them.

**Original fix**: Added `invalidate_hnsw_cache` calls in both update loops.
**Superseded**: Round 2 replaced with `maintain_hnsw_indexes_after_update` that actually writes graph.

### Finding 4 — P1: PK-label contract internally inconsistent (DONE)

**Problem**: `hnsw_pk_label` in create_index.rs uses hash fallback for non-integer PKs.

**Fix**: Added PK validation at CREATE INDEX time. Replaced hash fallback with error.

### Finding 5 — P2: HNSW tuning knobs are no-ops (PARTIAL)

**Fix (ef_search)**: Wired up as minimum search beam width.
**Fix (WITH clause)**: NOT IMPLEMENTED — sqlparser 0.40 limitation.

### Finding 6 — P2: DROP INDEX doesn't remove persisted HNSW keys (DONE)

**Fix**: Added `txn_delete` calls for both graph and meta keys.

---

## Round 2 — Critical Bug Fixes

### Bug 1 — P0: Cache can become permanently stale after concurrent INSERT commit (DONE)

**Root cause**: INSERT invalidates cache BEFORE txn commits. Concurrent reader repopulates cache with stale TiKV data between invalidation and commit. After commit, stale cache persists permanently.

**Fix**: Removed the process-level HNSW cache entirely. Each operation loads the graph directly from TiKV via `load_hnsw_graph` (which opens its own read txn → sees committed state). This eliminates the entire class of cache-coherence bugs.

**Changes**:
- `src/sql/hnsw/mod.rs` — Removed `HNSW_CACHE`, `HnswCacheKey`, `HnswCacheValue`, `get_or_load_hnsw`, `invalidate_hnsw_cache`. Added consolidated `hnsw_pk_label` (single source of truth for PK→label).
- `src/sql/operators/hnsw_scan.rs` — Replaced `get_or_load_hnsw` with `load_hnsw_graph` directly. Updated `search_ranked_labels` to take `&HnswIndexHandle` instead of `&Arc<RwLock<...>>`.
- `src/sql/dml/insert.rs` — Removed `invalidate_hnsw_cache` call. Replaced local `hnsw_pk_label_from_values` with `hnsw::hnsw_pk_label`.
- `src/sql/dml/delete.rs` — Removed `invalidate_hnsw_cache` import and call. HNSW indexes just `continue` (lazy deletion via over-fetch).
- `src/sql/dml/update.rs` — Removed `invalidate_hnsw_cache` import and all 3 calls.
- `src/sql/ddl/drop.rs` — Removed `invalidate_hnsw_cache` import and call (kept `txn_delete` for keys).
- `src/sql/ddl/create_index.rs` — Replaced local `hnsw_pk_label` with `hnsw::hnsw_pk_label`.
- `src/storage/tikv_store/mod.rs` — Removed `store_id` field, `NEXT_STORE_ID` static, `store_id()` getter, `AtomicU64` import.

### Bug 2 — P1: UPDATE does not maintain HNSW graph contents (DONE)

**Root cause**: Round 1 UPDATE fix only called `invalidate_hnsw_cache` — no new graph snapshot was written to TiKV. After reload, the graph still had old vectors. Vector updates ranked by stale vectors; PK-changing updates made rows unreachable.

**Fix**: Added `maintain_hnsw_indexes_after_update` function that mirrors INSERT's approach: loads graph from TiKV, calls `add(new_pk_label, new_vector_f32)`, serializes, writes to txn buffer via `txn_put`. Called in both `update_row_indexes` and `execute_update_row_inner`.

**usearch 0.21 limitation**: No `remove` method. For same-PK updates, `add` overwrites (same label). For PK-changing updates, old label stays as stale entry — handled by over-fetch + `batch_get_rows` visibility filtering.

**Changes**:
- `src/sql/dml/update.rs` — Added `maintain_hnsw_indexes_after_update` (~60 lines), called in both update paths. Added imports for `hnsw_pk_label`, `load_hnsw_graph`, `serialize_hnsw_snapshot`, `hnsw_graph_key`, `hnsw_meta_key`, `vec_f64_to_f32`, `txn_put`.

---

## Round 3 — Review Findings (P1 + P2)

### Finding 1 — P1: UPDATE SET vec = NULL leaves stale HNSW entries (DONE)

**Root cause**: `maintain_hnsw_indexes_after_update` hits `Some(Value::Null) => continue`, skipping the graph write. Old PK label stays in graph. At scan time, `batch_get_rows` returns the row (still exists, just vector is NULL), and no NULL-vector filter → stale result returned.

**Why not fix in update.rs**: usearch 0.21 has no `remove` method. Cannot remove a label from the graph at UPDATE time.

**Fix**: In `hnsw_scan.rs`, after `batch_get_rows`, resolve the indexed vector column from the HNSW index definition and filter out rows where that column is NULL before sorting/truncating.

**Changes**: `src/sql/operators/hnsw_scan.rs` — look up vector column from index def, filter `Value::Null` rows in the `filter_map` after `batch_get_rows`.

### Finding 2 — P2: HNSW metadata count drifts on same-PK updates (DONE)

**Root cause**: `meta.count = meta.count.saturating_add(1)` always increments, but same-label `add()` is an overwrite — index size doesn't grow. Count inflates, causing premature capacity growth at the 80% threshold.

**Fix**: Replace `meta.count.saturating_add(1)` with `hnsw_index.size() as u64` after `add()`. Applied to both `update.rs` and `insert.rs` (insert has same pattern; re-inserted PK after DELETE leaves stale label → overwrite, not new entry).

**Changes**:
- `src/sql/dml/update.rs:491` — `meta.count = hnsw_index.size() as u64`
- `src/sql/dml/insert.rs:177` — `meta.count = hnsw_index.size() as u64`

---

## Verification (Round 3)

```
cargo build                          — PASS (no errors, no warnings)
cargo clippy -- -D warnings          — PASS (no warnings)
cargo test -q                        — PASS (2755 tests, 0 failures)
```

---

## Round 4 — Retry Loop + Error Propagation

### Finding 1 — P1: HNSW can produce false negatives under churn (DONE)

**Root cause**: `fetch_k` is bounded but stale entries are unbounded. If many stale entries exist, all `fetch_k` results can be stale, yielding `< k` valid rows.

**Fix**: Retry loop in `hnsw_scan.rs`. After visibility filtering, if `rows.len() < k` and `fetch_k < graph_size`, doubles `fetch_k` (capped at graph size) and retries. Terminates when enough valid rows found or graph exhausted.

### Finding 2 — P2: `fill_row_defaults(...).ok()?` silently drops errors (DONE)

**Root cause**: `filter_map` with `.ok()?` converted `fill_row_defaults` errors to silent row drops.

**Fix**: Replaced `filter_map` with explicit `for` loop; `fill_row_defaults` errors propagated via `?`.

---

## Round 5 — Transaction-Accumulative Graph Updates + Partial Index Rejection

### Finding 1 — P0: HNSW graph updates not transaction-accumulative across rows (DONE)

**Root cause**: `load_hnsw_graph` opens a fresh `store.begin()` read txn (committed state only). In a multi-row INSERT/UPDATE, row N+1's maintenance reloads graph WITHOUT row N's vector addition — `txn_put` writes to the DML txn buffer, but `store.begin()` creates an independent snapshot that can't see it. Row N+1's graph write overwrites row N's, losing row N's vector.

**Fix**: Added `load_hnsw_graph_from_txn(txn, ...)` that reads via the DML transaction. `txn.get()` checks the local write buffer before hitting TiKV (confirmed in `vendor/tikv-client/src/transaction/transaction.rs:143` — `self.buffer.get_or_else`), so row N+1 sees row N's graph write.

**Changes**:
- `src/sql/hnsw/storage.rs` — Added `load_hnsw_graph_from_txn` (accepts `&mut Transaction`). Refactored `load_hnsw_graph` to delegate to it.
- `src/sql/dml/insert.rs` — Changed `maintain_hnsw_indexes_after_insert` to use `load_hnsw_graph_from_txn(txn, ...)`. Removed unused `store` parameter.
- `src/sql/dml/update.rs` — Changed `maintain_hnsw_indexes_after_update` to use `load_hnsw_graph_from_txn(txn, ...)`. Removed unused `store` parameter.
- `src/sql/hnsw/mod.rs` — Kept `load_hnsw_graph` re-export (used by read-only HnswScanOperator).

**Not affected**: CREATE INDEX (builds graph from scratch), HnswScanOperator (read-only, standalone txn correct), DELETE (lazy deletion).

### Finding 2 — P1: Partial HNSW index accepted but never enforced (DONE)

**Root cause**: `WHERE` predicate accepted at DDL time but ignored at all 5 downstream points: backfill, INSERT, UPDATE, planner, scan. pgvector doesn't support partial HNSW indexes either.

**Fix**: Reject at DDL time. Added guard in `execute_create_index`:
```rust
if is_hnsw && predicate.is_some() {
    return Err(anyhow!("HNSW indexes do not support partial index predicates (WHERE clause)"));
}
```

**Changes**: `src/sql/ddl/create_index.rs` — Added 5-line guard after CONCURRENTLY check (line 92).

## Verification (Round 5)

```
cargo build                          — PASS (no errors, no warnings)
cargo clippy -- -D warnings          — PASS (no warnings)
cargo test -q                        — PASS (2822 tests, 0 failures)
```
