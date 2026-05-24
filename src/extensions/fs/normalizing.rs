//! `NormalizingFsBackend` — single-source-of-truth adapter that
//! canonicalizes every path-bearing input to fs9 v2's `path.Clean` form
//! before delegating to an inner backend.
//!
//! ## Why
//!
//! `FsBackend` is implemented by `EmbeddedFsBackend` and
//! `GrpcFsBackend`. The two backends interpret caller paths
//! differently in at least two ways:
//!
//! 1. **Absolute prefix.** Embedded re-prepends `/` inside
//!    `pagefs::normalize_path`; gRPC forwards raw and fs9 v2
//!    `validatePath` rejects non-absolute paths. That asymmetry made
//!    `fs9_write('tests/x', ...)`, `read_parquet('fs9://tests/x')`
//!    and `COPY FROM 'fs9://tests/x'` work on embedded but fail on
//!    JuiceFS (PR #2547 review #1).
//! 2. **Dot segments.** `pagefs::resolve_path` walks segments via
//!    `lookup(parent_inode, segment)` — so `.` resolves to a literal
//!    child name. fs9 v2 server-side `path.Clean` collapses `.` and
//!    rejects `..`. That made mutating paths like `/./foo`, `/foo/.`,
//!    `/foo/./bar` alias `/foo` on JuiceFS but resolve to a distinct
//!    (usually nonexistent) literal entry on embedded (PR #2547
//!    review #2).
//!
//! Patching each gRPC request builder duplicates the rule across ~17
//! call sites and leaves the next backend / new trait method exposed.
//! Instead, the only construction point — `init_backend_with_args` —
//! wraps the chosen leaf in `NormalizingFsBackend`. Path
//! canonicalization becomes a property of the `FsBackend` contract,
//! not of any individual implementation. `pagefs::normalize_path` is
//! defense in depth on top.
//!
//! ## What it canonicalizes
//!
//! Every method that takes a `path: &str`, `paths: &[String]`,
//! `old_path/new_path`, or `Vec<FsBatchWriteFile>` (where `path` is a
//! struct field) is shaped through
//! [`crate::extensions::fs::to_fs9_canonical_path`] before delegation.
//! That function applies fs9 v2 `path.Clean` semantics: absolute
//! prefix, empty + `.` segments dropped, `..` segments rejected.
//!
//! ## What it does NOT canonicalize
//!
//! - `symlink(path, target)` — `target` is the symlink's contents,
//!   which may legitimately be a relative path under POSIX semantics.
//!   Only the link path is shaped.
//! - `presign_upload_part / complete_upload / abort_upload` —
//!   `upload_token` is an opaque server-issued credential, not a path.
//! - Results returned from inner are not rewritten; `FsFileInfo.path`
//!   already comes from inner in canonical form because inner saw
//!   canonical inputs.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::AsyncBufRead;

use crate::extensions::fs::backend::{
    FsBackend, FsBatchWriteEntry, FsBatchWriteFile, FsBatchWriteGroupedResult, FsCreateUpload,
    FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest, FsReaddirResult,
    FsRecursiveReaddirOptions, FsRecursiveReaddirResult, FsWriteStream, FsWriteStreamOptions,
};
use crate::extensions::fs::to_fs9_canonical_path;

pub(crate) struct NormalizingFsBackend {
    inner: Arc<dyn FsBackend>,
}

impl NormalizingFsBackend {
    pub(crate) fn new(inner: Arc<dyn FsBackend>) -> Self {
        Self { inner }
    }

    fn shape_each(paths: &[String]) -> Result<Vec<String>> {
        paths.iter().map(|p| to_fs9_canonical_path(p)).collect()
    }

    fn shape_batch_write_files(files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteFile>> {
        files
            .into_iter()
            .map(|f| {
                Ok(FsBatchWriteFile {
                    path: to_fs9_canonical_path(&f.path)?,
                    data: f.data,
                    mode: f.mode,
                })
            })
            .collect()
    }
}

#[async_trait]
impl FsBackend for NormalizingFsBackend {
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        self.inner.stat(&to_fs9_canonical_path(path)?).await
    }

    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        let shaped = Self::shape_each(paths)?;
        self.inner.batch_stat(&shaped).await
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        self.inner.readdir(&to_fs9_canonical_path(path)?).await
    }

    async fn readdir_with_meta(&self, path: &str) -> Result<FsReaddirResult> {
        self.inner
            .readdir_with_meta(&to_fs9_canonical_path(path)?)
            .await
    }

    async fn batch_readdir(&self, paths: &[String]) -> Result<Vec<Result<Vec<FsFileInfo>>>> {
        let shaped = Self::shape_each(paths)?;
        self.inner.batch_readdir(&shaped).await
    }

    async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<FsRecursiveReaddirResult> {
        self.inner
            .readdir_recursive(&to_fs9_canonical_path(path)?, opts)
            .await
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        self.inner
            .read_file(&to_fs9_canonical_path(path)?, max_bytes)
            .await
    }

    async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        let shaped = Self::shape_each(paths)?;
        self.inner
            .batch_inline_read(&shaped, max_file_bytes, max_total_bytes)
            .await
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        self.inner
            .read_file_stream(&to_fs9_canonical_path(path)?, max_bytes)
            .await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        self.inner.remove(&to_fs9_canonical_path(path)?).await
    }

    async fn remove_recursive(&self, path: &str) -> Result<u64> {
        self.inner
            .remove_recursive(&to_fs9_canonical_path(path)?)
            .await
    }

    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()> {
        self.inner
            .mkdir(&to_fs9_canonical_path(path)?, recursive, mode)
            .await
    }

    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize> {
        self.inner
            .write_file(&to_fs9_canonical_path(path)?, data, mode)
            .await
    }

    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        self.inner
            .batch_write(Self::shape_batch_write_files(files)?)
            .await
    }

    fn supports_batch_write_atomic(&self) -> bool {
        self.inner.supports_batch_write_atomic()
    }

    fn supports_presigned(&self) -> bool {
        self.inner.supports_presigned()
    }

    async fn batch_write_grouped(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<FsBatchWriteGroupedResult> {
        self.inner
            .batch_write_grouped(Self::shape_batch_write_files(files)?)
            .await
    }

    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        self.inner
            .begin_write_stream(&to_fs9_canonical_path(path)?, opts)
            .await
    }

    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.inner
            .read_file_at(&to_fs9_canonical_path(path)?, offset, length)
            .await
    }

    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        self.inner
            .write_file_at(&to_fs9_canonical_path(path)?, offset, data)
            .await
    }

    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        self.inner
            .append_file(&to_fs9_canonical_path(path)?, data)
            .await
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        self.inner
            .truncate(&to_fs9_canonical_path(path)?, size)
            .await
    }

    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old = to_fs9_canonical_path(old_path)?;
        let new = to_fs9_canonical_path(new_path)?;
        self.inner.rename(&old, &new).await
    }

    async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
        mode: Option<u32>,
        checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload> {
        self.inner
            .create_upload(
                &to_fs9_canonical_path(path)?,
                expected_size,
                mode,
                checksum_algorithm,
            )
            .await
    }

    async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
        checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest> {
        // `upload_token` is an opaque server-issued credential, not a
        // path — do not reshape.
        self.inner
            .presign_upload_part(upload_token, part_number, checksum_crc32c)
            .await
    }

    async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        self.inner
            .complete_upload(upload_token, parts, checksum)
            .await
    }

    async fn abort_upload(&self, upload_token: &str) -> Result<()> {
        self.inner.abort_upload(upload_token).await
    }

    async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload> {
        self.inner
            .prepare_download(&to_fs9_canonical_path(path)?)
            .await
    }

    async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        // `target` is the symlink's contents (may be relative under
        // POSIX); only the link `path` is shaped.
        self.inner
            .symlink(&to_fs9_canonical_path(path)?, target)
            .await
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        self.inner.readlink(&to_fs9_canonical_path(path)?).await
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        self.inner.chmod(&to_fs9_canonical_path(path)?, mode).await
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use crate::extensions::fs::backend::{
        FsBackend, FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload,
        FsPresignedRequest, FsStorage, FsWriteStream, FsWriteStreamOptions,
    };
    use crate::extensions::fs::to_fs9_canonical_path;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::io::{empty, AsyncBufRead};

    /// Records every path-typed argument the adapter forwarded to inner.
    #[derive(Default)]
    struct PathRecorder {
        seen: Mutex<Vec<(String, String)>>,
    }

    impl PathRecorder {
        fn push(&self, op: &str, path: &str) {
            self.seen.lock().push((op.to_string(), path.to_string()));
        }

        fn snapshot(&self) -> Vec<(String, String)> {
            self.seen.lock().clone()
        }
    }

    struct RecordingBackend {
        rec: Arc<PathRecorder>,
    }

    impl RecordingBackend {
        fn new_pair() -> (Arc<PathRecorder>, Arc<dyn FsBackend>) {
            let rec = Arc::new(PathRecorder::default());
            let backend: Arc<dyn FsBackend> = Arc::new(Self { rec: rec.clone() });
            (rec, backend)
        }

        fn stub_info(path: &str) -> FsFileInfo {
            FsFileInfo {
                path: path.to_string(),
                is_dir: false,
                is_symlink: false,
                size: 0,
                mode: 0o644,
                generation: 1,
                mtime: 0,
                storage: Some(FsStorage::Inline),
                sealed: Some(false),
            }
        }
    }

    #[async_trait]
    impl FsBackend for RecordingBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            self.rec.push("stat", path);
            Ok(Self::stub_info(path))
        }
        async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
            self.rec.push("readdir", path);
            Ok(Vec::new())
        }
        async fn read_file(&self, path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            self.rec.push("read_file", path);
            Ok(Vec::new())
        }
        async fn read_file_stream(
            &self,
            path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            self.rec.push("read_file_stream", path);
            Ok(Box::new(empty()))
        }
        async fn remove(&self, path: &str) -> Result<()> {
            self.rec.push("remove", path);
            Ok(())
        }
        async fn remove_recursive(&self, path: &str) -> Result<u64> {
            self.rec.push("remove_recursive", path);
            Ok(0)
        }
        async fn mkdir(&self, path: &str, _r: bool, _m: Option<u32>) -> Result<()> {
            self.rec.push("mkdir", path);
            Ok(())
        }
        async fn write_file(&self, path: &str, _data: &[u8], _m: Option<u32>) -> Result<usize> {
            self.rec.push("write_file", path);
            Ok(0)
        }
        async fn begin_write_stream(
            &self,
            path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            self.rec.push("begin_write_stream", path);
            Err(anyhow::anyhow!("stub"))
        }
        async fn read_file_at(&self, path: &str, _o: u64, _l: usize) -> Result<Vec<u8>> {
            self.rec.push("read_file_at", path);
            Ok(Vec::new())
        }
        async fn write_file_at(&self, path: &str, _o: u64, _d: &[u8]) -> Result<usize> {
            self.rec.push("write_file_at", path);
            Ok(0)
        }
        async fn append_file(&self, path: &str, _d: &[u8]) -> Result<usize> {
            self.rec.push("append_file", path);
            Ok(0)
        }
        async fn truncate(&self, path: &str, _s: u64) -> Result<()> {
            self.rec.push("truncate", path);
            Ok(())
        }
        async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
            self.rec.push("rename_old", old_path);
            self.rec.push("rename_new", new_path);
            Ok(())
        }
        async fn create_upload(
            &self,
            path: &str,
            _e: u64,
            _m: Option<u32>,
            _c: Option<&str>,
        ) -> Result<FsCreateUpload> {
            self.rec.push("create_upload", path);
            Err(anyhow::anyhow!("stub"))
        }
        async fn presign_upload_part(
            &self,
            tok: &str,
            _p: i32,
            _c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            self.rec.push("presign_upload_part_token", tok);
            Err(anyhow::anyhow!("stub"))
        }
        async fn complete_upload(
            &self,
            tok: &str,
            _p: Vec<FsMultipartCompletedPart>,
            _c: Option<[u8; 32]>,
        ) -> Result<usize> {
            self.rec.push("complete_upload_token", tok);
            Ok(0)
        }
        async fn abort_upload(&self, tok: &str) -> Result<()> {
            self.rec.push("abort_upload_token", tok);
            Ok(())
        }
        async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload> {
            self.rec.push("prepare_download", path);
            Err(anyhow::anyhow!("stub"))
        }
        async fn symlink(&self, path: &str, target: &str) -> Result<()> {
            self.rec.push("symlink_path", path);
            self.rec.push("symlink_target", target);
            Ok(())
        }
        async fn readlink(&self, path: &str) -> Result<String> {
            self.rec.push("readlink", path);
            Ok(String::new())
        }
        async fn chmod(&self, path: &str, _m: u32) -> Result<()> {
            self.rec.push("chmod", path);
            Ok(())
        }
    }

    fn shape(input: &str) -> String {
        to_fs9_canonical_path(input).unwrap()
    }

    #[test]
    fn canonical_path_absolute_prefix_and_empty_collapse() {
        // PR #2547 review #1: relative SQL paths
        // (`fs9_write('tests/x', ...)`, `read_parquet('fs9://tests/x')`)
        // must reach the gRPC backend as absolute paths.
        assert_eq!(shape("tests/x"), "/tests/x");
        assert_eq!(shape("relative/path"), "/relative/path");
        assert_eq!(shape("/already/abs"), "/already/abs");
        assert_eq!(shape("//foo//bar"), "/foo/bar");
        assert_eq!(shape("/foo/bar/"), "/foo/bar");
        assert_eq!(shape(""), "/");
        assert_eq!(shape("/"), "/");
        assert_eq!(shape("///"), "/");
    }

    #[test]
    fn canonical_path_collapses_dot_segments() {
        // PR #2547 review #2: `.` segments alias the parent on fs9 v2
        // (`path.Clean`) but resolve as a literal child on embedded.
        // The adapter folds them away so both backends operate on the
        // same target.
        assert_eq!(shape("/./foo"), "/foo");
        assert_eq!(shape("/foo/."), "/foo");
        assert_eq!(shape("/foo/./bar"), "/foo/bar");
        assert_eq!(shape("/./"), "/");
        assert_eq!(shape("/.//."), "/");
        assert_eq!(shape("./relative"), "/relative");
    }

    #[test]
    fn canonical_path_preserves_dotfile_names() {
        // A segment that STARTS with `.` but is not exactly `.` or `..`
        // is a legitimate filename (e.g. `.gitignore`, `.hidden`).
        // The canonicalizer must not eat those.
        assert_eq!(shape("/.hidden"), "/.hidden");
        assert_eq!(shape("/foo/.gitignore"), "/foo/.gitignore");
        assert_eq!(shape("/.x"), "/.x");
        assert_eq!(shape("/x."), "/x.");
        assert_eq!(shape("/..foo"), "/..foo");
        assert_eq!(shape("/foo..bar"), "/foo..bar");
    }

    #[test]
    fn canonical_path_rejects_parent_traversal() {
        // fs9 v2 `validatePath` rejects `..`; embedded would happily
        // look up a literal child named `..`. The adapter rejects up
        // front so both backends see the same `InvalidInput` error.
        for bad in ["/..", "/../foo", "/foo/..", "/foo/../bar", "..", "../foo"] {
            let err = to_fs9_canonical_path(bad).unwrap_err().to_string();
            assert!(
                err.contains(".."),
                "input {bad:?} should mention `..`, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn adapter_shapes_single_path_methods() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new(inner);

        adapter.stat("tests/x").await.unwrap();
        adapter.readdir("dir/").await.unwrap();
        adapter.read_file("a/b", 1024).await.unwrap();
        adapter.remove("rel/path").await.unwrap();
        adapter.remove_recursive("rel/dir").await.unwrap();
        adapter.mkdir("rel/mk", true, None).await.unwrap();
        adapter.write_file("rel/w", b"x", None).await.unwrap();
        adapter.read_file_at("rel/r", 0, 1).await.unwrap();
        adapter.write_file_at("rel/wa", 0, b"x").await.unwrap();
        adapter.append_file("rel/ap", b"x").await.unwrap();
        adapter.truncate("rel/t", 0).await.unwrap();
        adapter.readlink("rel/l").await.unwrap();
        adapter.chmod("rel/c", 0o644).await.unwrap();

        let seen = rec.snapshot();
        for (_op, p) in &seen {
            assert!(
                p.starts_with('/'),
                "every inner path must be absolute, got {p:?}"
            );
        }
        assert!(seen.iter().any(|(op, p)| op == "stat" && p == "/tests/x"));
        assert!(seen.iter().any(|(op, p)| op == "readdir" && p == "/dir"));
        assert!(seen.iter().any(|(op, p)| op == "read_file" && p == "/a/b"));
    }

    #[tokio::test]
    async fn adapter_shapes_rename_both_paths() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new(inner);
        adapter.rename("old/x", "new/y").await.unwrap();
        let seen = rec.snapshot();
        assert!(seen
            .iter()
            .any(|(op, p)| op == "rename_old" && p == "/old/x"));
        assert!(seen
            .iter()
            .any(|(op, p)| op == "rename_new" && p == "/new/y"));
    }

    #[tokio::test]
    async fn adapter_does_not_reshape_symlink_target_or_upload_token() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new(inner);

        adapter
            .symlink("links/foo", "../target/path")
            .await
            .unwrap();
        let _ = adapter.presign_upload_part("opaque-token", 1, None).await;
        let _ = adapter.abort_upload("opaque-token").await;

        let seen = rec.snapshot();
        // Link path is shaped.
        assert!(seen
            .iter()
            .any(|(op, p)| op == "symlink_path" && p == "/links/foo"));
        // Symlink target passes through verbatim — POSIX relative
        // symlinks are legitimate.
        assert!(seen
            .iter()
            .any(|(op, p)| op == "symlink_target" && p == "../target/path"));
        // Upload tokens are credentials, not paths — must pass through.
        assert!(seen
            .iter()
            .any(|(op, p)| op == "presign_upload_part_token" && p == "opaque-token"));
        assert!(seen
            .iter()
            .any(|(op, p)| op == "abort_upload_token" && p == "opaque-token"));
    }

    #[tokio::test]
    async fn adapter_shapes_each_path_in_batch_lists() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new(inner);
        let _ = adapter
            .batch_stat(&[
                "tests/a".to_string(),
                "/already/b".to_string(),
                "".to_string(),
            ])
            .await
            .unwrap();
        let seen = rec.snapshot();
        // The default batch_stat impl delegates to per-entry stat — so
        // we should see three shaped stat calls.
        let stat_paths: Vec<_> = seen
            .iter()
            .filter(|(op, _)| op == "stat")
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(stat_paths, vec!["/tests/a", "/already/b", "/"]);
    }

    /// PR #2547 review #2 regression: every mutating op invoked with a
    /// path containing `.` segments must land on the canonical target
    /// at the inner backend. On embedded that previously created a
    /// literal child named `.`; on JuiceFS fs9 v2 silently aliased to
    /// `/foo`. Both paths now reach inner as `/foo`.
    #[tokio::test]
    async fn adapter_collapses_dot_segments_for_mutating_ops() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new(inner);

        adapter.write_file("/./foo", b"x", None).await.unwrap();
        adapter.remove("/foo/.").await.unwrap();
        adapter.remove_recursive("/foo/./bar").await.unwrap();
        adapter.rename("/old/./x", "/new/./y/.").await.unwrap();
        adapter.chmod("/./foo", 0o600).await.unwrap();
        adapter.mkdir("/foo/./sub", true, None).await.unwrap();
        adapter.symlink("/./link", "../target").await.unwrap();

        let seen = rec.snapshot();
        let by_op = |op: &str| -> Vec<String> {
            seen.iter()
                .filter(|(o, _)| o == op)
                .map(|(_, p)| p.clone())
                .collect()
        };

        assert_eq!(by_op("write_file"), vec!["/foo".to_string()]);
        assert_eq!(by_op("remove"), vec!["/foo".to_string()]);
        assert_eq!(by_op("remove_recursive"), vec!["/foo/bar".to_string()]);
        assert_eq!(by_op("rename_old"), vec!["/old/x".to_string()]);
        assert_eq!(by_op("rename_new"), vec!["/new/y".to_string()]);
        assert_eq!(by_op("chmod"), vec!["/foo".to_string()]);
        assert_eq!(by_op("mkdir"), vec!["/foo/sub".to_string()]);
        // Link path canonicalized; target still passes through.
        assert_eq!(by_op("symlink_path"), vec!["/link".to_string()]);
        assert_eq!(by_op("symlink_target"), vec!["../target".to_string()]);
    }

    /// `..` segments must be refused before reaching either backend.
    /// Verifies both single-path and rename two-path code paths.
    #[tokio::test]
    async fn adapter_rejects_parent_traversal_on_mutators() {
        let (_, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new(inner);

        for op_name in ["write_file", "remove", "chmod", "rename_old", "rename_new"] {
            let err = match op_name {
                "write_file" => adapter
                    .write_file("/foo/../bar", b"x", None)
                    .await
                    .unwrap_err(),
                "remove" => adapter.remove("/foo/..").await.unwrap_err(),
                "chmod" => adapter.chmod("/..", 0o600).await.unwrap_err(),
                "rename_old" => adapter.rename("/foo/..", "/ok").await.unwrap_err(),
                "rename_new" => adapter.rename("/ok", "/foo/..").await.unwrap_err(),
                _ => unreachable!(),
            };
            assert!(
                err.to_string().contains(".."),
                "{op_name} should reject `..`, got: {err}"
            );
        }
    }
}
