# 33 — fs-plane: JuiceFS Client Proxy Behind db9 Metadata

**Status**: Draft

Date: 2026-04-13

Issue: [db9-ai/db9-backend#1063](https://github.com/db9-ai/db9-backend/issues/1063)

Audience: db9-server / fs9 / db9-cli / FUSE maintainers

Related:
- `29_fs9_v2_tikv_metadata_s3_packfiles.md` — authoritative metadata contract (TiKV)

---

## 0. Central Architectural Decisions

### 0.1 Why not direct-mount (v3.5 approach)

The v3.5 design proposed per-tenant JuiceFS FUSE mounts accessed directly by db9-server. This approach is rejected because db9-server is a compute/control layer where tenants and requests move between pods. It must not depend on node-local filesystem state:

- FUSE mounts — lifecycle management (mount/umount on tenant migration, orphan cleanup, reaper)
- local JuiceFS cache — node-local state that creates affinity
- mount health — a JuiceFS mount process crash causes FUSE fd breakage, which hangs all db9-server I/O on that mount
- local open-handle state — not portable across pods

### 0.2 What fs-plane actually is

fs-plane is a **thin gRPC proxy** that wraps the JuiceFS Go client. It holds runtime state (per-volume `*fs.FileSystem` instances with JuiceFS sessions and caches) but no business-logic state — no generation tracking, no idempotency cache, no tenant metadata. All it does is:

```
gRPC request → translate to JuiceFS Go client API call → return result
```

There is **no FUSE mount**. The Go process embeds the JuiceFS client directly, connecting to JuiceFS metadata store (TiKV) and object store (S3) without any kernel-level filesystem layer. No mount point, no kernel roundtrip, no mount lifecycle.

All business logic — namespace resolution, authz, CAS/generation, idempotency, routing, lifecycle, event emission — stays in db9-server. The proxy has no business logic of its own.

### 0.3 Ownership split

> **db9 owns namespace and routing. JuiceFS owns file content.**

- db9 TiKV: directory tree, path→inode mapping, `DataRef` (routing pointer), authz, tenant isolation, events
- JuiceFS: file content bytes, current size, current mtime — JuiceFS is the **file-level content authority** for `FsPlane` files
- `DataRef::FsPlane` is a routing pointer ("this file lives in JuiceFS at this path"), not a metadata cache
- For sealed files (`PackEntry`, `Object`), db9 TiKV remains authoritative (unchanged from design 29)
- db9-server continues to own the full WS/SQL/FUSE protocol surface

---

## 1. Problem

fs9 v2 has three storage forms:

| Form | Size range | Sub-file mutation |
|---|---|---|
| `InlineBlob` | ≤ 64 KB | Full: `write_at`, `append`, `truncate` |
| `PackEntry` | 64 KB – 1 MB (sealed) | None — full replacement only |
| `Object` | ≥ 1 MB (sealed) | None — full replacement only |

For files > 64 KB, `write_at`, `append`, and `truncate` either require full-file read-modify-write or are rejected with `EINVAL`.

The append-delta mechanism (`_fs_AD`) mitigates this for small appends to Object-backed files, but is capped at 64 KB per delta and 1024 deltas per file. It requires compaction on `prepare_download` and adds read-path complexity (concatenate base object + all deltas).

**The core gap: there is no efficient sub-file mutation path for files > 64 KB.**

---

## 2. Architecture

```
db9-cli (FUSE / cp / SQL)
  |
  |  WS control plane + presigned HTTP data plane (unchanged)
  |
db9-server (Rust)
  |
  +-- TiKV metadata plane (AUTHORITATIVE -- unchanged)
  |     inode, DataRef, generation, _fs_L, _fs_M, directory tree
  |
  +-- S3 data plane (unchanged for PackEntry + Object)
  |
  +-- fs-plane (NEW -- thin proxy, no FUSE mount)
        gRPC over Unix socket
        Go process embedding JuiceFS client directly
        |
        JuiceFS metadata (TiKV keyspace) + S3 (object store)
        No kernel FUSE layer. No mount point. No mount lifecycle.
```

### 2.1 New DataRef variant

```rust
enum DataRef {
    None,
    InlineBlob,          // existing — to be retired (see §11)
    PackEntry { ... },   // existing — sealed S3 bundle slice
    Object { ... },      // existing — sealed S3 object
    FsPlane {            // NEW — JuiceFS is content authority
        volume_id: String,
        internal_path: String,  // opaque, assigned by db9-server
        // NO size, checksum, generation — these live in JuiceFS, not db9 TiKV
    },
}
```

`DataRef::FsPlane` is a **routing pointer**, not a metadata cache. It says "go ask the proxy for this file's content." File size, mtime, and content state are authoritative in JuiceFS, not in db9 TiKV.

### 2.2 Routing policy

**All mutable files go through JuiceFS.** No size-based split, no InlineBlob for small files, no promotion/demotion logic.

| File class | Route | Content authority |
|---|---|---|
| All mutable files (any size) | `DataRef::FsPlane` | JuiceFS (via proxy) |
| Sealed bulk files | `PackEntry` (S3 bundle) | S3 (unchanged) |
| Sealed large files | `Object` (S3) | S3 (unchanged) |

One data path for all mutable content. JuiceFS handles small files efficiently (inline data in its own TiKV metadata for tiny files, chunk caching for reads).

**Sealing**: when a `FsPlane` file is closed without mutation for a configurable period, db9-server may seal it to `Object` or `PackEntry` for cost efficiency. This is a `DataRef` type change in db9 TiKV (read from proxy, write to S3, update pointer).

---

## 3. fs-plane gRPC API

The proxy's API is a **thin translation of POSIX operations** to gRPC. db9-server is the only caller. The proxy owns no business logic — it translates, calls the JuiceFS Go client, and returns.

```protobuf
service FsPlane {
  // Data reads
  rpc ReadAt(ReadAtRequest) returns (stream ReadAtChunk);
  rpc Stat(StatRequest) returns (StatResponse);

  // Data mutations -- proxy translates to JuiceFS client calls
  rpc WriteFile(stream WriteFileChunk) returns (WriteResponse);
  rpc WriteAt(WriteAtRequest) returns (WriteResponse);
  rpc Append(stream AppendChunk) returns (WriteResponse);
  rpc Truncate(TruncateRequest) returns (WriteResponse);

  // Lifecycle
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  rpc Sync(SyncRequest) returns (SyncResponse);  // JuiceFS Flush

  // Health
  rpc Ping(PingRequest) returns (PingResponse);
}
```

### 3.1 What the proxy does vs. what db9-server does

The proxy is deliberately dumb. All intelligence stays in db9-server:

| Concern | Owner | Where |
|---|---|---|
| Routing (which DataRef) | db9-server | Decides FsPlane vs Object vs PackEntry; proxy never sees this |
| Path resolution | db9-server | Resolves virtual path to opaque `internal_path`; proxy receives opaque path |
| Authz | db9-server | Verified before calling proxy |
| Tenant isolation | db9-server + proxy | db9-server generates inode-based path; proxy enforces `volume_id` scoping |
| Idempotency / replay safety | db9-server | Append dedup via request_id cache, **before** calling proxy |
| Event emission / watch | db9-server | After proxy returns success, emits advisory event (no TiKV update for content mutations) |
| Durability guarantee | proxy | Calls JuiceFS `Flush` before returning success |

### 3.2 Message shape

```protobuf
message WriteAtRequest {
  string volume_id = 1;           // selects JuiceFS client instance
  string internal_path = 2;       // opaque, assigned by db9-server
  uint64 offset = 3;
  bytes data = 4;
}

message WriteResponse {
  uint64 new_size = 1;
  bytes checksum = 2;             // full-file checksum after mutation
}
```

No `expected_generation`, no `request_id`, no `tenant_id` in the proxy API. These are db9-server concerns handled before the gRPC call.

### 3.3 What the proxy does per RPC

Each RPC is a direct translation to `pkg/fs.FileSystem` calls (the path-based API used by JuiceFS S3 Gateway and WebDAV server).

Note: `pkg/fs.FileSystem.Open` uses JuiceFS-internal flags (`MODE_MASK_R=4`, `MODE_MASK_W=2`), not POSIX `O_*` constants. `Create` has `O_EXCL` semantics (fails if file exists). There is no `O_APPEND` flag — append is implemented via `Seek` + `Write`.

### 3.3.1 Minimal lock principle

Most JuiceFS `pkg/fs` operations (Rename, Truncate, Delete, Mkdir, Symlink, Chmod) are single TiKV metadata transactions — internally atomic, no proxy-level locking needed. No official JuiceFS client (FUSE, S3 Gateway, Java SDK, Python SDK, WebDAV) adds custom locking for these operations.

**Only `Append` needs proxy-level locking.** `SeekEnd` uses `f.info.Size()` cached at `Open` time (`fs.go:1299`). Two concurrent Appends both see the same stale EOF and overwrite each other's data. The proxy serializes Appends via JuiceFS metadata `Flock(F_WRLCK)` on the target inode, then reopens the handle under lock to refresh the cached size.

**`WriteFile` uses temp+rename** for atomic replace semantics. Data is written to a temp file (`.fs9_tmp_<pid>_<ns>`), flushed, then atomically renamed to the target path. For `create_only=true`, `Rename` uses `RenameNoReplace` flag (returns `EEXIST` if target appeared concurrently). No Flock needed — readers see either old or new file, never partial.

**Flock owner uniqueness across replicas:** Each `Volume` instance seeds `lockOwner` with a random 48-bit base at initialization (`init.go`). The counter occupies the low 16 bits. This ensures Flock owner IDs are globally unique across proxy pods — without this, two replicas would generate the same owner sequence and JuiceFS would treat same-owner requests as "already held" instead of blocking.

| gRPC RPC | Implementation |
|---|---|
| `ReadAt(path, offset, len)` | `Open(R)` → `Pread` → stream chunks → `Close` |
| `WriteAt(path, offset, data)` | `Open(W)` (or `Create` if ENOENT) → `Pwrite` → `Flush` → `Close`. No lock — Pwrite is position-independent. |
| `Append(path, stream)` | **Flock(inode)** → close stale handle → invalidate cache → reopen for fresh Size → `SeekEnd` → `Write` → `Flush` → `Close` → unlock. If file missing and `createMissing`, creates before locking. |
| `WriteFile(path, stream)` | Write to temp file → `Flush` → `Close` → atomic `Rename(temp, target)`. Overwrite uses flags=0; create_only uses `RenameNoReplace` (flags=1). |
| `Truncate(path, size)` | Direct `FS.Truncate(path, size)` — single atomic JuiceFS transaction. |
| `Delete(path)` | Direct `FS.Delete(path)` — single atomic JuiceFS transaction. |
| `Rename(old, new)` | Direct `FS.Rename(old, new, 0)` — single atomic JuiceFS transaction. |
| `Mkdir(path, mode, recursive)` | Direct `FS.Mkdir`/`FS.MkdirAll` — single atomic JuiceFS transaction. |
| `Symlink(target, link)` | Direct `FS.Symlink` — single atomic JuiceFS transaction. |
| `Readlink(path)` | Direct `FS.Readlink` — read-only, no lock. |
| `Chmod(path, mode)` | `Open(R)` → `File.Chmod` → `Close` — single atomic `SetAttr` transaction. |
| `Stat(path)` | `FS.Stat(path)` → returns `FileStat` with size, mtime, mode. |
| `Flush(path)` | No-op in Phase 1 (every mutation already flushes before returning). |

### 3.4 Durability semantics

Every mutating RPC (`WriteAt`, `Append`, `WriteFile`, `Truncate`) calls `file.Flush(ctx)` on the write handle before returning success. `Flush` commits buffered data to JuiceFS metadata + object store. The proxy never returns success on unflushed data.

**`Sync` RPC**: JuiceFS `Fsync` only flushes the calling handle's write buffer (`f.wdata`). Opening a file read-only and calling `Fsync` is a no-op — there is no write buffer to flush. Therefore, `Sync` as a "flush all pending writes by other handles" primitive does not exist in JuiceFS. Since every mutating RPC already flushes before returning, `Sync` is redundant in the default configuration. If a future buffered-write mode is added (where mutating RPCs return before flush), the proxy must track open write handles and `Sync` must flush all of them.

Phase 1 supports **flush-per-mutation only**. Requests with `flush=false` are rejected with `INVALID_ARGUMENT`; accepting them without a handle-tracking design would create a false durability contract. Buffered writes are Phase 2 scope and require a separate design for dirty-handle ownership, crash recovery, `Sync` semantics, and backpressure.

### 3.5 Edge case semantics

The proxy does not implement edge case handling — JuiceFS follows POSIX semantics natively:

| Operation | Condition | JuiceFS POSIX behavior |
|---|---|---|
| `WriteAt(offset=50, data)` | File size = 40 | Zero-fills bytes 40–49, writes `data` at 50 |
| `Truncate(size > current)` | Any | Zero-fills extension (`ftruncate(2)`) |
| `Truncate(size == current)` | Any | No-op |
| `Append(data)` | File size = 0 | Writes at offset 0 |

Unlike `InlineBlob`, FsPlane has no 64 KB ceiling. The `inline_max_bytes` guard and the `_fs_AD` delta-block mechanism are TiKV-specific and do not apply.

### 3.6 Idempotency contract (db9-server side)

Since the proxy has no dedup logic, idempotency is db9-server's responsibility:

| Mutator | Naturally idempotent? | db9-server handling |
|---|---|---|
| `WriteAt(offset, data)` | Yes — same offset + same data = same bytes | Safe to retry without dedup |
| `WriteFile(data)` | Yes — full replacement | Safe to retry |
| `Truncate(size)` | Yes — same size is no-op | Safe to retry |
| **`Append(data)`** | **No** — replay doubles data | **db9-server must dedup via request_id before calling proxy** |

For `Append`, db9-server maintains a short-lived dedup cache keyed by `(keyspace, inode_id, request_id)`. On first call: execute and cache result. On replay: return cached result without calling proxy. This mirrors the `complete_upload` flow's `Published`-phase early-return pattern.

---

## 4. Ownership Boundary

db9-server and JuiceFS own different things. There is **one content authority per file**, not two:

| Concern | Authority | Store |
|---|---|---|
| Directory tree (path → inode) | db9-server | TiKV (db9 keyspace) |
| DataRef (routing pointer: where does this file live?) | db9-server | TiKV (db9 keyspace) |
| Auth / tenant isolation | db9-server | TiKV + PG |
| Watch / event emission | db9-server | TiKV (advisory, not transactional) |
| **File content** (bytes, size, mtime) | **JuiceFS** | JuiceFS metadata (TiKV `jfs_t_*`) + S3 |

For `FsPlane` files, **JuiceFS is the file-level content authority.** db9 does not cache or duplicate file size, mtime, checksum, or generation in its own TiKV keyspace. `DataRef::FsPlane` is a routing pointer, not a metadata store.

For `PackEntry` / `Object` files (sealed), db9 TiKV remains authoritative (unchanged from design 29).

### 4.1 Mutation sequence — single phase

When db9-server performs a mutation on a `FsPlane` file:

```
1. db9-server verifies authz, resolves path → inode, reads DataRef::FsPlane
2. db9-server calls proxy WriteAt / Append / Truncate
3. Proxy executes mutation on JuiceFS (Append: Flock → reopen → write; WriteFile: temp+rename; others: direct call), flushes, returns success
4. Done. No db9 TiKV metadata update needed.
5. db9-server emits advisory watch event (best-effort, not atomic with mutation)
```

**No two-phase commit. No rollback problem.** The mutation is a single operation against JuiceFS. If it succeeds, the file is changed. If it fails, the file is unchanged. There is no intermediate state where "JuiceFS succeeded but db9 TiKV failed."

### 4.2 When db9 TiKV changes for FsPlane files

db9 TiKV only changes for structural operations — not per-mutation:

| Operation | db9 TiKV changes? | What changes |
|---|---|---|
| `write_at` / `append` / `truncate` / `read_at` | **No** | Only JuiceFS changes |
| `stat` | **No** | Proxy call to JuiceFS for current size/mtime |
| File creation (new inode) | Yes | Create inode + `DataRef::FsPlane { volume_id, internal_path }` |
| File deletion (`unlink`) | Yes | Mark inode lifecycle as deleting, call proxy `Delete`, then remove/clear inode via retryable GC |
| Rename | Yes | Update directory entries only |
| Sealing (`FsPlane` → `Object`) | Yes | Read from proxy → write to S3 → update `DataRef` type |

### 4.3 Stat for FsPlane files

`stat` on a `FsPlane` file requires a proxy call to get current size and mtime from JuiceFS. This adds ~0.1ms (Unix socket gRPC) compared to a TiKV read for `InlineBlob` files. Acceptable tradeoff for a clean ownership model.

### 4.4 Watch events and FsPlane

The existing watch system (`watch_subscribe`) emits events with `seq`, `path`, `generation`, `size`, and `is_dir`. For FsPlane files, content mutations do not update db9 TiKV, which creates three semantic changes:

**Generation**: `inode.generation` is still authoritative for db9-owned namespace and sealed-file metadata. For `FsPlane` file content, db9 TiKV generation is not bumped per mutation; `stat` responses may use `generation=0` to mean "no db9 content generation is available; use `mtime`/size plus a fresh read for validation." Watch event `generation` for FsPlane content is advisory only. It must not be used as a cluster-stable CAS token unless db9-server later persists a generation/epoch in TiKV.

**Size**: Events must report the post-mutation size. db9-server uses the `new_size` returned by the proxy, not a stale TiKV value. This is passed through from the proxy response in step 3 of the mutation sequence.

**Event loss**: If the proxy succeeds (step 3) but db9-server crashes before emitting the event (step 5), the event is lost. **This is acceptable and explicitly documented.** The v3.5 design principle §0.4 already states: "Events are advisory, not authoritative. Mutations succeed even if event emission fails. Consumers must verify current state via read operations." No change from existing contract.

---

## 5. Concurrency Model

**For FsPlane files, content mutations do not touch db9 TiKV** (§4.1). This means TiKV OCC does not provide per-mutation serialization — there is no db9 TiKV transaction per `write_at` / `append` / `truncate`.

Instead, serialization relies on **JuiceFS's internal atomicity** for most operations, with proxy-level locking only where JuiceFS's `pkg/fs` API is not atomic:

1. **JuiceFS metadata transactions**: `Rename`, `Truncate`, `Delete`, `Mkdir`, `Symlink`, `Chmod` are each a single TiKV transaction inside JuiceFS (`m.txn()` in `tkv.go`). No proxy lock needed — concurrent calls are serialized by TiKV OCC. This is the same approach used by the JuiceFS FUSE mount, S3 Gateway, Java SDK, and Python SDK.
2. **Append inode Flock**: The sole exception. `Append` uses `Flock(F_WRLCK)` on the target inode because `SeekEnd` depends on `f.info.Size()` cached at `Open` time. Without Flock, two concurrent Appends see the same stale EOF and overwrite each other. The Flock is stored in JuiceFS TiKV metadata, so it works across proxy replicas. After locking, the proxy closes the stale handle, invalidates the local entry/attr cache, and reopens to get a fresh `Size`.
3. **WriteFile atomic replace**: `WriteFile` writes to a temp file and atomically renames to the target. No Flock needed — the rename is a single TiKV transaction. Concurrent `WriteFile` + `Append` to the same path: Append writes to the old inode (standard POSIX unlink-while-open behavior), WriteFile's rename swaps the directory entry. Last-writer-wins at the path level.
4. **WriteAt direct Pwrite**: `Pwrite` is position-independent — the offset is caller-provided, not derived from cached file state. No lock needed. The per-inode `fileWriter` singleton inside JuiceFS (`writer.go:297`) serializes concurrent writes to the same inode.
5. **JuiceFS close-to-open consistency**: Every mutating RPC calls `Flush` and closes the write handle before returning success. Subsequent opens observe committed state.

**Flock owner uniqueness**: Each proxy instance seeds `lockOwner` with a random 48-bit base at volume initialization. This prevents owner ID collisions across replicas (without this, two pods generate the same owner sequence and Flock treats same-owner as "already held" → no blocking).

TiKV OCC still applies for **structural operations** that change db9 TiKV (file create, delete, rename, sealing) — these go through the existing transaction + conflict retry path.

The proxy's concurrency control is deliberately minimal: only Append uses Flock. All other operations rely on JuiceFS's internal atomicity guarantees.

### 5.1 Sealing and delete barriers

Sealing (`FsPlane` → `Object`/`PackEntry`) is not protected by the existing append-delta compaction OCC pattern unless db9 first prevents new FsPlane writes to that inode. FsPlane content mutations do not update the db9 `DataRef`, so a CAS on `DataRef` alone cannot detect a concurrent JuiceFS `WriteAt` or `Append` that lands while sealing is reading bytes.

Required sealing sequence:

1. In db9 TiKV, transition the inode to a `Sealing`/write-blocked lifecycle state with OCC.
2. After that transition succeeds, reject or wait same-inode FsPlane mutators at db9-server.
3. Read the stable file content through fs-plane, write the sealed S3 object/pack entry, and atomically update `DataRef`.
4. Clear the lifecycle state or finish deletion of the FsPlane backing file through retryable GC.

Delete uses the same lifecycle principle: tombstone first in db9 TiKV, then call proxy `Delete`, then clear the inode/reference. If the process crashes after tombstoning but before proxy deletion, background GC retries with the retained `internal_path`. Delete-inode-first is not safe because it can leave invisible JuiceFS content with no db9 pointer.

### 5.2 JuiceFS internal compaction and GC transparency

JuiceFS performs two kinds of background work:

1. **Slice compaction**: merges fragmented internal chunk slices into fewer larger objects in the object store. This rewrites JuiceFS-internal storage keys (`chunks/0/0/xxx`) but does not modify the POSIX file path. The file at a given JuiceFS path reads identically before, during, and after compaction.

2. **Object GC**: deletes orphaned slice objects no longer referenced by any file. Cannot affect live files because JuiceFS metadata still holds references to current (post-compaction) slices.

**Neither operation changes file-level paths.** `DataRef::FsPlane { internal_path }` remains valid across any JuiceFS internal compaction. No notification protocol from fs-plane to db9-server is needed.

The only data movement that changes `DataRef` is **sealing** (§2.2): background promotion of idle `FsPlane` files to `Object` or `PackEntry`. This is initiated by db9-server's own maintenance loop (analogous to `run_background_maintenance_once` and `compact_append_deltas`), not by JuiceFS. db9-server atomically updates TiKV to replace `DataRef::FsPlane` with `DataRef::Object` — the same pattern as existing append-delta compaction. fs-plane is a passive target; it never autonomously mutates content or paths.

---

## 6. Tenant Isolation — Security Model

### 6.1 Trust boundary

The trust boundary is **db9-server**, not TiKV, not S3, not the proxy. Tenants connect via PostgreSQL wire protocol and never have direct access to TiKV or S3. All TiKV-connected processes (db9-server, fs-plane proxy, worker engine) are trusted infrastructure components.

This is the same security model as db9's existing TiKV-based storage: TiKV API V2 keyspace isolation is **client-side key prefix encoding**, not server-enforced access control. Any process with TiKV/PD network access and valid TLS credentials can bypass keyspace boundaries. Security depends on tenants never reaching TiKV directly.

### 6.2 Three isolation layers

**Layer 1: db9-server namespace resolution (existing)**

db9-server resolves virtual paths to TiKV inodes. The `internal_path` passed to fs-plane is a db9-server-generated opaque identifier (e.g., `{tenant_id}/{db9_id}/{inode_id}`), never a user-supplied path. User path traversal attacks stop at the db9-server layer.

**Layer 2: Per-tenant JuiceFS volume (structural isolation)**

Each tenant gets its own JuiceFS volume with:
- Separate TiKV keyspace (`jfs_t_{tenant_id}`) — transactions scoped by client-side key prefix
- Separate S3 key prefix (`jfs_t_{tenant_id}/chunks/...`) — the volume `Name` field determines the prefix
- Separate `*fs.FileSystem` instance in the proxy — one client per volume, no shared state

A bug in the proxy's `volume_id` routing is the primary risk: if the proxy dispatches a request to the wrong `*fs.FileSystem` instance, it reads/writes the wrong tenant's data. The volume name prefix is a client-side naming convention, not an S3 access control boundary.

**Layer 3: Infrastructure hardening (production)**

| Mechanism | What it protects | Status |
|---|---|---|
| mTLS on TiKV | Prevents unauthorized TiKV clients | Required for production |
| Network isolation (TiKV/PD not tenant-accessible) | Prevents direct keyspace bypass | Required for production |
| Per-tenant IAM roles scoped to S3 prefix | Prevents cross-tenant S3 access even on proxy bug | Recommended for production |
| Separate S3 buckets per tenant | Strongest S3 isolation, eliminates prefix-sharing risk | Optional, higher operational cost |

### 6.3 What is NOT a security boundary

- TiKV keyspace — client-side prefix, not server-enforced
- S3 key prefix — naming convention, not access control (unless backed by IAM policy)
- fs-plane path confinement — defense in depth, not primary boundary

---

## 7. fs9 Protocol Parity

Since fs-plane is a data mutation service (not a filesystem), most fs9 WS operations do not touch it:

| fs9 WS operation | Touches fs-plane? | Notes |
|---|---|---|
| `stat` | If `DataRef::FsPlane` | Proxy `Stat` for size/mtime (§4.3). TiKV for directory metadata. |
| `readdir` | Partially | db9 TiKV remains authoritative for directory entries. For `FsPlane` child files, db9-server populates live size/mtime from proxy `Stat` or a future `BatchStat`. |
| `mkdir`, `symlink`, `readlink`, `chmod` | No | TiKV metadata only |
| `rename` | No | TiKV directory entries only; `internal_path` uses inode ID |
| `unlink` / `rm` | If `DataRef::FsPlane` | Mark db9 inode deleting, call proxy `Delete`, then remove/clear inode via retryable GC (§4.2) |
| `read` / `read_at` | If `DataRef::FsPlane` | -> `FsPlane.ReadAt` |
| `write` (full file) | If routing to FsPlane | -> `FsPlane.WriteFile` |
| `pwrite` | If `DataRef::FsPlane` | -> `FsPlane.WriteAt` |
| `append` | If `DataRef::FsPlane` | -> `FsPlane.Append` |
| `truncate` | If `DataRef::FsPlane` | -> `FsPlane.Truncate` |
| `create_upload` / presigned | No change | S3 Object path, unchanged |
| `batch_stat` | If entries include `DataRef::FsPlane` | Proxy `Stat` per FsPlane entry in Phase 1; future `BatchStat` to avoid N+1 |
| `watch_subscribe` | No | Advisory event system |
| `prepare_download` | If `DataRef::FsPlane` | Streaming read from proxy |

**Rename handling**: `internal_path` uses `{tenant_id}/{db9_id}/{inode_id}`, not the user-visible path. Renames only update TiKV directory entries and inode metadata — no fs-plane call needed.

The current whole-volume gRPC backend POC may call proxy `Readdir` directly because the POC routes an entire tenant to JuiceFS. That is not the final hybrid contract in this document. In the final contract, db9 owns the namespace and directory tree; fs-plane owns file bytes plus live file size/mtime for `DataRef::FsPlane` files.

**No client-side changes in Phase 1.** The existing `write`, `pwrite`, `append`, `truncate`, `read` WS operations work unchanged from the client's perspective. db9-server internally routes to fs-plane based on `DataRef` type.

### 7.1 Batch operations and prepare_download

**N+1 problem for listings and batch operations**: `readdir`, `batch_stat`, and `batch_inline_read` currently do batched parallel TiKV reads in one round trip. For FsPlane files, each file whose live size/mtime is needed requires a proxy stat call in Phase 1. This is correct but can regress large listings.

If this becomes a bottleneck, add batch RPCs to the proxy:

```protobuf
rpc BatchStat(BatchStatRequest) returns (BatchStatResponse);   // N paths in, N stats out
rpc BatchReadAt(BatchReadAtRequest) returns (stream BatchReadAtChunk);  // N range reads
```

These are optimization RPCs, not required for correctness. Phase 1 uses N serial calls. Phase 2 adds batch RPCs if POC latency data shows the N+1 is a problem.

**`prepare_download` for FsPlane files**: The existing `prepare_download` returns a presigned S3 GET URL for `Object`-backed files, allowing the client to download directly from S3 bypassing db9-server. For FsPlane files, there is no S3 object to presign — content is in JuiceFS.

Three options:

| Option | Mechanism | Tradeoff |
|---|---|---|
| **Streaming read** (Phase 1) | `prepare_download` returns `DownloadMode::Streaming { size }`. Client reads via WS streaming. db9-server proxies from FsPlane. | Simple, but db9-server is in the data path for large reads |
| **Seal first** | `prepare_download` triggers sealing (FsPlane → Object), then returns presigned URL. | Client gets direct S3 access, but sealing takes time and changes DataRef |
| **Error** | Return `ENOSYS` for FsPlane files. Client uses `read` / `read_at` instead. | Simplest, but degrades CLI `db9 fs cp` for large FsPlane files |

**Phase 1 recommendation**: streaming read. This matches the existing behavior for `InlineBlob` / `PackEntry` files (which also return `Streaming` mode). No client changes needed.

### 7.2 FUSE FlushStrategy interaction

The FUSE client's `FlushStrategy` (`file_handle.rs`) decides between `AppendDelta` and `FullWrite` based on two guards:

- **Guard A**: `delta <= 64 KB` — the append payload size
- **Guard B**: `flushed_size <= 64 KB` — the file must not already exceed the inline threshold

Both guards were designed for the InlineBlob→Object boundary. With all mutable files on FsPlane, both guards are overly conservative — FsPlane accepts appends of any size to files of any size.

**Phase 1 (no client changes)**: Guard B causes `FullWrite` fallback for all files > 64 KB. The fallback is correct (server routes `write` through `FsPlane.WriteFile`) but suboptimal — it does a full rewrite for a small append.

**Phase 2 (client optimization)**: Server returns `append_mutable: true` in stat response for `DataRef::FsPlane` files. Client checks this flag to skip both guards. Clean capability-based approach — no size threshold in the client at all.

The 1024 append-delta limit (`MAX_APPEND_DELTAS`) and 64 KB per-delta guard are specific to the `DataRef::Object` delta-block mechanism in TiKV (`_fs_AD` keys). They do not apply to FsPlane.

---

## 8. Deployment and Operations

### 8.1 Sidecar model (POC)

```
Pod:
  db9-server (Rust)  <-gRPC Unix socket->  fs-plane proxy (Go)
                                              |
                                         JuiceFS Go client (embedded)
                                              |
                                         TiKV (JuiceFS metadata) + S3 (data)
                                         No FUSE. No mount. No kernel layer.
```

The Go process starts, initializes JuiceFS client instances (one per tenant volume), and serves gRPC. No `juicefs mount` command, no `/mnt/jfs/...` paths, no MountManager, no orphan cleanup.

The API is designed as a remote service boundary so it can later move to a separate deployment with N replicas.

### 8.2 Health and liveness

- `FsPlane.Ping` RPC with 500 ms timeout. db9-server marks fs-plane unhealthy after 3 consecutive failures.
- When unhealthy: all operations on `FsPlane`-routed files return `EIO`. No fallback — the proxy is the only path to FsPlane content.
- db9-server emits a metric and alert on fs-plane health state transitions.

### 8.3 Graceful shutdown ordering

1. db9-server receives SIGTERM, stops accepting new connections
2. db9-server drains in-flight fs9 requests (existing behavior)
3. In Phase 1 there are no dirty paths after a successful mutating RPC because every mutation flushes before returning
4. db9-server closes gRPC connection
5. fs-plane receives connection close, closes JuiceFS sessions and exits

### 8.4 Memory budget

fs-plane Go process target: ≤ 512 MB RSS. JuiceFS client cache is bounded via chunk store config (`CacheSize`, `BufferSize`), but the dominant risk is per-volume fixed overhead from `*fs.FileSystem`, metadata session state, readers/writers, goroutines, object-store client state, and Go heap fragmentation.

Phase 1 defaults are intentionally conservative:

| Setting | Default | Rationale |
|---|---:|---|
| `FS9_CACHE_SIZE_MB` / `--cache-size` | 16 MB per volume | Keeps chunk cache predictable under the 512 MB pod limit |
| `FS9_MAX_VOLUMES` / `--max-volumes` | 5 active volumes | Bounds worst-case resident state before RSS measurements justify a higher cap |

The volume manager must maintain an LRU of initialized volumes and evict only entries with zero active requests. If all active volumes are in use and the cap is reached, `InitVolume` fails fast rather than exceeding the memory budget. Re-initializing an evicted volume is expected to add cold-start latency (target: sub-second; measure in POC).

POC acceptance criteria:

1. Measure steady-state RSS for 1, 5, and 10 active volumes with representative file counts.
2. Verify the configured `max-volumes` keeps P99 RSS below 512 MB with 20% headroom.
3. Measure cold-start latency after LRU eviction.
4. Verify Go GC pause P99 is < 5 ms under sustained mixed reads/writes.

### 8.5 JuiceFS metadata backend

Use TiKV (already deployed) as JuiceFS metadata backend to reduce operational surface. Each tenant's JuiceFS volume gets a dedicated TiKV keyspace (`jfs_t_{tenant_id}`).

The db9-ai/juicefs fork (commit `f244448`) adds `?keyspace=` support for TiKV API V2 native keyspaces. The meta URL format is:

```
tikv://pd:2379?keyspace=jfs_t_{tenant_id}&gc-interval=0
```

This provides TiKV server-level keyspace isolation (client-side key prefix encoding via API V2), which is stronger than the upstream JuiceFS URL-path prefix mechanism. Both can coexist — `?keyspace=` for TiKV-level isolation, URL path for application-level sub-prefixing within a keyspace. This design uses `?keyspace=` only (no URL path prefix).

---

## 9. JuiceFS Data Model, COW, and Isolation

This section describes how JuiceFS maps files to S3 objects, why copy-on-write makes backup/clone/sharing possible, and what isolation guarantees hold between volumes.

### 9.0 JuiceFS internal data model: files → chunks → slices → S3 objects

```
File (inode 42, size 150 MB)
  |
  +-- Chunk 0 (bytes 0–63 MB)
  |     +-- Slice {id=101, offset=0, size=64MB}  → S3: {vol}/chunks/0/0/101_0_4194304
  |     +-- Slice {id=205, offset=32MB, size=4MB} → S3: {vol}/chunks/0/0/205_0_4194304
  |           (overwrote bytes 32-36MB — old slice 101 still covers 0-32MB and 36-64MB)
  |
  +-- Chunk 1 (bytes 64–127 MB)
  |     +-- Slice {id=102, offset=0, size=64MB}  → S3: {vol}/chunks/0/0/102_0_4194304
  |
  +-- Chunk 2 (bytes 128–150 MB)
        +-- Slice {id=103, offset=0, size=22MB}  → S3: {vol}/chunks/0/0/103_0_4194304
```

Key concepts:
- **File** = metadata inode in TiKV. Each file is divided into **chunks** (64 MB each).
- **Chunk** = an ordered list of **slices** stored as a byte buffer in TiKV at key `A{inode}C{chunk_index}`.
- **Slice** = a contiguous byte range backed by one or more S3 objects (blocks). Each slice has a unique `id` allocated from the per-volume `nextChunk` counter.
- **S3 object key** = `{volume_name}/chunks/{id/1000000}/{id/1000}/{id}_{block_index}_{block_size}`.

The volume `Name` field (set at `juicefs format` time, stored in the `setting` key in TiKV) is the **sole S3 prefix**. All chunks for a volume go under `{Name}/chunks/...`.

### 9.1 Copy-on-write: slices are immutable

JuiceFS writes are fully copy-on-write at the slice level:

1. **Write**: creates a **new slice** (new S3 object), appends it to the chunk's slice list. Old slices are never modified.
2. **Read**: resolves overlapping slices by picking the newest one at each byte range. Multiple slices may cover the same range — the last one wins.
3. **Compaction**: merges multiple slices into one new slice (new S3 object). Old slices become "delayed slices" (retained for `TrashDays` before deletion).
4. **Delete**: file's chunk entries are removed from TiKV. Slice reference counts are decremented. When a ref goes negative, the S3 object is deleted.

**S3 objects are never modified in place** — only created and deleted. This is the foundational property that makes backup, clone, and sharing work.

### 9.2 Per-volume isolation: metadata + S3 prefix

Each tenant's JuiceFS volume has:

| Layer | Isolation mechanism | What it protects |
|---|---|---|
| TiKV metadata | Separate keyspace (`jfs_t_{tenant_id}`) | Inodes, chunk→slice mappings, slice ref counts, counters |
| S3 data | Separate prefix (`jfs_t_{tenant_id}/chunks/...`) | Chunk objects |
| Slice ID space | Independent `nextChunk` counter per keyspace | No ID collision between volumes |

Two volumes with different `Name` values on the same S3 bucket are fully isolated at the data level — their S3 key paths never overlap, their metadata is in separate TiKV keyspaces, and their slice ID counters are independent.

**Important**: both TiKV keyspace and S3 prefix isolation are **client-side conventions** (see §6). The security boundary is db9-server, not TiKV or S3. A proxy bug that routes to the wrong `*fs.FileSystem` instance would serve the wrong tenant's data.

### 9.3 Clone: shared S3 objects between volumes

When cloning a volume (via TiKV BR keyspace clone), the new keyspace gets an exact copy of all metadata — including the `Format.Name` (S3 prefix) and the `nextChunk` counter. This means:

- Both volumes' metadata points to the **same S3 objects**
- Because slices are immutable (COW), reads from both volumes are safe — neither modifies shared objects
- New writes create new slices with new IDs → new S3 objects → volumes diverge

```
Before clone:
  Volume A: metadata → slices [101, 102, 103] → S3 objects at A/chunks/...

After TiKV BR clone:
  Volume A: metadata → slices [101, 102, 103] → S3: A/chunks/...  (same objects)
  Volume B: metadata → slices [101, 102, 103] → S3: A/chunks/...  (same objects!)

After Volume B writes:
  Volume A: metadata → slices [101, 102, 103]        → S3: A/chunks/...
  Volume B: metadata → slices [101, 102, 103, 50001] → S3: A/chunks/... (50001 is new)
```

**Two hazards must be addressed for this to work safely:**

#### 9.3.1 Hazard 1: Slice ID collision (CRITICAL)

After clone, both keyspaces have `nextChunk` at the same value. Both allocate new slice IDs from the same starting point. Since both write to the same S3 prefix, they would produce S3 objects at the same keys with different content — **silent data corruption**.

**Required fix**: Immediately after clone, before any write, advance the clone's `nextChunk` to a non-overlapping range:

```
original nextChunk = 50000
clone nextChunk    = 50000 + 2^40
```

One `incrBy` operation on the `CnextChunk` key in the cloned keyspace. After this, new slice IDs are guaranteed non-overlapping, so new S3 objects never collide.

#### 9.3.2 Hazard 2: GC deletes shared chunks

The original volume's compaction/GC may delete S3 objects that the clone still references. Original overwrites a file → old slices become delayed → after `TrashDays`, S3 objects deleted → clone's metadata points to missing objects.

**Two strategies based on clone lifetime**:

| Strategy | Mechanism | When to use |
|---|---|---|
| **Shared prefix** (fast clone) | Fix `nextChunk`. `TrashDays` on original protects shared chunks for N days. New writes use non-overlapping IDs — no collision. | Short-lived branches (< TrashDays) |
| **Independent prefix** (full isolation) | Quiesce clone → full S3 chunk copy to new prefix → update clone's `Format.Name` → resume. **Not progressive** — `Format.Name` determines the read path for ALL slices at read time, so changing it before copy completes causes reads to fail. | Long-lived clones, production branches |

This is not just an operational note. Product/API code must enforce one of these policies when clone support is enabled: shared-prefix clones require an expiry shorter than `TrashDays` plus monitoring/cleanup, while long-lived or production clones must use the independent-prefix copy procedure before they are allowed to outlive the source retention window.

**Why progressive cutover doesn't work**: The S3 key prefix is derived from `format.Name` and baked into the `withPrefix` object storage wrapper at client init time (`cmd/format.go:284`). A running proxy does NOT pick up `Name` changes — the `NewReloadableStorage` reload callback only watches `Storage`, `Bucket`, and credentials, not `Name`. However, on proxy restart, the new instance loads the new `Name` and reads from the new prefix — if chunks haven't been fully copied, reads return `NoSuchKey`. The cutover must be: stop proxy → full S3 chunk copy → update `Format.Name` → restart proxy.

#### 9.3.3 Clone procedure (complete)

```
1. TiKV BR backup at ts=T (covers db9 keyspace + jfs_t_* keyspace)
2. TiKV BR restore to new keyspaces (db9_new + jfs_t_new_*)
3. BEFORE any write: fix nextChunk in jfs_t_new_* to non-overlapping range
4. Clone is now usable for reads and writes (shared S3 prefix, non-overlapping IDs)
5. For long-lived clone: quiesce → full S3 chunk copy → update Format.Name → resume
```

#### 9.3.4 Sharing as a product primitive

The COW + clone mechanism enables filesystem sharing between tenants. This is an infrastructure capability — product-level decisions not covered by this design:

| Question | Scope |
|---|---|
| Authorization: who can clone whose volume? | Product / authz layer |
| Cost attribution for shared S3 storage | Billing |
| Revocation: what happens when sharing is revoked? | Force chunk copy or let natural divergence proceed |
| Read-only vs read-write clones | Product policy — read-only avoids the slice ID collision entirely |
| Cross-tenant GC coordination | Operational — TrashDays or background chunk copy |

---

## 10. Backup and Restore

### 10.1 Backup flow

Since db9 metadata and JuiceFS metadata are both in TiKV, **one TiKV BR snapshot at one timestamp covers everything**:

```
TiKV BR backup at ts=T
  → db9 keyspace: inodes, DataRef, _fs_L, _fs_B, directory tree
  → jfs_t_{tenant_id} keyspace: JuiceFS inode table, chunk/slice mapping
  → atomically consistent — same ts, same TiKV cluster
  → S3 chunks: immutable (COW), no copy needed — already there
```

| Data | Storage location | Backup method |
|---|---|---|
| db9 SQL data | TiKV (db9 keyspace) | TiKV BR at ts=T |
| db9 fs metadata (inode, DataRef) | TiKV (db9 keyspace) | TiKV BR at ts=T |
| db9 InlineBlob file content | TiKV (`_fs_B`) | TiKV BR at ts=T |
| JuiceFS metadata (file→chunk mapping) | TiKV (`jfs_t_*` keyspace) | **same TiKV BR at ts=T** |
| JuiceFS data chunks | S3 (immutable, COW) | No copy needed — already there |
| S3 Object / PackEntry content | S3 (immutable) | No copy needed — already there |

**One ts, one operation, all stores covered.** Same mechanism as db9 branching (`BRANCHING.md`), extended to include JuiceFS keyspace. `juicefs dump/load --binary` remains useful as a portable export for cross-cluster migration.

### 10.2 Restore

**In-place restore**: stop writes → TiKV BR restore at ts=T overwriting current keyspaces → orphan chunks reclaimed by GC → resume writes.

**Clone / branch**: see §9.3 for the complete clone procedure (TiKV BR clone + nextChunk fix + lifecycle-based chunk sharing strategy).

### 10.3 GC safety — two independent GC layers

PITR faces two independent GC layers that must both be controlled:

**Layer 1: TiKV MVCC GC (purges old metadata versions)**

JuiceFS's `doCleanupSlices` (runs hourly) calls `client.GC()` to advance TiKV GC safe point to `now - gc-interval` (default 3h). After this, old MVCC versions are purged and `snapshot_ts` reads return `GCTooEarly`.

**Control**: Set `gc-interval=0` in the JuiceFS TiKV meta URL. db9-server manages the safe point centrally (same mechanism as `EXPORT SNAPSHOT`).

**Layer 2: JuiceFS slice GC (deletes S3 chunks)**

JuiceFS GC only looks at the **current/latest** metadata state — it has no awareness of TiKV MVCC. When file overwrites/compaction produce stale slices:
- `TrashDays > 0`: stale slices recorded as "delayed slices", retained for `TrashDays` days before S3 deletion
- `TrashDays == 0`: S3 objects deleted immediately in the compaction transaction

Key fact: **compaction cannot be disabled** — it is triggered inline during reads (≥5 slices) and writes (≥100 slices). `NoBGJob` only disables cleanup goroutines, not compaction.

**Recommended configuration**:

| Config | Effect |
|---|---|
| `gc-interval=0` (TiKV meta URL) | Prevents JuiceFS from advancing TiKV GC safe point |
| `TrashDays=N` (N ≥ PITR window in days) | Stale slices from compaction retained for N days |
| `NoBGJob=false` (default) | Cleanup runs normally, reclaims expired slices after N days |

Do not use `--max-deletes=0` — it blocks ALL S3 deletion (not just concurrency), causing unbounded storage growth. `TrashDays` is the precise control.

PITR window = min(TiKV safe point retention, TrashDays).

### 10.4 Capability / gap decision table

| Capability | Status | Gap |
|---|---|---|
| **PITR** | Available, window-bounded by `TrashDays` + GC safe point. | Config: `gc-interval=0` + `TrashDays >= N` |
| **Consistent cross-store backup** | Available — same snapshot_ts for all stores. | None — same TiKV cluster |
| **Per-tenant isolation** | Available — independent keyspace and safe point. | None |
| **Backup without quiesce** | Available — TiKV snapshot reads don't block writes. | None |
| **Incremental backup** | Feasible — diff metadata between two snapshot_ts. | Not yet implemented |

---

## 11. Migration Path

### Phase 1: POC (this issue)

- New `DataRef::FsPlane` variant (routing pointer only — no size/checksum/generation)
- fs-plane Go proxy with JuiceFS `pkg/fs.FileSystem` embedding
- All new file creates route to `FsPlane` for tenants with fs-plane enabled
- Existing `InlineBlob` files continue to work (read via TiKV, no migration)
- Feature-gated per tenant/keyspace via env var

Phase 1 cannot be enabled for production traffic until these gates pass:

- Same-inode mutators are serialized with a JuiceFS metadata lock, including the stale-open append regression test.
- `flush=false` is rejected until buffered-write durability is designed.
- Delete uses a tombstone/retry lifecycle so proxy deletion failures do not create invisible orphans.
- Sealing uses a db9-side write barrier before reading FsPlane bytes.
- Memory defaults and staging config respect the 512 MB RSS target.
- Latency SLOs reflect flush-per-mutation measurements, not future buffered-mode goals.

### Phase 2: Retire InlineBlob

- Background migration: read `InlineBlob` content from TiKV, write to proxy, update `DataRef` to `FsPlane`
- After migration, remove `InlineBlob` write path — one data path for all mutable files
- `PackEntry` / `Object` unchanged (sealed files)
- Append-delta mechanism (`_fs_AD`) retired — FsPlane handles all appends natively

### Phase 3: Mature

- All mutable file content through JuiceFS
- `InlineBlob` code removed
- Sealing: idle `FsPlane` files compact to `Object` / `PackEntry` for cost
- Evaluate whether to keep JuiceFS or replace with native slice engine behind same gRPC API

---

## 12. JuiceFS Go Client Embedding

The proxy embeds JuiceFS as a Go library, not via FUSE mount. The correct API is `pkg/fs.FileSystem` — the same path-based API used by JuiceFS S3 Gateway (`cmd/gateway.go`) and WebDAV server. This is proven in production and supports multi-instance embedding.

### 12.1 Initialization sequence

Reference implementation: `initForSvc()` in `cmd/gateway.go:246`. Minimal sequence:

```go
import (
	"github.com/juicedata/juicefs/pkg/chunk"
	"github.com/juicedata/juicefs/pkg/fs"
	"github.com/juicedata/juicefs/pkg/meta"
	"github.com/juicedata/juicefs/pkg/vfs"
)

// 1. Create meta client (TiKV, gc-interval=0)
metaConf := meta.DefaultConf()
metaCli := meta.NewClient(
    "tikv://pd:2379?keyspace=jfs_t_xxx&gc-interval=0", metaConf)

// 2. Load volume format
format, _ := metaCli.Load(true)

// 3. Create object storage (S3)
// Either call the JuiceFS reloadable storage helper or maintain an equivalent
// local createStorage implementation that covers decrypt, TLS, sharding,
// prefix, storage class, tiers, and encryption.
blob, _ := createStorage(*format)

// 4. Create chunk store
chunkConf := &chunk.Config{
    BlockSize:  format.BlockSize * 1024,
    Compress:   format.Compression,
    HashPrefix: format.HashPrefix,
}
store := chunk.NewCachedStore(blob, *chunkConf, registerer)

// 5. Register compaction + deletion callbacks
metaCli.OnMsg(meta.DeleteSlice, func(args ...interface{}) error {
    return store.Remove(args[0].(uint64), int(args[1].(uint32)))
})
metaCli.OnMsg(meta.CompactChunk, func(args ...interface{}) error {
    return vfs.Compact(*chunkConf, store,
        args[0].([]meta.Slice), args[1].(uint64), args[2].(uint8))
})

// 6. Create session + FileSystem
metaCli.NewSession(true)
jfs, _ := fs.NewFileSystem(
    &vfs.Config{Meta: metaConf, Format: *format, Chunk: chunkConf},
    metaCli, store, registry)
```

The proxy implementation currently keeps a local `createStorage` copy instead of importing `cmd`. That is acceptable only if it stays behaviorally equivalent to the JuiceFS storage initialization path for credential decrypt, TLS options, sharding, volume prefix, storage class, tiers, and encryption. Credential rotation/reload behavior must be validated separately; importing `cmd.NewReloadableStorage` is not required if the local path has explicit parity tests.

### 12.2 Multi-tenant: multiple volumes in one Go process

The Java SDK (`sdk/java/libjfs/main.go`) proves multiple `*fs.FileSystem` instances can coexist in one process. Each volume gets independent `meta.NewClient` + `chunk.NewCachedStore` + `fs.NewFileSystem` — no shared state conflicts.

The proxy maintains `map[string]*fs.FileSystem` (key = volume_id), initialized/destroyed on demand.

Only global state: TiKV TLS config and logger. Not an issue when all volumes connect to the same TiKV cluster.

### 12.3 Version management

- Pin JuiceFS to a specific commit hash of the db9-ai/juicefs fork
- `pkg/fs.FileSystem` is more stable than `pkg/vfs` (S3 Gateway and Java SDK both depend on it)
- The gRPC boundary ensures JuiceFS internal changes do not affect db9-server

---

## 13. POC Validation Plan

### 13.1 Functional tests

| # | Test | Validates |
|---|---|---|
| 1 | Create 1 MB file via `WriteFile` -> FsPlane. Append 4 KB. Read back. | No full-file rewrite for append |
| 2 | Create 10 MB file. `WriteAt` 4 KB at offset 5 MB. Range read back. | Sub-file random write |
| 3 | Truncate 10 MB file to 1 MB. Verify `Stat` and read-back. | Truncation correctness |
| 4 | Write 60 KB inline file, append 10 KB -> exceeds 64 KB -> promotes to FsPlane. Read back. | Promotion correctness |
| 5 | Kill fs-plane proxy. Verify committed data readable after restart. | Crash recovery |
| 6 | Attempt `internal_path` with `../`, absolute path, null bytes. Verify rejection. | Path confinement |
| 7 | Concurrent reads from two db9-server connections to the same FsPlane file. | Read consistency |
| 8 | Concurrent append streams opened against the same file before either writes. Verify both payloads survive. | Inode lock + stale-handle refresh |
| 9 | Repeat append test with local JuiceFS attr/entry cache enabled on one proxy client. Verify both payloads survive. | Cache invalidation before lock-after-reopen |
| 10 | Two concurrent WriteFile(create_only=true) to same path. Verify exactly one succeeds and the other returns EEXIST (RenameNoReplace). | Atomic create via temp+rename |
| 11 | Send `flush=false` to `WriteAt`, `Append`, and `WriteFile`. Verify `INVALID_ARGUMENT`. | Phase 1 durability contract |
| 12 | Crash between db9 delete tombstone and proxy delete. Verify background GC retries and removes JuiceFS backing file. | Delete orphan prevention |

### 13.2 Latency targets

Phase 1 uses flush-per-mutation. The write latency floor includes JuiceFS metadata work plus object-store PUT latency, so sub-5ms sync writes are not a realistic production target. POC measurements observed `Append(4 KB)` around P50 33 ms with a flush on every mutation; this is the baseline to optimize from.

**Phase 1: flush-per-mutation targets**

| Operation | Payload | P50 target | P99 target |
|---|---|---|---|
| `Append` | 4 KB | < 40 ms | < 120 ms |
| `WriteAt` | 4 KB | < 40 ms | < 120 ms |
| `ReadAt` | 64 KB | < 5 ms | < 20 ms |
| `WriteFile` | 1 MB | < 80 ms | < 250 ms |
| `Ping` | — | < 1 ms | < 5 ms |

**Phase 2: buffered-write aspirational targets**

| Operation | Payload | P50 target | P99 target | Requirement |
|---|---|---|---|---|
| `Append` | 4 KB | < 5 ms | < 20 ms | Requires explicit buffered-write durability design |
| `WriteAt` | 4 KB | < 5 ms | < 20 ms | Requires explicit buffered-write durability design |

If Phase 1 P99 exceeds 2x target, investigate object-store latency, JuiceFS write buffering, lock contention, and gRPC transport overhead separately. Do not enable buffered writes as a latency shortcut without the Phase 2 durability contract from §3.4.

### 13.3 Backup / restore tests

1. Write 100 files to tenant A's volume. Run `juicefs dump --binary`. Verify export succeeds.
2. Modify some of tenant A's files. Load backup metadata into a new keyspace via `juicefs load --binary`. Verify the new keyspace shows file contents from backup time.
3. Verify TrashDays protection: overwrite a file (triggers compaction), dump+load restore within TrashDays window, verify old chunks still readable.
4. Verify `gc-interval=0`: confirm TiKV GC safe point is not advanced while the proxy is running.
5. Verify per-tenant isolation: backing up tenant A does not affect tenant B's reads/writes.

### 13.4 Operational tests

1. Measure proxy Go process RSS at steady state (1000 files, mixed sizes, 5 tenant volumes).
2. Kill proxy mid-write. Verify db9-server detects unhealthy state within 2 s. Verify no data corruption on restart.
3. Go GC pause distribution under sustained write load (target: P99 < 5 ms).

---

## 14. Decision Framework

The POC produces one of three outcomes:

| Outcome | Condition | Next step |
|---|---|---|
| **Adopt JuiceFS fs-plane** | Latency targets met, backup viable, API stable | Phase 2: gradual adoption |
| **Adopt fs-plane boundary, replace engine** | fs-plane abstraction valuable but JuiceFS latency/stability/backup unacceptable | Build native slice engine behind same gRPC API |
| **Reject fs-plane** | gRPC hop latency unacceptable even with native engine, or complexity unjustified | Revisit: extend embedded TiKV storage or explore alternative approach |

---

## 15. sys9 Compatibility

fs-plane should be designed with awareness that it may serve sys9 file (`f`) and stream (`s`) node types in the future, not just db9 database files.

The current namespace model (`{tenant_id}/{db9_id}/{inode_id}`) is db9-centric. To accommodate sys9 selectors (e.g., `/<space>/agents/<name>/state/`), the `internal_path` format should be treated as opaque by fs-plane — db9-server (or sys9) is responsible for mapping its own namespace to an `internal_path`, and fs-plane only enforces `volume_id` confinement without interpreting path semantics.

This requires no API changes. The `volume_id` field already supports routing to different JuiceFS volumes per use case (e.g., a sys9-specific volume if isolation is needed). The key constraint: fs-plane must not embed db9-specific assumptions into path validation beyond tenant confinement.

---

## 16. Open Questions

1. **Write durability latency**: What is the P99 latency of `file.Flush(ctx)`? Is JuiceFS write-back cache (`--writeback`) needed for acceptable latency, and what durability contract would buffered mode expose?
2. **Memory footprint**: What is steady-state RSS per active tenant volume after representative load, and what `max-volumes` value keeps 512 MB pods below the alert threshold?
3. **Sealing implementation**: Should the write-blocking lifecycle state be `DataRef::Sealing`, a separate inode lifecycle enum, or a lock row keyed by inode?
4. **Delete GC implementation**: Should FsPlane delete reuse the existing embedded lifecycle table or add a FsPlane-specific tombstone queue that stores `(volume_id, internal_path)`?
5. **Compaction impact on PITR**: Under high write load, are compaction frequency and delayed slice accumulation rate manageable?
6. **Clone nextChunk safety gate**: After TiKV BR keyspace clone, both volumes share the same `nextChunk` counter and S3 prefix. New writes allocate the same slice IDs → S3 object collision → silent data corruption (§9.3.1). The required `nextChunk` advance is documented but not enforced. The proxy `InitVolume` should detect when `nextChunk` is in the danger range (equal to or close to the source volume's counter) and refuse to open for writes until the counter is advanced. This requires either: (a) passing a `clone_source_next_chunk` hint in `InitVolume`, or (b) the proxy checking a metadata flag set by the clone tool.
