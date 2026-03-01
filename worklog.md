# Worklog

## Issue #1284 Step 3 — Control Retry Storm (2026-03-01)

### Goal
Make the autocommit retry budget observable and configurable via GUCs, with structured logging.

### Changes Made
1. **`src/sql/error.rs`** — Added `SqlError::RetryTimeout { elapsed_ms, limit_ms }` variant mapped to SQLSTATE 57014 (same as `StatementTimeout`). Uses `SqlError` instead of standalone `RetryTimeoutError` struct to ensure proper SQLSTATE mapping through `sqlstate_for_executor_error`.

2. **`src/sql/session/settings.rs`** — Added two GUCs:
   - `db9.retry_max_attempts` (u64, default 64): max retry count; validation rejects 0 (must be >= 1)
   - `db9.retry_timeout` (timeout-style, default 0 = disabled): wall-time ceiling using same `parse_timeout_millis` / `format_timeout_show` as `statement_timeout`
   - Wired into `KNOWN_GUCS`, `SessionSettings` struct fields, `new_with_defaults`, `validate_and_normalize_value`, `set_known_setting`, `show_value`, `reset_setting`, `KNOWN_SETTING_KEYS`

3. **`src/sql/executor/core/dispatch/transaction.rs`** — Retry loop changes:
   - `ddl_dml_retry_max_attempts()` now takes `session_max: usize` third parameter, with defensive `.max(1)` clamp
   - `DDL_DML_MAX_RETRY_ATTEMPTS` constant moved to `#[cfg(test)]` (only used in tests now)
   - Two-layer timeout check: (1) at loop top on `attempt > 0` (catches post-backoff overshoot), (2) before `autocommit_backoff` in both autocommit and explicit-txn error paths
   - Structured logging: `tracing::info!` on each retry attempt; `tracing::warn!` only for timeout abort and retry budget exhaustion
   - Conflict-only final warn: non-conflict errors return silently without logging
   - Cleanup on timeout: explicit-txn always does `rollback + clear_trigger_activations`; autocommit relies on rollback already done before the retry check
   - New test: `ddl_dml_retry_budget_respects_session_override`

### Key Design Decisions
- **SqlError::RetryTimeout instead of standalone struct**: Per P1 review feedback — `sqlstate_for_executor_error` only downcasts `SqlError` and TiKV lock conflicts. A standalone struct would fall through to XX000.
- **`else { 1 }` preserved**: Non-autocommit non-first-stmt always executes once (no retry). Changing to 0 would skip execution entirely.
- **Default behavior unchanged**: Default values (64 attempts, 0 timeout) match prior hardcoded constants.

4. **`src/sql/executor/core/dispatch/prepared.rs`** — Same GUC wiring and timeout logic as transaction.rs, replacing hardcoded `max_attempts = 10`.

5. **Observability metrics** (5 new counters in `TenantObservability`):
   - `retry_attempts` — total individual write-conflict retry attempts
   - `retry_budget_exhausted` — statements that hit max retry count
   - `retry_timeout_aborts` — statements aborted by `db9.retry_timeout` wall-time cap
   - `hnsw_graph_bytes_written` — cumulative HNSW graph bytes serialized to TiKV
   - `hnsw_serialize_duration_us` — cumulative HNSW serialization time (microseconds)

   Files changed:
   - `src/observability.rs` — 5 new `AtomicU64` fields, recording methods, snapshot fields
   - `src/sql/catalog/virtual_tables.rs` — 5 new `Int64` columns in `_DB9_SYS_OBSERVABILITY`
   - `src/sql/executor/table_utils/mod.rs` — 5 new values in observability row builder
   - `src/sql/executor/core/dispatch/transaction.rs` — `record_retry_*` calls at all 6 exit sites
   - `src/sql/executor/core/dispatch/prepared.rs` — `record_retry_*` calls at all 4 exit sites
   - `src/sql/dml/update.rs` — `HnswBatchStats` struct, return type changes, timing instrumentation
   - `src/sql/executor/dml_analyzed/insert.rs` — capture `HnswBatchStats`, call `record_hnsw_serialize`
   - `src/sql/executor/dml_analyzed/update.rs` — capture `HnswBatchStats`, call `record_hnsw_serialize`

### Verification
- `cargo build` — clean (0 warnings)
- `cargo clippy -- -D warnings` — clean
- `cargo fmt` — clean
- `cargo test` — 2966/2966 pass (0 failures)

---

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

---

## Round 6 — Issue #1284: HNSW Concurrent UPDATE Stalls Server

### Date: 2026-03-01

### Problem
Under concurrent HNSW UPDATE workload (300 rows, 2 workers, 120 iterations each), server becomes unresponsive with ~200 MiB/s disk read I/O. User confirmed the stalling also occurs without HNSW, but HNSW greatly amplifies it.

### Root Cause Analysis (Revised — TiKV-aware)

**Tier 1: Structural root causes (highest impact)**

1. **Single-key full-graph rewrite (持久化粒度错误)** — HNSW graph is stored as ONE large KV pair (`hnsw_graph_key` at `storage.rs:49`). Every UPDATE rewrites the entire ~100-300 KB value. This creates a single-Region/single-leader hot key in TiKV, triggering Raft replication amplification (not just local RocksDB). Each put goes through Raft log → replicate → apply on all replicas. The observed ~200 MiB/s is consistent with distributed write-hot-spot behavior.

2. **Conflict retry storm** — Default pessimistic txn (`begin()` at `tikv_store/mod.rs:191`). Autocommit DML retries up to 64 times (`DDL_DML_MAX_RETRY_ATTEMPTS` at `dispatch/transaction.rs:163`). When w1 and w2 contend on the same row (id=1) AND the same graph key, each retry re-executes the **entire statement**: full table scan + all row upserts + graph serialize. This is a hidden multiplier on all other costs.
   - Retry condition: `is_retryable_tikv_error` at `retry.rs:3` — retries on ANY WriteConflict reason.
   - Backoff: exponential from 5ms (`autocommit_backoff` at `retry.rs:43`), but still re-does all heavy work.

3. **MVCC version chain accumulation** — Same graph key rewritten at high frequency accumulates many historical versions in TiKV (until GC/compaction). Both reads and writes on this key become progressively heavier as the version chain grows.

**Tier 2: Amplifying factors**

4. **No process-level cache** — Removed in Round 2 for correctness. Every DML loads full graph from TiKV. This is a consistency trade-off, not the root cause, but it amplifies Tier 1 costs.

5. **Full table scan on every UPDATE** — `scan_and_fill()` at `executor/dml_analyzed/update.rs:60` is a paginated RPC scan (`tables.rs:597`, batch 1024 at `tikv_store/mod.rs:48`). Under high-frequency UPDATE, this becomes cross-Region RPC storm. Worse: conflicts are discovered LATE (at write phase), so the expensive scan work is wasted on retry.

6. **Per-row graph serialize via temp file I/O** — 6 disk ops per vector update (save/read/delete × 2). Still expensive, but secondary to the distributed amplification.

7. **Graph bloat from usearch append-only** — Graph grows without bound on repeated updates to same row. Makes each serialization progressively larger.

### Key Insight
The "没有 HNSW 也有问题" observation maps to root causes #2 and #5: conflict retry storm + full table scan are general DML problems. HNSW adds root causes #1 and #3 (single-key hot-spot + version chain), which dramatically amplify the stall.

### Fix Strategy (revised priority)
1. **Fix HNSW persistence granularity** — break single-key monolith into smaller pieces, or use delta-based updates
2. **Reduce retry blast radius** — avoid re-doing full scan + serialize on each retry
3. **Batch HNSW maintenance per-statement** — O(N) → O(1) serializations
4. **In-memory serialization** — eliminate temp file I/O

### Qualifications Added (v3)
1. RC1: "strong inference" — single-key design confirmed in code, but Region hot-spot/Raft amplification needs TiKV metrics to verify.
2. RC2: 64-retry budget only when `is_autocommit || explicit_first_stmt_retry_eligible` (gated by `ddl_dml_retry_max_attempts` at `transaction.rs:165`). Reproduction script uses `psql -c` = autocommit = 64-retry path.
3. RC3: Version chain grows per committed transaction, not per `txn_put` within a single txn.
4. RC5 (full table scan): explicitly marked as general root cause that applies without HNSW.

### RC2 Wording Fix
"re-executes full scan + all row upserts" → "re-executes full scan + all predicate-matching row writes" (w2 only writes 1 row, not all 300).

### Implementation Plan (phased)
1. **Bandaid: batch HNSW maintenance per-statement** — update.rs, insert.rs. O(rows × graph) → O(graph + rows). Low risk.
2. **PK/unique key fast path for UPDATE** — point-get instead of full scan. Medium risk.
3. **Observable + configurable retry budget** — metrics + config. Medium risk.
4. **Structural: change single-key storage granularity** — sharded or delta-log. High risk.

### Step 1 Implementation — Batch HNSW Maintenance (DONE)

**What changed:**

UPDATE path:
- `src/sql/dml/update.rs`: Added `skip_hnsw` parameter to `execute_update_row_inner`. Added `execute_update_row_defer_hnsw` (public wrapper). Added `batch_maintain_hnsw_indexes` — loads graph ONCE, adds all changed vectors, serializes ONCE, writes ONCE.
- `src/sql/dml/update.rs`: Added `batch_maintain_hnsw_indexes_for_inserts` — same pattern for INSERT.
- `src/sql/executor/dml_analyzed/update.rs`: Uses `execute_update_row_defer_hnsw` when HNSW indexes exist, collects `(old_row, new_row)` pairs, calls `batch_maintain_hnsw_indexes` after the loop.

INSERT path:
- `src/sql/dml/insert.rs`: Extracted `execute_insert_row_inner` with `skip_hnsw` flag. Added `execute_insert_row_defer_hnsw` (public wrapper).
- `src/sql/executor/dml_analyzed/insert.rs`: Uses `execute_insert_row_defer_hnsw` when HNSW indexes exist, collects inserted rows, calls `batch_maintain_hnsw_indexes_for_inserts` after the loop.

Module exports:
- `src/sql/dml/mod.rs`: Exported new functions.

**Semantics preserved:**
- All writes still go to the same transaction buffer via `txn_put`
- Transaction visibility unchanged (writes visible within same txn)
- Non-HNSW tables take the unchanged code path (zero overhead)
- FK cascade and ON CONFLICT callers unchanged (per-row HNSW, typically 1 row)

**Complexity reduction:**
- Per-statement graph I/O: O(updated_rows × graph_size) → O(graph_size + updated_rows)
- For w2 in reproduction script (1 row per statement): no change (1 load + 1 serialize)
- For w1 with vector changes (250 rows): 250 load/serialize → 1 load/serialize

**Verification:**
```
cargo build                          — PASS
cargo clippy -- -D warnings          — PASS
cargo test -q                        — PASS (2959 tests, 0 failures)
```

### Step 2 Implementation — PK/Unique-Key Fast Path for UPDATE (DONE)

**What changed:**

Single file: `src/sql/executor/dml_analyzed/update.rs`

Added `try_pk_fast_fetch` method to `Executor` that attempts point-get fetch when WHERE targets a PK or unique index, replacing the full table scan in `scan_and_fill`. Three strict predicate shapes supported:

1. **`pk = const`** — single/composite PK equality via `batch_get_rows`
2. **`pk IN (c1, c2, ...)`** — single-column PK in-list via `batch_get_rows`
3. **AND-connected `col = const` covering all columns of a UNIQUE index** — via `scan_index` → `batch_get_rows`

**Integration point:** Replaced line 60's unconditional `scan_and_fill` with:
```
if !from.is_empty() || where is None → fallback to scan_and_fill
else → try_pk_fast_fetch; if None → fallback to scan_and_fill
```

**Hard constraints enforced (per user requirements):**
1. PK IN (...) deduplicates values before fetch (prevents double-update of same row)
2. NULL constants in predicates disable fast path (col = NULL is UNKNOWN)
3. Unique index fast path only for state=Ready, pure-column, non-partial, non-expression indexes
4. Fast-path rows still pass through the original WHERE eval loop (behavior unchanged by construction)

**Disqualifiers (automatic fallback):**
- FROM clause present
- WHERE is None (update all rows)
- Predicate contains OR, range ops, subqueries, function calls, etc.

**Infrastructure reused:**
- `collect_typed_eq_predicates` from `src/sql/planner/predicate.rs`
- `store.batch_get_rows` from `src/storage/tikv_store/indexes.rs`
- `store.scan_index` from `src/storage/tikv_store/indexes.rs`
- `fill_row_defaults` from `src/sql/projection.rs`

**Behavioral equivalence:** WHERE eval loop still runs on fast-path rows, so any predicate mismatch (e.g. extra non-PK predicates) is caught. No behavioral change by construction.

**Verification:**
```
cargo build                          — PASS
cargo clippy -- -D warnings          — PASS
cargo test -q                        — PASS (2959 tests, 0 failures)
```
