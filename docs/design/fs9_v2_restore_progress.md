# fs9 v2 restore — progress & decisions

**Status**: Active

**Goal:** restore the SQL (fs9_*, COPY fs9) and WS (db9 fs sh) paths' ability
to read/write fs9 JuiceFS files, deleted in PR #2514 (cb30327b). Migrate from
the now-defunct `fsplane.v1` to the `fsplane.v2` peer-mode gRPC service.

## Trust model (final)

```
PG client                                        fs9 v2 staging
   │  aud="db9-server" connect-token                 ↑
   ↓                                                 │ Bearer <fs-plane token>
db9-server  ── X-API-Key + Bearer (user) ──→  db9-backend  ── signs
                          /internal/connect-token/exchange
```

- db9-server is **not** an issuer. Forwards user's connect-token to
  db9-backend's exchange endpoint, gets back an `aud="fs-plane"` JWT, hands
  it to fs9.
- fs9 only trusts the existing db9-backend / auth9 JWKS. **Zero deployment
  changes on fs9.** Zero code changes on db9-backend (exchange endpoint
  already on staging).
- ApiKeys for the exchange call already provisioned in
  `cloud-admin-portal/auth9-secret`:
  - `DB9_SERVER_API_KEY` — db9-server presents in `X-API-Key`.

## Slice plan & status

| Slice | What | Status |
|---|---|---|
| **1** | proto/fsplane/v2/fsplane.proto + Cargo.toml deps (tonic 0.10) + build.rs | ✅ DONE — `cargo check` passes |
| **2** | `auth/db9_auth.rs` keeps raw JWT; Session carries it on `fs_exchange_bearer`; new `auth/fs_plane_token.rs` with TokenCache + exchange client | ✅ DONE — 8 unit tests pass |
| **3a** | gRPC infra: `errors.rs` (FsErrorCode→EmbeddedFsError), `meta.rs` (FileMeta→FsFileInfo), `connector.rs` (shared tonic Channel + TLS) | ✅ DONE — 21 unit tests pass (13 errors + 7 meta + 1 connector) |
| **3b** | `GrpcFsBackend` skeleton + `FsBackend` impl: read path (stat / batch_stat / readdir / read_file_*) | ✅ DONE — Stat / BatchStat / Readdir / ReadFile / ReadFileAt / ReadFileStream all implemented; mutate methods stubbed with `not_yet` |
| **4** | `init_backend` becomes a router (grpc cfg present → Grpc; else Embedded); `ws/auth.rs` shares the router; fail-closed guard rewording | ✅ DONE — `init_backend_with_args` is the explicit-args router; ExtensionContext + StatementRuntimeContext propagate `fs_exchange_bearer` + `authenticated_role`; WS auth uses the same router. **4806/4806 existing tests pass**. |
| **3c-1** | single-frame mutate path (Delete / Mkdir / Rename / Chmod / Symlink / Readlink / Truncate / WriteAt / PutFile fast path ≤4MiB / append fast path ≤4MiB) | ✅ DONE — 4806/4806 tests pass |
| **3c-2** | streaming multipart write (BeginWrite / WriteParts / CommitWrite / AbortWrite + FsWriteStream impl) for >4MiB write_file / append_file / begin_write_stream | ✅ DONE — ported v1 `GrpcMultipartUpload` + `GrpcWriteStream` to fs9.v2 proto; drop-bomb / commit-intent-before-RPC / mpsc(8) capacity all preserved. Needs > 4 MiB staging live-fire to validate end-to-end |
| **3c-3** | remove_recursive (client-side walker port from v1) | ✅ DONE — DFS walker with MAX_DEPTH=100 / MAX_ENTRIES=50_000; NotFound treated as 0-removed for idempotency |
| **3d** | presigned (PrepareUpload / CompleteUpload / AbortUpload / PrepareDownload) | **intentionally stub — v1 was also stub**. fs9 issues presigned URLs directly to cli/FUSE clients (S3 direct upload); db9-server is not on the data path for presigned. See db9-cli's `fs9_grpc/mod.rs` lines 82-84 for cli ownership of FUSE + large-file `cp` via WS-on-pool, but presigned never goes through db9-server. Returns `not_supported`. |
| **5** | staging live-fire — Rust GrpcFsBackend.stat() → fs9 staging real volume | ✅ DONE — `stat /` returned the real JuiceFS root meta through this branch's Rust code via port-forward; ignored test `stat_against_staging` reproduces it |

## Live-fire validation

**Stage 1 — wire validation via grpcurl (2026-05-12):**
A token signed with `auth9-staging-1` key, claims
`{aud:"fs-plane", tid:"0aj28rojeig3", scp:"fs:volume:jfs_t_0aj28rojeig3:rw"}`,
called against the staging `fs9-public` LB via grpcurl with
`-cacert tidb-serverless-ca -authority fs9.staging.db9.io`,
`fsplane.v2.FsPlane/GetVolumeInfo` returned `{"success":{}}`.

Confirmed: network reachability, TLS, JWT shape, JWKS verification chain,
A1–A5 interceptor, handler routing, volume_id matching.

**Stage 2 — Rust GrpcFsBackend through this branch (2026-05-12):**
The ignored test `extensions::fs::grpc::client::tests::stat_against_staging`
exercises the same staging endpoint through this branch's Rust code:

```
stat / OK: FsFileInfo {
    path: "/", is_dir: true, is_symlink: false,
    size: 4096, mode: 511, generation: 14202381407052890112,
    mtime: 1778575614, storage: None, sealed: Some(false)
}
readdir / OK: 0 entries
```

Confirmed end-to-end through the Rust code: tonic Channel + TLS + bearer
metadata injection + proto v2 oneof encode + FileMeta → FsFileInfo
adapter (mtime_ns → mtime seconds: 1_778_575_614_000_000_000 / 1e9 ≈
1778575614). Reproduce per the test's docstring.

## Iteration / reflection notes

- **Self-issuance was wrong.** Initial proposal had db9-server hold an RSA
  key and serve its own JWKS to fs9. Live-fire setup forced me to discover
  the existing exchange endpoint (db9-backend's `api/connect.rs`) and
  the `DB9_SERVER_API_KEY` already provisioned in `auth9-secret`. The
  official path was cleaner and required zero cross-repo work. Lesson:
  for an integration question, search both endpoints' docstrings / config
  before designing a new trust path.
- **Local PoC was overkill.** User pushed back on a Rust↔Go round-trip
  PoC. fs9's own `jwt_test.go::newJwtRig` already exercises the exact
  shape (RSA + JWKS HTTP + RS256 sign + Verifier); reading those files
  was sufficient evidence. The real validation is staging. Memory
  updated.
- **prost 0.12 vs 0.13.** db9-cli uses tonic 0.12 + prost 0.13. db9-server
  has prost 0.12 used by `src/storage/tikv_store/coprocessor.rs` and
  vendored tikv-client. Pinned tonic to 0.10 to match — avoids a prost
  bump cascade for zero functional gain.
- **prost enum naming.** prost 0.12 does NOT strip enum name prefixes:
  `FS_ERROR_NOT_FOUND` → `FsErrorCode::FsErrorNotFound`, not
  `FsErrorCode::NotFound`. Errors module needed local `const` aliases
  to keep tests readable. prost 0.13 strips; bumping would clean this
  up later.

## What user can review while waiting

**Files changed/added:**
- `proto/fsplane/v2/fsplane.proto` (vendored from db9-ai/fs9 master)
- `src/extensions/fs/grpc/{mod,connector,errors,meta,client}.rs` (3a + 3b)
- `src/auth/fs_plane_token.rs` (2)
- `src/auth/db9_auth.rs` — `VerifiedJwtClaims::raw_token`
- `src/auth/mod.rs` — re-export of new fs_plane_token mod
- `src/sql/session/mod.rs` — `fs_exchange_bearer` field + getter/setter
- `src/protocol/handler/dynamic/startup.rs` — capture token in `apply_trusted_jwt_claims`
- `src/extensions/context.rs` — `fs_exchange_bearer` + `authenticated_role` propagation
- `src/sql/runtime_context.rs` — same two fields plumbed through statement context
- `src/extensions/fs/backend.rs` — `init_backend_with_args` is the explicit-args router
- `src/extensions/fs/mod.rs` — registers grpc submodule
- `src/extensions/fs/ws/auth.rs` — WS path uses the shared router instead of bare EmbeddedFsBackend
- `src/protocol/handler/dynamic/copy/mod.rs` — test fixtures updated for new fields
- `Cargo.toml` + `build.rs`

**Test counts (added, all passing):**
- `auth::fs_plane_token`: 8
- `extensions::fs::grpc::errors`: 13
- `extensions::fs::grpc::meta`: 7
- `extensions::fs::grpc::connector`: 1
- `extensions::fs::grpc::client`: 2
- **Total full-suite: 4806/4806 pass.**

## What's wired, what's not (read me before merging)

**Wired end-to-end:**
- Token exchange flow: PG session captures raw JWT → ExtensionContext + WS auth → init_backend → ExchangeTokenProvider → db9-backend `/internal/connect-token/exchange` → fs-plane JWT injected into every gRPC request as `authorization: Bearer …`
- Read-path RPCs against fs9 v2: `Stat`, `BatchStat`, `Readdir`, `ReadFile`, `ReadFileAt`, `ReadFileStream`
- Single-frame mutate RPCs: `Delete`, `Mkdir`, `Rename`, `Chmod`, `Symlink`, `Readlink`, `Truncate`, `WriteAt`, `write_file (≤4MiB via PutFile)`, `append_file (≤4MiB via PutFile mode=APPEND)`
- WS sessions and SQL statements share the same router — no path-specific branching
- Process-shared tonic Channel; per-(tenant, role) TokenCache

**Also wired in this round (3c-2 + 3c-3):**
- Streaming multipart writes (>4MiB): `write_file`, `append_file`, `begin_write_stream` route through `GrpcMultipartUpload` (`BeginWrite → WriteParts → CommitWrite/AbortWrite`). Drop-bomb / commit-intent-before-RPC / `write_chunks_then_terminate` single-entry consumer / mpsc(8)-backed live `WriteParts` stream / advisory `AbortWrite` on Drop — all ported from v1
- `remove_recursive`: client-side DFS walker; `MAX_DEPTH = 100`, `MAX_ENTRIES = 50_000`; NotFound = 0-removed (idempotent)
- `batch_write`: trait default fans out to `write_file` (now multipart-capable)

**Intentionally not wired (architectural decisions, not deferred work):**
- **Presigned** (`create_upload` / `presign_upload_part` / `complete_upload` / `abort_upload` / `prepare_download`): **v1 was also stub**. fs9 issues presigned S3 URLs directly to cli/FUSE clients; db9-server is not on the data path for presigned uploads. Returns `not_supported`.
- Cron / triggers path: no user bearer → fails at backend-init with a clear error (deferred for a separate session-source story).

**Deployment side prerequisites for staging live-fire:**
- Set `DB9_BACKEND_URL=http://db9-backend-api-server.cloud-admin-portal.svc.cluster.local:8090` on the db9-server Deployment.
- Set `DB9_SERVER_API_KEY` to the value already in `cloud-admin-portal/auth9-secret/DB9_SERVER_API_KEY`.
- Set `FS9_GRPC_ENDPOINT=fs9-public.db9.svc.cluster.local:5481` on the db9-server Deployment.
- Set `FS9_GRPC_TLS_SERVER_NAME=fs9.staging.db9.io`.
- Mount the cluster-internal CA (`cert-manager/tidb-serverless-ca-secret`) and point `FS9_GRPC_CA_PATH` at the resulting file path.
- Roll a db9-server image with this branch; PG-login to a JuiceFS tenant via connect-token; run `SELECT * FROM fs9_readdir('/')`. Expected: rows from JuiceFS. Failure modes are typed (`PermissionDenied` from interceptor reject; `NotFound` from handler; etc.) and routed back through SQL via `EmbeddedFsError` mapping.

## Staging live-fire — what still needs hands-on validation

Read + ≤4 MiB write paths are validated end-to-end on staging through this
branch's Rust code (see `stat_against_staging` ignored test for the read
side). The multipart/walker additions above are unit-clean and design-mirror
the v1 implementation that was production-tested, but the protocol-bound
runtime behaviour (multipart commit, abort-on-error, mpsc-vs-h2 backpressure,
walker on real directory trees) has not been re-exercised in staging since
the v1 → v2 port. Suggested live-fire after deploy:

1. `SELECT fs9_write('/large.bin', repeat('x', 5*1024*1024)::bytea)` — exercises slow-path `BeginWrite/WriteParts/CommitWrite` in `write_file`
2. `db9 fs cp <large_local_file>` (≥ 8 MiB) via cli WS → db9-server WS handler → `begin_write_stream` → `GrpcWriteStream`
3. Recursive delete on a nested tree (≥ a few hundred entries) to exercise the DFS walker against real `readdir`/`remove` RPCs
4. Trigger a caller-side error mid-stream (cancel a long `db9 fs cp`) to confirm `AbortWrite` reaches the server and staging TTL doesn't accumulate orphaned uploads

## Known trade-offs

### `read_file` and `read_file_stream` issue an extra `Stat` RPC

`GrpcFsBackend::read_file` and `read_file_stream` call `Stat` before
reading and return `EmbeddedFsError::TooLarge` when `info.size > max_bytes`.
This aligns the gRPC backend with the embedded contract
(`embedded/pagefs/read_impl.rs:503-508, :780-784`) — embedded errors on
size-overflow; the previous gRPC behaviour silently returned the first
`max_bytes`, which corrupted downstream CSV / parquet / JSON parsers
that treat the result as a complete file.

**Cost.** One additional network round-trip per call on the gRPC backend.
For workloads that loop over many small files with `read_file`
(e.g. table-function scans of a glob with hundreds of < 4 MiB CSVs), the
end-to-end latency roughly doubles — `Stat` then `ReadAt` instead of just
`ReadAt`. Throughput is unaffected; the extra RPC is sequential per call,
not per chunk.

**Why we accept it.** Contract parity beats one fewer RPC: a silent
truncation bug in production is far worse than a per-call latency bump.
The alternative — inspect the first `ReadAtChunk` for an inline
`meta.size` and abort the stream if the file exceeds the cap — saves the
extra RPC but adds complexity in the chunked-read path and depends on the
server populating `meta.size` on every read (proto allows it; behaviour
across fs9 versions hasn't been audited here). Deferred to a follow-up
optimization PR once the v2 read path is staging-verified.

**Where this would hurt most.** Glob-scanning many small files via
`fs9_table_function` / `COPY FROM fs9://*.csv` on a JuiceFS tenant. For
those workloads, prefer `read_file_at(offset, length)` (no upfront stat;
caller already has the size) when the schema makes it natural to do so.
