# fs9 v2: Final Storage Architecture (TiKV Metadata, S3 Objects, Packfiles)

**Status**: Active (2026-03-11). Local copy of the consolidated design comment in `c4pt0r/db9-server#1752` (comment id: `4039365600`).

> **Non-SoT note**
>
> This document is a design proposal, not a current-behavior contract.
> Validate current behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`, and the implementation under `src/**` before treating this as shipped behavior.
>
> **Implementation drift note (2026-03-12)**
>
> The current fs9-v2 implementation no longer exposes a published `LegacyPages`
> storage class or `legacy` wire metadata. Published files are `InlineBlob`,
> `PackEntry`, or `Object`; `_fs_P{inode}:{page}` is now an internal staging
> spool only. Current rollout also requires a fresh fs9 keyspace / S3 prefix;
> older fs9-v2 keyspaces are rejected instead of being auto-migrated. The
> finalized schema revision is `FS9_STORAGE_FORMAT_VERSION = 4`, and local
> spool state is namespaced by `keyspace / format-version / fs_instance_id`.

## 0. Finalized Contract (2026-03-12)

This section is the current fs9 v2 contract for implementation and rollout.

- fs9 v2 serves only three published storage classes: `InlineBlob`, `PackEntry`, and `Object`.
- `_fs_P{inode}:{page}` is staging-only internal state, never a published storage form.
- The supported durable schema is storage-format version `4` only.
- Rollout is **fresh TiKV keyspace + fresh S3 prefix**. Older fs9-v2 prototype revisions are rejected.
- The persisted superblock binds a keyspace to both one `fs_instance_id` and one object-store identity.
- All user-facing fs9 surfaces for one tenant keyspace, including SQL builtins,
  SQL table functions, the db9-server WebSocket API, and clients layered on
  that WebSocket path such as FUSE / `db9 fs cp`, must resolve the same
  authoritative namespace and visibility contract. Transport differences are
  not allowed to create separate filesystem roots or alternate directory views.
- If the same keyspace name is recreated into a different fs instance while db9-server stays alive, restart db9-server before further fs9 access.
- Bootstrap / backend acquire is the only supported gate for format, object-store binding, and process-local keyspace identity. Serving paths use bound runtime state only; they do not re-read `_fs_S` or coordinate instance changes on the hot path.
- Hidden mutators are fenced by durable per-record `fs_instance_id` ownership. Background maintenance also keeps a low-frequency self-stop probe so stale loops stand down promptly after an instance replacement.
- Abort / GC / recovery are internal cleanup paths and must keep working best-effort on internal transactions.
- Malformed local pack journals are quarantined as helper-state corruption and must not block authoritative TiKV/S3 cleanup.

Operational note:

- Restart is only required for operator-driven keyspace recreation / rebinding, for example deleting and recreating fs9 metadata for the same keyspace name, or reusing the same keyspace name with a fresh S3 prefix / bucket while `db9-server` is still running.
- Ordinary file reads and writes against the same bound filesystem instance do **not** require restart. Using S3 as the data plane does not by itself trigger this rule.

### 0.1 SQL Scalar Execution Model (2026-03-13)

The fs9-v2 SQL scalar path needs its own execution model instead of treating
`fs9_*` builtins as thin wrappers over raw filesystem backend calls.

The root cause behind the current SQL issues is structural:

- the SQL scalar layer currently calls `FsBackend` directly
- `FsBackend` is a byte-oriented, storage-class-aware backend boundary
- backend acquire / bootstrap is repeated from each builtin leaf
- raw backend mutation limits for sealed `PackEntry` / `Object` files leak into
  SQL builtins that appear to be generic logical file operations
- text vs binary read semantics are implicit and currently inconsistent

The design correction is to introduce a **statement-scoped SQL fs client** as
the only boundary between SQL builtins and the raw backend.

That SQL fs client should own three responsibilities:

1. **Statement-scoped acquire/cache**

   - acquire the fs backend once per statement runtime scope
   - reuse the bound backend for all scalar fs9 calls in that statement
   - keep bootstrap / runtime binding checks out of individual builtin leaves

   This acquire/cache rule is not scalar-only. Any SQL entry point that reads
   or writes the tenant fs9 namespace, including `extensions.fs9(...)`,
   `read_parquet('fs9://...')`, and `COPY FROM 'fs9://...'`, must reuse the
   same statement-scoped backend binding instead of constructing an uncached
   backend independently.

2. **Explicit SQL type contract**

   - `fs9_read` and `fs9_read_at` are **text** functions, not generic byte
     readers
   - text reads must be **strict UTF-8**; invalid UTF-8 is an error, not a
     lossy conversion
   - `fs9_read_at(path, offset, len)` remains a byte-window read; it returns
     `TEXT` only when the selected byte slice is itself valid UTF-8
   - SQL must expose separate **bytea-safe** read entry points for binary
     round-trip use cases
   - write-side functions may continue to accept `TEXT` or `BYTEA`, but the
     read side must no longer pretend that one text API covers both

3. **No SQL mutation carve-out for sealed files in Phase 1**

   - SQL scalar `fs9_write_at`, `fs9_append`, and `fs9_truncate` must preserve
     the same sealed-file mutation boundary enforced by the raw fs backend
   - `PackEntry` and `Object` remain sealed for partial mutation in Phase 1
   - `SqlFsClient` exists to centralize statement-scoped backend acquire/cache
     and the explicit text-vs-bytea read contract, not to change write
     semantics relative to WS / FUSE / protocol paths

Non-goals for this SQL execution model:

- do **not** change raw `FsBackend` semantics into a SQL-specific contract
- do **not** make WebSocket / FUSE partial mutation on sealed files implicitly
  succeed
- do **not** reintroduce hot-path `_fs_S` probing or per-call runtime
  coordination

Implementation follow-up for this model is tracked in:

- #1806 SQL text vs bytea read contract
- #1808 statement-scoped backend reuse for SQL fs9 builtins

## 1. Summary

fs9 v2 should evolve from a whole-file TiKV page filesystem into a three-plane architecture:

- **metadata plane** in **TiKV** as the authoritative system of record
- **data plane** in **S3-compatible object storage**
- **optional local node state** only for spool/cache/journal, never as authoritative metadata

This is **not** merely “replace pagefs data on TiKV with S3.” The real problem is that the current stack is whole-file end to end:

- server reads whole files into memory
- WebSocket streaming still materializes whole-file buffers
- FUSE `open()` downloads full files into `Vec<u8>`
- FUSE `flush()` uploads full-file buffers
- CLI `db9 fs cp` uses whole-file upload/download

Large files and small files need different storage primitives:

- **tiny hot mutable files** stay inline in TiKV
- **small sealed files** go into immutable **packfiles / bundles** in S3
- **large files** go to direct S3 objects with presigned multipart upload and range download

The architectural correction in this final version is explicit:

> Keep authoritative fs9 metadata in **TiKV**.
> Do not move authoritative inode / directory / file-location metadata into per-node SQLite or WAL.

SQLite/WAL is acceptable only as:

- local durable spool for pending pack flushes
- local crash-recovery journal for upload workers
- local bundle cache index

## 2. Goals

1. Preserve **tenant-isolated authoritative metadata** in TiKV across multi-node deployments.
2. Reach the large-file throughput target through **presigned multipart upload** and **parallel range GET**.
3. Solve the **small-file + S3** problem without creating one object per file.
4. Preserve current fs9 path-level atomicity by using **hidden staging + atomic publish**.
5. Freeze one durable storage contract and reject older prototype revisions cleanly.
6. Make pre-Phase-4 product boundaries explicit so FUSE does not silently hit pathological object behaviors.

## 3. Non-Goals

1. Phase 1 is **not** a full handle-based FUSE redesign.
2. Phase 1 is **not** “all fs9 operations become streaming and zero-copy.”
3. Phase 1 does **not** make S3 objects support cheap `append`, `truncate`, or `write_at`.
4. Phase 1 does **not** make local SQLite/WAL authoritative cluster metadata.
5. Phase 1 does **not** preserve old fs9-v2 prototype metadata; rollout uses fresh keyspace / prefix.

## 4. Current Bottlenecks

The current issue correctly identifies the main bottleneck: whole-file behavior at every layer.

| Layer | Current behavior | Effect |
|------|------------------|--------|
| Server `read_file_stream()` | effectively whole-file buffer today | memory amplification |
| WS streaming read | whole file then 64 KiB frames | no true backend streaming |
| WS streaming write | server buffers complete file then commits | large memory spike |
| FUSE `open()` | full-file download into handle buffer | poor latency and memory use |
| FUSE `flush()` / `release()` | full-file writeback | pathological for large files |
| FUSE large write | overwrite + append loop | unacceptable for S3 objects |
| CLI `db9 fs cp` | whole-file upload/download | throughput far below target |

Reviewer benchmark baseline from **March 11, 2026** confirms this shape:

- small file upload: about **4.4 files/s**
- small file download: about **12.9 files/s**
- 16 MiB upload: about **1.55 MB/s**
- 16 MiB download: about **4.01 MB/s**

For small files, the limiting factor is not only TiKV page layout. It is also **per-request RTT**, especially on the current single-connection FUSE/WS path. A final design has to address both.

## 5. Final Architecture

```text
Client (CLI / FUSE / SQL)
    |
    |  Control plane: WebSocket / SQL
    |  Data plane: presigned HTTP for large objects
    v
db9-server
    |
    |-- TiKV metadata plane (authoritative)
    |     - inode metadata
    |     - directory tree
    |     - DataRef
    |     - lifecycle keys (_fs_L)
    |     - bundle manifests / bundle entry metadata
    |     - upload reservation metadata
    |
    |-- Local node state (non-authoritative)
    |     - pack spool
    |     - upload journal
    |     - bundle/object cache
    |
    `-- S3 data plane
          - direct objects
          - immutable packfiles / bundles
```

### 5.1 Metadata Plane: Authoritative in TiKV

TiKV remains the durable source of truth for:

- inode metadata
- directory entries
- file-to-data mapping
- lifecycle / GC state
- upload reservations
- bundle manifests
- version / generation counters

This aligns with the repo’s storage invariant that persistent tenant data stays keyspace-isolated in TiKV.

### 5.2 Local Node State: Optional and Non-Authoritative

Local disk state is allowed for performance and crash tolerance, but only as a helper:

- pending pack spool before S3 flush
- upload worker journal
- local bundle/object LRU cache
- temporary FUSE writeback state

Loss of this local state must not change visible committed filesystem metadata.

### 5.3 Data Plane: S3-Compatible Objects

S3 is the data plane for:

- direct large-file objects
- immutable small-file packfiles

Large-file data should bypass db9-server whenever possible:

- upload: client -> S3 multipart PUT
- download: client -> S3 GET / range GET

db9-server stays in the control plane for:

- auth
- policy
- metadata updates
- reservation / publish
- GC

## 6. Storage Forms

The final design uses three published file layouts plus one internal staging layout:

```rust
enum DataRef {
    None,
    InlineBlob,
    PackEntry {
        bundle_id: u64,
        offset: u64,
        len: u32,
        checksum: [u8; 32],
        generation: u64,
    },
    Object {
        key: String,
        version: u64,
        checksum: [u8; 32],
    },
    StagingPages,
}
```

### 6.1 `InlineBlob`

- stored as one TiKV value, e.g. `_fs_B{inode_id}`
- best for tiny mutable files
- lowest latency for `write_file`, `write_at`, `append`, `truncate`
- default initial threshold: **64 KiB**, subject to validation

### 6.2 `PackEntry`

- points into an immutable S3 bundle
- best for sealed small files, especially bulk upload / directory copy
- read via local cache or S3 range GET
- not rewritten in place

### 6.3 `Object`

- one large logical file maps to one S3 object
- best for files that want multipart upload and range download
- partial mutation is not supported in Phase 1

### 6.4 `StagingPages`

- internal TiKV paged spool under `_fs_P{inode}:{page}`
- used only before routing a stream to `InlineBlob` or `Object`
- never exposed as published file metadata

### 6.5 Durable Format Rules

The finalized fs9-v2 implementation does **not** promise compatibility with earlier prototype revisions that wrote different metadata under the same storage-format version.

- the superblock version must match exactly
- older prototype keyspaces must fail fast before lifecycle or inode scans begin
- local spool state must be namespaced by storage-format version so older journals are ignored structurally

## 7. Routing Policy

Routing should be based on **size + mutability + access pattern**, not only size.

| File class | Route | Reason |
|-----------|-------|--------|
| `<= 64 KiB`, hot or frequently mutated | `InlineBlob` | low latency, cheap mutation |
| roughly `64 KiB - 1 MiB`, sealed or bulk-uploaded | `PackEntry` | amortize S3 RTT/cost |
| `>= 1 MiB` or sequential throughput workload | `Object` | multipart + range GET |
| no S3 configured, above inline threshold | reject | no old pagefs fallback in the finalized contract |

### 7.1 Important Rule: Do Not Pack Hot Mutable Files Immediately

Bundles are for **sealed** small files, not for general mutation.

If every small overwrite immediately rewrites bundle contents, the system just moves write amplification from TiKV pages to S3 packfiles. The correct rule is:

- hot mutable small files stay inline
- bulk or sealed small files enter packs
- mutation of a packed file uses **full replacement**
- optional future rule: first mutation of a packed file may **promote** it back to `InlineBlob`

### 7.2 Batch Bounds

`BatchWrite` must be bounded by:

- max file count
- max per-file inline size
- max total raw bytes
- max total encoded payload
- max total TiKV transaction size

A concrete starting point should distinguish transport from storage:

- WS JSON transport payload: keep encoded request size under about **1 MiB**
- inline-per-file cap: at or below `FS9_INLINE_MAX`
- `FS9_BATCH_WRITE_MAX_FILES = 32`
- server-side commit budget: `FS9_BATCH_WRITE_MAX_TOTAL_BYTES = 4 MiB`

The 64 KiB inline threshold is a reasonable starting default, but the real constraint is transaction size and compaction behavior, not a hard TiKV per-value limit.

## 8. Packfile / Bundle Design

### 8.1 Bundle Semantics

Bundles are **immutable** S3 objects that store many logical files:

```text
/{tenant}/packs/{bundle_id}.pack
    [entry header][file bytes]
    [entry header][file bytes]
    ...
    [footer / manifest]
```

Each committed file entry is recorded in TiKV as:

- `bundle_id`
- `offset`
- `len`
- `checksum`
- `generation`

### 8.2 Bundle Write Path

1. Client sends many small files via bounded batch or streaming-to-spool path.
2. Server writes pending bytes into a local spool/journal.
3. Flush triggers when any threshold fires:
   - size threshold, e.g. 4 MiB
   - timeout, e.g. 1 second
   - file count threshold
4. Server uploads one immutable bundle object to S3.
5. Server commits TiKV metadata in one transaction:
   - bundle manifest
   - per-file `PackEntry` mappings
   - lifecycle cleanup

### 8.3 Bundle Read Path

1. Resolve path -> inode -> `PackEntry`
2. Check local bundle cache
3. On miss, use S3 range GET against the exact bundle byte range
4. Optionally cache full bundle or hot slices locally

### 8.4 Bundle Compaction

Overwrite never mutates a bundle in place.

Instead:

- new data lands in a new inline value, new bundle, or new object
- old `PackEntry` becomes garbage
- background compaction can later rewrite live entries into fresh bundles

### 8.5 Metadata Placement Rule

Bundle manifests and file-to-bundle mappings stay authoritative in TiKV.
If SQLite exists in this design, it is only a local crash-safe spool/cache helper.

## 9. Lifecycle and Atomic Publish

### 9.1 `_fs_L` Is the Only Source of Transitional Truth

Use `_fs_L{inode_id}` as the sole source of non-clean lifecycle state.

Absence of `_fs_L` means `Clean`.

Do **not** duplicate lifecycle state into both the inode and `_fs_L`.

This avoids split-brain between:

- reader-visible inode state
- GC-visible sidecar state

### 9.2 Lifecycle States

Conceptually:

```rust
enum FileLifecycle {
    Uploading { ... },
    Committing { ... },
    Packing { ... },
    Deleting { data_ref: DataRef },
}
```

`Packing` is worth making explicit in the final design because pack flush now becomes a real recovery boundary, just like object upload.

### 9.3 Hidden Staging + Atomic Publish

Large upload and pack publish must preserve current path-level atomicity.

Rule:

> `PrepareUpload` creates a hidden staging inode / reservation, not a visible namespace entry.

The visible directory entry only appears during the final TiKV publish transaction.

Reservation state must capture:

- target path hash
- expected parent inode
- expected prior target generation
- target object version or bundle generation
- upload ID / bundle flush job ID
- expiry

To preserve current fs9 semantics, missing parent directories are created during the final publish transaction, not as a visible side effect of the reservation step.

### 9.4 Commit Behavior

`CompleteUpload` / bundle publish must:

1. verify HMAC token
2. verify token expiry
3. re-check path CAS conditions
4. verify uploaded object or bundle availability
5. atomically publish metadata in TiKV, including final parent creation if needed
6. mark replaced prior data for deferred deletion
7. delete `_fs_L`

### 9.5 Retry and Verification Rules

This final design explicitly includes the review corrections:

- **TiKV optimistic commit retry**: retry publish commit a small bounded number of times with exponential backoff
- **HEAD verification retry**: retry `HEAD` / existence verification for S3-compatible stores that may lag briefly
- **startup GC jitter**: randomize initial sweep delay to avoid herd behavior
- **idempotent GC CAS**: cleanup should proceed only if lifecycle state still matches expected value

### 9.6 Version / Generation Allocation

Object version and publish generation must be allocated **inside TiKV transactions**, never inferred client-side.

Required properties:

- no S3 key collision under concurrent overwrite
- deterministic stale-version cleanup
- explicit CAS against overwrite races

A practical model is:

- per-file logical generation counter for namespace replacement
- per-file monotonic object version counter for `Object`
- monotonic bundle ID allocation for new packs

Rapid overwrite of an object-backed file will temporarily amplify S3 storage until GC catches up. That is acceptable for sealed/object workflows, but it is another reason hot mutable files should stay inline instead of being routed eagerly to `Object`.

## 10. Protocol and Existing Path Mapping

WebSocket remains the control plane. The design must explicitly map existing operations to the new storage forms.

### 10.1 `stat` / `readdir`

Extend `FileInfo` with:

- `storage`: `inline | pack | object`
- `sealed`: whether partial mutation is rejected

This is backward-compatible with the current `db9-cli` parser because the client `FileInfo` deserializer does **not** use `deny_unknown_fields`.

`readdir` should remain metadata-first. The final design should avoid making a general `readdir_with_content` style API the default path, because that would blur the metadata/data-plane separation again and can overfetch badly on large directories.

### 10.2 Write Path Mapping

| Request path | `InlineBlob` | `PackEntry` | `Object` |
|-------------|--------------|-------------|----------|
| WS inline `write_file` | direct TiKV write | full replacement, usually route to inline or new pack | server-side PUT for bounded files |
| WS streaming write | stream to local spool then inline commit | stream to local spool then pack publish | server-side multipart upload forwarded by server |
| `CreateUpload` + presigned parts | N/A | optional future sealed-small direct path | preferred large-file path |

The missing mapping called out in review is deliberate here: the existing WS streaming write path continues to work, but its backend target depends on routing. It must not silently fall back to buffering a giant S3 object in memory.

### 10.3 Read Path Mapping

| Request path | `InlineBlob` | `PackEntry` | `Object` |
|-------------|--------------|-------------|----------|
| `read_file` | direct TiKV | exact pack slice | bounded server-side GET |
| `read_file_at` | direct TiKV | exact pack slice | S3 range GET |
| `read_file_stream` | streaming cursor | pack slice stream | `get_object_stream()` |
| `PrepareDownload` | usually unnecessary | optional future | preferred large-file path |

The S3 client must include a real streaming adapter:

```rust
async fn get_object_stream(...) -> Result<impl AsyncBufRead>
```

Without this, object-backed WS streaming reads and `COPY FROM fs9://` claims are not credible.

### 10.4 Bounded Directory Prefetch Instead of `readdir_with_content`

If directory-local small-file prefetch is needed, keep it explicitly bounded rather than baking content return into plain `readdir`.

Safer shapes are:

- `BatchStat`
- `BatchInlineRead`
- a bounded `PrefetchDirectoryEntries` operation

Such a prefetch path should only trigger when:

- file count is below a strict cap
- per-file size is below a strict cap
- total response payload is capped
- locality is favorable, ideally many entries share one bundle

This preserves the metadata/data-plane separation while still allowing a targeted latency optimization for tiny directory-local workloads.

## 11. Product Boundary for SQL and FUSE

### 11.1 Mutation Rules

Phase 1 mutation rules should be explicit:

| Operation | InlineBlob | PackEntry | Object |
|----------|------------|-----------|--------|
| `write_file` (full replace) | yes | yes | yes |
| `write_file_at` | yes | no or promote-to-inline | no |
| `append_file` | yes | no or promote-to-inline | no |
| `truncate` | yes | no or promote-to-inline | no |

The important part is that **Object is sealed** in Phase 1. `PackEntry` is either sealed or promoted on first mutation; it must not rewrite bundles in place.

### 11.2 FUSE Before Phase 4

The current FUSE path is still whole-file and single-connection biased. For that reason:

- large `Object` reads via current `open()` are dangerous
- large `Object` writes via current `flush()` are unacceptable

So this design requires one of these before exposing large objects broadly through mount:

1. a **size guard** for object-backed `open()` before Phase 4, or
2. a **read-only presigned/range fast path** for large object reads

Additionally, a cheap Phase 2 improvement should be included:

- replace one serialized WS client with a **4-8 connection pool**

That does not solve the whole FUSE problem, but it materially improves small-file RTT-bound throughput.

### 11.3 SQL Boundary

Phase 1 SQL scope should be conservative:

- `fs9_read(path)` remains bounded by `MAX_BYTES_PER_FILE`
- `fs9_read_at(path, offset, len)` can be efficient for `PackEntry` and `Object`
  but its effective post-EOF-trim read window must still error explicitly if it
  exceeds `MAX_BYTES_PER_FILE` (no silent truncation)
- `fs9_write(path, data)` is full replacement only
- `fs9_write_at` / `append` / `truncate` reject `Object`
- `COPY FROM fs9://` for `Object` or `PackEntry` should be treated as **deferred unless true streaming backend support lands in the same phase**

Also, the current global 128 MiB read budget may need to split into separate TiKV and S3 budgets because object reads hold resources for much longer.

## 12. Security, Credentials, and Tokens

### 12.1 S3 Credential Model

Phase 1 should use:

- server-managed IAM/API credentials
- shared bucket or endpoint
- strict per-tenant prefix layout
- exact-key presigned URLs

Prefix validation should be enforced in the S3 client as defense in depth.

### 12.2 Upload Token Format

The upload token should be explicit and cluster-safe:

```text
base64url(json(claims)) + "." + base64url(hmac_sha256(secret, claims))
```

Claims should include:

- tenant / keyspace
- staging inode ID
- target path hash
- expected parent inode
- expected prior generation
- upload ID
- target version / bundle ID
- nonce
- expires_at

The signing secret must be cluster-wide, not per-process ephemeral state.

Replay is safe only because publish is guarded by lifecycle state and CAS.

## 13. Rollout and Restart Rules

### 13.1 No-S3 Behavior

The finalized routing function is explicit:

- `InlineBlob` remains available below the inline threshold
- above the inline threshold, fs9 requires S3-backed object storage and rejects the write if S3 is disabled

### 13.2 Rollout Rule

Rollout requires:

- a fresh TiKV fs9 keyspace
- a fresh S3 prefix
- one db9-server restart before reacquiring fs9 if the same keyspace name is recreated into a new `fs_instance_id`

### 13.3 Local Spool Rule

Local spool / journal paths must include:

- keyspace
- storage-format version
- `fs_instance_id`

This prevents stale local recovery state from being replayed under a different filesystem instance or an older prototype format.

## 14. Validation Spikes

Before implementation, run focused spikes:

1. **Multipart throughput spike**
   - MinIO first, then real S3
   - validate multipart upload and parallel range GET
2. **InlineBlob vs PackEntry vs reject-without-S3 benchmark**
   - 1 KiB / 4 KiB / 32 KiB / 64 KiB / 128 KiB
   - cold vs hot cache
   - directory-local vs random
3. **Bundle churn benchmark**
   - repeated overwrite of small files
   - validate “do not immediately pack hot mutable files”
4. **Lifecycle failpoint / recovery spike**
   - fail after upload but before TiKV publish
   - fail after TiKV publish but before old-data GC
   - verify convergence
5. **FUSE compatibility spike**
   - object read size guard vs presigned read-only fast path
   - pack-entry mutation behavior
6. **Multi-node correctness spike**
   - two db9-server nodes against one tenant
   - verify TiKV-authoritative metadata, cache loss tolerance, and failover visibility

## 15. Performance Claim Discipline

The large-file target is credible only with presigned multipart upload/download.

For small files, performance claims should be stated more carefully:

- hot small-file reads with bundle-cache locality can improve dramatically
- bulk small-file writes can improve dramatically with bounded batch + async pack flush
- cold random reads spread across many unrelated bundles will improve, but should not be sold as the same order-of-magnitude gain as hot/locality-friendly cases

## 16. Phasing

### Phase 1: Server Storage Primitives

- add `DataRef` variants: `InlineBlob`, `PackEntry`, `Object`, `StagingPages`
- add `_fs_B` and `_fs_L`
- add S3 client with `put/get/head/delete/get_range/get_object_stream`
- add pack spool, pack manifest model, and bundle GC
- add explicit no-S3 rejection above the inline threshold

### Phase 2: Protocol and Fast Client Paths

- add `CreateUpload`, `PresignPart`, `CompleteUpload`, `AbortUpload`, `PrepareDownload`
- add `BatchStat` and bounded `BatchWrite`
- add `storage` / `sealed` fields to metadata responses
- large file `db9 fs cp`: direct multipart upload/download
- small file bulk path: bundle-aware batch upload
- add WS connection pool in `db9-backend`

### Phase 3: Guardrails and Rollout

- split or tune read budgets for S3-backed reads
- ship size guard or read-only fast path for object-backed FUSE reads
- document fresh-keyspace / fresh-prefix rollout
- reject backend reacquire until restart when a keyspace is recreated into a new fs instance

### Phase 4: FUSE v2

- handle-based protocol
- block-level reads/writes
- local writeback cache
- precise `flush` / `fsync` semantics
- object/pack mutation policy beyond full replacement

## 17. Final Recommendations

The merged design direction is:

1. Keep **authoritative metadata in TiKV**.
2. Use **S3 direct objects** for large files.
3. Use **immutable S3 packfiles** for sealed small files.
4. Keep **tiny mutable files inline** in TiKV.
5. Use `_fs_L` as the only transitional lifecycle source of truth.
6. Use **hidden staging + atomic publish + CAS token** for long-lived uploads.
7. Make the pre-Phase-4 **FUSE boundary explicit** instead of pretending S3 objects behave like mutable pagefs files.

This version is materially stronger than a simple “pagefs -> S3” rewrite because it addresses:

- small-file S3 latency
- multi-node metadata correctness
- transactional publish races
- explicit durable-format versioning
- existing WS and FUSE implementation constraints
