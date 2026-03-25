use crate::extensions::fs::backend::{
    batch_inline_read_entry_too_large_error, batch_inline_read_payload_too_large_error,
    FsBatchWriteEntry, FsBatchWriteFile, FsBatchWriteGroupedResult, FsCreateUpload,
    FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest, FsRecursiveReaddirOptions,
    FsStorage, FsWriteStream, FsWriteStreamOptions,
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
use crate::extensions::fs::notify::{
    notify_metrics_for_keyspace, EventRing, FsEventBuilder, FsEventType,
};
use crate::extensions::fs::s3::FsS3Client;
use crate::extensions::fs::upload_token::{
    normalized_path_hash_hex, sign_upload_token, verify_upload_token, UploadTokenClaims,
};
use crate::txn::txn_put;
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
use tracing::{debug, warn};

mod ops_impl;
mod read_impl;
mod write_impl;

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
    notify_ring: Arc<EventRing>,
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

type DirectoryEntries = Vec<(String, Inode)>;
type DirectoryEntriesResult = Result<DirectoryEntries>;

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
        txn: Box<Transaction>,
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
        // Get-or-create the EventRing for this keyspace from the global registry.
        // This is the owner-side registration point; the TVF read path uses
        // get_event_ring() which only returns existing rings.
        let notify_ring = crate::extensions::fs::notify::get_or_create_event_ring(&keyspace);

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
            notify_ring,
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

    /// Emit a single fs event to the in-memory ring AND enqueue for Redis
    /// persistence after a successful TiKV commit.
    /// Silently drops if the ring lock is poisoned (Hard Contract #4).
    /// Records emit metrics for observability.
    fn emit_event(&self, builder: FsEventBuilder) {
        // Persist to Redis Streams so fs9_events() TVF / `fs watch` can see it.
        self.persist_events_async(vec![builder.clone()]);
        let metrics = notify_metrics_for_keyspace(&self.keyspace);
        let event_type = builder.event_type;
        match self.notify_ring.push(builder) {
            Ok(_) => metrics.record_emit(&event_type),
            Err(_) => metrics.record_emit_error(),
        }
    }

    /// Emit multiple fs events atomically to the in-memory ring AND enqueue
    /// for Redis persistence after commit.
    /// Applies commit-scope coalescing: same path → keep only last event (Hard Contract #2).
    fn emit_events(&self, builders: Vec<FsEventBuilder>) {
        if builders.is_empty() {
            return;
        }
        // Coalesce: same path within a commit → keep last state only (HC #2).
        let input_count = builders.len();
        let mut coalesced: std::collections::HashMap<String, FsEventBuilder> =
            std::collections::HashMap::with_capacity(input_count);
        for b in builders {
            coalesced.insert(b.path.clone(), b);
        }
        let final_builders: Vec<FsEventBuilder> = coalesced.into_values().collect();
        // Persist to Redis Streams so fs9_events() TVF / `fs watch` can see them.
        self.persist_events_async(final_builders.clone());
        let metrics = notify_metrics_for_keyspace(&self.keyspace);
        let suppressed = (input_count - final_builders.len()) as u64;
        if suppressed > 0 {
            metrics.record_coalesced(suppressed);
        }
        // Capture event types before push_batch consumes the builders.
        let event_types: Vec<FsEventType> = final_builders.iter().map(|b| b.event_type).collect();
        match self.notify_ring.push_batch(final_builders) {
            Ok(_) => {
                for et in &event_types {
                    metrics.record_emit(et);
                }
            }
            Err(_) => metrics.record_emit_error(),
        }
    }

    /// Enqueue multiple events for async Redis persistence (zero I/O on caller).
    fn persist_events_async(&self, builders: Vec<FsEventBuilder>) {
        crate::extensions::fs::notify::enqueue_persist_events(&self.keyspace, builders);
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
                    let mut inode = Inode::new_file(inode_id, file.mode.unwrap_or(0o644));
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
        let mut event_builders: Vec<FsEventBuilder> = Vec::with_capacity(files.len());

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
            let mut is_overwrite = false;
            if let Some(existing_inode_id) = lookup(&mut txn, parent_inode, &name).await? {
                let existing_inode = load_inode(&mut txn, existing_inode_id)
                    .await?
                    .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(&file.path)))?;
                if existing_inode.is_directory() {
                    return Err(anyhow!(EmbeddedFsError::is_directory(&file.path)));
                }
                if existing_inode.is_symlink() {
                    return Err(anyhow!(EmbeddedFsError::InvalidInput(
                        "cannot write to symlink as file; use readlink".to_string()
                    )));
                }

                is_overwrite = true;
                publish_generation = existing_inode.generation.checked_add(1).ok_or_else(|| {
                    anyhow!(EmbeddedFsError::internal("inode generation overflow"))
                })?;
                // Preserve existing file's mode on overwrite (new-inode-only semantics).
                staging_inode.mode = existing_inode.mode;
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

            event_builders.push(FsEventBuilder {
                event_type: if is_overwrite {
                    FsEventType::Write
                } else {
                    FsEventType::Create
                },
                path: normalize_path(&file.path),
                old_path: None,
                inode: file.staging_inode_id,
                parent_inode,
                generation: staging_inode.generation,
                is_dir: false,
                size: staging_inode.size,
            });
        }

        save_bundle_manifest(&mut txn, manifest).await?;
        txn.commit().await?;

        self.emit_events(event_builders);

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
}

/// Result of `prepare_replace_file_txn`, carrying parent inode for event emission.
struct PreparedFile {
    inode_id: u64,
    inode: Inode,
    parent_inode: u64,
    /// True if the inode was freshly allocated in this txn (generation == 1 before bump).
    is_new: bool,
}

async fn prepare_replace_file_txn(
    fs: &EmbeddedPageFs,
    txn: &mut Transaction,
    path: &str,
    mode: Option<u32>,
) -> Result<PreparedFile> {
    let (parent_inode, name) = ensure_parents_and_resolve_parent(fs, txn, path).await?;

    if let Some(existing_inode_id) = lookup(txn, parent_inode, &name).await? {
        let mut inode = load_inode(txn, existing_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "cannot write to symlink as file; use readlink".to_string()
            )));
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
        Ok(PreparedFile {
            inode_id: existing_inode_id,
            inode,
            parent_inode,
            is_new: false,
        })
    } else {
        let inode_id = fs.alloc_inode_id().await?;
        let mut inode = Inode::new_file(inode_id, mode.unwrap_or(0o644));
        inode.data = DataRef::None;
        link(txn, parent_inode, &name, inode_id).await?;
        Ok(PreparedFile {
            inode_id,
            inode,
            parent_inode,
            is_new: true,
        })
    }
}

async fn prepare_write_at_file_txn(
    fs: &EmbeddedPageFs,
    txn: &mut Transaction,
    path: &str,
    mode: Option<u32>,
) -> Result<PreparedFile> {
    let (parent_inode, name) = ensure_parents_and_resolve_parent(fs, txn, path).await?;

    if let Some(existing_inode_id) = lookup(txn, parent_inode, &name).await? {
        let inode = load_inode(txn, existing_inode_id)
            .await?
            .ok_or_else(|| anyhow!(EmbeddedFsError::not_found(path)))?;

        if inode.is_directory() {
            return Err(anyhow!(EmbeddedFsError::is_directory(path)));
        }
        if inode.is_symlink() {
            return Err(anyhow!(EmbeddedFsError::InvalidInput(
                "cannot write to symlink as file; use readlink".to_string()
            )));
        }

        Ok(PreparedFile {
            inode_id: existing_inode_id,
            inode,
            parent_inode,
            is_new: false,
        })
    } else {
        let inode_id = fs.alloc_inode_id().await?;
        let inode = Inode::new_file(inode_id, mode.unwrap_or(0o644));
        save_inode(txn, &inode).await?;
        link(txn, parent_inode, &name, inode_id).await?;
        Ok(PreparedFile {
            inode_id,
            inode,
            parent_inode,
            is_new: true,
        })
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
        .join(format!("format-{}", FS9_SPOOL_LAYOUT_VERSION))
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
    if sb.format_version > FS9_FORMAT_VERSION_MAX {
        return Err(anyhow!(
            "fs9: storage format version {} is newer than this binary supports (max {}). \
             Upgrade db9-server to access this keyspace.",
            sb.format_version,
            FS9_FORMAT_VERSION_MAX
        ));
    }
    if sb.format_version < FS9_FORMAT_VERSION_MIN {
        return Err(anyhow!(
            "fs9: storage format version {} is too old (min supported: {}). \
             Recreate the fs9 keyspace with the current format.",
            sb.format_version,
            FS9_FORMAT_VERSION_MIN
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
    txn_put(txn, keys::superblock_key(), data).await?;
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
    txn_put(txn, key.to_vec(), next.to_be_bytes().to_vec()).await?;
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
        // Diagnostic classification: distinguish version skew from data corruption.
        // This is NOT a compatibility gate — superblock format_version is the sole gate.
        if let Ok(raw) = serde_json::from_slice::<serde_json::Value>(data) {
            if let Some(t) = raw.get("inode_type").and_then(|v| v.as_str()) {
                if !matches!(t, "File" | "Directory" | "Symlink") {
                    return anyhow!(
                        "fs9: inode {inode_id} has unrecognized type \"{t}\". \
                         This keyspace was written by a newer version of db9-server. \
                         Upgrade db9-server to access this keyspace."
                    );
                }
            }
        }
        anyhow!("fs9: corrupt inode data for inode {inode_id}: {err}.")
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
    txn_put(txn, keys::inode_key(inode.id), data).await?;
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
    txn_put(
        txn,
        keys::dir_entry_key(parent_inode, name),
        child_inode.to_be_bytes().to_vec(),
    )
    .await?;
    touch_directory_entry_parent(txn, parent_inode).await?;
    Ok(())
}

async fn unlink(txn: &mut Transaction, parent_inode: u64, name: &str) -> Result<()> {
    txn.delete(keys::dir_entry_key(parent_inode, name)).await?;
    touch_directory_entry_parent(txn, parent_inode).await?;
    Ok(())
}

async fn touch_directory_entry_parent(txn: &mut Transaction, parent_inode: u64) -> Result<()> {
    let mut inode = load_inode(txn, parent_inode).await?.ok_or_else(|| {
        anyhow!(EmbeddedFsError::internal(
            "parent inode missing during directory update"
        ))
    })?;
    if !inode.is_directory() {
        return Err(anyhow!(EmbeddedFsError::internal(
            "parent inode is not a directory during directory update",
        )));
    }
    bump_inode_generation(&mut inode)?;
    inode.touch_mtime();
    save_inode(txn, &inode).await?;
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
    txn_put(
        txn,
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
    txn_put(txn, keys::orphan_inode_key(inode_id), Vec::new()).await?;
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
    txn_put(txn, keys::page_key(inode_id, page_num), page_data).await?;
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
mod bench_grouped_write;
#[cfg(test)]
mod tests;
