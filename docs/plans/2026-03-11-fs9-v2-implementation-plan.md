# fs9 v2 Implementation Plan

Date: 2026-03-11  
Scope: fs9 v2 final design rollout (`TiKV metadata + S3 objects + packfiles`)  
Goal: turn the final storage design into an executable implementation sequence with clear milestones, dependency order, and acceptance gates.

Tracking / references:

- Design discussion: `c4pt0r/db9-server#1752` (final consolidated comment id: `4039365600`)
- Server execution tracker (M0 spikes): `c4pt0r/db9-server#1757`
- Backend epic: `c4pt0r/db9-backend#296`
- Local design doc: `docs/design/29_fs9_v2_tikv_metadata_s3_packfiles.md`
- Local Spike A write-up: `docs/plans/2026-03-11-fs9-v2-m0-spike-a-s3-throughput.md`
- Local Spike B write-up: `docs/plans/2026-03-11-fs9-v2-m0-spike-b-tikv-inline-batch.md`
- Local Spike C write-up: `docs/plans/2026-03-11-fs9-v2-m0-spike-c-lifecycle-recovery.md`
- Local Spike D write-up: `docs/plans/2026-03-11-fs9-v2-m0-spike-d-fuse-object-policy.md`

## 0. Finalized Rollout Contract (2026-03-12)

This section is the current implementation contract for fs9 v2. Earlier prototype notes below are historical unless they agree with this contract.

- fs9 v2 now has a **frozen durable schema revision**. `FS9_STORAGE_FORMAT_VERSION = 4` is the only supported metadata format for the current implementation.
- Rollout is **fresh keyspace + fresh S3 prefix only**. Older fs9-v2 prototype keyspaces are rejected instead of being auto-migrated.
- Published file storage classes are only `InlineBlob`, `PackEntry`, and `Object`. `_fs_P{inode}:{page}` is staging-only internal state, not a published storage class.
- There is **no TiKV pagefs fallback** for large files in this finalized contract. If S3 is disabled, files above the inline threshold are rejected.
- db9-server binds a keyspace to one persisted `fs_instance_id` for the lifetime of the process. If the same keyspace name is recreated into a different fs instance while db9-server stays alive, **restart db9-server before further fs9 access**.
- Bootstrap / backend acquire is the only supported contract gate for format, object-store binding, and process-local keyspace identity. Serving requests use the already-bound runtime state and do not coordinate instance changes on the hot path.
- Hidden mutators are fenced by durable per-record `fs_instance_id` ownership. Background maintenance also keeps a low-frequency self-stop probe so stale loops stand down promptly after an instance replacement.
- Abort / GC / recovery are **internal cleanup paths**, not serving paths. They must remain independent from serving-path validity checks.
- Local pack spool state is namespaced by **keyspace + storage-format version + fs_instance_id** so stale local journals from older revisions are ignored structurally.
- Malformed local pack journals are quarantined as non-authoritative helper-state corruption; they must not block TiKV/S3 maintenance.

Operational note:

- Restart is only required for operator-driven keyspace recreation / rebinding, for example deleting and recreating the fs9 metadata for the same keyspace name, or reusing the same keyspace name with a fresh S3 prefix / bucket while `db9-server` is still running.
- Ordinary fs9 reads and writes against the same bound filesystem instance do **not** require restart. Using S3 as the data plane by itself is not the trigger.

## 1. Core Execution Strategy

The implementation order should be:

1. **Validate the hard assumptions first**
2. **Land metadata/lifecycle scaffolding with minimal behavior change**
3. **Ship the large-file object path before the packfile path**
4. **Add packfiles only after object/lifecycle correctness is stable**
5. **Treat FUSE v2 as a separate project, not something hidden inside Phase 1**

The most important sequencing decision is this:

> Do **not** start with the full packfile system.
> First build a correct vertical slice for `DataRef + lifecycle + InlineBlob + Object + explicit reject-without-S3`.

Reason:

- `PackEntry` introduces the most moving parts: local spool, immutable bundle format, bundle manifest, bundle GC, and more recovery states.
- `Object` already unlocks the main large-file throughput win.
- `InlineBlob` already improves tiny mutable files and lets us establish the new metadata model safely.
- Once `DataRef` and `_fs_L` are real and stable, `PackEntry` becomes an additive extension instead of a big-bang rewrite.

## 2. Delivery Principle

Every phase must satisfy these constraints:

- finalized schema changes must bump the fs9 storage format version
- unsupported prototype keyspaces must fail fast at bootstrap instead of partially decoding
- new lifecycle semantics use `_fs_L` as the only non-clean source of truth
- object-backed files fail partial mutation explicitly instead of silently degrading
- serving paths must not re-read `_fs_S` or perform instance fencing on the TiKV hot path
- each milestone has one measurable benefit and one clear rollback boundary

## 3. Phase Overview

| Phase | Theme | Main Outcome |
|------|-------|--------------|
| M0 | Validation spikes | freeze thresholds and risky product decisions |
| M1 | Frozen v3 metadata contract | `DataRef`, `_fs_L`, object-store binding, fs instance identity |
| M2 | `InlineBlob` + strict no-S3 reject | first shipped mutable storage path with final routing semantics |
| M3 | `Object` backend in server | correct object semantics, streaming, lifecycle, GC |
| M4 | Presigned multipart + CLI fast path | first major throughput win for large files |
| M5 | `PackEntry` / bundle path | solve small-file + S3 latency correctly |
| M6 | Client optimization follow-up | batch APIs, WS pool, bounded prefetch |
| M7 | FUSE v2 | separate handle-based redesign |

## 4. Milestone Detail

### M0. Validation Spikes and Decision Freeze

This is the real first step. Do not start feature coding before the risky choices are validated.

#### M0.1 Spike A: multipart throughput

Goal:

- get an initial measured baseline for multipart throughput on real S3 from the dev cluster environment
- optionally repeat the same benchmark using presigned URLs if we need to quantify presign overhead

Output:

- recommended part size
- concurrency level
- retry policy
- whether download should prefer direct GET + range GET for CLI

Status:

- completed initial measurement on 2026-03-11 (dev EKS IRSA pod -> S3). See `docs/plans/2026-03-11-fs9-v2-m0-spike-a-s3-throughput.md`.

#### M0.2 Spike B: inline threshold benchmark

Goal:

- validate `InlineBlob` threshold against TiKV behavior

Measure:

- 1 KiB / 4 KiB / 32 KiB / 64 KiB / 128 KiB
- single write latency
- batched write latency
- TiKV transaction size pressure
- compaction / write amplification signals if measurable

Output:

- initial `FS9_INLINE_MAX`
- initial `FS9_BATCH_WRITE_MAX_TOTAL_BYTES`

Status:

- completed initial measurement on 2026-03-11 (dev cluster TiKV optimistic txn). See `docs/plans/2026-03-11-fs9-v2-m0-spike-b-tikv-inline-batch.md`.
- initial defaults to carry into implementation (subject to later tuning):
  - `FS9_INLINE_MAX = 64KiB`
  - `FS9_BATCH_WRITE_MAX_TOTAL_BYTES = 4MiB`

#### M0.3 Spike C: lifecycle / recovery failpoints

Goal:

- validate the `_fs_L` state machine before wide implementation

Inject failures:

- after upload or bundle write, before TiKV publish
- after TiKV publish, before old-data cleanup
- during GC cleanup

Output:

- recovery rules
- CAS points
- retry count defaults

Status:

- completed initial scenarios on 2026-03-11 (dev TiKV + S3). See `docs/plans/2026-03-11-fs9-v2-m0-spike-c-lifecycle-recovery.md`.
- initial defaults to carry into implementation:
  - optimistic commit retry: 5 attempts (base backoff 30ms)
  - HEAD/existence verification retry: 5 attempts (base backoff 50ms)

#### M0.4 Spike D: FUSE object-read policy

Goal:

- decide between:
  - size guard
  - read-only presigned fast path

This decision should be frozen before object-backed files are widely exposed to the current mount path.

Status:

- completed initial dev validation on 2026-03-11. See `docs/plans/2026-03-11-fs9-v2-m0-spike-d-fuse-object-policy.md`.
- frozen decision: **size guard** for pre-Phase-4. Whole-file APIs remain bounded and must not become the implicit object access path.

#### Exit Gate for M0

- `FS9_INLINE_MAX` chosen
- multipart part size and concurrency chosen
- lifecycle retry policy chosen
- pre-Phase-4 FUSE object-read policy chosen

### M1. Metadata and Lifecycle Scaffolding

This milestone freezes the durable metadata contract and removes prototype ambiguity before wider rollout.

#### Scope

In `db9-server`:

- `src/extensions/fs/embedded/types.rs`
  - add `DataRef`
  - persist `fs_instance_id` and object-store binding in the superblock
  - set the finalized storage format version
- `src/extensions/fs/embedded/keys.rs`
  - add `_fs_B`
  - add `_fs_L`
- new lifecycle module
  - `FileLifecycle`
  - GC scan helpers
- `src/extensions/fs/backend.rs`
  - prepare `FsFileInfo` extension fields
- `src/extensions/fs/embedded/mod.rs`
  - bind backend acquire to one `fs_instance_id` per keyspace and start maintenance self-stop probing

#### What should ship in M1

- finalized superblock format rejects older prototype revisions cleanly
- `_fs_L` exists as the sole transitional state model
- GC / recovery framework exists for the finalized storage classes only
- local spool state is namespaced by storage format and fs instance

#### What should not ship in M1

- no presigned multipart API yet
- no packfile path yet
- no FUSE semantic change yet

#### Exit Gate for M1

- storage-format validation tests pass
- lifecycle state transition tests pass
- serving paths do not add per-request TiKV probes
- startup GC / recovery scan is safe and idempotent for the finalized format

### M2. `InlineBlob` and Strict No-S3 Reject

This is the first real storage behavior win and the safest new path to ship.

#### Scope

- implement `_fs_B{inode}` storage
- route tiny mutable files to `InlineBlob`
- if S3 is not enabled and size exceeds inline threshold, reject explicitly

#### Server work

- `read_file`, `read_file_at`, `write_file`, `write_file_at`, `append_file`, `truncate`
  route correctly for `InlineBlob`
- keep full current semantics for mutable files
- extend `stat`/`readdir` responses with `storage` / `sealed`

#### Why M2 exists before `Object`

- proves `DataRef` routing works
- proves metadata evolution works
- gives a low-risk production win
- preserves current mutation semantics, unlike `Object`

#### Exit Gate for M2

- inline files work end-to-end over SQL + WS
- old clients tolerate new `stat` fields
- no-S3 rejection branch is explicit and tested

### M3. `Object` Backend in Server

This milestone adds correct object semantics before exposing the direct client path.

#### Scope

- add `FsS3Client`
  - `put_object`
  - `get_object`
  - `get_range`
  - `head_object`
  - `delete_object`
  - `get_object_stream`
- implement hidden staging + atomic publish
- implement object lifecycle and GC
- make object mutation boundary explicit

#### Server semantics

- `read_file`: bounded server-side object GET
- `read_file_at`: S3 range GET
- `read_file_stream`: true backend stream via `get_object_stream()`
- `write_file`: full replacement only
- `write_file_at` / `append_file` / `truncate`: explicit `EINVAL`

#### Important implementation detail

Existing WS streaming write must map to `Object` correctly:

- for large object-routed writes, server should use multipart-style object upload on the server side
- it must not degrade into “buffer all chunks in memory, then PUT one giant object”

#### Exit Gate for M3

- object-backed files are correct and recoverable
- commit retry / HEAD retry / GC jitter are implemented
- `COPY FROM fs9://` is either truly supported via streaming or explicitly left deferred

### M4. Presigned Multipart + CLI Fast Path

This is the first milestone that should deliver the headline large-file throughput gain.

#### Server work

In `db9-server` WS protocol:

- `CreateUpload`
- `PresignPart`
- `CompleteUpload`
- `AbortUpload`
- `PrepareDownload`

#### Client work

In `db9-backend/db9-cli`:

- `src/fssh/protocol.rs`
- `src/fssh/client.rs`
- `src/commands/filesystem.rs`

Implement:

- multipart upload for large `db9 fs cp`
- direct download / range GET for large file download

#### Product outcome

- `db9 fs cp` large file path bypasses db9-server data plane
- first meaningful throughput milestone can be benchmarked and published

#### Exit Gate for M4

- upload and download throughput target for large files is validated in E2E
- overwrite race handling is correct
- tokens are cluster-safe

### M5. `PackEntry` / Bundle Path

This milestone solves the small-file + S3 problem properly.

#### Scope

- add `PackEntry` to `DataRef`
- define immutable bundle format
- add local spool/journal
- add bundle manifest metadata in TiKV
- add bundle read path with range GET
- add bundle GC / compaction skeleton

#### Server work

- batch small-file ingest path
- spool flush worker
- per-file mapping commit
- bundle cache hooks

#### Client work

- use bounded `BatchWrite` for many small files
- use `BatchStat` for metadata-heavy copy/list workflows

#### Important product rule

Do **not** pack every small file blindly:

- hot mutable files still prefer `InlineBlob`
- sealed/bulk-uploaded small files prefer `PackEntry`

#### Exit Gate for M5

- bulk small-file upload on S3 is materially better than object-per-file
- recovery from partial bundle flush is correct
- bundle compaction story is at least structurally valid

### M6. Client Optimization Follow-Up

This is where we remove obvious client-side bottlenecks without waiting for FUSE v2.

#### In `db9-backend`

- replace single serialized WS client with a 4-8 connection pool
- add bounded directory-local prefetch if justified
- add `BatchInlineRead` / bounded prefetch only if metrics show it is worth it

#### Why this is separate from M5

- it is orthogonal to server storage correctness
- it improves RTT-bound workloads even before FUSE v2

#### Exit Gate for M6

- small-file RTT-bound throughput improves in realistic mount/copy workloads
- no correctness regression from concurrency

### M7. FUSE v2

This is a separate project, not a stretch goal hidden in earlier milestones.

#### Scope

- handle-based protocol
- block-level read/write
- local writeback cache
- correct `flush` / `fsync` behavior
- formal mutation policy for `Object` and `PackEntry`

#### Rule

Do not block M1-M6 on FUSE v2.

## 5. Recommended PR Slicing

The clean review sequence should look like this:

1. PR1: inode / `DataRef` / key scaffolding / compatibility tests
2. PR2: `_fs_L` lifecycle model + GC framework
3. PR3: `InlineBlob` read/write path
4. PR4: `FsFileInfo` extension + client compatibility
5. PR5: S3 client + object read path + `get_object_stream`
6. PR6: object publish / hidden staging / CAS / retry / GC
7. PR7: WS multipart protocol
8. PR8: `db9 fs cp` large-file direct path
9. PR9: `PackEntry` + bundle format + spool
10. PR10: bundle ingest/read path
11. PR11: batch APIs + client-side bulk upload optimization
12. PR12: WS connection pool / bounded prefetch

This keeps correctness-heavy storage changes separate from performance-heavy client changes.

## 6. Ownership Split

### `db9-server`

- metadata model
- lifecycle / GC
- `InlineBlob`
- `Object`
- `PackEntry`
- protocol changes
- auth / token / CAS

### `db9-backend`

- CLI `db9 fs cp`
- WS client extensions
- connection pool
- bounded prefetch / batch client use
- eventual FUSE integration work

## 7. Test and Acceptance Matrix

Each milestone should be accepted only if these layers are covered:

1. unit tests
2. targeted fs9 integration tests
3. server + CLI E2E in `deploy/e2e`
4. explicit crash/recovery or restart tests for lifecycle-heavy phases

Minimum critical tests:

- storage-format version rejection
- no-S3 reject behavior above the inline threshold
- object overwrite conflict / CAS failure
- stale upload cleanup
- bundle partial flush recovery
- old client compatibility with new `stat` fields

## 8. What Should Ship First

If the team wants the most pragmatic first production milestone, it should be:

1. **M0**
2. **M1**
3. **M2**
4. **M3**
5. **M4**

That gives:

- a correct storage model
- low-risk small-file improvement for tiny mutable files
- large-file throughput breakthrough
- no dependence on packfiles to claim the first win

Then:

6. **M5**
7. **M6**
8. **M7**

This is the right order because it de-risks correctness first, unlocks the most visible performance gain second, and adds the more complex small-file packfile path only after the storage model is proven.

## 9. Concrete Recommendation

The next action should be:

1. run **M0 validation spikes**
2. freeze thresholds and pre-Phase-4 FUSE object policy
3. open **PR1 = DataRef + `_fs_L` + compatibility-safe inode evolution**

That is the correct “first step” because it establishes the foundation for every later milestone without prematurely committing the codebase to either the object path or the packfile path.
