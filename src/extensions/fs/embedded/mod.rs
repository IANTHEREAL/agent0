pub(crate) mod keys;
pub(crate) mod pagefs;
pub(crate) mod types;

use crate::extensions::fs::backend::{FsBackend, FsFileInfo};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use pagefs::EmbeddedPageFs;
use std::sync::Arc;
use tikv_client::TransactionClient;
use tokio::io::AsyncBufRead;
use types::{Inode, InodeType};

pub(crate) struct EmbeddedFsBackend {
    pagefs: EmbeddedPageFs,
}

impl EmbeddedFsBackend {
    pub(crate) async fn new(client: Arc<TransactionClient>) -> Result<Self> {
        let pagefs = EmbeddedPageFs::new(client);
        pagefs.init_filesystem().await?;
        Ok(Self { pagefs })
    }
}

#[async_trait]
impl FsBackend for EmbeddedFsBackend {
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let inode = self.pagefs.stat(path).await?;
        Ok(inode_to_file_info(path, &inode))
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        let entries = self.pagefs.readdir(path).await?;
        let normalized = normalize_dir_path(path);
        Ok(entries
            .into_iter()
            .map(|(name, inode)| {
                let child_path = if normalized == "/" {
                    format!("/{name}")
                } else {
                    format!("{normalized}/{name}")
                };
                inode_to_file_info(&child_path, &inode)
            })
            .collect())
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let inode = self.pagefs.stat(path).await?;
        if inode.size as usize > max_bytes {
            return Err(anyhow!(
                "fs9: file too large: {path} (exceeded max {max_bytes} bytes)"
            ));
        }
        self.pagefs.read_file(path).await
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        let data = self.read_file(path, max_bytes).await?;
        Ok(Box::new(std::io::Cursor::new(data)))
    }

    async fn remove(&self, path: &str) -> Result<()> {
        self.pagefs.remove(path).await
    }

    async fn remove_recursive(&self, path: &str) -> Result<u64> {
        self.pagefs.remove_recursive(path).await
    }

    async fn mkdir(&self, path: &str, recursive: bool) -> Result<()> {
        self.pagefs.mkdir(path, recursive).await
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        self.pagefs.write_file(path, data).await
    }

    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.pagefs.read_file_at(path, offset, length).await
    }

    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        self.pagefs.write_file_at(path, offset, data).await
    }

    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        self.pagefs.append_file(path, data).await
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        self.pagefs.truncate(path, size).await
    }
}

fn inode_to_file_info(path: &str, inode: &Inode) -> FsFileInfo {
    FsFileInfo {
        path: path.to_string(),
        is_dir: inode.inode_type == InodeType::Directory,
        is_symlink: false,
        size: inode.size,
        mode: inode.mode,
        mtime: inode.mtime as u64,
    }
}

fn normalize_dir_path(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path.trim_end_matches('/')
    }
}
