use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::io::AsyncBufRead;

use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::embedded::EmbeddedFsBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FsStorage {
    Inline,
    Pack,
    Object,
}

#[derive(Debug, Clone)]
pub(crate) struct FsFileInfo {
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub generation: u64,
    pub mtime: u64,
    // Optional fs9 storage metadata. Unset for directories and empty files.
    pub storage: Option<FsStorage>,
    pub sealed: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct FsBatchWriteFile {
    pub path: String,
    pub data: Vec<u8>,
    pub mode: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct FsBatchWriteEntry {
    pub path: String,
    pub result: Result<usize>,
    /// Stable machine-readable failure category for execution-level errors.
    /// Set by the backend when a subgroup commit fails (e.g., "execution.txn_conflict").
    /// `None` for successful entries or planner-level errors (handled separately).
    pub failure_category: Option<&'static str>,
}

/// Result of a grouped atomic batch write, including execution metadata.
#[derive(Debug)]
pub(crate) struct FsBatchWriteGroupedResult {
    pub entries: Vec<FsBatchWriteEntry>,
    /// Number of subgroup transactions actually executed by the backend.
    /// This accounts for chunking within large directory groups.
    pub actual_subgroup_count: usize,
    /// Total number of server-side retries across all subgroups (txn_conflict).
    pub total_retries: usize,
    /// Number of subgroups where retries were exhausted (still failed after all attempts).
    pub retries_exhausted: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct FsRecursiveReaddirOptions {
    pub max_depth: usize,
    pub max_entries: usize,
    pub exclude_set: Option<Arc<globset::GlobSet>>,
}

#[derive(Debug, Clone)]
pub(crate) struct FsRecursiveReaddirResult {
    pub entries: Vec<FsFileInfo>,
    pub truncated: bool,
    pub total_dirs_scanned: usize,
}

pub(crate) fn batch_inline_read_entry_too_large_error(
    size: u64,
    max_file_bytes: usize,
) -> anyhow::Error {
    anyhow!(EmbeddedFsError::too_large(format!(
        "file too large for batch_inline_read: {} bytes exceeds limit {}",
        size, max_file_bytes
    )))
}

pub(crate) fn batch_inline_read_payload_too_large_error(
    _total_planned: u64,
    max_total_bytes: usize,
) -> anyhow::Error {
    anyhow!(EmbeddedFsError::too_large(format!(
        "batch_inline_read raw payload exceeds limit {} bytes",
        max_total_bytes
    )))
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FsWriteStreamOptions {
    pub expected_size: Option<u64>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsPresignedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsCreateUpload {
    pub upload_token: String,
    pub upload_id: String,
    pub part_size: usize,
    pub expires_at: i64,
    pub checksum_algorithm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsMultipartCompletedPart {
    pub part_number: i32,
    pub etag: String,
    pub checksum_crc32c: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsPreparedDownload {
    pub request: FsPresignedRequest,
    pub size: u64,
    pub storage: FsStorage,
    pub range_supported: bool,
}

#[async_trait]
pub(crate) trait FsWriteStream: Send {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()>;
    async fn finish(self: Box<Self>) -> Result<usize>;
    async fn abort(self: Box<Self>) -> Result<()>;
}

#[async_trait]
pub(crate) trait FsBackend: Send + Sync {
    async fn stat(&self, path: &str) -> Result<FsFileInfo>;
    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            entries.push(self.stat(path).await);
        }
        Ok(entries)
    }
    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>>;
    async fn batch_readdir(&self, paths: &[String]) -> Result<Vec<Result<Vec<FsFileInfo>>>> {
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            entries.push(self.readdir(path).await);
        }
        Ok(entries)
    }
    async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<FsRecursiveReaddirResult> {
        if opts.max_entries == 0 {
            return Ok(FsRecursiveReaddirResult {
                entries: Vec::new(),
                truncated: false,
                total_dirs_scanned: 0,
            });
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        let mut depth_exhausted = false;
        let mut total_dirs_scanned = 0usize;
        let root = normalize_readdir_path(path);
        let mut frontier = VecDeque::from([(root.clone(), 0usize)]);
        let mut visited = HashSet::from([root]);

        while let Some((_, depth)) = frontier.front() {
            if entries.len() >= opts.max_entries {
                truncated = true;
                break;
            }

            let current_depth = *depth;
            let mut level_paths = Vec::new();
            while matches!(frontier.front(), Some((_, level_depth)) if *level_depth == current_depth)
            {
                let (dir_path, dir_depth) = frontier
                    .pop_front()
                    .expect("frontier entry must exist while draining current level");
                level_paths.push((dir_path, dir_depth));
            }

            let paths = level_paths
                .iter()
                .map(|(dir_path, _)| dir_path.clone())
                .collect::<Vec<_>>();
            let results = self.batch_readdir(&paths).await?;
            if results.len() != level_paths.len() {
                return Err(anyhow!(EmbeddedFsError::internal(&format!(
                    "batch_readdir returned {} results for {} input paths",
                    results.len(),
                    level_paths.len()
                ))));
            }

            total_dirs_scanned = total_dirs_scanned.saturating_add(level_paths.len());
            for ((_, dir_depth), result) in level_paths.into_iter().zip(results) {
                let dir_entries = result?;
                for entry in dir_entries {
                    if opts.exclude_set.as_deref().is_some_and(|set| {
                        crate::extensions::fs::glob::path_matches_exclude(&entry.path, set)
                    }) {
                        continue;
                    }

                    if entry.is_dir && !entry.is_symlink {
                        if dir_depth < opts.max_depth && visited.insert(entry.path.clone()) {
                            frontier.push_back((entry.path.clone(), dir_depth + 1));
                        } else if dir_depth >= opts.max_depth {
                            depth_exhausted = true;
                        }
                    }

                    if entries.len() >= opts.max_entries {
                        truncated = true;
                        break;
                    }

                    entries.push(entry);
                }

                if truncated {
                    break;
                }
            }

            if truncated {
                break;
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(FsRecursiveReaddirResult {
            entries,
            truncated: truncated || depth_exhausted,
            total_dirs_scanned,
        })
    }
    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>>;
    async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        let mut entries: Vec<Option<Result<Vec<u8>>>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut eligible = Vec::new();
        let mut total_planned = 0u64;

        for (idx, path) in paths.iter().enumerate() {
            match self.stat(path).await {
                Ok(info) => {
                    if info.is_dir {
                        entries[idx] = Some(Err(anyhow!(EmbeddedFsError::is_directory(path))));
                        continue;
                    }

                    if info.is_symlink {
                        entries[idx] = Some(Err(anyhow!(EmbeddedFsError::InvalidInput(
                            "cannot read symlink as file; use readlink".to_string(),
                        ))));
                        continue;
                    }

                    if info.size > max_file_bytes as u64 {
                        entries[idx] = Some(Err(batch_inline_read_entry_too_large_error(
                            info.size,
                            max_file_bytes,
                        )));
                        continue;
                    }

                    total_planned = total_planned.saturating_add(info.size);
                    eligible.push(idx);
                }
                Err(err) => entries[idx] = Some(Err(err)),
            }
        }

        if total_planned > max_total_bytes as u64 {
            return Err(batch_inline_read_payload_too_large_error(
                total_planned,
                max_total_bytes,
            ));
        }

        for idx in eligible {
            let path = &paths[idx];
            entries[idx] = Some(self.read_file(path, max_file_bytes).await);
        }

        Ok(entries
            .into_iter()
            .map(|entry| {
                entry.expect("batch_inline_read default implementation must fill every entry")
            })
            .collect())
    }
    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>>;
    async fn remove(&self, path: &str) -> Result<()>;
    async fn remove_recursive(&self, path: &str) -> Result<u64>;
    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()>;
    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize>;
    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        let mut entries = Vec::with_capacity(files.len());
        for file in files {
            let path = file.path;
            let result = self.write_file(&path, &file.data, file.mode).await;
            entries.push(FsBatchWriteEntry {
                path,
                result,
                failure_category: None,
            });
        }
        Ok(entries)
    }
    /// Whether this backend supports the `batch_write_atomic` operation with
    /// per-subgroup atomic semantics. Backends that return `false` must not
    /// have `batch_write_grouped` called on them.
    fn supports_batch_write_atomic(&self) -> bool {
        false
    }
    /// Grouped atomic batch write: files are grouped by parent directory and
    /// each subgroup (bounded by `grouped_write_subgroup_size`) is committed
    /// in a single transaction. Per-subgroup atomic semantics.
    /// Only call this if `supports_batch_write_atomic()` returns true.
    async fn batch_write_grouped(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<FsBatchWriteGroupedResult> {
        let _ = files;
        Err(anyhow!(
            "batch_write_grouped is not supported by this backend"
        ))
    }
    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>>;
    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>>;
    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize>;
    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize>;
    async fn truncate(&self, path: &str, size: u64) -> Result<()>;
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()>;
    async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
        mode: Option<u32>,
        checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload>;
    async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
        checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest>;
    async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize>;
    async fn abort_upload(&self, upload_token: &str) -> Result<()>;
    async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload>;
    async fn symlink(&self, path: &str, target: &str) -> Result<()>;
    async fn readlink(&self, path: &str) -> Result<String>;
    async fn chmod(&self, path: &str, mode: u32) -> Result<()>;
}

pub(crate) fn is_backend_available() -> bool {
    crate::extensions::context::tikv_client().is_some()
        || crate::extensions::context::cached_fs_backend().is_some()
}

pub(crate) fn is_not_found_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<EmbeddedFsError>(),
        Some(EmbeddedFsError::NotFound(_))
    )
}

fn normalize_readdir_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    }
}

async fn init_backend(tenant_keyspace: &str) -> Result<Arc<dyn FsBackend>> {
    let client = crate::extensions::context::tikv_client().ok_or_else(|| {
        anyhow!(
            "fs9: TiKV client not available in extension context. \
             Ensure the caller wraps this in with_context_opts()."
        )
    })?;
    EmbeddedFsBackend::new(client, tenant_keyspace.to_string())
        .await
        .map(|b| Arc::new(b) as Arc<dyn FsBackend>)
        .map_err(|e| anyhow!("fs9: failed to init embedded backend: {e}"))
}

/// Acquire the authoritative fs9 backend for the current statement.
///
/// All SQL fs9 entry points must reuse this helper so a statement either
/// shares one bound backend or fails under one consistent contract.
pub(crate) async fn acquire_statement_backend(tenant_keyspace: &str) -> Result<Arc<dyn FsBackend>> {
    if let Some(backend) = crate::extensions::context::cached_fs_backend() {
        return Ok(backend);
    }

    let backend = init_backend(tenant_keyspace).await?;
    crate::extensions::context::cache_fs_backend(backend.clone())?;
    Ok(backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tokio::io::{empty, AsyncBufRead};

    struct BatchInlineReadTestBackend {
        stats: HashMap<String, Result<FsFileInfo>>,
        files: HashMap<String, Result<Vec<u8>>>,
    }

    struct RecursiveReaddirTestBackend {
        dirs: HashMap<String, Result<Vec<FsFileInfo>>>,
    }

    impl BatchInlineReadTestBackend {
        fn new(
            stats: HashMap<String, Result<FsFileInfo>>,
            files: HashMap<String, Result<Vec<u8>>>,
        ) -> Self {
            Self { stats, files }
        }
    }

    impl RecursiveReaddirTestBackend {
        fn new(dirs: HashMap<String, Result<Vec<FsFileInfo>>>) -> Self {
            Self { dirs }
        }
    }

    #[async_trait]
    impl FsBackend for BatchInlineReadTestBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            match self.stats.get(path) {
                Some(Ok(info)) => Ok(info.clone()),
                Some(Err(err)) => Err(anyhow!(err.to_string())),
                None => Err(anyhow!(EmbeddedFsError::not_found(path))),
            }
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
            unreachable!("readdir is not used in these tests");
        }

        async fn read_file(&self, path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            match self.files.get(path) {
                Some(Ok(data)) => Ok(data.clone()),
                Some(Err(err)) => Err(anyhow!(err.to_string())),
                None => Err(anyhow!(EmbeddedFsError::not_found(path))),
            }
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(empty()))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            unreachable!("remove is not used in these tests");
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            unreachable!("remove_recursive is not used in these tests");
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            unreachable!("mkdir is not used in these tests");
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            unreachable!("write_file is not used in these tests");
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            unreachable!("begin_write_stream is not used in these tests");
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            unreachable!("read_file_at is not used in these tests");
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            unreachable!("write_file_at is not used in these tests");
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            unreachable!("append_file is not used in these tests");
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            unreachable!("truncate is not used in these tests");
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            unreachable!("rename is not used in these tests");
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            unreachable!("create_upload is not used in these tests");
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            unreachable!("presign_upload_part is not used in these tests");
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            unreachable!("complete_upload is not used in these tests");
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            unreachable!("abort_upload is not used in these tests");
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            unreachable!("prepare_download is not used in these tests");
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            unreachable!("symlink is not used in these tests");
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            unreachable!("readlink is not used in these tests");
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    #[async_trait]
    impl FsBackend for RecursiveReaddirTestBackend {
        async fn stat(&self, _path: &str) -> Result<FsFileInfo> {
            unreachable!("stat is not used in these tests");
        }

        async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
            match self.dirs.get(path) {
                Some(Ok(entries)) => Ok(entries.clone()),
                Some(Err(err)) => Err(anyhow!(err.to_string())),
                None => Err(anyhow!(EmbeddedFsError::not_found(path))),
            }
        }

        async fn read_file(&self, _path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            unreachable!("read_file is not used in these tests");
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(empty()))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            unreachable!("remove is not used in these tests");
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            unreachable!("remove_recursive is not used in these tests");
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            unreachable!("mkdir is not used in these tests");
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            unreachable!("write_file is not used in these tests");
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            unreachable!("begin_write_stream is not used in these tests");
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            unreachable!("read_file_at is not used in these tests");
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            unreachable!("write_file_at is not used in these tests");
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            unreachable!("append_file is not used in these tests");
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            unreachable!("truncate is not used in these tests");
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            unreachable!("rename is not used in these tests");
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            unreachable!("create_upload is not used in these tests");
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            unreachable!("presign_upload_part is not used in these tests");
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            unreachable!("complete_upload is not used in these tests");
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            unreachable!("abort_upload is not used in these tests");
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            unreachable!("prepare_download is not used in these tests");
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            unreachable!("symlink is not used in these tests");
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            unreachable!("readlink is not used in these tests");
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    #[tokio::test]
    async fn acquire_statement_backend_without_context_returns_error() {
        match acquire_statement_backend("tenant_a").await {
            Ok(_) => panic!("missing extension context must return error"),
            Err(err) => assert!(
                err.to_string().contains("TiKV client not available"),
                "unexpected error: {err}"
            ),
        }
    }

    #[test]
    fn is_not_found_error_matches_embedded_error_type() {
        let err = anyhow!(EmbeddedFsError::NotFound("/missing".to_string()));
        assert!(is_not_found_error(&err));

        let other = anyhow!(EmbeddedFsError::InvalidInput("bad".to_string()));
        assert!(!is_not_found_error(&other));
    }

    #[tokio::test]
    async fn init_backend_without_context_returns_error() {
        match init_backend("tenant_a").await {
            Ok(_) => panic!("missing extension context must return error"),
            Err(err) => assert!(
                err.to_string().contains("TiKV client not available"),
                "unexpected error: {err}"
            ),
        }
    }

    #[tokio::test]
    async fn default_batch_inline_read_preserves_mixed_entry_results() {
        let backend = BatchInlineReadTestBackend::new(
            HashMap::from([
                (
                    "/ok".to_string(),
                    Ok(FsFileInfo {
                        path: "/ok".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
                (
                    "/dir".to_string(),
                    Ok(FsFileInfo {
                        path: "/dir".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    }),
                ),
                (
                    "/large".to_string(),
                    Ok(FsFileInfo {
                        path: "/large".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 99,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
                (
                    "/missing".to_string(),
                    Err(anyhow!(EmbeddedFsError::not_found("/missing"))),
                ),
            ]),
            HashMap::from([("/ok".to_string(), Ok(b"hello".to_vec()))]),
        );

        let results = backend
            .batch_inline_read(
                &[
                    "/ok".to_string(),
                    "/dir".to_string(),
                    "/large".to_string(),
                    "/missing".to_string(),
                ],
                8,
                64,
            )
            .await
            .expect("batch_inline_read should succeed");

        assert_eq!(results.len(), 4);
        assert_eq!(results[0].as_ref().unwrap(), b"hello");
        assert!(results[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("IsDirectory"));
        let too_large = results[2]
            .as_ref()
            .unwrap_err()
            .downcast_ref::<EmbeddedFsError>()
            .expect("entry too large should be typed");
        match too_large {
            EmbeddedFsError::TooLarge(msg) => assert_eq!(
                msg,
                "file too large for batch_inline_read: 99 bytes exceeds limit 8"
            ),
            other => panic!("expected typed too-large error, got {other}"),
        }
        assert!(results[3]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("NotFound"));
    }

    #[tokio::test]
    async fn default_batch_inline_read_rejects_symlink_entries() {
        let backend = BatchInlineReadTestBackend::new(
            HashMap::from([
                (
                    "/link".to_string(),
                    Ok(FsFileInfo {
                        path: "/link".to_string(),
                        is_dir: false,
                        is_symlink: true,
                        size: 11,
                        mode: 0o777,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    }),
                ),
                (
                    "/ok".to_string(),
                    Ok(FsFileInfo {
                        path: "/ok".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
            ]),
            HashMap::from([
                ("/link".to_string(), Ok(b"target-path".to_vec())),
                ("/ok".to_string(), Ok(b"hello".to_vec())),
            ]),
        );

        let results = backend
            .batch_inline_read(&["/link".to_string(), "/ok".to_string()], 32, 64)
            .await
            .expect("batch_inline_read should succeed with per-entry errors");

        assert_eq!(results.len(), 2);
        let symlink_err = results[0]
            .as_ref()
            .unwrap_err()
            .downcast_ref::<EmbeddedFsError>()
            .expect("symlink entry should remain a typed error");
        assert!(matches!(
            symlink_err,
            EmbeddedFsError::InvalidInput(msg) if msg == "cannot read symlink as file; use readlink"
        ));
        assert_eq!(results[1].as_ref().unwrap(), b"hello");
    }

    #[tokio::test]
    async fn default_batch_inline_read_rejects_total_payload_over_limit() {
        let backend = BatchInlineReadTestBackend::new(
            HashMap::from([
                (
                    "/a".to_string(),
                    Ok(FsFileInfo {
                        path: "/a".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 4,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
                (
                    "/b".to_string(),
                    Ok(FsFileInfo {
                        path: "/b".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
            ]),
            HashMap::from([
                ("/a".to_string(), Ok(vec![1, 2, 3, 4])),
                ("/b".to_string(), Ok(vec![5, 6, 7, 8, 9])),
            ]),
        );

        let err = backend
            .batch_inline_read(&["/a".to_string(), "/b".to_string()], 8, 8)
            .await
            .expect_err("batch_inline_read must reject payloads that exceed the total cap");
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("payload over limit should be typed");
        assert!(matches!(fs_err, EmbeddedFsError::TooLarge(_)));
        assert_eq!(
            fs_err.to_string(),
            "embedded_fs: TooLarge: batch_inline_read raw payload exceeds limit 8 bytes"
        );
    }

    #[tokio::test]
    async fn default_batch_readdir_preserves_per_path_results() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([
            (
                "/".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/root.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 4,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
            (
                "/missing".to_string(),
                Err(anyhow!(EmbeddedFsError::not_found("/missing"))),
            ),
        ]));

        let results = backend
            .batch_readdir(&["/".to_string(), "/missing".to_string()])
            .await
            .expect("batch_readdir should succeed");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().len(), 1);
        assert!(results[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("NotFound"));
    }

    #[tokio::test]
    async fn default_readdir_recursive_walks_frontiers_and_caps_entries() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([
            (
                "/".to_string(),
                Ok(vec![
                    FsFileInfo {
                        path: "/alpha.txt".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    },
                    FsFileInfo {
                        path: "/dir".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ]),
            ),
            (
                "/dir".to_string(),
                Ok(vec![
                    FsFileInfo {
                        path: "/dir/bravo.txt".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    },
                    FsFileInfo {
                        path: "/dir/nested".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ]),
            ),
            (
                "/dir/nested".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/dir/nested/charlie.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 7,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
        ]));

        let result = backend
            .readdir_recursive(
                "/",
                FsRecursiveReaddirOptions {
                    max_depth: 8,
                    max_entries: 3,
                    exclude_set: None,
                },
            )
            .await
            .expect("readdir_recursive should succeed");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/alpha.txt", "/dir", "/dir/bravo.txt"]);
        assert!(result.truncated);
        assert_eq!(result.total_dirs_scanned, 2);
    }

    #[tokio::test]
    async fn default_readdir_recursive_prunes_excluded_directories() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([
            (
                "/".to_string(),
                Ok(vec![
                    FsFileInfo {
                        path: "/keep".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                    FsFileInfo {
                        path: "/skip".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ]),
            ),
            (
                "/keep".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/keep/visible.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 7,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
            (
                "/skip".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/skip/hidden.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 6,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
        ]));
        let exclude_set =
            crate::extensions::fs::glob::build_exclude_globset(Some("skip/**")).unwrap();

        let result = backend
            .readdir_recursive(
                "/",
                FsRecursiveReaddirOptions {
                    max_depth: 8,
                    max_entries: 10,
                    exclude_set,
                },
            )
            .await
            .expect("readdir_recursive should succeed");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/keep", "/keep/visible.txt"]);
        assert_eq!(result.total_dirs_scanned, 2);
    }
}
