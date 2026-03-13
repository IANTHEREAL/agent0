## fs9 v2 M4: WS Presigned Multipart, Download, and Bounded Batch Protocol

Date: 2026-03-11

This note records local implementation progress for **M4** of fs9 v2:

- WebSocket control-plane operations for direct large-file transfers
- cluster-safe upload/download token model
- bounded batch metadata/write protocol extensions

Design source of truth:

- `c4pt0r/db9-server#1752` consolidated design comment (`4039365600`)
- Local copy: `docs/design/29_fs9_v2_tikv_metadata_s3_packfiles.md`
- Execution plan: `docs/plans/2026-03-11-fs9-v2-implementation-plan.md`
- Tracking issue: `c4pt0r/db9-server#1761`

## Scope

Server-side protocol work only:

- `CreateUpload`
- `PresignPart`
- `CompleteUpload`
- `AbortUpload`
- `PrepareDownload`
- bounded `BatchStat`
- bounded `BatchWrite`

Out of scope for this milestone:

- `db9-cli` / `db9-backend` direct multipart client implementation
- `PackEntry` / bundle ingest path
- FUSE v2 redesign

## Design Constraints

- Keep authoritative publish semantics in TiKV.
- Use exact-key presigned URLs only.
- Keep upload/download tokens explicit and cluster-safe.
- Enforce request bounds for batch operations.
- Do not fake large-file acceleration by routing bulk data back through db9-server.

## Progress

- 2026-03-11: M4 implementation started from `origin/fs9-v2` after `#1760` / PR `#1775` alignment and closure.
- 2026-03-11: Implemented upload token signing / verification in `src/extensions/fs/upload_token.rs` using explicit `base64url(json(claims)).base64url(hmac_sha256(secret, payload))` format with `FS9_UPLOAD_TOKEN_SECRET`.
- 2026-03-11: Extended `FsBackend` and embedded fs object paths for:
  - `CreateUpload`
  - `PresignPart`
  - `CompleteUpload`
  - `AbortUpload`
  - `PrepareDownload`
- 2026-03-11: Kept authoritative reservation / publish semantics in TiKV by extending `_fs_L` lifecycle state with `UploadReservation` carried through `Uploading` -> `Committing` -> publish / cleanup.
- 2026-03-11: Added S3 presign / head support needed by the protocol path:
  - multipart upload creation
  - part presign
  - multipart completion / abort
  - object HEAD verification with retry
  - presigned GET for direct download
- 2026-03-11: Added bounded WS protocol / handler support for:
  - `CreateUpload`
  - `PresignPart`
  - `CompleteUpload`
  - `AbortUpload`
  - `PrepareDownload`
  - `BatchStat`
  - bounded sequential `BatchWrite`
- 2026-03-11: Extended metadata responses with `storage` / `sealed` fields already required by the fs9 v2 protocol direction.

## Implemented Semantics

- `CreateUpload` is only valid for object-routed large files:
  - requires `expected_size > 0`
  - requires `expected_size >= FS9_OBJECT_MIN`
  - requires S3 configuration
- Upload token claims include:
  - keyspace
  - staging inode id
  - normalized target path hash
  - expected parent inode
  - expected prior inode generation
  - S3 multipart `upload_id`
  - target version
  - nonce
  - `expires_at`
- `PresignPart` only succeeds while lifecycle state is still `Uploading`.
- `CompleteUpload`:
  - verifies HMAC token + expiry
  - reloads reservation state from TiKV
  - normalizes multipart parts
  - completes multipart upload when still `Uploading`
  - verifies object visibility with `HEAD` retry
  - persists final object metadata
  - publishes through TiKV CAS using the stored reservation
- `AbortUpload` only succeeds while lifecycle state is still `Uploading`.
- `PrepareDownload` is intentionally limited to object-backed files and returns a presigned GET.
- `BatchStat` and `BatchWrite` are hard-bounded by config:
  - `FS9_BATCH_STAT_MAX_FILES`
  - `FS9_BATCH_WRITE_MAX_FILES`
  - `FS9_BATCH_WRITE_MAX_TOTAL_BYTES`
  - `FS9_BATCH_WRITE_MAX_ENCODED_BYTES`
- `BatchWrite` in M4 is deliberately a bounded sequential full-replacement path for inline-sized files only. It is not presented as pack-aware optimization.

## Verification

Commands run locally on 2026-03-11:

```bash
cargo fmt
cargo check -p db9-server --tests
cargo test -p db9-server ws:: -- --nocapture
cargo test -p db9-server upload_token -- --nocapture
cargo test -p db9-server test_normalize_completed_parts_sorts_and_rejects_duplicates -- --nocapture
```

Results:

- `cargo check -p db9-server --tests`: passed
- `cargo test -p db9-server ws:: -- --nocapture`: passed (`44` tests)
- `cargo test -p db9-server upload_token -- --nocapture`: passed (`3` tests)
- `cargo test -p db9-server test_normalize_completed_parts_sorts_and_rejects_duplicates -- --nocapture`: passed (`1` test)

## Notes

- This milestone is still server-side only and does not include `db9-backend` direct multipart client work.
- This milestone does not implement packfile ingest or bundle-aware small-file acceleration.
- The current branch still contains an unrelated untracked file at `k8s/dev-deploy.yaml`; it is not part of M4.
