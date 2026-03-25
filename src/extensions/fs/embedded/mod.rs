pub(crate) mod blob;
pub(crate) mod bundle;
pub(crate) mod keys;
pub(crate) mod lifecycle;
pub(crate) mod pagefs;
pub(crate) mod types;

use crate::extensions::fs::backend::{
    FsBackend, FsBatchWriteEntry, FsBatchWriteFile, FsBatchWriteGroupedResult, FsCreateUpload,
    FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest,
    FsRecursiveReaddirOptions, FsRecursiveReaddirResult, FsStorage, FsWriteStream,
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

type FsRegistry = Mutex<HashMap<FsInstanceIdentity, Arc<OnceCell<Arc<EmbeddedPageFs>>>>>;

fn backend_registry() -> &'static FsRegistry {
    static REGISTRY: OnceLock<FsRegistry> = OnceLock::new();
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

    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        let inodes = self.pagefs.batch_stat(paths).await?;
        map_batch_stat_results(paths, inodes)
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

    async fn batch_readdir(&self, paths: &[String]) -> Result<Vec<Result<Vec<FsFileInfo>>>> {
        let dir_entries = self.pagefs.batch_readdir(paths).await?;
        map_batch_readdir_results(paths, dir_entries)
    }

    async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<FsRecursiveReaddirResult> {
        let result = self.pagefs.readdir_recursive(path, opts).await?;
        Ok(FsRecursiveReaddirResult {
            entries: result
                .entries
                .into_iter()
                .map(|(entry_path, inode)| inode_to_file_info(&entry_path, &inode))
                .collect::<Result<Vec<_>>>()?,
            truncated: result.truncated,
            total_dirs_scanned: result.total_dirs_scanned,
        })
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        self.pagefs.read_file_capped(path, max_bytes).await
    }

    async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        self.pagefs
            .batch_inline_read(paths, max_file_bytes, max_total_bytes)
            .await
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

    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()> {
        self.pagefs.mkdir(path, recursive, mode).await
    }

    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize> {
        self.pagefs.write_file(path, data, mode).await
    }

    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        self.pagefs.batch_write(files).await
    }

    fn supports_batch_write_atomic(&self) -> bool {
        true
    }

    async fn batch_write_grouped(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<FsBatchWriteGroupedResult> {
        self.pagefs.batch_write_grouped(files).await
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

    async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
        mode: Option<u32>,
    ) -> Result<FsCreateUpload> {
        self.pagefs.create_upload(path, expected_size, mode).await
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

    async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        self.pagefs.symlink(path, target).await
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        self.pagefs.readlink(path).await
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        self.pagefs.chmod(path, mode).await
    }
}

fn inode_to_file_info(path: &str, inode: &Inode) -> Result<FsFileInfo> {
    let is_symlink = inode.inode_type == InodeType::Symlink;
    let (storage, sealed) = if inode.inode_type == InodeType::Directory || is_symlink {
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
        is_symlink,
        size: inode.size,
        mode: inode.mode,
        generation: inode.generation,
        mtime: inode.mtime as u64,
        storage,
        sealed,
    })
}

fn map_batch_stat_results(
    paths: &[String],
    inodes: Vec<Result<Inode>>,
) -> Result<Vec<Result<FsFileInfo>>> {
    if inodes.len() != paths.len() {
        return Err(anyhow!(types::EmbeddedFsError::internal(&format!(
            "fs9: batch_stat returned {} results for {} input paths",
            inodes.len(),
            paths.len()
        ))));
    }

    Ok(paths
        .iter()
        .cloned()
        .zip(inodes)
        .map(|(path, inode_result)| {
            inode_result.and_then(|inode| inode_to_file_info(&path, &inode))
        })
        .collect())
}

fn map_batch_readdir_results(
    paths: &[String],
    dir_entries: Vec<Result<Vec<(String, Inode)>>>,
) -> Result<Vec<Result<Vec<FsFileInfo>>>> {
    if dir_entries.len() != paths.len() {
        return Err(anyhow!(types::EmbeddedFsError::internal(&format!(
            "fs9: batch_readdir returned {} results for {} input paths",
            dir_entries.len(),
            paths.len()
        ))));
    }

    Ok(paths
        .iter()
        .zip(dir_entries)
        .map(|(path, entries_result)| {
            let normalized = normalize_dir_path(path);
            entries_result.and_then(|entries| {
                entries
                    .into_iter()
                    .map(|(name, inode)| {
                        let child_path = if normalized == "/" {
                            format!("/{name}")
                        } else {
                            format!("{normalized}/{name}")
                        };
                        inode_to_file_info(&child_path, &inode)
                    })
                    .collect::<Result<Vec<_>>>()
            })
        })
        .collect())
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
        assert_eq!(info.generation, inode.generation);
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

    #[test]
    fn map_batch_stat_results_preserves_mixed_entry_results() {
        let paths = vec!["/ok.bin".to_string(), "/missing.bin".to_string()];
        let mapped = map_batch_stat_results(
            &paths,
            vec![
                Ok(Inode::new_file(1, 0o644)),
                Err(anyhow!(types::EmbeddedFsError::not_found("/missing.bin"))),
            ],
        )
        .expect("adapter should accept exact-length result vectors");

        assert_eq!(mapped.len(), 2);
        assert!(mapped[0].is_ok(), "successful inode must stay successful");
        let err = mapped[1]
            .as_ref()
            .expect_err("missing inode must stay a per-entry error");
        assert!(
            err.to_string().contains("/missing.bin"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn map_batch_stat_results_rejects_short_result_vectors() {
        let paths = vec!["/a".to_string(), "/b".to_string()];
        let err = map_batch_stat_results(&paths, vec![Ok(Inode::new_file(1, 0o644))])
            .expect_err("short adapter result vectors must be rejected");
        assert!(
            err.to_string()
                .contains("returned 1 results for 2 input paths"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn map_batch_stat_results_rejects_long_result_vectors() {
        let paths = vec!["/a".to_string()];
        let err = map_batch_stat_results(
            &paths,
            vec![Ok(Inode::new_file(1, 0o644)), Ok(Inode::new_file(2, 0o644))],
        )
        .expect_err("long adapter result vectors must be rejected");
        assert!(
            err.to_string()
                .contains("returned 2 results for 1 input paths"),
            "unexpected error: {err}"
        );
    }
}
