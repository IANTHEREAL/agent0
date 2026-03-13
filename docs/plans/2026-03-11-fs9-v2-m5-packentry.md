# fs9 v2 M5: PackEntry Bundle Path in Server

Date: 2026-03-11

This note records the local implementation progress for **M5** of fs9 v2:

- `DataRef::PackEntry` backed by immutable S3 bundles
- authoritative bundle manifests in TiKV
- local spool/journal as non-authoritative helper state
- bundle read path, replacement retirement, and GC skeleton

Design source of truth:

- `c4pt0r/db9-server#1752` consolidated design comment (`4039365600`)
- Local copy: `docs/design/29_fs9_v2_tikv_metadata_s3_packfiles.md`
- Execution plan: `docs/plans/2026-03-11-fs9-v2-implementation-plan.md`

## Goals (M5)

- Solve the small-file + S3 path without turning every file into a separate object.
- Keep authoritative metadata in TiKV:
  - inode `DataRef`
  - bundle manifest metadata
  - lifecycle sidecars for hidden staging inodes
- Keep local disk state explicitly non-authoritative:
  - spool `.pack`
  - spool journal `.json`
  - in-memory bundle slice cache
- Preserve the design rule that hot mutable writes do not get silently packed.

## Implementation Summary

### New Bundle Module

- `src/extensions/fs/embedded/bundle.rs`
  - adds immutable bundle build logic
  - appends a footer + trailer to each bundle object
  - defines `BundleManifest` and `BundleManifestState`
  - implements TiKV helpers for:
    - load/save/delete manifest
    - manifest scan
    - live-entry retirement
  - implements local spool/journal helpers
  - implements a bounded in-memory bundle slice cache

### Metadata and Keyspace Changes

- `src/extensions/fs/embedded/types.rs`
  - extends `Superblock` with monotonic `next_bundle`
- `src/extensions/fs/embedded/keys.rs`
  - adds `_fs_M{bundle_id}` bundle manifest keys
- `src/extensions/fs/embedded/lifecycle.rs`
  - extends `FileLifecycle::Packing` with `bundle_id`

### Routing and Write Path

- `BatchWrite` is now the sealed-small-file pack route on the server:
  - ordinary `write_file` remains inline/object/legacy routed
  - bounded `BatchWrite` with S3 enabled stages one bundle publish unit
- server batch flow:
  1. allocate bundle ID + hidden staging inodes in TiKV
  2. mark each staging inode as `_fs_L = Packing { bundle_id, updated_at }`
  3. build one immutable bundle object locally
  4. persist local spool/journal
  5. upload the bundle object to S3 and HEAD-verify it
  6. publish bundle manifest + final `PackEntry` inode mappings in TiKV
  7. clear staging lifecycle and remove local spool/journal

This intentionally keeps pack routing explicit for bulk/sealed writes instead of packing every small `write_file`.

### Read Path

- `read_file`
  - `PackEntry` now resolves the TiKV bundle manifest and reads the exact bundle slice
- `read_file_at`
  - `PackEntry` now uses exact offset range GET against the bundle object
- `read_file_stream`
  - `PackEntry` uses the same exact-slice fetch and streams the returned bytes in chunks
- bundle reads use:
  - local in-memory slice cache first
  - S3 range GET on cache miss

### Replacement / Remove / GC Semantics

- pack-backed files now retire correctly during:
  - full replacement
  - remove
  - rename-overwrite
  - recursive delete
  - object publish overwrites
- retirement decrements the authoritative bundle manifest live-entry count in TiKV
- when a bundle reaches zero live entries:
  - manifest moves to `pending_delete`
  - background GC deletes the S3 object and then deletes the manifest
- if a bundle still has live entries but also has stale entries:
  - it remains an explicit compaction candidate
  - this is the M5 compaction skeleton

### Recovery

- startup recovery now handles:
  - stale local bundle journals
  - stale `Packing` lifecycle state from interrupted bundle publish
- runtime lifecycle GC now also handles stale `Packing` sidecars
- recovery rule is conservative:
  - if TiKV has no published manifest, local spool is not authoritative
  - best-effort delete the S3 bundle object
  - clear hidden staging state
- both journal sweep and `Packing` reaping are guarded by a staleness window so a fresh in-flight bundle publish on another node is not treated as crash debris

## Protocol Surface

- `src/extensions/fs/backend.rs`
  - adds backend-native `batch_write(...)`
- `src/extensions/fs/ws/handler.rs`
  - `BatchWrite` now dispatches through the backend batch path
  - duplicate paths in one request are rejected explicitly

## Validation

Executed locally on 2026-03-11:

- `cargo fmt --manifest-path Cargo.toml`
- `cargo check --manifest-path Cargo.toml -p db9-server`
- targeted unit tests:
  - `cargo test --manifest-path Cargo.toml -p db9-server bundle_spool_roundtrip_loads_and_removes_pending_journal`
  - `cargo test --manifest-path Cargo.toml -p db9-server build_bundle_roundtrip_footer_shape_is_stable`
  - `cargo test --manifest-path Cargo.toml -p db9-server bundle_slice_cache_evicts_old_entries_by_total_bytes`

## Notes

- This milestone does **not** change the ordinary single-file `write_file` route to pack by default.
- This milestone does **not** add client-side connection-pool / prefetch work; that remains backend M5/M6.
- Manual dev-environment E2E is still expected afterward for real TiKV + S3 verification.

## Next

- open a dedicated `db9-server:fs9-v2` PR for M5
- after merge, move to `db9-backend` M5 + M6 on its `fs9-v2` branch
