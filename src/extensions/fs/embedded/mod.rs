pub(crate) mod blob;
pub(crate) mod bundle;
pub(crate) mod keys;
pub(crate) mod lifecycle;
pub(crate) mod pagefs;
pub(crate) mod types;

use crate::extensions::fs::backend::{
    FsBackend, FsBatchWriteEntry, FsBatchWriteFile, FsCreateUpload, FsFileInfo,
    FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest, FsStorage, FsWriteStream,
    FsWriteStreamOptions,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use pagefs::EmbeddedPageFs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tikv_client::TransactionClient;
use tokio::io::AsyncBufRead;
use tokio::sync::OnceCell;
use types::{DataRef, FsInstanceIdentity, Inode, InodeType};

fn backend_registry(
) -> &'static Mutex<HashMap<FsInstanceIdentity, Arc<OnceCell<Arc<EmbeddedPageFs>>>>> {
    static REGISTRY: OnceLock<
        Mutex<HashMap<FsInstanceIdentity, Arc<OnceCell<Arc<EmbeddedPageFs>>>>>,
    > = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) struct EmbeddedFsBackend {
    pagefs: Arc<EmbeddedPageFs>,
}

impl EmbeddedFsBackend {
    pub(crate) async fn new(client: Arc<TransactionClient>, keyspace: String) -> Result<Self> {
        let superblock = EmbeddedPageFs::load_runtime_superblock(client.clone(), &keyspace).await?;
        let identity = EmbeddedPageFs::identity_from_superblock(&keyspace, &superblock);
        EmbeddedPageFs::register_process_identity(&identity)?;

        let init_cell = {
            let mut registry = match backend_registry().lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            registry
                .entry(identity.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let pagefs = init_cell
            .get_or_try_init(|| {
                let client = client.clone();
                let keyspace = keyspace.clone();
                let superblock = superblock.clone();
                async move {
                    let pagefs = Arc::new(EmbeddedPageFs::new(client, keyspace, &superblock));
                    pagefs.init_filesystem().await?;
                    Ok::<Arc<EmbeddedPageFs>, anyhow::Error>(pagefs)
                }
            })
            .await?
            .clone();
        pagefs.ensure_background_maintenance();

        Ok(Self { pagefs })
    }
}

#[async_trait]
impl FsBackend for EmbeddedFsBackend {
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let inode = self.pagefs.stat(path).await?;
        inode_to_file_info(path, &inode)
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
            .collect::<Result<Vec<_>>>()?)
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
        self.pagefs.read_file_stream(path, max_bytes).await
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

    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        self.pagefs.batch_write(files).await
    }

    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        self.pagefs.begin_write_stream(path, opts).await
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

    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.pagefs.rename(old_path, new_path).await
    }

    async fn create_upload(&self, path: &str, expected_size: u64) -> Result<FsCreateUpload> {
        self.pagefs.create_upload(path, expected_size).await
    }

    async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
    ) -> Result<FsPresignedRequest> {
        self.pagefs
            .presign_upload_part(upload_token, part_number)
            .await
    }

    async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        self.pagefs
            .complete_upload(upload_token, parts, checksum)
            .await
    }

    async fn abort_upload(&self, upload_token: &str) -> Result<()> {
        self.pagefs.abort_upload(upload_token).await
    }

    async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload> {
        self.pagefs.prepare_download(path).await
    }
}

fn inode_to_file_info(path: &str, inode: &Inode) -> Result<FsFileInfo> {
    let (storage, sealed) = if inode.inode_type == InodeType::Directory {
        (None, Some(false))
    } else {
        match &inode.data {
            DataRef::InlineBlob => (Some(FsStorage::Inline), Some(false)),
            DataRef::PackEntry { .. } => (Some(FsStorage::Pack), Some(true)),
            DataRef::Object { .. } => (Some(FsStorage::Object), Some(true)),
            DataRef::None => (None, Some(false)),
            DataRef::StagingPages => {
                return Err(anyhow!(types::EmbeddedFsError::internal(&format!(
                    "published path {path} exposed internal staging data"
                ))))
            }
        }
    };

    Ok(FsFileInfo {
        path: path.to_string(),
        is_dir: inode.inode_type == InodeType::Directory,
        is_symlink: false,
        size: inode.size,
        mode: inode.mode,
        mtime: inode.mtime as u64,
        storage,
        sealed,
    })
}

fn normalize_dir_path(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path.trim_end_matches('/')
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn backend_registry_keeps_process_seen_instances() {
        let first = FsInstanceIdentity::new("tenant".to_string(), [1u8; 16]);
        let second = FsInstanceIdentity::new("tenant".to_string(), [2u8; 16]);
        let mut registry = HashMap::<FsInstanceIdentity, Arc<OnceCell<Arc<EmbeddedPageFs>>>>::new();
        registry.insert(first.clone(), Arc::new(OnceCell::new()));
        registry.insert(second.clone(), Arc::new(OnceCell::new()));

        assert!(registry.contains_key(&first));
        assert!(registry.contains_key(&second));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_to_file_info_maps_supported_storage_classes() {
        let mut inode = Inode::new_file(7, 0o644);
        inode.data = DataRef::Object {
            key: "fs9/test".to_string(),
            version: 7,
            checksum: [0u8; 32],
        };
        let info = inode_to_file_info("/data.bin", &inode).expect("object inode must map");
        assert_eq!(info.storage, Some(FsStorage::Object));
        assert_eq!(info.sealed, Some(true));
    }

    #[test]
    fn inode_to_file_info_rejects_internal_staging_data() {
        let mut inode = Inode::new_file(9, 0o644);
        inode.data = DataRef::StagingPages;
        let err = inode_to_file_info("/staging.bin", &inode)
            .expect_err("staging data must stay internal");
        assert!(
            err.to_string().contains("exposed internal staging data"),
            "unexpected error: {err}"
        );
    }
}
