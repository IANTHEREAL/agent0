//! Adapts `fsplane.v2.FileMeta` to `FsFileInfo`.
//!
//! Field-level changes from v1 → v2 to be aware of:
//!
//! - `mtime_ms` → `mtime_ns`. `FsFileInfo.mtime` is seconds; divide by 1e9.
//! - New `is_symlink` field. fs9 v2 server populates it from the JuiceFS
//!   inode `ISLNK` bit (DESIGN.md §FileMeta — landed alongside the v2
//!   port). We trust the wire value verbatim.
//! - New `version` field, packed as `(mtime_ns << 16) | generation`,
//!   opaque to the client. We surface the FULL u64 in `FsFileInfo.generation`
//!   so downstream CAS checks (when they arrive) see the whole token;
//!   callers MUST NOT decompose it (DESIGN.md §FileMeta.version).

#![cfg(fsplane_v2_generated)]

use crate::extensions::fs::backend::FsFileInfo;
use crate::extensions::fs::grpc::proto::FileMeta;

/// Convert a proto `FileMeta` into an `FsFileInfo`. The path is supplied
/// by the caller because the meta payload does not carry it (mirroring
/// v1 client.rs behaviour where path always came from the request).
pub(crate) fn file_meta_to_fs_file_info(path: String, meta: FileMeta) -> FsFileInfo {
    let mtime_ns = meta.mtime_ns.max(0) as u64;
    let mtime = mtime_ns / 1_000_000_000;
    FsFileInfo {
        path,
        is_dir: meta.is_dir,
        is_symlink: meta.is_symlink,
        size: meta.size,
        mode: meta.mode,
        // Use the opaque v2 version token as the generation field. Old
        // callers that expected v1's "always 0" still see *something*
        // monotonic enough for caching decisions; new callers can adopt
        // CAS semantics once they're built.
        generation: meta.version,
        mtime,
        // Remote backends don't expose embedded-storage classification.
        // None matches v1 GrpcFsBackend behaviour at backend.rs.
        storage: None,
        // Files served by fs9 are not "sealed" in the embedded sense.
        // Some(false) keeps callers that branch on `sealed.is_some()`
        // happy without claiming a sealed status that doesn't apply.
        sealed: Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta(size: u64, mtime_ns: i64, mode: u32, is_dir: bool, is_symlink: bool) -> FileMeta {
        FileMeta {
            size,
            mtime_ns,
            mode,
            is_dir,
            is_symlink,
            version: ((mtime_ns as u64) << 16) | 1,
        }
    }

    #[test]
    fn mtime_ns_converts_to_seconds() {
        let meta = make_meta(0, 1_500_000_000_000i64, 0o644, false, false);
        let info = file_meta_to_fs_file_info("/x".to_string(), meta);
        assert_eq!(info.mtime, 1500);
    }

    #[test]
    fn negative_mtime_clamps_to_zero() {
        let meta = make_meta(0, -42, 0o644, false, false);
        let info = file_meta_to_fs_file_info("/x".to_string(), meta);
        assert_eq!(info.mtime, 0);
    }

    #[test]
    fn is_symlink_passes_through() {
        let meta = make_meta(0, 0, 0o777, false, true);
        let info = file_meta_to_fs_file_info("/link".to_string(), meta);
        assert!(info.is_symlink);
        assert!(!info.is_dir);
    }

    #[test]
    fn is_dir_passes_through() {
        let meta = make_meta(0, 0, 0o755, true, false);
        let info = file_meta_to_fs_file_info("/dir".to_string(), meta);
        assert!(info.is_dir);
        assert!(!info.is_symlink);
    }

    #[test]
    fn size_and_mode_pass_through() {
        let meta = make_meta(42, 0, 0o600, false, false);
        let info = file_meta_to_fs_file_info("/x".to_string(), meta);
        assert_eq!(info.size, 42);
        assert_eq!(info.mode, 0o600);
    }

    #[test]
    fn version_surfaces_as_generation() {
        let meta = make_meta(0, 1_000_000_000, 0o644, false, false);
        let info = file_meta_to_fs_file_info("/x".to_string(), meta);
        // Caller MUST NOT decompose version; verify the whole u64 round-trips.
        assert_eq!(info.generation, meta_version_packed(1_000_000_000));
    }

    fn meta_version_packed(mtime_ns: i64) -> u64 {
        ((mtime_ns as u64) << 16) | 1
    }

    #[test]
    fn storage_is_none_and_sealed_is_some_false() {
        let meta = make_meta(0, 0, 0o644, false, false);
        let info = file_meta_to_fs_file_info("/x".to_string(), meta);
        assert!(info.storage.is_none());
        assert_eq!(info.sealed, Some(false));
    }
}
