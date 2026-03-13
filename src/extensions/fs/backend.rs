use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufRead;

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
    pub mtime: u64,
    // Optional fs9 storage metadata. Unset for directories and empty files.
    pub storage: Option<FsStorage>,
    pub sealed: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct FsBatchWriteFile {
    pub path: String,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct FsBatchWriteEntry {
    pub path: String,
    pub result: Result<usize>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FsWriteStreamOptions {
    pub expected_size: Option<u64>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsMultipartCompletedPart {
    pub part_number: i32,
    pub etag: String,
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
    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>>;
    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>>;
    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>>;
    async fn remove(&self, path: &str) -> Result<()>;
    async fn remove_recursive(&self, path: &str) -> Result<u64>;
    async fn mkdir(&self, path: &str, recursive: bool) -> Result<()>;
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize>;
    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        let mut entries = Vec::with_capacity(files.len());
        for file in files {
            let path = file.path;
            let result = self.write_file(&path, &file.data).await;
            entries.push(FsBatchWriteEntry { path, result });
        }
        Ok(entries)
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
    async fn create_upload(&self, path: &str, expected_size: u64) -> Result<FsCreateUpload>;
    async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
    ) -> Result<FsPresignedRequest>;
    async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize>;
    async fn abort_upload(&self, upload_token: &str) -> Result<()>;
    async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload>;
}

pub(crate) fn is_backend_available() -> bool {
    crate::extensions::context::tikv_client().is_some()
}

pub(crate) async fn get_backend(tenant_keyspace: &str) -> Result<Box<dyn FsBackend>> {
    let client = crate::extensions::context::tikv_client().ok_or_else(|| {
        anyhow!(
            "fs9: TiKV client not available in extension context. \
             Ensure the caller wraps this in with_context_opts()."
        )
    })?;
    EmbeddedFsBackend::new(client, tenant_keyspace.to_string())
        .await
        .map(|b| Box::new(b) as Box<dyn FsBackend>)
        .map_err(|e| anyhow!("fs9: failed to init embedded backend: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_backend_without_context_returns_error() {
        match get_backend("tenant_a").await {
            Ok(_) => panic!("missing extension context must return error"),
            Err(err) => assert!(
                err.to_string().contains("TiKV client not available"),
                "unexpected error: {err}"
            ),
        }
    }
}
