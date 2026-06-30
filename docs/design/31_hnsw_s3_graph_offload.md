# HNSW Graph S3 Offload

> **Status**: Draft

**Issue:** #1971
**Depends on:** #1970 (frozen guard, merged)

## Problem

The HNSW graph is serialized as a single KV value in TiKV via usearch `save()`.
At ~1,300 rows for VECTOR(1536), the blob exceeds TiKV's `raft-entry-max-size`
(8 MB). PR #1970 added a frozen guard that stops merging when the graph exceeds
the limit, but frozen indexes degrade over time as deltas accumulate.

## Design Decision

Store the HNSW graph blob in S3 instead of TiKV. Keep usearch as-is.

**Why S3 over TiKV paging:**
- usearch `save(path)` already writes to a temp file — uploading to S3 is a
  natural extension of the existing flow, not a new abstraction.
- No single-value size limit at the storage layer. S3 object size is not the
  binding runtime constraint for this MVP; see **Operating Envelope** below.
- No transaction size limit concern (TiKV's `txn-total-size-limit` = 100 MB
  would cap paged graphs at ~100 MB without multi-transaction complexity).
- db9 already ships `aws-sdk-s3 = "1.110.0"` and a production S3 client
  (`src/extensions/fs/s3.rs`) with put/get/delete, MinIO support, and
  presigned URLs.

**Why keep usearch:**
- The usearch FFI surface is narrow (7 methods) and well-isolated behind
  `HnswIndexHandle`.
- Replacing the HNSW engine is a separate, larger effort with recall
  regression risk.
- The S3 offload requires zero changes to usearch or the FFI layer.

## Operating Envelope

This design removes the TiKV 8 MB storage ceiling. It does **not** remove the
whole-graph materialization model.

- Write path still does `index.save(...)` followed by `fs::read(...)`, so the
  full serialized graph is materialized in memory before the S3 PUT.
- Read path still does `get_graph(...) -> Bytes`, then `index.load(...)` of the
  whole graph. The cache removes repeated S3 GETs, but not per-query whole-index
  load or memory pressure.
- The practical runtime envelope is now bounded by process memory, local cache
  disk, and acceptable S3 transfer + `index.load(...)` latency, not by the
  S3 object size limit.

For example, the doc already models a 60 MB graph and notes that 10 concurrent
queries imply roughly 660 MB per-index memory pressure (cache bytes plus one
`Index` per query). That is a major improvement over the old ~1,300-row freeze
point, but it is not a streaming or partitioned ANN architecture.

This MVP should therefore be described as "removes the TiKV blob limit" rather
than "object-store-scaled" in the general sense. Truly huge indexes remain a
separate segmented/chunked design effort.

## Current Architecture

```
WRITE (merge / CREATE INDEX):
  usearch index.save(temp_file)
  graph_bytes = fs::read(temp_file)        ← entire graph in memory
  txn_put(hnsw_graph_key, graph_bytes)     ← single KV, fails > 8 MB
  txn_put(hnsw_meta_key, meta_bytes)

READ (query scan / merge load):
  graph_bytes = txn.get(hnsw_graph_key)    ← single KV from TiKV
  fs::write(temp_file, graph_bytes)
  usearch index.load(temp_file)
```

Key observation: the temp file roundtrip already exists on both sides. The
change is replacing the TiKV leg with an S3 leg.

## Proposed Architecture

```
WRITE (merge / CREATE INDEX):
  usearch index.save(temp_file)            ← UNCHANGED
  if s3_enabled:
    graph_bytes = fs::read(temp_file)
    s3.put_graph(s3_key, graph_bytes)      ← NEW: replaces txn_put
    txn_put(hnsw_meta_key, meta_bytes)     ← meta stays in TiKV
  else:
    graph_bytes = fs::read(temp_file)
    txn_put(hnsw_graph_key, graph_bytes)   ← UNCHANGED (8 MB frozen guard)
    txn_put(hnsw_meta_key, meta_bytes)

READ (query scan / merge load):
  meta = txn.get(hnsw_meta_key)            ← UNCHANGED
  if meta.graph_version > 0:
    if !s3_enabled:
      ERROR "HNSW index requires S3 (graph_version=N). Set HNSW_S3_BUCKET."
    graph_bytes = s3.get_graph(s3_key)     ← NEW: replaces txn.get
  else:
    graph_bytes = txn.get(hnsw_graph_key)  ← UNCHANGED (also migration path)
  fs::write(temp_file, graph_bytes)        ← UNCHANGED
  usearch index.load(temp_file)            ← UNCHANGED

DROP INDEX / DROP TABLE:
  # Inside transaction (pre-commit):
  txn_delete(hnsw_graph_key)               ← KEEP (idempotent cleanup)
  meta.dropped_at = now_unix_seconds()
  txn_put(hnsw_meta_key, meta)             ← TOMBSTONE (not delete!)
  delete_all_deltas(...)                   ← UNCHANGED
  # Meta is kept as a tombstone with dropped_at timestamp.
  # S3 objects are NOT deleted here or post-commit.
  # GC deletes S3 objects when now - dropped_at > gc_life_time_sec,
  # then deletes the tombstone meta itself.
  # Using dropped_at (not S3 LastModified) as the reference time is
  # critical: LastModified reflects upload time (could be weeks old),
  # while dropped_at reflects the actual DDL time.

TRUNCATE (index stays alive, just emptied):
  txn_delete(hnsw_graph_key)               ← KEEP
  fresh_meta = copy index params from old meta
  fresh_meta.count = 0
  fresh_meta.graph_version = 0             ← reset, NOT tombstone
  fresh_meta.dropped_at = None
  txn_put(hnsw_meta_key, fresh_meta)       ← fresh empty meta
  delete_all_deltas(...)                   ← UNCHANGED
  # Old S3 versions become orphans (graph_version reset to 0).
  # GC cleans them after the MVCC retention window as reset/orphaned versions.
  # Next INSERT writes deltas normally; next merge writes to S3.
```

## S3 Key Format

```
{prefix}/{hex(keyspace)}/{db_id}/{table_id}/{index_id}/graph_v{version}.usearch
```

- `prefix`: configurable via `HNSW_S3_PREFIX`, default `"hnsw"`.
- `hex(keyspace)`: the TiKV keyspace string, **hex-encoded** (following the
  fs9 precedent in `encode_s3_key_component`). Required because:
  - TiKV isolates keyspaces at the client level; S3 has no equivalent.
  - `tenant_id` comes from user input with no character validation — could
    contain `/` or `..` which would break the S3 key hierarchy.
  - Hex encoding guarantees injectivity and eliminates collision risk.
  - Single-tenant deployments use keyspace `"DEFAULT"` (pool always maps
    None → Some("DEFAULT")).
- `version`: monotonic u64 counter stored in `HnswMeta.graph_version`. Starts
  at 0 (default), first S3 write uses version 1. This ensures:
  - Concurrent readers see a consistent graph via MVCC snapshot + versioned
    S3 key (reader loads meta version=N → fetches `graph_vN`).
  - Crash safety: if S3 PUT succeeds but TiKV meta commit fails, the orphaned
    new-version object is never referenced and gets cleaned up by GC.
- Deterministically derived from `(keyspace, db_id, table_id, index_id,
  version)` — no need to store the S3 key itself in `HnswMeta`.

### Storage version

When a graph is first written to S3, `meta.storage_version` is bumped from
`1` to `2`. This ensures that old code (without S3 support) **rejects** the
index with a clear error instead of silently reading stale TiKV data:

```
"HNSW index has unsupported storage_version=2; only v1 (delta-log) is supported."
```

The existing version check at `engine.rs:1391` and `storage.rs:484` already
enforces this. `graph_version` (monotonic counter for S3 object naming)
remains a separate field from `storage_version` (schema format version).

### Version lifecycle

```
merge starts:
  old_version = meta.graph_version        (e.g. 3)
  new_version = old_version + 1           (e.g. 4)
  s3.put_graph(.../graph_v4.usearch)      write new version
  meta.graph_version = 4
  txn_put(meta_key, meta)                 commit atomically with delta deletes
  # old version is NOT deleted here — GC handles all cleanup
```

Old versions are cleaned up exclusively by the S3 GC sweep (see below).
This avoids a race where a reader loads meta (version=3) from TiKV via MVCC
snapshot, but the merge deletes `graph_v3` from S3 before the reader can
fetch it.

## Configuration

New environment variables (independent from fs9):

```
HNSW_S3_BUCKET              # required to enable S3 offload
HNSW_S3_REGION              # optional, falls back to AWS_REGION
HNSW_S3_ENDPOINT            # optional, for MinIO / S3-compatible
HNSW_S3_PREFIX              # default: "hnsw"
HNSW_S3_FORCE_PATH_STYLE    # default: false
HNSW_CACHE_MAX_ENTRIES      # default: 64 (graph byte cache LRU size)
HNSW_CACHE_DIR              # default: std::env::temp_dir() (/tmp)
```

When `HNSW_S3_BUCKET` is unset, the system behaves exactly as today (TiKV-only,
with 8 MB frozen guard from #1970).

## S3 Client Timeout

The `HnswS3Client` must configure explicit operation timeouts. The AWS SDK
`BehaviorVersion::latest()` sets a connect timeout (~3.1s) but has **no**
overall operation timeout. Without one, a slow S3 response could block a
user query for 30-60 seconds (3 retries × exponential backoff), exceeding
`statement_timeout`.

```rust
let timeout_config = TimeoutConfig::builder()
    .operation_timeout(Duration::from_secs(30))
    .operation_attempt_timeout(Duration::from_secs(10))
    .build();
```

For the query path, also wrap the S3 GET with the remaining
`statement_timeout` budget via `tokio::time::timeout`.

## Consistency

**S3 PUT succeeds, TiKV meta commit fails:**
- Orphaned S3 object at version N+1, never referenced by any meta.
- Next successful merge writes version N+1 again (same key, overwrite),
  commits meta, self-heals. GC cleans up any remaining orphans.

**Concurrent read during merge:**
- Reader loads meta from TiKV via MVCC snapshot → gets `graph_version = N`.
- Reader fetches `graph_vN.usearch` from S3 → gets the correct graph.
- Merge writes `graph_v(N+1).usearch` to S3, then commits meta with
  `graph_version = N+1` to TiKV.
- Old version `graph_vN` is **not deleted** by the merge — it remains
  available for any in-flight readers. GC deletes it later.
- No conflict: versioned keys + deferred deletion guarantee readers
  never encounter `NoSuchKey` for a version they legitimately need.

**Concurrent merge protection:**
- The worker task claiming mechanism uses pessimistic locking
  (`try_claim_worker_task` with deterministic claim key). The claim key
  includes `fire_time_min = now() / 60`, so a merge lasting >1 minute
  theoretically allows a second worker to claim a new slot. However,
  this is safe because:
  - Both merges read the same meta (`graph_version = N`) via `start_ts`
    snapshot and process overlapping delta sets. The later snapshot may
    include additional deltas committed between the two `start_ts`
    values, producing a **superset** graph.
  - Both write `graph_v(N+1)` to S3 — S3 is last-writer-wins.
  - Pessimistic lock conflict detection on `meta_key` at `put()` time
    (not commit time — `WakeUpModeNormal` returns `WriteConflict`)
    ensures that when lock acquisition overlaps, only the winner
    commits. When it doesn't overlap (A fully commits before B starts
    its puts), both may commit, but the later graph is always a
    superset — safe.
  - **Known minor behavior:** If the loser's S3 PUT lands after the
    winner's, the S3 object may contain a superset graph with an
    extra delta dN+1 that is also still in TiKV (loser's txn rolled
    back, delta not deleted). The next merge re-adds dN+1 via
    `index.add()`, creating one duplicate entry per conflict. This is
    benign: `search_ranked_labels` (`hnsw_scan.rs:102-115`) deduplicates
    by label, so query results are correct. The frequency is very low
    (requires merge > 1 min for concurrent claim window to open).
- **`get_for_update` is NOT used.** While it would serialize concurrent
  merges, it reads at `for_update_ts` (not `start_ts`), creating a
  timestamp skew: meta would show version N+1 (from the concurrent
  merge) while the delta scan sees `start_ts` data, causing duplicate
  delta application and graph inflation. Plain `txn.get` (snapshot read)
  avoids this by keeping meta and deltas at the same MVCC snapshot.

**Concurrent read during DROP INDEX / DROP TABLE / TRUNCATE:**
- Reader at `start_ts = T0` opens a TiKV transaction.
- DDL drops the index: sets `meta.dropped_at = now()` (tombstone, not
  delete), commits at `T1 > T0`.
- Reader's `txn.get(meta_key)` at `start_ts = T0` still sees the
  pre-tombstone meta (MVCC snapshot before T1). Gets `graph_version = N`.
- Reader does S3 GET for `graph_vN.usearch`.
- **Critical:** The S3 object must still exist. GC only deletes S3
  objects when `now - meta.dropped_at > gc_life_time_sec`. Since
  `dropped_at` is set at DDL commit time (T1), and
  `gc_life_time_sec` (default 24h) bounds the maximum MVCC snapshot age,
  the S3 object is guaranteed to exist for any reader whose snapshot
  predates the DDL.
- **Why `dropped_at`, not `LastModified`:** S3 `LastModified` reflects
  when the object was uploaded (last merge), not when the index was
  dropped. A quiet index merged 30 days ago would have `LastModified`
  30 days old — GC using `LastModified` would delete it immediately
  after DROP, breaking any concurrent reader. `dropped_at` is the
  correct reference point.

**S3 unavailable:**
- If S3 is configured and unreachable, `load_base_graph` returns an error,
  which surfaces as a query error. This is correct — if the storage backend
  is down, queries should fail explicitly. No fallback to TiKV (stale data).

## S3 GC Sweep

Old S3 graph versions are cleaned up by a periodic GC sweep, not by inline
deletion after merge. This is **mandatory** — without it, leaked objects
accumulate unboundedly (~144 GB/month at scale).

**Implementation:** New method `sweep_hnsw_s3_orphans_for_entry()` in `src/worker/gc.rs`,
running alongside the existing `run_hnsw_sweep_loop()` on a configurable
interval (default: 600s).

### Retention policy

**Safepoint marker + lineage guard.** GC never deletes committed S3 graph
objects based on `LastModified` or wall-clock age. When a version or prefix
becomes reclaimable, the worker writes a durable TiKV marker sealed with the
current PD timestamp (`delete_after_safepoint`). The S3 delete happens only
after TiKV's GC safepoint has advanced past that sealed timestamp.

This ties external-object cleanup to the same MVCC boundary as TiKV versions:
if TiKV has not reclaimed old versions yet, a reader may still see metadata
that references the S3 object. TiKV safepoint advancement is governed by
`gc_life_time_sec` (default: 86400s), but the S3 sweep consumes the safepoint
itself instead of reimplementing the retention window with object timestamps.

The lineage guard is:

1. `version == meta.graph_version`: current live graph; keep it and clear any
   stale retired-version marker.
2. `version < meta.graph_version`: historical retired graph; write/use a
   per-version retired marker and delete after the safepoint crosses it.
3. `version > meta.graph_version`: future/speculative upload; leave it alone
   and clear any stale retired-version marker.
4. `meta.dropped_at IS NOT NULL` or `meta.graph_version == 0`: write/use a
   prefix marker and delete the whole index prefix after the safepoint crosses
   it. Dropped metadata is removed only after the prefix delete succeeds.
5. No metadata and no durable prefix marker: leave objects untouched. They may
   belong to an uncommitted explicit transaction; durable upload intents own
   cleanup for abandoned writer-owned uploads once the source transaction is
   below the TiKV GC safepoint.

### Batched LIST for cost efficiency

Per-index LIST is too expensive at scale (20K indexes × $0.005/1K requests
× 4320 cycles/month = ~$432/month). Instead, batch at the database level:

```
for each (keyspace, db_id) in worker_registry:
  continuation = None
  metas = {}
  prefix_deleted = {}

  loop:
    # One paged LIST stream per database, not per index.
    page = s3.list_objects_page(prefix="{prefix}/{hex(ks)}/{db}/", continuation)
    index_objects = group_by(page.objects, extract_table_index_from_key,
                             skip_prefixes=prefix_deleted)

    # Point-read only metadata for indexes seen on this page.
    missing = index_objects.keys - metas.keys
    metas += read_hnsw_metas_for_indexes(db_id, missing)

    for each (table_id, index_id), objects in index_objects:
      meta = metas.get((table_id, index_id))
      prefix_marker = read_hnsw_s3_prefix_gc_marker(table_id, index_id)

      if prefix_marker exists:
        if meta is live and meta.graph_version > 0:
          delete stale prefix_marker
        elif gc_safepoint >= prefix_marker.delete_after_safepoint:
          s3.delete_prefix(table_id, index_id)
          delete prefix_marker and retired-version markers
          if meta is dropped: delete hnsw_meta tombstone
          prefix_deleted.add((table_id, index_id))
          continue

      if meta is None:
        # Could be an uncommitted explicit transaction; upload-intent GC owns
        # abandoned writer-owned uploads.
        continue
      if meta.dropped_at is not None or meta.graph_version == 0:
        write prefix_marker(delete_after_safepoint = current_pd_tso)
        continue

      for each object in objects:
        v = parse version from key
        if v == meta.graph_version:
          delete stale retired-version marker if present
        elif v > meta.graph_version:
          # Future/speculative upload; never infer retirement from LIST alone.
          delete stale retired-version marker if present
        else:
          marker = read_hnsw_s3_retired_version_marker(v)
          if marker is missing:
            write retired-version marker(delete_after_safepoint = current_pd_tso)
          elif gc_safepoint >= marker.delete_after_safepoint:
            s3.delete_graph(v)
            delete retired-version marker

    continuation = page.next_continuation_token
    if continuation is None: break
```

Base cost at database-level batching when each database fits one LIST page:
1K databases × 4320 cycles/month × $0.005/1K = ~$22/month. Large database
prefixes scale by page count, still avoiding per-index LIST fanout.

## Interaction with Frozen Guard (#1970)

When S3 is enabled:
- `HNSW_GRAPH_MAX_BYTES` (8 MB) limit is **not applied** — S3 has no size
  constraint.
- `check_graph_oversize_freeze()` is skipped in the merge path.
- `should_skip_frozen_merge()` returns `false` — previously frozen indexes
  are picked up by merge, written to S3, and automatically unfrozen. This
  is critical: frozen indexes are exactly the ones that need S3 most, and
  without this change they would never migrate.
- CREATE INDEX does not reject large initial graphs.
- The `frozen` field in `HnswMeta` remains for the TiKV-only path.

When S3 is not enabled:
- Everything works exactly as #1970 shipped. No behavior change.

## Migration (TiKV → S3)

When S3 is newly enabled on an existing deployment:
1. `load_base_graph()` checks `meta.graph_version`.
2. If `graph_version == 0` (pre-migration): skip S3, read from TiKV directly.
3. If `graph_version > 0`: fetch from S3.
4. The next merge cycle writes the graph to S3 with `graph_version = 1`
   and `storage_version = 2`, completing migration for that index.
5. Previously frozen indexes are automatically picked up by merge when S3
   is enabled (`should_skip_frozen_merge` returns false). The oversized
   graph goes to S3, `frozen` is cleared.
6. After all indexes have merged at least once, old TiKV graph blobs can be
   cleaned up (optional, they just waste space).

The `graph_version == 0` check is the migration boundary — no S3 GET is
attempted for legacy indexes, avoiding unnecessary `NoSuchKey` errors.

### Rollback (S3 → TiKV)

If the operator unsets `HNSW_S3_BUCKET`:
- Indexes with `graph_version == 0` continue working on TiKV (never migrated).
- Indexes with `graph_version > 0` **cannot** read from TiKV (the graph blob
  was not written there after S3 migration). The read path returns an
  explicit error:

  ```
  "HNSW index requires S3 storage (storage_version=2). Set HNSW_S3_BUCKET
   to restore access, or DROP and recreate the index."
  ```

  This is enforced by the `storage_version = 2` check — old code and
  S3-disabled code both reject these indexes cleanly instead of silently
  serving stale data.

- To fully roll back: `DROP INDEX` + `CREATE INDEX` on TiKV-only
  (graph must fit within 8 MB, or it freezes).

## Latency Impact

| Operation | TiKV (current) | S3 (AWS, same region) | MinIO (local) |
|-----------|---------------|----------------------|---------------|
| Read 10 MB graph (S3 transfer + temp file + usearch load) | ~15-35 ms | ~56-98 ms | ~11-28 ms |
| Read 60 MB graph (S3 transfer + temp file + usearch load) | N/A (frozen) | ~240-460 ms | ~60-90 ms |
| Write 10 MB graph | ~10 ms | ~60-120 ms | ~10 ms |

For MinIO / S3-compatible local storage, latency is comparable to TiKV. For
AWS S3, the added latency is 50-200 ms per graph load. The latency numbers
above include S3 transfer, `fs::write` to temp file, and `usearch index.load`.

**Delta apply latency (pre-existing, amplified by S3):** When an index has
accumulated deltas (common for frozen indexes), each query also pays the
cost of applying all pending deltas in memory via `usearch index.add()`.
At ~300 μs per delta for VECTOR(1536), 10K deltas add ~3 seconds and 50K
deltas add ~15 seconds to query latency. This cost exists today on TiKV
but becomes the dominant latency concern for S3-enabled frozen indexes
during migration, until the first merge completes.

## Process-Level Graph Cache (P0)

Without a cache, **every** HNSW query downloads the full graph from S3.
At 10 QPS on a 10 MB graph, this is 100 MB/s of S3 egress — latency
prohibitive (p99 ~1s) and cost prohibitive for high-QPS workloads.

The cache is enabled by the versioned S3 key design: `meta.graph_version`
(read from TiKV via MVCC snapshot, ~1-3 ms) serves as a cheap freshness
check. On cache hit (same version), zero S3 calls. On cache miss (version
changed after merge), one S3 GET to reload.

**Implementation:** Process-level LRU cache keyed by
`(keyspace, db_id, table_id, index_id)`, storing
`(graph_version, PathBuf)` — a **persistent file path** to the cached
graph bytes, not a loaded `HnswIndexHandle` or in-memory bytes.
The keyspace component is required because ID sequences are
per-keyspace (each tenant's `_sys_next_database_id` starts at 1),
so two tenants can have identical `(db_id, table_id, index_id)` tuples.

**Why bytes, not Index:** usearch `Index` is not thread-safe — both `add()`
and `search()` use `contexts_[0]` (shared mutable scratch buffers, per
`index.hpp:2012,2093`). Sharing a loaded Index across concurrent queries
causes data races. `HnswIndexHandle` is also `!Clone`. Caching the raw
bytes as `Arc<Vec<u8>>` is safe (immutable, `Send+Sync`), and each query
creates its own private `Index` via `index.load()` from the cached bytes
(~5-15 ms for 10 MB, still saves the 200-400 ms S3 GET).

In `load_base_graph()`:
1. Read `meta.graph_version` from TiKV (cheap, ~1 ms).
2. Check cache: if version matches, use cached persistent file path.
3. Cache miss: S3 GET → write to unique temp file → `rename()` to
   persistent path (atomic) → insert path into cache.
4. Call `index.load(persistent_path)` (~5-15 ms for 10 MB).
5. Each query gets its own `HnswIndexHandle` — deltas applied privately.

**Persistent files, not per-query temp files.** The current code
(`storage.rs:200-209`) creates a temp file per query with a
nanos-based name that has collision risk under concurrent load.
Instead, the cache writes graph bytes to a **persistent file** (one
per version, written once on cache miss). All queries for the same
version `load()` from the same persistent file. This eliminates:
- 720 MB/s sustained writes at 12 QPS × 60 MB graphs
- SSD endurance concerns (41 DWPD vs 3 DWPD spec)
- /tmp tmpfs OOM risk
- Temp file name collision bugs

**Atomic file creation:** On cache miss, write S3 bytes to a temp
file with a unique name (`..._v{version}.{pid}_{counter}.tmp`), then
`rename()` to the persistent path. POSIX `rename()` is atomic — a
concurrent `fopen()` sees either the old or new file, never a
truncated intermediate. `usearch index.load()` calls `fclose()` before
returning (`index.hpp:2418`), so the file is never held open long.

**File lifecycle:**
- On cache eviction: delete the persistent file.
- On version change (v5 → v6): delete old file after a 30-second
  delay (any in-flight `load()` completes `fclose` within ~15 ms).
- On startup: delete all files in `HNSW_CACHE_DIR` matching
  `*_v*.usearch` (the in-memory cache is empty, so no query could
  be referencing any file; needed graphs are re-downloaded from S3).
  This also handles crash recovery — no orphaned files persist.

The prior cache (`src/sql/hnsw/mod.rs:6-12`) was removed due to an
eager-invalidation race condition. The new design avoids this by using
`graph_version` comparison (no invalidation needed — version monotonically
increases, stale entries are simply replaced on miss). DROP INDEX is safe:
`txn.get(meta_key)` returns a tombstoned meta (`dropped_at` set),
treated as `None` before the cache is checked.

Cache eviction: LRU by entry count (configurable via
`HNSW_CACHE_MAX_ENTRIES`, default 64). Cache directory configurable
via `HNSW_CACHE_DIR` (default: `std::env::temp_dir()`). Per-query
`Index` copies add ~60 MB each during `load()` (pre-existing cost,
same as today without cache).

## Files to Change

### New file

**`src/sql/hnsw/s3.rs`** — HNSW S3 client module.

```rust
// Configuration parsed from env vars
pub(crate) struct HnswS3Config {
    pub bucket: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub prefix: String,           // default "hnsw"
    pub force_path_style: bool,
}

// Thin wrapper over aws_sdk_s3::Client with operation timeout
pub(crate) struct HnswS3Client { ... }

impl HnswS3Client {
    pub async fn new(config: &HnswS3Config) -> Result<Self>;

    // All methods take keyspace for multi-tenant isolation.
    // Keyspace is hex-encoded internally before building the S3 key.
    pub async fn put_graph(
        &self, keyspace: &str, db_id: u64, table_id: u64,
        index_id: u64, version: u64, data: Bytes,
    ) -> Result<()>;

    pub async fn get_graph(
        &self, keyspace: &str, db_id: u64, table_id: u64,
        index_id: u64, version: u64,
    ) -> Result<Option<Bytes>>;

    pub async fn delete_graph(
        &self, keyspace: &str, db_id: u64, table_id: u64,
        index_id: u64, version: u64,
    ) -> Result<()>;

    /// List and delete all S3 objects under a prefix.
    /// Used by GC sweep and tenant deletion cleanup.
    pub async fn delete_prefix(
        &self, keyspace: &str, db_id: u64, table_id: u64,
        index_id: u64,
    ) -> Result<u64>;  // returns count of deleted objects

    /// List objects under a database prefix with LastModified.
    /// Used by GC sweep for batched version cleanup.
    pub async fn list_objects(
        &self, keyspace: &str, db_id: u64,
    ) -> Result<Vec<S3ObjectInfo>>;  // key + last_modified + size
}

// Process-level singleton, initialized once at startup
static HNSW_S3: OnceLock<Option<HnswS3Client>> = OnceLock::new();

pub(crate) fn hnsw_s3_client() -> Option<&'static HnswS3Client>;
pub(crate) fn init_hnsw_s3() -> Result<()>;  // called from main.rs
```

**`HnswMeta` additions** (`src/sql/hnsw/storage.rs`):

```rust
pub struct HnswMeta {
    // ... existing fields ...

    /// S3 graph version. Monotonically increasing, incremented on each merge.
    /// 0 means no S3 graph has been written (TiKV-only or pre-migration).
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub graph_version: u64,

    /// Unix timestamp (seconds) when the index was dropped via DDL.
    /// Used as a tombstone: meta is NOT deleted on DROP, but marked with
    /// dropped_at. GC deletes S3 objects when now - dropped_at > gc_life_time_sec,
    /// then deletes the tombstone meta itself. This ensures MVCC-safe S3 cleanup:
    /// a reader with a pre-DROP snapshot sees the un-tombstoned meta and the S3
    /// object still exists. Using dropped_at (not S3 LastModified) is critical
    /// because LastModified reflects upload time, not drop time.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_at: Option<u64>,
}
```

### Modified files

**`src/sql/hnsw/mod.rs`**
- Add `pub(crate) mod s3;`

**`src/sql/hnsw/storage.rs`** — `load_base_graph()` + `load_hnsw_graph_with_deltas()`
- Update `storage_version != 1` guard at line 484 in
  `load_hnsw_graph_with_deltas()` to accept version 2.
- After reading meta: if `meta.dropped_at.is_some()`, return `Ok(None)`
  (treat tombstoned meta as dropped — readers with old MVCC snapshots
  see the pre-tombstone meta and proceed normally).
- In `load_base_graph()`: check `hnsw_s3_client()` and `meta.graph_version`.
- If S3 enabled and `graph_version > 0`: `s3.get_graph(...)`.
- If `graph_version == 0`: read from TiKV (legacy/migration path).
- Wrap S3 errors with human-readable context before converting to SqlError.

**`src/sql/ddl/create_index.rs`** — HNSW CREATE INDEX path
- After `serialize_hnsw_snapshot()`: if S3 enabled, call `s3.put_graph(...)`
  with `version = 1`, set `meta.graph_version = 1`.
- Skip `txn_put(graph_key, graph_bytes)` when S3 is enabled.
- Remove 8 MB size guard when S3 is enabled.
- On S3 PUT failure: abort CREATE INDEX cleanly (no compensating delete
  needed — GC handles orphans).

**`src/worker/engine.rs`** — `execute_hnsw_merge()`
- Update `storage_version != 1` guard at line 1391 to accept version 2.
- After `serialize_hnsw_snapshot()`: if S3 enabled, call `s3.put_graph(...)`
  with `version = meta.graph_version + 1`.
- Skip `check_graph_oversize_freeze()` when S3 is enabled.
- Do NOT delete old S3 versions inline — GC handles all cleanup.
- Keyspace available via `store.keyspace().unwrap_or("DEFAULT")`.
- Keep `txn.get(meta_key)` as snapshot read (do NOT use `get_for_update` —
  it reads at `for_update_ts`, causing timestamp skew with delta scan).

**`src/sql/dml/update.rs`** — storage_version checks
- Update `storage_version != 1` guards at lines 423, 566, and 929 to
  accept version 2 (S3-backed indexes). Without this, INSERT/UPDATE on
  S3-backed indexes would fail with "unsupported storage_version=2".

**`src/sql/ddl/drop.rs`** — DROP INDEX
- Change `txn_delete(hnsw_meta_key)` to: read meta, set
  `dropped_at = now_unix_seconds()`, `txn_put` the tombstoned meta back.
- No S3 cleanup here. GC handles S3 orphans using `dropped_at` as
  the MVCC-safe reference time.

**`src/storage/tikv_store/tables.rs`** — DROP TABLE / TRUNCATE
- **DROP TABLE**: set `dropped_at` tombstone on each HNSW index's meta
  (same as DROP INDEX). GC handles S3 orphans using `dropped_at`.
- **TRUNCATE**: different from DROP — the index stays alive. Read old
  meta, write a **fresh empty meta** preserving index parameters
  (`dimensions`, `distance_metric`, `m`, `ef_construction`, `label_mode`)
  but resetting `count=0, capacity=0, graph_version=0, frozen=false,
  dropped_at=None`. Delete graph blob + all deltas + rowid mappings
  (same as today). Old S3 versions become orphans cleaned by GC after the
  MVCC retention window.

**`src/worker/gc.rs`** — S3 GC sweep
- New method `sweep_hnsw_s3_orphans_for_entry()`.
- Runs on configurable interval (default 600s).
- Lists S3 objects per index prefix, deletes versions < current - 1.

**`src/main.rs`**
- Call `init_hnsw_s3()` at startup (after config parsing, before listener).

## Implementation Phases

### Phase 1: S3 client module + graph cache

- New file `src/sql/hnsw/s3.rs` with `HnswS3Config`, `HnswS3Client`, global
  singleton, `delete_prefix()`.
- Parse `HNSW_S3_*` env vars.
- Configure operation timeout (30s total including retries, 10s per retry attempt).
- Hex-encode keyspace in S3 key construction.
- Process-level LRU graph cache keyed by `(keyspace, db_id, table_id, index_id)` with
  `graph_version` freshness check. Without this cache, the S3 path is not
  viable for any workload with >1 QPS per index.

### Phase 2: Write path

- `create_index.rs`: S3 PUT instead of `txn_put(graph_key)`.
- `engine.rs`: S3 PUT in merge, skip freeze when S3 enabled.
- Both paths: write S3 **before** TiKV meta commit (write-ahead pattern).
  If S3 fails, abort. If TiKV commit fails, orphaned S3 object is
  overwritten on next merge (self-healing).

### Phase 3: Read path

- `storage.rs`: `load_base_graph()` routes by `graph_version`.
  `version > 0` → S3; `version == 0` → TiKV (migration path).
- Wrap S3 errors with context: `"HNSW index unavailable: ..."`.

### Phase 4: GC-only S3 cleanup

- `drop.rs`, `tables.rs`: NO S3 cleanup on DROP/TRUNCATE. S3 objects
  become orphans, cleaned by GC. This preserves MVCC consistency for
  concurrent readers that still see the old meta via snapshot.
- `gc.rs`: `sweep_hnsw_s3_orphans_for_entry()` — paged LIST per database.
  For live indexes: keep the current graph, leave future/speculative uploads
  untouched, and retire older versions through durable safepoint markers. For
  dropped/truncated indexes: write a prefix marker and delete the prefix only
  after TiKV's GC safepoint crosses the marker. Objects with no metadata and no
  durable cleanup marker are left untouched; upload-intent GC owns abandoned
  writer-owned uploads. Runs alongside the existing HNSW sweep loop.

### Phase 5: Tests

- Unit test: `HnswS3Client` with mock or localstack.
- Integration test: CREATE INDEX + INSERT + query + DROP INDEX with
  `HNSW_S3_ENDPOINT` pointing to MinIO.
- Regression: existing HNSW tests pass with `HNSW_S3_BUCKET` unset (TiKV-only
  path unchanged).

## Known Gaps (post-MVP)

- **DROP DATABASE:** Uses `unsafe_destroy_range` which bypasses per-index
  cleanup. The GC sweep's batched LIST at `{prefix}/{hex(ks)}/{db}/` can
  discover and delete orphaned objects for dropped databases (the worker
  registry entry survives DROP DATABASE, so GC still iterates it; if no
  schemas remain, all S3 objects under that db_id prefix are orphans).
  Pre-existing gap: HNSW TiKV keys are also orphaned today due to key
  encoding mismatch.
- **Tenant deletion:** When a keyspace is removed, S3 objects under
  `{prefix}/{hex(keyspace)}/` persist. Requires a tenant lifecycle hook
  to call `s3.delete_prefix("{prefix}/{hex(keyspace)}/")`.
- **SQLSTATE:** S3 failures currently map to `XX000` (internal_error).
  Should use `58030` (io_error) for external I/O failures. Requires a
  new `SqlError::ExternalIoError` variant.
- **EXPLAIN:** HNSW scan output does not show storage backend (S3 vs TiKV).
  Would help debugging latency issues during S3 degradation.
- **Metrics:** No S3 GET/PUT latency or error counters in the HNSW path.
  Add `hnsw_s3_get_ok/err`, `hnsw_s3_put_ok/err` to `WorkerMetrics`.
- **Migration observability:** No SQL-visible migration status. DBA cannot
  query which indexes have migrated (`graph_version > 0`), which are frozen,
  or how many deltas are pending. Needs a `pg_catalog.pg_hnsw_indexes`
  virtual table exposing `graph_version`, `storage_version`, `frozen`,
  `count`, and `storage_backend`.
- **On-demand merge trigger:** No SQL command to force immediate merge of a
  specific index. DBA must wait for the 600s background sweep. Needs
  `hnsw_force_merge(index_name text)` or equivalent.

## Non-Goals

- **Replace usearch.** The HNSW engine stays. S3 offload is orthogonal.
- **Per-tenant S3 buckets.** MVP uses one global bucket. Multi-tenant
  isolation is handled by the hex-encoded keyspace in the S3 key path
  (`{prefix}/{hex(keyspace)}/{db_id}/...`).
- **Compression.** usearch's binary format is already compact (~3-7% overhead
  for high-dimensional vectors). Compression can be added later if S3 transfer
  cost becomes a concern.

## References

- #1969 — P0: HNSW graph exceeds raft-entry-max-size
- #1970 — Frozen guard (merged)
- #1971 — S3 offload implementation (this design)
- #1968 — txn_put size guard (parent)
- `docs/design/30_tikv_value_size_design_lessons.md` — design lessons
- `src/extensions/fs/s3.rs` — existing S3 client (reference implementation)
- `src/extensions/fs/embedded/pagefs.rs:6320` — `encode_s3_key_component`
- `src/sql/hnsw/storage.rs` — current graph serialization/loading
- `src/worker/engine.rs:1358` — merge execution

## Review Changelog

Each round of expert review is recorded here with confirmed fixes and
rejected challenges, providing an audit trail of design evolution.

### Round 1 (2026-03-22)

**Experts:** 4 (consistency, multi-tenancy, storage lifecycle, error handling)

**Confirmed fixes (7):**
1. Deferred version deletion — old S3 versions cleaned by GC only, not inline after merge (reader NoSuchKey race)
2. Mandatory S3 GC sweep — `sweep_hnsw_s3_orphans_for_entry()` in `worker/gc.rs`
3. Keyspace hex-encoding — `tenant_id` has no char validation, follow fs9 `encode_s3_key_component`
4. S3 operation timeout — 30s operation, 10s per attempt
5. `delete_prefix()` API — for DROP INDEX/TABLE post-commit cleanup
6. S3 DELETE must be post-commit, fire-and-forget
7. Migration boundary — `graph_version==0` skips S3, reads TiKV directly

**Rejected (4):**
- Concurrent merge race — worker claim has pessimistic lock + TiKV write-conflict detection (3-layer defense, verified at `engine.rs:461`, `worker.rs:252-278`)
- `store.keyspace()` None — pool always maps to `Some("DEFAULT")` (verified at `pool.rs:654`, `pool.rs:835-840`)
- DROP INDEX during merge zombie resurrection — prevented by claim layer
- Non-Goals key format — doc bug, fixed inline

### Round 2 (2026-03-22)

**Experts:** 3 (GC design, error UX, migration path)

**Confirmed fixes (5):**
1. S3 rollback safety — `graph_version > 0` without S3 returns explicit error, not stale TiKV data
2. Old code rollback — S3 indexes use `storage_version = 2`, rejected by old code with clear error
3. Frozen index auto-migration — `should_skip_frozen_merge` returns false when S3 enabled
4. GC time-based retention — initially `max(statement_timeout, 5 min)`, later corrected to `gc_life_time_sec` in Round 5
5. GC batched LIST — per-database instead of per-index, cost from ~$432/mo to ~$22/mo at scale

**Rejected (0)**

**New known gaps documented:**
- SQLSTATE `58030` instead of `XX000` for S3 failures (post-MVP)
- EXPLAIN `Storage: S3` for HNSW scans (post-MVP)
- S3 GET/PUT metrics counters (post-MVP)

### Round 3 (2026-03-22)

**Experts:** 3 (storage_version safety, write-ahead ordering, CREATE INDEX path)

**Confirmed fixes (2):**
1. ~~Concurrent merge S3 overwrite race — fix: `get_for_update(meta_key)`.~~ **REVERTED in Round 8:** `get_for_update` reads at `for_update_ts`, causing timestamp skew with delta scan. Replaced with commit-conflict detection + S3 overwrite equivalence argument.
2. Missing `update.rs` in Files to Change — 3 `storage_version != 1` checks at lines 423, 566, 929 would break INSERT/UPDATE on S3-backed indexes if not updated

**Rejected (0):**
All other reviewed areas found correct with evidence:
- `storage_version` checks at all 5 sites return hard errors, never skip (verified at `storage.rs:484`, `engine.rs:1391`, `update.rs:423,566,929`)
- Write-ahead ordering is safe: S3 PUT failure → abort, commit failure → self-healing overwrite at same version (verified merge loop at `engine.rs:1375-1505`)
- CREATE INDEX: S3 integration does not break existing flow; CONCURRENTLY is already rejected for HNSW (`create_index.rs:165-168`)
- Rollback: old code rejects `storage_version=2` before any S3/TiKV access (verified all 5 check sites)

**Suggestions (not blocking):**
- Skip S3 for empty-table CREATE INDEX (count=0), keep graph_version=0
- Document that each merge batch writes a new S3 version (N batches = N PUTs)

### Round 4 (2026-03-22)

**Experts:** 3 (latency analysis, migration UX, S3 cost model)

**Confirmed fixes (3):**
1. Process-level LRU cache promoted to P0 — without it, every HNSW query does a full S3 GET (200-400ms latency, 60 TB/day bandwidth at scale). Cache uses `graph_version` for invalidation. Added to Phase 1 and new design section. (Evidence: `hnsw_scan.rs:166` calls `load_hnsw_graph_with_deltas` on every query; `mod.rs:6-12` confirms no cache exists; cost model shows $42/mo S3 cost is fine but 60 TB/day bandwidth without cache is not viable)
2. Delta apply latency documented — 50K deltas × VECTOR(1536) = ~15s per query on frozen indexes. Pre-existing issue amplified by S3 migration path. Added to Latency section.
3. Latency table corrected — now includes temp file I/O + usearch load costs (was understating by ~6-60ms)

**Rejected (0)**

**New known gaps documented (post-MVP):**
- Migration observability: `pg_catalog.pg_hnsw_indexes` virtual table for `graph_version`, `frozen`, `storage_backend`
- On-demand merge trigger: `hnsw_force_merge(index_name)` SQL function
- `init_hnsw_s3()` should fail-fast on invalid bucket/credentials (not silently disable)
- Frozen index merge OOM risk: memory estimation before merge start

### Round 5 (2026-03-22)

**Experts:** 2 (cache correctness, GC+cache interaction)

**Confirmed fixes (3):**
1. Cache must store serialized bytes `Arc<Vec<u8>>`, NOT loaded `HnswIndexHandle` — usearch `Index` is not thread-safe (`add()` and `search()` both use `contexts_[0]` shared mutable scratch, `index.hpp:2012,2093`). `HnswIndexHandle` is `!Clone`. Each query loads its own Index from cached bytes (~5-15ms). (Evidence: `storage.rs:100-101` unsafe Send+Sync; usearch `rust/lib.cpp:36-38` uses default `add_config_t` with thread=0)
2. GC retention_window must use `gc_life_time_sec` (default 24h), not `statement_timeout` — `statement_timeout` is per-session GUC, users can SET to hours or 0. GC runs outside sessions. MVCC snapshot lifetime bounded by `gc_life_time_sec` (`config.rs:53`). Plus count-based N-1 guard as belt-and-suspenders.
3. Per-query memory cost documented — cache bytes (60MB) + each concurrent query loads its own Index (60MB). 10 concurrent queries = 660MB per index. Pre-existing cost, same as without cache.

**Rejected (confirmed correct):**
- Process restart after GC: safe — cache re-reads meta on restart, GC never deletes current version (`v < current` guard)
- Prior eager-invalidation race: correctly avoided by version comparison (no invalidation step, just staleness detection on next read)
- DROP INDEX with cached entries: safe — `txn.get(meta_key)` returns None before cache is checked (`storage.rs:473-478`)

### Round 6 (2026-03-22)

**Experts:** 2 (cache+temp file ops, document completeness)

**Confirmed fixes (2):**
1. Persistent cache files instead of per-query temp files — at 12 QPS × 60 MB, per-query temp files cause 1.44 GB/s disk I/O (41 DWPD, 14x over SSD spec). Plus `temp_file_path()` at `storage.rs:200-209` has collision risk (nanos-based, no thread/task uniqueness). Fix: write graph bytes to persistent file once on cache miss, all queries `load()` from same file. Eliminates sustained writes entirely. (Evidence: `storage.rs:200-209` collision, SSD math: 720 MB/s writes / (500 GB × 3 DWPD) = 41 DWPD)
2. Document completeness fixes — added `HNSW_CACHE_MAX_ENTRIES` and `HNSW_CACHE_DIR` to Configuration section; added `list_objects` to `HnswS3Client` API; fixed gc.rs Files to Change to include dual retention guard; fixed Round 2 changelog stale `statement_timeout` reference; clarified Phase 1 timeout wording

**Rejected (0):**
All other reviewed areas found consistent — Non-Goals correctly excludes cache (promoted to P0 in Round 4), S3 key format consistent across sections, frozen guard interaction matches Files to Change, migration `graph_version==0` boundary used consistently

### Round 7 (2026-03-22)

**Experts:** 2 (persistent cache file correctness, DROP/TRUNCATE cleanup paths)

**Confirmed fixes (2):**
1. Atomic file creation for persistent cache — concurrent cache misses both writing the same file causes `fs::write` truncation race (`fread` sees truncated file mid-load). Fix: write to unique temp file then `rename()` (POSIX atomic). (Evidence: `fs::write` uses `O_CREAT|O_TRUNC` per Rust std, usearch `load()` holds file open via `fopen/fread/fclose` loop at `index.hpp:2347-2418`)
2. Cache file lifecycle — startup cleanup (delete all `*_v*.usearch` in `HNSW_CACHE_DIR` on init, handles crash recovery); version transition cleanup (delay 30s delete of superseded file, `load()` completes `fclose` within ~15ms per `index.hpp:2418`); DELETE path uses `flush_pending_s3_cleanups` pattern (same as `flush_pending_hnsw_merges` at `executor/core/mod.rs:313`)

**Rejected (confirmed correct):**
- `usearch index.load()` fully copies data into memory and `fclose`'s before returning (`index.hpp:2347-2418`) — file safe to delete after `load()` returns
- Cache eviction during `load()`: Linux `unlink()` preserves inode while fd is open — safe (narrow TOCTOU window for `fopen` is low-risk)
- DROP/TRUNCATE S3 cleanup architecture: `pending_s3_cleanups` pattern is implementable following `pending_hnsw_merges` precedent; rollback safety via `clear_trigger_activations()`
- TRUNCATE resets `graph_version` to 0, `delete_prefix` cleans all S3 versions — correct

### Round 8 (2026-03-22)

**Experts:** 2 (final adversarial audit, end-to-end lifecycle trace)

**Confirmed fixes (2):**
1. `storage_version` checks still missing 2 of 5 sites — `storage.rs:484` (query path) and `engine.rs:1391` (merge path) were not in Files to Change. Round 3 added `update.rs` (3 sites) but missed these 2. An S3-backed index with `storage_version=2` would fail ALL queries and merges. Fix: added both to Files to Change.
2. `get_for_update` REVERTED — Round 3 added `get_for_update(meta_key)` to serialize concurrent merges. Round 8 found this causes timestamp skew: `get_for_update` reads at `for_update_ts` (current), but delta scan reads at `start_ts` (older). After waiting for a concurrent lock, meta shows version N+1 while deltas come from the N snapshot — causing duplicate delta application and graph inflation. Fix: reverted to plain `txn.get(meta_key)` (snapshot read). Concurrent merge safety relies on TiKV commit conflict detection + S3 overwrite equivalence (same base + same deltas = equivalent content).

**Rejected (confirmed correct):**
- End-to-end lifecycle (CREATE → INSERT → query → merge → query cache hit → DROP): fully specified, no gaps beyond the 2 fixes above
- Cache hit fast path (~6ms): TiKV meta read + index.load from persistent file — correct
- S3 cleanup via `flush_pending_s3_cleanups` pattern — implementable, rollback-safe
- DROP INDEX cache entry: becomes zombie until LRU eviction, causes no incorrect results (meta None check fires first)

### Round 9 (2026-03-22)

**Experts:** 2 (adversarial final audit, delta equivalence proof)

**Confirmed fixes (1):**
1. Concurrent merge description corrected — "same deltas" changed to "overlapping delta sets (superset)"; "commit-time write-conflict" changed to "pessimistic lock conflict at `put()` time" (evidence: `transaction.rs:463-473` shows `put()` calls `pessimistic_lock()` eagerly; `WakeUpModeNormal` returns `WriteConflict` per `kvrpcpb.proto:154`). Documented the rare graph inflation behavior (duplicate entry from loser's S3 overwrite + unconsumed delta re-add) as known and benign (scan dedup at `hnsw_scan.rs:102-115` handles it, frequency requires merge > 1 min).

**Rejected (confirmed correct):**
- `storage_version=2 + graph_version=0` impossible state: CREATE INDEX aborts on S3 failure before meta commit — meta never committed (verified at `create_index.rs:537`)
- Concurrent `fread` on persistent cache file: safe, each `fopen` gets its own `FILE*` with independent stdio lock, kernel page cache serves data
- S3 key length: max ~306 bytes, well under 1024-byte S3 limit
- Both-commit scenario (no lock overlap): safe because later snapshot produces superset graph (verified: `start_ts_B > commit_ts_A` means B sees A's committed state + new deltas)

### Round 10 (2026-03-22) — FINAL

**Expert:** 1 (final completeness audit)

**Confirmed fixes (2):**
1. GC pseudocode off-by-one: `v < current` corrected to `v < current - 1` to match retention policy (line 252) and Files to Change (line 569). Without this fix, GC would delete version N-1, breaking in-flight readers.
2. Cache type annotation: `Arc<Vec<u8>>` corrected to `PathBuf` to match the persistent file design (Round 6-7). The cache stores a file path, not in-memory bytes.

**Verified correct (all other checks passed):**
- All 5 storage_version check sites listed in Files to Change (5/5)
- Cache design internally consistent (after type fix): persistent files, atomic rename, startup cleanup, version transition cleanup, LRU eviction
- GC design internally consistent (after pseudocode fix): dual guard, batched LIST, gc_life_time_sec retention
- DELETE path: flush_pending_s3_cleanups pattern documented in both architecture and Files to Change
- Migration path: complete (graph_version=0 boundary, frozen auto-unfreeze, storage_version=2, rollback error)
- Configuration: all 7 env vars listed
- Known Gaps: all properly scoped as post-MVP, none are disguised blockers

### Round 11 (2026-03-22) — External review response

**Source:** PR #1980 review comments (external reviewer)

**Confirmed fixes (3):**
1. **P0: DROP INDEX breaks MVCC for concurrent readers.** `flush_pending_s3_cleanups` deleted S3 objects immediately post-commit, but a reader with an older MVCC snapshot could still see the meta and need the S3 object. Fix: removed ALL inline S3 deletion from DROP/TRUNCATE. S3 orphans are cleaned exclusively by GC sweep, which applies `gc_life_time_sec` time guard to dropped indexes too — matching TiKV's MVCC reclamation window. (Evidence: `hnsw_scan.rs:166` reads meta from TiKV txn, then does S3 GET outside txn; `storage.rs:420-424` confirms meta+graph were both in same TiKV snapshot before S3)
2. **P1: Cache key missing keyspace.** ID sequences are per-keyspace (`_sys_next_database_id` starts at 1 per keyspace, `encoding/mod.rs:81`; keyspace isolation is at TiKV wire protocol level, `keyspace.rs`). Two tenants can have identical `(db_id=1, table_id=1, index_id=1)`. Cache key changed to `(keyspace, db_id, table_id, index_id)`.
3. **P2: `executor/core/mod.rs` missing from Files to Change.** The `flush_pending_s3_cleanups` pattern requires changes to the executor core (new field, push method, flush method, rollback clearing in `clear_trigger_activations`). Now moot because inline S3 deletion was removed entirely (P0 fix), but GC-based cleanup still benefits from the executor knowing which indexes were dropped for metrics/logging.

**Note on reviewer's txn_put guard claim:** Reviewer was **correct**.
`check_value_size()` was added to `src/txn/mod.rs` by PR #1979 on master
after this branch diverged. `txn_put()` and `txn_batch_mutate()` now call
it before every write. The earlier R11 claim that "txn/mod.rs is 110 lines,
no such function exists" was wrong — it was checked against the stale
branch, not master. The design doc's references to #1968 as "unimplemented"
are also stale; the centralized guard now exists.

### Round 12 (2026-03-22) — External review response #2

**Source:** PR #1980 follow-up review

**Confirmed fixes (2):**
1. **P0: GC for dropped indexes still used LastModified, not drop time.** R11 removed inline deletion but the GC's "dropped index" branch still used `object.last_modified < now - retention_window`. A quiet index merged 30 days ago would have LastModified = 30 days, and GC would delete it immediately after DROP — same MVCC break, just moved to GC. Fix: tombstone design — meta is NOT deleted on DROP, but marked with `dropped_at = now()`. GC uses `now - dropped_at > gc_life_time_sec` as the MVCC-safe boundary. Three branches: no meta (legacy fallback on LastModified), tombstoned meta (use dropped_at), live meta (existing dual guard). Reader path treats `dropped_at.is_some()` as `None`. (Evidence: reviewer's scenario — 30-day-old graph, gc_life_time_sec=24h, GC deletes immediately on first pass after DROP)
2. **P2: Phase 1 cache key still had old 3-tuple.** R11 fixed the main cache section but missed the Phase 1 summary at line 626. Fixed to `(keyspace, db_id, table_id, index_id)`.

**Corrected (1):**
- Reviewer was right about `check_value_size()` — it exists on master
  (added by PR #1979, commit `5338968e`). Our branch diverged before
  that merge. `src/txn/mod.rs` on master is 344 lines with
  `check_value_size()` at line 96, called by `txn_put` at line 149.
  R11's rejection was based on stale branch state. Corrected above.

### Round 13 (2026-03-22) — External review response #3

**Source:** PR #1980 follow-up review

**Confirmed fixes (3):**
1. **TRUNCATE must NOT use tombstone.** TRUNCATE keeps the index alive (just empties it). Setting `dropped_at` strands the index: INSERTs write deltas but scans return `Ok(None)`. Fix: TRUNCATE writes a fresh empty meta (`graph_version=0, count=0, dropped_at=None`) preserving index parameters. Old S3 versions become GC-eligible orphans. DROP INDEX/TABLE keeps the tombstone pattern. (Evidence: current `truncate_table` at `tables.rs:847` already deletes meta, not tombstones; DML at `update.rs:432` handles `meta=None` as "empty"; `schema.indexes` survives TRUNCATE)
2. **`check_value_size()` exists on master.** R11/R12 incorrectly rejected this — our branch diverged before PR #1979 (`5338968e`) added the centralized guard. `txn_put` on master calls `check_value_size()` at line 149. Corrected R11 note and R12 rejection.
3. **`delete_prefix()` API doc comment stale.** Still said "Used by DROP INDEX/TABLE (post-commit cleanup)". Changed to "Used by GC sweep and tenant deletion cleanup."

**Design review complete after 13 rounds. 33 total issues found and fixed.**
