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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

type LifecycleOperationGuard = crate::sql::session::db_connections::DbOperationGuard;

use crate::extensions::fs::backend::{
    FsBackend, FsBatchWriteEntry, FsBatchWriteFile, FsBatchWriteGroupedResult, FsCreateUpload,
    FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest, FsReaddirResult,
    FsRecursiveReaddirOptions, FsRecursiveReaddirResult, FsWriteStream, FsWriteStreamOptions,
};
use crate::extensions::fs::to_fs9_canonical_path;
use crate::sql::error::SqlError;
use crate::storage::TikvStore;

#[async_trait]
pub(crate) trait FsLifecycleAdmission: Send + Sync {
    fn database_id(&self) -> Option<u64> {
        None
    }

    async fn ensure_active(&self) -> Result<()>;
}

pub(crate) struct DatabaseLifecycleAdmission {
    store: Arc<TikvStore>,
    database_id: u64,
}

impl DatabaseLifecycleAdmission {
    pub(crate) fn new(store: Arc<TikvStore>, database_id: u64) -> Self {
        Self { store, database_id }
    }
}

#[async_trait]
impl FsLifecycleAdmission for DatabaseLifecycleAdmission {
    fn database_id(&self) -> Option<u64> {
        Some(self.database_id)
    }

    async fn ensure_active(&self) -> Result<()> {
        crate::worker::database_lifecycle::ensure_database_lifecycle_accepts_traffic()?;
        if self.store.database_active(self.database_id).await? {
            return Ok(());
        }
        Err(anyhow!(SqlError::InvalidCatalogName(
            "database is being dropped".to_string()
        )))
    }
}

pub(crate) struct NormalizingFsBackend {
    tenant_keyspace: String,
    inner: Arc<dyn FsBackend>,
    lifecycle_admission: Option<Arc<dyn FsLifecycleAdmission>>,
}

impl NormalizingFsBackend {
    pub(crate) fn new(tenant_keyspace: impl Into<String>, inner: Arc<dyn FsBackend>) -> Self {
        Self {
            tenant_keyspace: tenant_keyspace.into(),
            inner,
            lifecycle_admission: None,
        }
    }

    pub(crate) fn new_with_lifecycle_admission(
        tenant_keyspace: impl Into<String>,
        inner: Arc<dyn FsBackend>,
        lifecycle_admission: Arc<dyn FsLifecycleAdmission>,
    ) -> Self {
        Self {
            tenant_keyspace: tenant_keyspace.into(),
            inner,
            lifecycle_admission: Some(lifecycle_admission),
        }
    }

    async fn begin_lifecycle_operation(&self) -> Result<Option<LifecycleOperationGuard>> {
        let Some(admission) = &self.lifecycle_admission else {
            return Ok(None);
        };
        let guard = admission
            .database_id()
            .map(|db_id| {
                crate::sql::session::db_connections::db_connection_registry()
                    .try_track_operation(&self.tenant_keyspace, db_id)
            })
            .transpose()
            .map_err(|err| anyhow!(SqlError::InvalidCatalogName(err.to_string())))?;
        admission.ensure_active().await?;
        Ok(guard)
    }

    fn record_op(&self, operation: &'static str, start: std::time::Instant, result: &'static str) {
        crate::metrics::record_fs9_operation_latency(
            &self.tenant_keyspace,
            self.inner.backend_kind(),
            operation,
            result,
            start.elapsed(),
        );
    }

    fn record_result<T>(
        &self,
        operation: &'static str,
        start: std::time::Instant,
        result: &Result<T>,
    ) {
        self.record_op(operation, start, result_label(result.is_ok()));
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

fn result_label(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "err"
    }
}

fn entry_results_label<T>(entries: &[Result<T>]) -> &'static str {
    if entries.iter().all(Result::is_ok) {
        "ok"
    } else if entries.iter().any(Result::is_ok) {
        "partial"
    } else {
        "err"
    }
}

fn batch_write_entries_label(entries: &[FsBatchWriteEntry]) -> &'static str {
    if entries.iter().all(|entry| entry.result.is_ok()) {
        "ok"
    } else if entries.iter().any(|entry| entry.result.is_ok()) {
        "partial"
    } else {
        "err"
    }
}

#[async_trait]
impl FsBackend for NormalizingFsBackend {
    fn backend_kind(&self) -> &'static str {
        self.inner.backend_kind()
    }

    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.stat(&path).await;
        self.record_result("stat", start, &result);
        result
    }

    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        let shaped = Self::shape_each(paths)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.batch_stat(&shaped).await;
        let label = result
            .as_ref()
            .map(|entries| entry_results_label(entries))
            .unwrap_or("err");
        self.record_op("batch_stat", start, label);
        result
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.readdir(&path).await;
        self.record_result("readdir", start, &result);
        result
    }

    async fn readdir_with_meta(&self, path: &str) -> Result<FsReaddirResult> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.readdir_with_meta(&path).await;
        self.record_result("readdir_with_meta", start, &result);
        result
    }

    async fn batch_readdir(&self, paths: &[String]) -> Result<Vec<Result<Vec<FsFileInfo>>>> {
        let shaped = Self::shape_each(paths)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.batch_readdir(&shaped).await;
        let label = result
            .as_ref()
            .map(|entries| entry_results_label(entries))
            .unwrap_or("err");
        self.record_op("batch_readdir", start, label);
        result
    }

    async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<FsRecursiveReaddirResult> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.readdir_recursive(&path, opts).await;
        self.record_result("readdir_recursive", start, &result);
        result
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.read_file(&path, max_bytes).await;
        self.record_result("read_file", start, &result);
        result
    }

    async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        let shaped = Self::shape_each(paths)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self
            .inner
            .batch_inline_read(&shaped, max_file_bytes, max_total_bytes)
            .await;
        let label = result
            .as_ref()
            .map(|entries| entry_results_label(entries))
            .unwrap_or("err");
        self.record_op("batch_inline_read", start, label);
        result
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        let path = to_fs9_canonical_path(path)?;
        let lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self
            .inner
            .read_file_stream(&path, max_bytes)
            .await
            .map(|reader| {
                if let Some(admission) = &self.lifecycle_admission {
                    Box::new(LifecycleGuardedReadStream {
                        inner: reader,
                        lifecycle_admission: admission.clone(),
                        _operation_guard: lifecycle_guard,
                        check: None,
                    }) as Box<dyn AsyncBufRead + Unpin + Send>
                } else {
                    reader
                }
            });
        self.record_result("read_file_stream", start, &result);
        result
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.remove(&path).await;
        self.record_result("remove", start, &result);
        result
    }

    async fn remove_recursive(&self, path: &str) -> Result<u64> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.remove_recursive(&path).await;
        self.record_result("remove_recursive", start, &result);
        result
    }

    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.mkdir(&path, recursive, mode).await;
        self.record_result("mkdir", start, &result);
        result
    }

    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.write_file(&path, data, mode).await;
        self.record_result("write_file", start, &result);
        result
    }

    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        let files = Self::shape_batch_write_files(files)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.batch_write(files).await;
        let label = result
            .as_ref()
            .map(|entries| batch_write_entries_label(entries))
            .unwrap_or("err");
        self.record_op("batch_write", start, label);
        result
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
        let files = Self::shape_batch_write_files(files)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.batch_write_grouped(files).await;
        let label = result
            .as_ref()
            .map(|grouped| batch_write_entries_label(&grouped.entries))
            .unwrap_or("err");
        self.record_op("batch_write_grouped", start, label);
        result
    }

    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        let path = to_fs9_canonical_path(path)?;
        let lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self
            .inner
            .begin_write_stream(&path, opts)
            .await
            .map(|stream| {
                if let Some(admission) = &self.lifecycle_admission {
                    Box::new(LifecycleGuardedWriteStream {
                        inner: stream,
                        lifecycle_admission: admission.clone(),
                        _operation_guard: lifecycle_guard,
                    }) as Box<dyn FsWriteStream>
                } else {
                    stream
                }
            });
        self.record_result("begin_write_stream", start, &result);
        result
    }

    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.read_file_at(&path, offset, length).await;
        self.record_result("read_file_at", start, &result);
        result
    }

    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.write_file_at(&path, offset, data).await;
        self.record_result("write_file_at", start, &result);
        result
    }

    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.append_file(&path, data).await;
        self.record_result("append_file", start, &result);
        result
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.truncate(&path, size).await;
        self.record_result("truncate", start, &result);
        result
    }

    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old = to_fs9_canonical_path(old_path)?;
        let new = to_fs9_canonical_path(new_path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.rename(&old, &new).await;
        self.record_result("rename", start, &result);
        result
    }

    async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
        mode: Option<u32>,
        checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self
            .inner
            .create_upload(&path, expected_size, mode, checksum_algorithm)
            .await;
        self.record_result("create_upload", start, &result);
        result
    }

    async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
        checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest> {
        // `upload_token` is an opaque server-issued credential, not a
        // path — do not reshape.
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self
            .inner
            .presign_upload_part(upload_token, part_number, checksum_crc32c)
            .await;
        self.record_result("presign_upload_part", start, &result);
        result
    }

    async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self
            .inner
            .complete_upload(upload_token, parts, checksum)
            .await;
        self.record_result("complete_upload", start, &result);
        result
    }

    async fn abort_upload(&self, upload_token: &str) -> Result<()> {
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.abort_upload(upload_token).await;
        self.record_result("abort_upload", start, &result);
        result
    }

    async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.prepare_download(&path).await;
        self.record_result("prepare_download", start, &result);
        result
    }

    async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        // `target` is the symlink's contents (may be relative under
        // POSIX); only the link `path` is shaped.
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.symlink(&path, target).await;
        self.record_result("symlink", start, &result);
        result
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.readlink(&path).await;
        self.record_result("readlink", start, &result);
        result
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let path = to_fs9_canonical_path(path)?;
        let _lifecycle_guard = self.begin_lifecycle_operation().await?;
        let start = std::time::Instant::now();
        let result = self.inner.chmod(&path, mode).await;
        self.record_result("chmod", start, &result);
        result
    }
}

type LifecycleCheckFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

struct LifecycleGuardedReadStream {
    inner: Box<dyn AsyncBufRead + Unpin + Send>,
    lifecycle_admission: Arc<dyn FsLifecycleAdmission>,
    _operation_guard: Option<LifecycleOperationGuard>,
    check: Option<LifecycleCheckFuture>,
}

impl LifecycleGuardedReadStream {
    fn poll_lifecycle_active(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.check.is_none() {
            let admission = self.lifecycle_admission.clone();
            self.check = Some(Box::pin(async move { admission.ensure_active().await }));
        }

        let Some(check) = &mut self.check else {
            unreachable!("lifecycle check future must be installed");
        };
        match check.as_mut().poll(cx) {
            Poll::Ready(result) => {
                self.check = None;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncRead for LifecycleGuardedReadStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.poll_lifecycle_active(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_read(cx, buf),
            Poll::Ready(Err(err)) => Poll::Ready(Err(std::io::Error::other(err))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncBufRead for LifecycleGuardedReadStream {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        let this = self.get_mut();
        match this.poll_lifecycle_active(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_fill_buf(cx),
            Poll::Ready(Err(err)) => Poll::Ready(Err(std::io::Error::other(err))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amt);
    }
}

struct LifecycleGuardedWriteStream {
    inner: Box<dyn FsWriteStream>,
    lifecycle_admission: Arc<dyn FsLifecycleAdmission>,
    _operation_guard: Option<LifecycleOperationGuard>,
}

#[async_trait]
impl FsWriteStream for LifecycleGuardedWriteStream {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.lifecycle_admission.ensure_active().await?;
        self.inner.write_chunk(chunk).await
    }

    async fn terminate(self: Box<Self>, outcome: Result<()>) -> Result<usize> {
        let Self {
            inner,
            lifecycle_admission,
            _operation_guard,
        } = *self;
        if outcome.is_ok() {
            lifecycle_admission.ensure_active().await?;
        }
        inner.terminate(outcome).await
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use tokio::io::{empty, AsyncBufRead, AsyncReadExt};

    static NEXT_DB_ID: AtomicU64 = AtomicU64::new(800_000);

    fn unique_database_identity() -> (String, u64) {
        let db_id = NEXT_DB_ID.fetch_add(1, Ordering::SeqCst);
        (format!("fs_lifecycle_test_{db_id}"), db_id)
    }

    fn test_recorder() -> (
        metrics_exporter_prometheus::PrometheusRecorder,
        metrics_exporter_prometheus::PrometheusHandle,
    ) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        (recorder, handle)
    }

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

    struct StaticAdmission {
        allow: bool,
    }

    #[async_trait]
    impl FsLifecycleAdmission for StaticAdmission {
        async fn ensure_active(&self) -> Result<()> {
            if self.allow {
                Ok(())
            } else {
                Err(anyhow::anyhow!("database fenced"))
            }
        }
    }

    fn static_admission(allow: bool) -> Arc<dyn FsLifecycleAdmission> {
        Arc::new(StaticAdmission { allow })
    }

    struct ToggleAdmission {
        allow: AtomicBool,
        database_id: Option<u64>,
    }

    impl ToggleAdmission {
        fn new(allow: bool) -> Self {
            Self {
                allow: AtomicBool::new(allow),
                database_id: None,
            }
        }

        fn with_database_id(allow: bool, database_id: u64) -> Self {
            Self {
                allow: AtomicBool::new(allow),
                database_id: Some(database_id),
            }
        }

        fn set(&self, allow: bool) {
            self.allow.store(allow, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl FsLifecycleAdmission for ToggleAdmission {
        fn database_id(&self) -> Option<u64> {
            self.database_id
        }

        async fn ensure_active(&self) -> Result<()> {
            if self.allow.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(anyhow::anyhow!("database fenced"))
            }
        }
    }

    struct PartialBatchBackend;

    #[async_trait]
    impl FsBackend for PartialBatchBackend {
        fn backend_kind(&self) -> &'static str {
            "partial_test"
        }

        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            Ok(RecordingBackend::stub_info(path))
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
            Ok(Vec::new())
        }

        async fn read_file(&self, _path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(empty()))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            Ok(())
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            Ok(0)
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            Ok(())
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            Ok(0)
        }

        async fn batch_write(
            &self,
            files: Vec<FsBatchWriteFile>,
        ) -> Result<Vec<FsBatchWriteEntry>> {
            Ok(files
                .into_iter()
                .enumerate()
                .map(|(idx, file)| FsBatchWriteEntry {
                    path: file.path,
                    result: if idx == 0 {
                        Ok(file.data.len())
                    } else {
                        Err(anyhow::anyhow!("entry failed"))
                    },
                    failure_category: if idx == 0 {
                        None
                    } else {
                        Some("execution.test")
                    },
                })
                .collect())
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            Err(anyhow::anyhow!("stub"))
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, data: &[u8]) -> Result<usize> {
            Ok(data.len())
        }

        async fn append_file(&self, _path: &str, data: &[u8]) -> Result<usize> {
            Ok(data.len())
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            Ok(())
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            Ok(())
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            Err(anyhow::anyhow!("stub"))
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            Err(anyhow::anyhow!("stub"))
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            Ok(0)
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            Ok(())
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            Err(anyhow::anyhow!("stub"))
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            Ok(())
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            Ok(String::new())
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
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
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);

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
    async fn lifecycle_admission_blocks_before_inner_backend() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new_with_lifecycle_admission(
            "tenant_metrics",
            inner,
            static_admission(false),
        );

        let err = adapter.stat("tests/x").await.unwrap_err();
        assert!(err.to_string().contains("database fenced"));
        assert!(
            rec.snapshot().is_empty(),
            "fenced fs operation must not reach inner backend"
        );
    }

    #[tokio::test]
    async fn lifecycle_admission_also_blocks_opaque_token_methods() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new_with_lifecycle_admission(
            "tenant_metrics",
            inner,
            static_admission(false),
        );

        let err = adapter
            .presign_upload_part("opaque-token", 1, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("database fenced"));
        assert!(
            rec.snapshot().is_empty(),
            "opaque-token operations must still pass lifecycle admission"
        );
    }

    #[tokio::test]
    async fn lifecycle_admission_guards_read_stream_chunks() {
        let (_rec, inner) = RecordingBackend::new_pair();
        let admission = Arc::new(ToggleAdmission::new(true));
        let adapter = NormalizingFsBackend::new_with_lifecycle_admission(
            "tenant_metrics",
            inner,
            admission.clone(),
        );

        let mut reader = adapter.read_file_stream("tests/x", 1024).await.unwrap();
        admission.set(false);

        let mut buf = [0_u8; 1];
        let err = reader.read(&mut buf).await.unwrap_err();
        assert!(
            err.to_string().contains("database fenced"),
            "stream chunks must re-check lifecycle before emitting data: {err}"
        );
    }

    #[tokio::test]
    async fn lifecycle_operation_guard_releases_after_non_stream_call() {
        let (_rec, inner) = RecordingBackend::new_pair();
        let (keyspace, db_id) = unique_database_identity();
        let adapter = NormalizingFsBackend::new_with_lifecycle_admission(
            keyspace.clone(),
            inner,
            Arc::new(ToggleAdmission::with_database_id(true, db_id)),
        );

        adapter.stat("tests/x").await.unwrap();

        let mut dropping = crate::sql::session::db_connections::db_connection_registry()
            .try_mark_dropping(&keyspace, db_id)
            .expect("finished fs operation should release lifecycle guard");
        dropping.commit();
    }

    #[tokio::test]
    async fn lifecycle_operation_guard_keeps_read_stream_active_until_drop() {
        let (_rec, inner) = RecordingBackend::new_pair();
        let (keyspace, db_id) = unique_database_identity();
        let adapter = NormalizingFsBackend::new_with_lifecycle_admission(
            keyspace.clone(),
            inner,
            Arc::new(ToggleAdmission::with_database_id(true, db_id)),
        );

        let reader = adapter.read_file_stream("tests/x", 1024).await.unwrap();
        assert!(
            matches!(
                crate::sql::session::db_connections::db_connection_registry()
                    .try_mark_dropping(&keyspace, db_id),
                Err(1)
            ),
            "open fs read stream must count as active database work"
        );

        drop(reader);
        let mut dropping = crate::sql::session::db_connections::db_connection_registry()
            .try_mark_dropping(&keyspace, db_id)
            .expect("drop can start once the fs read stream is gone");
        dropping.commit();
    }

    #[tokio::test]
    async fn default_tenant_lifecycle_operation_blocks_uppercase_drop_keyspace() {
        let (_rec, inner) = RecordingBackend::new_pair();
        let (_, db_id) = unique_database_identity();
        let adapter = NormalizingFsBackend::new_with_lifecycle_admission(
            "default",
            inner,
            Arc::new(ToggleAdmission::with_database_id(true, db_id)),
        );

        let reader = adapter.read_file_stream("tests/x", 1024).await.unwrap();
        assert!(
            matches!(
                crate::sql::session::db_connections::db_connection_registry()
                    .try_mark_dropping("DEFAULT", db_id),
                Err(1)
            ),
            "default-tenant fs operations must block DROP DATABASE even when DROP uses the store keyspace casing"
        );

        drop(reader);
        let mut dropping = crate::sql::session::db_connections::db_connection_registry()
            .try_mark_dropping("DEFAULT", db_id)
            .expect("drop can start once the default-tenant fs stream is gone");
        dropping.commit();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn operation_metrics_use_constructor_keyspace_outside_statement_context() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let (_rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);
        adapter.stat("tests/x").await.unwrap();

        let rendered = handle.render();
        assert!(
            rendered.contains(r#"db9_fs9_operation_duration_seconds{keyspace="tenant_metrics""#)
                || rendered.contains(
                    r#"db9_fs9_operation_duration_seconds_bucket{keyspace="tenant_metrics""#
                ),
            "metric must use explicit tenant keyspace, not session fallback: {rendered}"
        );
        assert!(
            !rendered.contains(r#"keyspace="default""#),
            "metric must not fall back to default outside statement context: {rendered}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn operation_metrics_record_previously_uncovered_methods() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let (_rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);
        adapter.append_file("rel/ap", b"x").await.unwrap();
        adapter.rename("old/x", "new/y").await.unwrap();

        let rendered = handle.render();
        assert!(
            rendered.contains(r#"operation="append_file""#),
            "append_file latency should be exported: {rendered}"
        );
        assert!(
            rendered.contains(r#"operation="rename""#),
            "rename latency should be exported: {rendered}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn batch_operation_metrics_distinguish_partial_failures() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let inner: Arc<dyn FsBackend> = Arc::new(PartialBatchBackend);
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);
        let files = vec![
            FsBatchWriteFile {
                path: "ok".to_string(),
                data: b"ok".to_vec(),
                mode: None,
            },
            FsBatchWriteFile {
                path: "bad".to_string(),
                data: b"bad".to_vec(),
                mode: None,
            },
        ];
        let _ = adapter.batch_write(files).await.unwrap();

        let rendered = handle.render();
        assert!(
            rendered.contains(r#"operation="batch_write",result="partial""#),
            "partial per-entry failure must not be exported as ok: {rendered}"
        );
    }

    #[tokio::test]
    async fn adapter_shapes_rename_both_paths() {
        let (rec, inner) = RecordingBackend::new_pair();
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);
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
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);

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
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);
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
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);

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
        let adapter = NormalizingFsBackend::new("tenant_metrics", inner);

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
