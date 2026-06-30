use super::*;
use crate::storage::backpressure::tikv_op;
use crate::worker::types::{
    HnswS3DbPrefixCleanupIntent, HnswS3GraphUploadIntent, StorageScanBgState,
    StorageScanBgStateStatus, StorageScanCapacityToken, TaskDescriptorV2, TaskPayloadV2,
    TaskQueueEntry, TaskRegistryEntry, TaskType, WorkerClaim, WorkerExecutorLease,
    WorkerExecutorLeaseResult, TASK_TYPE_CRON,
};

/// A V2 index row decoded into its identity plus the reconstructed V2 due-queue
/// key it points at. Carries no command payload.
#[derive(Debug, Clone)]
pub struct WqIndexRow {
    pub due_key: Vec<u8>,
    pub keyspace: String,
    pub db_id: u64,
    pub task_type: u8,
    pub task_id: i64,
    pub fire_time_ms: i64,
}

#[derive(Debug, Clone)]
pub struct DroppingDbIntent {
    pub keyspace: String,
    pub db_id: u64,
    pub created_at_ms: i64,
}

const GC_INSTANCE_STATE_BASE_VALUE_LEN: usize = 17;
const GC_INSTANCE_STATE_VALUE_LEN: usize = 18;
const LEGACY_GC_INSTANCE_STATE_VALUE_LEN: usize = 25;
const GC_INSTANCE_STATE_PUBLISH_MODE_OFFSET: usize = 17;
const DROPPING_DB_INTENT_VALUE_LEN: usize = 8;
const LIFECYCLE_PROCESS_LIVENESS_VALUE_VERSION: u8 = 1;
const LIFECYCLE_TENANT_VALUE_VERSION: u8 = 1;
const LIFECYCLE_TENANT_VALUE_LEN: usize = 18;
const WORKER_QUEUE_SCHEMA_V2: u8 = 2;
const STORAGE_SCAN_BG_STATE_VALUE_VERSION: u8 = 1;
const STORAGE_SCAN_BG_STATE_VALUE_LEN: usize = 48;
const STORAGE_SCAN_CAPACITY_TOKEN_VALUE_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DroppedDbEnqueueFence {
    None,
    DroppingIntent,
    Tombstone,
}

impl DroppedDbEnqueueFence {
    fn is_fenced(self) -> bool {
        !matches!(self, Self::None)
    }
}

fn decode_dropping_db_intent_created_at(value: &[u8]) -> i64 {
    if value.len() == DROPPING_DB_INTENT_VALUE_LEN {
        let bytes: [u8; DROPPING_DB_INTENT_VALUE_LEN] = value.try_into().unwrap_or_default();
        i64::from_be_bytes(bytes)
    } else {
        0
    }
}

/// Published GC instance state read back from the configured GC registry store.
#[derive(Clone)]
pub struct GcInstanceState {
    pub instance_id: String,
    pub min_start_ts: Option<u64>,
    pub updated_at_version: u64,
    /// Publish protocol advertised by this SQL-serving process. Legacy rows
    /// without this trailing byte decode as `OldOnly`, which fail-closed blocks
    /// any future new-only GC registry cutover.
    pub publish_mode: GcPublishMode,
    /// Legacy 25-byte row compatibility during mixed-version rollout.
    /// New-format rows do not publish this timeout tail.
    pub legacy_max_untracked_timeout_sec: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPublishMode {
    OldOnly,
    DualWrite,
    NewPrimary,
}

impl GcPublishMode {
    fn encode(self) -> u8 {
        match self {
            Self::OldOnly => 0,
            Self::DualWrite => 1,
            Self::NewPrimary => 2,
        }
    }

    fn decode(byte: u8) -> Self {
        match byte {
            1 => Self::DualWrite,
            2 => Self::NewPrimary,
            _ => Self::OldOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleTenantStatus {
    Live,
    Dropped,
}

impl LifecycleTenantStatus {
    fn encode(self) -> u8 {
        match self {
            Self::Live => 1,
            Self::Dropped => 2,
        }
    }

    fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Live),
            2 => Some(Self::Dropped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct LifecycleTenantRecord {
    pub keyspace: String,
    pub db_id: u64,
    pub incarnation: u64,
    pub status: LifecycleTenantStatus,
    pub updated_at_version: u64,
}

fn encode_gc_instance_state_value(
    min_start_ts: Option<u64>,
    updated_at_version: u64,
    publish_mode: GcPublishMode,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(GC_INSTANCE_STATE_VALUE_LEN);
    match min_start_ts {
        Some(ts) => {
            data.push(1);
            data.extend_from_slice(&ts.to_be_bytes());
        }
        None => {
            data.push(0);
            data.extend_from_slice(&0u64.to_be_bytes());
        }
    }
    data.extend_from_slice(&updated_at_version.to_be_bytes());
    data.push(publish_mode.encode());
    data
}

fn decode_gc_instance_state_value(
    val: &[u8],
) -> Option<(Option<u64>, u64, GcPublishMode, Option<u64>)> {
    if val.len() < GC_INSTANCE_STATE_BASE_VALUE_LEN {
        return None;
    }
    let has_min = val[0] == 1;
    let min_ts = if has_min {
        Some(u64::from_be_bytes(val[1..9].try_into().unwrap_or([0; 8])))
    } else {
        None
    };
    let updated_at = u64::from_be_bytes(val[9..17].try_into().unwrap_or([0; 8]));
    let legacy_max_untracked_timeout_sec = if val.len() >= LEGACY_GC_INSTANCE_STATE_VALUE_LEN {
        Some(u64::from_be_bytes(val[17..25].try_into().unwrap_or([0; 8])))
    } else {
        None
    };
    let publish_mode = if val.len() > LEGACY_GC_INSTANCE_STATE_VALUE_LEN {
        GcPublishMode::decode(val[LEGACY_GC_INSTANCE_STATE_VALUE_LEN])
    } else if val.len() > GC_INSTANCE_STATE_PUBLISH_MODE_OFFSET
        && val.len() < LEGACY_GC_INSTANCE_STATE_VALUE_LEN
    {
        GcPublishMode::decode(val[GC_INSTANCE_STATE_PUBLISH_MODE_OFFSET])
    } else {
        GcPublishMode::OldOnly
    };
    Some((
        min_ts,
        updated_at,
        publish_mode,
        legacy_max_untracked_timeout_sec,
    ))
}

fn encode_lifecycle_process_liveness_value(
    updated_at_version: u64,
    self_fence_after_ms: u64,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(17);
    data.push(LIFECYCLE_PROCESS_LIVENESS_VALUE_VERSION);
    data.extend_from_slice(&updated_at_version.to_be_bytes());
    data.extend_from_slice(&self_fence_after_ms.to_be_bytes());
    data
}

fn encode_lifecycle_tenant_value(
    incarnation: u64,
    status: LifecycleTenantStatus,
    updated_at_version: u64,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(LIFECYCLE_TENANT_VALUE_LEN);
    data.push(LIFECYCLE_TENANT_VALUE_VERSION);
    data.push(status.encode());
    data.extend_from_slice(&incarnation.to_be_bytes());
    data.extend_from_slice(&updated_at_version.to_be_bytes());
    data
}

#[allow(dead_code)]
fn decode_lifecycle_tenant_value(value: &[u8]) -> Option<(u64, LifecycleTenantStatus, u64)> {
    if value.len() != LIFECYCLE_TENANT_VALUE_LEN || value[0] != LIFECYCLE_TENANT_VALUE_VERSION {
        return None;
    }
    let status = LifecycleTenantStatus::decode(value[1])?;
    let incarnation = u64::from_be_bytes(value[2..10].try_into().ok()?);
    let updated_at_version = u64::from_be_bytes(value[10..18].try_into().ok()?);
    Some((incarnation, status, updated_at_version))
}

fn encode_storage_scan_bg_state_value(state: &StorageScanBgState) -> Vec<u8> {
    let mut data = Vec::with_capacity(STORAGE_SCAN_BG_STATE_VALUE_LEN);
    data.push(STORAGE_SCAN_BG_STATE_VALUE_VERSION);
    data.push(state.status.encode());
    data.extend_from_slice(&state.tenant_incarnation.to_be_bytes());
    data.extend_from_slice(&state.work_id.to_be_bytes());
    data.extend_from_slice(&state.run_after_ms.to_be_bytes());
    data.extend_from_slice(&state.lease_until_ms.to_be_bytes());
    data.extend_from_slice(&state.attempt.to_be_bytes());
    data.extend_from_slice(&state.last_done_work_id.to_be_bytes());
    data.extend_from_slice(&state.capacity_token_id.unwrap_or(u16::MAX).to_be_bytes());
    data
}

fn decode_storage_scan_bg_state_value(
    keyspace: &str,
    db_id: u64,
    value: &[u8],
) -> Option<StorageScanBgState> {
    if value.len() != STORAGE_SCAN_BG_STATE_VALUE_LEN
        || value[0] != STORAGE_SCAN_BG_STATE_VALUE_VERSION
    {
        return None;
    }
    let token_id = u16::from_be_bytes(value[46..48].try_into().ok()?);
    Some(StorageScanBgState {
        keyspace: keyspace.to_string(),
        db_id,
        status: StorageScanBgStateStatus::decode(value[1])?,
        tenant_incarnation: u64::from_be_bytes(value[2..10].try_into().ok()?),
        work_id: i64::from_be_bytes(value[10..18].try_into().ok()?),
        run_after_ms: i64::from_be_bytes(value[18..26].try_into().ok()?),
        lease_until_ms: i64::from_be_bytes(value[26..34].try_into().ok()?),
        attempt: u32::from_be_bytes(value[34..38].try_into().ok()?),
        last_done_work_id: i64::from_be_bytes(value[38..46].try_into().ok()?),
        capacity_token_id: (token_id != u16::MAX).then_some(token_id),
    })
}

fn encode_storage_scan_capacity_token_value(token: &StorageScanCapacityToken) -> Vec<u8> {
    let mut data = Vec::with_capacity(1 + 2 + token.keyspace.len() + 8 + 8 + 8 + 4 + 8);
    data.push(STORAGE_SCAN_CAPACITY_TOKEN_VALUE_VERSION);
    data.extend_from_slice(&(token.keyspace.len() as u16).to_be_bytes());
    data.extend_from_slice(token.keyspace.as_bytes());
    data.extend_from_slice(&token.db_id.to_be_bytes());
    data.extend_from_slice(&token.tenant_incarnation.to_be_bytes());
    data.extend_from_slice(&token.work_id.to_be_bytes());
    data.extend_from_slice(&token.attempt.to_be_bytes());
    data.extend_from_slice(&token.lease_until_ms.to_be_bytes());
    data
}

fn decode_storage_scan_capacity_token_value(value: &[u8]) -> Option<StorageScanCapacityToken> {
    if value.len() < 1 + 2 + 8 + 8 + 8 + 4 + 8
        || value[0] != STORAGE_SCAN_CAPACITY_TOKEN_VALUE_VERSION
    {
        return None;
    }
    let keyspace_len = u16::from_be_bytes([value[1], value[2]]) as usize;
    let mut idx = 3;
    if idx + keyspace_len + 8 + 8 + 8 + 4 + 8 != value.len() {
        return None;
    }
    let keyspace = std::str::from_utf8(&value[idx..idx + keyspace_len])
        .ok()?
        .to_string();
    idx += keyspace_len;
    let db_id = u64::from_be_bytes(value[idx..idx + 8].try_into().ok()?);
    idx += 8;
    let tenant_incarnation = u64::from_be_bytes(value[idx..idx + 8].try_into().ok()?);
    idx += 8;
    let work_id = i64::from_be_bytes(value[idx..idx + 8].try_into().ok()?);
    idx += 8;
    let attempt = u32::from_be_bytes(value[idx..idx + 4].try_into().ok()?);
    idx += 4;
    let lease_until_ms = i64::from_be_bytes(value[idx..idx + 8].try_into().ok()?);
    Some(StorageScanCapacityToken {
        keyspace,
        db_id,
        tenant_incarnation,
        work_id,
        attempt,
        lease_until_ms,
    })
}

fn tikv_error_is_worker_claim_contention(err: &tikv_client::Error) -> bool {
    match err {
        tikv_client::Error::ResolveLockError(_) => true,
        tikv_client::Error::KeyError(key_error) => {
            key_error.locked.is_some()
                || key_error.conflict.is_some()
                || key_error.deadlock.is_some()
        }
        tikv_client::Error::PessimisticLockError { inner, .. } => {
            tikv_error_is_worker_claim_contention(inner)
        }
        tikv_client::Error::UndeterminedError(inner) => {
            tikv_error_is_worker_claim_contention(inner)
        }
        tikv_client::Error::ExtractedErrors(errors)
        | tikv_client::Error::MultipleKeyErrors(errors) => {
            !errors.is_empty() && errors.iter().all(tikv_error_is_worker_claim_contention)
        }
        _ => err.is_lock_conflict(),
    }
}

fn gc_instance_state_scan_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    end.push(0xFF);
    end
}

fn worker_queue_schema_is_v2(value: Option<&[u8]>) -> bool {
    value.is_some_and(|bytes| bytes.first().copied() == Some(WORKER_QUEUE_SCHEMA_V2))
}

impl TikvStore {
    // ========================================================================
    // Registry methods
    // ========================================================================

    async fn put_worker_registry(
        &self,
        txn: &mut Transaction,
        entry: &TaskRegistryEntry,
    ) -> Result<()> {
        let key = self.key(&encode_worker_registry_key(&entry.keyspace, entry.db_id));
        let data =
            bincode::serialize(entry).context("Failed to serialize worker registry entry")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Best-effort inventory repair for maintenance paths.
    ///
    /// This deliberately uses non-locking fence reads: registry rows are repair
    /// hints, not task truth, and taking a pessimistic lock on the per-DB
    /// tombstone key for every live-database repair turns ordinary producers
    /// into a hot lock. If a DROP races this write after the snapshot check, the
    /// durable tombstone plus registry sweep will clean the stale hint.
    pub async fn ensure_worker_registry_unless_db_dropped_best_effort(
        &self,
        txn: &mut Transaction,
        entry: &TaskRegistryEntry,
    ) -> Result<bool> {
        match self
            .dropped_db_enqueue_fence(txn, &entry.keyspace, entry.db_id)
            .await?
        {
            DroppedDbEnqueueFence::None => {
                if self
                    .get_worker_registry(txn, &entry.keyspace, entry.db_id)
                    .await?
                    .is_some()
                {
                    return Ok(false);
                }
                self.put_worker_registry(txn, entry).await?;
                Ok(true)
            }
            DroppedDbEnqueueFence::Tombstone => Ok(false),
            DroppedDbEnqueueFence::DroppingIntent => Err(anyhow!(
                "database inventory registry write deferred by in-flight DROP intent for keyspace='{}' db_id={}",
                entry.keyspace,
                entry.db_id
            )),
        }
    }

    pub async fn get_worker_registry(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Option<TaskRegistryEntry>> {
        let key = self.key(&encode_worker_registry_key(keyspace, db_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data)
                    .context("Failed to deserialize worker registry entry")?,
            )),
            None => Ok(None),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn list_worker_registry(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<TaskRegistryEntry>> {
        let prefix = encode_worker_registry_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut entries = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry: TaskRegistryEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize worker registry entry")?;
            entries.push(entry);
        }
        Ok(entries)
    }

    pub async fn scan_worker_registry_page(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<TaskRegistryEntry>, Option<Vec<u8>>)> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let prefix = encode_worker_registry_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0x00);
                next_start
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(Some(limit))).await)?;

        let mut entries = Vec::new();
        let mut last_key = None;
        let mut scanned = 0usize;
        for pair in pairs {
            scanned += 1;
            let key: &[u8] = pair.key().as_ref().into();
            last_key = Some(key.to_vec());
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry: TaskRegistryEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize worker registry entry")?;
            entries.push(entry);
        }

        let next_cursor = if scanned == limit { last_key } else { None };
        Ok((entries, next_cursor))
    }

    async fn delete_worker_registry(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_registry_key(keyspace, db_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Reap all worker queue entries for a database, then delete its registry
    /// inventory row. The registry row is intentionally retained if queue
    /// cleanup fails so bounded maintenance can retry from durable inventory.
    ///
    /// FIRST writes the durable dropped-DB tombstone (committed in its own system
    /// txn) so the cross-store DROP-vs-enqueue race is closed proactively: a cron
    /// next-fire enqueue that takes `get_for_update` on the tombstone in the same
    /// system txn as its `put_task_v2` either (a) started after this commit and
    /// observes the tombstone → suppresses, or (b) raced this commit and
    /// conflicts on the tombstone key → at most one of {reap, enqueue} commits,
    /// and on enqueue retry the tombstone is present → suppresses. Either way no
    /// stale `_sys_worker` next-fire row can survive for the dropped db_id. The
    /// tombstone is written BEFORE the queue scan so it fences enqueues that race
    /// the reap's own delete window.
    pub async fn reap_db_queue_entries_then_delete_worker_registry(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<usize> {
        self.put_dropped_db_tombstone_with_retry(keyspace, db_id)
            .await?;

        let deleted = self.reap_db_queue_entries(keyspace, db_id).await?;

        let mut txn = self.begin().await?;
        self.delete_storage_scan_bg_state(&mut txn, keyspace, db_id)
            .await?;
        self.delete_worker_registry(&mut txn, keyspace, db_id)
            .await?;
        txn.commit().await?;

        Ok(deleted)
    }

    // ========================================================================
    // Dropped-DB tombstone (cross-store DROP-vs-enqueue fence, issue #2628)
    //
    // The DB liveness fence (`database_alive_for_update`) lives in the TENANT
    // store, but every cron next-fire enqueue commits in the SYSTEM store, so a
    // single-txn fence across the two is impossible. These helpers add a durable
    // tombstone IN THE SYSTEM STORE so the DROP-reap and the enqueue can conflict
    // within one store: the reap PUTs the tombstone and the enqueue takes
    // `get_for_update` on it in the SAME system txn as `put_task_v2`. Under
    // pessimistic txns the two writers serialize — the orphan next-fire row is
    // made impossible, not merely self-healing. `db_id` is monotonic /
    // non-recycled, so the tombstone is safe to keep forever.
    // ========================================================================

    /// Write the durable dropped-DB tombstone in the SYSTEM store. Idempotent:
    /// re-running DROP's reap (or a sweep retry) just rewrites the same marker.
    pub async fn put_dropped_db_tombstone(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_dropped_db_tombstone_key(keyspace, db_id));
        // Value carries no information; presence is the whole signal.
        txn_put(txn, key, vec![1u8]).await?;
        Ok(())
    }

    pub async fn put_dropping_db_intent(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        created_at_ms: i64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_dropping_db_intent_key(keyspace, db_id));
        txn_put(txn, key, created_at_ms.to_be_bytes().to_vec()).await?;
        Ok(())
    }

    pub async fn delete_dropping_db_intent(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_dropping_db_intent_key(keyspace, db_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn delete_dropping_db_intent_if_created_at(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        expected_created_at_ms: i64,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_dropping_db_intent_key(keyspace, db_id));
        let Some(value) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            return Ok(false);
        };
        if decode_dropping_db_intent_created_at(&value) != expected_created_at_ms {
            return Ok(false);
        }
        txn_delete(txn, key).await?;
        Ok(true)
    }

    pub async fn put_dropping_db_intent_with_retry(
        &self,
        keyspace: &str,
        db_id: u64,
        created_at_ms: i64,
    ) -> Result<()> {
        const INTENT_RETRY_ATTEMPTS: usize = 8;

        let mut last_error = None;
        for attempt in 0..INTENT_RETRY_ATTEMPTS {
            let mut txn = self.begin().await?;
            let put_result = self
                .put_dropping_db_intent(&mut txn, keyspace, db_id, created_at_ms)
                .await;
            let result = match put_result {
                Ok(()) => txn.commit().await.map_err(anyhow::Error::from),
                Err(e) => {
                    txn.rollback().await.ok();
                    Err(e)
                }
            };
            match result {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_error = Some(e);
                    if attempt + 1 < INTENT_RETRY_ATTEMPTS {
                        let backoff_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("dropping-DB intent publish failed")))
            .with_context(|| {
                format!(
                    "failed to publish dropping-DB intent for keyspace='{keyspace}' db_id={db_id}"
                )
            })
    }

    pub async fn delete_dropping_db_intent_if_created_at_with_retry(
        &self,
        keyspace: &str,
        db_id: u64,
        expected_created_at_ms: i64,
    ) -> Result<bool> {
        const INTENT_RETRY_ATTEMPTS: usize = 8;

        let mut last_error = None;
        for attempt in 0..INTENT_RETRY_ATTEMPTS {
            let mut txn = self.begin().await?;
            let delete_result = self
                .delete_dropping_db_intent_if_created_at(
                    &mut txn,
                    keyspace,
                    db_id,
                    expected_created_at_ms,
                )
                .await;
            let result = match delete_result {
                Ok(deleted) => txn
                    .commit()
                    .await
                    .map(|_| deleted)
                    .map_err(anyhow::Error::from),
                Err(e) => {
                    txn.rollback().await.ok();
                    Err(e)
                }
            };
            match result {
                Ok(deleted) => return Ok(deleted),
                Err(e) => {
                    last_error = Some(e);
                    if attempt + 1 < INTENT_RETRY_ATTEMPTS {
                        let backoff_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("dropping-DB intent conditional delete failed")))
            .with_context(|| {
                format!(
                    "failed to conditionally delete dropping-DB intent for keyspace='{keyspace}' db_id={db_id}"
                )
            })
    }

    pub async fn finalize_dropping_db_intent_with_retry(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        const INTENT_RETRY_ATTEMPTS: usize = 8;

        let mut last_error = None;
        for attempt in 0..INTENT_RETRY_ATTEMPTS {
            let mut txn = self.begin().await?;
            let finalize_result = async {
                self.put_dropped_db_tombstone(&mut txn, keyspace, db_id)
                    .await?;
                self.delete_dropping_db_intent(&mut txn, keyspace, db_id)
                    .await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            let result = match finalize_result {
                Ok(()) => txn.commit().await.map_err(anyhow::Error::from),
                Err(e) => {
                    txn.rollback().await.ok();
                    Err(e)
                }
            };
            match result {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_error = Some(e);
                    if attempt + 1 < INTENT_RETRY_ATTEMPTS {
                        let backoff_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("dropping-DB intent finalize failed")))
            .with_context(|| {
                format!(
                    "failed to finalize dropping-DB intent for keyspace='{keyspace}' db_id={db_id}"
                )
            })
    }

    /// Publish the dropped-DB tombstone with bounded retry before any slow
    /// post-DROP cleanup. If a producer wins the first tombstone contention, the
    /// retry observes the now-serialized store state and installs the fence before
    /// queue reap continues.
    pub async fn put_dropped_db_tombstone_with_retry(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        const TOMBSTONE_RETRY_ATTEMPTS: usize = 8;

        let mut last_error = None;
        for attempt in 0..TOMBSTONE_RETRY_ATTEMPTS {
            let mut txn = self.begin().await?;
            let put_result = self
                .put_dropped_db_tombstone(&mut txn, keyspace, db_id)
                .await;
            let result = match put_result {
                Ok(()) => txn.commit().await.map_err(anyhow::Error::from),
                Err(e) => {
                    txn.rollback().await.ok();
                    Err(e)
                }
            };
            match result {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_error = Some(e);
                    if attempt + 1 < TOMBSTONE_RETRY_ATTEMPTS {
                        let backoff_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("dropped-DB tombstone publish failed")))
            .with_context(|| {
                format!(
                    "failed to publish dropped-DB tombstone for keyspace='{keyspace}' db_id={db_id}"
                )
            })
    }

    pub(crate) async fn dropped_db_enqueue_fence_for_update(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<DroppedDbEnqueueFence> {
        let tombstone_key = self.key(&encode_worker_dropped_db_tombstone_key(keyspace, db_id));
        if tikv_op!(txn.get_for_update(tombstone_key).await)?.is_some() {
            return Ok(DroppedDbEnqueueFence::Tombstone);
        }
        let intent_key = self.key(&encode_worker_dropping_db_intent_key(keyspace, db_id));
        if tikv_op!(txn.get_for_update(intent_key).await)?.is_some() {
            return Ok(DroppedDbEnqueueFence::DroppingIntent);
        }
        Ok(DroppedDbEnqueueFence::None)
    }

    async fn dropped_db_enqueue_fence(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<DroppedDbEnqueueFence> {
        let tombstone_key = self.key(&encode_worker_dropped_db_tombstone_key(keyspace, db_id));
        if tikv_op!(txn.get(tombstone_key).await)?.is_some() {
            return Ok(DroppedDbEnqueueFence::Tombstone);
        }
        let intent_key = self.key(&encode_worker_dropping_db_intent_key(keyspace, db_id));
        if tikv_op!(txn.get(intent_key).await)?.is_some() {
            return Ok(DroppedDbEnqueueFence::DroppingIntent);
        }
        Ok(DroppedDbEnqueueFence::None)
    }

    pub async fn dropped_db_enqueue_fence_exists_for_update(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<bool> {
        Ok(self
            .dropped_db_enqueue_fence_for_update(txn, keyspace, db_id)
            .await?
            .is_fenced())
    }

    /// Terminal dropped-DB fence only.
    ///
    /// This deliberately does NOT treat a pre-commit dropping intent as terminal:
    /// DROP may still fail or roll back, and committed one-shot work such as
    /// AsyncTrigger has no tenant-local outbox in this PR. If DROP really
    /// commits, it publishes the tombstone before reaping V2 queue/index rows.
    #[allow(dead_code)]
    pub async fn dropped_db_tombstone_exists_for_update(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<bool> {
        let tombstone_key = self.key(&encode_worker_dropped_db_tombstone_key(keyspace, db_id));
        Ok(tikv_op!(txn.get_for_update(tombstone_key).await)?.is_some())
    }

    pub async fn scan_dropping_db_intents_page(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<DroppingDbIntent>, Option<Vec<u8>>)> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let prefix = encode_worker_dropping_db_intent_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0x00);
                next_start
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(Some(limit))).await)?;
        let mut intents = Vec::new();
        let mut last_key = None;
        let mut scanned = 0usize;
        for pair in pairs {
            scanned += 1;
            let key: &[u8] = pair.key().as_ref().into();
            last_key = Some(key.to_vec());
            let Some((keyspace, db_id)) = decode_worker_dropping_db_intent_key(key) else {
                continue;
            };
            let created_at_ms = decode_dropping_db_intent_created_at(pair.value());
            intents.push(DroppingDbIntent {
                keyspace,
                db_id,
                created_at_ms,
            });
        }

        let next_cursor = if scanned == limit { last_key } else { None };
        Ok((intents, next_cursor))
    }

    /// THE single cross-store enqueue path: fence on the dropped-DB tombstone or
    /// pre-commit dropping intent (`get_for_update`) and, only if the DB is NOT
    /// fenced, enqueue the task — both in the caller's SAME system transaction.
    /// Returns whether the task was enqueued (`false` = the DB was dropped or is
    /// being dropped, enqueue suppressed).
    ///
    /// All cross-store next-fire producers (cron reconcile, post-exec next-fire,
    /// the SQL cron-enqueue path) MUST route through this so the proactive fence
    /// stays consistent across every site. Because the tombstone read and the
    /// `put_task_v2` write share one pessimistic txn, a concurrent DROP-reap
    /// (which PUTs the tombstone) and this enqueue serialize: exactly one commits.
    /// When DROP wins, this enqueue's commit conflicts and is retried; the retry
    /// observes the tombstone and suppresses — so NO stale `_sys_worker` next-fire
    /// row can remain for the dropped db_id.
    pub async fn enqueue_task_v2_unless_db_dropped(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<bool> {
        if self
            .dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)
            .await?
        {
            return Ok(false);
        }
        self.put_task_v2(txn, entry, fire_time_ms).await?;
        Ok(true)
    }

    pub async fn enqueue_singleton_task_v2_unless_db_dropped(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<bool> {
        if self
            .dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)
            .await?
        {
            return Ok(false);
        }
        self.put_singleton_task_v2(txn, entry, fire_time_ms).await
    }

    /// Fence the dropped-DB tombstone / dropping intent ONCE (`get_for_update`)
    /// and, only if the DB is not fenced, write BOTH the `_sys_worker` registry
    /// inventory bit AND the queue row — all in the caller's SAME system
    /// transaction. Returns whether the task was enqueued (`false` = the DB was
    /// dropped/being dropped, the whole enqueue suppressed).
    ///
    /// Why a dedicated helper: direct SQL producers write the registry inventory
    /// row in addition to the queue row, and BOTH are `_sys_worker` rows that must
    /// not survive for a dropped db_id. If the registry write were done before the
    /// fence (or outside it), a strictly-sequential `DROP DATABASE` (which commits
    /// the tombstone PUT, the registry delete, and the queue reap) followed by a
    /// producer for that db_id would re-create a stale registry inventory row even
    /// though the queue row is suppressed — degrading back to self-healing
    /// registry-sweep cleanup and violating the "no stale `_sys_worker` row"
    /// invariant. Folding the registry write under the SAME tombstone
    /// `get_for_update` closes that gap: a concurrent DROP-reap (tombstone PUT)
    /// and this enqueue serialize on the tombstone key, so exactly one commits,
    /// and on retry the fence suppresses registry AND queue together.
    pub async fn enqueue_registry_task_v2_unless_db_dropped(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
        task_type_bit: u8,
    ) -> Result<bool> {
        if entry.task_type.to_bitmask() != task_type_bit {
            return Err(anyhow!(
                "registry task bit {:?} does not match queue entry task type {:?}",
                task_type_bit,
                entry.task_type
            ));
        }
        if self
            .dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)
            .await?
        {
            return Ok(false);
        }
        self.update_registry_task_types(txn, &entry.keyspace, entry.db_id, task_type_bit, 0)
            .await?;
        self.put_task_v2(txn, entry, fire_time_ms).await?;
        Ok(true)
    }

    /// One-shot post-commit producer path for work that cannot be reconstructed
    /// from tenant state in this PR. A terminal tombstone suppresses it; a
    /// transient dropping intent does not. If DROP commits, its tombstone+reap
    /// path removes any race-won queue/registry rows before finalizing cleanup.
    pub async fn enqueue_registry_task_v2_unless_db_tombstoned(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
        task_type_bit: u8,
    ) -> Result<bool> {
        if entry.task_type.to_bitmask() != task_type_bit {
            return Err(anyhow!(
                "registry task bit {:?} does not match queue entry task type {:?}",
                task_type_bit,
                entry.task_type
            ));
        }
        if self
            .dropped_db_tombstone_exists_for_update(txn, &entry.keyspace, entry.db_id)
            .await?
        {
            return Ok(false);
        }
        self.update_registry_task_types(txn, &entry.keyspace, entry.db_id, task_type_bit, 0)
            .await?;
        self.put_task_v2(txn, entry, fire_time_ms).await?;
        Ok(true)
    }

    /// Singleton variant of `enqueue_registry_task_v2_unless_db_dropped`.
    ///
    /// If the singleton work already exists, this still refreshes the registry bit
    /// under the same enqueue fence. A `false` return means either the DB was
    /// fenced before any writes, or the singleton was already pending/claimed; in
    /// both cases callers should commit/rollback according to their local flow and
    /// must not attempt a raw enqueue fallback.
    pub async fn enqueue_singleton_registry_task_v2_unless_db_dropped(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
        task_type_bit: u8,
    ) -> Result<bool> {
        if entry.task_type.to_bitmask() != task_type_bit {
            return Err(anyhow!(
                "registry task bit {:?} does not match queue entry task type {:?}",
                task_type_bit,
                entry.task_type
            ));
        }
        if self
            .dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)
            .await?
        {
            return Ok(false);
        }
        let enqueued = self.put_singleton_task_v2(txn, entry, fire_time_ms).await?;
        self.update_registry_task_types(txn, &entry.keyspace, entry.db_id, task_type_bit, 0)
            .await?;
        Ok(enqueued)
    }

    /// SQL cron-enqueue path (`cron.schedule` / `cron.alter_job`): fence the
    /// dropped-DB tombstone ONCE (`get_for_update`) and, only if the DB is NOT
    /// tombstoned, write BOTH the `_sys_worker` registry inventory bit AND the
    /// next-fire queue row — all in the caller's SAME system transaction. Returns
    /// whether the cron job was enqueued (`false` = the DB was dropped, the whole
    /// enqueue suppressed).
    ///
    /// The two engine-side cross-store sites (cron reconcile, post-exec next-fire)
    /// do NOT touch the registry — they enqueue into an already-registered
    /// inventory — so they keep using `enqueue_task_v2_unless_db_dropped`.
    pub async fn enqueue_cron_registry_and_task_unless_db_dropped(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<bool> {
        self.enqueue_registry_task_v2_unless_db_dropped(txn, entry, fire_time_ms, TASK_TYPE_CRON)
            .await
    }

    /// Registry-only writer variant: fence the dropped-DB tombstone / dropping
    /// intent (`get_for_update`) and only then mutate the `_sys_worker` registry
    /// inventory row in the caller's SAME system transaction.
    ///
    /// Use this for DDL journal / HNSW registry inventory paths that do not create
    /// a queue row at the same time. A raw registry bit update can resurrect a
    /// registry row after DROP has tombstoned and reaped the DB, so the raw helper
    /// stays private and production callers must use this fenced entry point.
    pub async fn update_registry_task_types_unless_db_dropped(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        set_bits: u8,
        clear_bits: u8,
    ) -> Result<bool> {
        if self
            .dropped_db_enqueue_fence_exists_for_update(txn, keyspace, db_id)
            .await?
        {
            return Ok(false);
        }
        self.update_registry_task_types(txn, keyspace, db_id, set_bits, clear_bits)
            .await?;
        Ok(true)
    }

    async fn update_registry_task_types(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        set_bits: u8,
        clear_bits: u8,
    ) -> Result<()> {
        let mut entry = self
            .get_worker_registry(txn, keyspace, db_id)
            .await?
            .unwrap_or_else(|| TaskRegistryEntry::new(keyspace.to_string(), db_id));

        entry.task_types |= set_bits;
        entry.task_types &= !clear_bits;

        self.put_worker_registry(txn, &entry).await?;
        Ok(())
    }

    // ========================================================================
    // External object lifecycle intents
    // ========================================================================

    pub async fn put_hnsw_s3_graph_upload_intent(
        &self,
        txn: &mut Transaction,
        intent: &HnswS3GraphUploadIntent,
    ) -> Result<()> {
        let key = self.key(&encode_hnsw_s3_graph_upload_intent_key(
            &intent.keyspace,
            intent.db_id,
            intent.table_id,
            intent.index_id,
            intent.version,
        ));
        let data =
            bincode::serialize(intent).context("Failed to serialize HNSW S3 upload intent")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn delete_hnsw_s3_graph_upload_intent(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> Result<()> {
        let key = self.key(&encode_hnsw_s3_graph_upload_intent_key(
            keyspace, db_id, table_id, index_id, version,
        ));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn scan_hnsw_s3_graph_upload_intents_page(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<HnswS3GraphUploadIntent>, Option<Vec<u8>>)> {
        self.scan_external_intent_page(
            txn,
            encode_hnsw_s3_graph_upload_intent_prefix(),
            start_after,
            limit,
            "HNSW S3 graph upload intent",
        )
        .await
    }

    pub async fn put_hnsw_s3_db_prefix_cleanup_intent(
        &self,
        txn: &mut Transaction,
        intent: &HnswS3DbPrefixCleanupIntent,
    ) -> Result<()> {
        let key = self.key(&encode_hnsw_s3_db_prefix_cleanup_intent_key(
            &intent.keyspace,
            intent.db_id,
        ));
        let data =
            bincode::serialize(intent).context("Failed to serialize HNSW S3 DB cleanup intent")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn delete_hnsw_s3_db_prefix_cleanup_intent(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_hnsw_s3_db_prefix_cleanup_intent_key(
            keyspace, db_id,
        ));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn scan_hnsw_s3_db_prefix_cleanup_intents_page(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<HnswS3DbPrefixCleanupIntent>, Option<Vec<u8>>)> {
        self.scan_external_intent_page(
            txn,
            encode_hnsw_s3_db_prefix_cleanup_intent_prefix(),
            start_after,
            limit,
            "HNSW S3 DB prefix cleanup intent",
        )
        .await
    }

    async fn scan_external_intent_page<T>(
        &self,
        txn: &mut Transaction,
        prefix: Vec<u8>,
        start_after: Option<&[u8]>,
        limit: usize,
        label: &'static str,
    ) -> Result<(Vec<T>, Option<Vec<u8>>)>
    where
        T: serde::de::DeserializeOwned,
    {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0x00);
                next_start
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(Some(limit))).await)?;

        let mut intents = Vec::new();
        let mut last_key = None;
        let mut scanned = 0usize;
        for pair in pairs {
            scanned += 1;
            let key: &[u8] = pair.key().as_ref().into();
            last_key = Some(key.to_vec());
            if !key.starts_with(&prefix) {
                continue;
            }
            let intent = bincode::deserialize(pair.value())
                .with_context(|| format!("Failed to deserialize {label}"))?;
            intents.push(intent);
        }

        let next_cursor = if scanned == limit { last_key } else { None };
        Ok((intents, next_cursor))
    }

    // ========================================================================
    // Queue methods
    // ========================================================================

    pub async fn try_acquire_or_renew_worker_executor_lease(
        &self,
        owner_id: &str,
        lease_ms: i64,
    ) -> Result<WorkerExecutorLeaseResult> {
        self.try_acquire_or_renew_worker_executor_lease_with_clock(
            owner_id,
            lease_ms,
            crate::worker::now_epoch_ms,
        )
        .await
    }

    async fn try_acquire_or_renew_worker_executor_lease_with_clock<F>(
        &self,
        owner_id: &str,
        lease_ms: i64,
        now_fn: F,
    ) -> Result<WorkerExecutorLeaseResult>
    where
        F: Fn() -> i64,
    {
        let mut txn = self.begin().await?;
        let key = self.key(&encode_worker_executor_lease_key());
        let existing = tikv_op!(txn.get_for_update(key.clone()).await)?;
        let lease_now_ms = now_fn();

        let lease = match existing {
            Some(bytes) => match bincode::deserialize::<WorkerExecutorLease>(&bytes) {
                Ok(current) => {
                    if current.is_live_at(lease_now_ms) && current.owner_id != owner_id {
                        txn.rollback().await.ok();
                        return Ok(WorkerExecutorLeaseResult::HeldByOther(current));
                    }
                    if current.owner_id == owner_id {
                        current.renewed(lease_now_ms, lease_ms)
                    } else {
                        WorkerExecutorLease::new(
                            owner_id.to_string(),
                            lease_now_ms,
                            lease_ms,
                            current.generation.saturating_add(1),
                        )
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "Overwriting corrupt worker executor lease row"
                    );
                    WorkerExecutorLease::new(owner_id.to_string(), lease_now_ms, lease_ms, 1)
                }
            },
            None => WorkerExecutorLease::new(owner_id.to_string(), lease_now_ms, lease_ms, 1),
        };

        txn_put(
            &mut txn,
            key,
            bincode::serialize(&lease).context("Failed to serialize worker executor lease")?,
        )
        .await?;
        txn.commit().await?;
        Ok(WorkerExecutorLeaseResult::Held(lease))
    }

    pub async fn release_worker_executor_lease_if_owned(&self, owner_id: &str) -> Result<bool> {
        let mut txn = self.begin().await?;
        let key = self.key(&encode_worker_executor_lease_key());
        let Some(existing) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            txn.rollback().await.ok();
            return Ok(false);
        };
        let lease: WorkerExecutorLease = bincode::deserialize(&existing)
            .context("Failed to deserialize worker executor lease for release")?;
        if lease.owner_id != owner_id {
            txn.rollback().await.ok();
            return Ok(false);
        }
        txn_delete(&mut txn, key).await?;
        txn.commit().await?;
        Ok(true)
    }

    pub async fn ensure_worker_queue_schema_v2(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let version_key = self.key(&encode_worker_queue_schema_version_key());
        let lock_key = self.key(&encode_worker_queue_migration_lock_key());

        let current = tikv_op!(txn.get_for_update(version_key.clone()).await)?;
        if worker_queue_schema_is_v2(current.as_deref()) {
            if tikv_op!(txn.get(lock_key.clone()).await)?.is_some() {
                txn_delete(&mut txn, lock_key).await?;
                txn.commit().await?;
            } else {
                txn.rollback().await.ok();
            }
            return Ok(());
        }

        txn_put(&mut txn, version_key, vec![WORKER_QUEUE_SCHEMA_V2]).await?;
        txn_delete(&mut txn, lock_key).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Fail-fast retirement gate for the pre-V2 `_worker_queue_` family.
    ///
    /// The V1 drain/projection path is intentionally removed. To avoid silently
    /// stranding a historical V1 backlog, startup must prove the retired prefix
    /// is empty with a bounded 1-key probe before running without the drain.
    pub async fn ensure_legacy_worker_queue_retired(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let prefix = encode_worker_queue_prefix();
        let upper = encode_prefix_end(&prefix);
        let range: BoundRange = (prefix.clone()..upper).into();
        let first_key = tikv_op!(txn.scan_keys(range, 1).await)?
            .map(Vec::from)
            .find(|key| key.starts_with(&prefix));
        txn.rollback().await.ok();

        if let Some(key) = first_key {
            return Err(anyhow!(
                "legacy V1 worker queue row remains under _worker_queue_ \
                 (first_key_len={}); run a version with the V1-to-V2 drain or \
                 explicit cleanup before retiring the V1 drain",
                key.len()
            ));
        }
        Ok(())
    }

    // ========================================================================
    // V2 secondary index (issue #2576)
    //
    // The `_wq_idx_v2_` index lets every task-targeted / tenant-targeted
    // operation resolve via a bounded prefix scan that reads only 1-byte index
    // values, instead of scanning the global due queue and deserializing every
    // entry's (unbounded) `command` SQL. The index is maintained atomically
    // alongside the due entry by `put_task_v2` / `delete_task_v2`.
    // ========================================================================

    /// Page size for V2 index scans. Index values are a single byte, so this is
    /// purely a cursor batch size; even very large tenants stay far under the
    /// gRPC frame limit (~50-byte keys × this batch ≈ a few hundred KiB).
    const WQ_INDEX_SCAN_PAGE: u32 = 4096;

    /// Page size for the streamed DROP DATABASE reap. One page is BOTH the read
    /// unit and the 2PC delete unit, so the read phase never holds more than this
    /// many index rows and the delete txn's write/lock set stays bounded.
    const REAP_INDEX_PAGE: u32 = 256;

    /// Enqueue (or idempotently overwrite) a task in V2 layout: write the
    /// due-queue descriptor, the index row, and — for split task types — the
    /// out-of-line payload, all in the SAME transaction. All production enqueue
    /// paths must use this so the three rows never drift apart within a
    /// committed transaction.
    async fn put_task_v2(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<()> {
        validate_task_v2_enqueue(entry)?;
        let task_type = entry.task_type.to_bitmask();
        let (descriptor, payload) = TaskDescriptorV2::split_from_entry(entry);

        let due_key = self.key(&encode_wq_due_v2_key(
            entry.priority,
            fire_time_ms,
            task_type,
            &entry.keyspace,
            entry.db_id,
            entry.task_id,
        )?);
        let due_value = descriptor
            .encode()
            .context("Failed to serialize V2 task descriptor")?;
        txn_put(txn, due_key, due_value).await?;

        if let Some(payload) = payload {
            let payload_key = self.key(&encode_wq_payload_v2_key(
                task_type,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                fire_time_ms,
            )?);
            let payload_value = payload
                .encode()
                .context("Failed to serialize V2 task payload")?;
            txn_put(txn, payload_key, payload_value).await?;
        }

        let index_key = self.key(&encode_wq_index_key(
            &entry.keyspace,
            entry.db_id,
            task_type,
            entry.task_id,
            fire_time_ms,
        )?);
        // Value = priority byte, enough to reconstruct the exact due key.
        txn_put(txn, index_key, vec![entry.priority]).await?;
        Ok(())
    }

    /// Enqueue a deterministic-key task only if no pending/claimed row already
    /// represents the same logical task.
    ///
    /// Deterministic tasks use a stable due key, so blindly calling
    /// `put_task_v2` can replace the descriptor while a worker is processing an
    /// older nonce. That leaves the newer row behind after successful cleanup and
    /// causes immediate redundant execution. Callers that deliberately want
    /// replacement semantics must use a non-deterministic key or delete first.
    async fn put_singleton_task_v2(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<bool> {
        validate_task_v2_enqueue(entry)?;
        if !entry.task_type.uses_deterministic_queue_key() {
            return Err(anyhow!(
                "singleton enqueue requires a deterministic worker task type, got {:?}",
                entry.task_type
            ));
        }
        if self
            .task_has_pending(
                txn,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                entry.task_type,
            )
            .await?
            || self
                .task_has_claim(
                    txn,
                    &entry.keyspace,
                    entry.db_id,
                    entry.task_id,
                    fire_time_ms,
                    entry.task_type,
                )
                .await?
        {
            return Ok(false);
        }
        self.put_task_v2(txn, entry, fire_time_ms).await?;
        Ok(true)
    }

    /// Delete a task's V2 due descriptor, index row, and (for split types)
    /// payload row in the SAME transaction, given the raw due key and identity.
    /// All production delete paths must use this so no row outlives the due
    /// entry.
    pub async fn delete_task_v2(
        &self,
        txn: &mut Transaction,
        due_key: &[u8],
        keyspace: &str,
        db_id: u64,
        task_type: u8,
        task_id: i64,
        fire_time_ms: i64,
    ) -> Result<()> {
        txn_delete(txn, due_key.to_vec()).await?;
        let index_key = self.key(&encode_wq_index_key(
            keyspace,
            db_id,
            task_type,
            task_id,
            fire_time_ms,
        )?);
        txn_delete(txn, index_key).await?;
        if TaskType::from_bitmask(task_type).is_some_and(|t| t.payload_split()) {
            let payload_key = self.key(&encode_wq_payload_v2_key(
                task_type,
                keyspace,
                db_id,
                task_id,
                fire_time_ms,
            )?);
            txn_delete(txn, payload_key).await?;
        }
        Ok(())
    }

    /// Delete every V2 row for one task by identity. This is bounded by that
    /// task's V2 index rows and never scans the legacy `_worker_queue_` layer.
    pub async fn delete_task_v2_by_identity(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        task_type: TaskType,
    ) -> Result<usize> {
        let rows = self
            .index_rows_for_task(txn, keyspace, db_id, task_id, task_type)
            .await?;
        let deleted = rows.len();
        for r in rows {
            self.delete_task_v2(
                txn,
                &r.due_key,
                &r.keyspace,
                r.db_id,
                r.task_type,
                r.task_id,
                r.fire_time_ms,
            )
            .await?;
        }
        Ok(deleted)
    }

    /// Fetch the out-of-line payload for a split-type V2 task, by full identity.
    pub async fn get_task_payload_v2(
        &self,
        txn: &mut Transaction,
        task_type: u8,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
    ) -> Result<Option<TaskPayloadV2>> {
        let key = self.key(&encode_wq_payload_v2_key(
            task_type,
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        )?);
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                TaskPayloadV2::decode(&data).context("Failed to deserialize V2 task payload")?,
            )),
            None => Ok(None),
        }
    }

    /// Scan ONE bounded page of the V2 index under `logical_prefix`, starting
    /// after `start_after` (exclusive; `None` = from the prefix start). Returns
    /// the decoded rows plus a `next_cursor` (the raw last key) when the page was
    /// full and more rows MAY remain, or `None` when this page reached the end of
    /// the prefix range. Index values are a single byte, so the response is
    /// byte-safe regardless of command payload size; due-queue values are never
    /// read.
    ///
    /// This is the single page primitive. `scan_index_rows` loops it to collect a
    /// full result; `reap_db_queue_entries` drives it page-by-page so the read
    /// phase never materializes the whole per-db backlog at once.
    async fn scan_index_rows_page(
        &self,
        txn: &mut Transaction,
        logical_prefix: &[u8],
        start_after: Option<&[u8]>,
        page_size: u32,
    ) -> Result<(Vec<WqIndexRow>, Option<Vec<u8>>)> {
        let prefix = self.key(logical_prefix);
        let upper = encode_prefix_end(&prefix);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0);
                next_start
            }
            None => prefix.clone(),
        };

        let range: BoundRange = (start..upper).into();
        let pairs: Vec<_> = tikv_op!(txn.scan(range, page_size).await)?.collect();
        let page_len = pairs.len();

        let mut rows = Vec::with_capacity(page_len);
        let mut last_key: Vec<u8> = Vec::new();
        for pair in &pairs {
            let key: &[u8] = pair.key().as_ref().into();
            last_key = key.to_vec();
            if !key.starts_with(&prefix) {
                continue;
            }
            let Some(entry) = decode_wq_index_key(key) else {
                continue;
            };
            // The index value is always a 1-byte priority (written by
            // put_task_v2). A missing/empty value is corruption: skip it
            // rather than defaulting priority to 0, which would reconstruct a
            // due_key at the wrong priority and delete a nonexistent row.
            let Some(priority) = pair.value().first().copied() else {
                tracing::warn!(
                    "Skipping V2 index row with empty value: keyspace={} db_id={} task_type={} task_id={}",
                    entry.keyspace, entry.db_id, entry.task_type, entry.task_id
                );
                continue;
            };
            let due_key = self.key(&encode_wq_due_v2_key(
                priority,
                entry.fire_time_ms,
                entry.task_type,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
            )?);
            rows.push(WqIndexRow {
                due_key,
                keyspace: entry.keyspace,
                db_id: entry.db_id,
                task_type: entry.task_type,
                task_id: entry.task_id,
                fire_time_ms: entry.fire_time_ms,
            });
        }

        // A short page (or an empty last_key) means we reached the end of the
        // prefix range; otherwise hand back the raw last key as the next cursor.
        let next_cursor = if (page_len as u32) < page_size || last_key.is_empty() {
            None
        } else {
            Some(last_key)
        };
        Ok((rows, next_cursor))
    }

    /// Scan the V2 index under `logical_prefix`, returning each matching row with
    /// its reconstructed V2 due-queue key. Loops [`scan_index_rows_page`] until
    /// the prefix range is exhausted. Paged by count (values are 1 byte, so
    /// byte-safe regardless of command payload size). Never reads due-queue
    /// values.
    async fn scan_index_rows(
        &self,
        txn: &mut Transaction,
        logical_prefix: Vec<u8>,
    ) -> Result<Vec<WqIndexRow>> {
        let mut rows = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let (page, next) = self
                .scan_index_rows_page(
                    txn,
                    &logical_prefix,
                    cursor.as_deref(),
                    Self::WQ_INDEX_SCAN_PAGE,
                )
                .await?;
            rows.extend(page);
            match next {
                Some(next_cursor) => cursor = Some(next_cursor),
                None => break,
            }
        }
        Ok(rows)
    }

    /// Index rows for one (keyspace, db_id, task_type, task_id).
    pub async fn index_rows_for_task(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        task_type: TaskType,
    ) -> Result<Vec<WqIndexRow>> {
        self.scan_index_rows(
            txn,
            encode_wq_index_prefix_task(keyspace, db_id, task_type.to_bitmask(), task_id),
        )
        .await
    }

    /// Index rows for one (keyspace, db_id, task_type) — e.g. all cron entries
    /// of a database for reconciliation.
    pub async fn index_rows_for_db_type(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_type: TaskType,
    ) -> Result<Vec<WqIndexRow>> {
        self.scan_index_rows(
            txn,
            encode_wq_index_prefix_db_type(keyspace, db_id, task_type.to_bitmask()),
        )
        .await
    }

    /// All index rows for one (keyspace, db_id), every task type. The DROP
    /// DATABASE reap (`reap_db_queue_entries`) now streams the index page-by-page
    /// instead of materializing the whole backlog, so this collect-all variant is
    /// retained only as a test/diagnostic helper that asserts the full per-db
    /// index set (e.g. "the index is empty after reap").
    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn index_rows_for_db(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Vec<WqIndexRow>> {
        self.scan_index_rows(txn, encode_wq_index_prefix_db(keyspace, db_id))
            .await
    }

    /// All index rows for one keyspace (every db, every task type) — used by the
    /// per-keyspace async-trigger queue-stats metric.
    pub async fn index_rows_for_keyspace(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
    ) -> Result<Vec<WqIndexRow>> {
        self.scan_index_rows(txn, encode_wq_index_prefix_keyspace(keyspace))
            .await
    }

    // ========================================================================
    // V2 due-queue dequeue + byte-safe legacy migration scan (issue #2576)
    // ========================================================================

    /// Scan due V2 descriptors across all priorities up to `now_ms`, ordered by
    /// priority then fire_time. Reads only small descriptors (never payloads),
    /// so the scan response is provably small regardless of command size.
    pub async fn scan_due_v2(
        &self,
        txn: &mut Transaction,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, TaskDescriptorV2)>> {
        let mut results = Vec::new();
        if limit == 0 {
            return Ok(results);
        }
        let due_exclusive = now_ms.saturating_add(1);
        for priority in 0u8..=255 {
            if results.len() >= limit as usize {
                break;
            }
            let mut start = encode_wq_due_v2_prefix();
            start.push(priority);
            let end = encode_wq_due_v2_scan_end(priority, due_exclusive)?;
            let range: BoundRange = (start.clone()..end).into();
            let remaining = limit as usize - results.len();
            let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(Some(remaining))).await)?;
            for pair in pairs {
                let key: &[u8] = pair.key().as_ref().into();
                if !key.starts_with(&start) {
                    continue;
                }
                // Skip (don't abort) on a single undecodable descriptor: this is
                // the GLOBAL tick scan, so aborting on one poison row would wedge
                // the worker for ALL tenants every poll. The undecodable case is
                // latent today (only this binary writes `_wq_due_v2_`, always
                // WQ_FORMAT_V1) but would arise if a future binary wrote a newer
                // descriptor version while an older reader is still in the fleet
                // during a forward rolling deploy. Hard-error semantics are kept
                // for the post-claim hydration of the specific entry to execute.
                match TaskDescriptorV2::decode(pair.value()) {
                    Ok(descriptor) => results.push((key.to_vec(), descriptor)),
                    Err(e) => {
                        tracing::warn!(
                            "Skipping undecodable V2 due descriptor (key_len={}): {}",
                            key.len(),
                            e
                        );
                    }
                }
            }
        }
        Ok(results)
    }

    /// Delete every V2 queue entry for one task. Test-only guard used by queue
    /// storage tests; production SQL hot paths use `delete_task_v2_by_identity`.
    #[cfg(test)]
    pub async fn delete_task_all_layers(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        task_type: TaskType,
    ) -> Result<usize> {
        let mut deleted = 0usize;
        for r in self
            .index_rows_for_task(txn, keyspace, db_id, task_id, task_type)
            .await?
        {
            self.delete_task_v2(
                txn,
                &r.due_key,
                &r.keyspace,
                r.db_id,
                r.task_type,
                r.task_id,
                r.fire_time_ms,
            )
            .await?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Whether any pending queue entry exists for one task. V2-only by contract:
    /// production no longer reads or projects legacy `_worker_queue_` rows.
    pub async fn task_has_pending(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        task_type: TaskType,
    ) -> Result<bool> {
        if !self
            .index_rows_for_task(txn, keyspace, db_id, task_id, task_type)
            .await?
            .is_empty()
        {
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn task_has_claim(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        task_type: TaskType,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        Ok(tikv_op!(txn.get(key).await)?.is_some())
    }

    /// Delete every V2 queue entry for an entire (keyspace, db_id), used by
    /// DROP DATABASE so worker queue entries do not leak. Self-contained and
    /// STREAMED: the read phase fetches ONE bounded index page at a time (via the
    /// dedicated per-db prefix index, 1-byte values — never a global scan), deletes
    /// that page in its own bounded 2PC transaction, advances the cursor, and
    /// repeats. The whole per-db backlog is therefore never materialized into one
    /// Vec, and no single transaction builds an oversized write/lock set. Deletes
    /// stay idempotent (a re-deleted key is a no-op), so a retry after a partial
    /// failure is safe.
    pub async fn reap_db_queue_entries(&self, keyspace: &str, db_id: u64) -> Result<usize> {
        let logical_prefix = encode_wq_index_prefix_db(keyspace, db_id);

        let mut deleted = 0usize;
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            // Read ONE bounded page of index rows from a snapshot read txn.
            let (page, next_cursor) = {
                let mut read_txn = self.begin().await?;
                let result = self
                    .scan_index_rows_page(
                        &mut read_txn,
                        &logical_prefix,
                        cursor.as_deref(),
                        Self::REAP_INDEX_PAGE,
                    )
                    .await?;
                read_txn.rollback().await.ok();
                result
            };

            // Delete exactly this page in its own bounded transaction.
            if !page.is_empty() {
                let mut txn = self.begin().await?;
                for r in &page {
                    self.delete_task_v2(
                        &mut txn,
                        &r.due_key,
                        &r.keyspace,
                        r.db_id,
                        r.task_type,
                        r.task_id,
                        r.fire_time_ms,
                    )
                    .await?;
                    deleted += 1;
                }
                txn.commit().await?;
            }

            // Advance the cursor; stop when this page reached the prefix end.
            match next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(deleted)
    }

    /// Task IDs of all pending AsyncTrigger entries for a keyspace (every db),
    /// using the V2 identity index. Used by the trigger queue-stats metric; reads
    /// only identities, never command payloads.
    pub async fn pending_async_trigger_task_ids(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
    ) -> Result<Vec<i64>> {
        let async_mask = TaskType::AsyncTrigger.to_bitmask();
        let ids: Vec<i64> = self
            .index_rows_for_keyspace(txn, keyspace)
            .await?
            .into_iter()
            .filter(|r| r.task_type == async_mask)
            .map(|r| r.task_id)
            .collect();
        Ok(ids)
    }

    // ========================================================================
    // Claim methods
    // ========================================================================

    /// Acquire a worker claim. CAS-style: takes a write lock on the claim key
    /// with `get_for_update` so concurrent claimers under pessimistic
    /// transactions resolve to a single winner by construction (a plain `get`
    /// takes no lock — design §K4).
    ///
    /// A claim whose lease has EXPIRED is treated as absent and overwritten:
    /// this is how a second worker takes over after the original holder's lease
    /// lapsed (e.g. it crashed or was partitioned). A live (unexpired) lease
    /// held by anyone blocks the claim.
    pub async fn try_claim_worker_task(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        claim: &WorkerClaim,
        legacy_orphan_timeout_ms: i64,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            claim.task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        if let Some(existing) = match tikv_op!(txn.get_for_update(key.clone()).await) {
            Ok(existing) => existing,
            Err(err) if tikv_error_is_worker_claim_contention(&err) => {
                tracing::debug!(
                    task_type = claim.task_type.as_str(),
                    keyspace,
                    db_id,
                    task_id,
                    fire_time_ms,
                    "Worker claim key is contended; treating as claim miss"
                );
                return Ok(false);
            }
            Err(err) => return Err(err.into()),
        } {
            let existing = WorkerClaim::decode_compat(&existing)
                .context("Failed to deserialize existing worker claim")?;
            // A live lease blocks the claim; an expired lease may be taken over.
            if !existing.is_expired(crate::worker::now_epoch_ms(), legacy_orphan_timeout_ms) {
                return Ok(false);
            }
        }
        txn_put(
            txn,
            key,
            bincode::serialize(claim).context("Failed to serialize worker claim")?,
        )
        .await?;
        Ok(true)
    }

    /// Renew (extend the lease of) a claim the calling worker already owns.
    /// Identity-checked: only refreshes the lease if the stored claim still has
    /// the same `worker_id`. Returns `false` when the claim is gone or owned by
    /// someone else, which the caller treats as a lost lease and aborts the run
    /// before its next tenant commit.
    pub async fn renew_worker_claim(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        worker_id: &str,
        task_type: TaskType,
        new_lease_until_ms: i64,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        let Some(existing) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            return Ok(false);
        };
        let mut claim = WorkerClaim::decode_compat(&existing)
            .context("Failed to deserialize worker claim for renewal")?;
        if claim.worker_id != worker_id {
            return Ok(false);
        }
        claim.claimed_at = crate::worker::now_epoch_ms();
        claim.lease_until_ms = new_lease_until_ms;
        txn_put(
            txn,
            key,
            bincode::serialize(&claim).context("Failed to serialize renewed worker claim")?,
        )
        .await?;
        Ok(true)
    }

    pub async fn delete_worker_claim(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        task_type: TaskType,
    ) -> Result<()> {
        let key = self.key(&encode_worker_claim_key(
            task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Read-only ownership re-check: is the claim still held by `worker_id`?
    ///
    /// Returns `true` only when the claim exists AND its `worker_id` matches.
    /// `false` means the claim is gone or was taken over by another worker after
    /// a lost lease. Takes a write lock (`get_for_update`, same identity check as
    /// `delete_worker_claim_if_owned`) so the verdict serializes against a
    /// concurrent takeover/renew/delete, but writes nothing — used as a
    /// commit-adjacent ownership fence on long-task paths (e.g. cron finalize)
    /// where the caller wants to gate a *tenant-store* terminal commit on still
    /// owning the *system-store* claim, BEFORE doing the ownership-checked delete.
    pub async fn is_worker_claim_owned_by(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        task_type: TaskType,
        worker_id: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        let Some(existing) = tikv_op!(txn.get_for_update(key).await)? else {
            return Ok(false);
        };
        let claim = WorkerClaim::decode_compat(&existing)
            .context("Failed to deserialize worker claim for ownership check")?;
        Ok(claim.worker_id == worker_id)
    }

    /// Delete a worker claim ONLY if `worker_id` still owns it. Returns whether
    /// the caller still held the claim (`true`) or it was gone / taken over by
    /// another worker after a lost lease (`false`). Used by the executor's
    /// cleanup so a worker that lost its lease (and whose task was taken over by
    /// a second worker) does NOT delete the new owner's claim. Takes a write
    /// lock (`get_for_update`) so the ownership check and delete are atomic.
    pub async fn delete_worker_claim_if_owned(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        task_type: TaskType,
        worker_id: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        let Some(existing) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            return Ok(false);
        };
        let claim = WorkerClaim::decode_compat(&existing)
            .context("Failed to deserialize worker claim for owned-delete")?;
        if claim.worker_id != worker_id {
            return Ok(false);
        }
        txn_delete(txn, key).await?;
        Ok(true)
    }

    /// Delete a worker claim by its raw TiKV key.
    ///
    /// Used by GC when iterating claims via `list_worker_claims_batch`,
    /// which returns raw keys directly. Callers must only pass keys
    /// obtained from claim-namespace scans.
    pub async fn delete_worker_claim_by_raw_key(
        &self,
        txn: &mut Transaction,
        key: &[u8],
    ) -> Result<()> {
        debug_assert!(
            key.starts_with(&encode_worker_claim_prefix()),
            "delete_worker_claim_by_raw_key called with non-claim key"
        );
        txn_delete(txn, key.to_vec()).await?;
        Ok(())
    }

    pub async fn list_worker_claims(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<(Vec<u8>, WorkerClaim)>> {
        self.list_worker_claims_batch(txn, None, None).await
    }

    pub async fn list_worker_claims_batch(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: Option<usize>,
    ) -> Result<Vec<(Vec<u8>, WorkerClaim)>> {
        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        let prefix = encode_worker_claim_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0x00);
                next_start
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(limit)).await)?;

        let mut results = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let claim = WorkerClaim::decode_compat(pair.value())
                .context("Failed to deserialize worker claim")?;
            results.push((key.to_vec(), claim));
        }
        Ok(results)
    }

    // ========================================================================
    // Background SQL task ID allocation
    // ========================================================================

    /// Allocate the next collision-free background task ID for a (keyspace, db_id) scope.
    ///
    /// Uses a dedicated TiKV CAS sequence key so that concurrent allocations across
    /// multiple db9-server instances always produce distinct IDs.
    pub async fn next_bg_task_id(&self, keyspace: &str, db_id: u64) -> Result<i64> {
        let key = self.key(&encode_worker_bg_task_seq_key(keyspace, db_id));
        self.autocommit_update_key(key, |current| {
            let next_val = match current {
                Some(data) => {
                    let id = i64::from_be_bytes(
                        data.try_into()
                            .map_err(|_| anyhow!("Invalid bg task sequence format"))?,
                    );
                    id.checked_add(1)
                        .ok_or_else(|| anyhow!("Background task ID overflow"))?
                }
                None => 1,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }

    // ========================================================================
    // Background SQL result methods
    // ========================================================================

    pub async fn put_bg_result(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        result_text: &str,
    ) -> Result<()> {
        let key = self.key(&encode_worker_bg_result_key(keyspace, db_id, task_id));
        txn_put(txn, key, result_text.as_bytes().to_vec()).await?;
        Ok(())
    }

    pub async fn get_bg_result(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_worker_bg_result_key(keyspace, db_id, task_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                String::from_utf8(data).context("Failed to decode bg result as UTF-8")?,
            )),
            None => Ok(None),
        }
    }

    // ========================================================================
    // DB lifecycle methods (Core tenant identity and process liveness)
    // ========================================================================

    /// Publish this SQL process's DB lifecycle process-liveness probe.
    ///
    /// This is deliberately not the stable node lease / drain-state row. The
    /// versioned value prevents a future lifecycle reader from mistaking this
    /// scaffold-only process heartbeat for node-drain truth.
    pub async fn put_lifecycle_process_liveness(
        &self,
        txn: &mut Transaction,
        process_instance_id: &str,
        updated_at_version: u64,
        self_fence_after_ms: u64,
    ) -> Result<()> {
        let key = self.key(&encode_lifecycle_process_liveness_key(process_instance_id));
        let data = encode_lifecycle_process_liveness_value(updated_at_version, self_fence_after_ms);
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Delete this process's DB lifecycle process-liveness probe during graceful shutdown.
    pub async fn delete_lifecycle_process_liveness(
        &self,
        txn: &mut Transaction,
        process_instance_id: &str,
    ) -> Result<()> {
        let key = self.key(&encode_lifecycle_process_liveness_key(process_instance_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Allocate a globally monotonic tenant incarnation from the Core lifecycle
    /// domain. Callers must treat a successful allocation as consumed even if a
    /// later tenant-local stamp fails; gaps are safe, reuse is not.
    pub async fn allocate_lifecycle_tenant_incarnation(
        &self,
        txn: &mut Transaction,
    ) -> Result<u64> {
        const FIRST_TENANT_INCARNATION: u64 = 1;

        let key = self.key(&encode_lifecycle_tenant_incarnation_seq_key());
        let current = tikv_op!(txn.get_for_update(key.clone()).await)?;
        let next = match current {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid lifecycle incarnation sequence format"))?;
                u64::from_be_bytes(bytes)
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("Lifecycle tenant incarnation overflow"))?
            }
            None => FIRST_TENANT_INCARNATION,
        };
        txn_put(txn, key, next.to_be_bytes().to_vec()).await?;
        Ok(next)
    }

    /// Upsert the authoritative Core lifecycle inventory row for a tenant DB.
    pub async fn put_lifecycle_tenant_record(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        incarnation: u64,
        status: LifecycleTenantStatus,
        updated_at_version: u64,
    ) -> Result<()> {
        let key = self.key(&encode_lifecycle_tenant_key(keyspace, db_id));
        let value = encode_lifecycle_tenant_value(incarnation, status, updated_at_version);
        txn_put(txn, key, value).await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn get_lifecycle_tenant_record(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Option<LifecycleTenantRecord>> {
        let key = self.key(&encode_lifecycle_tenant_key(keyspace, db_id));
        let Some(value) = tikv_op!(txn.get(key).await)? else {
            return Ok(None);
        };
        let (incarnation, status, updated_at_version) = decode_lifecycle_tenant_value(&value)
            .ok_or_else(|| {
                anyhow!(
                    "Failed to decode lifecycle tenant record for keyspace='{}' db_id={}",
                    keyspace,
                    db_id
                )
            })?;
        Ok(Some(LifecycleTenantRecord {
            keyspace: keyspace.to_string(),
            db_id,
            incarnation,
            status,
            updated_at_version,
        }))
    }

    pub async fn get_lifecycle_tenant_record_for_update(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Option<LifecycleTenantRecord>> {
        let key = self.key(&encode_lifecycle_tenant_key(keyspace, db_id));
        let Some(value) = tikv_op!(txn.get_for_update(key).await)? else {
            return Ok(None);
        };
        let (incarnation, status, updated_at_version) = decode_lifecycle_tenant_value(&value)
            .ok_or_else(|| {
                anyhow!(
                    "Failed to decode lifecycle tenant record for keyspace='{}' db_id={}",
                    keyspace,
                    db_id
                )
            })?;
        Ok(Some(LifecycleTenantRecord {
            keyspace: keyspace.to_string(),
            db_id,
            incarnation,
            status,
            updated_at_version,
        }))
    }

    #[allow(dead_code)]
    pub async fn scan_lifecycle_tenant_records_page(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<LifecycleTenantRecord>, Option<Vec<u8>>)> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let prefix = encode_lifecycle_tenant_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next = last_key.to_vec();
                next.push(0x00);
                next
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(Some(limit))).await)?;

        let mut records = Vec::new();
        let mut last_key = None;
        let mut scanned = 0usize;
        for pair in pairs {
            scanned += 1;
            let key: &[u8] = pair.key().as_ref().into();
            last_key = Some(key.to_vec());
            let Some((keyspace, db_id)) = decode_lifecycle_tenant_key(key) else {
                continue;
            };
            let Some((incarnation, status, updated_at_version)) =
                decode_lifecycle_tenant_value(pair.value())
            else {
                tracing::warn!(
                    keyspace,
                    db_id,
                    "skipping malformed lifecycle tenant record"
                );
                continue;
            };
            records.push(LifecycleTenantRecord {
                keyspace,
                db_id,
                incarnation,
                status,
                updated_at_version,
            });
        }

        let next_cursor = if scanned == limit { last_key } else { None };
        Ok((records, next_cursor))
    }

    pub async fn get_storage_scan_bg_state_for_update(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Option<StorageScanBgState>> {
        let key = self.key(&encode_worker_bg_storage_scan_state_key(keyspace, db_id));
        let Some(value) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            return Ok(None);
        };
        if let Some(state) = decode_storage_scan_bg_state_value(keyspace, db_id, &value) {
            return Ok(Some(state));
        }

        tracing::warn!(
            keyspace,
            db_id,
            value_len = value.len(),
            "deleting malformed StorageSizeScan bg state so it can be re-derived"
        );
        txn_delete(txn, key).await?;
        Ok(None)
    }

    pub async fn put_storage_scan_bg_state(
        &self,
        txn: &mut Transaction,
        state: &StorageScanBgState,
    ) -> Result<()> {
        let key = self.key(&encode_worker_bg_storage_scan_state_key(
            &state.keyspace,
            state.db_id,
        ));
        txn_put(txn, key, encode_storage_scan_bg_state_value(state)).await?;
        Ok(())
    }

    pub async fn delete_storage_scan_bg_state(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_bg_storage_scan_state_key(keyspace, db_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn get_storage_scan_capacity_token_for_update(
        &self,
        txn: &mut Transaction,
        token_id: u16,
    ) -> Result<Option<StorageScanCapacityToken>> {
        let key = self.key(&encode_worker_bg_storage_scan_capacity_key(token_id));
        let Some(value) = tikv_op!(txn.get_for_update(key).await)? else {
            return Ok(None);
        };
        decode_storage_scan_capacity_token_value(&value)
            .map(Some)
            .ok_or_else(|| anyhow!("Failed to decode StorageSizeScan capacity token {token_id}"))
    }

    pub async fn put_storage_scan_capacity_token(
        &self,
        txn: &mut Transaction,
        token_id: u16,
        token: &StorageScanCapacityToken,
    ) -> Result<()> {
        let key = self.key(&encode_worker_bg_storage_scan_capacity_key(token_id));
        txn_put(txn, key, encode_storage_scan_capacity_token_value(token)).await?;
        Ok(())
    }

    pub async fn delete_storage_scan_capacity_token(
        &self,
        txn: &mut Transaction,
        token_id: u16,
    ) -> Result<()> {
        let key = self.key(&encode_worker_bg_storage_scan_capacity_key(token_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    // ========================================================================
    // GC instance state methods (shared cross-instance registry)
    // ========================================================================

    /// GC instance state stored in the configured registry for cross-instance coordination.
    pub async fn put_gc_instance_state(
        &self,
        txn: &mut Transaction,
        instance_id: &str,
        min_start_ts: Option<u64>,
        updated_at_version: u64,
        publish_mode: GcPublishMode,
    ) -> Result<()> {
        let key = self.key(&encode_gc_instance_state_key(instance_id));
        let data = encode_gc_instance_state_value(min_start_ts, updated_at_version, publish_mode);
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Delete a GC instance state record from the shared registry.
    pub async fn delete_gc_instance_state(
        &self,
        txn: &mut Transaction,
        instance_id: &str,
    ) -> Result<()> {
        let key = self.key(&encode_gc_instance_state_key(instance_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Lock and read a GC instance state record by instance ID.
    pub async fn get_gc_instance_state_for_update(
        &self,
        txn: &mut Transaction,
        instance_id: &str,
    ) -> Result<Option<GcInstanceState>> {
        let key = self.key(&encode_gc_instance_state_key(instance_id));
        let Some(data) = tikv_op!(txn.get_for_update(key).await)? else {
            return Ok(None);
        };
        Ok(decode_gc_instance_state_value(&data).map(
            |(min_start_ts, updated_at_version, publish_mode, legacy_max_untracked_timeout_sec)| {
                GcInstanceState {
                    instance_id: instance_id.to_string(),
                    min_start_ts,
                    updated_at_version,
                    publish_mode,
                    legacy_max_untracked_timeout_sec,
                }
            },
        ))
    }

    /// Scan all GC instance states from the shared registry.
    pub async fn scan_gc_instance_states(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<GcInstanceState>> {
        let prefix = self.key(&encode_gc_instance_state_prefix());
        let end = gc_instance_state_scan_end(&prefix);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut results = Vec::new();
        for pair in pairs {
            let key_bytes: &[u8] = pair.key().as_ref().into();
            if !key_bytes.starts_with(&prefix) {
                break;
            }
            let id_bytes = &key_bytes[prefix.len()..];
            let instance_id = String::from_utf8_lossy(id_bytes).to_string();

            // Accept both the current 17-byte format and the older 25-byte
            // format that appended max_untracked_timeout_sec.
            if let Some((min_ts, updated_at, publish_mode, legacy_max_untracked_timeout_sec)) =
                decode_gc_instance_state_value(pair.value())
            {
                results.push(GcInstanceState {
                    instance_id,
                    min_start_ts: min_ts,
                    updated_at_version: updated_at,
                    publish_mode,
                    legacy_max_untracked_timeout_sec,
                });
            }
        }
        Ok(results)
    }
}

fn validate_task_v2_enqueue(entry: &TaskQueueEntry) -> Result<()> {
    if entry.task_type.uses_deterministic_queue_key() && entry.nonce == 0 {
        return Err(anyhow!(
            "deterministic worker task {:?} requires a non-zero nonce before enqueue",
            entry.task_type
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn gc_instance_state_value_round_trips_current_format() {
        let encoded = encode_gc_instance_state_value(Some(123), 456, GcPublishMode::DualWrite);
        assert_eq!(encoded.len(), GC_INSTANCE_STATE_VALUE_LEN);
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456, GcPublishMode::DualWrite, None))
        );
    }

    #[test]
    fn test_gc_publish_mode_backward_compatibility() {
        let mut encoded = Vec::new();
        encoded.push(1);
        encoded.extend_from_slice(&123u64.to_be_bytes());
        encoded.extend_from_slice(&456u64.to_be_bytes());
        assert_eq!(encoded.len(), GC_INSTANCE_STATE_BASE_VALUE_LEN);
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456, GcPublishMode::OldOnly, None))
        );

        encoded.extend_from_slice(&789u64.to_be_bytes());
        assert_eq!(encoded.len(), LEGACY_GC_INSTANCE_STATE_VALUE_LEN);
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456, GcPublishMode::OldOnly, Some(789)))
        );
    }

    #[test]
    fn gc_instance_state_value_decodes_legacy_format_with_timeout_tail() {
        let mut encoded = Vec::new();
        encoded.push(1);
        encoded.extend_from_slice(&123u64.to_be_bytes());
        encoded.extend_from_slice(&456u64.to_be_bytes());
        encoded.extend_from_slice(&789u64.to_be_bytes());
        assert_eq!(encoded.len(), LEGACY_GC_INSTANCE_STATE_VALUE_LEN);
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456, GcPublishMode::OldOnly, Some(789)))
        );
    }

    #[test]
    fn gc_instance_state_scan_end_stays_within_prefix_family() {
        let prefix = b"_sys_worker_gc_instance_abc".to_vec();
        let end = gc_instance_state_scan_end(&prefix);
        assert_eq!(end, [prefix, vec![0xFF]].concat());
    }

    #[test]
    fn lifecycle_process_liveness_value_is_versioned_scaffold_envelope() {
        let encoded = encode_lifecycle_process_liveness_value(123, 60_000);
        assert_eq!(encoded.len(), 17);
        assert_eq!(encoded[0], LIFECYCLE_PROCESS_LIVENESS_VALUE_VERSION);
        assert_eq!(u64::from_be_bytes(encoded[1..9].try_into().unwrap()), 123);
        assert_eq!(
            u64::from_be_bytes(encoded[9..17].try_into().unwrap()),
            60_000
        );
    }

    #[test]
    fn lifecycle_tenant_value_is_versioned_authoritative_inventory_envelope() {
        let encoded = encode_lifecycle_tenant_value(123, LifecycleTenantStatus::Live, 456);
        assert_eq!(encoded.len(), LIFECYCLE_TENANT_VALUE_LEN);
        assert_eq!(encoded[0], LIFECYCLE_TENANT_VALUE_VERSION);
        assert_eq!(
            decode_lifecycle_tenant_value(&encoded),
            Some((123, LifecycleTenantStatus::Live, 456))
        );

        let dropped = encode_lifecycle_tenant_value(123, LifecycleTenantStatus::Dropped, 789);
        assert_eq!(
            decode_lifecycle_tenant_value(&dropped),
            Some((123, LifecycleTenantStatus::Dropped, 789))
        );

        assert_eq!(decode_lifecycle_tenant_value(&encoded[..17]), None);
        let mut bad_status = encoded.clone();
        bad_status[1] = 99;
        assert_eq!(decode_lifecycle_tenant_value(&bad_status), None);
    }

    #[test]
    fn storage_scan_bg_state_value_round_trips_capacity_owner() {
        let state = StorageScanBgState {
            keyspace: "ks".to_string(),
            db_id: 7,
            tenant_incarnation: 11,
            status: StorageScanBgStateStatus::Running,
            work_id: 21,
            run_after_ms: 31,
            lease_until_ms: 41,
            attempt: 3,
            last_done_work_id: 13,
            capacity_token_id: Some(2),
        };
        let encoded = encode_storage_scan_bg_state_value(&state);
        assert_eq!(encoded.len(), STORAGE_SCAN_BG_STATE_VALUE_LEN);
        assert_eq!(encoded[0], STORAGE_SCAN_BG_STATE_VALUE_VERSION);
        assert_eq!(
            decode_storage_scan_bg_state_value("ks", 7, &encoded),
            Some(state)
        );

        let mut bad_status = encoded.clone();
        bad_status[1] = 99;
        assert_eq!(
            decode_storage_scan_bg_state_value("ks", 7, &bad_status),
            None
        );
    }

    #[test]
    fn malformed_storage_scan_bg_state_is_repairable_projection() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn get_storage_scan_bg_state_for_update(")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn put_storage_scan_bg_state").next())
            .expect("get_storage_scan_bg_state_for_update must exist before put helper");

        assert!(
            helper.contains("decode_storage_scan_bg_state_value(keyspace, db_id, &value)")
                && helper.contains("txn_delete(txn, key).await?")
                && helper.contains("Ok(None)"),
            "malformed derived StorageScan state must be deleted and re-derived, not returned as a hard scheduler error"
        );
    }

    #[test]
    fn storage_scan_capacity_token_value_round_trips_owner() {
        let token = StorageScanCapacityToken {
            keyspace: "tenant_a".to_string(),
            db_id: 9,
            tenant_incarnation: 17,
            work_id: 23,
            attempt: 5,
            lease_until_ms: 99,
        };
        let encoded = encode_storage_scan_capacity_token_value(&token);
        assert_eq!(encoded[0], STORAGE_SCAN_CAPACITY_TOKEN_VALUE_VERSION);
        assert_eq!(
            decode_storage_scan_capacity_token_value(&encoded),
            Some(token)
        );

        let truncated = &encoded[..encoded.len() - 1];
        assert_eq!(decode_storage_scan_capacity_token_value(truncated), None);
    }

    #[test]
    fn ordered_worker_inventory_cleanup_reaps_queue_before_registry_delete() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn reap_db_queue_entries_then_delete_worker_registry")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn update_registry_task_types").next())
            .expect("ordered worker inventory cleanup helper must exist");

        let reap_pos = helper
            .find("reap_db_queue_entries(keyspace, db_id).await?")
            .expect("helper must reap DB queue entries");
        let delete_pos = helper
            .find("delete_worker_registry(&mut txn, keyspace, db_id)")
            .expect("helper must delete registry after queue reap");
        assert!(
            reap_pos < delete_pos,
            "registry row must be deleted only after queue reap succeeds"
        );
    }

    #[test]
    fn raw_worker_registry_delete_is_not_public_api() {
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("worker.rs must contain #[cfg(test)] mod tests");
        assert!(
            !prod_source.contains("pub async fn delete_worker_registry"),
            "direct registry delete must stay private; production cleanup must go through the ordered helper"
        );
        assert!(
            !prod_source.contains("pub async fn put_worker_registry")
                && prod_source.contains("async fn put_worker_registry"),
            "direct registry put must stay private; production writers must go through fenced helpers"
        );
    }

    #[test]
    fn storage_dirty_marker_producers_are_removed() {
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("worker.rs must contain #[cfg(test)] mod tests");
        assert!(
            !prod_source.contains("mark_storage_size_dirty")
                && !prod_source.contains("get_storage_size_dirty_marker")
                && !prod_source.contains("clear_storage_size_dirty_if_version")
                && !prod_source.contains("encode_worker_storage_scan_dirty_key"),
            "storage-size dirty marker producer/API/key path must stay removed"
        );

        let singleton_fn = source
            .split("pub async fn enqueue_singleton_task_v2_unless_db_dropped")
            .nth(1)
            .and_then(|rest| rest.split("/// SQL cron-enqueue path").next())
            .expect("dropped-DB fenced singleton helper must exist");
        let singleton_fence_pos = singleton_fn
            .find("dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)")
            .expect("fenced singleton helper must check the dropping/tombstone fence");
        let singleton_put_pos = singleton_fn
            .find("put_singleton_task_v2(txn, entry, fire_time_ms)")
            .expect("fenced singleton helper must delegate to singleton enqueue");
        assert!(
            singleton_fence_pos < singleton_put_pos,
            "fenced singleton enqueue must check tombstone before writing queue rows"
        );
    }

    #[test]
    fn registry_enqueue_helpers_fence_before_registry_and_queue_writes() {
        let source = include_str!("worker.rs");
        let registry_fn = source
            .split("pub async fn enqueue_registry_task_v2_unless_db_dropped")
            .nth(1)
            .and_then(|rest| {
                rest.split("/// Singleton variant of `enqueue_registry_task_v2_unless_db_dropped`")
                    .next()
            })
            .expect("registry enqueue helper must exist");
        let fence_pos = registry_fn
            .find("dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)")
            .expect("registry enqueue helper must check the dropping/tombstone fence");
        let registry_pos = registry_fn
            .find("update_registry_task_types(txn, &entry.keyspace, entry.db_id, task_type_bit, 0)")
            .expect("registry enqueue helper must update the registry inventory");
        let put_pos = registry_fn
            .find("put_task_v2(txn, entry, fire_time_ms)")
            .expect("registry enqueue helper must write the queue row");
        assert!(
            fence_pos < registry_pos && registry_pos < put_pos,
            "registry enqueue helper must take the enqueue fence before writing registry and queue rows"
        );

        let singleton_fn = source
            .split("pub async fn enqueue_singleton_registry_task_v2_unless_db_dropped")
            .nth(1)
            .and_then(|rest| rest.split("/// SQL cron-enqueue path").next())
            .expect("singleton registry enqueue helper must exist");
        let singleton_fence_pos = singleton_fn
            .find("dropped_db_enqueue_fence_exists_for_update(txn, &entry.keyspace, entry.db_id)")
            .expect("singleton registry helper must check the dropping/tombstone fence");
        let singleton_put_pos = singleton_fn
            .find("put_singleton_task_v2(txn, entry, fire_time_ms)")
            .expect("singleton registry helper must use singleton enqueue");
        let singleton_registry_pos = singleton_fn
            .find("update_registry_task_types(txn, &entry.keyspace, entry.db_id, task_type_bit, 0)")
            .expect("singleton registry helper must update the registry inventory");
        assert!(
            singleton_fence_pos < singleton_put_pos && singleton_put_pos < singleton_registry_pos,
            "singleton registry enqueue helper must fence first, then enqueue/dedupe, then refresh registry"
        );
    }

    #[test]
    fn one_shot_registry_enqueue_helper_uses_terminal_tombstone_only() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn enqueue_registry_task_v2_unless_db_tombstoned")
            .nth(1)
            .and_then(|rest| {
                rest.split("/// Singleton variant of `enqueue_registry_task_v2_unless_db_dropped`")
                    .next()
            })
            .expect("tombstone-only one-shot registry helper must exist");
        let tombstone_pos = helper
            .find("dropped_db_tombstone_exists_for_update(txn, &entry.keyspace, entry.db_id)")
            .expect("one-shot helper must check only the terminal tombstone");
        let registry_pos = helper
            .find("update_registry_task_types(txn, &entry.keyspace, entry.db_id, task_type_bit, 0)")
            .expect("one-shot helper must update registry inventory");
        let put_pos = helper
            .find("put_task_v2(txn, entry, fire_time_ms)")
            .expect("one-shot helper must write the queue row");

        assert!(
            tombstone_pos < registry_pos && registry_pos < put_pos,
            "one-shot helper must check the terminal tombstone before registry and queue writes"
        );
        assert!(
            !helper.contains("dropped_db_enqueue_fence_exists_for_update")
                && !helper.contains("DroppingIntent"),
            "one-shot helper must not permanently suppress committed work on a transient dropping intent"
        );
    }

    #[test]
    fn registry_only_helper_fences_before_registry_write() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn update_registry_task_types_unless_db_dropped")
            .nth(1)
            .and_then(|rest| rest.split("async fn update_registry_task_types").next())
            .expect("fenced registry-only helper must exist");
        let fence_pos = helper
            .find("dropped_db_enqueue_fence_exists_for_update(txn, keyspace, db_id)")
            .expect("registry-only helper must check the dropping/tombstone fence");
        let update_pos = helper
            .find("update_registry_task_types(txn, keyspace, db_id, set_bits, clear_bits)")
            .expect("registry-only helper must delegate to raw registry update");
        assert!(
            fence_pos < update_pos,
            "registry-only helper must take the enqueue fence before writing registry rows"
        );
    }

    #[test]
    fn raw_v2_enqueue_and_registry_primitives_are_not_public_api() {
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("worker.rs must contain #[cfg(test)] mod tests");
        assert!(
            !prod_source.contains("pub async fn put_task_v2(")
                && prod_source.contains("async fn put_task_v2("),
            "raw V2 enqueue must stay private; production callers must use fenced helpers"
        );
        assert!(
            !prod_source.contains("pub async fn put_singleton_task_v2(")
                && prod_source.contains("async fn put_singleton_task_v2("),
            "raw singleton V2 enqueue must stay private; production callers must use fenced helpers"
        );
        assert!(
            !prod_source.contains("pub async fn update_registry_task_types(")
                && prod_source.contains("async fn update_registry_task_types("),
            "raw registry bit update must stay private; production callers must use the fenced registry helper"
        );
    }

    #[test]
    fn sql_worker_task_producers_do_not_call_raw_v2_enqueue_or_registry_update() {
        let sources = [
            include_str!("../../sql/executor/core/mod.rs"),
            include_str!("../../sql/executor/dml_analyzed/mod.rs"),
            include_str!("../../sql/ddl/create_index.rs"),
            include_str!("../../sql/executor/bg_sql.rs"),
            include_str!("../../sql/executor/procedure/materialized_views.rs"),
            include_str!("../../sql/ddl/create_table.rs"),
        ];

        for source in sources {
            assert!(
                !source.contains(".put_task_v2(") && !source.contains(".put_singleton_task_v2("),
                "SQL worker producers must route through dropped-DB fenced enqueue helpers"
            );
            assert!(
                !source.contains(".update_registry_task_types("),
                "SQL worker registry-only producers must route through the dropped-DB fenced registry helper"
            );
        }
    }

    #[test]
    fn lifecycle_scan_cursor_advances_on_physical_rows() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn scan_lifecycle_tenant_records_page")
            .nth(1)
            .and_then(|rest| {
                rest.split(
                    "// ========================================================================",
                )
                .next()
            })
            .expect("lifecycle tenant scan helper must exist");
        let scanned_pos = helper
            .find("scanned += 1")
            .expect("lifecycle scan must count physical rows");
        let last_key_pos = helper
            .find("last_key = Some(key.to_vec())")
            .expect("lifecycle scan must advance cursor from physical keys");
        let decode_pos = helper
            .find("decode_lifecycle_tenant_key(key)")
            .expect("lifecycle scan must decode tenant keys");

        assert!(
            scanned_pos < decode_pos && last_key_pos < decode_pos,
            "lifecycle scan must advance physical cursor before malformed rows can be skipped"
        );
        assert!(
            helper.contains("let next_cursor = if scanned == limit"),
            "lifecycle scan continuation must depend on scanned physical rows, not decoded record count"
        );
    }

    #[test]
    fn registry_and_intent_scans_advance_cursor_on_physical_rows() {
        let source = include_str!("worker.rs");
        let registry_scan = source
            .split("pub async fn scan_worker_registry_page")
            .nth(1)
            .and_then(|rest| rest.split("async fn delete_worker_registry").next())
            .expect("worker registry scan helper must exist");
        assert!(
            registry_scan.contains("scanned += 1")
                && registry_scan.contains("let next_cursor = if scanned == limit"),
            "worker registry scan cursor must be based on physical rows scanned"
        );

        let intent_scan = source
            .split("async fn scan_external_intent_page")
            .nth(1)
            .and_then(|rest| {
                rest.split(
                    "// ========================================================================",
                )
                .next()
            })
            .expect("external object intent scan helper must exist");
        assert!(
            intent_scan.contains("scanned += 1")
                && intent_scan.contains("let next_cursor = if scanned == limit"),
            "external object intent scan cursor must be based on physical rows scanned"
        );
    }

    #[test]
    fn deterministic_queue_tasks_require_nonzero_nonce_on_enqueue() {
        let mut hnsw = TaskQueueEntry::new(
            "ks".to_string(),
            7,
            42,
            TaskType::HnswMerge,
            "__hnsw_merge 1 2".to_string(),
            "system".to_string(),
            192,
        );
        assert!(
            validate_task_v2_enqueue(&hnsw).is_err(),
            "HnswMerge uses a deterministic key and must carry a nonce"
        );
        hnsw.nonce = 1;
        validate_task_v2_enqueue(&hnsw).expect("non-zero nonce is valid");

        let mut storage_scan = TaskQueueEntry::new(
            "ks".to_string(),
            7,
            7,
            TaskType::StorageSizeScan,
            String::new(),
            "system".to_string(),
            200,
        );
        assert!(
            validate_task_v2_enqueue(&storage_scan).is_err(),
            "StorageSizeScan also uses a deterministic key and must carry a nonce"
        );
        storage_scan.nonce = 1;
        validate_task_v2_enqueue(&storage_scan).expect("non-zero nonce is valid");

        let cron = cron_entry("ks", 7, 1, "SELECT 1");
        validate_task_v2_enqueue(&cron).expect("non-deterministic queue tasks do not need a nonce");
    }

    #[test]
    fn legacy_v1_drain_projection_is_removed() {
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("worker.rs must contain tests");
        for forbidden in [
            "drain_legacy_worker_queue_batch",
            "scan_due_legacy_bytesafe",
            "legacy_worker_queue_is_empty",
            "legacy_queue_has_entries",
            "scan_legacy_filtered",
            "delete_worker_queue_entry",
            "LegacyWorkerQueueDrainRow",
            "classify_legacy_worker_queue_row_for_drain",
            "migrated_legacy_worker_nonce",
        ] {
            assert!(
                !prod_source.contains(forbidden),
                "legacy V1 queue drain/projection must stay removed from production code: {forbidden}"
            );
        }
    }

    #[test]
    fn legacy_v1_retirement_preflight_is_bounded_and_non_migrating() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn ensure_legacy_worker_queue_retired")
            .nth(1)
            .and_then(|rest| rest.split("// V2 secondary index").next())
            .expect("legacy V1 retirement preflight helper must exist");

        assert!(
            helper.contains("encode_worker_queue_prefix") && helper.contains("scan_keys(range, 1)"),
            "retirement preflight must be a bounded 1-key legacy-prefix probe"
        );
        for forbidden in [
            "put_task_v2",
            "put_singleton_task_v2",
            "delete_worker_queue_entry",
            "delete_task_v2",
            "TaskQueueEntry::deserialize_compat",
        ] {
            assert!(
                !helper.contains(forbidden),
                "retirement preflight must not migrate, execute, delete, or decode V1 rows: {forbidden}"
            );
        }
    }

    #[test]
    fn singleton_enqueue_checks_pending_and_claim_before_put() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("async fn put_singleton_task_v2")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn delete_task_v2").next())
            .expect("singleton enqueue helper must exist before delete_task_v2");

        assert!(
            helper.contains("uses_deterministic_queue_key"),
            "singleton enqueue must be restricted to deterministic task identities"
        );
        assert!(
            helper.contains(".task_has_pending(") && helper.contains(".task_has_claim("),
            "singleton enqueue must skip existing pending or claimed work"
        );
        assert!(
            helper.contains("self.put_task_v2(txn, entry, fire_time_ms).await?"),
            "singleton enqueue must delegate the actual row writes to put_task_v2"
        );
    }

    #[test]
    fn worker_queue_schema_enablement_is_o1_and_does_not_drain_legacy_queue() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn ensure_worker_queue_schema_v2")
            .nth(1)
            .and_then(|rest| rest.split("// V2 secondary index").next())
            .expect("worker queue schema enablement helper must exist");

        assert!(
            helper.contains("encode_worker_queue_schema_version_key"),
            "worker queue schema enablement must write an explicit schema version"
        );
        assert!(
            helper.contains("encode_worker_queue_migration_lock_key"),
            "worker queue schema enablement must clear the legacy startup migration lock"
        );
        assert!(
            !helper.contains("scan_due_legacy_bytesafe")
                && !helper.contains("drain_legacy_worker_queue_batch")
                && !helper.contains("migrate_legacy_worker_queue_to_v2")
                && !helper.contains("try_acquire_worker_queue_migration_lock")
                && !helper.contains("tokio::time::sleep"),
            "startup schema enablement must not scan/drain legacy queue rows or wait on the old lock"
        );
    }

    #[test]
    fn production_targeted_queue_paths_are_v2_only() {
        let source = include_str!("worker.rs");
        let task_has_pending = source
            .split("pub async fn task_has_pending")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn task_has_claim").next())
            .expect("task_has_pending must exist before task_has_claim");
        let reap_db = source
            .split("pub async fn reap_db_queue_entries(&self")
            .nth(1)
            .and_then(|rest| {
                rest.split("/// Task IDs of all pending AsyncTrigger")
                    .next()
            })
            .expect("reap_db_queue_entries must exist before async trigger stats");
        let async_stats = source
            .split("pub async fn pending_async_trigger_task_ids")
            .nth(1)
            .and_then(|rest| {
                rest.split(
                    "// ========================================================================",
                )
                .next()
            })
            .expect("pending_async_trigger_task_ids must exist before claim methods");

        for (name, body) in [
            ("task_has_pending", task_has_pending),
            ("reap_db_queue_entries", reap_db),
            ("pending_async_trigger_task_ids", async_stats),
        ] {
            assert!(
                !body.contains("legacy_queue_has_entries")
                    && !body.contains("scan_legacy_filtered")
                    && !body.contains("delete_worker_queue_entry"),
                "{name} must not scan or delete legacy _worker_queue_ rows"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bg_task_id_atomic_counter_concurrent_allocations_are_unique() {
        let next_id = Arc::new(AtomicI64::new(0));
        let task_count = 100;
        let mut handles = Vec::with_capacity(task_count);

        for _ in 0..task_count {
            let next_id = Arc::clone(&next_id);
            handles.push(tokio::spawn(async move {
                // Mirror the monotonic increment contract behind TiKV CAS allocation.
                next_id.fetch_add(1, Ordering::SeqCst) + 1
            }));
        }

        let mut ids = Vec::with_capacity(task_count);
        for handle in handles {
            ids.push(handle.await.expect("spawned allocator task should join"));
        }

        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            task_count,
            "concurrent atomic allocations must be unique"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn next_bg_task_id_concurrent_allocations_are_unique() {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_worker_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace: system_keyspace.clone(),
            ..Default::default()
        };
        let store = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("failed to initialize system store for bg task ID concurrency test");

        let scope_keyspace = format!(
            "test_bg_task_seq_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let db_id = 4242_u64;

        let task_count = 100;
        let mut handles = Vec::with_capacity(task_count);
        for _ in 0..task_count {
            let store = Arc::clone(&store);
            let keyspace = scope_keyspace.clone();
            handles.push(tokio::spawn(async move {
                let mut backoff_ms = 1_u64;
                for attempt in 0..20 {
                    match store.next_bg_task_id(&keyspace, db_id).await {
                        Ok(id) => return id,
                        Err(err)
                            if err.to_string().contains("autocommit update failed after")
                                && attempt < 19 =>
                        {
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(50);
                        }
                        Err(err) => {
                            panic!("next_bg_task_id allocation should succeed: {err}");
                        }
                    }
                }
                panic!("next_bg_task_id allocation exhausted retry attempts")
            }));
        }

        let mut ids = Vec::with_capacity(task_count);
        for handle in handles {
            ids.push(handle.await.expect("spawned allocator task should join"));
        }

        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            task_count,
            "concurrent next_bg_task_id allocations must be unique"
        );
    }

    // ── V2 worker queue storage (issue #2576) ────────────────────────────
    //
    // TiKV-backed; run with a reachable PD cluster (CI integration-tests job).

    async fn v2_test_store() -> Arc<TikvStore> {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_wq_v2_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store")
    }

    fn unique_ks(tag: &str) -> String {
        format!(
            "ks_{tag}_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn worker_executor_lease_is_single_owner_and_allows_expired_takeover() {
        let store = v2_test_store().await;
        let lease_ms = 30_000;
        let now = 1_000_000;

        let first = store
            .try_acquire_or_renew_worker_executor_lease_with_clock("worker-a", lease_ms, || now)
            .await
            .unwrap();
        let WorkerExecutorLeaseResult::Held(first) = first else {
            panic!("first worker must acquire an empty executor lease");
        };
        assert_eq!(first.owner_id, "worker-a");
        assert_eq!(first.generation, 1);
        assert_eq!(first.lease_until_ms, now + lease_ms);

        let second = store
            .try_acquire_or_renew_worker_executor_lease_with_clock("worker-b", lease_ms, || now + 1)
            .await
            .unwrap();
        let WorkerExecutorLeaseResult::HeldByOther(second) = second else {
            panic!("second worker must stand by while first lease is live");
        };
        assert_eq!(second.owner_id, "worker-a");
        assert_eq!(second.generation, 1);

        let renewed = store
            .try_acquire_or_renew_worker_executor_lease_with_clock("worker-a", lease_ms, || {
                now + 5_000
            })
            .await
            .unwrap();
        let WorkerExecutorLeaseResult::Held(renewed) = renewed else {
            panic!("owner must renew its own executor lease");
        };
        assert_eq!(renewed.owner_id, "worker-a");
        assert_eq!(renewed.generation, 1);
        assert_eq!(renewed.acquired_at_ms, first.acquired_at_ms);
        assert_eq!(renewed.lease_until_ms, now + 5_000 + lease_ms);

        let takeover = store
            .try_acquire_or_renew_worker_executor_lease_with_clock("worker-b", lease_ms, || {
                now + 40_000
            })
            .await
            .unwrap();
        let WorkerExecutorLeaseResult::Held(takeover) = takeover else {
            panic!("second worker must take over after the first lease expires");
        };
        assert_eq!(takeover.owner_id, "worker-b");
        assert_eq!(takeover.generation, 2);

        assert!(!store
            .release_worker_executor_lease_if_owned("worker-a")
            .await
            .unwrap());
        assert!(store
            .release_worker_executor_lease_if_owned("worker-b")
            .await
            .unwrap());
    }

    fn cron_entry(ks: &str, db_id: u64, job_id: i64, command: &str) -> TaskQueueEntry {
        TaskQueueEntry::new(
            ks.to_string(),
            db_id,
            job_id,
            TaskType::Cron,
            command.to_string(),
            "admin".to_string(),
            128,
        )
        .with_schedule("*/5 * * * *".to_string())
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn put_task_v2_splits_payload_indexes_and_keeps_due_value_small() {
        let store = v2_test_store().await;
        let ks = unique_ks("split");
        let db_id = 1u64;
        // 2 MiB command — must NOT appear in the due-scan value.
        let big = "x".repeat(2 * 1024 * 1024);
        let entry = cron_entry(&ks, db_id, 7, &big);
        let fire = 1_000_000i64;

        let mut txn = store.begin().await.unwrap();
        store.put_task_v2(&mut txn, &entry, fire).await.unwrap();
        txn.commit().await.unwrap();

        // Due scan returns a small descriptor (no command inline).
        let mut txn = store.begin().await.unwrap();
        let due = store.scan_due_v2(&mut txn, i64::MAX, 1000).await.unwrap();
        let (_key, descriptor) = due
            .iter()
            .find(|(_, d)| d.keyspace == ks && d.task_id == 7)
            .expect("descriptor present");
        assert!(
            descriptor.inline.is_none(),
            "cron command must be out-of-line"
        );
        assert!(descriptor.needs_payload());

        // Payload fetch returns the full command.
        let payload = store
            .get_task_payload_v2(&mut txn, TaskType::Cron.to_bitmask(), &ks, db_id, 7, fire)
            .await
            .unwrap()
            .expect("payload present");
        assert_eq!(payload.command.len(), big.len());

        // Index finds the task.
        let rows = store
            .index_rows_for_task(&mut txn, &ks, db_id, 7, TaskType::Cron)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fire_time_ms, fire);
        txn.rollback().await.ok();

        // Cleanup
        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id, 7, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn delete_task_all_layers_removes_due_index_and_payload() {
        let store = v2_test_store().await;
        let ks = unique_ks("del");
        let db_id = 1u64;
        let entry = cron_entry(&ks, db_id, 11, "SELECT 1");
        let fire = 2_000_000i64;

        let mut txn = store.begin().await.unwrap();
        store.put_task_v2(&mut txn, &entry, fire).await.unwrap();
        txn.commit().await.unwrap();

        let mut txn = store.begin().await.unwrap();
        let deleted = store
            .delete_task_all_layers(&mut txn, &ks, db_id, 11, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(deleted >= 1);

        // All three rows gone: index empty, payload gone, not pending.
        let mut txn = store.begin().await.unwrap();
        assert!(store
            .index_rows_for_task(&mut txn, &ks, db_id, 11, TaskType::Cron)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .get_task_payload_v2(&mut txn, TaskType::Cron.to_bitmask(), &ks, db_id, 11, fire)
            .await
            .unwrap()
            .is_none());
        assert!(!store
            .task_has_pending(&mut txn, &ks, db_id, 11, TaskType::Cron)
            .await
            .unwrap());
        txn.rollback().await.ok();
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn schedule_replace_leaves_single_entry_via_index() {
        let store = v2_test_store().await;
        let ks = unique_ks("replace");
        let db_id = 1u64;

        // Initial schedule at fire1.
        let mut txn = store.begin().await.unwrap();
        store
            .put_task_v2(&mut txn, &cron_entry(&ks, db_id, 5, "SELECT 1"), 1_000i64)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // Replace: delete-all-layers then put at fire2 (the cron.schedule path).
        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id, 5, TaskType::Cron)
            .await
            .unwrap();
        store
            .put_task_v2(&mut txn, &cron_entry(&ks, db_id, 5, "SELECT 2"), 9_000i64)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // Exactly one index row remains, at the new fire time.
        let mut txn = store.begin().await.unwrap();
        let rows = store
            .index_rows_for_task(&mut txn, &ks, db_id, 5, TaskType::Cron)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "replace must leave a single entry");
        assert_eq!(rows[0].fire_time_ms, 9_000);
        txn.rollback().await.ok();

        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id, 5, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn reap_db_queue_entries_clears_all_task_types_for_db() {
        let store = v2_test_store().await;
        let ks = unique_ks("reap");
        let db_id = 77u64;

        let mut txn = store.begin().await.unwrap();
        store
            .put_task_v2(&mut txn, &cron_entry(&ks, db_id, 1, "SELECT 1"), 100)
            .await
            .unwrap();
        let bg = TaskQueueEntry::new(
            ks.clone(),
            db_id,
            2,
            TaskType::BgSql,
            "SELECT 2".to_string(),
            "admin".to_string(),
            128,
        );
        store.put_task_v2(&mut txn, &bg, 200).await.unwrap();
        // An entry in a DIFFERENT db must survive.
        store
            .put_task_v2(&mut txn, &cron_entry(&ks, db_id + 1, 3, "SELECT 3"), 300)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let reaped = store.reap_db_queue_entries(&ks, db_id).await.unwrap();
        assert!(reaped >= 2);

        let mut txn = store.begin().await.unwrap();
        assert!(store
            .index_rows_for_db(&mut txn, &ks, db_id)
            .await
            .unwrap()
            .is_empty());
        // Other db untouched.
        assert_eq!(
            store
                .index_rows_for_db(&mut txn, &ks, db_id + 1)
                .await
                .unwrap()
                .len(),
            1
        );
        txn.rollback().await.ok();

        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id + 1, 3, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    /// #2628 item 1: the DROP DATABASE reap STREAMS its read phase. Seed more than
    /// one index page of entries for one db and assert (a) the page primitive
    /// `scan_index_rows_page` returns at most `REAP_INDEX_PAGE` rows per call and
    /// hands back a non-None cursor while more remain — i.e. the read phase never
    /// materializes more than one page at once — and (b) `reap_db_queue_entries`
    /// still reaps ALL rows for the db (the same return count and empty index the
    /// collect-all implementation produced), with another db untouched.
    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn reap_db_queue_entries_streams_read_phase_one_page_at_a_time() {
        let store = v2_test_store().await;
        let ks = unique_ks("reap_stream");
        let db_id = 91u64;

        // Seed more than one page so the streamed read must paginate. Spread
        // across multiple task types so the per-db prefix (all task types) is
        // exercised end to end.
        let page = TikvStore::REAP_INDEX_PAGE as i64;
        let total = page + 37; // > 1 page, < 2 pages
        for job in 0..total {
            let mut txn = store.begin().await.unwrap();
            let entry = if job % 2 == 0 {
                cron_entry(&ks, db_id, job, "SELECT 1")
            } else {
                TaskQueueEntry::new(
                    ks.clone(),
                    db_id,
                    job,
                    TaskType::BgSql,
                    "SELECT 2".to_string(),
                    "admin".to_string(),
                    128,
                )
            };
            store
                .put_task_v2(&mut txn, &entry, 1_000 + job)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        }

        // A different db must survive the reap untouched.
        let mut txn = store.begin().await.unwrap();
        store
            .put_task_v2(&mut txn, &cron_entry(&ks, db_id + 1, 5, "SELECT 3"), 9_000)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // (a) Boundedness: the page primitive yields at most one page and a
        // non-None cursor while more rows remain. Walk it manually and assert the
        // invariant on every page.
        let logical_prefix = encode_wq_index_prefix_db(&ks, db_id);
        let mut cursor: Option<Vec<u8>> = None;
        let mut pages = 0usize;
        let mut walked = 0usize;
        loop {
            let mut read_txn = store.begin().await.unwrap();
            let (rows, next) = store
                .scan_index_rows_page(
                    &mut read_txn,
                    &logical_prefix,
                    cursor.as_deref(),
                    TikvStore::REAP_INDEX_PAGE,
                )
                .await
                .unwrap();
            read_txn.rollback().await.ok();
            assert!(
                rows.len() <= TikvStore::REAP_INDEX_PAGE as usize,
                "a single index page must never materialize more than REAP_INDEX_PAGE rows"
            );
            walked += rows.len();
            pages += 1;
            match next {
                Some(next_cursor) => {
                    assert_eq!(
                        rows.len(),
                        TikvStore::REAP_INDEX_PAGE as usize,
                        "a non-None cursor must only follow a FULL page"
                    );
                    cursor = Some(next_cursor);
                }
                None => break,
            }
        }
        assert_eq!(
            walked as i64, total,
            "the streamed walk must cover every row"
        );
        assert!(
            pages >= 2,
            "the seeded backlog must span more than one page (streamed, not one Vec)"
        );

        // (b) The streamed reap deletes every row for the db and reports the count.
        let reaped = store.reap_db_queue_entries(&ks, db_id).await.unwrap();
        assert_eq!(
            reaped as i64, total,
            "streamed reap must delete exactly all rows for the db"
        );

        let mut txn = store.begin().await.unwrap();
        assert!(
            store
                .index_rows_for_db(&mut txn, &ks, db_id)
                .await
                .unwrap()
                .is_empty(),
            "no index row may survive the streamed reap"
        );
        // Other db untouched.
        assert_eq!(
            store
                .index_rows_for_db(&mut txn, &ks, db_id + 1)
                .await
                .unwrap()
                .len(),
            1,
            "a different db must be untouched by the reap"
        );
        txn.rollback().await.ok();

        // Re-reaping is idempotent: a second pass deletes nothing.
        let reaped_again = store.reap_db_queue_entries(&ks, db_id).await.unwrap();
        assert_eq!(reaped_again, 0, "re-reaping an emptied db is a no-op");

        // Cleanup the surviving db.
        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id + 1, 5, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn due_scan_stays_byte_safe_with_many_large_commands() {
        // Regression for the incident: a due scan over many large-command tasks
        // must succeed (V2 descriptors are tiny) where a V1 value scan would
        // exceed the 64 MiB gRPC frame.
        let store = v2_test_store().await;
        let ks = unique_ks("bytesafe");
        let db_id = 1u64;
        let big = "x".repeat(1024 * 1024); // 1 MiB each
        let count = 80i64; // 80 MiB of commands — would blow a V1 value scan.

        for job in 0..count {
            let mut txn = store.begin().await.unwrap();
            store
                .put_task_v2(&mut txn, &cron_entry(&ks, db_id, job, &big), 1_000 + job)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        }

        // The due scan must succeed and return small descriptors only.
        let mut txn = store.begin().await.unwrap();
        let due = store.scan_due_v2(&mut txn, i64::MAX, 1000).await.unwrap();
        let mine = due.iter().filter(|(_, d)| d.keyspace == ks).count();
        assert_eq!(mine as i64, count);
        assert!(due.iter().all(|(_, d)| d.inline.is_none()));
        txn.rollback().await.ok();

        store.reap_db_queue_entries(&ks, db_id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn claim_blocks_while_lease_live_and_allows_takeover_when_expired() {
        // Design §K4 / T4: a live lease blocks a second claimer; an EXPIRED
        // lease lets a second worker take over the same task identity.
        let store = v2_test_store().await;
        let ks = unique_ks("lease_takeover");
        let db_id = 1u64;
        let task_id = 77i64;
        let fire = 1_000i64;
        let legacy_orphan_ms = 300_000;

        // Worker A claims with a live lease.
        let claim_a = WorkerClaim::with_lease("A".to_string(), TaskType::BgSql, 60_000);
        let mut txn = store.begin().await.unwrap();
        assert!(store
            .try_claim_worker_task(
                &mut txn,
                &ks,
                db_id,
                task_id,
                fire,
                &claim_a,
                legacy_orphan_ms
            )
            .await
            .unwrap());
        txn.commit().await.unwrap();

        // Worker B cannot take over while A's lease is live.
        let claim_b = WorkerClaim::with_lease("B".to_string(), TaskType::BgSql, 60_000);
        let mut txn = store.begin().await.unwrap();
        assert!(
            !store
                .try_claim_worker_task(
                    &mut txn,
                    &ks,
                    db_id,
                    task_id,
                    fire,
                    &claim_b,
                    legacy_orphan_ms
                )
                .await
                .unwrap(),
            "a live lease must block a second claimer"
        );
        txn.rollback().await.ok();

        // Force A's lease to be already expired, then B takes over.
        let expired = WorkerClaim {
            worker_id: "A".to_string(),
            claimed_at: crate::worker::now_epoch_ms() - 120_000,
            task_type: TaskType::BgSql,
            lease_until_ms: crate::worker::now_epoch_ms() - 1,
        };
        let key = store.key(&encode_worker_claim_key(
            TaskType::BgSql.to_bitmask(),
            &ks,
            db_id,
            task_id,
            fire,
        ));
        let mut txn = store.begin().await.unwrap();
        txn_put(&mut txn, key, bincode::serialize(&expired).unwrap())
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let mut txn = store.begin().await.unwrap();
        assert!(
            store
                .try_claim_worker_task(
                    &mut txn,
                    &ks,
                    db_id,
                    task_id,
                    fire,
                    &claim_b,
                    legacy_orphan_ms
                )
                .await
                .unwrap(),
            "an expired lease must allow takeover by a second worker"
        );
        txn.commit().await.unwrap();

        // After takeover, A's cleanup must NOT delete B's claim.
        let mut txn = store.begin().await.unwrap();
        let a_still_owns = store
            .delete_worker_claim_if_owned(&mut txn, &ks, db_id, task_id, fire, TaskType::BgSql, "A")
            .await
            .unwrap();
        assert!(
            !a_still_owns,
            "former owner A must not delete the takeover worker B's claim"
        );
        txn.rollback().await.ok();
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn renew_extends_lease_only_for_owner() {
        // Design §K4 / T2: the owner renews its lease; a non-owner renewal fails.
        let store = v2_test_store().await;
        let ks = unique_ks("lease_renew");
        let db_id = 1u64;
        let task_id = 88i64;
        let fire = 2_000i64;
        let legacy_orphan_ms = 300_000;

        let claim = WorkerClaim::with_lease("owner".to_string(), TaskType::BgDdl, 10_000);
        let original_lease = claim.lease_until_ms;
        let mut txn = store.begin().await.unwrap();
        assert!(store
            .try_claim_worker_task(
                &mut txn,
                &ks,
                db_id,
                task_id,
                fire,
                &claim,
                legacy_orphan_ms
            )
            .await
            .unwrap());
        txn.commit().await.unwrap();

        // A foreign worker cannot renew.
        let mut txn = store.begin().await.unwrap();
        assert!(
            !store
                .renew_worker_claim(
                    &mut txn,
                    &ks,
                    db_id,
                    task_id,
                    fire,
                    "intruder",
                    TaskType::BgDdl,
                    crate::worker::now_epoch_ms() + 999_999,
                )
                .await
                .unwrap(),
            "a non-owner must not be able to renew the claim"
        );
        txn.rollback().await.ok();

        // The owner renews, extending the lease.
        let new_lease = crate::worker::now_epoch_ms() + 60_000;
        let mut txn = store.begin().await.unwrap();
        assert!(store
            .renew_worker_claim(
                &mut txn,
                &ks,
                db_id,
                task_id,
                fire,
                "owner",
                TaskType::BgDdl,
                new_lease,
            )
            .await
            .unwrap());
        txn.commit().await.unwrap();

        let mut txn = store.begin().await.unwrap();
        let claims = store.list_worker_claims(&mut txn).await.unwrap();
        txn.rollback().await.ok();
        let stored = claims
            .iter()
            .find(|(_, c)| c.worker_id == "owner")
            .map(|(_, c)| c.clone())
            .expect("owner claim must persist");
        assert!(
            stored.lease_until_ms >= new_lease && stored.lease_until_ms > original_lease,
            "renewal must extend the stored lease"
        );
    }

    // #2627 producer durability (decouple from execution): the test that
    // proves the HNSW-merge, auto-ANALYZE, and async-trigger producers still
    // ENQUEUE a durable V2 row while `execution_enabled() == false` now drives
    // the REAL producers (flush_pending_hnsw_merges / maybe_enqueue_auto_analyze
    // / flush_trigger_activations) through a constructed Executor, so a
    // regression that gates a producer on execution is caught. It lives next to
    // those producers at
    // src/sql/executor/core/tests.rs::producers_enqueue_v2_row_even_when_worker_execution_disabled
    // (the auto-ANALYZE producer is pub(super) to crate::sql::executor and is
    // not reachable from this storage-layer test module).
}
