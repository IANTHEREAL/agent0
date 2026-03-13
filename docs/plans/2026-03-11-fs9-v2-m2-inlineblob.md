# fs9 v2 M2: InlineBlob + No-S3 Fallback

Date: 2026-03-11

This note records the local implementation progress for **M2** of fs9 v2:

- `InlineBlob` (tiny mutable files stored as one TiKV KV pair)
- explicit **no-S3 fallback** to `LegacyPages` for files above the inline threshold

Design source of truth:

- `c4pt0r/db9-server#1752` consolidated design comment (`4039365600`)
- Local copy: `docs/design/29_fs9_v2_tikv_metadata_s3_packfiles.md`
- Execution plan: `docs/plans/2026-03-11-fs9-v2-implementation-plan.md`

## Goals (M2)

- Route tiny mutable files to `InlineBlob` stored at `_fs_B{inode}`.
- Preserve current behavior for larger files without S3:
  - if `InlineBlob` is disabled or file size exceeds the inline threshold, use `LegacyPages` (`_fs_P{inode}:{page}`).
- Keep file mutation semantics intact for `write_file`, `write_file_at`, `append_file`, `truncate`, `remove`.
- Ensure cleanup paths delete the correct backing data (blob vs pages).

## Implementation Summary

### New Modules

- `src/extensions/fs/config.rs`
  - Adds `Fs9Config` with `FS9_INLINE_MAX` (byte-size parser supports `64KiB`, `4MiB`, etc).
  - Default: `64KiB`.

- `src/extensions/fs/embedded/blob.rs`
  - Implements `_fs_B{inode}` read/write/delete helpers.
  - Provides `apply_write_at` and `apply_truncate` for inline mutation semantics.

### Embedded PageFS Routing

Primary routing rule (M2):

- If `inline_max_bytes > 0` and the **resulting file size** is `<= inline_max_bytes`:
  - store as `DataRef::InlineBlob`
  - maintain `inode.page_count = 0`
- Otherwise:
  - store as `DataRef::LegacyPages`

Key implementation points:

- `read_file` / `read_file_at` / `read_file_stream` now handle `InlineBlob` by routing reads through a `DataRef`-aware range reader.
- `write_file` routes the whole-file replacement to `InlineBlob` vs `LegacyPages`.
- `write_file_at` supports:
  - `LegacyPages -> InlineBlob` promotion when the resulting size stays under the inline threshold
  - `InlineBlob -> LegacyPages` demotion when the resulting size exceeds the inline threshold
- `truncate` supports promotion/demotion between `LegacyPages` and `InlineBlob`.
- `rename` overwrite path deletes the correct data backing for the destination (blob vs pages).
- staging/orphan cleanup paths delete the correct backing data (blob vs pages).

## Validation

- Rust unit tests:
  - `cargo test -p db9-server` (ignored tests not executed)
- Added ignored behavioral tests (require a running TiKV/PD cluster):
  - `inlineblob_behavioral_*` tests in `src/extensions/fs/embedded/pagefs.rs`
  - Run example:
    - `PD_ENDPOINTS=127.0.0.1:2379 cargo test -p db9-server inlineblob_behavioral -- --ignored`

## Operational Notes

- `FS9_INLINE_MAX=0` disables InlineBlob routing (forces `LegacyPages`).
- Mixed-version note:
  - Old binaries that do not understand `DataRef::InlineBlob` may misread inline-backed files.
  - Recommended rollout posture is to keep `FS9_INLINE_MAX=0` during mixed-version deploys and enable it only after all nodes are upgraded.

## Next (M3)

- Implement `DataRef::Object` semantics with S3 client + hidden staging + publish + lifecycle/GC.
- Keep the pre-Phase-4 size guard boundary for object-backed files (per Spike D).

