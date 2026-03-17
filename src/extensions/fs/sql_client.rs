use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::extensions::context;
use crate::extensions::fs::backend::{self, FsBackend, FsFileInfo};
use crate::extensions::fs::glob;
use crate::extensions::fs::{MAX_BYTES_PER_FILE, MAX_FILES_PER_GLOB};

pub(crate) struct SqlFsClient {
    backend: Arc<dyn FsBackend>,
}

impl SqlFsClient {
    // SqlFsClient is a thin statement-scoped adapter over the raw fs backend.
    // It centralizes backend acquisition/caching and text-vs-bytea read rules,
    // but it must not introduce SQL-only write semantics or a separate namespace.
    pub(crate) async fn from_context() -> Result<Self> {
        let tenant = context::tenant_keyspace()
            .ok_or_else(|| anyhow!("fs9: tenant keyspace not available in extension context"))?;
        let backend = backend::acquire_statement_backend(&tenant).await?;
        Ok(Self { backend })
    }

    pub(crate) async fn read_bytes(&self, path: &str) -> Result<Vec<u8>> {
        self.backend.read_file(path, MAX_BYTES_PER_FILE).await
    }

    pub(crate) async fn read_bytes_at(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.backend.read_file_at(path, offset, length).await
    }

    pub(crate) async fn read_text(&self, path: &str) -> Result<String> {
        decode_utf8(self.read_bytes(path).await?, "fs9_read", "fs9_read_bytea")
    }

    pub(crate) async fn read_text_at(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<String> {
        decode_utf8(
            self.read_bytes_at(path, offset, length).await?,
            "fs9_read_at",
            "fs9_read_at_bytea",
        )
    }

    pub(crate) async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        self.backend.write_file(path, data).await
    }

    pub(crate) async fn write_file_at(
        &self,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<usize> {
        self.backend.write_file_at(path, offset, data).await
    }

    pub(crate) async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        self.backend.append_file(path, data).await
    }

    pub(crate) async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        self.backend.truncate(path, size).await
    }

    pub(crate) async fn exists(&self, path: &str) -> Result<bool> {
        match self.backend.stat(path).await {
            Ok(_) => Ok(true),
            Err(err) if backend::is_not_found_error(&err) => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub(crate) async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        self.backend.stat(path).await
    }

    pub(crate) async fn mkdir(&self, path: &str, recursive: bool) -> Result<()> {
        self.backend.mkdir(path, recursive).await
    }

    pub(crate) async fn remove(&self, path: &str, recursive: bool) -> Result<i64> {
        if glob::is_glob_pattern(path) {
            let files =
                glob::expand_glob(self.backend.as_ref(), path, MAX_FILES_PER_GLOB, None).await?;
            let mut count = 0i64;
            for file in files {
                if recursive {
                    count += self.backend.remove_recursive(&file).await? as i64;
                } else {
                    self.backend.remove(&file).await?;
                    count += 1;
                }
            }
            return Ok(count);
        }

        if recursive {
            Ok(self.backend.remove_recursive(path).await? as i64)
        } else {
            self.backend.remove(path).await?;
            Ok(1)
        }
    }
}

fn decode_utf8(bytes: Vec<u8>, fn_name: &str, binary_fn_name: &str) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| {
        anyhow!("{fn_name}: file is not valid UTF-8; use {binary_fn_name} for binary data")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context;
    use crate::extensions::fs::backend::{
        FsCreateUpload, FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest,
        FsStorage, FsWriteStream, FsWriteStreamOptions,
    };
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::io::{AsyncBufRead, BufReader};

    struct MockWriteStream;

    #[async_trait]
    impl FsWriteStream for MockWriteStream {
        async fn write_chunk(&mut self, _chunk: &[u8]) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn finish(self: Box<Self>) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort(self: Box<Self>) -> Result<()> {
            anyhow::bail!("not implemented")
        }
    }

    #[derive(Clone)]
    struct MockFile {
        bytes: Vec<u8>,
        sealed: bool,
        is_dir: bool,
    }

    struct MockBackend {
        files: Mutex<HashMap<String, MockFile>>,
        last_read_at_len: AtomicUsize,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                last_read_at_len: AtomicUsize::new(0),
            }
        }

        fn insert_file(&self, path: &str, bytes: Vec<u8>, sealed: bool) {
            self.files.lock().unwrap().insert(
                path.to_string(),
                MockFile {
                    bytes,
                    sealed,
                    is_dir: false,
                },
            );
        }
    }

    #[async_trait]
    impl FsBackend for MockBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            let files = self.files.lock().unwrap();
            let file = files.get(path).ok_or_else(|| {
                anyhow!(
                    crate::extensions::fs::embedded::types::EmbeddedFsError::NotFound(
                        path.to_string()
                    )
                )
            })?;
            Ok(FsFileInfo {
                path: path.to_string(),
                is_dir: file.is_dir,
                is_symlink: false,
                size: file.bytes.len() as u64,
                mode: if file.is_dir { 0o755 } else { 0o644 },
                mtime: 0,
                storage: if file.is_dir {
                    None
                } else if file.sealed {
                    Some(FsStorage::Pack)
                } else {
                    Some(FsStorage::Inline)
                },
                sealed: Some(file.sealed),
            })
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
            anyhow::bail!("not implemented")
        }

        async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
            let file = self
                .files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        crate::extensions::fs::embedded::types::EmbeddedFsError::NotFound(
                            path.to_string()
                        )
                    )
                })?;
            if file.bytes.len() > max_bytes {
                anyhow::bail!("too large")
            }
            Ok(file.bytes)
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(BufReader::new(tokio::io::empty())))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            anyhow::bail!("not implemented")
        }

        async fn mkdir(&self, _path: &str, _recursive: bool) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize> {
            self.files.lock().unwrap().insert(
                path.to_string(),
                MockFile {
                    bytes: data.to_vec(),
                    sealed: false,
                    is_dir: false,
                },
            );
            Ok(data.len())
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            Ok(Box::new(MockWriteStream))
        }

        async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
            self.last_read_at_len.store(length, Ordering::Relaxed);
            let file = self
                .files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        crate::extensions::fs::embedded::types::EmbeddedFsError::NotFound(
                            path.to_string()
                        )
                    )
                })?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= file.bytes.len() || length == 0 {
                return Ok(Vec::new());
            }
            let end = start.saturating_add(length).min(file.bytes.len());
            Ok(file.bytes[start..end].to_vec())
        }

        async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
            let mut files = self.files.lock().unwrap();
            let file = files.entry(path.to_string()).or_insert(MockFile {
                bytes: Vec::new(),
                sealed: false,
                is_dir: false,
            });
            if file.sealed {
                anyhow::bail!("partial mutation is not supported for sealed files");
            }
            let start = usize::try_from(offset).map_err(|_| anyhow!("offset exceeds memory"))?;
            let end = start
                .checked_add(data.len())
                .ok_or_else(|| anyhow!("write range exceeds memory"))?;
            if file.bytes.len() < end {
                file.bytes.resize(end, 0);
            }
            file.bytes[start..end].copy_from_slice(data);
            Ok(data.len())
        }

        async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
            let mut files = self.files.lock().unwrap();
            let file = files.entry(path.to_string()).or_insert(MockFile {
                bytes: Vec::new(),
                sealed: false,
                is_dir: false,
            });
            if file.sealed {
                anyhow::bail!("append is not supported for sealed files");
            }
            file.bytes.extend_from_slice(data);
            Ok(data.len())
        }

        async fn truncate(&self, path: &str, size: u64) -> Result<()> {
            let mut files = self.files.lock().unwrap();
            let file = files.get_mut(path).ok_or_else(|| {
                anyhow!(
                    crate::extensions::fs::embedded::types::EmbeddedFsError::NotFound(
                        path.to_string()
                    )
                )
            })?;
            if file.sealed {
                anyhow::bail!("truncate is not supported for sealed files");
            }
            let size = usize::try_from(size).map_err(|_| anyhow!("size exceeds memory"))?;
            if file.bytes.len() > size {
                file.bytes.truncate(size);
            } else if file.bytes.len() < size {
                file.bytes.resize(size, 0);
            }
            Ok(())
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn create_upload(&self, _path: &str, _expected_size: u64) -> Result<FsCreateUpload> {
            anyhow::bail!("not implemented")
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
        ) -> Result<FsPresignedRequest> {
            anyhow::bail!("not implemented")
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            anyhow::bail!("not implemented")
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            anyhow::bail!("not implemented")
        }
    }

    #[tokio::test]
    async fn from_context_reuses_cached_backend_within_statement() {
        let backend: Arc<dyn FsBackend> = Arc::new(MockBackend::new());

        context::with_context(true, "tenant_a", async {
            context::cache_fs_backend(backend.clone()).expect("cache backend");

            let first = SqlFsClient::from_context()
                .await
                .expect("first acquire from context");
            let second = SqlFsClient::from_context()
                .await
                .expect("second acquire from context");

            assert!(Arc::ptr_eq(&first.backend, &second.backend));
            assert!(Arc::ptr_eq(&first.backend, &backend));
        })
        .await;
    }

    #[tokio::test]
    async fn read_text_rejects_invalid_utf8() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/bad.bin", vec![0xff, 0xfe], false);
        let client = SqlFsClient { backend };

        let err = client
            .read_text("/bad.bin")
            .await
            .expect_err("invalid UTF-8 must fail");
        assert!(err.to_string().contains("fs9_read_bytea"));
    }

    #[tokio::test]
    async fn read_bytes_at_passes_requested_length_through() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/tiny.bin", b"tiny".to_vec(), false);
        let client = SqlFsClient {
            backend: backend.clone(),
        };

        let data = client
            .read_bytes_at("/tiny.bin", 0, MAX_BYTES_PER_FILE.saturating_add(123))
            .await
            .expect("raw read_at");

        assert_eq!(data, b"tiny".to_vec());
        assert_eq!(
            backend.last_read_at_len.load(Ordering::Relaxed),
            MAX_BYTES_PER_FILE.saturating_add(123)
        );
    }

    #[tokio::test]
    async fn read_text_at_rejects_invalid_utf8_window() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/utf8.txt", "café".as_bytes().to_vec(), false);
        let client = SqlFsClient { backend };

        let err = client
            .read_text_at("/utf8.txt", 4, 1)
            .await
            .expect_err("misaligned utf-8 slice must fail");
        assert!(err.to_string().contains("fs9_read_at_bytea"));
    }

    #[tokio::test]
    async fn write_file_at_preserves_sealed_mutation_boundary() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"abcdef".to_vec(), true);
        let client = SqlFsClient { backend };

        let err = client
            .write_file_at("/sealed.bin", 0, b"Z")
            .await
            .expect_err("sealed partial mutation must be rejected");
        assert!(
            err.to_string()
                .contains("partial mutation is not supported for sealed files"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn append_file_preserves_sealed_mutation_boundary() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"abcdef".to_vec(), true);
        let client = SqlFsClient { backend };

        let err = client
            .append_file("/sealed.bin", b"Z")
            .await
            .expect_err("sealed append must be rejected");
        assert!(
            err.to_string()
                .contains("append is not supported for sealed files"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn truncate_preserves_sealed_mutation_boundary() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"abcdef".to_vec(), true);
        let client = SqlFsClient { backend };

        let err = client
            .truncate("/sealed.bin", 1)
            .await
            .expect_err("sealed truncate must be rejected");
        assert!(
            err.to_string()
                .contains("truncate is not supported for sealed files"),
            "unexpected error: {err}"
        );
    }
}
