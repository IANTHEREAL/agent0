use crate::extensions::fs::backend::{
    batch_inline_read_entry_too_large_error, batch_inline_read_payload_too_large_error,
    FsBatchWriteEntry, FsBatchWriteFile, FsCreateUpload, FsMultipartCompletedPart,
    FsPreparedDownload, FsPresignedRequest, FsRecursiveReaddirOptions, FsStorage, FsWriteStream,
    FsWriteStreamOptions,
};
use crate::extensions::fs::channel_reader::ChunkReceiverReader;
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::embedded::bundle::{
    build_bundle, delete_bundle_manifest, load_bundle_manifest, retire_bundle_entry,
    save_bundle_manifest, scan_bundle_manifests, BundleBuildInput, BundleJournal, BundleManifest,
    BundleManifestState, BundleSliceCache, BundleSpool,
};
use crate::extensions::fs::embedded::lifecycle::{self, FileLifecycle, UploadReservation};
use crate::extensions::fs::embedded::types::*;
use crate::extensions::fs::embedded::{blob, keys};
use crate::extensions::fs::s3::FsS3Client;
use crate::extensions::fs::upload_token::{
    normalized_path_hash_hex, sign_upload_token, verify_upload_token, UploadTokenClaims,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock as SyncOnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tikv_client::{CheckLevel, Key, Transaction, TransactionClient, TransactionOptions};
use tokio::fs;
use tokio::io::{AsyncBufRead, AsyncReadExt};
use tokio::sync::{mpsc, Mutex as AsyncMutex, OnceCell, Semaphore};
use tokio::time::sleep;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FsRuntimeState {
    identity: FsInstanceIdentity,
    fs_instance_id: [u8; 16],
    fs_instance_id_hex: String,
    object_store: Option<ObjectStoreBinding>,
}

#[derive(Debug, Default)]
struct CachedIdRange {
    next: u64,
    end_exclusive: u64,
}

impl CachedIdRange {
    fn take(&mut self) -> Option<u64> {
        if self.next >= self.end_exclusive {
            return None;
        }
        let id = self.next;
        self.next += 1;
        Some(id)
    }
}

#[derive(Clone)]
pub(crate) struct EmbeddedPageFs {
    client: Arc<TransactionClient>,
    keyspace: String,
    bundle_cache: Arc<BundleSliceCache>,
    bundle_spool: Arc<BundleSpool>,
    runtime_state: Arc<FsRuntimeState>,
    s3_client: Arc<OnceCell<Option<Arc<FsS3Client>>>>,
    inode_allocator: Arc<AsyncMutex<CachedIdRange>>,
    bundle_allocator: Arc<AsyncMutex<CachedIdRange>>,
}

const STREAM_READ_CHUNK_BYTES: usize = 64 * 1024;
const WRITE_STREAM_FLUSH_BYTES: usize = PAGE_SIZE * 16;
const STALE_WRITE_STREAM_SECS: i64 = 60 * 60;
const STALE_PACKING_SECS: i64 = 5 * 60;
const STAGING_REFRESH_INTERVAL_SECS: i64 = 5 * 60;
const INODE_ALLOC_BLOCK_SIZE: u64 = 1024;
const BUNDLE_ALLOC_BLOCK_SIZE: u64 = 128;
const INODE_BATCH_GET_CHUNK_SIZE: usize = 256;
const PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES: u64 = 4 * 1024;
const PACK_SPOOL_HEARTBEAT_FILE: &str = ".heartbeat";
const INSTANCE_PROBE_INTERVAL_SECS: u64 = 5;

static FS9_MAINTENANCE_STARTED: SyncOnceLock<Mutex<HashSet<FsInstanceIdentity>>> =
    SyncOnceLock::new();
static FS9_PROCESS_IDENTITIES: SyncOnceLock<Mutex<HashMap<String, FsInstanceIdentity>>> =
    SyncOnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadLifecyclePhase {
    Uploading,
    Committing,
    Published,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintenanceProbeAction {
    Run,
    Stop,
    Defer,
}

#[derive(Debug, Clone)]
struct UploadContext {
    phase: UploadLifecyclePhase,
    staging_inode_id: u64,
    key: String,
    upload_id: Option<String>,
    inode: Inode,
    reservation: UploadReservation,
}

#[derive(Debug, Clone)]
struct PendingPackBatchFile {
    path: String,
    staging_inode_id: u64,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct PreparedPackPublishFile {
    path: String,
    staging_inode_id: u64,
    size: u64,
    pack_entry: Option<PackEntryRef>,
}

#[derive(Debug, Clone, Copy)]
struct PackEntryRef {
    bundle_id: u64,
    offset: u64,
    len: u32,
    checksum: [u8; 32],
    generation: u64,
}

#[derive(Debug, Clone)]
struct BatchStatRequest {
    normalized: String,
    parts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingBatchStat {
    request_idx: usize,
    parent_inode: u64,
    next_part_idx: usize,
}

#[derive(Debug, Clone)]
struct ResolvedPath {
    inode_id: u64,
    inode: Inode,
}

#[derive(Debug, Clone)]
pub(crate) struct PageFsRecursiveReaddirResult {
    pub(crate) entries: Vec<(String, Inode)>,
    pub(crate) truncated: bool,
    pub(crate) total_dirs_scanned: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirLookupRequest {
    parent_inode: u64,
    name: String,
}

#[derive(Debug, Clone)]
struct PendingBatchInlineReadInline {
    result_idx: usize,
    inode: Inode,
}

#[derive(Debug, Clone)]
struct PendingBatchInlineReadPack {
    result_idx: usize,
    bundle_id: u64,
    bundle_offset: u64,
    len: usize,
}

#[derive(Debug, Clone)]
struct PlannedBatchInlineReadPackWindowEntry {
    result_idx: usize,
    bundle_offset: u64,
    len: usize,
}

#[derive(Debug, Clone)]
struct PlannedBatchInlineReadPackWindow {
    bundle_id: u64,
    start_offset: u64,
    end_offset: u64,
    entries: Vec<PlannedBatchInlineReadPackWindowEntry>,
}

#[derive(Debug, Clone)]
struct PendingBatchInlineReadObject {
    result_idx: usize,
    key: String,
    len: usize,
}

enum ResolvedFileReadPlan {
    Ready(Vec<u8>),
    Object {
        key: String,
        len: usize,
    },
    Pack {
        manifest: BundleManifest,
        bundle_id: u64,
        bundle_offset: u64,
        len: usize,
    },
}

enum FileRangeReadPlan {
    Ready(Vec<u8>),
    Object {
        key: String,
        offset: u64,
        len: usize,
    },
    Pack {
        manifest: BundleManifest,
        bundle_id: u64,
        bundle_offset: u64,
        file_offset: u64,
        len: usize,
    },
}

enum StreamReadPlan {
    Inline {
        txn: Transaction,
        inode_id: u64,
        inode: Inode,
    },
    Object {
        key: String,
    },
    Pack {
        manifest: BundleManifest,
        bundle_offset: u64,
        len: usize,
    },
}

#[async_trait]
trait BatchStatStore {
    async fn load_root_inode(&mut self) -> Result<Option<Inode>>;
    async fn lookup_dir_entries(
        &mut self,
        requests: &[DirLookupRequest],
    ) -> Result<HashMap<(u64, String), Option<u64>>>;
    async fn load_inodes(
        &mut self,
        inode_ids: &[u64],
    ) -> Result<HashMap<u64, Result<Option<Inode>>>>;
}

struct EmbeddedStagingWriteStream {
    fs: EmbeddedPageFs,
    path: String,
    staging_inode_id: u64,
    buffered: Vec<u8>,
    committed_bytes: u64,
    last_staging_refresh: i64,
}

impl EmbeddedStagingWriteStream {
    async fn flush_buffer(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }

        self.fs
            .flush_staged_write_chunk(self.staging_inode_id, self.committed_bytes, &self.buffered)
            .await?;
        self.committed_bytes = self
            .committed_bytes
            .checked_add(self.buffered.len() as u64)
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("stream write overflow")))?;
        self.buffered.clear();
        self.last_staging_refresh = current_unix_timestamp();
        Ok(())
    }

    async fn maybe_refresh_staging_marker(&mut self) -> Result<()> {
        let now = current_unix_timestamp();
        if now - self.last_staging_refresh >= STAGING_REFRESH_INTERVAL_SECS {
            self.fs.touch_staging_write(self.staging_inode_id).await?;
            self.last_staging_refresh = now;
        }
        Ok(())
    }
}

#[async_trait]
impl FsWriteStream for EmbeddedStagingWriteStream {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        self.buffered.extend_from_slice(chunk);
        if self.buffered.len() >= WRITE_STREAM_FLUSH_BYTES {
            self.flush_buffer().await?;
        } else {
            self.maybe_refresh_staging_marker().await?;
        }
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<usize> {
        if let Err(err) = self.flush_buffer().await {
            let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .fs
            .finalize_stream_spool(self.staging_inode_id, self.committed_bytes)
            .await
        {
            let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
            return Err(err);
        }

        match self
            .fs
            .publish_staged_write(&self.path, self.staging_inode_id)
            .await
        {
            Ok(written) => Ok(written),
            Err(err) => {
                let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
                Err(err)
            }
        }
    }

    async fn abort(self: Box<Self>) -> Result<()> {
        let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
        Ok(())
    }
}

struct EmbeddedObjectWriteStream {
    fs: EmbeddedPageFs,
    s3: Arc<FsS3Client>,
    path: String,
    staging_inode_id: u64,
    key: String,
    upload_id: String,
    part_size: usize,
    buffered: BytesMut,
    next_part_number: i32,
    parts: Vec<(i32, String)>,
    bytes_written: u64,
    hasher: Sha256,
    last_lifecycle_refresh: i64,
}

impl EmbeddedObjectWriteStream {
    async fn should_abort_external_data(&self) -> bool {
        let mut txn = match self.fs.begin_internal().await {
            Ok(txn) => txn,
            Err(_) => return false,
        };
        let inode = load_inode(&mut txn, self.staging_inode_id).await;
        let _ = txn.rollback().await;

        match inode {
            Ok(Some(inode)) => inode.nlink == 0,
            _ => false,
        }
    }

    async fn refresh_uploading_lifecycle(&mut self) -> Result<()> {
        let now = current_unix_timestamp();
        if now - self.last_lifecycle_refresh < STAGING_REFRESH_INTERVAL_SECS {
            return Ok(());
        }

        let mut txn = self.fs.begin().await?;
        if let Some(FileLifecycle::Uploading {
            upload_id,
            updated_at: _,
            reservation,
            ..
        }) = lifecycle::load_lifecycle(&mut txn, self.staging_inode_id).await?
        {
            lifecycle::save_lifecycle(
                &mut txn,
                self.staging_inode_id,
                &FileLifecycle::Uploading {
                    fs_instance_id: self.fs.runtime_state().fs_instance_id,
                    upload_id,
                    updated_at: now,
                    reservation,
                },
            )
            .await?;
            txn.commit().await?;
            self.last_lifecycle_refresh = now;
            return Ok(());
        }
        let _ = txn.rollback().await;
        Ok(())
    }

    async fn upload_part(&mut self, data: Bytes) -> Result<()> {
        let part_number = self.next_part_number;
        let etag = self
            .s3
            .upload_part(&self.key, &self.upload_id, part_number, data)
            .await?;
        self.parts.push((part_number, etag));
        self.next_part_number = self
            .next_part_number
            .checked_add(1)
            .ok_or_else(|| anyhow!("fs9: multipart part number overflow"))?;
        Ok(())
    }

    async fn flush_parts(&mut self) -> Result<()> {
        while self.buffered.len() >= self.part_size {
            let part = self.buffered.split_to(self.part_size).freeze();
            self.upload_part(part).await?;
            self.refresh_uploading_lifecycle().await?;
        }
        Ok(())
    }

    async fn abort_upload(&mut self) {
        if !self.should_abort_external_data().await {
            return;
        }
        self.abort_multipart_unchecked().await;
        let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
    }

    async fn abort_multipart_unchecked(&mut self) {
        let _ = self
            .s3
            .abort_multipart_upload(&self.key, &self.upload_id)
            .await;
        let _ = self.s3.delete_object(&self.key).await;
    }
}

#[async_trait]
impl FsWriteStream for EmbeddedObjectWriteStream {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        self.buffered.extend_from_slice(chunk);
        self.bytes_written = self
            .bytes_written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("stream write overflow")))?;
        self.hasher.update(chunk);

        self.flush_parts().await?;
        self.refresh_uploading_lifecycle().await?;
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<usize> {
        if let Err(err) = self.flush_parts().await {
            self.abort_upload().await;
            return Err(err);
        }

        if self.bytes_written == 0 {
            // Empty file: do not keep an object or multipart upload.
            self.abort_multipart_unchecked().await;
            let mut txn = self.fs.begin().await?;
            let mut inode = load_inode(&mut txn, self.staging_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
            inode.data = DataRef::None;
            inode.size = 0;
            save_inode(&mut txn, &inode).await?;

            let reservation =
                match lifecycle::load_lifecycle(&mut txn, self.staging_inode_id).await? {
                    Some(FileLifecycle::Uploading { reservation, .. }) => reservation,
                    Some(FileLifecycle::Committing { reservation, .. }) => reservation,
                    _ => None,
                };
            lifecycle::save_lifecycle(
                &mut txn,
                self.staging_inode_id,
                &FileLifecycle::Committing {
                    fs_instance_id: self.fs.runtime_state().fs_instance_id,
                    updated_at: current_unix_timestamp(),
                    reservation,
                },
            )
            .await?;
            txn.commit().await?;

            return match self
                .fs
                .publish_staged_write(&self.path, self.staging_inode_id)
                .await
            {
                Ok(written) => Ok(written),
                Err(err) => {
                    let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
                    Err(err)
                }
            };
        }

        // Upload the final (possibly small) part.
        if !self.buffered.is_empty() {
            let part = self.buffered.split().freeze();
            if let Err(err) = self.upload_part(part).await {
                self.abort_upload().await;
                return Err(err);
            }
        }

        if let Err(err) = self
            .s3
            .complete_multipart_upload(&self.key, &self.upload_id, std::mem::take(&mut self.parts))
            .await
        {
            self.abort_upload().await;
            return Err(err);
        }

        // Verify the object is visible before publishing metadata.
        let head_attempts = fs9_config()
            .s3
            .as_ref()
            .map(|c| c.head_retry_attempts)
            .unwrap_or(1);
        let base_ms = fs9_config()
            .s3
            .as_ref()
            .map(|c| c.head_retry_base_ms)
            .unwrap_or(50);
        let mut ok = false;
        for attempt in 0..head_attempts {
            match self.s3.head_object(&self.key).await {
                Ok(_) => {
                    ok = true;
                    break;
                }
                Err(err) => {
                    if attempt + 1 == head_attempts {
                        self.abort_upload().await;
                        return Err(err);
                    }
                    let factor = 1u64 << attempt.min(31);
                    let backoff = base_ms.saturating_mul(factor);
                    sleep(std::time::Duration::from_millis(backoff)).await;
                }
            }
        }
        if !ok {
            self.abort_upload().await;
            return Err(anyhow!("fs9: object HEAD verification failed"));
        }

        // Persist the final inode metadata (size + checksum) before publishing.
        let checksum: [u8; 32] = self
            .hasher
            .clone()
            .finalize()
            .as_slice()
            .try_into()
            .unwrap_or([0u8; 32]);
        self.fs
            .mark_object_staging_committing(
                self.staging_inode_id,
                &self.key,
                self.bytes_written,
                checksum,
                None,
            )
            .await?;

        match self
            .fs
            .publish_staged_write(&self.path, self.staging_inode_id)
            .await
        {
            Ok(written) => Ok(written),
            Err(err) => {
                // Publishing failed: clean up the uploaded object and staging inode.
                if self.should_abort_external_data().await {
                    let _ = self.s3.delete_object(&self.key).await;
                    let _ = self.fs.abort_staged_write(self.staging_inode_id).await;
                }
                Err(err)
            }
        }
    }

    async fn abort(mut self: Box<Self>) -> Result<()> {
        self.abort_upload().await;
        Ok(())
    }
}

impl EmbeddedPageFs {
    pub(crate) async fn load_runtime_superblock(
        client: Arc<TransactionClient>,
        keyspace: &str,
    ) -> Result<Superblock> {
        load_runtime_superblock_for_process(client, keyspace).await
    }

    pub(crate) fn new(
        client: Arc<TransactionClient>,
        keyspace: String,
        superblock: &Superblock,
    ) -> Self {
        let runtime_state = Arc::new(runtime_state_from_superblock(&keyspace, superblock));
        let spool_root =
            pack_spool_root(&runtime_state.identity, &runtime_state.fs_instance_id_hex);
        Self {
            client,
            keyspace,
            bundle_cache: Arc::new(BundleSliceCache::new(fs9_config().pack_cache_bytes)),
            bundle_spool: Arc::new(BundleSpool::new(spool_root, runtime_state.fs_instance_id)),
            runtime_state,
            s3_client: Arc::new(OnceCell::new()),
            inode_allocator: Arc::new(AsyncMutex::new(CachedIdRange::default())),
            bundle_allocator: Arc::new(AsyncMutex::new(CachedIdRange::default())),
        }
    }

    #[cfg(test)]
    pub(crate) async fn load_or_init_superblock(
        client: Arc<TransactionClient>,
        keyspace: &str,
    ) -> Result<Superblock> {
        load_or_init_superblock_for_keyspace(client, keyspace).await
    }

    pub(crate) fn identity_from_superblock(
        keyspace: &str,
        superblock: &Superblock,
    ) -> FsInstanceIdentity {
        FsInstanceIdentity::new(keyspace.to_string(), superblock.fs_instance_id)
    }

    fn runtime_state(&self) -> &FsRuntimeState {
        &self.runtime_state
    }

    pub(crate) fn instance_identity(&self) -> &FsInstanceIdentity {
        &self.runtime_state.identity
    }

    pub(crate) fn register_process_identity(identity: &FsInstanceIdentity) -> Result<()> {
        register_process_identity(identity)
    }

    async fn s3_client(&self) -> Result<Option<Arc<FsS3Client>>> {
        let binding = self.runtime_state().object_store.clone();
        let client = self
            .s3_client
            .get_or_try_init(|| async move {
                let Some(binding) = binding else {
                    return Ok::<Option<Arc<FsS3Client>>, anyhow::Error>(None);
                };
                let client = FsS3Client::new(&binding).await?;
                Ok::<Option<Arc<FsS3Client>>, anyhow::Error>(Some(Arc::new(client)))
            })
            .await?;
        Ok(client.clone())
    }

    fn has_object_storage(&self) -> bool {
        self.runtime_state().object_store.is_some()
    }

    pub(crate) fn ensure_background_maintenance(&self) {
        self.maybe_start_background_maintenance();
    }

    async fn alloc_inode_id(&self) -> Result<u64> {
        self.alloc_cached_id(
            &self.inode_allocator,
            keys::inode_allocator_key(),
            INODE_ALLOC_BLOCK_SIZE,
            "inode",
        )
        .await
    }

    async fn alloc_bundle_id(&self) -> Result<u64> {
        self.alloc_cached_id(
            &self.bundle_allocator,
            keys::bundle_allocator_key(),
            BUNDLE_ALLOC_BLOCK_SIZE,
            "bundle",
        )
        .await
    }

    async fn alloc_cached_id(
        &self,
        cache: &AsyncMutex<CachedIdRange>,
        key: Vec<u8>,
        block_size: u64,
        label: &str,
    ) -> Result<u64> {
        let mut cache = cache.lock().await;
        if let Some(id) = cache.take() {
            return Ok(id);
        }

        *cache = self.reserve_id_block(key, block_size, label).await?;
        cache.take().ok_or_else(|| {
            anyhow!(EmbeddedFsError::internal(&format!(
                "{label} allocator returned an empty block",
            )))
        })
    }

    async fn reserve_id_block(
        &self,
        key: Vec<u8>,
        block_size: u64,
        label: &str,
    ) -> Result<CachedIdRange> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let result: Result<CachedIdRange> = async {
                let next = load_allocator_counter(&mut txn, &key, label).await?;
                let end_exclusive = next.checked_add(block_size).ok_or_else(|| {
                    anyhow!(EmbeddedFsError::internal(&format!(
                        "{label} allocator overflow",
                    )))
                })?;
                save_allocator_counter(&mut txn, &key, end_exclusive).await?;
                txn.commit().await?;
                Ok(CachedIdRange {
                    next,
                    end_exclusive,
                })
            }
            .await;

            match result {
                Ok(range) => return Ok(range),
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(&format!(
            "{label} allocator retry exhausted",
        ))))
    }

    fn object_key(&self, inode_id: u64) -> Result<String> {
        build_object_key(&self.keyspace, self.runtime_state(), inode_id)
    }

    fn bundle_key(&self, bundle_id: u64) -> Result<String> {
        build_bundle_key(&self.keyspace, self.runtime_state(), bundle_id)
    }

    async fn load_upload_context(
        &self,
        claims: &UploadTokenClaims,
        refresh_uploading_lifecycle: bool,
    ) -> Result<UploadContext> {
        let now = current_unix_timestamp();
        if claims.keyspace != self.keyspace {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "upload token keyspace mismatch".to_string(),
            )));
        }
        if claims.fs_instance_id != self.runtime_state().fs_instance_id {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "upload token filesystem instance mismatch".to_string(),
            )));
        }
        if now > claims.expires_at {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "upload token has expired".to_string(),
            )));
        }
        let mut txn = self.begin_internal().await?;
        let result: Result<(UploadContext, bool)> = async {
            let inode = load_inode(&mut txn, claims.staging_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found("upload staging inode")))?;
            let lifecycle = lifecycle::load_lifecycle(&mut txn, claims.staging_inode_id).await?;

            let (key, version) = match &inode.data {
                DataRef::Object { key, version, .. } => (key.clone(), *version),
                _ => {
                    return Err(anyhow!(EmbeddedFsError::conflict(
                        "upload staging inode is no longer object-backed",
                    )))
                }
            };
            if version != claims.target_version {
                return Err(anyhow!(EmbeddedFsError::conflict(
                    "upload token target version no longer matches the staging inode",
                )));
            }

            match lifecycle {
                Some(FileLifecycle::Uploading {
                    upload_id,
                    reservation: Some(reservation),
                    ..
                }) => {
                    validate_upload_claims(claims, &reservation, upload_id.as_deref())?;
                    if refresh_uploading_lifecycle {
                        lifecycle::save_lifecycle(
                            &mut txn,
                            claims.staging_inode_id,
                            &FileLifecycle::Uploading {
                                fs_instance_id: self.runtime_state().fs_instance_id,
                                upload_id: upload_id.clone(),
                                updated_at: now,
                                reservation: Some(reservation.clone()),
                            },
                        )
                        .await?;
                    }
                    Ok((
                        UploadContext {
                            phase: UploadLifecyclePhase::Uploading,
                            staging_inode_id: claims.staging_inode_id,
                            key,
                            upload_id,
                            inode,
                            reservation,
                        },
                        refresh_uploading_lifecycle,
                    ))
                }
                Some(FileLifecycle::Committing {
                    reservation: Some(reservation),
                    ..
                }) => {
                    validate_upload_claims(claims, &reservation, None)?;
                    Ok((
                        UploadContext {
                            phase: UploadLifecyclePhase::Committing,
                            staging_inode_id: claims.staging_inode_id,
                            key,
                            upload_id: None,
                            inode,
                            reservation,
                        },
                        false,
                    ))
                }
                None if inode.nlink > 0 => {
                    let published_size = inode.size;
                    Ok((
                        UploadContext {
                            phase: UploadLifecyclePhase::Published,
                            staging_inode_id: claims.staging_inode_id,
                            key,
                            upload_id: None,
                            inode,
                            reservation: UploadReservation {
                                fs_instance_id: self.runtime_state().fs_instance_id,
                                path: String::new(),
                                path_hash: [0u8; 32],
                                expected_parent_inode: None,
                                expected_prior_inode: None,
                                expected_prior_generation: None,
                                expected_size: published_size,
                                nonce: claims.nonce,
                                expires_at: claims.expires_at,
                            },
                        },
                        false,
                    ))
                }
                _ => Err(anyhow!(EmbeddedFsError::conflict(
                    "upload token no longer matches an active upload reservation",
                ))),
            }
        }
        .await;

        match result {
            Ok((ctx, wrote_lifecycle)) => {
                if wrote_lifecycle {
                    txn.commit().await?;
                } else {
                    let _ = txn.rollback().await;
                }
                Ok(ctx)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    async fn prepare_pack_staging_batch(
        &self,
        files: &[FsBatchWriteFile],
    ) -> Result<(u64, String, Vec<PendingPackBatchFile>)> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let result: Result<(u64, String, Vec<PendingPackBatchFile>)> = async {
                let bundle_id = self.alloc_bundle_id().await?;
                let bundle_key = self.bundle_key(bundle_id)?;
                let updated_at = current_unix_timestamp();
                let mut staged_files = Vec::with_capacity(files.len());

                for file in files {
                    let inode_id = self.alloc_inode_id().await?;
                    let mut inode = Inode::new_file(inode_id, 0o644);
                    inode.nlink = 0;
                    inode.data = DataRef::None;
                    inode.size = u64::try_from(file.data.len()).map_err(|_| {
                        anyhow!(EmbeddedFsError::internal("batch file size exceeds u64"))
                    })?;
                    save_inode(&mut txn, &inode).await?;
                    lifecycle::save_lifecycle(
                        &mut txn,
                        inode_id,
                        &FileLifecycle::Packing {
                            fs_instance_id: self.runtime_state().fs_instance_id,
                            bundle_id,
                            updated_at,
                        },
                    )
                    .await?;
                    staged_files.push(PendingPackBatchFile {
                        path: file.path.clone(),
                        staging_inode_id: inode_id,
                        data: file.data.clone(),
                    });
                }

                txn.commit().await?;
                Ok((bundle_id, bundle_key, staged_files))
            }
            .await;

            match result {
                Ok(staged) => return Ok(staged),
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "pack staging retry exhausted",
        )))
    }

    async fn publish_pack_batch(
        &self,
        manifest: &BundleManifest,
        files: &[PreparedPackPublishFile],
    ) -> Result<()> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            match self.publish_pack_batch_once(manifest, files).await {
                Ok(()) => return Ok(()),
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "pack publish retry exhausted",
        )))
    }

    async fn publish_pack_batch_once(
        &self,
        manifest: &BundleManifest,
        files: &[PreparedPackPublishFile],
    ) -> Result<()> {
        let mut txn = self.begin().await?;

        for file in files {
            let (parent_inode, name) =
                ensure_parents_and_resolve_parent(self, &mut txn, &file.path).await?;

            let mut staging_inode = load_inode(&mut txn, file.staging_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("pack staging inode missing")))?;
            if staging_inode.nlink != 0 {
                return Err(anyhow!(EmbeddedFsError::conflict(
                    "pack staging inode already published",
                )));
            }

            let lifecycle = lifecycle::load_lifecycle(&mut txn, file.staging_inode_id).await?;
            match lifecycle {
                Some(FileLifecycle::Packing { bundle_id, .. })
                    if bundle_id == manifest.bundle_id => {}
                _ => {
                    return Err(anyhow!(EmbeddedFsError::conflict(
                        "pack staging inode is no longer in packing state",
                    )))
                }
            }

            let mut publish_generation = staging_inode.generation.max(1);
            if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
                let existing_inode = load_inode(&mut txn, existing_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&file.path)))?;
                if existing_inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(&file.path)));
                }

                publish_generation = existing_inode.generation.checked_add(1).ok_or_else(|| {
                    anyhow!(EmbeddedFsError::internal("inode generation overflow"))
                })?;
                retire_inode_data_ref(
                    &mut txn,
                    existing_inode_id,
                    &existing_inode.data,
                    self.runtime_state().fs_instance_id,
                )
                .await?;
                delete_inode(&mut txn, existing_inode_id).await?;
            }

            if let Some(pack_entry) = file.pack_entry {
                staging_inode.data = DataRef::PackEntry {
                    bundle_id: pack_entry.bundle_id,
                    offset: pack_entry.offset,
                    len: pack_entry.len,
                    checksum: pack_entry.checksum,
                    generation: pack_entry.generation,
                };
            } else {
                staging_inode.data = DataRef::None;
            }
            staging_inode.size = file.size;
            staging_inode.generation = publish_generation;
            staging_inode.nlink = 1;
            staging_inode.touch_mtime();
            save_inode(&mut txn, &staging_inode).await?;
            lifecycle::clear_lifecycle(&mut txn, file.staging_inode_id).await?;
            link(&mut txn, parent_inode, &name, file.staging_inode_id).await?;
        }

        save_bundle_manifest(&mut txn, manifest).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn cleanup_failed_pack_batch(&self, staging_inode_ids: &[u64], bundle_id: u64) {
        if !self
            .should_abort_pack_batch(staging_inode_ids, bundle_id)
            .await
        {
            return;
        }

        // Only clear the durable retry handles (spool journal + packing lifecycle) after
        // we have successfully deleted the external bundle object. Otherwise we'd lose
        // the only state that allows GC/recovery to retry the delete later.
        let deleted = match self.bundle_key(bundle_id) {
            Ok(key) => match self.s3_client().await {
                Ok(Some(s3)) => match s3.delete_object(&key).await {
                    Ok(()) => true,
                    Err(err) => {
                        warn!("fs9: failed to delete bundle object {key} for aborted pack bundle {bundle_id}: {err}");
                        false
                    }
                },
                Ok(None) => {
                    warn!("fs9: aborted pack bundle {bundle_id} retained because S3 is not configured");
                    false
                }
                Err(err) => {
                    warn!(
                        "fs9: aborted pack bundle {bundle_id} retained because S3 client init failed: {err}"
                    );
                    false
                }
            },
            Err(err) => {
                warn!("fs9: aborted pack bundle {bundle_id} retained because bundle key build failed: {err}");
                false
            }
        };

        if !deleted {
            return;
        }

        let _ = self.bundle_spool.remove_bundle(bundle_id).await;
        for inode_id in staging_inode_ids {
            // We already deleted the shared bundle object above. At this point we should
            // clear the packing lifecycle marker and drop the unpublished staging inode.
            let _ = self.cleanup_staging_inode(*inode_id).await;
        }
    }

    async fn should_abort_pack_batch(&self, staging_inode_ids: &[u64], bundle_id: u64) -> bool {
        let mut txn = match self.begin_internal().await {
            Ok(txn) => txn,
            Err(_) => return false,
        };
        let manifest = load_bundle_manifest(&mut txn, bundle_id).await;
        match manifest {
            Ok(Some(_)) => {
                let _ = txn.rollback().await;
                return false;
            }
            Ok(None) => {}
            Err(_) => {
                let _ = txn.rollback().await;
                return false;
            }
        }

        for inode_id in staging_inode_ids {
            match load_inode(&mut txn, *inode_id).await {
                Ok(Some(inode)) if inode.nlink != 0 => {
                    let _ = txn.rollback().await;
                    return false;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = txn.rollback().await;
                    return false;
                }
            }
        }

        let _ = txn.rollback().await;
        true
    }

    async fn cleanup_pending_bundle_journals(&self) -> Result<()> {
        let journals = self.bundle_spool.load_journals().await?;
        let now = current_unix_timestamp();
        for journal in journals {
            let mut txn = self.begin_internal().await?;
            let manifest_exists = load_bundle_manifest(&mut txn, journal.bundle_id)
                .await?
                .is_some();
            let _ = txn.rollback().await;

            if manifest_exists {
                let _ = self.bundle_spool.remove_bundle(journal.bundle_id).await;
                continue;
            }
            if now.saturating_sub(journal.created_at) < STALE_PACKING_SECS {
                continue;
            }

            let deleted = match self.s3_client().await {
                Ok(Some(s3)) => match s3.delete_object(&journal.key).await {
                    Ok(()) => true,
                    Err(err) => {
                        warn!(
                            "fs9: failed to delete stale bundle object {} for bundle {}: {err}",
                            journal.key, journal.bundle_id,
                        );
                        false
                    }
                },
                Ok(None) => {
                    warn!(
                        "fs9: stale bundle journal {} retained because S3 is not configured",
                        journal.bundle_id
                    );
                    false
                }
                Err(err) => {
                    warn!(
                        "fs9: stale bundle journal {} retained because S3 client init failed: {err}",
                        journal.bundle_id
                    );
                    false
                }
            };
            if deleted {
                let _ = self.bundle_spool.remove_bundle(journal.bundle_id).await;
            }
        }
        Ok(())
    }

    async fn maintain_local_pack_spool(&self) -> Result<()> {
        let now = current_unix_timestamp();
        let root = pack_spool_root(
            self.instance_identity(),
            &self.runtime_state().fs_instance_id_hex,
        );
        touch_pack_spool_heartbeat(&root, now).await?;
        self.bundle_spool
            .scavenge_active_root(pack_spool_stale_grace_secs())
            .await?;
        reap_stale_pack_spool_directories(&root, now, pack_spool_stale_grace_secs()).await
    }

    async fn cleanup_pending_packing_recovery(&self) -> Result<()> {
        let s3 = self.s3_client().await?;
        let mut txn = self.begin_internal().await?;
        let entries = lifecycle::scan_lifecycle(&mut txn, 1024).await?;
        let _ = txn.rollback().await;

        for (inode_id, state) in entries {
            let FileLifecycle::Packing {
                fs_instance_id,
                bundle_id,
                updated_at,
            } = state
            else {
                continue;
            };
            if fs_instance_id != self.runtime_state().fs_instance_id {
                continue;
            }
            if current_unix_timestamp().saturating_sub(updated_at) < STALE_PACKING_SECS {
                continue;
            }

            let deleted = match s3.as_ref() {
                Some(s3) => {
                    let key = match self.bundle_key(bundle_id) {
                        Ok(key) => key,
                        Err(err) => {
                            warn!("fs9: failed to build bundle key for stale pack bundle {bundle_id}: {err}");
                            continue;
                        }
                    };
                    match s3.delete_object(&key).await {
                        Ok(()) => true,
                        Err(err) => {
                            warn!("fs9: failed to delete stale pack bundle {bundle_id}: {err}");
                            false
                        }
                    }
                }
                None => {
                    warn!(
                        "fs9: stale pack bundle {bundle_id} retained because S3 is not configured"
                    );
                    false
                }
            };
            let _ = self.bundle_spool.remove_bundle(bundle_id).await;
            if !deleted {
                continue;
            }

            let mut txn = self.begin_internal().await?;
            let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
            if current
                == Some(FileLifecycle::Packing {
                    fs_instance_id,
                    bundle_id,
                    updated_at,
                })
            {
                lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                    if inode.nlink == 0 && !inode.is_directory() {
                        delete_inode(&mut txn, inode_id).await?;
                    }
                }
                txn.commit().await?;
            } else {
                let _ = txn.rollback().await;
            }
        }

        Ok(())
    }

    async fn read_pack_entry_bytes_with_s3(
        &self,
        s3: &FsS3Client,
        manifest: &BundleManifest,
        bundle_id: u64,
        bundle_offset: u64,
        file_offset: u64,
        length: usize,
    ) -> Result<Bytes> {
        if length == 0 {
            return Ok(Bytes::new());
        }

        let length_u64 = u64::try_from(length)
            .map_err(|_| anyhow!(EmbeddedFsError::internal("pack read length exceeds u64")))?;
        let read_offset = bundle_offset
            .checked_add(file_offset)
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("pack read offset overflow")))?;
        let cache_offset = read_offset;
        if let Some(bytes) = self.bundle_cache.get(bundle_id, cache_offset, length) {
            return Ok(bytes);
        }

        let bytes = self
            .fetch_pack_range_with_s3(s3, manifest, read_offset, length, length_u64)
            .await?;
        self.bundle_cache
            .insert(bundle_id, cache_offset, bytes.clone());
        Ok(bytes)
    }

    async fn fetch_pack_range_with_s3(
        &self,
        s3: &FsS3Client,
        manifest: &BundleManifest,
        read_offset: u64,
        length: usize,
        expected_len_u64: u64,
    ) -> Result<Bytes> {
        let bytes = s3
            .get_object_range_bytes(&manifest.key, read_offset, length)
            .await?;
        if u64::try_from(bytes.len()).ok() != Some(expected_len_u64) {
            return Err(anyhow!(EmbeddedFsError::internal(
                "pack read returned an unexpected byte length",
            )));
        }
        Ok(bytes)
    }

    fn split_pack_window_bytes(
        window: &PlannedBatchInlineReadPackWindow,
        window_bytes: Bytes,
    ) -> Result<Vec<(usize, u64, Vec<u8>)>> {
        let mut entry_results = Vec::with_capacity(window.entries.len());
        for entry in &window.entries {
            let relative_offset = entry
                .bundle_offset
                .checked_sub(window.start_offset)
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("pack read window underflow")))?;
            let start = usize::try_from(relative_offset).map_err(|_| {
                anyhow!(EmbeddedFsError::internal("pack read window exceeds usize"))
            })?;
            let end = start
                .checked_add(entry.len)
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("pack read slice overflow")))?;
            if end > window_bytes.len() {
                return Err(anyhow!(EmbeddedFsError::internal(
                    "pack read window returned an unexpected byte layout",
                )));
            }

            let bytes = window_bytes.slice(start..end).to_vec();
            entry_results.push((entry.result_idx, entry.bundle_offset, bytes));
        }
        Ok(entry_results)
    }

    async fn read_pack_window_entries_with_s3(
        &self,
        s3: &FsS3Client,
        manifest: &BundleManifest,
        window: PlannedBatchInlineReadPackWindow,
    ) -> Vec<(usize, Result<Vec<u8>>)> {
        let window_len_u64 = match window.end_offset.checked_sub(window.start_offset) {
            Some(len) => len,
            None => {
                let err = anyhow!(EmbeddedFsError::internal("pack read window underflow"));
                return window
                    .entries
                    .into_iter()
                    .map(|entry| (entry.result_idx, Err(clone_fs_error(&err))))
                    .collect();
            }
        };
        let window_len = match usize::try_from(window_len_u64) {
            Ok(len) => len,
            Err(_) => {
                let err = anyhow!(EmbeddedFsError::internal("pack read window exceeds usize"));
                return window
                    .entries
                    .into_iter()
                    .map(|entry| (entry.result_idx, Err(clone_fs_error(&err))))
                    .collect();
            }
        };

        let window_bytes = match self
            .fetch_pack_range_with_s3(
                s3,
                manifest,
                window.start_offset,
                window_len,
                window_len_u64,
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(err) => {
                return window
                    .entries
                    .into_iter()
                    .map(|entry| (entry.result_idx, Err(clone_fs_error(&err))))
                    .collect();
            }
        };

        // Keep transport failures shared across the window; only retry per entry when a
        // successful coalesced fetch returns an unexpected byte layout for slicing.
        match Self::split_pack_window_bytes(&window, window_bytes) {
            Ok(entries) => entries
                .into_iter()
                .map(|(idx, bundle_offset, data)| {
                    self.bundle_cache.insert(
                        window.bundle_id,
                        bundle_offset,
                        Bytes::copy_from_slice(&data),
                    );
                    (idx, Ok(data))
                })
                .collect(),
            Err(_layout_err) => {
                let mut results = Vec::with_capacity(window.entries.len());
                for entry in window.entries {
                    let result = self
                        .read_pack_entry_bytes_with_s3(
                            s3,
                            manifest,
                            window.bundle_id,
                            entry.bundle_offset,
                            0,
                            entry.len,
                        )
                        .await
                        .map(|bytes| bytes.to_vec());
                    results.push((entry.result_idx, result));
                }
                results
            }
        }
    }

    async fn read_pack_entry_bytes(
        &self,
        manifest: &BundleManifest,
        bundle_id: u64,
        bundle_offset: u64,
        file_offset: u64,
        length: usize,
    ) -> Result<Bytes> {
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        self.read_pack_entry_bytes_with_s3(
            s3.as_ref(),
            manifest,
            bundle_id,
            bundle_offset,
            file_offset,
            length,
        )
        .await
    }

    fn maybe_start_background_maintenance(&self) {
        let identity = self.instance_identity().clone();
        let started = FS9_MAINTENANCE_STARTED.get_or_init(|| Mutex::new(HashSet::new()));
        let mut guard = match started.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.insert(identity.clone()) {
            return;
        }

        let fs = self.clone();
        tokio::spawn(async move {
            let jitter_ms = fs9_config().gc_initial_jitter_ms;
            let probe_interval = std::time::Duration::from_secs(INSTANCE_PROBE_INTERVAL_SECS);
            let initial_maintenance_delay = if jitter_ms == 0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_millis(rand::random::<u64>() % jitter_ms.max(1))
            };
            let mut next_maintenance_at = tokio::time::Instant::now() + initial_maintenance_delay;
            let mut consecutive_maintenance_failures: u32 = 0;
            loop {
                let probe_result = fs.current_instance_matches_superblock().await;
                match maintenance_probe_action(&probe_result) {
                    MaintenanceProbeAction::Run => {}
                    MaintenanceProbeAction::Stop => {
                        unregister_background_maintenance(&identity);
                        break;
                    }
                    MaintenanceProbeAction::Defer => {
                        let Err(err) = probe_result else {
                            unreachable!("deferred maintenance probe requires an error");
                        };
                        warn!("fs9 maintenance instance probe failed: {err}");
                        sleep(probe_interval).await;
                        continue;
                    }
                }

                if tokio::time::Instant::now() >= next_maintenance_at {
                    match fs.run_background_maintenance_once().await {
                        Ok(()) => {
                            consecutive_maintenance_failures = 0;
                            next_maintenance_at = tokio::time::Instant::now()
                                + std::time::Duration::from_secs(
                                    fs9_config().gc_interval_secs.max(1),
                                );
                        }
                        Err(err) => {
                            consecutive_maintenance_failures =
                                consecutive_maintenance_failures.saturating_add(1);
                            warn!("fs9 background maintenance failed: {err}");
                            let base_secs = fs9_config().gc_interval_secs.max(1);
                            let max_secs = fs9_config().gc_max_backoff_secs.max(base_secs);
                            let factor = 1u64 << consecutive_maintenance_failures.min(10);
                            let sleep_secs = base_secs.saturating_mul(factor).min(max_secs);
                            next_maintenance_at = tokio::time::Instant::now()
                                + std::time::Duration::from_secs(sleep_secs);
                        }
                    }
                }

                sleep(probe_interval).await;
            }
        });
    }

    async fn run_background_maintenance_once(&self) -> Result<()> {
        self.cleanup_pending_write_recovery().await?;
        self.cleanup_lifecycle_once().await?;
        self.maintain_local_pack_spool().await?;
        Ok(())
    }

    async fn begin_internal(&self) -> Result<Transaction> {
        begin_transaction(&self.client).await
    }

    async fn begin_read(&self) -> Result<Transaction> {
        begin_read_transaction(&self.client).await
    }

    #[cfg(test)]
    async fn begin_unchecked(&self) -> Result<Transaction> {
        self.begin_internal().await
    }

    async fn begin(&self) -> Result<Transaction> {
        self.begin_internal().await
    }

    async fn current_instance_matches_superblock(&self) -> Result<bool> {
        let mut txn = self.begin_internal().await?;
        let current = load_current_superblock_if_present(&mut txn).await;
        let _ = txn.rollback().await;

        match current? {
            Some(superblock) => {
                let current_state = runtime_state_from_superblock(&self.keyspace, &superblock);
                Ok(&current_state == self.runtime_state())
            }
            None => Ok(false),
        }
    }

    async fn load_staging_object_key(&self, inode_id: u64) -> Result<Option<String>> {
        let mut txn = self.begin_internal().await?;
        let result: Result<Option<String>> = async {
            let Some(inode) = load_inode(&mut txn, inode_id).await? else {
                return Ok(None);
            };
            match inode.data {
                DataRef::Object { key, .. } => Ok(Some(key)),
                _ => Ok(None),
            }
        }
        .await;

        match result {
            Ok(key) => {
                let _ = txn.rollback().await;
                Ok(key)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    async fn mark_object_staging_committing(
        &self,
        staging_inode_id: u64,
        key: &str,
        bytes_written: u64,
        checksum: [u8; 32],
        reservation: Option<UploadReservation>,
    ) -> Result<()> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let result: Result<()> = async {
                let mut inode = load_inode(&mut txn, staging_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
                if inode.nlink != 0 {
                    return Err(anyhow!(EmbeddedFsError::internal(
                        "staging inode already published"
                    )));
                }

                match &inode.data {
                    DataRef::None => {}
                    DataRef::InlineBlob => blob::delete_blob(&mut txn, staging_inode_id).await?,
                    DataRef::StagingPages => {
                        delete_staging_pages(&mut txn, staging_inode_id).await?
                    }
                    DataRef::Object {
                        key: current_key, ..
                    } => {
                        if current_key != key {
                            return Err(anyhow!(EmbeddedFsError::internal(
                                "staging inode object key mismatch"
                            )));
                        }
                    }
                    DataRef::PackEntry { .. } => {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "staging inode cannot transition from pack entry to object"
                        )))
                    }
                }

                inode.data = DataRef::Object {
                    key: key.to_string(),
                    version: staging_inode_id,
                    checksum,
                };
                inode.size = bytes_written;
                save_inode(&mut txn, &inode).await?;
                lifecycle::save_lifecycle(
                    &mut txn,
                    staging_inode_id,
                    &FileLifecycle::Committing {
                        fs_instance_id: self.runtime_state().fs_instance_id,
                        updated_at: current_unix_timestamp(),
                        reservation: reservation.clone(),
                    },
                )
                .await?;
                txn.commit().await?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => return Ok(()),
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "object staging commit retry exhausted"
        )))
    }

    pub(crate) async fn init_filesystem(&self) -> Result<()> {
        let Some(superblock) =
            load_superblock_if_present_for_keyspace(self.client.clone(), &self.keyspace).await?
        else {
            return Err(restart_required_for_keyspace_instance(
                &self.keyspace,
                Some(self.instance_identity()),
                None,
            ));
        };
        let runtime_state = runtime_state_from_superblock(&self.keyspace, &superblock);
        if &runtime_state != self.runtime_state() {
            return Err(restart_required_for_keyspace_instance(
                &self.keyspace,
                Some(self.instance_identity()),
                Some(&runtime_state.identity),
            ));
        }
        register_process_identity(self.instance_identity())?;
        self.maybe_start_background_maintenance();
        Ok(())
    }

    async fn finalize_stream_spool(&self, staging_inode_id: u64, bytes_written: u64) -> Result<()> {
        if bytes_written == 0 {
            return Ok(());
        }

        if should_route_stream_spool_to_inline(bytes_written) {
            self.finalize_stream_spool_to_inline(staging_inode_id)
                .await?;
            return Ok(());
        }

        if !self.has_object_storage() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "files larger than {} bytes require S3-backed object storage",
                fs9_config().inline_max_bytes
            ))));
        }

        if should_route_stream_spool_to_object(bytes_written) {
            self.finalize_stream_spool_to_object(staging_inode_id, bytes_written)
                .await?;
        }

        Ok(())
    }

    async fn finalize_stream_spool_to_inline(&self, staging_inode_id: u64) -> Result<()> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let result: Result<()> = async {
                let mut inode = load_inode(&mut txn, staging_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
                if inode.nlink != 0 {
                    return Err(anyhow!(EmbeddedFsError::internal(
                        "staging inode already published"
                    )));
                }

                match &inode.data {
                    DataRef::None | DataRef::InlineBlob => {
                        let _ = txn.rollback().await;
                        return Ok(());
                    }
                    DataRef::StagingPages => {
                        let data = read_staging_pages(&mut txn, staging_inode_id, &inode).await?;
                        delete_staging_pages(&mut txn, staging_inode_id).await?;
                        blob::write_blob(&mut txn, staging_inode_id, &data).await?;
                        inode.data = DataRef::InlineBlob;
                        save_inode(&mut txn, &inode).await?;
                    }
                    DataRef::Object { .. } | DataRef::PackEntry { .. } => {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "unexpected sealed staging inode on mutable stream publish"
                        )))
                    }
                }

                txn.commit().await?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => return Ok(()),
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "inline stream finalization retry exhausted",
        )))
    }

    async fn finalize_stream_spool_to_object(
        &self,
        staging_inode_id: u64,
        bytes_written: u64,
    ) -> Result<()> {
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        let key = self.object_key(staging_inode_id)?;
        let part_size = fs9_config()
            .s3
            .as_ref()
            .map(|c| c.multipart_part_bytes)
            .unwrap_or(WRITE_STREAM_FLUSH_BYTES);
        let upload_id = s3.create_multipart_upload(&key).await?;
        let mut offset = 0u64;
        let mut part_number = 1i32;
        let mut parts = Vec::new();
        let mut hasher = Sha256::new();

        while offset < bytes_written {
            let remaining = bytes_written
                .checked_sub(offset)
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("stream upload underflow")))?;
            let chunk_len = usize::try_from(remaining.min(part_size as u64)).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "stream upload chunk exceeds addressable memory",
                ))
            })?;
            let chunk = match self
                .read_staging_inode_range(staging_inode_id, offset, chunk_len)
                .await
            {
                Ok(chunk) => chunk,
                Err(err) => {
                    let _ = s3.abort_multipart_upload(&key, &upload_id).await;
                    let _ = s3.delete_object(&key).await;
                    return Err(err);
                }
            };
            hasher.update(&chunk);

            match s3
                .upload_part(&key, &upload_id, part_number, Bytes::from(chunk))
                .await
            {
                Ok(etag) => parts.push((part_number, etag)),
                Err(err) => {
                    let _ = s3.abort_multipart_upload(&key, &upload_id).await;
                    let _ = s3.delete_object(&key).await;
                    return Err(err);
                }
            }

            part_number = part_number
                .checked_add(1)
                .ok_or_else(|| anyhow!("fs9: multipart part number overflow"))?;
            offset = offset.checked_add(chunk_len as u64).ok_or_else(|| {
                anyhow!(EmbeddedFsError::internal("stream upload offset overflow"))
            })?;
        }

        if let Err(err) = s3.complete_multipart_upload(&key, &upload_id, parts).await {
            let _ = s3.abort_multipart_upload(&key, &upload_id).await;
            let _ = s3.delete_object(&key).await;
            return Err(err);
        }

        let head_attempts = fs9_config()
            .s3
            .as_ref()
            .map(|c| c.head_retry_attempts)
            .unwrap_or(1);
        let base_ms = fs9_config()
            .s3
            .as_ref()
            .map(|c| c.head_retry_base_ms)
            .unwrap_or(50);
        let mut head_ok = false;
        for attempt in 0..head_attempts {
            match s3.head_object(&key).await {
                Ok(_) => {
                    head_ok = true;
                    break;
                }
                Err(err) => {
                    if attempt + 1 == head_attempts {
                        let _ = s3.delete_object(&key).await;
                        return Err(err);
                    }
                    let factor = 1u64 << attempt.min(31);
                    sleep(std::time::Duration::from_millis(
                        base_ms.saturating_mul(factor),
                    ))
                    .await;
                }
            }
        }
        if !head_ok {
            let _ = s3.delete_object(&key).await;
            return Err(anyhow!("fs9: object HEAD verification failed"));
        }

        let checksum: [u8; 32] = hasher.finalize().as_slice().try_into().unwrap_or([0u8; 32]);
        if let Err(err) = self
            .mark_object_staging_committing(staging_inode_id, &key, bytes_written, checksum, None)
            .await
        {
            let _ = s3.delete_object(&key).await;
            return Err(err);
        }

        Ok(())
    }

    async fn read_staging_inode_range(
        &self,
        staging_inode_id: u64,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        let mut txn = self.begin().await?;
        let result = async {
            let inode = load_inode(&mut txn, staging_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
            if inode.nlink != 0 {
                return Err(anyhow!(EmbeddedFsError::internal(
                    "staging inode already published"
                )));
            }
            read_staging_file_range_from_txn(&mut txn, staging_inode_id, &inode, offset, length)
                .await
        }
        .await;

        match result {
            Ok(data) => {
                let _ = txn.rollback().await;
                Ok(data)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    pub(crate) async fn stat(&self, path: &str) -> Result<Inode> {
        let mut txn = self.begin_read().await?;
        let (_, inode) = resolve_path(&mut txn, path).await?;
        Ok(inode)
    }

    pub(crate) async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<Inode>>> {
        let mut txn = self.begin().await?;
        let mut store = TxnBatchStatStore { txn: &mut txn };
        let result = Ok(resolve_paths_batched(&mut store, paths).await);
        let _ = txn.rollback().await;
        result
    }

    pub(crate) async fn batch_readdir(
        &self,
        paths: &[String],
    ) -> Result<Vec<Result<Vec<(String, Inode)>>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut txn = self.begin_read().await?;
        let resolved = {
            let mut store = TxnBatchStatStore { txn: &mut txn };
            resolve_paths_with_ids_batched(&mut store, paths).await
        };

        let mut raw_entries_by_index: Vec<Option<Result<Vec<(String, Inode)>>>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut dir_request_order = Vec::new();
        let mut dir_inode_ids = Vec::new();

        for (idx, resolved_result) in resolved.into_iter().enumerate() {
            let path = &paths[idx];
            let resolved = match resolved_result {
                Ok(resolved) => resolved,
                Err(err) => {
                    raw_entries_by_index[idx] = Some(Err(err));
                    continue;
                }
            };

            if !resolved.inode.is_directory() {
                raw_entries_by_index[idx] =
                    Some(Err(anyhow!(EmbeddedFsError::not_directory(path))));
                continue;
            }

            dir_request_order.push(idx);
            dir_inode_ids.push(resolved.inode_id);
        }

        let hydrated_dirs = load_directory_entries_batch(&mut txn, &dir_inode_ids).await?;
        if hydrated_dirs.len() != dir_request_order.len() {
            return Err(anyhow!(EmbeddedFsError::internal(&format!(
                "batch_readdir hydrated {} directories for {} directory requests",
                hydrated_dirs.len(),
                dir_request_order.len()
            ))));
        }

        for (idx, dir_entries) in dir_request_order.into_iter().zip(hydrated_dirs) {
            raw_entries_by_index[idx] = Some(Ok(dir_entries));
        }

        Ok(raw_entries_by_index
            .into_iter()
            .map(|entry| entry.expect("batch_readdir must produce an entry for every input path"))
            .collect())
    }

    pub(crate) async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut results: Vec<Option<Result<Vec<u8>>>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut inline_reads = Vec::new();
        let mut pack_reads = Vec::new();
        let mut object_reads = Vec::new();
        let mut total_planned = 0u64;

        let mut txn = self.begin_read().await?;
        let resolved = {
            let mut store = TxnBatchStatStore { txn: &mut txn };
            resolve_paths_batched(&mut store, paths).await
        };

        for (idx, inode_result) in resolved.into_iter().enumerate() {
            let path = &paths[idx];
            let inode = match inode_result {
                Ok(inode) => inode,
                Err(err) => {
                    results[idx] = Some(Err(err));
                    continue;
                }
            };

            if inode.is_directory() {
                results[idx] = Some(Err(anyhow!(EmbeddedFsError::is_directory(path))));
                continue;
            }

            if inode.size > max_file_bytes as u64 {
                results[idx] = Some(Err(batch_inline_read_entry_too_large_error(
                    inode.size,
                    max_file_bytes,
                )));
                continue;
            }

            total_planned = total_planned.saturating_add(inode.size);
            let file_len = usize::try_from(inode.size).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "file size exceeds addressable memory"
                ))
            })?;

            match &inode.data {
                DataRef::InlineBlob | DataRef::None => {
                    inline_reads.push(PendingBatchInlineReadInline {
                        result_idx: idx,
                        inode,
                    })
                }
                DataRef::PackEntry {
                    bundle_id,
                    offset,
                    len,
                    ..
                } => pack_reads.push(PendingBatchInlineReadPack {
                    result_idx: idx,
                    bundle_id: *bundle_id,
                    bundle_offset: *offset,
                    len: file_len.min(usize::try_from(*len).map_err(|_| {
                        anyhow!(EmbeddedFsError::internal("pack entry length exceeds usize"))
                    })?),
                }),
                DataRef::Object { key, .. } => object_reads.push(PendingBatchInlineReadObject {
                    result_idx: idx,
                    key: key.clone(),
                    len: file_len,
                }),
                DataRef::StagingPages => {
                    results[idx] = Some(Err(anyhow!(EmbeddedFsError::internal(
                        "published read reached internal staging pages",
                    ))));
                }
            }
        }

        if total_planned > max_total_bytes as u64 {
            return Err(batch_inline_read_payload_too_large_error(
                total_planned,
                max_total_bytes,
            ));
        }

        for pending in inline_reads {
            let len = usize::try_from(pending.inode.size).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "file size exceeds addressable memory"
                ))
            })?;
            let data =
                read_file_range_from_txn(&mut txn, pending.inode.id, &pending.inode, 0, len).await;
            results[pending.result_idx] = Some(data);
        }

        let mut uncached_pack_reads = Vec::new();
        for pending in pack_reads {
            if let Some(bytes) =
                self.bundle_cache
                    .get(pending.bundle_id, pending.bundle_offset, pending.len)
            {
                results[pending.result_idx] = Some(Ok(bytes.to_vec()));
            } else {
                uncached_pack_reads.push(pending);
            }
        }

        let pack_windows =
            plan_batch_inline_read_pack_windows(uncached_pack_reads, max_total_bytes)?;
        let mut manifests_by_bundle = HashMap::new();
        for window in &pack_windows {
            if manifests_by_bundle.contains_key(&window.bundle_id) {
                continue;
            }

            let manifest_result = match load_bundle_manifest(&mut txn, window.bundle_id).await {
                Ok(Some(manifest)) => Ok(manifest),
                Ok(None) => Err(anyhow!(EmbeddedFsError::internal(
                    "bundle manifest missing"
                ))),
                Err(err) => Err(err),
            };
            manifests_by_bundle.insert(window.bundle_id, manifest_result);
        }

        drop(txn);

        let mut external_tasks = tokio::task::JoinSet::new();
        let concurrency = fs9_config().batch_stat_concurrency.max(1);
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let has_external_reads = !pack_windows.is_empty() || !object_reads.is_empty();
        let mut shared_s3_error = None;
        let shared_s3 = if has_external_reads {
            match self.s3_client().await {
                Ok(Some(s3)) => Some(s3),
                Ok(None) => {
                    shared_s3_error =
                        Some(anyhow!(EmbeddedFsError::internal("S3 is not configured")));
                    None
                }
                Err(err) => {
                    shared_s3_error = Some(err);
                    None
                }
            }
        } else {
            None
        };

        if let Some(err) = shared_s3_error.as_ref() {
            for window in &pack_windows {
                for entry in &window.entries {
                    results[entry.result_idx] = Some(Err(clone_fs_error(err)));
                }
            }
            for pending in &object_reads {
                results[pending.result_idx] = Some(Err(clone_fs_error(err)));
            }
        }

        if let Some(s3) = shared_s3 {
            for window in pack_windows {
                match manifests_by_bundle.get(&window.bundle_id) {
                    Some(Ok(manifest)) => {
                        let permit = semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("batch_inline_read semaphore must not be closed");
                        let fs = self.clone();
                        let manifest = manifest.clone();
                        let s3 = s3.clone();
                        external_tasks.spawn(async move {
                            let _permit = permit;
                            fs.read_pack_window_entries_with_s3(s3.as_ref(), &manifest, window)
                                .await
                        });
                    }
                    Some(Err(err)) => {
                        for entry in &window.entries {
                            results[entry.result_idx] = Some(Err(clone_fs_error(err)));
                        }
                    }
                    None => {
                        for entry in &window.entries {
                            results[entry.result_idx] = Some(Err(anyhow!(
                                EmbeddedFsError::internal("bundle manifest plan missing")
                            )));
                        }
                    }
                }
            }

            for pending in object_reads {
                let permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("batch_inline_read semaphore must not be closed");
                let s3 = s3.clone();
                external_tasks.spawn(async move {
                    let _permit = permit;
                    let result = s3.get_object_bytes(&pending.key).await.map(|bytes| {
                        let mut data = bytes.to_vec();
                        if data.len() > pending.len {
                            data.truncate(pending.len);
                        }
                        data
                    });
                    vec![(pending.result_idx, result)]
                });
            }

            while let Some(join_result) = external_tasks.join_next().await {
                let task_entries = match join_result {
                    Ok(entries) => entries,
                    Err(err) => {
                        return Err(anyhow!("batch_inline_read task failed: {err}"));
                    }
                };
                for (idx, result) in task_entries {
                    results[idx] = Some(result);
                }
            }
        }

        Ok(results
            .into_iter()
            .map(|entry| {
                entry.expect("batch_inline_read must produce a result entry for each input path")
            })
            .collect())
    }

    pub(crate) async fn readdir(&self, path: &str) -> Result<Vec<(String, Inode)>> {
        let mut txn = self.begin_read().await?;
        let (inode_id, inode) = resolve_path(&mut txn, path).await?;
        if !inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(path)));
        }

        let mut entries = load_directory_entries_batch(&mut txn, &[inode_id]).await?;
        Ok(entries.pop().unwrap_or_default())
    }

    pub(crate) async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<PageFsRecursiveReaddirResult> {
        if opts.max_entries == 0 {
            return Ok(PageFsRecursiveReaddirResult {
                entries: Vec::new(),
                truncated: false,
                total_dirs_scanned: 0,
            });
        }

        let normalized = normalize_path(path);
        let mut txn = self.begin_read().await?;
        let (root_inode_id, root_inode) = resolve_path(&mut txn, &normalized).await?;
        if !root_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(path)));
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        let mut total_dirs_scanned = 0usize;
        let mut visited_dirs = HashSet::from([root_inode_id]);
        let mut frontier = VecDeque::from([(normalized, root_inode_id, 0usize)]);

        while !frontier.is_empty() {
            if entries.len() >= opts.max_entries {
                truncated = true;
                break;
            }

            let current_depth = frontier
                .front()
                .map(|(_, _, depth)| *depth)
                .expect("frontier must be non-empty while traversing");
            let mut current_level = Vec::new();
            while matches!(frontier.front(), Some((_, _, depth)) if *depth == current_depth) {
                current_level.push(
                    frontier
                        .pop_front()
                        .expect("frontier entry must exist while draining current level"),
                );
            }

            let mut level_dirs = Vec::with_capacity(current_level.len());
            let mut planned_entries = 0usize;
            for (dir_path, dir_inode_id, _) in &current_level {
                let remaining = opts
                    .max_entries
                    .saturating_sub(entries.len().saturating_add(planned_entries));
                if remaining == 0 {
                    truncated = true;
                    break;
                }

                let (dir_entries, dir_truncated) = load_directory_entries_limited(
                    &mut txn,
                    dir_path,
                    *dir_inode_id,
                    remaining,
                    opts.exclude_set.as_deref(),
                )
                .await?;
                total_dirs_scanned = total_dirs_scanned.saturating_add(1);
                planned_entries = planned_entries.saturating_add(dir_entries.len());
                level_dirs.push(dir_entries);

                if dir_truncated {
                    truncated = true;
                    break;
                }
            }

            for ((dir_path, _, dir_depth), dir_entries) in current_level.into_iter().zip(level_dirs)
            {
                for (name, child_inode) in dir_entries {
                    let child_path = if dir_path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir_path}/{name}")
                    };

                    if entries.len() >= opts.max_entries {
                        truncated = true;
                        break;
                    }

                    if child_inode.is_directory()
                        && dir_depth < opts.max_depth
                        && visited_dirs.insert(child_inode.id)
                    {
                        frontier.push_back((child_path.clone(), child_inode.id, dir_depth + 1));
                    }

                    entries.push((child_path, child_inode));
                }

                if truncated {
                    break;
                }
            }

            if truncated {
                break;
            }
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(PageFsRecursiveReaddirResult {
            entries,
            truncated,
            total_dirs_scanned,
        })
    }

    // Plan metadata under a TiKV snapshot, then execute any external object-store reads after the
    // snapshot drops. Inline reads stay fully inside the metadata phase.
    async fn plan_resolved_file_read(
        &self,
        txn: &mut Transaction,
        path: &str,
        inode: Inode,
        max_bytes: Option<usize>,
    ) -> Result<ResolvedFileReadPlan> {
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        let file_len = usize::try_from(inode.size).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "file size exceeds addressable memory"
            ))
        })?;
        if let Some(max_bytes) = max_bytes {
            if file_len > max_bytes {
                return Err(anyhow!(
                    "fs9: file too large: {path} (exceeded max {max_bytes} bytes)"
                ));
            }
        }

        match &inode.data {
            DataRef::Object { key, .. } => Ok(ResolvedFileReadPlan::Object {
                key: key.clone(),
                len: file_len,
            }),
            DataRef::PackEntry {
                bundle_id,
                offset,
                len,
                ..
            } => {
                let manifest = load_bundle_manifest(txn, *bundle_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("bundle manifest missing")))?;
                let entry_len = usize::try_from(*len).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal("pack entry length exceeds usize"))
                })?;
                Ok(ResolvedFileReadPlan::Pack {
                    manifest,
                    bundle_id: *bundle_id,
                    bundle_offset: *offset,
                    len: file_len.min(entry_len),
                })
            }
            _ => Ok(ResolvedFileReadPlan::Ready(
                read_file_range_from_txn(txn, inode.id, &inode, 0, file_len).await?,
            )),
        }
    }

    async fn execute_resolved_file_read_plan(&self, plan: ResolvedFileReadPlan) -> Result<Vec<u8>> {
        match plan {
            ResolvedFileReadPlan::Ready(data) => Ok(data),
            ResolvedFileReadPlan::Object { key, len } => {
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                let bytes = s3.get_object_bytes(&key).await?;
                let mut data = bytes.to_vec();
                if data.len() > len {
                    data.truncate(len);
                }
                Ok(data)
            }
            ResolvedFileReadPlan::Pack {
                manifest,
                bundle_id,
                bundle_offset,
                len,
            } => Ok(self
                .read_pack_entry_bytes(&manifest, bundle_id, bundle_offset, 0, len)
                .await?
                .to_vec()),
        }
    }

    async fn plan_file_range_read(
        &self,
        txn: &mut Transaction,
        path: &str,
        inode: Inode,
        offset: u64,
        length: usize,
    ) -> Result<FileRangeReadPlan> {
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        if offset >= inode.size || length == 0 {
            return Ok(FileRangeReadPlan::Ready(Vec::new()));
        }

        match &inode.data {
            DataRef::Object { key, .. } => {
                let available = inode.size - offset;
                let len = usize::try_from(available.min(length as u64)).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal(
                        "read length exceeds addressable memory"
                    ))
                })?;
                Ok(FileRangeReadPlan::Object {
                    key: key.clone(),
                    offset,
                    len,
                })
            }
            DataRef::PackEntry {
                bundle_id,
                offset: bundle_offset,
                len,
                ..
            } => {
                let manifest = load_bundle_manifest(txn, *bundle_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("bundle manifest missing")))?;
                let entry_size = u64::from(*len);
                let available = entry_size.saturating_sub(offset);
                let len = usize::try_from(available.min(length as u64)).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal(
                        "pack read length exceeds addressable memory",
                    ))
                })?;
                Ok(FileRangeReadPlan::Pack {
                    manifest,
                    bundle_id: *bundle_id,
                    bundle_offset: *bundle_offset,
                    file_offset: offset,
                    len,
                })
            }
            _ => Ok(FileRangeReadPlan::Ready(
                read_file_range_from_txn(txn, inode.id, &inode, offset, length).await?,
            )),
        }
    }

    async fn execute_file_range_read_plan(&self, plan: FileRangeReadPlan) -> Result<Vec<u8>> {
        match plan {
            FileRangeReadPlan::Ready(data) => Ok(data),
            FileRangeReadPlan::Object { key, offset, len } => {
                if len == 0 {
                    return Ok(Vec::new());
                }
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                Ok(s3.get_object_range_bytes(&key, offset, len).await?.to_vec())
            }
            FileRangeReadPlan::Pack {
                manifest,
                bundle_id,
                bundle_offset,
                file_offset,
                len,
            } => {
                if len == 0 {
                    return Ok(Vec::new());
                }
                Ok(self
                    .read_pack_entry_bytes(&manifest, bundle_id, bundle_offset, file_offset, len)
                    .await?
                    .to_vec())
            }
        }
    }

    pub(crate) async fn read_file_capped(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let plan = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
            self.plan_resolved_file_read(&mut txn, path, inode, Some(max_bytes))
                .await?
        };
        self.execute_resolved_file_read_plan(plan).await
    }

    #[cfg(test)]
    pub(crate) async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let plan = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
            self.plan_resolved_file_read(&mut txn, path, inode, None)
                .await?
        };
        self.execute_resolved_file_read_plan(plan).await
    }

    pub(crate) async fn read_file_at(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        let plan = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, path).await?;
            self.plan_file_range_read(&mut txn, path, inode, offset, length)
                .await?
        };
        self.execute_file_range_read_plan(plan).await
    }

    pub(crate) async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        let mut txn = self.begin_read().await?;
        let (inode_id, inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        let file_len = usize::try_from(inode.size).map_err(|_| {
            anyhow!(EmbeddedFsError::internal(
                "file size exceeds addressable memory"
            ))
        })?;
        if file_len > max_bytes {
            return Err(anyhow!(
                "fs9: file too large: {path} (exceeded max {max_bytes} bytes)"
            ));
        }

        let plan = self.plan_stream_read(txn, inode_id, inode).await?;
        let (tx, rx) = mpsc::channel(8);
        let fs = self.clone();
        let err_sender = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = fs.stream_read_plan_into_channel(plan, tx).await {
                let _ = err_sender
                    .send(Err(std::io::Error::other(err.to_string())))
                    .await;
            }
        });

        Ok(Box::new(ChunkReceiverReader::new(rx)))
    }

    async fn cleanup_pending_write_recovery(&self) -> Result<()> {
        self.cleanup_pending_bundle_journals().await?;
        self.cleanup_pending_packing_recovery().await?;
        self.cleanup_marked_orphans().await?;
        self.cleanup_stale_staging_writes().await
    }

    async fn cleanup_lifecycle_once(&self) -> Result<()> {
        let s3 = self.s3_client().await?;
        let Some(s3) = s3 else {
            return Ok(());
        };

        let mut txn = self.begin_internal().await?;
        let entries = lifecycle::scan_lifecycle(&mut txn, 1024).await?;
        let _ = txn.rollback().await;

        let now = current_unix_timestamp();
        for (inode_id, state) in entries {
            if state.fs_instance_id() != self.runtime_state().fs_instance_id {
                continue;
            }

            match state {
                FileLifecycle::Deleting {
                    fs_instance_id,
                    data_ref,
                } => {
                    if let DataRef::Object { key, .. } = &data_ref {
                        // External delete is idempotent.
                        if let Err(err) = s3.delete_object(key).await {
                            warn!("fs9 gc: failed to delete object for inode {inode_id}: {err}");
                            continue;
                        }
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Deleting {
                            fs_instance_id,
                            data_ref,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        // Best-effort: if the inode is still around and unpublished, drop it.
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
                FileLifecycle::Uploading {
                    fs_instance_id,
                    upload_id,
                    updated_at,
                    reservation,
                } => {
                    if !uploading_lifecycle_is_reapable(now, updated_at, reservation.as_ref()) {
                        continue;
                    }

                    let Some(key) = self.load_staging_object_key(inode_id).await? else {
                        continue;
                    };
                    let mut external_ok = true;
                    if let Some(upload_id) = upload_id.as_deref() {
                        if let Err(err) = s3.abort_multipart_upload(&key, upload_id).await {
                            warn!(
                                "fs9 gc: failed to abort multipart upload for inode {inode_id}: {err}"
                            );
                            external_ok = false;
                        }
                    }
                    if let Err(err) = s3.delete_object(&key).await {
                        warn!("fs9 gc: failed to delete object for inode {inode_id}: {err}");
                        external_ok = false;
                    }
                    if !external_ok {
                        continue;
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Uploading {
                            fs_instance_id,
                            upload_id,
                            updated_at,
                            reservation,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        let _ = clear_staging_write(&mut txn, inode_id).await;
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
                FileLifecycle::Committing {
                    fs_instance_id,
                    updated_at,
                    reservation,
                } => {
                    if now.saturating_sub(updated_at) < STALE_WRITE_STREAM_SECS {
                        continue;
                    }

                    let Some(key) = self.load_staging_object_key(inode_id).await? else {
                        continue;
                    };
                    if let Err(err) = s3.delete_object(&key).await {
                        warn!("fs9 gc: failed to delete object for inode {inode_id}: {err}");
                        continue;
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Committing {
                            fs_instance_id,
                            updated_at,
                            reservation,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        let _ = clear_staging_write(&mut txn, inode_id).await;
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
                FileLifecycle::Packing {
                    fs_instance_id,
                    bundle_id,
                    updated_at,
                } => {
                    if now.saturating_sub(updated_at) < STALE_PACKING_SECS {
                        continue;
                    }

                    let key = match self.bundle_key(bundle_id) {
                        Ok(key) => key,
                        Err(_) => continue,
                    };
                    let deleted = match s3.delete_object(&key).await {
                        Ok(()) => true,
                        Err(err) => {
                            warn!("fs9: failed to delete stale pack bundle {bundle_id}: {err}");
                            false
                        }
                    };
                    let _ = self.bundle_spool.remove_bundle(bundle_id).await;
                    if !deleted {
                        continue;
                    }

                    let mut txn = self.begin_internal().await?;
                    let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
                    if current.as_ref()
                        == Some(&FileLifecycle::Packing {
                            fs_instance_id,
                            bundle_id,
                            updated_at,
                        })
                    {
                        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
                        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
                            if inode.nlink == 0 && !inode.is_directory() {
                                delete_inode(&mut txn, inode_id).await?;
                            }
                        }
                        txn.commit().await?;
                    } else {
                        let _ = txn.rollback().await;
                    }
                }
            }
        }

        let mut txn = self.begin_internal().await?;
        let manifests = scan_bundle_manifests(&mut txn, 1024).await?;
        let _ = txn.rollback().await;

        for manifest in manifests {
            if manifest.fs_instance_id != self.runtime_state().fs_instance_id {
                continue;
            }
            if manifest.needs_compaction() {
                continue;
            }
            if manifest.state != BundleManifestState::PendingDelete {
                continue;
            }

            if let Err(err) = s3.delete_object(&manifest.key).await {
                warn!(
                    "fs9: failed to delete pending-delete bundle {} (key={}): {err}",
                    manifest.bundle_id, manifest.key
                );
                continue;
            }

            let mut txn = self.begin_internal().await?;
            let current = load_bundle_manifest(&mut txn, manifest.bundle_id).await?;
            if current.as_ref() == Some(&manifest) {
                delete_bundle_manifest(&mut txn, manifest.bundle_id).await?;
                txn.commit().await?;
            } else {
                let _ = txn.rollback().await;
            }
        }

        Ok(())
    }

    pub(crate) async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        if !data.is_empty() && !can_store_inline_len(data.len()) {
            if !self.has_object_storage() {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                    "files larger than {} bytes require S3-backed object storage",
                    fs9_config().inline_max_bytes
                ))));
            }

            let mut writer = self
                .begin_write_stream(
                    path,
                    FsWriteStreamOptions {
                        expected_size: Some(u64::try_from(data.len()).map_err(|_| {
                            anyhow!(EmbeddedFsError::internal("write length exceeds u64"))
                        })?),
                    },
                )
                .await?;
            if let Err(err) = writer.write_chunk(data).await {
                let _ = writer.abort().await;
                return Err(err);
            }
            return writer.finish().await;
        }

        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = prepare_replace_file_txn(self, &mut txn, path).await?;

        if data.is_empty() {
            inode.data = DataRef::None;
            inode.size = 0;
        } else {
            blob::write_blob(&mut txn, inode_id, data).await?;
            inode.data = DataRef::InlineBlob;
            inode.size = data.len() as u64;
        }

        bump_inode_generation(&mut inode)?;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;

        txn.commit().await?;
        Ok(data.len())
    }

    pub(crate) async fn batch_write(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<Vec<FsBatchWriteEntry>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        let use_pack_route = fs9_config().s3.is_some()
            && files.len() > 1
            && files.iter().any(|file| !file.data.is_empty());
        if !use_pack_route {
            return self.batch_write_sequential(files).await;
        }

        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        match self.batch_write_pack(files).await {
            Ok(entries) => Ok(entries),
            Err(err) => {
                let message = err.to_string();
                Ok(paths
                    .into_iter()
                    .map(|path| FsBatchWriteEntry {
                        path,
                        result: Err(anyhow!(message.clone())),
                    })
                    .collect())
            }
        }
    }

    async fn batch_write_sequential(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<Vec<FsBatchWriteEntry>> {
        let mut entries = Vec::with_capacity(files.len());
        for file in files {
            let path = file.path;
            let result = self.write_file(&path, &file.data).await;
            entries.push(FsBatchWriteEntry { path, result });
        }
        Ok(entries)
    }

    async fn batch_write_pack(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<Vec<FsBatchWriteEntry>> {
        let (bundle_id, key, staged_files) = self.prepare_pack_staging_batch(&files).await?;
        let staging_inode_ids: Vec<u64> = staged_files
            .iter()
            .map(|file| file.staging_inode_id)
            .collect();

        let build_inputs: Vec<BundleBuildInput> = staged_files
            .iter()
            .filter(|file| !file.data.is_empty())
            .map(|file| BundleBuildInput {
                staging_inode_id: file.staging_inode_id,
                data: file.data.clone(),
            })
            .collect();
        let built = match build_bundle(bundle_id, &build_inputs) {
            Ok(built) => built,
            Err(err) => {
                self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                    .await;
                return Err(err);
            }
        };
        let journal = BundleJournal {
            keyspace: self.keyspace.clone(),
            fs_instance_id: self.runtime_state().fs_instance_id,
            bundle_id,
            key: key.clone(),
            created_at: current_unix_timestamp(),
            object_size: built.object_size,
            staging_inode_ids: staging_inode_ids.clone(),
        };
        if let Err(err) = self
            .bundle_spool
            .write_pending_bundle(&journal, &built)
            .await
        {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        let s3 = match self.s3_client().await {
            Ok(Some(s3)) => s3,
            Ok(None) => {
                self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                    .await;
                return Err(anyhow!(EmbeddedFsError::internal("S3 is not configured")));
            }
            Err(err) => {
                self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                    .await;
                return Err(err);
            }
        };

        if let Err(err) = s3.put_object(&key, built.bytes.clone()).await {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        if let Err(err) = head_object_with_retry(&s3, &key).await {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        let entry_by_inode: HashMap<u64, PackEntryRef> = built
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.staging_inode_id,
                    PackEntryRef {
                        bundle_id,
                        offset: entry.offset,
                        len: entry.len,
                        checksum: entry.checksum,
                        generation: entry.staging_inode_id,
                    },
                )
            })
            .collect();
        let publish_files: Vec<PreparedPackPublishFile> = staged_files
            .iter()
            .map(|file| PreparedPackPublishFile {
                path: file.path.clone(),
                staging_inode_id: file.staging_inode_id,
                size: file.data.len() as u64,
                pack_entry: entry_by_inode.get(&file.staging_inode_id).copied(),
            })
            .collect();
        let manifest = BundleManifest {
            fs_instance_id: self.runtime_state().fs_instance_id,
            bundle_id,
            key,
            created_at: current_unix_timestamp(),
            object_size: built.object_size,
            footer_offset: built.footer_offset,
            entry_count: u32::try_from(built.entries.len()).map_err(|_| {
                anyhow!(EmbeddedFsError::internal("bundle entry count exceeds u32"))
            })?,
            live_entries: u32::try_from(built.entries.len()).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "bundle live entry count exceeds u32"
                ))
            })?,
            live_bytes: publish_files
                .iter()
                .filter_map(|file| file.pack_entry.map(|_| file.size))
                .sum(),
            stale_entries: 0,
            checksum: built.checksum,
            state: BundleManifestState::Active,
        };

        if let Err(err) = self.publish_pack_batch(&manifest, &publish_files).await {
            self.cleanup_failed_pack_batch(&staging_inode_ids, bundle_id)
                .await;
            return Err(err);
        }

        if let Err(err) = self.bundle_spool.remove_bundle(bundle_id).await {
            warn!(
                "fs9: pack spool cleanup failed for bundle {bundle_id} after successful publish: {err}"
            );
        }
        Ok(publish_files
            .into_iter()
            .map(|file| FsBatchWriteEntry {
                path: file.path,
                result: Ok(file.size as usize),
            })
            .collect())
    }

    pub(crate) async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        // Validate early that the target is not a directory.
        let mut check_txn = self.begin().await?;
        match resolve_path(&mut check_txn, path).await {
            Ok((_, inode)) if inode.is_directory() => {
                let _ = check_txn.rollback().await;
                return Err(anyhow!(EmbeddedFsError::is_directory(path)));
            }
            Ok(_) => {}
            Err(err) if !is_not_found_error(&err) => {
                let _ = check_txn.rollback().await;
                return Err(err);
            }
            Err(_) => {}
        }
        let _ = check_txn.rollback().await;

        let has_object_storage = self.has_object_storage();
        if opts
            .expected_size
            .is_some_and(|size| !can_store_inline_u64(size) && !has_object_storage)
        {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "files larger than {} bytes require S3-backed object storage",
                fs9_config().inline_max_bytes
            ))));
        }

        let want_object = should_use_direct_object_stream(opts.expected_size, has_object_storage);

        if want_object {
            let s3 = self
                .s3_client()
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
            let part_size = fs9_config()
                .s3
                .as_ref()
                .map(|c| c.multipart_part_bytes)
                .unwrap_or(WRITE_STREAM_FLUSH_BYTES);

            let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
            for attempt in 0..attempts {
                let mut txn = self.begin().await?;
                let inode_id = self.alloc_inode_id().await?;
                let key = self.object_key(inode_id)?;
                let upload_id = match s3.create_multipart_upload(&key).await {
                    Ok(id) => id,
                    Err(err) => {
                        let _ = txn.rollback().await;
                        return Err(err);
                    }
                };

                let now = current_unix_timestamp();
                let mut inode = Inode::new_file(inode_id, 0o644);
                inode.nlink = 0;
                inode.size = opts.expected_size.unwrap_or(0);
                inode.data = DataRef::Object {
                    key: key.clone(),
                    version: inode_id,
                    checksum: [0u8; 32],
                };
                save_inode(&mut txn, &inode).await?;
                lifecycle::save_lifecycle(
                    &mut txn,
                    inode_id,
                    &FileLifecycle::Uploading {
                        fs_instance_id: self.runtime_state().fs_instance_id,
                        upload_id: Some(upload_id.clone()),
                        updated_at: now,
                        reservation: None,
                    },
                )
                .await?;

                match txn.commit().await {
                    Ok(_) => {
                        return Ok(Box::new(EmbeddedObjectWriteStream {
                            fs: self.clone(),
                            s3,
                            path: normalize_path(path),
                            staging_inode_id: inode_id,
                            key,
                            upload_id,
                            part_size,
                            buffered: BytesMut::with_capacity(
                                part_size.min(WRITE_STREAM_FLUSH_BYTES),
                            ),
                            next_part_number: 1,
                            parts: Vec::new(),
                            bytes_written: 0,
                            hasher: Sha256::new(),
                            last_lifecycle_refresh: now,
                        }));
                    }
                    Err(err) if attempt + 1 < attempts => {
                        let _ = s3.abort_multipart_upload(&key, &upload_id).await;
                        let err = anyhow!(err);
                        if is_retryable_tikv_write_conflict(&err) {
                            fs9_commit_backoff(attempt).await;
                            continue;
                        }
                        return Err(err);
                    }
                    Err(err) => {
                        let _ = s3.abort_multipart_upload(&key, &upload_id).await;
                        return Err(anyhow!(err));
                    }
                }
            }

            return Err(anyhow!(EmbeddedFsError::internal(
                "begin_write_stream retry exhausted",
            )));
        }

        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let inode_id = self.alloc_inode_id().await?;
            let mut inode = Inode::new_file(inode_id, 0o644);
            inode.nlink = 0;
            save_inode(&mut txn, &inode).await?;
            let now = current_unix_timestamp();
            mark_staging_write(&mut txn, inode_id, now).await?;

            match txn.commit().await {
                Ok(_) => {
                    return Ok(Box::new(EmbeddedStagingWriteStream {
                        fs: self.clone(),
                        path: normalize_path(path),
                        staging_inode_id: inode_id,
                        buffered: Vec::with_capacity(WRITE_STREAM_FLUSH_BYTES),
                        committed_bytes: 0,
                        last_staging_refresh: now,
                    }));
                }
                Err(err) if attempt + 1 < attempts => {
                    let err = anyhow!(err);
                    if is_retryable_tikv_write_conflict(&err) {
                        fs9_commit_backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => return Err(anyhow!(err)),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "begin_write_stream retry exhausted",
        )))
    }

    pub(crate) async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
    ) -> Result<FsCreateUpload> {
        if expected_size == 0 {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "create_upload requires a positive expected_size".to_string(),
            )));
        }

        let object_min = u64::try_from(fs9_config().object_min_bytes)
            .map_err(|_| anyhow!(EmbeddedFsError::internal("FS9_OBJECT_MIN exceeds u64")))?;
        if expected_size < object_min {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "create_upload requires expected_size >= {} bytes",
                object_min
            ))));
        }

        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;

        let normalized = normalize_path(path);
        let part_size = fs9_config()
            .s3
            .as_ref()
            .map(|cfg| cfg.multipart_part_bytes)
            .unwrap_or(WRITE_STREAM_FLUSH_BYTES);

        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let (expected_prior_inode, expected_prior_generation) =
                match resolve_path(&mut txn, &normalized).await {
                    Ok((inode_id, inode)) => {
                        if inode.is_directory() {
                            return Err(anyhow!(EmbeddedFsError::is_directory(&normalized)));
                        }
                        (Some(inode_id), Some(inode.generation))
                    }
                    Err(err) if is_not_found_error(&err) => (None, None),
                    Err(err) => return Err(err),
                };
            let expected_parent_inode = resolve_existing_parent(&mut txn, &normalized).await?;
            let inode_id = self.alloc_inode_id().await?;
            let object_key = self.object_key(inode_id)?;
            let upload_id = match s3.create_multipart_upload(&object_key).await {
                Ok(upload_id) => upload_id,
                Err(err) => {
                    let _ = txn.rollback().await;
                    return Err(err);
                }
            };

            let now = current_unix_timestamp();
            let expires_at = now
                .checked_add(
                    i64::try_from(fs9_config().presign_ttl_secs).map_err(|_| {
                        anyhow!(EmbeddedFsError::internal("presign ttl exceeds i64"))
                    })?,
                )
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("presign expiry overflow")))?;
            let nonce = rand::thread_rng().gen_range(1..=u64::MAX);
            let path_hash = path_hash_bytes(&normalized);
            let reservation = UploadReservation {
                fs_instance_id: self.runtime_state().fs_instance_id,
                path: normalized.clone(),
                path_hash,
                expected_parent_inode,
                expected_prior_inode,
                expected_prior_generation,
                expected_size,
                nonce,
                expires_at,
            };
            let claims = UploadTokenClaims {
                keyspace: self.keyspace.clone(),
                fs_instance_id: self.runtime_state().fs_instance_id,
                staging_inode_id: inode_id,
                target_path_hash: normalized_path_hash_hex(&normalized),
                expected_parent_inode,
                expected_prior_inode,
                expected_prior_generation,
                upload_id: upload_id.clone(),
                target_version: inode_id,
                nonce,
                expires_at,
            };
            let upload_token = match sign_upload_token(&claims) {
                Ok(token) => token,
                Err(err) => {
                    let _ = s3.abort_multipart_upload(&object_key, &upload_id).await;
                    let _ = txn.rollback().await;
                    return Err(err);
                }
            };

            let mut inode = Inode::new_file(inode_id, 0o644);
            inode.nlink = 0;
            inode.size = expected_size;
            inode.data = DataRef::Object {
                key: object_key.clone(),
                version: inode_id,
                checksum: [0u8; 32],
            };
            save_inode(&mut txn, &inode).await?;
            lifecycle::save_lifecycle(
                &mut txn,
                inode_id,
                &FileLifecycle::Uploading {
                    fs_instance_id: self.runtime_state().fs_instance_id,
                    upload_id: Some(upload_id.clone()),
                    updated_at: now,
                    reservation: Some(reservation),
                },
            )
            .await?;

            match txn.commit().await {
                Ok(_) => {
                    return Ok(FsCreateUpload {
                        upload_token,
                        upload_id,
                        part_size,
                        expires_at,
                    });
                }
                Err(err) if attempt + 1 < attempts => {
                    let _ = s3.abort_multipart_upload(&object_key, &upload_id).await;
                    let err = anyhow!(err);
                    if is_retryable_tikv_write_conflict(&err) {
                        fs9_commit_backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => {
                    let _ = s3.abort_multipart_upload(&object_key, &upload_id).await;
                    return Err(anyhow!(err));
                }
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "create_upload retry exhausted"
        )))
    }

    pub(crate) async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
    ) -> Result<FsPresignedRequest> {
        if !(1..=10_000).contains(&part_number) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "invalid multipart part number: {}",
                part_number
            ))));
        }

        let claims = verify_upload_token(upload_token)?;
        let ctx = self.load_upload_context(&claims, true).await?;
        if ctx.phase != UploadLifecyclePhase::Uploading {
            return Err(anyhow!(EmbeddedFsError::conflict(
                "upload is no longer in uploading state",
            )));
        }

        let max_part_number = max_presign_part_number(ctx.reservation.expected_size)?;
        if part_number > max_part_number {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "multipart part number {} exceeds per-upload limit {}",
                part_number, max_part_number
            ))));
        }

        let upload_id = ctx.upload_id.as_deref().ok_or_else(|| {
            anyhow!(EmbeddedFsError::internal(
                "uploading state missing upload_id"
            ))
        })?;
        let ttl_secs = presign_ttl_secs_from_claims(claims.expires_at)?;
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        s3.presign_upload_part(&ctx.key, upload_id, part_number, ttl_secs)
            .await
    }

    pub(crate) async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        let claims = verify_upload_token(upload_token)?;
        let ctx = self.load_upload_context(&claims, false).await?;
        if ctx.phase == UploadLifecyclePhase::Published {
            return usize::try_from(ctx.inode.size)
                .map_err(|_| anyhow!(EmbeddedFsError::internal("published size exceeds usize")));
        }

        let completed_parts = normalize_completed_parts(parts)?;
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;

        if ctx.phase == UploadLifecyclePhase::Uploading {
            let max_part_number = max_presign_part_number(ctx.reservation.expected_size)?;
            if completed_parts
                .last()
                .is_some_and(|part| part.part_number > max_part_number)
            {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                    "multipart part number exceeds per-upload limit {}",
                    max_part_number
                ))));
            }

            let upload_id = ctx.upload_id.as_deref().ok_or_else(|| {
                anyhow!(EmbeddedFsError::internal(
                    "uploading state missing upload_id"
                ))
            })?;
            s3.complete_multipart_upload(
                &ctx.key,
                upload_id,
                completed_parts
                    .iter()
                    .map(|part| (part.part_number, part.etag.clone()))
                    .collect(),
            )
            .await?;
        }

        let head = head_object_with_retry(&s3, &ctx.key).await?;
        if head.size != ctx.reservation.expected_size {
            return Err(anyhow!(EmbeddedFsError::conflict(&format!(
                "uploaded object size {} does not match reserved size {}",
                head.size, ctx.reservation.expected_size
            ))));
        }

        self.mark_object_staging_committing(
            ctx.staging_inode_id,
            &ctx.key,
            head.size,
            checksum.unwrap_or([0u8; 32]),
            Some(ctx.reservation.clone()),
        )
        .await?;

        self.publish_staged_write_with_reservation(
            &ctx.reservation.path,
            ctx.staging_inode_id,
            Some(&ctx.reservation),
        )
        .await
    }

    pub(crate) async fn abort_upload(&self, upload_token: &str) -> Result<()> {
        let claims = verify_upload_token(upload_token)?;
        let ctx = self.load_upload_context(&claims, false).await?;
        if ctx.phase != UploadLifecyclePhase::Uploading {
            return Err(anyhow!(EmbeddedFsError::conflict(
                "upload can only be aborted while uploading",
            )));
        }

        self.abort_staged_write(ctx.staging_inode_id).await
    }

    pub(crate) async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload> {
        let normalized = normalize_path(path);
        let (key, storage, size) = {
            let mut txn = self.begin_read().await?;
            let (_inode_id, inode) = resolve_path(&mut txn, &normalized).await?;
            if inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::is_directory(&normalized)));
            }

            let (key, storage) = match &inode.data {
                DataRef::Object { key, .. } => (key.clone(), FsStorage::Object),
                _ => {
                    return Err(anyhow!(EmbeddedFsError::InvalidInput(
                        "prepare_download is only supported for object-backed files".to_string(),
                    )))
                }
            };
            (key, storage, inode.size)
        };

        let ttl_secs = fs9_config().presign_ttl_secs.max(1);
        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
        let request = s3.presign_get_object(&key, ttl_secs).await?;
        Ok(FsPreparedDownload {
            request,
            size,
            storage,
            range_supported: true,
        })
    }

    pub(crate) async fn write_file_at(
        &self,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = prepare_write_at_file_txn(self, &mut txn, path).await?;
        apply_inline_write_at(
            &mut txn,
            inode_id,
            &mut inode,
            offset,
            data,
            "write_file_at",
            "partial mutation is not supported for sealed files",
            "partial mutation is only supported for inline files up to",
        )
        .await?;
        txn.commit().await?;
        Ok(data.len())
    }

    pub(crate) async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = prepare_write_at_file_txn(self, &mut txn, path).await?;
        let offset = inode.size;
        apply_inline_write_at(
            &mut txn,
            inode_id,
            &mut inode,
            offset,
            data,
            "append_file",
            "append is not supported for sealed files",
            "append is only supported for inline files up to",
        )
        .await?;
        txn.commit().await?;
        Ok(data.len())
    }

    pub(crate) async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        let (inode_id, mut inode) = resolve_path(&mut txn, path).await?;
        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if matches!(
            inode.data,
            DataRef::Object { .. } | DataRef::PackEntry { .. }
        ) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "truncate is not supported for sealed files".to_string()
            )));
        }

        if size == inode.size {
            inode.touch_atime();
            save_inode(&mut txn, &inode).await?;
            txn.commit().await?;
            return Ok(());
        }

        if !can_store_inline_u64(size) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "truncate is only supported for inline files up to {} bytes",
                fs9_config().inline_max_bytes
            ))));
        }

        let mut buf = load_inline_file_buffer(&mut txn, inode_id, &inode, "truncate").await?;
        blob::apply_truncate(&mut buf, size)?;
        blob::write_blob(&mut txn, inode_id, &buf).await?;
        inode.data = DataRef::InlineBlob;
        inode.size = size;
        bump_inode_generation(&mut inode)?;
        inode.touch_mtime();
        save_inode(&mut txn, &inode).await?;
        txn.commit().await?;
        Ok(())
    }

    pub(crate) async fn remove(&self, path: &str) -> Result<()> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot remove root".to_string()
            )));
        }

        let mut txn = self.begin().await?;
        let (inode_id, inode) = resolve_path(&mut txn, &normalized).await?;

        if inode.is_directory() {
            let children = list_dir(&mut txn, inode_id).await?;
            if !children.is_empty() {
                return Err(anyhow!(EmbeddedFsError::directory_not_empty(&normalized)));
            }
        } else {
            retire_inode_data_ref(
                &mut txn,
                inode_id,
                &inode.data,
                self.runtime_state().fs_instance_id,
            )
            .await?;
        }

        let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
        unlink(&mut txn, parent_inode, &name).await?;
        delete_inode(&mut txn, inode_id).await?;

        txn.commit().await?;
        Ok(())
    }

    pub(crate) async fn remove_recursive(&self, path: &str) -> Result<u64> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot remove root".to_string()
            )));
        }

        let mut txn = self.begin().await?;
        let (inode_id, inode) = resolve_path(&mut txn, &normalized).await?;
        let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;

        let removed = remove_inode_recursive(
            &mut txn,
            inode_id,
            inode,
            self.runtime_state().fs_instance_id,
        )
        .await?;
        unlink(&mut txn, parent_inode, &name).await?;

        txn.commit().await?;
        Ok(removed)
    }

    pub(crate) async fn mkdir(&self, path: &str, recursive: bool) -> Result<()> {
        let normalized = normalize_path(path);
        if normalized == "/" {
            return Ok(());
        }

        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            let mut txn = self.begin().await?;
            let result: Result<()> = async {
                if !recursive {
                    let (parent_inode, name) = resolve_parent(&mut txn, &normalized).await?;
                    if lookup(&mut txn, parent_inode, &name).await?.is_some() {
                        return Err(anyhow!(EmbeddedFsError::already_exists(&normalized)));
                    }

                    let new_inode_id = self.alloc_inode_id().await?;
                    let inode = Inode::new_directory(new_inode_id, 0o755);
                    save_inode(&mut txn, &inode).await?;
                    link(&mut txn, parent_inode, &name, new_inode_id).await?;
                    txn.commit().await?;
                    return Ok(());
                }

                let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
                let mut current_inode = ROOT_INODE;

                for part in parts {
                    if let Some(next_inode_id) = lookup(&mut txn, current_inode, part).await? {
                        let next_inode = load_inode(&mut txn, next_inode_id)
                            .await?
                            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&normalized)))?;
                        if !next_inode.is_directory() {
                            return Err(anyhow!(EmbeddedFsError::not_directory(part)));
                        }
                        current_inode = next_inode_id;
                    } else {
                        let new_inode_id = self.alloc_inode_id().await?;
                        let inode = Inode::new_directory(new_inode_id, 0o755);
                        save_inode(&mut txn, &inode).await?;
                        link(&mut txn, current_inode, part, new_inode_id).await?;
                        current_inode = new_inode_id;
                    }
                }

                txn.commit().await?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => return Ok(()),
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal("mkdir retry exhausted")))
    }

    pub(crate) async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old_normalized = normalize_path(old_path);
        let new_normalized = normalize_path(new_path);
        let new_has_trailing_slash = new_path.len() > 1 && new_path.ends_with('/');

        if old_normalized == "/" {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "cannot rename root".to_string(),
            )));
        }

        // No-op if paths are identical — but source must exist (POSIX: ENOENT)
        if old_normalized == new_normalized {
            let mut txn = self.begin().await?;
            let (_, inode) = resolve_path(&mut txn, &old_normalized).await?;
            if new_has_trailing_slash && !inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
            }
            return Ok(());
        }

        let mut txn = self.begin().await?;

        // Resolve the source first — NotFound takes precedence over cycle check
        let (old_inode_id, old_inode) = resolve_path(&mut txn, &old_normalized).await?;
        let (old_parent_inode, old_name) = resolve_parent(&mut txn, &old_normalized).await?;

        // Destination parent must already exist (POSIX semantics — no auto-create)
        let (new_parent_inode, new_name) = resolve_parent(&mut txn, &new_normalized).await?;

        // If destination has trailing slash, source must be a directory (POSIX ENOTDIR).
        if new_has_trailing_slash && !old_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
        }

        // Prevent directory cycle: renaming a dir into its own subtree
        // would corrupt the directory tree (POSIX returns EINVAL for this).
        // Parent existence must be checked first so ENOENT takes precedence.
        // Only applies to directories — files cannot create cycles.
        if old_inode.is_directory() && new_normalized.starts_with(&format!("{old_normalized}/")) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "cannot rename {old_normalized} into its own subdirectory {new_normalized}"
            ))));
        }

        // Check if destination already exists
        if let Some(existing_inode_id) = lookup(&mut txn, new_parent_inode, &new_name).await? {
            let existing_inode = load_inode(&mut txn, existing_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&new_normalized)))?;

            if new_has_trailing_slash && !existing_inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
            }

            if existing_inode.is_directory() {
                // Don't replace existing directories
                return Err(anyhow!(EmbeddedFsError::already_exists(&new_normalized)));
            }

            if old_inode.is_directory() {
                // Can't overwrite a file with a directory
                return Err(anyhow!(EmbeddedFsError::not_directory(&new_normalized)));
            }

            // Source is file, dest is file: replace (delete dest's data)
            retire_inode_data_ref(
                &mut txn,
                existing_inode_id,
                &existing_inode.data,
                self.runtime_state().fs_instance_id,
            )
            .await?;
            delete_inode(&mut txn, existing_inode_id).await?;
            unlink(&mut txn, new_parent_inode, &new_name).await?;
        }

        // Unlink from old parent, link to new parent
        unlink(&mut txn, old_parent_inode, &old_name).await?;
        link(&mut txn, new_parent_inode, &new_name, old_inode_id).await?;

        txn.commit().await?;
        Ok(())
    }

    async fn plan_stream_read(
        &self,
        mut txn: Transaction,
        inode_id: u64,
        inode: Inode,
    ) -> Result<StreamReadPlan> {
        match &inode.data {
            DataRef::Object { key, .. } => Ok(StreamReadPlan::Object { key: key.clone() }),
            DataRef::PackEntry {
                bundle_id,
                offset,
                len,
                ..
            } => {
                let manifest = load_bundle_manifest(&mut txn, *bundle_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("bundle manifest missing")))?;
                let entry_len = usize::try_from(*len).map_err(|_| {
                    anyhow!(EmbeddedFsError::internal("pack entry length exceeds usize"))
                })?;
                Ok(StreamReadPlan::Pack {
                    manifest,
                    bundle_offset: *offset,
                    len: entry_len.min(usize::try_from(inode.size).unwrap_or(entry_len)),
                })
            }
            _ => Ok(StreamReadPlan::Inline {
                txn,
                inode_id,
                inode,
            }),
        }
    }

    async fn stream_read_plan_into_channel(
        &self,
        plan: StreamReadPlan,
        sender: mpsc::Sender<std::io::Result<Vec<u8>>>,
    ) -> Result<()> {
        match plan {
            StreamReadPlan::Object { key } => {
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                let mut reader = s3.get_object_stream(&key).await?;
                let mut buf = vec![0u8; STREAM_READ_CHUNK_BYTES];
                loop {
                    let n = reader.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    if sender.send(Ok(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
            StreamReadPlan::Pack {
                manifest,
                bundle_offset,
                len,
            } => {
                if len == 0 {
                    return Ok(());
                }
                let s3 = self
                    .s3_client()
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;
                let mut reader = s3
                    .get_object_range_stream(&manifest.key, bundle_offset, len)
                    .await?;
                let mut remaining = len;
                let mut buf = vec![0u8; STREAM_READ_CHUNK_BYTES];
                while remaining > 0 {
                    if sender.is_closed() {
                        break;
                    }
                    let n = reader
                        .read(&mut buf[..STREAM_READ_CHUNK_BYTES.min(remaining)])
                        .await?;
                    if n == 0 {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "pack entry range read ended early"
                        )));
                    }
                    remaining = remaining.saturating_sub(n);
                    if sender.send(Ok(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
            StreamReadPlan::Inline {
                mut txn,
                inode_id,
                inode,
            } => {
                let mut offset = 0u64;
                let file_size = inode.size;
                while offset < file_size {
                    let remaining = file_size - offset;
                    let chunk_len = usize::try_from(remaining.min(STREAM_READ_CHUNK_BYTES as u64))
                        .map_err(|_| {
                            anyhow!(EmbeddedFsError::internal("stream chunk exceeds usize"))
                        })?;
                    let chunk =
                        read_file_range_from_txn(&mut txn, inode_id, &inode, offset, chunk_len)
                            .await?;

                    if sender.send(Ok(chunk)).await.is_err() {
                        break;
                    }

                    offset = offset.checked_add(chunk_len as u64).ok_or_else(|| {
                        anyhow!(EmbeddedFsError::internal("stream offset overflow"))
                    })?;
                }

                Ok(())
            }
        }
    }

    async fn flush_staged_write_chunk(
        &self,
        staging_inode_id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let mut txn = self.begin().await?;
        let mut inode = load_inode(&mut txn, staging_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
        if inode.nlink != 0 {
            return Err(anyhow!(EmbeddedFsError::internal(
                "staging inode already published"
            )));
        }

        match inode.data {
            DataRef::None | DataRef::StagingPages => {
                inode.data = DataRef::StagingPages;
            }
            _ => {
                return Err(anyhow!(EmbeddedFsError::internal(
                    "staging inode has unexpected non-paged data ref"
                )))
            }
        }

        write_staging_chunk_to_txn(&mut txn, staging_inode_id, &mut inode, offset, data).await?;
        save_inode(&mut txn, &inode).await?;
        mark_staging_write(&mut txn, staging_inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn touch_staging_write(&self, staging_inode_id: u64) -> Result<()> {
        let mut txn = self.begin().await?;
        mark_staging_write(&mut txn, staging_inode_id, current_unix_timestamp()).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn publish_staged_write(&self, path: &str, staging_inode_id: u64) -> Result<usize> {
        self.publish_staged_write_with_reservation(path, staging_inode_id, None)
            .await
    }

    async fn publish_staged_write_with_reservation(
        &self,
        path: &str,
        staging_inode_id: u64,
        reservation: Option<&UploadReservation>,
    ) -> Result<usize> {
        let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
        for attempt in 0..attempts {
            match self
                .publish_staged_write_once(path, staging_inode_id, reservation)
                .await
            {
                Ok((size, orphan_inode_id)) => {
                    if let Some(inode_id) = orphan_inode_id {
                        self.spawn_orphan_cleanup(inode_id);
                    }
                    return Ok(size);
                }
                Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                    fs9_commit_backoff(attempt).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(anyhow!(EmbeddedFsError::internal(
            "publish staged write retry exhausted"
        )))
    }

    async fn publish_staged_write_once(
        &self,
        path: &str,
        staging_inode_id: u64,
        reservation: Option<&UploadReservation>,
    ) -> Result<(usize, Option<u64>)> {
        let mut txn = self.begin().await?;
        if let Some(reservation) = reservation {
            verify_upload_publish_preconditions(&mut txn, path, reservation).await?;
        }
        let (parent_inode, name) = ensure_parents_and_resolve_parent(self, &mut txn, path).await?;

        let mut staging_inode = load_inode(&mut txn, staging_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("staging inode missing")))?;
        if staging_inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        // Publish should advance the per-path generation counter so CAS can detect
        // intervening in-place mutations even when inode ids are stable.
        let mut publish_generation = staging_inode.generation.max(1);
        let orphan_inode_id =
            if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
                let mut existing_inode = load_inode(&mut txn, existing_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;
                if existing_inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(path)));
                }

                publish_generation = existing_inode.generation.checked_add(1).ok_or_else(|| {
                    anyhow!(EmbeddedFsError::internal("inode generation overflow"))
                })?;
                existing_inode.nlink = 0;
                save_inode(&mut txn, &existing_inode).await?;
                match &existing_inode.data {
                    DataRef::Object { .. } => {
                        retire_inode_data_ref(
                            &mut txn,
                            existing_inode_id,
                            &existing_inode.data,
                            self.runtime_state().fs_instance_id,
                        )
                        .await?;
                        None
                    }
                    DataRef::PackEntry { .. } => {
                        retire_inode_data_ref(
                            &mut txn,
                            existing_inode_id,
                            &existing_inode.data,
                            self.runtime_state().fs_instance_id,
                        )
                        .await?;
                        delete_inode(&mut txn, existing_inode_id).await?;
                        None
                    }
                    DataRef::None | DataRef::InlineBlob => {
                        mark_orphan_inode(&mut txn, existing_inode_id).await?;
                        Some(existing_inode_id)
                    }
                    DataRef::StagingPages => {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "published file cannot use staging pages during overwrite",
                        )))
                    }
                }
            } else {
                None
            };

        staging_inode.generation = publish_generation;
        staging_inode.nlink = 1;
        staging_inode.touch_mtime();
        save_inode(&mut txn, &staging_inode).await?;
        link(&mut txn, parent_inode, &name, staging_inode_id).await?;
        lifecycle::clear_lifecycle(&mut txn, staging_inode_id).await?;
        clear_staging_write(&mut txn, staging_inode_id).await?;
        txn.commit().await?;

        Ok((
            usize::try_from(staging_inode.size)
                .map_err(|_| anyhow!(EmbeddedFsError::internal("staging size exceeds usize")))?,
            orphan_inode_id,
        ))
    }

    async fn abort_staged_write(&self, staging_inode_id: u64) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        let inode = load_inode(&mut txn, staging_inode_id).await?;
        let lifecycle_state = lifecycle::load_lifecycle(&mut txn, staging_inode_id).await?;
        let _ = txn.rollback().await;

        // Guard: if the inode has been published (nlink > 0), it is live data.
        // This can happen when publish_staged_write commits in TiKV but the
        // client observes a timeout and triggers abort. Deleting it would
        // corrupt the published file.
        if inode.as_ref().is_some_and(|inode| inode.nlink != 0) {
            return Ok(());
        }

        let Some(expected) = lifecycle_state else {
            return self.cleanup_staging_inode(staging_inode_id).await;
        };

        let s3 = self
            .s3_client()
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("S3 is not configured")))?;

        let object_key = || -> Result<String> {
            if let Some(inode) = inode.as_ref() {
                if let DataRef::Object { key, .. } = &inode.data {
                    return Ok(key.clone());
                }
            }
            self.object_key(staging_inode_id)
        };

        let mut external_ok = true;
        match &expected {
            FileLifecycle::Deleting { data_ref, .. } => {
                if let DataRef::Object { key, .. } = data_ref {
                    if let Err(err) = s3.delete_object(key).await {
                        warn!(
                            "fs9 abort: failed to delete object for inode {staging_inode_id}: {err}"
                        );
                        external_ok = false;
                    }
                }
            }
            FileLifecycle::Uploading { upload_id, .. } => {
                let key = object_key()?;
                if let Some(upload_id) = upload_id.as_deref() {
                    if let Err(err) = s3.abort_multipart_upload(&key, upload_id).await {
                        warn!(
                            "fs9 abort: failed to abort multipart upload for inode {staging_inode_id}: {err}"
                        );
                        external_ok = false;
                    }
                }
                if let Err(err) = s3.delete_object(&key).await {
                    warn!("fs9 abort: failed to delete object for inode {staging_inode_id}: {err}");
                    external_ok = false;
                }
            }
            FileLifecycle::Committing { .. } => {
                let key = object_key()?;
                if let Err(err) = s3.delete_object(&key).await {
                    warn!("fs9 abort: failed to delete object for inode {staging_inode_id}: {err}");
                    external_ok = false;
                }
            }
            FileLifecycle::Packing { .. } => {
                // Packing is not implemented in M4; preserve lifecycle state for future GC.
                external_ok = false;
            }
        }

        if !external_ok {
            self.mark_lifecycle_stale_for_retry(staging_inode_id, &expected)
                .await?;
            return Ok(());
        }

        let mut txn = self.begin_internal().await?;
        let current = lifecycle::load_lifecycle(&mut txn, staging_inode_id).await?;
        if current.as_ref() == Some(&expected) {
            lifecycle::clear_lifecycle(&mut txn, staging_inode_id).await?;
            clear_staging_write(&mut txn, staging_inode_id).await?;
            if let Some(inode) = load_inode(&mut txn, staging_inode_id).await? {
                if inode.nlink == 0 && !inode.is_directory() {
                    delete_inode(&mut txn, staging_inode_id).await?;
                }
            }
            txn.commit().await?;
        } else {
            let _ = txn.rollback().await;
        }

        Ok(())
    }

    async fn mark_lifecycle_stale_for_retry(
        &self,
        inode_id: u64,
        expected: &FileLifecycle,
    ) -> Result<()> {
        let stale = match expected {
            FileLifecycle::Uploading {
                fs_instance_id,
                upload_id,
                updated_at: _,
                reservation,
            } => FileLifecycle::Uploading {
                fs_instance_id: *fs_instance_id,
                upload_id: upload_id.clone(),
                updated_at: 0,
                reservation: reservation.clone(),
            },
            FileLifecycle::Committing {
                fs_instance_id,
                updated_at: _,
                reservation,
            } => FileLifecycle::Committing {
                fs_instance_id: *fs_instance_id,
                updated_at: 0,
                reservation: reservation.clone(),
            },
            FileLifecycle::Packing {
                fs_instance_id,
                bundle_id,
                updated_at: _,
            } => FileLifecycle::Packing {
                fs_instance_id: *fs_instance_id,
                bundle_id: *bundle_id,
                updated_at: 0,
            },
            FileLifecycle::Deleting { .. } => return Ok(()),
        };

        let mut txn = self.begin_internal().await?;
        let current = lifecycle::load_lifecycle(&mut txn, inode_id).await?;
        if current.as_ref() == Some(expected) {
            lifecycle::save_lifecycle(&mut txn, inode_id, &stale).await?;
            txn.commit().await?;
        } else {
            let _ = txn.rollback().await;
        }
        Ok(())
    }

    async fn cleanup_marked_orphans(&self) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        let orphan_inode_ids = list_orphan_inodes(&mut txn).await?;
        let _ = txn.rollback().await;

        for inode_id in orphan_inode_ids {
            self.cleanup_orphan_inode(inode_id).await?;
        }
        Ok(())
    }

    async fn cleanup_stale_staging_writes(&self) -> Result<()> {
        let cutoff = current_unix_timestamp().saturating_sub(STALE_WRITE_STREAM_SECS);
        let mut txn = self.begin_internal().await?;
        let staging_inode_ids = list_stale_staging_writes(&mut txn, cutoff).await?;
        let _ = txn.rollback().await;

        for inode_id in staging_inode_ids {
            self.cleanup_staging_inode(inode_id).await?;
        }
        Ok(())
    }

    async fn cleanup_staging_inode(&self, inode_id: u64) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        lifecycle::clear_lifecycle(&mut txn, inode_id).await?;
        clear_staging_write(&mut txn, inode_id).await?;

        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
            // Guard: if the inode has been published (nlink > 0), it is live data.
            // This can happen when publish_staged_write commits in TiKV but the
            // client observes a timeout and triggers abort. Deleting it would
            // corrupt the published file.
            if inode.nlink != 0 {
                let _ = txn.rollback().await;
                return Ok(());
            }
            if !inode.is_directory() {
                match inode.data {
                    DataRef::None => {}
                    DataRef::InlineBlob => blob::delete_blob(&mut txn, inode_id).await?,
                    DataRef::Object { .. } => {
                        // Preserve a durable lifecycle marker so object cleanup remains retryable.
                        lifecycle::save_lifecycle(
                            &mut txn,
                            inode_id,
                            &FileLifecycle::Deleting {
                                fs_instance_id: self.runtime_state().fs_instance_id,
                                data_ref: inode.data.clone(),
                            },
                        )
                        .await?;
                    }
                    DataRef::StagingPages => delete_staging_pages(&mut txn, inode_id).await?,
                    other => {
                        return Err(anyhow!(EmbeddedFsError::internal(&format!(
                            "staging cleanup not implemented for data ref {other:?}"
                        ))));
                    }
                }
            }
            delete_inode(&mut txn, inode_id).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    async fn cleanup_orphan_inode(&self, inode_id: u64) -> Result<()> {
        let mut txn = self.begin_internal().await?;
        clear_orphan_inode(&mut txn, inode_id).await?;

        if let Some(inode) = load_inode(&mut txn, inode_id).await? {
            if !inode.is_directory() {
                match inode.data {
                    DataRef::None => {}
                    DataRef::InlineBlob => blob::delete_blob(&mut txn, inode_id).await?,
                    DataRef::Object { .. } => {}
                    DataRef::StagingPages => {
                        return Err(anyhow!(EmbeddedFsError::internal(
                            "published orphan inode cannot use staging pages",
                        )))
                    }
                    other => {
                        return Err(anyhow!(EmbeddedFsError::internal(&format!(
                            "orphan cleanup not implemented for data ref {other:?}"
                        ))));
                    }
                }
            }
            delete_inode(&mut txn, inode_id).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    fn spawn_orphan_cleanup(&self, inode_id: u64) {
        let fs = self.clone();
        tokio::spawn(async move {
            if let Err(err) = fs.cleanup_orphan_inode(inode_id).await {
                warn!("embedded fs orphan cleanup failed for inode {inode_id}: {err}");
            }
        });
    }
}

async fn prepare_replace_file_txn(
    fs: &EmbeddedPageFs,
    txn: &mut Transaction,
    path: &str,
) -> Result<(u64, Inode)> {
    let (parent_inode, name) = ensure_parents_and_resolve_parent(fs, txn, path).await?;

    if let Some(existing_inode_id) = lookup(txn, parent_inode, &name).await? {
        let mut inode = load_inode(txn, existing_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        retire_inode_data_ref(
            txn,
            existing_inode_id,
            &inode.data,
            fs.runtime_state().fs_instance_id,
        )
        .await?;
        inode.data = DataRef::None;
        inode.size = 0;
        Ok((existing_inode_id, inode))
    } else {
        let inode_id = fs.alloc_inode_id().await?;
        let mut inode = Inode::new_file(inode_id, 0o644);
        inode.data = DataRef::None;
        link(txn, parent_inode, &name, inode_id).await?;
        Ok((inode_id, inode))
    }
}

async fn prepare_write_at_file_txn(
    fs: &EmbeddedPageFs,
    txn: &mut Transaction,
    path: &str,
) -> Result<(u64, Inode)> {
    let (parent_inode, name) = ensure_parents_and_resolve_parent(fs, txn, path).await?;

    if let Some(existing_inode_id) = lookup(txn, parent_inode, &name).await? {
        let inode = load_inode(txn, existing_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }

        Ok((existing_inode_id, inode))
    } else {
        let inode_id = fs.alloc_inode_id().await?;
        let inode = Inode::new_file(inode_id, 0o644);
        save_inode(txn, &inode).await?;
        link(txn, parent_inode, &name, inode_id).await?;
        Ok((inode_id, inode))
    }
}

fn current_object_store_binding() -> Option<ObjectStoreBinding> {
    let s3 = fs9_config().s3.as_ref()?;
    Some(ObjectStoreBinding {
        bucket: s3.bucket.clone(),
        region: s3.region.clone(),
        endpoint: s3.endpoint.clone(),
        prefix: s3.prefix.clone(),
        force_path_style: s3.force_path_style,
    })
}

fn new_fs_instance_id() -> [u8; 16] {
    loop {
        let id = rand::random::<[u8; 16]>();
        if id != [0u8; 16] {
            return id;
        }
    }
}

fn build_object_key(keyspace: &str, state: &FsRuntimeState, inode_id: u64) -> Result<String> {
    let Some(object_store) = state.object_store.as_ref() else {
        return Err(anyhow!(EmbeddedFsError::internal(
            "object storage is not configured for this filesystem",
        )));
    };

    let mut key = String::new();
    if !object_store.prefix.is_empty() {
        key.push_str(&object_store.prefix);
        key.push('/');
    }

    key.push_str(&encode_s3_key_component(keyspace));
    key.push('/');
    key.push_str(&state.fs_instance_id_hex);
    key.push_str("/objects/");
    let digest = Sha256::digest(inode_id.to_be_bytes());
    use std::fmt::Write as _;
    let _ = write!(&mut key, "{:02x}{:02x}/{}", digest[0], digest[1], inode_id);
    Ok(key)
}

fn build_bundle_key(keyspace: &str, state: &FsRuntimeState, bundle_id: u64) -> Result<String> {
    let Some(object_store) = state.object_store.as_ref() else {
        return Err(anyhow!(EmbeddedFsError::internal(
            "object storage is not configured for this filesystem",
        )));
    };

    let mut key = String::new();
    if !object_store.prefix.is_empty() {
        key.push_str(&object_store.prefix);
        key.push('/');
    }

    key.push_str(&encode_s3_key_component(keyspace));
    key.push('/');
    key.push_str(&state.fs_instance_id_hex);
    key.push_str("/packs/");
    key.push_str(&bundle_id.to_string());
    key.push_str(".pack");
    Ok(key)
}

fn runtime_state_from_superblock(keyspace: &str, superblock: &Superblock) -> FsRuntimeState {
    FsRuntimeState {
        identity: FsInstanceIdentity::new(keyspace.to_string(), superblock.fs_instance_id),
        fs_instance_id: superblock.fs_instance_id,
        fs_instance_id_hex: hex::encode(superblock.fs_instance_id),
        object_store: superblock.object_store.clone(),
    }
}

fn process_identity_registry() -> &'static Mutex<HashMap<String, FsInstanceIdentity>> {
    FS9_PROCESS_IDENTITIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remembered_process_identity(keyspace: &str) -> Option<FsInstanceIdentity> {
    let guard = match process_identity_registry().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.get(keyspace).cloned()
}

fn register_process_identity(identity: &FsInstanceIdentity) -> Result<()> {
    let mut guard = match process_identity_registry().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    match guard.get(identity.keyspace.as_str()) {
        Some(expected) if expected == identity => Ok(()),
        Some(expected) => Err(restart_required_for_keyspace_instance(
            &identity.keyspace,
            Some(expected),
            Some(identity),
        )),
        None => {
            guard.insert(identity.keyspace.clone(), identity.clone());
            Ok(())
        }
    }
}

fn restart_required_message_for_keyspace_instance(
    keyspace: &str,
    expected: Option<&FsInstanceIdentity>,
    observed: Option<&FsInstanceIdentity>,
) -> String {
    match (expected, observed) {
        (Some(expected), Some(observed)) => format!(
            "filesystem instance for keyspace '{keyspace}' changed while db9-server stayed alive (expected {}, observed {}); fs9 does not support hot keyspace-instance switching, so restart db9-server before reacquiring this keyspace",
            hex::encode(expected.fs_instance_id),
            hex::encode(observed.fs_instance_id),
        ),
        (Some(expected), None) => format!(
            "filesystem instance for keyspace '{keyspace}' disappeared after db9-server had already bound to {}; fs9 does not support hot keyspace-instance switching, so restart db9-server before reacquiring this keyspace",
            hex::encode(expected.fs_instance_id),
        ),
        (None, Some(observed)) => format!(
            "filesystem instance for keyspace '{keyspace}' changed to {}; fs9 does not support hot keyspace-instance switching, so restart db9-server before reacquiring this keyspace",
            hex::encode(observed.fs_instance_id),
        ),
        (None, None) => format!(
            "filesystem instance for keyspace '{keyspace}' changed while db9-server stayed alive; fs9 does not support hot keyspace-instance switching, so restart db9-server before reacquiring this keyspace",
        ),
    }
}

fn restart_required_for_keyspace_instance(
    keyspace: &str,
    expected: Option<&FsInstanceIdentity>,
    observed: Option<&FsInstanceIdentity>,
) -> anyhow::Error {
    let detail = restart_required_message_for_keyspace_instance(keyspace, expected, observed);
    anyhow!(EmbeddedFsError::restart_required(&detail))
}

fn pack_spool_keyspace_root(keyspace: &str) -> PathBuf {
    fs9_config()
        .pack_spool_root
        .join(encode_s3_key_component(keyspace))
}

fn pack_spool_root(identity: &FsInstanceIdentity, fs_instance_id_hex: &str) -> PathBuf {
    pack_spool_keyspace_root(&identity.keyspace)
        .join(format!("format-{}", FS9_STORAGE_FORMAT_VERSION))
        .join(fs_instance_id_hex)
}

fn pack_spool_heartbeat_path(root: &Path) -> PathBuf {
    root.join(PACK_SPOOL_HEARTBEAT_FILE)
}

fn pack_spool_stale_grace_secs() -> i64 {
    let gc_window = fs9_config()
        .gc_interval_secs
        .max(fs9_config().gc_max_backoff_secs);
    (STALE_WRITE_STREAM_SECS as u64)
        .max(STALE_PACKING_SECS as u64)
        .max(gc_window) as i64
}

async fn touch_pack_spool_heartbeat(root: &Path, now: i64) -> Result<()> {
    fs::create_dir_all(root).await?;
    fs::write(pack_spool_heartbeat_path(root), now.to_string()).await?;
    Ok(())
}

async fn read_pack_spool_heartbeat(root: &Path) -> Result<Option<i64>> {
    match fs::read_to_string(pack_spool_heartbeat_path(root)).await {
        Ok(contents) => {
            if let Ok(heartbeat) = contents.trim().parse::<i64>() {
                return Ok(Some(heartbeat));
            }

            let metadata = match fs::metadata(root).await {
                Ok(metadata) => metadata,
                Err(metadata_err) if metadata_err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(metadata_err) => return Err(metadata_err.into()),
            };
            Ok(Some(system_time_to_unix_secs(metadata.modified()?)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let metadata = match fs::metadata(root).await {
                Ok(metadata) => metadata,
                Err(metadata_err) if metadata_err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(metadata_err) => return Err(metadata_err.into()),
            };
            let modified = metadata.modified()?;
            Ok(Some(system_time_to_unix_secs(modified)))
        }
        Err(err) => Err(err.into()),
    }
}

async fn reap_stale_pack_spool_directories(
    current_root: &Path,
    now: i64,
    grace_secs: i64,
) -> Result<()> {
    let Some(parent) = current_root.parent() else {
        return Ok(());
    };

    let mut entries = match fs::read_dir(parent).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path == current_root {
            continue;
        }
        if !entry.file_type().await?.is_dir() {
            continue;
        }

        let Some(last_heartbeat) = read_pack_spool_heartbeat(&path).await? else {
            continue;
        };
        if now.saturating_sub(last_heartbeat) < grace_secs {
            continue;
        }

        remove_dir_all_if_exists(&path).await?;
    }

    Ok(())
}

async fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn system_time_to_unix_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn unregister_background_maintenance(identity: &FsInstanceIdentity) {
    let started = FS9_MAINTENANCE_STARTED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = match started.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.remove(identity);
}

fn maintenance_probe_action<E>(result: &std::result::Result<bool, E>) -> MaintenanceProbeAction {
    match result {
        Ok(true) => MaintenanceProbeAction::Run,
        Ok(false) => MaintenanceProbeAction::Stop,
        Err(_) => MaintenanceProbeAction::Defer,
    }
}

fn parse_superblock_bytes(data: &[u8]) -> Result<Superblock> {
    let sb: Superblock = serde_json::from_slice(data).map_err(|err| {
        anyhow!(
            "fs9: invalid superblock json: {err}. \
             Existing fs9 metadata is not compatible with this storage format."
        )
    })?;
    validate_superblock_format(&sb)?;
    Ok(sb)
}

fn validate_superblock_format(sb: &Superblock) -> Result<()> {
    if sb.format_version != FS9_STORAGE_FORMAT_VERSION {
        return Err(anyhow!(
            "fs9: unsupported storage format version {} (expected {}). \
             Recreate the fs9 keyspace with the current format.",
            sb.format_version,
            FS9_STORAGE_FORMAT_VERSION
        ));
    }
    if sb.fs_instance_id == [0u8; 16] {
        return Err(anyhow!(
            "fs9: invalid superblock: missing filesystem instance id. \
             Recreate the fs9 keyspace with the current format.",
        ));
    }
    Ok(())
}

fn validate_superblock_binding(
    superblock: &Superblock,
    current_binding: &Option<ObjectStoreBinding>,
    keyspace: &str,
) -> Result<()> {
    if &superblock.object_store == current_binding {
        return Ok(());
    }

    Err(anyhow!(
        "fs9: object storage binding mismatch for keyspace '{}': persisted={}, current={}. \
         fs9 binds a keyspace to one object store identity; recreate the keyspace to change it.",
        keyspace,
        describe_object_store_binding(&superblock.object_store),
        describe_object_store_binding(current_binding),
    ))
}

fn describe_object_store_binding(binding: &Option<ObjectStoreBinding>) -> String {
    let Some(binding) = binding else {
        return "disabled".to_string();
    };
    format!(
        "bucket={}, region={}, endpoint={}, prefix={}, force_path_style={}",
        binding.bucket,
        binding.region.as_deref().unwrap_or("-"),
        binding.endpoint.as_deref().unwrap_or("-"),
        if binding.prefix.is_empty() {
            "-"
        } else {
            binding.prefix.as_str()
        },
        binding.force_path_style,
    )
}

async fn fs_keyspace_is_empty(txn: &mut Transaction) -> Result<bool> {
    let start = keys::FS_NAMESPACE_PREFIX.to_vec();
    let end = keys::scan_end_key(keys::FS_NAMESPACE_PREFIX);
    let mut pairs = txn.scan(start..end, 1).await?;
    Ok(pairs.next().is_none())
}

async fn begin_transaction(client: &Arc<TransactionClient>) -> Result<Transaction> {
    let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
    client
        .begin_with_options(options)
        .await
        .map_err(|e| anyhow!(e))
}

async fn begin_read_transaction(client: &Arc<TransactionClient>) -> Result<Transaction> {
    // TiKV read-only transactions are snapshot handles. They finish on drop;
    // explicit rollback() is invalid for this transaction status.
    let options = TransactionOptions::new_optimistic()
        .read_only()
        .drop_check(CheckLevel::Warn);
    client
        .begin_with_options(options)
        .await
        .map_err(|e| anyhow!(e))
}

async fn load_current_superblock_if_present(txn: &mut Transaction) -> Result<Option<Superblock>> {
    match txn.get(keys::superblock_key()).await? {
        Some(data) => Ok(Some(parse_superblock_bytes(&data)?)),
        None => Ok(None),
    }
}

async fn load_superblock_if_present_for_keyspace(
    client: Arc<TransactionClient>,
    _keyspace: &str,
) -> Result<Option<Superblock>> {
    let mut txn = begin_transaction(&client).await?;
    let result: Result<Option<Superblock>> = async {
        let maybe_superblock = load_current_superblock_if_present(&mut txn).await?;
        let Some(superblock) = maybe_superblock else {
            return Ok(None);
        };

        if load_inode(&mut txn, ROOT_INODE).await?.is_none() {
            return Err(anyhow!(EmbeddedFsError::internal(
                "filesystem root inode missing for initialized fs9 keyspace",
            )));
        }
        let _ = load_allocator_counter(&mut txn, &keys::inode_allocator_key(), "inode").await?;
        let _ = load_allocator_counter(&mut txn, &keys::bundle_allocator_key(), "bundle").await?;
        Ok(Some(superblock))
    }
    .await;
    let _ = txn.rollback().await;
    result
}

async fn load_runtime_superblock_for_process(
    client: Arc<TransactionClient>,
    keyspace: &str,
) -> Result<Superblock> {
    if let Some(expected) = remembered_process_identity(keyspace) {
        let current = load_superblock_if_present_for_keyspace(client.clone(), keyspace).await?;
        match current {
            Some(superblock) => {
                let observed = EmbeddedPageFs::identity_from_superblock(keyspace, &superblock);
                if observed != expected {
                    return Err(restart_required_for_keyspace_instance(
                        keyspace,
                        Some(&expected),
                        Some(&observed),
                    ));
                }
                return Ok(superblock);
            }
            None => {
                return Err(restart_required_for_keyspace_instance(
                    keyspace,
                    Some(&expected),
                    None,
                ));
            }
        }
    }

    let superblock = load_or_init_superblock_for_keyspace(client, keyspace).await?;
    let identity = EmbeddedPageFs::identity_from_superblock(keyspace, &superblock);
    register_process_identity(&identity)?;
    Ok(superblock)
}

async fn load_or_init_superblock_for_keyspace(
    client: Arc<TransactionClient>,
    keyspace: &str,
) -> Result<Superblock> {
    let current_binding = current_object_store_binding();
    let attempts = fs9_config().tikv_commit_retry_attempts.max(1);
    for attempt in 0..attempts {
        let mut txn = begin_transaction(&client).await?;
        let result: Result<Superblock> = async {
            let maybe_superblock = load_current_superblock_if_present(&mut txn).await?;

            if let Some(superblock) = maybe_superblock {
                validate_superblock_binding(&superblock, &current_binding, keyspace)?;
                if load_inode(&mut txn, ROOT_INODE).await?.is_none() {
                    return Err(anyhow!(EmbeddedFsError::internal(
                        "filesystem root inode missing for initialized fs9 keyspace",
                    )));
                }
                let _ = load_allocator_counter(&mut txn, &keys::inode_allocator_key(), "inode")
                    .await?;
                let _ = load_allocator_counter(&mut txn, &keys::bundle_allocator_key(), "bundle")
                    .await?;
                let _ = txn.rollback().await;
                return Ok(superblock);
            }

            if !fs_keyspace_is_empty(&mut txn).await? {
                return Err(anyhow!(EmbeddedFsError::internal(
                    "fs9 keyspace contains metadata without a supported superblock; recreate the fs9 keyspace with the current format",
                )));
            }

            let superblock = Superblock::new(new_fs_instance_id(), current_binding.clone());
            save_superblock(&mut txn, &superblock).await?;
            save_allocator_counter(&mut txn, &keys::inode_allocator_key(), ROOT_INODE + 1).await?;
            save_allocator_counter(&mut txn, &keys::bundle_allocator_key(), 1).await?;
            let root = Inode::new_directory(ROOT_INODE, 0o755);
            save_inode(&mut txn, &root).await?;
            txn.commit().await?;
            Ok(superblock)
        }
        .await;

        match result {
            Ok(superblock) => return Ok(superblock),
            Err(err) if is_retryable_tikv_write_conflict(&err) && attempt + 1 < attempts => {
                fs9_commit_backoff(attempt).await;
            }
            Err(err) => return Err(err),
        }
    }

    Err(anyhow!(EmbeddedFsError::internal(
        "filesystem bootstrap retry exhausted",
    )))
}

#[cfg(test)]
async fn load_superblock(txn: &mut Transaction) -> Result<Superblock> {
    let data = txn
        .get(keys::superblock_key())
        .await?
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("superblock not found")))?;
    parse_superblock_bytes(&data)
}

async fn save_superblock(txn: &mut Transaction, sb: &Superblock) -> Result<()> {
    let data = serde_json::to_vec(sb)?;
    txn.put(keys::superblock_key(), data).await?;
    Ok(())
}

async fn load_allocator_counter(txn: &mut Transaction, key: &[u8], label: &str) -> Result<u64> {
    let data = txn.get(key.to_vec()).await?.ok_or_else(|| {
        anyhow!(EmbeddedFsError::internal(&format!(
            "{label} allocator key missing for initialized fs9 keyspace",
        )))
    })?;
    if data.len() != 8 {
        return Err(anyhow!(EmbeddedFsError::internal(&format!(
            "{label} allocator value has invalid length {}",
            data.len()
        ))));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data);
    Ok(u64::from_be_bytes(buf))
}

async fn save_allocator_counter(txn: &mut Transaction, key: &[u8], next: u64) -> Result<()> {
    txn.put(key.to_vec(), next.to_be_bytes().to_vec()).await?;
    Ok(())
}

async fn load_inode(txn: &mut Transaction, inode_id: u64) -> Result<Option<Inode>> {
    match txn.get(keys::inode_key(inode_id)).await? {
        Some(data) => Ok(Some(deserialize_inode(inode_id, &data)?)),
        None => Ok(None),
    }
}

struct TxnBatchStatStore<'a> {
    txn: &'a mut Transaction,
}

#[async_trait]
impl BatchStatStore for TxnBatchStatStore<'_> {
    async fn load_root_inode(&mut self) -> Result<Option<Inode>> {
        load_inode(self.txn, ROOT_INODE).await
    }

    async fn lookup_dir_entries(
        &mut self,
        requests: &[DirLookupRequest],
    ) -> Result<HashMap<(u64, String), Option<u64>>> {
        lookup_dir_entries_batch(self.txn, requests).await
    }

    async fn load_inodes(
        &mut self,
        inode_ids: &[u64],
    ) -> Result<HashMap<u64, Result<Option<Inode>>>> {
        load_inodes_batch_tolerant(self.txn, inode_ids).await
    }
}

fn deserialize_inode(inode_id: u64, data: &[u8]) -> Result<Inode> {
    serde_json::from_slice(data).map_err(|err| {
        anyhow!(
            "fs9: invalid inode json for inode {inode_id}: {err}. \
             Recreate the fs9 keyspace with the current format."
        )
    })
}

async fn load_inodes_batch(txn: &mut Transaction, inode_ids: &[u64]) -> Result<Vec<Option<Inode>>> {
    let mut out = Vec::with_capacity(inode_ids.len());

    for chunk in inode_ids.chunks(INODE_BATCH_GET_CHUNK_SIZE) {
        let keys: Vec<Vec<u8>> = chunk
            .iter()
            .map(|inode_id| keys::inode_key(*inode_id))
            .collect();
        let pairs = txn.batch_get(keys.iter().cloned()).await?;
        let mut by_key: HashMap<Key, tikv_client::Value> = HashMap::with_capacity(keys.len());

        for pair in pairs {
            let tikv_client::KvPair(key, value) = pair;
            by_key.insert(key, value);
        }

        for (inode_id, key) in chunk.iter().zip(keys.iter()) {
            let key_ref: &Key = key.into();
            if let Some(value) = by_key.get(key_ref) {
                out.push(Some(deserialize_inode(*inode_id, value)?));
            } else {
                out.push(None);
            }
        }
    }

    Ok(out)
}

async fn hydrate_directory_raw_entries(
    txn: &mut Transaction,
    raw_dirs: Vec<Vec<(String, u64)>>,
) -> Result<Vec<Vec<(String, Inode)>>> {
    let mut child_inode_ids = Vec::new();
    let mut seen_inode_ids = HashSet::new();
    for dir_entries in &raw_dirs {
        for (_, child_inode_id) in dir_entries {
            if seen_inode_ids.insert(*child_inode_id) {
                child_inode_ids.push(*child_inode_id);
            }
        }
    }

    let child_inodes = load_inodes_batch(txn, &child_inode_ids).await?;
    let child_inode_map: HashMap<u64, Inode> = child_inode_ids
        .into_iter()
        .zip(child_inodes.into_iter())
        .filter_map(|(inode_id, inode)| inode.map(|inode| (inode_id, inode)))
        .collect();

    Ok(raw_dirs
        .into_iter()
        .map(|dir_entries| {
            dir_entries
                .into_iter()
                .filter_map(|(name, child_inode_id)| {
                    child_inode_map
                        .get(&child_inode_id)
                        .cloned()
                        .map(|inode| (name, inode))
                })
                .collect::<Vec<_>>()
        })
        .collect())
}

async fn load_directory_entries_batch(
    txn: &mut Transaction,
    dir_inode_ids: &[u64],
) -> Result<Vec<Vec<(String, Inode)>>> {
    let mut raw_dirs = Vec::with_capacity(dir_inode_ids.len());
    for dir_inode_id in dir_inode_ids {
        raw_dirs.push(list_dir(txn, *dir_inode_id).await?);
    }
    hydrate_directory_raw_entries(txn, raw_dirs).await
}

async fn load_inodes_batch_tolerant(
    txn: &mut Transaction,
    inode_ids: &[u64],
) -> Result<HashMap<u64, Result<Option<Inode>>>> {
    let mut out = HashMap::with_capacity(inode_ids.len());

    for chunk in inode_ids.chunks(INODE_BATCH_GET_CHUNK_SIZE) {
        let keys: Vec<Vec<u8>> = chunk
            .iter()
            .map(|inode_id| keys::inode_key(*inode_id))
            .collect();
        let pairs = txn.batch_get(keys.iter().cloned()).await?;
        let mut by_key: HashMap<Key, tikv_client::Value> = HashMap::with_capacity(keys.len());

        for pair in pairs {
            let tikv_client::KvPair(key, value) = pair;
            by_key.insert(key, value);
        }

        for (inode_id, key) in chunk.iter().zip(keys.iter()) {
            let key_ref: &Key = key.into();
            let inode = if let Some(value) = by_key.get(key_ref) {
                deserialize_inode(*inode_id, value).map(Some)
            } else {
                Ok(None)
            };
            out.insert(*inode_id, inode);
        }
    }

    Ok(out)
}

async fn lookup_dir_entries_batch(
    txn: &mut Transaction,
    requests: &[DirLookupRequest],
) -> Result<HashMap<(u64, String), Option<u64>>> {
    let mut out = HashMap::with_capacity(requests.len());
    if requests.is_empty() {
        return Ok(out);
    }

    for chunk in requests.chunks(INODE_BATCH_GET_CHUNK_SIZE) {
        let keys: Vec<Vec<u8>> = chunk
            .iter()
            .map(|request| keys::dir_entry_key(request.parent_inode, &request.name))
            .collect();
        let pairs = txn.batch_get(keys.iter().cloned()).await?;
        let mut by_key: HashMap<Key, tikv_client::Value> = HashMap::with_capacity(keys.len());

        for pair in pairs {
            let tikv_client::KvPair(key, value) = pair;
            by_key.insert(key, value);
        }

        for (request, key) in chunk.iter().zip(keys.iter()) {
            let key_ref: &Key = key.into();
            let inode_id = match by_key.get(key_ref) {
                Some(value) if value.len() == 8 => {
                    Some(u64::from_be_bytes(value.as_slice().try_into()?))
                }
                Some(_) | None => None,
            };
            out.insert((request.parent_inode, request.name.clone()), inode_id);
        }
    }

    Ok(out)
}

async fn save_inode(txn: &mut Transaction, inode: &Inode) -> Result<()> {
    let data = serde_json::to_vec(inode)?;
    txn.put(keys::inode_key(inode.id), data).await?;
    Ok(())
}

async fn delete_inode(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::inode_key(inode_id)).await?;
    Ok(())
}

async fn lookup(txn: &mut Transaction, parent_inode: u64, name: &str) -> Result<Option<u64>> {
    match txn.get(keys::dir_entry_key(parent_inode, name)).await? {
        Some(data) if data.len() == 8 => {
            let id = u64::from_be_bytes(data.as_slice().try_into()?);
            Ok(Some(id))
        }
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

async fn link(
    txn: &mut Transaction,
    parent_inode: u64,
    name: &str,
    child_inode: u64,
) -> Result<()> {
    validate_dir_entry_name(name)?;
    txn.put(
        keys::dir_entry_key(parent_inode, name),
        child_inode.to_be_bytes().to_vec(),
    )
    .await?;
    Ok(())
}

async fn unlink(txn: &mut Transaction, parent_inode: u64, name: &str) -> Result<()> {
    txn.delete(keys::dir_entry_key(parent_inode, name)).await?;
    Ok(())
}

async fn list_dir(txn: &mut Transaction, parent_inode: u64) -> Result<Vec<(String, u64)>> {
    let prefix = keys::dir_prefix(parent_inode);
    let end = keys::scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, u32::MAX).await?;

    let mut out = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        let value = pair.1;
        let Some(name) = dir_entry_name_from_scan_key(&prefix, &key) else {
            continue;
        };
        if value.len() != 8 {
            continue;
        }
        let child_inode = u64::from_be_bytes(value.as_slice().try_into()?);
        out.push((name.to_string(), child_inode));
    }
    Ok(out)
}

fn next_scan_start_key(last_key: &[u8]) -> Vec<u8> {
    let mut next = last_key.to_vec();
    next.push(0);
    next
}

async fn load_directory_entries_limited(
    txn: &mut Transaction,
    dir_path: &str,
    parent_inode: u64,
    limit: usize,
    exclude_set: Option<&globset::GlobSet>,
) -> Result<(Vec<(String, Inode)>, bool)> {
    if limit == 0 {
        return Ok((Vec::new(), true));
    }

    let prefix = keys::dir_prefix(parent_inode);
    let end = keys::scan_end_key(&prefix);
    let mut start = prefix.clone();
    let mut out = Vec::new();

    loop {
        let mut pairs = txn
            .scan(
                start.clone()..end.clone(),
                u32::try_from(INODE_BATCH_GET_CHUNK_SIZE).unwrap_or(u32::MAX),
            )
            .await?
            .peekable();
        if pairs.peek().is_none() {
            break;
        }

        let mut raw_batch = Vec::new();
        let mut next_start = None;
        for pair in pairs {
            let key: Vec<u8> = pair.0.into();
            next_start = Some(next_scan_start_key(&key));

            let value = pair.1;
            let Some(name) = dir_entry_name_from_scan_key(&prefix, &key) else {
                continue;
            };
            if value.len() != 8 {
                continue;
            }
            let child_inode = u64::from_be_bytes(value.as_slice().try_into()?);
            raw_batch.push((name.to_string(), child_inode));
        }

        let mut hydrated = hydrate_directory_raw_entries(txn, vec![raw_batch]).await?;
        for (name, inode) in hydrated.pop().unwrap_or_default() {
            let child_path = if dir_path == "/" {
                format!("/{name}")
            } else {
                format!("{dir_path}/{name}")
            };
            if exclude_set.is_some_and(|set| {
                crate::extensions::fs::glob::path_matches_exclude(&child_path, set)
            }) {
                continue;
            }
            if out.len() >= limit {
                return Ok((out, true));
            }
            out.push((name, inode));
        }

        let Some(next_start) = next_start else {
            break;
        };
        start = next_start;
    }

    Ok((out, false))
}

fn clone_fs_error(err: &anyhow::Error) -> anyhow::Error {
    if let Some(fs_err) = err.downcast_ref::<EmbeddedFsError>() {
        anyhow!(fs_err.clone())
    } else {
        anyhow!(err.to_string())
    }
}

fn pack_read_end_offset(offset: u64, len: usize) -> Result<u64> {
    let len_u64 = u64::try_from(len)
        .map_err(|_| anyhow!(EmbeddedFsError::internal("pack read length exceeds u64")))?;
    offset
        .checked_add(len_u64)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("pack read offset overflow")))
}

fn plan_batch_inline_read_pack_windows(
    pack_reads: Vec<PendingBatchInlineReadPack>,
    max_window_bytes: usize,
) -> Result<Vec<PlannedBatchInlineReadPackWindow>> {
    if pack_reads.is_empty() {
        return Ok(Vec::new());
    }

    let max_window_bytes_u64 = u64::try_from(max_window_bytes)
        .map_err(|_| anyhow!(EmbeddedFsError::internal("pack read window exceeds u64")))?;
    let mut by_bundle: HashMap<u64, Vec<PendingBatchInlineReadPack>> = HashMap::new();
    for pending in pack_reads {
        by_bundle
            .entry(pending.bundle_id)
            .or_default()
            .push(pending);
    }

    let mut bundle_ids = by_bundle.keys().copied().collect::<Vec<_>>();
    bundle_ids.sort_unstable();

    let mut windows = Vec::new();
    for bundle_id in bundle_ids {
        let mut entries = by_bundle
            .remove(&bundle_id)
            .expect("bundle id must exist while planning pack windows");
        entries.sort_by_key(|entry| entry.bundle_offset);

        let mut current: Option<PlannedBatchInlineReadPackWindow> = None;
        for entry in entries {
            let entry_end = pack_read_end_offset(entry.bundle_offset, entry.len)?;
            let merge_plan = match current.as_ref() {
                Some(window) => {
                    let gap = entry.bundle_offset.saturating_sub(window.end_offset);
                    let merged_end = window.end_offset.max(entry_end);
                    let merged_len =
                        merged_end.checked_sub(window.start_offset).ok_or_else(|| {
                            anyhow!(EmbeddedFsError::internal("pack read window underflow"))
                        })?;
                    Some((
                        gap <= PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES
                            && merged_len <= max_window_bytes_u64,
                        merged_end,
                    ))
                }
                None => None,
            };

            if let Some((true, merged_end)) = merge_plan {
                let window = current
                    .as_mut()
                    .expect("current window must exist while merging pack reads");
                window.end_offset = merged_end;
                window.entries.push(PlannedBatchInlineReadPackWindowEntry {
                    result_idx: entry.result_idx,
                    bundle_offset: entry.bundle_offset,
                    len: entry.len,
                });
            } else {
                if let Some(window) = current.take() {
                    windows.push(window);
                }
                current = Some(PlannedBatchInlineReadPackWindow {
                    bundle_id,
                    start_offset: entry.bundle_offset,
                    end_offset: entry_end,
                    entries: vec![PlannedBatchInlineReadPackWindowEntry {
                        result_idx: entry.result_idx,
                        bundle_offset: entry.bundle_offset,
                        len: entry.len,
                    }],
                });
            }
        }

        if let Some(window) = current.take() {
            windows.push(window);
        }
    }

    Ok(windows)
}

fn fail_resolved_path_requests(
    results: &mut [Option<Result<ResolvedPath>>],
    request_indices: impl IntoIterator<Item = usize>,
    err: &anyhow::Error,
) {
    for idx in request_indices {
        results[idx] = Some(Err(clone_fs_error(err)));
    }
}

async fn resolve_paths_with_ids_batched<S>(
    store: &mut S,
    paths: &[String],
) -> Vec<Result<ResolvedPath>>
where
    S: BatchStatStore + Send,
{
    let requests: Vec<BatchStatRequest> = paths
        .iter()
        .map(|path| {
            let normalized = normalize_path(path);
            let parts = normalized
                .split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect();
            BatchStatRequest { normalized, parts }
        })
        .collect();
    let mut results: Vec<Option<Result<ResolvedPath>>> =
        (0..requests.len()).map(|_| None).collect();
    let mut pending = Vec::new();
    let mut root_request_indices = Vec::new();

    for (idx, request) in requests.iter().enumerate() {
        if request.parts.is_empty() {
            root_request_indices.push(idx);
        } else {
            pending.push(PendingBatchStat {
                request_idx: idx,
                parent_inode: ROOT_INODE,
                next_part_idx: 0,
            });
        }
    }

    if !root_request_indices.is_empty() {
        match store.load_root_inode().await {
            Ok(Some(root_inode)) => {
                for idx in root_request_indices {
                    results[idx] = Some(Ok(ResolvedPath {
                        inode_id: ROOT_INODE,
                        inode: root_inode.clone(),
                    }));
                }
            }
            Ok(None) => {
                let err = anyhow!(EmbeddedFsError::internal("root inode missing"));
                fail_resolved_path_requests(&mut results, 0..requests.len(), &err);
            }
            Err(err) => fail_resolved_path_requests(&mut results, 0..requests.len(), &err),
        }
    }

    while !pending.is_empty() {
        let mut unique_requests = Vec::new();
        let mut seen_requests = HashSet::new();
        for step in &pending {
            let request = &requests[step.request_idx];
            let lookup = DirLookupRequest {
                parent_inode: step.parent_inode,
                name: request.parts[step.next_part_idx].clone(),
            };
            if seen_requests.insert((lookup.parent_inode, lookup.name.clone())) {
                unique_requests.push(lookup);
            }
        }

        let dir_entries = match store.lookup_dir_entries(&unique_requests).await {
            Ok(entries) => entries,
            Err(err) => {
                fail_resolved_path_requests(
                    &mut results,
                    pending.iter().map(|step| step.request_idx),
                    &err,
                );
                break;
            }
        };
        let mut child_inode_ids = Vec::new();
        let mut seen_inode_ids = HashSet::new();
        for step in &pending {
            let request = &requests[step.request_idx];
            let key = (step.parent_inode, request.parts[step.next_part_idx].clone());
            if let Some(Some(child_inode_id)) = dir_entries.get(&key) {
                if seen_inode_ids.insert(*child_inode_id) {
                    child_inode_ids.push(*child_inode_id);
                }
            }
        }
        let child_inodes = match store.load_inodes(&child_inode_ids).await {
            Ok(inodes) => inodes,
            Err(err) => {
                fail_resolved_path_requests(
                    &mut results,
                    pending.iter().map(|step| step.request_idx),
                    &err,
                );
                break;
            }
        };

        let mut next_pending = Vec::new();
        for step in pending.drain(..) {
            let request = &requests[step.request_idx];
            let part = &request.parts[step.next_part_idx];
            let lookup_key = (step.parent_inode, part.clone());
            let Some(Some(child_inode_id)) = dir_entries.get(&lookup_key) else {
                results[step.request_idx] = Some(Err(anyhow!(EmbeddedFsError::not_found(
                    &request.normalized
                ))));
                continue;
            };

            let Some(inode_result) = child_inodes.get(child_inode_id) else {
                results[step.request_idx] = Some(Err(anyhow!(EmbeddedFsError::not_found(
                    &request.normalized
                ))));
                continue;
            };
            let inode = match inode_result {
                Ok(Some(inode)) => inode.clone(),
                Ok(None) => {
                    results[step.request_idx] = Some(Err(anyhow!(EmbeddedFsError::not_found(
                        &request.normalized
                    ))));
                    continue;
                }
                Err(err) => {
                    results[step.request_idx] = Some(Err(clone_fs_error(err)));
                    continue;
                }
            };

            if step.next_part_idx + 1 == request.parts.len() {
                results[step.request_idx] = Some(Ok(ResolvedPath {
                    inode_id: *child_inode_id,
                    inode,
                }));
                continue;
            }

            if !inode.is_directory() {
                results[step.request_idx] =
                    Some(Err(anyhow!(EmbeddedFsError::not_directory(part))));
                continue;
            }

            next_pending.push(PendingBatchStat {
                request_idx: step.request_idx,
                parent_inode: *child_inode_id,
                next_part_idx: step.next_part_idx + 1,
            });
        }
        pending = next_pending;
    }

    results
        .into_iter()
        .enumerate()
        .map(|(idx, result)| {
            result.unwrap_or_else(|| {
                Err(anyhow!(EmbeddedFsError::internal(&format!(
                    "batch stat missing result for request index {idx}"
                ))))
            })
        })
        .collect()
}

async fn resolve_paths_batched<S>(store: &mut S, paths: &[String]) -> Vec<Result<Inode>>
where
    S: BatchStatStore + Send,
{
    resolve_paths_with_ids_batched(store, paths)
        .await
        .into_iter()
        .map(|result| result.map(|resolved| resolved.inode))
        .collect()
}

fn validate_dir_entry_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(
            "directory entry name must not be empty".to_string(),
        )));
    }
    if matches!(name, "." | "..") {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
            "directory entry name {name:?} is reserved"
        ))));
    }
    if name.contains('/') {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
            "directory entry name must be a single path component: {name}"
        ))));
    }
    Ok(())
}

fn dir_entry_name_from_scan_key<'a>(prefix: &[u8], key: &'a [u8]) -> Option<&'a str> {
    let suffix = key.strip_prefix(prefix)?;
    if suffix.is_empty() || suffix.contains(&b'/') {
        return None;
    }
    let name = std::str::from_utf8(suffix).ok()?;
    if matches!(name, "." | "..") {
        return None;
    }
    Some(name)
}

async fn read_staging_page(
    txn: &mut Transaction,
    inode_id: u64,
    page_num: u64,
) -> Result<Option<Vec<u8>>> {
    let data = txn.get(keys::page_key(inode_id, page_num)).await?;
    Ok(data)
}

async fn read_file_range_from_txn(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &Inode,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let file_size = inode.size;
    if offset >= file_size || length == 0 {
        return Ok(Vec::new());
    }

    let requested_len = u64::try_from(length)
        .map_err(|_| anyhow!(EmbeddedFsError::internal("requested length exceeds u64")))?;
    let actual_len_u64 = requested_len.min(file_size - offset);
    let actual_len = usize::try_from(actual_len_u64).map_err(|_| {
        anyhow!(EmbeddedFsError::internal(
            "requested length exceeds addressable memory"
        ))
    })?;

    let mut data = Vec::with_capacity(actual_len);
    match &inode.data {
        DataRef::None => {
            data.resize(actual_len, 0);
        }
        DataRef::InlineBlob => {
            let blob_data = blob::read_blob(txn, inode_id, inode.size).await?;
            let start = usize::try_from(offset).map_err(|_| {
                anyhow!(EmbeddedFsError::internal(
                    "offset exceeds addressable memory for inline blob"
                ))
            })?;
            let end = start
                .checked_add(actual_len)
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("read range overflow")))?;
            data.extend_from_slice(&blob_data[start..end]);
        }
        DataRef::StagingPages => {
            return Err(anyhow!(EmbeddedFsError::internal(
                "published read reached internal staging pages",
            )))
        }
        other => {
            return Err(anyhow!(EmbeddedFsError::internal(&format!(
                "read_file_range not implemented for data ref {other:?}"
            ))));
        }
    }

    Ok(data)
}

async fn read_staging_file_range_from_txn(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &Inode,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let file_size = inode.size;
    if offset >= file_size || length == 0 {
        return Ok(Vec::new());
    }

    let requested_len = u64::try_from(length)
        .map_err(|_| anyhow!(EmbeddedFsError::internal("requested length exceeds u64")))?;
    let actual_len_u64 = requested_len.min(file_size - offset);
    let actual_len = usize::try_from(actual_len_u64).map_err(|_| {
        anyhow!(EmbeddedFsError::internal(
            "requested length exceeds addressable memory"
        ))
    })?;

    let file_end = offset
        .checked_add(actual_len_u64)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("read range overflow")))?;

    let mut data = Vec::with_capacity(actual_len);
    match &inode.data {
        DataRef::None => data.resize(actual_len, 0),
        DataRef::StagingPages => {
            if let Some((start_page, end_page)) = page_range(offset, actual_len_u64) {
                for page_num in start_page..=end_page {
                    let page_data = match read_staging_page(txn, inode_id, page_num).await? {
                        Some(mut page) => {
                            if page.len() < PAGE_SIZE {
                                page.resize(PAGE_SIZE, 0);
                            }
                            page
                        }
                        None => vec![0u8; PAGE_SIZE],
                    };
                    let (start, end) = page_byte_range(page_num, offset, file_end);
                    data.extend_from_slice(&page_data[start..end]);
                }
            }
        }
        other => {
            return Err(anyhow!(EmbeddedFsError::internal(&format!(
                "staging read not implemented for data ref {other:?}"
            ))));
        }
    }

    Ok(data)
}

async fn retire_inode_data_ref(
    txn: &mut Transaction,
    inode_id: u64,
    data_ref: &DataRef,
    fs_instance_id: [u8; 16],
) -> Result<()> {
    match data_ref {
        DataRef::None => {}
        DataRef::InlineBlob => blob::delete_blob(txn, inode_id).await?,
        DataRef::PackEntry { bundle_id, len, .. } => {
            let _ = retire_bundle_entry(txn, *bundle_id, *len).await?;
        }
        data_ref @ DataRef::Object { .. } => {
            lifecycle::save_lifecycle(
                txn,
                inode_id,
                &FileLifecycle::Deleting {
                    fs_instance_id,
                    data_ref: data_ref.clone(),
                },
            )
            .await?;
        }
        DataRef::StagingPages => {
            return Err(anyhow!(EmbeddedFsError::internal(
                "published file cannot retire staging pages",
            )))
        }
    }
    Ok(())
}

async fn write_staging_chunk_to_txn(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &mut Inode,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }

    let write_len = u64::try_from(data.len())
        .map_err(|_| anyhow!(EmbeddedFsError::internal("write length exceeds u64")))?;
    let write_end = offset
        .checked_add(write_len)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("write range overflow")))?;

    if let Some((start_page, end_page)) = page_range(offset, write_len) {
        let mut data_offset = 0usize;
        for page_num in start_page..=end_page {
            let (page_start, page_end) = page_byte_range(page_num, offset, write_end);
            let chunk_len = page_end - page_start;
            let next_offset = data_offset + chunk_len;
            let chunk = &data[data_offset..next_offset];

            let is_partial = page_start != 0 || page_end != PAGE_SIZE;
            if is_partial {
                let mut page_data = match read_staging_page(txn, inode_id, page_num).await? {
                    Some(mut page) => {
                        if page.len() < PAGE_SIZE {
                            page.resize(PAGE_SIZE, 0);
                        }
                        page
                    }
                    None => vec![0u8; PAGE_SIZE],
                };
                page_data[page_start..page_end].copy_from_slice(chunk);
                write_staging_page(txn, inode_id, page_num, &page_data).await?;
            } else {
                write_staging_page(txn, inode_id, page_num, chunk).await?;
            }

            data_offset = next_offset;
        }
    }

    inode.size = inode.size.max(write_end);
    Ok(())
}

async fn mark_staging_write(txn: &mut Transaction, inode_id: u64, updated_at: i64) -> Result<()> {
    txn.put(
        keys::staging_write_key(inode_id),
        updated_at.to_be_bytes().to_vec(),
    )
    .await?;
    Ok(())
}

async fn clear_staging_write(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::staging_write_key(inode_id)).await?;
    Ok(())
}

async fn mark_orphan_inode(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.put(keys::orphan_inode_key(inode_id), Vec::new())
        .await?;
    Ok(())
}

async fn clear_orphan_inode(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::orphan_inode_key(inode_id)).await?;
    Ok(())
}

async fn list_orphan_inodes(txn: &mut Transaction) -> Result<Vec<u64>> {
    let prefix = keys::orphan_inode_prefix();
    let end = keys::scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, u32::MAX).await?;

    let mut inode_ids = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        if let Some(inode_id) = parse_marked_inode_id(&prefix, &key) {
            inode_ids.push(inode_id);
        }
    }
    Ok(inode_ids)
}

async fn list_stale_staging_writes(txn: &mut Transaction, cutoff: i64) -> Result<Vec<u64>> {
    let prefix = keys::staging_write_prefix();
    let end = keys::scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, u32::MAX).await?;

    let mut inode_ids = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        let value = pair.1;
        let Some(inode_id) = parse_marked_inode_id(&prefix, &key) else {
            continue;
        };
        let Some(updated_at) = parse_staging_write_timestamp(&value) else {
            continue;
        };
        if updated_at <= cutoff {
            inode_ids.push(inode_id);
        }
    }
    Ok(inode_ids)
}

fn parse_marked_inode_id(prefix: &[u8], key: &[u8]) -> Option<u64> {
    let suffix = key.strip_prefix(prefix)?;
    let bytes: [u8; 8] = suffix.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn parse_staging_write_timestamp(value: &[u8]) -> Option<i64> {
    let bytes: [u8; 8] = value.try_into().ok()?;
    Some(i64::from_be_bytes(bytes))
}

fn current_unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn bump_inode_generation(inode: &mut Inode) -> Result<()> {
    inode.generation = inode
        .generation
        .checked_add(1)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("inode generation overflow")))?;
    Ok(())
}

fn is_not_found_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<EmbeddedFsError>(),
        Some(EmbeddedFsError::NotFound(_))
    )
}

async fn write_staging_page(
    txn: &mut Transaction,
    inode_id: u64,
    page_num: u64,
    data: &[u8],
) -> Result<()> {
    let mut page_data = data.to_vec();
    if page_data.len() < PAGE_SIZE {
        page_data.resize(PAGE_SIZE, 0);
    }
    txn.put(keys::page_key(inode_id, page_num), page_data)
        .await?;
    Ok(())
}

async fn delete_staging_pages(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    let prefix = keys::page_prefix(inode_id);
    let end = keys::scan_end_key(&prefix);
    let pairs = txn.scan(prefix..end, u32::MAX).await?;
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        txn.delete(key).await?;
    }
    Ok(())
}

async fn read_staging_pages(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &Inode,
) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    for page_num in 0..pages_needed(inode.size) {
        if let Some(page_data) = read_staging_page(txn, inode_id, page_num).await? {
            data.extend_from_slice(&page_data);
        } else {
            data.extend(std::iter::repeat_n(0u8, PAGE_SIZE));
        }
    }

    let file_len = usize::try_from(inode.size).map_err(|_| {
        anyhow!(EmbeddedFsError::internal(
            "file size exceeds addressable memory"
        ))
    })?;
    if data.len() > file_len {
        data.truncate(file_len);
    }
    Ok(data)
}

async fn resolve_path(txn: &mut Transaction, path: &str) -> Result<(u64, Inode)> {
    let path = normalize_path(path);
    if path == "/" {
        let inode = load_inode(txn, ROOT_INODE)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("root inode missing")))?;
        return Ok((ROOT_INODE, inode));
    }

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current_inode = ROOT_INODE;

    for (i, part) in parts.iter().enumerate() {
        let child_inode = lookup(txn, current_inode, part)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&path)))?;

        if i < parts.len() - 1 {
            let inode = load_inode(txn, child_inode)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&path)))?;
            if !inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(part)));
            }
        }

        current_inode = child_inode;
    }

    let inode = load_inode(txn, current_inode)
        .await?
        .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&path)))?;
    Ok((current_inode, inode))
}

async fn ensure_parents_and_resolve_parent(
    fs: &EmbeddedPageFs,
    txn: &mut Transaction,
    path: &str,
) -> Result<(u64, String)> {
    let normalized = normalize_path(path);
    if normalized == "/" {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "cannot get parent of root".to_string()
        )));
    }

    let (parent, name) = normalized
        .rsplit_once('/')
        .unwrap_or(("", normalized.as_str()));
    let parent_path = if parent.is_empty() { "/" } else { parent };
    if parent_path == "/" {
        return Ok((ROOT_INODE, name.to_string()));
    }

    let parts: Vec<&str> = parent_path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current_inode = ROOT_INODE;

    for part in parts {
        if let Some(next_inode_id) = lookup(txn, current_inode, part).await? {
            let next_inode = load_inode(txn, next_inode_id)
                .await?
                .ok_or_else(|| anyhow!(EmbeddedFsError::internal("dangling dir entry")))?;
            if !next_inode.is_directory() {
                return Err(anyhow!(EmbeddedFsError::not_directory(part)));
            }
            current_inode = next_inode_id;
        } else {
            let new_inode_id = fs.alloc_inode_id().await?;
            let inode = Inode::new_directory(new_inode_id, 0o755);
            save_inode(txn, &inode).await?;
            link(txn, current_inode, part, new_inode_id).await?;
            current_inode = new_inode_id;
        }
    }

    Ok((current_inode, name.to_string()))
}

async fn resolve_parent(txn: &mut Transaction, path: &str) -> Result<(u64, String)> {
    let path = normalize_path(path);
    if path == "/" {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "cannot get parent of root".to_string()
        )));
    }

    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
    let parent_path = if parent.is_empty() { "/" } else { parent };

    let (parent_inode, parent_node) = resolve_path(txn, parent_path).await?;
    if !parent_node.is_directory() {
        return Err(anyhow!(EmbeddedFsError::not_directory(parent_path)));
    }

    Ok((parent_inode, name.to_string()))
}

fn normalize_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

fn encode_s3_key_component(raw: &str) -> String {
    // Encode the UTF-8 bytes directly so distinct keyspaces cannot collide in S3 prefixes.
    hex::encode(raw.as_bytes())
}

fn path_hash_bytes(path: &str) -> [u8; 32] {
    Sha256::digest(path.as_bytes()).into()
}

fn validate_upload_claims(
    claims: &UploadTokenClaims,
    reservation: &UploadReservation,
    upload_id: Option<&str>,
) -> Result<()> {
    if claims.fs_instance_id != reservation.fs_instance_id {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token filesystem instance mismatch".to_string(),
        )));
    }
    if claims.target_path_hash != hex::encode(reservation.path_hash) {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token path hash mismatch".to_string(),
        )));
    }
    if claims.expected_parent_inode != reservation.expected_parent_inode {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token parent expectation mismatch".to_string(),
        )));
    }
    if claims.expected_prior_inode != reservation.expected_prior_inode {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token prior expectation mismatch".to_string(),
        )));
    }
    if claims.expected_prior_generation != reservation.expected_prior_generation {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token prior generation mismatch".to_string(),
        )));
    }
    if claims.nonce != reservation.nonce {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token nonce mismatch".to_string(),
        )));
    }
    if claims.expires_at != reservation.expires_at {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token expiry mismatch".to_string(),
        )));
    }
    if let Some(upload_id) = upload_id {
        if claims.upload_id != upload_id {
            return Err(anyhow!(EmbeddedFsError::PermissionDenied(
                "upload token upload_id mismatch".to_string(),
            )));
        }
    }
    Ok(())
}

fn uploading_lifecycle_is_reapable(
    now: i64,
    updated_at: i64,
    reservation: Option<&UploadReservation>,
) -> bool {
    if now.saturating_sub(updated_at) < STALE_WRITE_STREAM_SECS {
        return false;
    }

    match reservation {
        Some(reservation) => now > reservation.expires_at,
        None => true,
    }
}

async fn resolve_existing_parent(txn: &mut Transaction, path: &str) -> Result<Option<u64>> {
    let normalized = normalize_path(path);
    if normalized == "/" {
        return Ok(Some(ROOT_INODE));
    }

    let parent_path = normalized
        .rsplit_once('/')
        .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
        .unwrap_or("/");
    if parent_path == "/" {
        return Ok(Some(ROOT_INODE));
    }

    let mut current_inode = ROOT_INODE;
    for part in parent_path.trim_start_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        let Some(next_inode_id) = lookup(txn, current_inode, part).await? else {
            return Ok(None);
        };
        let inode = load_inode(txn, next_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("dangling dir entry")))?;
        if !inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::not_directory(part)));
        }
        current_inode = next_inode_id;
    }

    Ok(Some(current_inode))
}

async fn verify_upload_publish_preconditions(
    txn: &mut Transaction,
    path: &str,
    reservation: &UploadReservation,
) -> Result<()> {
    let normalized = normalize_path(path);
    if normalized != reservation.path {
        return Err(anyhow!(EmbeddedFsError::conflict(
            "upload reservation path no longer matches the publish target",
        )));
    }

    let current_parent_inode = resolve_existing_parent(txn, &normalized).await?;
    if let Some(expected_parent_inode) = reservation.expected_parent_inode {
        if current_parent_inode != Some(expected_parent_inode) {
            return Err(anyhow!(EmbeddedFsError::conflict(
                "upload reservation no longer matches the current parent namespace",
            )));
        }
    }

    let (current_target_inode, current_target_generation) =
        match resolve_path(txn, &normalized).await {
            Ok((inode_id, inode)) => {
                if inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(&normalized)));
                }
                (Some(inode_id), Some(inode.generation))
            }
            Err(err) if is_not_found_error(&err) => (None, None),
            Err(err) => return Err(err),
        };

    if current_target_inode != reservation.expected_prior_inode {
        return Err(anyhow!(EmbeddedFsError::conflict(
            "upload reservation no longer matches the current file generation",
        )));
    }
    if let Some(expected_generation) = reservation.expected_prior_generation {
        if current_target_generation != Some(expected_generation) {
            return Err(anyhow!(EmbeddedFsError::conflict(
                "upload reservation no longer matches the current file generation",
            )));
        }
    }

    Ok(())
}

fn normalize_completed_parts(
    mut parts: Vec<FsMultipartCompletedPart>,
) -> Result<Vec<FsMultipartCompletedPart>> {
    if parts.is_empty() {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(
            "complete_upload requires at least one uploaded part".to_string(),
        )));
    }

    parts.sort_by_key(|part| part.part_number);
    let mut last_part_number = 0;
    for part in &parts {
        if !(1..=10_000).contains(&part.part_number) {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "invalid multipart part number: {}",
                part.part_number
            ))));
        }
        if part.part_number == last_part_number {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "duplicate multipart part number: {}",
                part.part_number
            ))));
        }
        if part.etag.trim().is_empty() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                "missing etag for multipart part {}",
                part.part_number
            ))));
        }
        last_part_number = part.part_number;
    }
    Ok(parts)
}

async fn head_object_with_retry(
    s3: &FsS3Client,
    key: &str,
) -> Result<crate::extensions::fs::s3::FsS3HeadObject> {
    let attempts = fs9_config()
        .s3
        .as_ref()
        .map(|cfg| cfg.head_retry_attempts)
        .unwrap_or(1)
        .max(1);
    let base_ms = fs9_config()
        .s3
        .as_ref()
        .map(|cfg| cfg.head_retry_base_ms)
        .unwrap_or(50)
        .max(1);

    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..attempts {
        match s3.head_object(key).await {
            Ok(head) => return Ok(head),
            Err(err) if attempt + 1 < attempts => {
                last_error = Some(err);
                let factor = 1u64 << attempt.min(10);
                sleep(std::time::Duration::from_millis(
                    base_ms.saturating_mul(factor),
                ))
                .await;
            }
            Err(err) => return Err(err),
        }
    }

    if let Some(err) = last_error {
        Err(anyhow!(EmbeddedFsError::internal(&format!(
            "head verification retry exhausted: {err}"
        ))))
    } else {
        Err(anyhow!(EmbeddedFsError::internal(
            "head verification retry exhausted",
        )))
    }
}

fn max_presign_part_number(expected_size: u64) -> Result<i32> {
    // Guardrail against presigned URL farming: derive an upload-specific upper bound from
    // the reserved expected size and the configured multipart part size.
    let global_max = fs9_config().max_parts_per_upload.clamp(1, 10_000);
    let part_bytes = fs9_config()
        .s3
        .as_ref()
        .map(|cfg| cfg.multipart_part_bytes)
        .unwrap_or(WRITE_STREAM_FLUSH_BYTES);
    if part_bytes == 0 {
        return Err(anyhow!(EmbeddedFsError::internal(
            "FS9_S3_MULTIPART_PART must be > 0"
        )));
    }

    let part_bytes = u64::try_from(part_bytes)
        .map_err(|_| anyhow!(EmbeddedFsError::internal("multipart part size exceeds u64")))?;
    let expected_parts = expected_size.div_ceil(part_bytes).max(1);

    let max_parts = expected_parts.saturating_mul(2).min(u64::from(global_max));
    i32::try_from(max_parts).map_err(|_| {
        anyhow!(EmbeddedFsError::internal(
            "multipart part limit exceeds i32"
        ))
    })
}

fn presign_ttl_secs_from_claims(expires_at: i64) -> Result<u64> {
    let now = current_unix_timestamp();
    if now > expires_at {
        return Err(anyhow!(EmbeddedFsError::PermissionDenied(
            "upload token has expired".to_string(),
        )));
    }
    let remaining = expires_at
        .checked_sub(now)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("presign ttl underflow")))?;
    u64::try_from(remaining.max(1))
        .map_err(|_| anyhow!(EmbeddedFsError::internal("presign ttl exceeds u64")))
}

fn should_use_direct_object_stream(expected_size: Option<u64>, has_object_storage: bool) -> bool {
    let Some(expected_size) = expected_size else {
        return false;
    };
    let Ok(expected_size) = usize::try_from(expected_size) else {
        return false;
    };
    has_object_storage && expected_size >= fs9_config().object_min_bytes
}

fn should_route_stream_spool_to_object(actual_size: u64) -> bool {
    actual_size > 0 && !should_route_stream_spool_to_inline(actual_size)
}

fn should_route_stream_spool_to_inline(actual_size: u64) -> bool {
    can_store_inline_u64(actual_size)
}

fn can_store_inline_len(len: usize) -> bool {
    fs9_config().inline_max_bytes > 0 && len <= fs9_config().inline_max_bytes
}

fn can_store_inline_u64(size: u64) -> bool {
    usize::try_from(size).ok().is_some_and(can_store_inline_len)
}

async fn apply_inline_write_at(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &mut Inode,
    offset: u64,
    data: &[u8],
    op: &str,
    sealed_error: &str,
    size_error_prefix: &str,
) -> Result<()> {
    if matches!(
        inode.data,
        DataRef::Object { .. } | DataRef::PackEntry { .. }
    ) {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(
            sealed_error.to_string(),
        )));
    }

    let write_len = u64::try_from(data.len())
        .map_err(|_| anyhow!(EmbeddedFsError::internal("write length exceeds u64")))?;
    let write_end = offset
        .checked_add(write_len)
        .ok_or_else(|| anyhow!(EmbeddedFsError::internal("write range overflow")))?;
    let new_size = inode.size.max(write_end);
    if !can_store_inline_u64(new_size) {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
            "{size_error_prefix} {} bytes",
            fs9_config().inline_max_bytes
        ))));
    }

    let mut buf = load_inline_file_buffer(txn, inode_id, inode, op).await?;
    blob::apply_write_at(&mut buf, offset, data)?;
    blob::apply_truncate(&mut buf, new_size)?;
    blob::write_blob(txn, inode_id, &buf).await?;
    inode.data = DataRef::InlineBlob;
    inode.size = new_size;
    bump_inode_generation(inode)?;
    inode.touch_mtime();
    save_inode(txn, inode).await?;
    Ok(())
}

async fn load_inline_file_buffer(
    txn: &mut Transaction,
    inode_id: u64,
    inode: &Inode,
    op: &str,
) -> Result<Vec<u8>> {
    match &inode.data {
        DataRef::None => Ok(Vec::new()),
        DataRef::InlineBlob => {
            let mut buf = txn.get(keys::blob_key(inode_id)).await?.unwrap_or_default();
            blob::apply_truncate(&mut buf, inode.size)?;
            Ok(buf)
        }
        DataRef::StagingPages => Err(anyhow!(EmbeddedFsError::internal(&format!(
            "{op} reached internal staging pages on a published inode"
        )))),
        other => Err(anyhow!(EmbeddedFsError::internal(&format!(
            "{op} not implemented for data ref {other:?}"
        )))),
    }
}

fn is_retryable_tikv_write_conflict(err: &anyhow::Error) -> bool {
    fn contains_write_conflict(err: &tikv_client::Error) -> bool {
        match err {
            tikv_client::Error::PessimisticLockError { inner, .. } => {
                contains_write_conflict(inner)
            }
            tikv_client::Error::UndeterminedError(inner) => contains_write_conflict(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                errors.iter().any(contains_write_conflict)
            }
            tikv_client::Error::KeyError(key_error) => key_error.conflict.is_some(),
            _ => false,
        }
    }

    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(contains_write_conflict)
    })
}

async fn fs9_commit_backoff(attempt: u32) {
    let base_ms = fs9_config().tikv_commit_retry_base_ms.max(1);
    let factor = 1u64 << attempt.min(10);
    sleep(std::time::Duration::from_millis(
        base_ms.saturating_mul(factor),
    ))
    .await;
}

fn pages_needed(size: u64) -> u64 {
    if size == 0 {
        0
    } else {
        size.div_ceil(PAGE_SIZE as u64)
    }
}

/// Compute the page range [start_page, end_page] for a byte range [offset, offset+length).
/// Returns None if length is 0.
#[allow(dead_code)]
fn page_range(offset: u64, length: u64) -> Option<(u64, u64)> {
    if length == 0 {
        return None;
    }
    let start = offset / PAGE_SIZE as u64;
    let end_byte = offset.saturating_add(length - 1);
    let end = end_byte / PAGE_SIZE as u64;
    Some((start, end))
}

/// Compute the byte range within a page that a [file_offset, file_offset+length) range touches.
/// Returns (start_in_page, end_in_page) where the range is [start_in_page, end_in_page).
#[allow(dead_code)]
fn page_byte_range(page_num: u64, file_offset: u64, file_end: u64) -> (usize, usize) {
    let page_start = page_num * PAGE_SIZE as u64;
    let page_end = page_start.saturating_add(PAGE_SIZE as u64);
    let start = if file_offset > page_start {
        (file_offset - page_start) as usize
    } else {
        0
    };
    let end = if file_end < page_end {
        (file_end - page_start) as usize
    } else {
        PAGE_SIZE
    };
    (start, end)
}

async fn remove_inode_recursive(
    txn: &mut Transaction,
    inode_id: u64,
    inode: Inode,
    fs_instance_id: [u8; 16],
) -> Result<u64> {
    if !inode.is_directory() {
        retire_inode_data_ref(txn, inode_id, &inode.data, fs_instance_id).await?;
        delete_inode(txn, inode_id).await?;
        return Ok(1);
    }

    let entries = list_dir(txn, inode_id).await?;
    let mut removed = 1;
    for (name, child_id) in entries {
        let child_inode = load_inode(txn, child_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::internal("dangling directory entry")))?;
        removed += Box::pin(remove_inode_recursive(
            txn,
            child_id,
            child_inode,
            fs_instance_id,
        ))
        .await?;
        unlink(txn, inode_id, &name).await?;
    }
    delete_inode(txn, inode_id).await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::AsyncReadExt;

    static TEST_KEYSPACE_SEQ: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct FakeBatchStatStore {
        dir_entries: HashMap<(u64, String), u64>,
        inodes: HashMap<u64, Inode>,
        lookup_batches: Vec<Vec<(u64, String)>>,
        inode_load_batches: Vec<Vec<u64>>,
        lookup_error: Option<anyhow::Error>,
        inode_errors: HashMap<u64, anyhow::Error>,
    }

    impl FakeBatchStatStore {
        fn new() -> Self {
            let mut store = Self::default();
            store
                .inodes
                .insert(ROOT_INODE, Inode::new_directory(ROOT_INODE, 0o755));
            store
        }

        fn link_inode(&mut self, parent_inode: u64, name: &str, inode: Inode) {
            self.dir_entries
                .insert((parent_inode, name.to_string()), inode.id);
            self.inodes.insert(inode.id, inode);
        }
    }

    #[async_trait]
    impl BatchStatStore for FakeBatchStatStore {
        async fn load_root_inode(&mut self) -> Result<Option<Inode>> {
            Ok(self.inodes.get(&ROOT_INODE).cloned())
        }

        async fn lookup_dir_entries(
            &mut self,
            requests: &[DirLookupRequest],
        ) -> Result<HashMap<(u64, String), Option<u64>>> {
            if let Some(err) = self.lookup_error.take() {
                return Err(err);
            }
            self.lookup_batches.push(
                requests
                    .iter()
                    .map(|request| (request.parent_inode, request.name.clone()))
                    .collect(),
            );

            let mut out = HashMap::with_capacity(requests.len());
            for request in requests {
                out.insert(
                    (request.parent_inode, request.name.clone()),
                    self.dir_entries
                        .get(&(request.parent_inode, request.name.clone()))
                        .copied(),
                );
            }
            Ok(out)
        }

        async fn load_inodes(
            &mut self,
            inode_ids: &[u64],
        ) -> Result<HashMap<u64, Result<Option<Inode>>>> {
            self.inode_load_batches.push(inode_ids.to_vec());
            Ok(inode_ids
                .iter()
                .copied()
                .map(|inode_id| {
                    let inode = self
                        .inode_errors
                        .remove(&inode_id)
                        .map(Err)
                        .unwrap_or_else(|| Ok(self.inodes.get(&inode_id).cloned()));
                    (inode_id, inode)
                })
                .collect())
        }
    }

    fn assert_not_found(result: &Result<Inode>, path: &str) {
        let err = result.as_ref().expect_err("path must fail");
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("error must be EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(actual) if actual == path),
            "unexpected error: {err}"
        );
    }

    fn assert_not_directory(result: &Result<Inode>, part: &str) {
        let err = result.as_ref().expect_err("path must fail");
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("error must be EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(actual) if actual == part),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_resolve_paths_batched_shares_sibling_parent_traversal() {
        let mut store = FakeBatchStatStore::new();
        store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
        store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));
        store.link_inode(2, "beta.txt", Inode::new_file(4, 0o644));
        store.link_inode(2, "gamma.txt", Inode::new_file(5, 0o644));

        let paths = vec![
            "/data/alpha.txt".to_string(),
            "/data/beta.txt".to_string(),
            "/data/gamma.txt".to_string(),
        ];
        let results = resolve_paths_batched(&mut store, &paths).await;

        assert_eq!(results.len(), paths.len());
        assert!(results.iter().all(|result| result.is_ok()));
        assert_eq!(store.lookup_batches.len(), 2);
        assert_eq!(
            store.lookup_batches[0],
            vec![(ROOT_INODE, "data".to_string())]
        );
        assert_eq!(
            store.lookup_batches[1],
            vec![
                (2, "alpha.txt".to_string()),
                (2, "beta.txt".to_string()),
                (2, "gamma.txt".to_string()),
            ]
        );
        assert_eq!(store.inode_load_batches, vec![vec![2], vec![3, 4, 5]]);
    }

    #[tokio::test]
    async fn test_resolve_paths_with_ids_batched_preserves_inode_ids() {
        let mut store = FakeBatchStatStore::new();
        store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
        store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));

        let results = resolve_paths_with_ids_batched(
            &mut store,
            &["/".to_string(), "/data/alpha.txt".to_string()],
        )
        .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().inode_id, ROOT_INODE);
        assert_eq!(results[1].as_ref().unwrap().inode_id, 3);
    }

    #[tokio::test]
    async fn test_resolve_paths_batched_preserves_mixed_result_semantics() {
        let mut store = FakeBatchStatStore::new();
        store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
        store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));
        store.link_inode(ROOT_INODE, "note.txt", Inode::new_file(4, 0o644));

        let paths = vec![
            "/data".to_string(),
            "/data/alpha.txt".to_string(),
            "/missing".to_string(),
            "/note.txt/child".to_string(),
        ];
        let results = resolve_paths_batched(&mut store, &paths).await;

        assert!(results[0].is_ok(), "directory stat must succeed");
        assert!(results[1].is_ok(), "file stat must succeed");
        assert_not_found(&results[2], "/missing");
        assert_not_directory(&results[3], "note.txt");
        assert_eq!(store.lookup_batches.len(), 2);
        assert_eq!(
            store.lookup_batches[0],
            vec![
                (ROOT_INODE, "data".to_string()),
                (ROOT_INODE, "missing".to_string()),
                (ROOT_INODE, "note.txt".to_string()),
            ]
        );
        assert_eq!(store.lookup_batches[1], vec![(2, "alpha.txt".to_string())]);
    }

    #[tokio::test]
    async fn test_resolve_paths_batched_collapses_deep_shared_prefixes() {
        let mut store = FakeBatchStatStore::new();
        store.link_inode(ROOT_INODE, "a", Inode::new_directory(2, 0o755));
        store.link_inode(2, "b", Inode::new_directory(3, 0o755));
        store.link_inode(3, "c", Inode::new_directory(4, 0o755));
        store.link_inode(3, "d", Inode::new_directory(5, 0o755));
        store.link_inode(4, "file1.txt", Inode::new_file(6, 0o644));
        store.link_inode(4, "file2.txt", Inode::new_file(7, 0o644));
        store.link_inode(5, "file3.txt", Inode::new_file(8, 0o644));

        let paths = vec![
            "/a/b/c/file1.txt".to_string(),
            "/a/b/c/file2.txt".to_string(),
            "/a/b/d/file3.txt".to_string(),
        ];
        let results = resolve_paths_batched(&mut store, &paths).await;

        assert!(results.iter().all(|result| result.is_ok()));
        assert_eq!(store.lookup_batches.len(), 4);
        assert_eq!(store.lookup_batches[0], vec![(ROOT_INODE, "a".to_string())]);
        assert_eq!(store.lookup_batches[1], vec![(2, "b".to_string())]);
        assert_eq!(
            store.lookup_batches[2],
            vec![(3, "c".to_string()), (3, "d".to_string())]
        );
        assert_eq!(
            store.lookup_batches[3],
            vec![
                (4, "file1.txt".to_string()),
                (4, "file2.txt".to_string()),
                (5, "file3.txt".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_resolve_paths_batched_isolates_single_inode_decode_error() {
        let mut store = FakeBatchStatStore::new();
        store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
        store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));
        store.link_inode(2, "broken.txt", Inode::new_file(4, 0o644));
        store.inode_errors.insert(
            4,
            anyhow!("fs9: invalid inode json for inode 4: boom. Recreate the fs9 keyspace."),
        );

        let paths = vec![
            "/data/alpha.txt".to_string(),
            "/data/broken.txt".to_string(),
        ];
        let results = resolve_paths_batched(&mut store, &paths).await;

        assert!(results[0].is_ok(), "healthy sibling must still succeed");
        let err = results[1].as_ref().expect_err("broken inode must fail");
        assert!(
            err.to_string().contains("invalid inode json for inode 4"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_resolve_paths_batched_preserves_resolved_entries_on_shared_lookup_failure() {
        let mut store = FakeBatchStatStore::new();
        store.lookup_error = Some(anyhow!("tikv read failed"));

        let paths = vec!["/".to_string(), "/data/alpha.txt".to_string()];
        let results = resolve_paths_batched(&mut store, &paths).await;

        assert!(
            results[0].is_ok(),
            "already-resolved root entry must stay successful"
        );
        let err = results[1]
            .as_ref()
            .expect_err("pending entry must be converted to per-entry failure");
        assert!(
            err.to_string().contains("tikv read failed"),
            "unexpected error: {err}"
        );
    }

    fn pending_pack(
        result_idx: usize,
        bundle_id: u64,
        bundle_offset: u64,
        len: usize,
    ) -> PendingBatchInlineReadPack {
        PendingBatchInlineReadPack {
            result_idx,
            bundle_id,
            bundle_offset,
            len,
        }
    }

    fn planned_pack_window(
        bundle_id: u64,
        start_offset: u64,
        end_offset: u64,
        entries: &[(usize, u64, usize)],
    ) -> PlannedBatchInlineReadPackWindow {
        PlannedBatchInlineReadPackWindow {
            bundle_id,
            start_offset,
            end_offset,
            entries: entries
                .iter()
                .map(
                    |(result_idx, bundle_offset, len)| PlannedBatchInlineReadPackWindowEntry {
                        result_idx: *result_idx,
                        bundle_offset: *bundle_offset,
                        len: *len,
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn test_plan_batch_inline_read_pack_windows_merges_small_gaps_within_bundle() {
        let windows = plan_batch_inline_read_pack_windows(
            vec![
                pending_pack(0, 7, 0, 8),
                pending_pack(1, 7, 10, 4),
                pending_pack(2, 7, 20, 4),
            ],
            64,
        )
        .unwrap();

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].bundle_id, 7);
        assert_eq!(windows[0].start_offset, 0);
        assert_eq!(windows[0].end_offset, 24);
        assert_eq!(windows[0].entries.len(), 3);
        assert_eq!(windows[0].entries[0].result_idx, 0);
        assert_eq!(windows[0].entries[1].result_idx, 1);
        assert_eq!(windows[0].entries[2].result_idx, 2);
    }

    #[test]
    fn test_plan_batch_inline_read_pack_windows_does_not_merge_across_bundle_or_large_gap() {
        let windows = plan_batch_inline_read_pack_windows(
            vec![
                pending_pack(0, 7, 0, 8),
                pending_pack(1, 7, PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES + 9, 4),
                pending_pack(2, 8, 0, 4),
            ],
            128,
        )
        .unwrap();

        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].bundle_id, 7);
        assert_eq!(windows[0].start_offset, 0);
        assert_eq!(windows[0].end_offset, 8);
        assert_eq!(windows[1].bundle_id, 7);
        assert_eq!(
            windows[1].start_offset,
            PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES + 9
        );
        assert_eq!(
            windows[1].end_offset,
            PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES + 13
        );
        assert_eq!(windows[2].bundle_id, 8);
        assert_eq!(windows[2].start_offset, 0);
        assert_eq!(windows[2].end_offset, 4);
    }

    #[test]
    fn test_plan_batch_inline_read_pack_windows_respects_window_cap() {
        let windows = plan_batch_inline_read_pack_windows(
            vec![pending_pack(0, 7, 0, 8), pending_pack(1, 7, 8, 8)],
            12,
        )
        .unwrap();

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].start_offset, 0);
        assert_eq!(windows[0].end_offset, 8);
        assert_eq!(windows[1].start_offset, 8);
        assert_eq!(windows[1].end_offset, 16);
    }

    #[test]
    fn test_split_pack_window_bytes_returns_expected_entry_payloads() {
        let window = planned_pack_window(7, 10, 20, &[(0, 10, 4), (1, 16, 4)]);
        let entries =
            EmbeddedPageFs::split_pack_window_bytes(&window, Bytes::from_static(b"abcdefghij"))
                .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, 0);
        assert_eq!(entries[0].1, 10);
        assert_eq!(entries[0].2, b"abcd".to_vec());
        assert_eq!(entries[1].0, 1);
        assert_eq!(entries[1].1, 16);
        assert_eq!(entries[1].2, b"ghij".to_vec());
    }

    #[test]
    fn test_split_pack_window_bytes_rejects_short_window_payload() {
        let window = planned_pack_window(7, 10, 20, &[(0, 10, 4), (1, 16, 4)]);
        let err = EmbeddedPageFs::split_pack_window_bytes(&window, Bytes::from_static(b"abcdefg"))
            .expect_err("short window payload must fail");
        assert!(
            err.to_string()
                .contains("pack read window returned an unexpected byte layout"),
            "unexpected error: {err}"
        );
    }

    // normalize_path tests
    #[test]
    fn test_normalize_path_root() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn test_normalize_path_empty() {
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn test_normalize_path_trailing_slash() {
        assert_eq!(normalize_path("/foo/bar/"), "/foo/bar");
    }

    #[test]
    fn test_normalize_path_no_trailing_slash() {
        assert_eq!(normalize_path("/foo/bar"), "/foo/bar");
    }

    #[test]
    fn test_normalize_path_root_trailing_slash() {
        assert_eq!(normalize_path("/"), "/");
    }

    // pages_needed tests
    #[test]
    fn test_pages_needed_zero() {
        assert_eq!(pages_needed(0), 0);
    }

    #[test]
    fn test_pages_needed_one_byte() {
        assert_eq!(pages_needed(1), 1);
    }

    #[test]
    fn test_pages_needed_exact_page() {
        assert_eq!(pages_needed(16384), 1); // PAGE_SIZE = 16 * 1024
    }

    #[test]
    fn test_pages_needed_one_over() {
        assert_eq!(pages_needed(16385), 2);
    }

    #[test]
    fn test_pages_needed_two_pages() {
        assert_eq!(pages_needed(32768), 2);
    }

    #[test]
    fn test_pages_needed_large() {
        assert_eq!(pages_needed(1_000_000), 62); // ceil(1000000 / 16384)
    }

    #[test]
    fn test_pages_needed_last_byte_of_page() {
        assert_eq!(pages_needed((PAGE_SIZE - 1) as u64), 1);
    }

    #[test]
    fn test_pages_needed_three_pages_minus_one() {
        assert_eq!(pages_needed((PAGE_SIZE as u64 * 3) - 1), 3);
    }

    #[test]
    fn test_pages_needed_three_pages_plus_one() {
        assert_eq!(pages_needed((PAGE_SIZE as u64 * 3) + 1), 4);
    }

    #[test]
    fn test_pages_needed_u64_max() {
        assert_eq!(pages_needed(u64::MAX), u64::MAX.div_ceil(PAGE_SIZE as u64));
    }

    #[test]
    fn test_page_range_zero_length() {
        assert_eq!(page_range(0, 0), None);
    }

    #[test]
    fn test_read_at_page_range_single_page() {
        assert_eq!(page_range(123, 456), Some((0, 0)));
    }

    #[test]
    fn test_read_at_page_range_cross_boundary() {
        assert_eq!(page_range((PAGE_SIZE - 2) as u64, 4), Some((0, 1)));
    }

    #[test]
    fn test_read_at_page_range_exact_page() {
        assert_eq!(page_range(PAGE_SIZE as u64, PAGE_SIZE as u64), Some((1, 1)));
    }

    #[test]
    fn test_write_at_page_range_partial_first() {
        assert_eq!(
            page_range((PAGE_SIZE / 2) as u64, PAGE_SIZE as u64),
            Some((0, 1))
        );
    }

    #[test]
    fn test_write_at_page_range_full_pages() {
        assert_eq!(
            page_range(PAGE_SIZE as u64, (PAGE_SIZE * 2) as u64),
            Some((1, 2))
        );
    }

    #[test]
    fn test_page_range_single_byte_page_start() {
        assert_eq!(page_range((PAGE_SIZE * 3) as u64, 1), Some((3, 3)));
    }

    #[test]
    fn test_page_range_single_byte_page_end() {
        assert_eq!(page_range((PAGE_SIZE - 1) as u64, 1), Some((0, 0)));
    }

    #[test]
    fn test_page_range_three_pages_plus_tail() {
        assert_eq!(page_range(10, (PAGE_SIZE as u64 * 3) + 5), Some((0, 3)));
    }

    #[test]
    fn test_page_range_large_offset() {
        let base = 1_000_000u64 * PAGE_SIZE as u64;
        assert_eq!(
            page_range(base + 7, (PAGE_SIZE as u64 * 2) + 1),
            Some((1_000_000, 1_000_002))
        );
    }

    #[test]
    fn test_page_range_u64_max_single_byte() {
        assert_eq!(
            page_range(u64::MAX, 1),
            Some((u64::MAX / PAGE_SIZE as u64, u64::MAX / PAGE_SIZE as u64))
        );
    }

    #[test]
    fn test_page_byte_range_single_byte_at_page_start() {
        assert_eq!(
            page_byte_range(2, (PAGE_SIZE as u64) * 2, (PAGE_SIZE as u64) * 2 + 1),
            (0, 1)
        );
    }

    #[test]
    fn test_page_byte_range_single_byte_in_middle() {
        let start = (PAGE_SIZE as u64) * 4 + 1234;
        assert_eq!(page_byte_range(4, start, start + 1), (1234, 1235));
    }

    #[test]
    fn test_page_byte_range_single_byte_at_page_end() {
        let end = (PAGE_SIZE as u64) * 5;
        assert_eq!(page_byte_range(4, end - 1, end), (PAGE_SIZE - 1, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_exact_full_page() {
        let start = PAGE_SIZE as u64;
        let end = start + PAGE_SIZE as u64;
        assert_eq!(page_byte_range(1, start, end), (0, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_first_page_of_cross_boundary() {
        let start = (PAGE_SIZE - 10) as u64;
        let end = start + 100;
        assert_eq!(page_byte_range(0, start, end), (PAGE_SIZE - 10, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_second_page_of_cross_boundary() {
        let start = (PAGE_SIZE - 10) as u64;
        let end = start + 100;
        assert_eq!(page_byte_range(1, start, end), (0, 90));
    }

    #[test]
    fn test_page_byte_range_middle_page_three_pages() {
        let start = (PAGE_SIZE as u64) * 2 + 50;
        let end = start + (PAGE_SIZE as u64 * 3) + 10;
        assert_eq!(page_byte_range(4, start, end), (0, PAGE_SIZE));
    }

    #[test]
    fn test_page_byte_range_last_page_three_pages() {
        let start = (PAGE_SIZE as u64) * 2 + 50;
        let end = start + (PAGE_SIZE as u64 * 3) + 10;
        assert_eq!(page_byte_range(5, start, end), (0, 60));
    }

    #[test]
    fn test_page_byte_range_large_offset() {
        let base = (PAGE_SIZE as u64) * 2_000_000;
        assert_eq!(page_byte_range(2_000_000, base + 7, base + 20), (7, 20));
    }

    #[test]
    fn test_page_byte_range_u64_max_page() {
        let last_page = u64::MAX / PAGE_SIZE as u64;
        let page_start = last_page * PAGE_SIZE as u64;
        let file_end = page_start + 17;
        assert_eq!(
            page_byte_range(last_page, page_start + 5, file_end),
            (5, 17)
        );
    }

    #[test]
    fn test_pages_needed() {
        assert_eq!(pages_needed(0), 0);
        assert_eq!(pages_needed(1), 1);
        assert_eq!(pages_needed((PAGE_SIZE as u64) - 1), 1);
        assert_eq!(pages_needed(PAGE_SIZE as u64), 1);
        assert_eq!(pages_needed((PAGE_SIZE as u64) + 1), 2);
    }

    #[test]
    fn test_validate_superblock_format_accepts_current_layout() {
        let superblock = Superblock::new([1u8; 16], None);
        validate_superblock_format(&superblock).expect("current superblock must validate");
    }

    #[test]
    fn test_validate_superblock_format_rejects_legacy_layout() {
        let superblock_json = r#"{
            "next_inode": 2,
            "next_bundle": 1
        }"#;
        let superblock = parse_superblock_bytes(superblock_json.as_bytes())
            .expect_err("legacy superblock layout must be rejected");
        assert!(
            superblock
                .to_string()
                .contains("unsupported storage format version"),
            "unexpected error: {superblock}"
        );
    }

    #[test]
    fn test_validate_superblock_format_rejects_missing_instance_id() {
        let err = validate_superblock_format(&Superblock::default())
            .expect_err("zero fs_instance_id must be rejected");
        assert!(
            err.to_string().contains("missing filesystem instance id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_superblock_format_rejects_previous_v3_revision() {
        let superblock = Superblock {
            format_version: 3,
            fs_instance_id: [7u8; 16],
            object_store: None,
        };
        let err = validate_superblock_format(&superblock)
            .expect_err("previous prototype revision must be rejected");
        assert!(
            err.to_string()
                .contains("unsupported storage format version 3"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_superblock_binding_rejects_binding_mismatch() {
        let persisted = Some(ObjectStoreBinding {
            bucket: "bucket-a".to_string(),
            region: Some("us-east-1".to_string()),
            endpoint: Some("https://s3-a.example.com".to_string()),
            prefix: "tenant-a".to_string(),
            force_path_style: false,
        });
        let current = Some(ObjectStoreBinding {
            bucket: "bucket-b".to_string(),
            region: Some("us-east-1".to_string()),
            endpoint: Some("https://s3-a.example.com".to_string()),
            prefix: "tenant-a".to_string(),
            force_path_style: false,
        });
        let err = validate_superblock_binding(
            &Superblock::new([7u8; 16], persisted),
            &current,
            "tenant_a",
        )
        .expect_err("binding mismatch must be rejected");
        assert!(
            err.to_string().contains("object storage binding mismatch"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("tenant_a"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_superblock_binding_accepts_exact_match() {
        let binding = Some(ObjectStoreBinding {
            bucket: "bucket-a".to_string(),
            region: Some("us-east-1".to_string()),
            endpoint: Some("https://s3-a.example.com".to_string()),
            prefix: "tenant-a".to_string(),
            force_path_style: true,
        });
        validate_superblock_binding(
            &Superblock::new([9u8; 16], binding.clone()),
            &binding,
            "tenant_a",
        )
        .expect("matching binding must validate");
    }

    #[test]
    fn test_object_and_bundle_keys_include_fs_instance_namespace() {
        let state = FsRuntimeState {
            identity: FsInstanceIdentity::new("tenant-a".to_string(), [0xabu8; 16]),
            fs_instance_id: [0xabu8; 16],
            fs_instance_id_hex: hex::encode([0xabu8; 16]),
            object_store: Some(ObjectStoreBinding {
                bucket: "bucket-a".to_string(),
                region: Some("us-east-1".to_string()),
                endpoint: None,
                prefix: "fs9-prefix".to_string(),
                force_path_style: false,
            }),
        };

        let object_key = build_object_key("tenant-a", &state, 42).expect("object key");
        let encoded_keyspace = encode_s3_key_component("tenant-a");
        assert!(
            object_key.contains("/abababababababababababababababab/objects/"),
            "unexpected object key: {object_key}"
        );
        assert!(object_key.starts_with(&format!("fs9-prefix/{encoded_keyspace}/")));
        assert!(
            object_key.ends_with("/42"),
            "unexpected object key: {object_key}"
        );

        let bundle_key = build_bundle_key("tenant-a", &state, 9).expect("bundle key");
        assert_eq!(
            bundle_key,
            format!("fs9-prefix/{encoded_keyspace}/abababababababababababababababab/packs/9.pack")
        );
    }

    #[test]
    fn test_pack_spool_root_is_scoped_by_storage_format_and_fs_instance() {
        let identity = FsInstanceIdentity::new("tenant-a".to_string(), [0x11u8; 16]);
        let root = pack_spool_root(&identity, &hex::encode(identity.fs_instance_id));
        let rendered = root.display().to_string();
        assert!(
            rendered.ends_with(&format!(
                "{}/format-{}/{}",
                encode_s3_key_component("tenant-a"),
                FS9_STORAGE_FORMAT_VERSION,
                hex::encode([0x11u8; 16])
            )),
            "unexpected spool root: {rendered}"
        );
    }

    #[test]
    fn test_register_process_identity_rejects_instance_change() {
        let keyspace = format!(
            "fs9_process_identity_test_{}_{}",
            std::process::id(),
            rand::random::<u64>()
        );
        let first = FsInstanceIdentity::new(keyspace.clone(), [1u8; 16]);
        let second = FsInstanceIdentity::new(keyspace.clone(), [2u8; 16]);

        register_process_identity(&first).expect("first identity must register");
        register_process_identity(&first).expect("same identity must re-register");

        let err = register_process_identity(&second)
            .expect_err("different instance for same keyspace must fail");
        assert!(
            err.to_string().contains("restart db9-server"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_maintenance_probe_action_is_fail_closed_on_errors() {
        assert_eq!(
            maintenance_probe_action(&Ok::<bool, ()>(true)),
            MaintenanceProbeAction::Run
        );
        assert_eq!(
            maintenance_probe_action(&Ok::<bool, ()>(false)),
            MaintenanceProbeAction::Stop
        );
        assert_eq!(
            maintenance_probe_action(&Err::<bool, ()>(())),
            MaintenanceProbeAction::Defer
        );
    }

    // scan_end_key tests
    // rename contract: cycle detection + path normalization
    #[test]
    fn test_rename_cycle_detection_logic() {
        // Simulates the cycle guard: new_path starts with old_path + "/"
        let old = normalize_path("/a/b");
        let new = normalize_path("/a/b/c/d");
        assert!(
            new.starts_with(&format!("{old}/")),
            "moving /a/b into /a/b/c/d is a directory cycle"
        );
    }

    #[test]
    fn test_rename_no_cycle_for_sibling() {
        let old = normalize_path("/a/b");
        let new = normalize_path("/a/b2");
        assert!(
            !new.starts_with(&format!("{old}/")),
            "/a/b → /a/b2 is not a cycle (sibling, not subtree)"
        );
    }

    #[test]
    fn test_rename_no_cycle_for_parent() {
        let old = normalize_path("/a/b/c");
        let new = normalize_path("/a");
        assert!(
            !new.starts_with(&format!("{old}/")),
            "moving deeper path to shallower is not a cycle"
        );
    }

    #[test]
    fn test_rename_same_path_noop() {
        let old = normalize_path("/foo/bar/");
        let new = normalize_path("/foo/bar");
        assert_eq!(
            old, new,
            "trailing slash normalization makes paths equal → no-op"
        );
    }

    #[test]
    fn test_rename_root_normalized() {
        let path = normalize_path("/");
        assert_eq!(path, "/");
    }

    #[test]
    fn test_normalize_path_collapses_repeated_slashes() {
        assert_eq!(normalize_path("/a//b"), "/a/b");
        assert_eq!(normalize_path("/a///b/c"), "/a/b/c");
        assert_eq!(normalize_path("//a//b//"), "/a/b");
        assert_eq!(normalize_path("///"), "/");
    }

    #[test]
    fn test_cycle_guard_with_repeated_slashes() {
        // /a//b/c normalizes to /a/b/c — the cycle guard must detect
        // that moving /a/b under /a/b/c is a cycle even with repeated slashes.
        let old = normalize_path("/a/b");
        let new = normalize_path("/a//b/c");
        assert!(
            new.starts_with(&format!("{old}/")),
            "repeated slashes must not bypass cycle guard"
        );
    }

    #[test]
    fn test_encode_s3_key_component_is_injective_for_unicode_inputs() {
        assert_ne!(
            encode_s3_key_component("db9_tenant_租户"),
            encode_s3_key_component("db9_tenant_用户")
        );
    }

    #[test]
    fn test_uploading_lifecycle_is_not_reapable_before_reservation_expiry() {
        let reservation = UploadReservation {
            fs_instance_id: [5u8; 16],
            path: "/data/object.bin".to_string(),
            path_hash: [1u8; 32],
            expected_parent_inode: Some(ROOT_INODE),
            expected_prior_inode: None,
            expected_prior_generation: None,
            expected_size: 1024,
            nonce: 7,
            expires_at: STALE_WRITE_STREAM_SECS + 120,
        };

        assert!(!uploading_lifecycle_is_reapable(
            STALE_WRITE_STREAM_SECS + 1,
            0,
            Some(&reservation)
        ));
        assert!(uploading_lifecycle_is_reapable(
            reservation.expires_at + 1,
            0,
            Some(&reservation)
        ));
    }

    #[test]
    fn test_validate_upload_claims_rejects_filesystem_instance_mismatch() {
        let claims = UploadTokenClaims {
            keyspace: "tenant-a".to_string(),
            fs_instance_id: [1u8; 16],
            staging_inode_id: 42,
            target_path_hash: hex::encode([7u8; 32]),
            expected_parent_inode: Some(ROOT_INODE),
            expected_prior_inode: Some(9),
            expected_prior_generation: Some(3),
            upload_id: "upload-1".to_string(),
            target_version: 42,
            nonce: 77,
            expires_at: 1234,
        };
        let reservation = UploadReservation {
            fs_instance_id: [2u8; 16],
            path: "/data/object.bin".to_string(),
            path_hash: [7u8; 32],
            expected_parent_inode: Some(ROOT_INODE),
            expected_prior_inode: Some(9),
            expected_prior_generation: Some(3),
            expected_size: 1024,
            nonce: 77,
            expires_at: 1234,
        };

        let err = validate_upload_claims(&claims, &reservation, Some("upload-1"))
            .expect_err("instance mismatch must be rejected");
        assert!(
            err.to_string().contains("filesystem instance mismatch"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_reap_stale_pack_spool_directories_removes_only_expired_siblings() {
        let root = std::env::temp_dir().join(format!(
            "db9-fs9-spool-reap-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let current = root.join("current");
        let stale = root.join("stale");
        let fresh = root.join("fresh");

        touch_pack_spool_heartbeat(&current, 200).await.unwrap();
        touch_pack_spool_heartbeat(&stale, 0).await.unwrap();
        touch_pack_spool_heartbeat(&fresh, 170).await.unwrap();
        fs::write(stale.join("bundle.pack"), b"bundle")
            .await
            .unwrap();

        reap_stale_pack_spool_directories(&current, 200, 60)
            .await
            .unwrap();

        assert!(fs::metadata(&current).await.is_ok());
        assert!(fs::metadata(&fresh).await.is_ok());
        assert!(fs::metadata(&stale).await.is_err());

        remove_dir_all_if_exists(&root).await.unwrap();
    }

    #[test]
    fn test_scan_end_key_simple() {
        let prefix = b"_fs_D";
        let end = keys::scan_end_key(prefix);
        assert_eq!(end, b"_fs_E"); // 'D' + 1 = 'E'
    }

    #[test]
    fn test_scan_end_key_is_exclusive_upper_bound() {
        let prefix = b"_fs_S";
        let end = keys::scan_end_key(prefix);
        // prefix < end
        assert!(prefix.to_vec() < end);
    }

    #[test]
    fn test_scan_end_key_with_bytes() {
        let prefix = vec![0x01, 0x02, 0x03];
        let end = keys::scan_end_key(&prefix);
        assert_eq!(end, vec![0x01, 0x02, 0x04]);
    }

    #[test]
    fn test_scan_end_key_empty_prefix_is_unbounded() {
        let end = keys::scan_end_key(b"");
        assert!(
            end.is_empty(),
            "empty prefix must produce an unbounded end key"
        );
    }

    #[test]
    fn test_scan_end_key_all_ff_prefix_is_unbounded() {
        let prefix = vec![0xFF, 0xFF, 0xFF];
        let end = keys::scan_end_key(&prefix);
        assert!(
            end.is_empty(),
            "all-0xFF prefix has no exclusive successor and must be unbounded"
        );
    }

    #[test]
    fn test_validate_dir_entry_name_rejects_nested_paths() {
        let err = validate_dir_entry_name("batch/a.txt").expect_err("nested path must be rejected");
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(matches!(fs_err, EmbeddedFsError::InvalidInput(_)));
    }

    #[test]
    fn test_dir_entry_name_from_scan_key_requires_exact_prefix() {
        let prefix = keys::dir_prefix(ROOT_INODE);
        let key = keys::dir_entry_key(ROOT_INODE + 1, "hello.txt");
        assert_eq!(dir_entry_name_from_scan_key(&prefix, &key), None);
    }

    #[test]
    fn test_dir_entry_name_from_scan_key_rejects_nested_path_suffix() {
        let prefix = keys::dir_prefix(ROOT_INODE);
        let mut key = prefix.clone();
        key.extend_from_slice(b"batch/a.txt");
        assert_eq!(dir_entry_name_from_scan_key(&prefix, &key), None);
    }

    #[test]
    fn test_dir_entry_name_from_scan_key_accepts_exact_fs_scan_key() {
        let prefix = keys::dir_prefix(ROOT_INODE);
        let key = keys::dir_entry_key(ROOT_INODE, "hello.txt");
        assert_eq!(
            dir_entry_name_from_scan_key(&prefix, &key),
            Some("hello.txt")
        );
    }

    #[test]
    fn test_dir_entry_name_from_scan_key_rejects_guessed_keyspace_prefix() {
        let prefix = keys::dir_prefix(ROOT_INODE);
        let mut key = vec![b'x', 0, 0, 7];
        key.extend_from_slice(&keys::dir_entry_key(ROOT_INODE, "hello.txt"));
        assert_eq!(dir_entry_name_from_scan_key(&prefix, &key), None);
    }

    #[test]
    fn test_parse_marked_inode_id_requires_exact_prefix() {
        let prefix = keys::staging_write_prefix();
        let key = keys::staging_write_key(42);
        assert_eq!(parse_marked_inode_id(&prefix, &key), Some(42));
        assert_eq!(
            parse_marked_inode_id(&keys::orphan_inode_prefix(), &key),
            None
        );
    }

    #[test]
    fn test_parse_marked_inode_id_rejects_guessed_keyspace_prefix() {
        let prefix = keys::staging_write_prefix();
        let key = keys::staging_write_key(42);
        let mut prefixed = vec![b'x', 0, 0, 5];
        prefixed.extend_from_slice(&key);
        assert_eq!(parse_marked_inode_id(&prefix, &prefixed), None);
    }

    // ── Behavioral rename tests (require TiKV) ─────────────────────────
    //
    // These tests exercise actual rename() calls on EmbeddedPageFs and
    // verify filesystem state.  They are #[ignore] because they need a
    // running TiKV cluster (PD_ENDPOINTS env var).
    //
    //   cargo test -p db9-server rename_behavioral -- --ignored

    fn behavioral_test_keyspace() -> String {
        if let Ok(keyspace) = std::env::var("TIKV_KEYSPACE") {
            if !keyspace.trim().is_empty() {
                return keyspace;
            }
        }

        assert!(
            std::env::var("TIKV_CA_PATH").is_err(),
            "behavioral fs9 tests in TLS mode require TIKV_KEYSPACE to be set to a fresh, pre-created keyspace"
        );

        let seq = TEST_KEYSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
        format!("fs9_behavioral_{}_{}", std::process::id(), seq)
    }

    async fn ensure_behavioral_test_keyspace(pd: &str, keyspace: &str) {
        if std::env::var("TIKV_CA_PATH").is_ok() {
            return;
        }

        let pd_primary = pd
            .split(',')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .expect("PD_ENDPOINTS must include at least one endpoint");
        let base = format!("http://{}/pd/api/v2/keyspaces", pd_primary);
        let keyspace_url = format!("{}/{}", base, keyspace);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build PD HTTP client for behavioral tests");

        let mut last_err = String::new();
        for _ in 0..15 {
            if let Ok(resp) = client
                .post(&base)
                .json(&serde_json::json!({ "name": keyspace }))
                .send()
                .await
            {
                let status = resp.status();
                if !(status.is_success() || status.as_u16() == 409 || status.as_u16() == 500) {
                    let _ = resp.text().await;
                }
            }

            match client.get(&keyspace_url).send().await {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("verify keyspace status={}, body={}", status.as_u16(), body);
                }
                Err(err) => {
                    last_err = format!("verify keyspace error: {err}");
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        panic!(
            "unable to provision behavioral fs9 keyspace '{}' via {}: {}",
            keyspace, pd_primary, last_err
        );
    }

    async fn make_fs() -> EmbeddedPageFs {
        let pd = std::env::var("PD_ENDPOINTS").unwrap_or("127.0.0.1:2379".into());
        let keyspace = behavioral_test_keyspace();
        ensure_behavioral_test_keyspace(&pd, &keyspace).await;
        let config = tikv_client::Config::default().with_keyspace(&keyspace);
        let client = TransactionClient::new_with_config(vec![pd], config)
            .await
            .expect("TiKV connection required for behavioral tests");
        let client = Arc::new(client);
        let superblock = EmbeddedPageFs::load_or_init_superblock(client.clone(), &keyspace)
            .await
            .expect("load_or_init_superblock");
        let fs = EmbeddedPageFs::new(client, keyspace, &superblock);
        fs.init_filesystem().await.expect("init_filesystem");
        fs
    }

    /// Helper: ensure directory exists (idempotent).
    async fn ensure_dir(fs: &EmbeddedPageFs, path: &str) {
        let _ = fs.mkdir(path, true).await;
    }

    /// Helper: clean up a path (file or dir) — best effort.
    async fn cleanup(fs: &EmbeddedPageFs, path: &str) {
        let _ = fs.remove_recursive(path).await;
        let _ = fs.remove(path).await;
    }

    async fn inode_snapshot(fs: &EmbeddedPageFs, path: &str) -> serde_json::Value {
        let mut txn = fs.begin_internal().await.unwrap();
        let (_, inode) = resolve_path(&mut txn, path).await.unwrap();
        let _ = txn.rollback().await;
        serde_json::to_value(&inode).unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn test_init_filesystem_persists_binding_and_allocator_state() {
        let fs = make_fs().await;

        let mut txn = fs.begin().await.unwrap();
        let superblock = load_superblock(&mut txn).await.unwrap();
        let inode_next = load_allocator_counter(&mut txn, &keys::inode_allocator_key(), "inode")
            .await
            .unwrap();
        let bundle_next = load_allocator_counter(&mut txn, &keys::bundle_allocator_key(), "bundle")
            .await
            .unwrap();
        let _ = txn.rollback().await;

        assert_eq!(superblock.format_version, FS9_STORAGE_FORMAT_VERSION);
        assert_ne!(superblock.fs_instance_id, [0u8; 16]);
        assert_eq!(superblock.object_store, current_object_store_binding());
        assert_eq!(inode_next, ROOT_INODE + 1);
        assert_eq!(bundle_next, 1);
    }

    #[tokio::test]
    #[ignore]
    async fn test_maintenance_probe_detects_superblock_replacement() {
        let fs = make_fs().await;
        fs.write_file("/stale.txt", b"stale").await.unwrap();

        let mut txn = fs.begin_unchecked().await.unwrap();
        let mut superblock = load_superblock(&mut txn).await.unwrap();
        superblock.fs_instance_id = new_fs_instance_id();
        save_superblock(&mut txn, &superblock).await.unwrap();
        txn.commit().await.unwrap();

        assert!(
            !fs.current_instance_matches_superblock().await.unwrap(),
            "maintenance probe must stop once the bound fs instance changes"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_backend_reacquire_requires_restart_after_instance_change() {
        use crate::extensions::fs::embedded::EmbeddedFsBackend;

        let fs = make_fs().await;
        fs.write_file("/stale.txt", b"stale").await.unwrap();

        let mut txn = fs.begin_unchecked().await.unwrap();
        let mut superblock = load_superblock(&mut txn).await.unwrap();
        superblock.fs_instance_id = new_fs_instance_id();
        save_superblock(&mut txn, &superblock).await.unwrap();
        txn.commit().await.unwrap();

        let err = match EmbeddedFsBackend::new(fs.client.clone(), fs.keyspace.clone()).await {
            Ok(_) => panic!("backend reacquire must require restart"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("restart db9-server"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_inode_allocator_reserves_ids_without_mutating_superblock() {
        let fs = make_fs().await;

        let mut txn = fs.begin().await.unwrap();
        let before = serde_json::to_vec(&load_superblock(&mut txn).await.unwrap()).unwrap();
        let _ = txn.rollback().await;

        let first = fs.alloc_inode_id().await.unwrap();
        let second = fs.alloc_inode_id().await.unwrap();
        assert_eq!(second, first + 1);

        let mut txn = fs.begin().await.unwrap();
        let after = serde_json::to_vec(&load_superblock(&mut txn).await.unwrap()).unwrap();
        let next_inode = load_allocator_counter(&mut txn, &keys::inode_allocator_key(), "inode")
            .await
            .unwrap();
        let _ = txn.rollback().await;

        assert_eq!(before, after, "inode allocation must not rewrite _fs_S");
        assert!(
            next_inode > ROOT_INODE + 1,
            "inode allocator counter must advance independently"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_inlineblob_behavioral_write_file_routes_to_blob() {
        let fs = make_fs().await;
        let base = "/test_inlineblob_write_file";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 {
            // InlineBlob is disabled in this environment.
            return;
        }
        let path = &format!("{base}/tiny.bin");
        let data: Vec<u8> = (0..inline_max.min(32)).map(|v| (v % 251) as u8).collect();
        fs.write_file(path, &data).await.unwrap();

        let inode = fs.stat(path).await.unwrap();
        assert_eq!(inode.data, DataRef::InlineBlob);
        assert_eq!(inode.size, data.len() as u64);
        assert_eq!(fs.read_file(path).await.unwrap(), data);

        let mut txn = fs.begin().await.unwrap();
        assert!(
            txn.get(keys::blob_key(inode.id)).await.unwrap().is_some(),
            "InlineBlob must have a blob key"
        );
        let page_prefix = keys::page_prefix(inode.id);
        let page_end = keys::scan_end_key(&page_prefix);
        let mut pages = txn.scan(page_prefix..page_end, u32::MAX).await.unwrap();
        assert!(
            pages.next().is_none(),
            "InlineBlob must not leave staging pages behind"
        );
        let _ = txn.rollback().await;

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    /// Regression for the fs metadata scan/decode path: exact path lookups can
    /// still work while `readdir` goes empty if scan keys are decoded through a
    /// guessed layout instead of the exact fs prefix.
    async fn test_readdir_behavioral_exact_names_are_preserved() {
        let fs = make_fs().await;
        let nonce = rand::thread_rng().gen::<u64>();
        let root_prefix = format!("test_readdir_exact_{nonce}");
        let root_file_1 = format!("/{root_prefix}_hello.txt");
        let root_file_2 = format!("/{root_prefix}_data.bin");
        let batch_dir = format!("/{root_prefix}_batch");
        let batch_file_names = ["alpha.txt", "beta.bin", "gamma.json"];

        cleanup(&fs, &root_file_1).await;
        cleanup(&fs, &root_file_2).await;
        cleanup(&fs, &batch_dir).await;
        ensure_dir(&fs, &batch_dir).await;

        fs.write_file(&root_file_1, b"hello").await.unwrap();
        fs.write_file(&root_file_2, b"\x00\x01\x02\x03")
            .await
            .unwrap();
        for name in batch_file_names {
            fs.write_file(&format!("{batch_dir}/{name}"), name.as_bytes())
                .await
                .unwrap();
        }

        let root_names: Vec<String> = fs
            .readdir("/")
            .await
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(root_names.iter().any(|name| name == &root_file_1[1..]));
        assert!(root_names.iter().any(|name| name == &root_file_2[1..]));
        assert!(root_names.iter().any(|name| name == &batch_dir[1..]));

        let batch_names: Vec<String> = fs
            .readdir(&batch_dir)
            .await
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            batch_names,
            batch_file_names
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        cleanup(&fs, &root_file_1).await;
        cleanup(&fs, &root_file_2).await;
        cleanup(&fs, &batch_dir).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_readdir_behavioral_ignores_nested_path_dir_entries() {
        let fs = make_fs().await;
        let base = "/test_readdir_invalid_entries";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        fs.write_file(&format!("{base}/hello.txt"), b"hello")
            .await
            .unwrap();

        let mut txn = fs.begin().await.unwrap();
        let bogus_inode_id = fs.alloc_inode_id().await.unwrap();
        let bogus_inode = Inode::new_file(bogus_inode_id, 0o644);
        save_inode(&mut txn, &bogus_inode).await.unwrap();
        txn.put(
            keys::dir_entry_key(
                ROOT_INODE,
                &format!("{}/bogus.txt", base.trim_start_matches('/')),
            ),
            bogus_inode_id.to_be_bytes().to_vec(),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let names: Vec<String> = fs
            .readdir("/")
            .await
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            names.iter().all(|name| !name.contains('/')),
            "root readdir must ignore nested-path dir entries: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name == base.trim_start_matches('/')),
            "root readdir must keep valid directory entries: {names:?}"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_readdir_recursive_behavioral_caps_frontier_scan_with_batched_hydration() {
        let fs = make_fs().await;
        let nonce = rand::thread_rng().gen::<u64>();
        let base = format!("/test_readdir_recursive_{nonce}");
        let dir_a = format!("{base}/a");
        let dir_b = format!("{dir_a}/b");
        let dir_c = format!("{dir_b}/c");
        let dir_batch = format!("{base}/batch");

        cleanup(&fs, &base).await;
        ensure_dir(&fs, &dir_c).await;
        ensure_dir(&fs, &dir_batch).await;

        fs.write_file(&format!("{base}/root.txt"), b"root")
            .await
            .unwrap();
        fs.write_file(&format!("{dir_a}/alpha.txt"), b"alpha")
            .await
            .unwrap();
        fs.write_file(&format!("{dir_b}/beta.txt"), b"beta")
            .await
            .unwrap();
        fs.write_file(&format!("{dir_c}/charlie.txt"), b"charlie")
            .await
            .unwrap();
        fs.write_file(&format!("{dir_batch}/delta.txt"), b"delta")
            .await
            .unwrap();

        let result = fs
            .readdir_recursive(
                &base,
                FsRecursiveReaddirOptions {
                    max_depth: 8,
                    max_entries: 6,
                    exclude_set: None,
                },
            )
            .await
            .unwrap();

        let paths = result
            .entries
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                format!("{base}/a"),
                format!("{base}/a/alpha.txt"),
                format!("{base}/a/b"),
                format!("{base}/batch"),
                format!("{base}/batch/delta.txt"),
                format!("{base}/root.txt"),
            ]
        );
        assert!(
            result.truncated,
            "recursive traversal should stop once max_entries is exhausted"
        );
        assert_eq!(
            result.total_dirs_scanned, 3,
            "should scan root plus the next frontier before hitting the cap"
        );

        cleanup(&fs, &base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_readdir_recursive_behavioral_dangling_dirent_does_not_consume_budget() {
        let fs = make_fs().await;
        let nonce = rand::thread_rng().gen::<u64>();
        let base = format!("/test_readdir_recursive_dangling_{nonce}");

        cleanup(&fs, &base).await;
        ensure_dir(&fs, &base).await;
        fs.write_file(&format!("{base}/bb.txt"), b"ok")
            .await
            .unwrap();

        let mut txn = fs.begin().await.unwrap();
        let (base_inode_id, _) = resolve_path(&mut txn, &base).await.unwrap();
        txn.put(
            keys::dir_entry_key(base_inode_id, "aa-dangling.txt"),
            u64::MAX.to_be_bytes().to_vec(),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let result = fs
            .readdir_recursive(
                &base,
                FsRecursiveReaddirOptions {
                    max_depth: 1,
                    max_entries: 1,
                    exclude_set: None,
                },
            )
            .await
            .unwrap();
        let paths = result
            .entries
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![format!("{base}/bb.txt")]);
        assert!(
            !result.truncated,
            "dangling dirents should not consume recursive result budget"
        );

        cleanup(&fs, &base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_inlineblob_behavioral_large_write_routes_to_object() {
        let fs = make_fs().await;
        let base = "/test_inlineblob_large_write";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 || fs9_config().s3.is_none() {
            return;
        }
        let path = &format!("{base}/large.bin");
        let data: Vec<u8> = (0..(inline_max + 1)).map(|v| (v % 251) as u8).collect();
        fs.write_file(path, &data).await.unwrap();

        let inode = fs.stat(path).await.unwrap();
        assert!(matches!(inode.data, DataRef::Object { .. }));
        assert_eq!(inode.size, data.len() as u64);
        assert_eq!(fs.read_file(path).await.unwrap(), data);

        let mut txn = fs.begin().await.unwrap();
        assert!(
            txn.get(keys::blob_key(inode.id)).await.unwrap().is_none(),
            "object route must not leave an inline blob key"
        );
        assert!(
            txn.get(keys::page_key(inode.id, 0))
                .await
                .unwrap()
                .is_none(),
            "published object route must not leave staged pages"
        );
        let _ = txn.rollback().await;

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_write_file_behavioral_replaces_existing_object_file() {
        let fs = make_fs().await;
        let base = "/test_write_file_replace_existing_object";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 || fs9_config().s3.is_none() {
            return;
        }

        let path = &format!("{base}/replace.bin");
        let original: Vec<u8> = (0..(inline_max + 1)).map(|idx| (idx % 251) as u8).collect();
        fs.write_file(path, &original).await.unwrap();

        let original_inode = fs.stat(path).await.unwrap();
        assert!(
            matches!(original_inode.data, DataRef::Object { .. }),
            "large write must route through object storage"
        );
        let original_data_ref = original_inode.data.clone();

        let append_err = fs
            .append_file(path, b"!")
            .await
            .expect_err("partial mutation on an object-backed file must stay sealed");
        assert!(
            append_err
                .to_string()
                .contains("append is not supported for sealed files"),
            "unexpected append error: {append_err}"
        );

        let replacement = b"replacement-inline".to_vec();
        fs.write_file(path, &replacement).await.unwrap();

        let replaced_inode = fs.stat(path).await.unwrap();
        assert_eq!(
            replaced_inode.id, original_inode.id,
            "full replace should reuse the inode instead of creating a second published file"
        );
        assert_eq!(replaced_inode.data, DataRef::InlineBlob);
        assert_eq!(replaced_inode.size, replacement.len() as u64);
        assert_eq!(fs.read_file(path).await.unwrap(), replacement);

        let mut txn = fs.begin_internal().await.unwrap();
        assert_eq!(
            lifecycle::load_lifecycle(&mut txn, replaced_inode.id)
                .await
                .unwrap(),
            Some(FileLifecycle::Deleting {
                fs_instance_id: fs.runtime_state().fs_instance_id,
                data_ref: original_data_ref,
            }),
            "full replace of an object-backed file must retire the old object via lifecycle cleanup"
        );
        let _ = txn.rollback().await;

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_inline_read_paths_do_not_mutate_inode_metadata() {
        let fs = make_fs().await;
        let base = "/test_inline_read_paths_do_not_mutate_inode_metadata";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 {
            return;
        }

        let path = &format!("{base}/inline.txt");
        let data = b"inline-read-regression".to_vec();
        fs.write_file(path, &data).await.unwrap();

        let inode = fs.stat(path).await.unwrap();
        assert_eq!(inode.data, DataRef::InlineBlob);

        let before = inode_snapshot(&fs, path).await;
        let stat_inode = fs.stat(path).await.unwrap();
        assert_eq!(stat_inode.size, data.len() as u64);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        assert_eq!(fs.read_file(path).await.unwrap(), data);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        assert_eq!(
            fs.read_file_at(path, 3, 6).await.unwrap(),
            data[3..9].to_vec()
        );
        assert_eq!(inode_snapshot(&fs, path).await, before);

        assert!(fs
            .read_file_at(path, data.len() as u64, 16)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(inode_snapshot(&fs, path).await, before);

        let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
        let mut streamed = Vec::new();
        reader.read_to_end(&mut streamed).await.unwrap();
        assert_eq!(streamed, data);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_pack_read_paths_do_not_mutate_inode_metadata() {
        if fs9_config().s3.is_none() {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_pack_read_paths_do_not_mutate_inode_metadata";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let first_path = format!("{base}/first.txt");
        let second_path = format!("{base}/second.txt");
        let first_data = b"pack-entry-first".to_vec();
        let second_data = b"pack-entry-second".to_vec();
        let entries = fs
            .batch_write(vec![
                FsBatchWriteFile {
                    path: first_path.clone(),
                    data: first_data.clone(),
                },
                FsBatchWriteFile {
                    path: second_path.clone(),
                    data: second_data,
                },
            ])
            .await
            .unwrap();
        assert!(entries.iter().all(|entry| entry.result.is_ok()));

        let inode = fs.stat(&first_path).await.unwrap();
        assert!(
            matches!(inode.data, DataRef::PackEntry { .. }),
            "batch pack write must publish pack-backed files"
        );

        let before = inode_snapshot(&fs, &first_path).await;
        let stat_inode = fs.stat(&first_path).await.unwrap();
        assert_eq!(stat_inode.size, first_data.len() as u64);
        assert_eq!(inode_snapshot(&fs, &first_path).await, before);

        assert_eq!(fs.read_file(&first_path).await.unwrap(), first_data);
        assert_eq!(inode_snapshot(&fs, &first_path).await, before);

        assert_eq!(
            fs.read_file_at(&first_path, 5, 4).await.unwrap(),
            first_data[5..9].to_vec()
        );
        assert_eq!(inode_snapshot(&fs, &first_path).await, before);

        let mut reader = fs
            .read_file_stream(&first_path, first_data.len())
            .await
            .unwrap();
        let mut streamed = Vec::new();
        reader.read_to_end(&mut streamed).await.unwrap();
        assert_eq!(streamed, first_data);
        assert_eq!(inode_snapshot(&fs, &first_path).await, before);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_batch_inline_read_behavioral_pack_entry_round_trip() {
        if fs9_config().s3.is_none() {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_batch_inline_read_behavioral_pack_entry_round_trip";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let first_path = format!("{base}/first.txt");
        let second_path = format!("{base}/second.txt");
        let first_data = b"pack-batch-first".to_vec();
        let second_data = b"pack-batch-second".to_vec();
        let entries = fs
            .batch_write(vec![
                FsBatchWriteFile {
                    path: first_path.clone(),
                    data: first_data.clone(),
                },
                FsBatchWriteFile {
                    path: second_path.clone(),
                    data: second_data.clone(),
                },
            ])
            .await
            .unwrap();
        assert!(entries.iter().all(|entry| entry.result.is_ok()));

        let first_inode = fs.stat(&first_path).await.unwrap();
        let second_inode = fs.stat(&second_path).await.unwrap();
        assert!(matches!(first_inode.data, DataRef::PackEntry { .. }));
        assert!(matches!(second_inode.data, DataRef::PackEntry { .. }));

        let first_before = inode_snapshot(&fs, &first_path).await;
        let second_before = inode_snapshot(&fs, &second_path).await;
        let results = fs
            .batch_inline_read(
                &[first_path.clone(), second_path.clone()],
                first_data.len().max(second_data.len()),
                first_data.len() + second_data.len(),
            )
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap(), &first_data);
        assert_eq!(results[1].as_ref().unwrap(), &second_data);
        assert_eq!(inode_snapshot(&fs, &first_path).await, first_before);
        assert_eq!(inode_snapshot(&fs, &second_path).await, second_before);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_object_read_paths_do_not_mutate_inode_metadata() {
        let fs = make_fs().await;
        let base = "/test_object_read_paths_do_not_mutate_inode_metadata";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 || fs9_config().s3.is_none() {
            return;
        }

        let path = &format!("{base}/object.bin");
        let data: Vec<u8> = (0..(inline_max + 1)).map(|idx| (idx % 251) as u8).collect();
        fs.write_file(path, &data).await.unwrap();

        let inode = fs.stat(path).await.unwrap();
        assert!(
            matches!(inode.data, DataRef::Object { .. }),
            "large write must route through object storage"
        );

        let before = inode_snapshot(&fs, path).await;
        let stat_inode = fs.stat(path).await.unwrap();
        assert_eq!(stat_inode.size, data.len() as u64);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        assert_eq!(fs.read_file(path).await.unwrap(), data);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        assert_eq!(
            fs.read_file_at(path, 7, 11).await.unwrap(),
            data[7..18].to_vec()
        );
        assert_eq!(inode_snapshot(&fs, path).await, before);

        let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
        let mut streamed = Vec::new();
        reader.read_to_end(&mut streamed).await.unwrap();
        assert_eq!(streamed, data);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        let prepared = fs.prepare_download(path).await.unwrap();
        assert_eq!(prepared.storage, FsStorage::Object);
        assert_eq!(prepared.size, data.len() as u64);
        assert!(prepared.range_supported);
        assert_eq!(inode_snapshot(&fs, path).await, before);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_batch_inline_read_behavioral_object_round_trip() {
        let fs = make_fs().await;
        let base = "/test_batch_inline_read_behavioral_object_round_trip";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 || fs9_config().s3.is_none() {
            return;
        }

        let path = format!("{base}/object.bin");
        let data: Vec<u8> = (0..(inline_max + 1)).map(|idx| (idx % 251) as u8).collect();
        fs.write_file(&path, &data).await.unwrap();

        let inode = fs.stat(&path).await.unwrap();
        assert!(matches!(inode.data, DataRef::Object { .. }));

        let before = inode_snapshot(&fs, &path).await;
        let results = fs
            .batch_inline_read(&[path.clone()], data.len(), data.len())
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap(), &data);
        assert_eq!(inode_snapshot(&fs, &path).await, before);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_batch_inline_read_behavioral_shared_s3_error_for_external_entries() {
        if fs9_config().s3.is_some() {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_batch_inline_read_behavioral_shared_s3_error";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let object_path = format!("{base}/object.bin");
        let pack_path = format!("{base}/pack.bin");
        let bundle_id = fs.alloc_bundle_id().await.unwrap();
        let object_inode_id = fs.alloc_inode_id().await.unwrap();
        let pack_inode_id = fs.alloc_inode_id().await.unwrap();

        let mut txn = fs.begin().await.unwrap();
        let (object_parent, object_name) =
            ensure_parents_and_resolve_parent(&fs, &mut txn, &object_path)
                .await
                .unwrap();
        let mut object_inode = Inode::new_file(object_inode_id, 0o644);
        object_inode.size = 4;
        object_inode.data = DataRef::Object {
            key: "missing-object".to_string(),
            version: 1,
            checksum: [1u8; 32],
        };
        save_inode(&mut txn, &object_inode).await.unwrap();
        link(&mut txn, object_parent, &object_name, object_inode_id)
            .await
            .unwrap();

        let (pack_parent, pack_name) = ensure_parents_and_resolve_parent(&fs, &mut txn, &pack_path)
            .await
            .unwrap();
        let mut pack_inode = Inode::new_file(pack_inode_id, 0o644);
        pack_inode.size = 4;
        pack_inode.data = DataRef::PackEntry {
            bundle_id,
            offset: 0,
            len: 4,
            checksum: [2u8; 32],
            generation: 1,
        };
        save_inode(&mut txn, &pack_inode).await.unwrap();
        link(&mut txn, pack_parent, &pack_name, pack_inode_id)
            .await
            .unwrap();

        save_bundle_manifest(
            &mut txn,
            &BundleManifest {
                fs_instance_id: fs.instance_identity().fs_instance_id,
                bundle_id,
                key: "missing-pack".to_string(),
                created_at: current_unix_timestamp(),
                object_size: 4,
                footer_offset: 0,
                entry_count: 1,
                live_entries: 1,
                live_bytes: 4,
                stale_entries: 0,
                checksum: [3u8; 32],
                state: BundleManifestState::Active,
            },
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let results = fs
            .batch_inline_read(&[object_path.clone(), pack_path.clone()], 8, 8)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        for result in results {
            let fs_err = result
                .unwrap_err()
                .downcast::<EmbeddedFsError>()
                .expect("external entry failure should stay typed");
            assert!(
                matches!(fs_err, EmbeddedFsError::Internal(msg) if msg == "S3 is not configured")
            );
        }

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_inlineblob_behavioral_truncate_rejects_non_inline_growth() {
        let fs = make_fs().await;
        let base = "/test_inlineblob_truncate_reject_non_inline";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 {
            return;
        }
        let path = &format!("{base}/file.bin");
        fs.write_file(path, b"hello").await.unwrap();

        let err = fs
            .truncate(path, u64::try_from(inline_max + 1).unwrap())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("truncate is only supported for inline files"),
            "unexpected error: {err}"
        );
        assert_eq!(fs.read_file(path).await.unwrap(), b"hello");

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_inlineblob_behavioral_write_at_rejects_non_inline_growth() {
        let fs = make_fs().await;
        let base = "/test_inlineblob_write_at_reject_non_inline";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 {
            return;
        }
        let path = &format!("{base}/file.bin");
        fs.write_file(path, b"hello").await.unwrap();

        let err = fs
            .write_file_at(path, u64::try_from(inline_max + 1).unwrap(), b"X")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("partial mutation is only supported for inline files"),
            "unexpected error: {err}"
        );
        assert_eq!(fs.read_file(path).await.unwrap(), b"hello");

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_inlineblob_behavioral_rename_overwrite_deletes_inline_blob() {
        let fs = make_fs().await;
        let base = "/test_inlineblob_rename_overwrite";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 {
            // InlineBlob is disabled in this environment.
            return;
        }
        let src = &format!("{base}/src.bin");
        let dst = &format!("{base}/dst.bin");
        fs.write_file(src, b"source").await.unwrap();
        fs.write_file(dst, b"dest").await.unwrap();

        let dst_inode = fs.stat(dst).await.unwrap();
        assert_eq!(dst_inode.data, DataRef::InlineBlob);

        fs.rename(src, dst).await.unwrap();
        assert!(fs.stat(src).await.is_err());
        assert_eq!(fs.read_file(dst).await.unwrap(), b"source");

        let mut txn = fs.begin().await.unwrap();
        assert!(
            txn.get(keys::inode_key(dst_inode.id))
                .await
                .unwrap()
                .is_none(),
            "rename overwrite must delete dest inode"
        );
        assert!(
            txn.get(keys::blob_key(dst_inode.id))
                .await
                .unwrap()
                .is_none(),
            "rename overwrite must delete dest inline blob key"
        );
        let _ = txn.rollback().await;

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_basic_file() {
        let fs = make_fs().await;
        let dir = "/test_rename_basic";
        cleanup(&fs, dir).await;
        ensure_dir(&fs, dir).await;

        let old = &format!("{dir}/a.txt");
        let new = &format!("{dir}/b.txt");
        fs.write_file(old, b"hello").await.unwrap();

        fs.rename(old, new).await.unwrap();

        // old name must be gone
        assert!(fs.stat(old).await.is_err(), "old path should not exist");
        // new name must exist with same content
        let data = fs.read_file(new).await.unwrap();
        assert_eq!(data, b"hello", "content must be preserved");

        cleanup(&fs, dir).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_cross_directory() {
        let fs = make_fs().await;
        let base = "/test_rename_cross";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/src")).await;
        ensure_dir(&fs, &format!("{base}/dst")).await;

        let old = &format!("{base}/src/file.txt");
        let new = &format!("{base}/dst/file.txt");
        fs.write_file(old, b"cross").await.unwrap();

        fs.rename(old, new).await.unwrap();

        assert!(fs.stat(old).await.is_err());
        assert_eq!(fs.read_file(new).await.unwrap(), b"cross");

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_read_file_stream_behavioral_matches_read_file() {
        let fs = make_fs().await;
        let base = "/test_read_file_stream";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/large.bin");
        let data: Vec<u8> = (0..(PAGE_SIZE * 6 + 123))
            .map(|idx| (idx % 251) as u8)
            .collect();
        fs.write_file(path, &data).await.unwrap();

        let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
        let mut streamed = Vec::new();
        reader.read_to_end(&mut streamed).await.unwrap();

        assert_eq!(streamed, data);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_behavioral_matches_write_file() {
        let fs = make_fs().await;
        let base = "/test_begin_write_stream";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/streamed.bin");
        let data: Vec<u8> = (0..(PAGE_SIZE * 5 + 77))
            .map(|idx| (idx % 239) as u8)
            .collect();

        let mut writer = fs
            .begin_write_stream(
                path,
                FsWriteStreamOptions {
                    expected_size: Some(data.len() as u64),
                },
            )
            .await
            .unwrap();
        for chunk in data.chunks(11_111) {
            writer.write_chunk(chunk).await.unwrap();
        }
        let written = writer.finish().await.unwrap();

        assert_eq!(written, data.len());
        assert_eq!(fs.read_file(path).await.unwrap(), data);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_without_size_keeps_small_files_mutable() {
        if fs9_config().s3.is_none() || fs9_config().inline_max_bytes == 0 {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_begin_write_stream_unknown_size_small";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/small.bin");
        let data: Vec<u8> = (0..fs9_config().inline_max_bytes.clamp(1, 32))
            .map(|idx| (idx % 251) as u8)
            .collect();

        let mut writer = fs
            .begin_write_stream(
                path,
                FsWriteStreamOptions {
                    expected_size: None,
                },
            )
            .await
            .unwrap();
        for chunk in data.chunks(7) {
            writer.write_chunk(chunk).await.unwrap();
        }
        let written = writer.finish().await.unwrap();

        assert_eq!(written, data.len());
        let inode = fs.stat(path).await.unwrap();
        assert_eq!(inode.data, DataRef::InlineBlob);

        let mut expected = data.clone();
        expected.extend_from_slice(b"-tail");
        fs.append_file(path, b"-tail").await.unwrap();
        assert_eq!(fs.read_file(path).await.unwrap(), expected);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_large_without_s3_rejects_without_touching_existing_file() {
        if fs9_config().s3.is_some() || fs9_config().inline_max_bytes == 0 {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_begin_write_stream_large_without_s3";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/existing.bin");
        let original = b"hello-inline";
        fs.write_file(path, original).await.unwrap();

        let err = fs
            .begin_write_stream(
                path,
                FsWriteStreamOptions {
                    expected_size: Some((fs9_config().inline_max_bytes + 1) as u64),
                },
            )
            .await
            .err()
            .expect("large streaming write without S3 must fail at init");
        assert!(
            err.to_string().contains("require S3-backed object storage"),
            "unexpected error: {err}"
        );

        let inode = fs.stat(path).await.unwrap();
        assert_eq!(inode.data, DataRef::InlineBlob);
        assert_eq!(inode.size, original.len() as u64);
        assert_eq!(fs.read_file(path).await.unwrap(), original);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_without_size_routes_large_files_after_spool() {
        if fs9_config().s3.is_none() || fs9_config().object_min_bytes == 0 {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_begin_write_stream_unknown_size_large";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/large.bin");
        let data_len = fs9_config()
            .object_min_bytes
            .max(WRITE_STREAM_FLUSH_BYTES)
            .saturating_add(17);
        let data: Vec<u8> = (0..data_len).map(|idx| (idx % 251) as u8).collect();

        let mut writer = fs
            .begin_write_stream(
                path,
                FsWriteStreamOptions {
                    expected_size: None,
                },
            )
            .await
            .unwrap();
        for chunk in data.chunks(19_337) {
            writer.write_chunk(chunk).await.unwrap();
        }
        let written = writer.finish().await.unwrap();

        assert_eq!(written, data.len());
        let inode = fs.stat(path).await.unwrap();
        assert!(matches!(inode.data, DataRef::Object { .. }));
        assert_eq!(fs.read_file(path).await.unwrap(), data);
        assert!(fs.append_file(path, b"!").await.is_err());

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_append_new_large_file_rolls_back_creation() {
        let fs = make_fs().await;
        let base = "/test_append_new_large_file_rolls_back_creation";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let inline_max = fs9_config().inline_max_bytes;
        if inline_max == 0 {
            return;
        }

        let path = &format!("{base}/large-append.bin");
        let data = vec![b'x'; inline_max.saturating_add(1)];
        let err = fs.append_file(path, &data).await.expect_err(
            "append_file on a missing path must reject non-inline growth without creating the file",
        );
        assert!(
            err.to_string()
                .contains("append is only supported for inline files"),
            "unexpected error: {err}"
        );

        let stat_err = fs
            .stat(path)
            .await
            .expect_err("failed append on a missing path must not leave an empty file behind");
        assert!(
            is_not_found_error(&stat_err),
            "unexpected stat error after failed append: {stat_err}"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_abort_preserves_existing_file() {
        let fs = make_fs().await;
        let base = "/test_begin_write_stream_abort";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/stable.bin");
        let original = b"stable-before-abort".to_vec();
        fs.write_file(path, &original).await.unwrap();

        let mut writer = fs
            .begin_write_stream(
                path,
                FsWriteStreamOptions {
                    expected_size: Some(32),
                },
            )
            .await
            .unwrap();
        writer
            .write_chunk(b"new-data-that-must-not-commit")
            .await
            .unwrap();
        writer.abort().await.unwrap();

        assert_eq!(fs.read_file(path).await.unwrap(), original);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_abort_upload_cleans_staging_state() {
        if fs9_config().s3.is_none() || fs9_config().object_min_bytes == 0 {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_abort_upload_cleanup";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/object.bin");
        let upload = fs
            .create_upload(path, fs9_config().object_min_bytes as u64)
            .await
            .unwrap();
        let claims = verify_upload_token(&upload.upload_token).unwrap();

        fs.abort_upload(&upload.upload_token)
            .await
            .expect("abort_upload must clear staging state");

        let mut txn = fs.begin_internal().await.unwrap();
        assert!(
            load_inode(&mut txn, claims.staging_inode_id)
                .await
                .unwrap()
                .is_none(),
            "staging inode must be removed by abort cleanup"
        );
        assert!(
            lifecycle::load_lifecycle(&mut txn, claims.staging_inode_id)
                .await
                .unwrap()
                .is_none(),
            "lifecycle marker must be removed by abort cleanup"
        );
        let _ = txn.rollback().await;

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_remove_recursive_object_file_stamps_runtime_lifecycle_owner() {
        if fs9_config().s3.is_none() || fs9_config().inline_max_bytes == 0 {
            return;
        }

        let fs = make_fs().await;
        let base = "/test_remove_recursive_object_lifecycle_owner";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/large.bin");
        let data: Vec<u8> = (0..(fs9_config().inline_max_bytes + 1))
            .map(|idx| (idx % 251) as u8)
            .collect();
        fs.write_file(path, &data).await.unwrap();

        let inode = fs.stat(path).await.unwrap();
        assert!(matches!(inode.data, DataRef::Object { .. }));

        let removed = fs.remove_recursive(base).await.unwrap();
        assert_eq!(removed, 2, "directory + child file must both be removed");

        let mut txn = fs.begin_internal().await.unwrap();
        assert!(
            load_inode(&mut txn, inode.id).await.unwrap().is_none(),
            "recursive delete must remove the file inode"
        );
        assert!(
            load_inode(&mut txn, ROOT_INODE).await.unwrap().is_some(),
            "sanity check: filesystem root must remain"
        );
        assert_eq!(
            lifecycle::load_lifecycle(&mut txn, inode.id).await.unwrap(),
            Some(FileLifecycle::Deleting {
                fs_instance_id: fs.runtime_state().fs_instance_id,
                data_ref: inode.data.clone(),
            }),
            "recursive delete must stamp lifecycle ownership with the active fs instance"
        );
        let _ = txn.rollback().await;

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_begin_write_stream_replaces_existing_file() {
        let fs = make_fs().await;
        let base = "/test_begin_write_stream_replace";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/replace.bin");
        fs.write_file(path, b"old-data").await.unwrap();

        let new_data: Vec<u8> = (0..(PAGE_SIZE * 4 + 19))
            .map(|idx| (idx % 251) as u8)
            .collect();
        let mut writer = fs
            .begin_write_stream(
                path,
                FsWriteStreamOptions {
                    expected_size: Some(new_data.len() as u64),
                },
            )
            .await
            .unwrap();
        for chunk in new_data.chunks(8192) {
            writer.write_chunk(chunk).await.unwrap();
        }
        let written = writer.finish().await.unwrap();

        assert_eq!(written, new_data.len());
        assert_eq!(fs.read_file(path).await.unwrap(), new_data);

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_directory() {
        let fs = make_fs().await;
        let base = "/test_rename_dir";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/old_dir")).await;
        fs.write_file(&format!("{base}/old_dir/child.txt"), b"nested")
            .await
            .unwrap();

        fs.rename(&format!("{base}/old_dir"), &format!("{base}/new_dir"))
            .await
            .unwrap();

        assert!(fs.stat(&format!("{base}/old_dir")).await.is_err());
        let children = fs.readdir(&format!("{base}/new_dir")).await.unwrap();
        assert!(
            children.iter().any(|(name, _)| name == "child.txt"),
            "children must follow renamed directory"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_missing_source_enoent() {
        let fs = make_fs().await;
        let err = fs
            .rename("/nonexistent_path_xyz", "/somewhere")
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "missing source should produce ENOENT-equivalent, got: {fs_err}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_missing_dest_parent_enoent_before_einval() {
        let fs = make_fs().await;
        let base = "/test_rename_parent_precedence";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/a")).await;

        let err = fs
            .rename(&format!("{base}/a"), &format!("{base}/a/missing/x"))
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "missing destination parent should produce ENOENT-equivalent before cycle EINVAL, got: {fs_err}"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_cycle_einval() {
        let fs = make_fs().await;
        let base = "/test_rename_cycle";
        cleanup(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/a/b/c")).await;

        let err = fs
            .rename(&format!("{base}/a"), &format!("{base}//a/b/c/moved"))
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::InvalidInput(_)),
            "cycle should produce EINVAL-equivalent, got: {fs_err}"
        );

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_root_rejected() {
        let fs = make_fs().await;
        let err = fs.rename("/", "/newroot").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot rename root"),
            "root rename must be rejected: {msg}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_same_path_missing_source_enoent() {
        let fs = make_fs().await;
        let err = fs
            .rename("/nonexistent_same", "/nonexistent_same")
            .await
            .unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "rename(missing, missing) with identical paths must return ENOENT, got: {fs_err}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_same_path_trailing_slash_enotdir() {
        let fs = make_fs().await;
        let base = "/test_rename_noop";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        let path = &format!("{base}/f.txt");
        fs.write_file(path, b"stable").await.unwrap();

        // trailing slash on destination requires a directory target
        let err = fs.rename(path, &format!("{path}/")).await.unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file should produce ENOTDIR-equivalent, got: {fs_err}"
        );
        let data = fs.read_file(path).await.unwrap();
        assert_eq!(data, b"stable", "failed rename must not corrupt data");

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_missing_dest_leaf_trailing_slash_enotdir() {
        let fs = make_fs().await;
        let base = "/test_rename_missing_leaf_trailing_slash";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        let src = &format!("{base}/f.txt");
        let dst = &format!("{base}/missing/");
        fs.write_file(src, b"stable").await.unwrap();

        let err = fs.rename(src, dst).await.unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file with missing leaf should produce ENOTDIR-equivalent, got: {fs_err}"
        );
        assert_eq!(
            fs.read_file(src).await.unwrap(),
            b"stable",
            "failed rename must not move source file"
        );
        assert!(fs.stat(&format!("{base}/missing")).await.is_err());

        cleanup(&fs, base).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_behavioral_existing_dir_dest_trailing_slash_enotdir() {
        let fs = make_fs().await;
        let base = "/test_rename_existing_dir_trailing_slash";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;
        ensure_dir(&fs, &format!("{base}/existing_dir")).await;
        let src = &format!("{base}/f.txt");
        let dst = &format!("{base}/existing_dir/");
        fs.write_file(src, b"stable").await.unwrap();

        let err = fs.rename(src, dst).await.unwrap_err();
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("expected EmbeddedFsError");
        assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file with existing directory destination should produce ENOTDIR-equivalent, got: {fs_err}"
        );
        assert_eq!(
            fs.read_file(src).await.unwrap(),
            b"stable",
            "failed rename must not move source file"
        );

        cleanup(&fs, base).await;
    }

    /// Regression test for P0 data-loss bug (#1680):
    /// If publish_staged_write commits at TiKV Raft level but the client
    /// observes a timeout, finish() calls abort_staged_write on the
    /// now-published inode. Without the nlink guard, this deletes the
    /// live file's pages and inode.
    #[tokio::test]
    #[ignore]
    async fn test_cleanup_staging_inode_skips_published_file() {
        let fs = make_fs().await;
        let base = "/test_nlink_guard";
        cleanup(&fs, base).await;
        ensure_dir(&fs, base).await;

        let path = &format!("{base}/published.bin");
        let data = b"important-data-must-survive";

        // 1. Allocate a staging inode (nlink=0) and write data into it.
        let mut txn = fs.begin().await.unwrap();
        let inode_id = fs.alloc_inode_id().await.unwrap();
        let mut inode = Inode::new_file(inode_id, 0o644);
        inode.nlink = 0;
        save_inode(&mut txn, &inode).await.unwrap();
        mark_staging_write(&mut txn, inode_id, current_unix_timestamp())
            .await
            .unwrap();
        txn.commit().await.unwrap();

        fs.flush_staged_write_chunk(inode_id, 0, data)
            .await
            .unwrap();

        // 2. Publish the staging inode to the target path (sets nlink=1).
        let written = fs.publish_staged_write(path, inode_id).await.unwrap();
        assert_eq!(written, data.len());

        // 3. Simulate abort-after-ambiguous-commit: call cleanup_staging_inode
        //    on the now-published inode. The nlink guard must prevent deletion.
        fs.cleanup_staging_inode(inode_id).await.unwrap();

        // 4. The published file must still be fully readable.
        let readback = fs.read_file(path).await.unwrap();
        assert_eq!(
            readback, data,
            "cleanup_staging_inode must not delete a published file (nlink > 0)"
        );

        cleanup(&fs, base).await;
    }

    #[test]
    fn test_normalize_completed_parts_sorts_and_rejects_duplicates() {
        let sorted = normalize_completed_parts(vec![
            FsMultipartCompletedPart {
                part_number: 2,
                etag: "etag-2".to_string(),
            },
            FsMultipartCompletedPart {
                part_number: 1,
                etag: "etag-1".to_string(),
            },
        ])
        .expect("parts should normalize");
        assert_eq!(sorted[0].part_number, 1);
        assert_eq!(sorted[1].part_number, 2);

        let err = normalize_completed_parts(vec![
            FsMultipartCompletedPart {
                part_number: 1,
                etag: "etag-1".to_string(),
            },
            FsMultipartCompletedPart {
                part_number: 1,
                etag: "etag-1b".to_string(),
            },
        ])
        .expect_err("duplicate part numbers must fail");
        assert!(err.to_string().contains("duplicate multipart part number"));
    }

    #[test]
    fn test_stream_routing_requires_explicit_size_for_direct_object() {
        let object_min = u64::try_from(fs9_config().object_min_bytes).unwrap_or(u64::MAX);
        let has_object_storage = fs9_config().s3.is_some();

        assert!(!should_use_direct_object_stream(None, has_object_storage));
        if object_min > 0 {
            assert!(!should_use_direct_object_stream(
                Some(object_min - 1),
                has_object_storage
            ));
        }
        assert_eq!(
            should_use_direct_object_stream(Some(object_min), has_object_storage),
            has_object_storage
        );
    }

    #[test]
    fn test_stream_spool_routes_non_inline_sizes_to_object() {
        let inline_max = fs9_config().inline_max_bytes;

        assert!(!should_route_stream_spool_to_object(0));
        if inline_max > 0 {
            assert!(should_route_stream_spool_to_inline(inline_max as u64));
            assert!(!should_route_stream_spool_to_object(inline_max as u64));
            assert!(should_route_stream_spool_to_object((inline_max as u64) + 1));
        }
    }
}
