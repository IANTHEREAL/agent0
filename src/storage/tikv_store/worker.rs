use super::*;
use crate::storage::backpressure::tikv_op;
use crate::worker::types::{
    HnswS3DbPrefixCleanupIntent, HnswS3GraphUploadIntent, TaskDescriptorV2, TaskPayloadV2,
    TaskQueueEntry, TaskRegistryEntry, TaskType, WorkerClaim,
};
use std::time::Duration;

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

const GC_INSTANCE_STATE_VALUE_LEN: usize = 17;
const LEGACY_GC_INSTANCE_STATE_VALUE_LEN: usize = 25;
const WORKER_QUEUE_SCHEMA_V2: u8 = 2;
/// Max rows migrated by ONE `drain_legacy_worker_queue_batch` call. Exposed at
/// crate scope so the worker engine's drain-tick budget-cap orchestration test
/// can compute the exact per-tick migration ceiling
/// (`LEGACY_DRAIN_MAX_BATCHES_PER_TICK * WORKER_QUEUE_MIGRATION_BATCH`).
pub(crate) const WORKER_QUEUE_MIGRATION_BATCH: u32 = 256;
const WORKER_QUEUE_MIGRATION_LOCK_STALE_MS: i64 = 30 * 60 * 1000;

/// Published GC instance state read back from `_sys_worker`.
#[derive(Clone)]
pub struct GcInstanceState {
    pub instance_id: String,
    pub min_start_ts: Option<u64>,
    pub updated_at_version: u64,
    /// Legacy 25-byte row compatibility during mixed-version rollout.
    /// New-format rows do not publish this timeout tail.
    pub legacy_max_untracked_timeout_sec: Option<u64>,
}

enum WorkerQueueMigrationLock {
    AlreadyV2,
    Acquired,
    Busy,
}

fn encode_gc_instance_state_value(min_start_ts: Option<u64>, updated_at_version: u64) -> Vec<u8> {
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
    data
}

fn decode_gc_instance_state_value(val: &[u8]) -> Option<(Option<u64>, u64, Option<u64>)> {
    if val.len() < GC_INSTANCE_STATE_VALUE_LEN {
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
    Some((min_ts, updated_at, legacy_max_untracked_timeout_sec))
}

fn gc_instance_state_scan_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    end.push(0xFF);
    end
}

fn worker_queue_schema_is_v2(value: Option<&[u8]>) -> bool {
    value.is_some_and(|bytes| bytes.first().copied() == Some(WORKER_QUEUE_SCHEMA_V2))
}

fn decode_worker_queue_migration_lock(value: &[u8]) -> i64 {
    if value.len() >= 8 {
        i64::from_be_bytes(value[..8].try_into().unwrap_or([0; 8]))
    } else {
        0
    }
}

fn migrated_legacy_worker_nonce(entry: &TaskQueueEntry, fire_time_ms: i64) -> u64 {
    fn mix(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }

    let mut hash = 0xcbf29ce484222325u64;
    mix(&mut hash, entry.keyspace.as_bytes());
    mix(&mut hash, &entry.db_id.to_be_bytes());
    mix(&mut hash, &entry.task_id.to_be_bytes());
    mix(&mut hash, &[entry.task_type.to_bitmask()]);
    mix(&mut hash, &fire_time_ms.to_be_bytes());
    if hash == 0 {
        1
    } else {
        hash
    }
}

impl TikvStore {
    // ========================================================================
    // Registry methods
    // ========================================================================

    pub async fn put_worker_registry(
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
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry: TaskRegistryEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize worker registry entry")?;
            entries.push(entry);
            last_key = Some(key.to_vec());
        }

        let next_cursor = if entries.len() == limit {
            last_key
        } else {
            None
        };
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
    pub async fn reap_db_queue_entries_then_delete_worker_registry(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<usize> {
        let deleted = self.reap_db_queue_entries(keyspace, db_id).await?;

        let mut txn = self.begin().await?;
        self.delete_worker_registry(&mut txn, keyspace, db_id)
            .await?;
        txn.commit().await?;

        Ok(deleted)
    }

    pub async fn update_registry_task_types(
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
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let intent = bincode::deserialize(pair.value())
                .with_context(|| format!("Failed to deserialize {label}"))?;
            intents.push(intent);
            last_key = Some(key.to_vec());
        }

        let next_cursor = if intents.len() == limit {
            last_key
        } else {
            None
        };
        Ok((intents, next_cursor))
    }

    // ========================================================================
    // Queue methods
    // ========================================================================

    pub async fn ensure_worker_queue_schema_v2(&self) -> Result<usize> {
        loop {
            match self.try_acquire_worker_queue_migration_lock().await? {
                WorkerQueueMigrationLock::AlreadyV2 => return Ok(0),
                WorkerQueueMigrationLock::Acquired => break,
                WorkerQueueMigrationLock::Busy => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }

        let migrated = match self.migrate_legacy_worker_queue_to_v2().await {
            Ok(migrated) => migrated,
            Err(e) => {
                self.clear_worker_queue_migration_lock().await.ok();
                return Err(e);
            }
        };

        let mut txn = self.begin().await?;
        let version_key = self.key(&encode_worker_queue_schema_version_key());
        let lock_key = self.key(&encode_worker_queue_migration_lock_key());
        txn_put(&mut txn, version_key, vec![WORKER_QUEUE_SCHEMA_V2]).await?;
        txn_delete(&mut txn, lock_key).await?;
        txn.commit().await?;
        Ok(migrated)
    }

    async fn try_acquire_worker_queue_migration_lock(&self) -> Result<WorkerQueueMigrationLock> {
        let mut txn = self.begin().await?;
        let version_key = self.key(&encode_worker_queue_schema_version_key());
        if worker_queue_schema_is_v2(tikv_op!(txn.get(version_key).await)?.as_deref()) {
            txn.rollback().await.ok();
            return Ok(WorkerQueueMigrationLock::AlreadyV2);
        }

        let lock_key = self.key(&encode_worker_queue_migration_lock_key());
        let now_ms = crate::worker::now_epoch_ms();
        if let Some(value) = tikv_op!(txn.get_for_update(lock_key.clone()).await)? {
            let locked_at = decode_worker_queue_migration_lock(&value);
            if now_ms.saturating_sub(locked_at) < WORKER_QUEUE_MIGRATION_LOCK_STALE_MS {
                txn.rollback().await.ok();
                return Ok(WorkerQueueMigrationLock::Busy);
            }
        }

        txn_put(&mut txn, lock_key, now_ms.to_be_bytes().to_vec()).await?;
        txn.commit().await?;
        Ok(WorkerQueueMigrationLock::Acquired)
    }

    async fn clear_worker_queue_migration_lock(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let lock_key = self.key(&encode_worker_queue_migration_lock_key());
        txn_delete(&mut txn, lock_key).await?;
        txn.commit().await?;
        Ok(())
    }

    /// One-shot bulk drain of every legacy (`_worker_queue_`) row into V2,
    /// looping the bounded batch primitive until the legacy queue is empty.
    /// Runs once at startup (under the migration lock) to convert all V1 rows a
    /// pre-upgrade binary left behind before the V2-only tick begins polling.
    ///
    /// This handles only the rows that exist at startup. Rows written by an OLD
    /// binary AFTER this node latches the V2 marker are caught by the convergent
    /// background drain (`drain_legacy_worker_queue_batch`) on the maintenance
    /// loop — see the design doc §II.8 M5.
    async fn migrate_legacy_worker_queue_to_v2(&self) -> Result<usize> {
        let mut migrated = 0usize;
        loop {
            let batch = self.drain_legacy_worker_queue_batch().await?;
            migrated += batch;
            if batch < WORKER_QUEUE_MIGRATION_BATCH as usize {
                break;
            }
        }
        Ok(migrated)
    }

    /// Cheap empty-range probe for the legacy (`_worker_queue_`) layer: a single
    /// 1-key scan. The converged steady state of the background drain costs only
    /// this probe, so the drain never imposes a global scan once stragglers stop
    /// appearing.
    pub async fn legacy_worker_queue_is_empty(&self) -> Result<bool> {
        let mut txn = self.begin().await?;
        let empty = !self.legacy_queue_has_entries(&mut txn).await?;
        txn.rollback().await.ok();
        Ok(empty)
    }

    /// Migrate ONE bounded batch (≤ `WORKER_QUEUE_MIGRATION_BATCH`) of due legacy
    /// (`_worker_queue_`) rows into V2 — writing each row's V2 due/index/payload
    /// and deleting its V1 key in the SAME batch transaction — and return how
    /// many were migrated. Returns 0 when the legacy queue is empty.
    ///
    /// This is the single convergent primitive shared by the one-shot startup
    /// migration AND the periodic background drain (maintenance loop). It is
    /// BOUNDED (one page-sized batch per call), never a per-operation or
    /// per-tick global scan: the V2-only enqueue/dequeue hot paths never touch
    /// the legacy layer (issue #2576 invariant). Because new producers only ever
    /// write V2, repeated calls strictly drain the legacy layer toward empty.
    pub async fn drain_legacy_worker_queue_batch(&self) -> Result<usize> {
        let mut txn = self.begin().await?;
        let rows = self
            .scan_due_legacy_bytesafe(&mut txn, i64::MAX, WORKER_QUEUE_MIGRATION_BATCH)
            .await?;
        if rows.is_empty() {
            txn.rollback().await.ok();
            return Ok(0);
        }

        let mut migrated = 0usize;
        for (legacy_key, mut entry) in rows {
            let fire_time_ms = decode_worker_queue_fire_time(&legacy_key).ok_or_else(|| {
                anyhow!("corrupted legacy worker queue key: missing fire_time_ms")
            })?;
            if entry.task_type.uses_deterministic_queue_key() {
                // Deterministic task types REQUIRE singleton semantics: a rolling
                // deploy can let an OLD binary write a V1 deterministic row AFTER
                // a NEW node already enqueued/claimed the V2 singleton. Migrating
                // such a row with raw `put_task_v2` would either overwrite the
                // live descriptor under a new nonce (cleanup's nonce check then
                // skips deletion → re-run) or create a second descriptor at a
                // different `fire_time_ms` (two workers run the same logical
                // singleton). Route through the IDENTICAL singleton guard that
                // every producer uses (`put_singleton_task_v2`): if a pending or
                // claimed V2 row already represents this logical task, drop the
                // V1 key WITHOUT writing another descriptor; otherwise write the
                // one V2 singleton and drop the V1 key — same batch txn.
                if entry.nonce == 0 {
                    entry.nonce = migrated_legacy_worker_nonce(&entry, fire_time_ms);
                }
                self.put_singleton_task_v2(&mut txn, &entry, fire_time_ms)
                    .await?;
            } else {
                self.put_task_v2(&mut txn, &entry, fire_time_ms).await?;
            }
            self.delete_worker_queue_entry(&mut txn, &legacy_key)
                .await?;
            migrated += 1;
        }
        txn.commit().await?;
        Ok(migrated)
    }

    /// Seed a single legacy (`_worker_queue_`) entry, simulating durable state
    /// written by a pre-V2 binary before an upgrade. Test-only: production
    /// paths never write the legacy layout. Used by the production-startup
    /// migration behavioral test (`worker::mod::tests`).
    #[cfg(test)]
    pub async fn seed_legacy_worker_queue_entry_for_test(
        &self,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<()> {
        let legacy_key = encode_worker_queue_key(
            entry.priority,
            fire_time_ms,
            entry.task_type.to_bitmask(),
            &entry.keyspace,
            entry.db_id,
            entry.task_id,
        )?;
        let mut txn = self.begin().await?;
        txn_put(
            &mut txn,
            self.key(&legacy_key),
            bincode::serialize(entry).context("serialize legacy worker queue entry")?,
        )
        .await?;
        txn.commit().await?;
        Ok(())
    }

    /// Seed many legacy (`_worker_queue_`) entries in batched transactions,
    /// simulating a large backlog left by a pre-V2 binary. Test-only; used by the
    /// drain-tick budget-cap orchestration test where seeding one txn per row
    /// would be prohibitively slow. Each `(entry, fire_time_ms)` becomes one V1
    /// key; rows are committed in chunks of `chunk_size`.
    #[cfg(test)]
    pub async fn seed_legacy_worker_queue_entries_for_test(
        &self,
        entries: &[(TaskQueueEntry, i64)],
        chunk_size: usize,
    ) -> Result<()> {
        let chunk_size = chunk_size.max(1);
        for chunk in entries.chunks(chunk_size) {
            let mut txn = self.begin().await?;
            for (entry, fire_time_ms) in chunk {
                let legacy_key = encode_worker_queue_key(
                    entry.priority,
                    *fire_time_ms,
                    entry.task_type.to_bitmask(),
                    &entry.keyspace,
                    entry.db_id,
                    entry.task_id,
                )?;
                txn_put(
                    &mut txn,
                    self.key(&legacy_key),
                    bincode::serialize(entry).context("serialize legacy worker queue entry")?,
                )
                .await?;
            }
            txn.commit().await?;
        }
        Ok(())
    }

    /// Delete a single due-queue key (legacy `_worker_queue_`). Used by the
    /// one-shot V1-to-V2 migration and test-only compatibility checks. Normal
    /// production queue paths are V2-only.
    async fn delete_worker_queue_entry(&self, txn: &mut Transaction, key: &[u8]) -> Result<()> {
        txn_delete(txn, key.to_vec()).await?;
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
    pub async fn put_task_v2(
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
    pub async fn put_singleton_task_v2(
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

    /// Byte-safe scan of due LEGACY (`_worker_queue_`) entries: fetch exactly
    /// one key/value pair per RPC. Used only by startup V1-to-V2 migration.
    async fn scan_due_legacy_bytesafe(
        &self,
        txn: &mut Transaction,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, TaskQueueEntry)>> {
        let mut results = Vec::new();
        if limit == 0 {
            return Ok(results);
        }
        let due_exclusive = now_ms.saturating_add(1);
        for priority in 0u8..=255 {
            if results.len() >= limit as usize {
                break;
            }
            let mut start = encode_worker_queue_prefix();
            start.push(priority);
            let end = encode_worker_queue_scan_end(priority, due_exclusive)?;
            let mut cursor = start.clone();
            while results.len() < limit as usize {
                let range: BoundRange = (cursor.clone()..end.clone()).into();
                let mut pairs = tikv_op!(txn.scan(range, 1).await)?;
                let Some(pair) = pairs.next() else {
                    break;
                };
                let key: &[u8] = pair.key().as_ref().into();
                if !key.starts_with(&start) {
                    break;
                }
                let key = key.to_vec();
                let entry = TaskQueueEntry::deserialize_compat(pair.value())
                    .context("Failed to deserialize worker queue entry")?;
                results.push((key.clone(), entry));
                cursor = key;
                cursor.push(0);
            }
        }
        Ok(results)
    }

    /// Cheap existence check for any legacy (`_worker_queue_`) entry: a single
    /// 1-key scan. Used by the convergent background drain's empty-range probe
    /// (`legacy_worker_queue_is_empty`) and by migration tests. It is NOT a
    /// per-operation gate on any enqueue/dequeue hot path — those are V2-only
    /// (issue #2576 invariant); only the bounded maintenance-loop drain consults
    /// it.
    async fn legacy_queue_has_entries(&self, txn: &mut Transaction) -> Result<bool> {
        let prefix = encode_worker_queue_prefix();
        let end = encode_prefix_end(&prefix);
        let range: BoundRange = (prefix.clone()..end).into();
        let keys: Vec<Vec<u8>> = tikv_op!(txn.scan_keys(range, 1).await)?
            .map(Vec::from)
            .collect();
        Ok(keys.iter().any(|k| k.starts_with(&prefix)))
    }

    /// Byte-safe scan of legacy (`_worker_queue_`) entries matching `pred`.
    /// Test-only: production V1 compatibility is handled by the startup schema
    /// migration and normal task/db-targeted paths are V2-only.
    #[cfg(test)]
    async fn scan_legacy_filtered<F>(
        &self,
        txn: &mut Transaction,
        mut pred: F,
    ) -> Result<Vec<(Vec<u8>, TaskQueueEntry)>>
    where
        F: FnMut(&TaskQueueEntry) -> bool,
    {
        let prefix = encode_worker_queue_prefix();
        let upper = encode_prefix_end(&prefix);

        let mut out = Vec::new();
        let mut cursor = prefix.clone();
        loop {
            let range: BoundRange = (cursor.clone()..upper.clone()).into();
            let mut pairs = tikv_op!(txn.scan(range, 1).await)?;
            let Some(pair) = pairs.next() else {
                break;
            };
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                break;
            }
            let key = key.to_vec();
            let entry = TaskQueueEntry::deserialize_compat(pair.value())
                .context("Failed to deserialize worker queue entry")?;
            if pred(&entry) {
                out.push((key.clone(), entry));
            }
            cursor = key;
            cursor.push(0);
        }
        Ok(out)
    }

    /// Delete every queue entry for one task across V2 and test-seeded legacy.
    /// Test-only guard; production SQL hot paths use `delete_task_v2_by_identity`
    /// after startup migration has made the queue schema V2-only.
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
        if self.legacy_queue_has_entries(txn).await? {
            let rows = self
                .scan_legacy_filtered(txn, |e| {
                    e.task_id == task_id
                        && e.db_id == db_id
                        && e.keyspace == keyspace
                        && e.task_type == task_type
                })
                .await?;
            for (key, _) in rows {
                self.delete_worker_queue_entry(txn, &key).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Legacy (`_worker_queue_`) entries for one (keyspace, db_id, task_type),
    /// as `(legacy_due_key, task_id)`. Test-only guard; production paths must
    /// not call this helper because it scans the whole legacy queue.
    #[cfg(test)]
    pub async fn legacy_entries_for_db_type(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_type: TaskType,
    ) -> Result<Vec<(Vec<u8>, i64)>> {
        if !self.legacy_queue_has_entries(txn).await? {
            return Ok(Vec::new());
        }
        let rows = self
            .scan_legacy_filtered(txn, |e| {
                e.task_type == task_type && e.db_id == db_id && e.keyspace == keyspace
            })
            .await?;
        Ok(rows.into_iter().map(|(key, e)| (key, e.task_id)).collect())
    }

    /// Whether any pending queue entry exists for one task. V2-only by contract:
    /// startup schema migration converts legacy rows before production paths run.
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
        if let Some(existing) = tikv_op!(txn.get_for_update(key.clone()).await)? {
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
    // GC instance state methods (shared cross-instance registry)
    // ========================================================================

    /// GC instance state stored in `_sys_worker` for cross-instance coordination.
    pub async fn put_gc_instance_state(
        &self,
        txn: &mut Transaction,
        instance_id: &str,
        min_start_ts: Option<u64>,
        updated_at_version: u64,
    ) -> Result<()> {
        let key = self.key(&encode_gc_instance_state_key(instance_id));
        let data = encode_gc_instance_state_value(min_start_ts, updated_at_version);
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
            |(min_start_ts, updated_at_version, legacy_max_untracked_timeout_sec)| {
                GcInstanceState {
                    instance_id: instance_id.to_string(),
                    min_start_ts,
                    updated_at_version,
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
            if let Some((min_ts, updated_at, legacy_max_untracked_timeout_sec)) =
                decode_gc_instance_state_value(pair.value())
            {
                results.push(GcInstanceState {
                    instance_id,
                    min_start_ts: min_ts,
                    updated_at_version: updated_at,
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
        let encoded = encode_gc_instance_state_value(Some(123), 456);
        assert_eq!(encoded.len(), GC_INSTANCE_STATE_VALUE_LEN);
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456, None))
        );
    }

    #[test]
    fn gc_instance_state_value_decodes_legacy_format_with_timeout_tail() {
        let mut encoded = encode_gc_instance_state_value(Some(123), 456);
        encoded.extend_from_slice(&789u64.to_be_bytes());
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456, Some(789)))
        );
    }

    #[test]
    fn gc_instance_state_scan_end_stays_within_prefix_family() {
        let prefix = b"_sys_worker_gc_instance_abc".to_vec();
        let end = gc_instance_state_scan_end(&prefix);
        assert_eq!(end, [prefix, vec![0xFF]].concat());
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
            .split("#[cfg(test)]")
            .next()
            .expect("worker.rs must contain #[cfg(test)]");
        assert!(
            !prod_source.contains("pub async fn delete_worker_registry"),
            "direct registry delete must stay private; production cleanup must go through the ordered helper"
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
    fn singleton_enqueue_checks_pending_and_claim_before_put() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn put_singleton_task_v2")
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
    fn worker_queue_migration_uses_schema_version_and_lock() {
        let source = include_str!("worker.rs");
        let helper = source
            .split("pub async fn ensure_worker_queue_schema_v2")
            .nth(1)
            .and_then(|rest| rest.split("/// Delete a single due-queue key").next())
            .expect("worker queue schema migration helper must exist");

        assert!(
            helper.contains("encode_worker_queue_schema_version_key"),
            "worker queue migration must write an explicit schema version"
        );
        assert!(
            helper.contains("encode_worker_queue_migration_lock_key"),
            "worker queue migration must use an explicit migration lock"
        );
        assert!(
            helper.contains("scan_due_legacy_bytesafe"),
            "legacy queue reads must be isolated to the startup migration helper"
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

    async fn raw_worker_test_store(tag: &str) -> Arc<TikvStore> {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_wq_{tag}_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        // `new_system` connects with `with_keyspace`, which requires the keyspace
        // to already exist in PD (the vendored client does NOT auto-create it).
        // Pre-create it with the canonical PD-API helper, mirroring what
        // `init_gc_registry_store` (used by `v2_test_store`) does — otherwise the
        // connect fails with "keyspace does not exist".
        crate::worker::ensure_system_keyspace(&pd_endpoints, &system_keyspace)
            .await
            .expect("pre-create raw worker test keyspace in PD");
        Arc::new(
            TikvStore::new_system(pd_endpoints, &system_keyspace)
                .await
                .expect("init raw system store"),
        )
    }

    fn unique_ks(tag: &str) -> String {
        format!(
            "ks_{tag}_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
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

    /// Convergent drain (design §II.8 M5): a legacy V1 row written by an OLD
    /// binary AFTER this node already latched `_wq_schema_version = 2` (the
    /// rolling-deploy straggler) must still be migrated to V2 by the periodic
    /// background drain, not stranded by the one-shot startup migration.
    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn convergent_drain_migrates_straggler_v1_rows_written_after_v2_marker() {
        // init_gc_registry_store runs the one-shot migration and latches the V2
        // marker — exactly the production startup state.
        let store = v2_test_store().await;
        let ks = unique_ks("drain");
        let db_id = 9u64;

        // Empty legacy queue right after startup: drain is a no-op empty probe.
        assert!(
            store
                .legacy_worker_queue_is_empty()
                .await
                .expect("empty probe"),
            "legacy queue must be empty immediately after startup migration"
        );
        assert_eq!(
            store
                .drain_legacy_worker_queue_batch()
                .await
                .expect("drain empty"),
            0,
            "draining an empty legacy queue migrates nothing"
        );

        // Simulate an OLD binary writing a V1 row AFTER the V2 marker is set.
        let entry = cron_entry(&ks, db_id, 314, "SELECT 1");
        let fire = crate::worker::now_epoch_ms() - 60_000;
        store
            .seed_legacy_worker_queue_entry_for_test(&entry, fire)
            .await
            .expect("seed straggler V1 row");
        assert!(
            !store
                .legacy_worker_queue_is_empty()
                .await
                .expect("non-empty probe"),
            "straggler V1 row must be observed by the empty probe"
        );

        // The V2-only tick scan must NOT yet see it (it is still a V1 row).
        let mut txn = store.begin().await.unwrap();
        let due_before = store.scan_due_v2(&mut txn, i64::MAX, 1000).await.unwrap();
        txn.rollback().await.ok();
        assert!(
            !due_before
                .iter()
                .any(|(_, d)| d.keyspace == ks && d.task_id == 314),
            "straggler is invisible to the V2-only tick before the drain"
        );

        // One convergent drain batch migrates the straggler.
        let migrated = store
            .drain_legacy_worker_queue_batch()
            .await
            .expect("drain straggler");
        assert_eq!(migrated, 1, "drain must migrate exactly the one straggler");

        // Legacy queue is now empty (converged) and the row is visible to V2.
        assert!(
            store
                .legacy_worker_queue_is_empty()
                .await
                .expect("converged probe"),
            "legacy queue must be empty after the straggler is drained"
        );
        let mut txn = store.begin().await.unwrap();
        let due_after = store.scan_due_v2(&mut txn, i64::MAX, 1000).await.unwrap();
        txn.rollback().await.ok();
        assert!(
            due_after.iter().any(|(_, d)| d.keyspace == ks
                && d.task_id == 314
                && d.task_type == TaskType::Cron),
            "drained straggler must be visible to the V2-only tick"
        );

        // Cleanup.
        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id, 314, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    fn hnsw_merge_entry(ks: &str, db_id: u64, task_id: i64) -> TaskQueueEntry {
        TaskQueueEntry::new(
            ks.to_string(),
            db_id,
            task_id,
            TaskType::HnswMerge,
            String::new(),
            "system".to_string(),
            128,
        )
    }

    /// Convergent drain + singleton contract (design §II.8 M5): a rolling deploy
    /// can let an OLD binary write a V1 deterministic (`HnswMerge`) row AFTER a
    /// NEW node already enqueued the V2 singleton. Draining that straggler must
    /// NOT create a second V2 descriptor (raw `put_task_v2` would) — it must
    /// route through the SAME singleton guard producers use and simply drop the
    /// V1 key, leaving exactly one V2 representation.
    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn drain_preserves_singleton_for_deterministic_straggler_when_v2_exists() {
        let store = v2_test_store().await;
        let ks = unique_ks("det_dup");
        let db_id = 21u64;
        let task_id = 7i64;

        // A NEW node already enqueued the V2 singleton via the producer path.
        let v2_entry = {
            let mut e = hnsw_merge_entry(&ks, db_id, task_id);
            e.nonce = 0xA1B2C3D4;
            e
        };
        let v2_fire = crate::worker::now_epoch_ms() - 30_000;
        let mut txn = store.begin().await.unwrap();
        assert!(
            store
                .put_singleton_task_v2(&mut txn, &v2_entry, v2_fire)
                .await
                .unwrap(),
            "first singleton enqueue must write the descriptor"
        );
        txn.commit().await.unwrap();

        // Snapshot the live V2 descriptor (identity + nonce) before the drain.
        let before = {
            let mut txn = store.begin().await.unwrap();
            let rows = store
                .index_rows_for_task(&mut txn, &ks, db_id, task_id, TaskType::HnswMerge)
                .await
                .unwrap();
            txn.rollback().await.ok();
            rows
        };
        assert_eq!(before.len(), 1, "exactly one V2 descriptor before drain");
        let before_fire = before[0].fire_time_ms;
        assert_eq!(
            before_fire, v2_fire,
            "V2 descriptor at the producer fire_time"
        );

        // An OLD binary writes a V1 deterministic straggler for the SAME logical
        // task at a DIFFERENT fire_time (nonce 0 — the pre-V2 layout).
        let straggler = hnsw_merge_entry(&ks, db_id, task_id);
        let straggler_fire = before_fire + 5_000;
        store
            .seed_legacy_worker_queue_entry_for_test(&straggler, straggler_fire)
            .await
            .expect("seed deterministic straggler V1 row");
        assert!(
            !store
                .legacy_worker_queue_is_empty()
                .await
                .expect("non-empty probe"),
            "straggler V1 row must be observed before the drain"
        );

        // Drain the straggler.
        let migrated = store
            .drain_legacy_worker_queue_batch()
            .await
            .expect("drain straggler");
        assert_eq!(migrated, 1, "drain removes exactly the one V1 straggler");

        // The V1 key is gone (converged).
        assert!(
            store
                .legacy_worker_queue_is_empty()
                .await
                .expect("converged probe"),
            "legacy queue must be empty after draining the straggler"
        );

        // Singleton preserved: STILL exactly one V2 descriptor, unchanged — no
        // second row at `straggler_fire`, no nonce overwrite.
        let after = {
            let mut txn = store.begin().await.unwrap();
            let rows = store
                .index_rows_for_task(&mut txn, &ks, db_id, task_id, TaskType::HnswMerge)
                .await
                .unwrap();
            txn.rollback().await.ok();
            rows
        };
        assert_eq!(
            after.len(),
            1,
            "draining a deterministic straggler must NOT create a second V2 descriptor"
        );
        assert_eq!(
            after[0].fire_time_ms, before_fire,
            "the surviving descriptor must be the original V2 singleton, not the straggler's fire_time"
        );

        // Cleanup.
        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id, task_id, TaskType::HnswMerge)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    /// The complementary case: when NO V2 row exists for the logical
    /// deterministic task, draining the legacy V1 straggler must materialize
    /// EXACTLY ONE V2 singleton (and drop the V1 key).
    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn drain_creates_single_singleton_for_deterministic_straggler_when_no_v2() {
        let store = v2_test_store().await;
        let ks = unique_ks("det_new");
        let db_id = 22u64;
        let task_id = 9i64;

        // No V2 row exists yet.
        let mut txn = store.begin().await.unwrap();
        assert!(
            store
                .index_rows_for_task(&mut txn, &ks, db_id, task_id, TaskType::HnswMerge)
                .await
                .unwrap()
                .is_empty(),
            "no V2 descriptor must exist before the drain"
        );
        txn.rollback().await.ok();

        // OLD binary wrote a V1 deterministic straggler (nonce 0).
        let straggler = hnsw_merge_entry(&ks, db_id, task_id);
        let straggler_fire = crate::worker::now_epoch_ms() - 10_000;
        store
            .seed_legacy_worker_queue_entry_for_test(&straggler, straggler_fire)
            .await
            .expect("seed deterministic straggler V1 row");

        // Drain materializes the singleton.
        let migrated = store
            .drain_legacy_worker_queue_batch()
            .await
            .expect("drain straggler");
        assert_eq!(migrated, 1, "drain removes exactly the one V1 straggler");
        assert!(
            store
                .legacy_worker_queue_is_empty()
                .await
                .expect("converged probe"),
            "legacy queue must be empty after the drain"
        );

        // Exactly one V2 singleton now exists, at the straggler's fire_time, with
        // a non-zero migrated nonce (V2 deterministic rows require a nonce).
        let after = {
            let mut txn = store.begin().await.unwrap();
            let rows = store
                .index_rows_for_task(&mut txn, &ks, db_id, task_id, TaskType::HnswMerge)
                .await
                .unwrap();
            txn.rollback().await.ok();
            rows
        };
        assert_eq!(
            after.len(),
            1,
            "draining a deterministic straggler with no V2 must create exactly one V2 singleton"
        );
        assert_eq!(
            after[0].fire_time_ms, straggler_fire,
            "the materialized singleton keeps the straggler's fire_time"
        );

        let mut txn = store.begin().await.unwrap();
        let due = store.scan_due_v2(&mut txn, i64::MAX, 1000).await.unwrap();
        txn.rollback().await.ok();
        let descriptor = due
            .iter()
            .find(|(_, d)| d.keyspace == ks && d.task_id == task_id)
            .map(|(_, d)| d)
            .expect("migrated descriptor present in the V2 due scan");
        assert_eq!(descriptor.task_type, TaskType::HnswMerge);
        assert_ne!(
            descriptor.nonce, 0,
            "migrated deterministic singleton must carry a non-zero nonce"
        );

        // Cleanup.
        let mut txn = store.begin().await.unwrap();
        store
            .delete_task_all_layers(&mut txn, &ks, db_id, task_id, TaskType::HnswMerge)
            .await
            .unwrap();
        txn.commit().await.unwrap();
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
    async fn legacy_entry_is_migrated_to_v2_before_production_paths() {
        let store = raw_worker_test_store("legacy_migrate").await;
        let ks = unique_ks("legacy");
        let db_id = 1u64;
        let fire = 5_000i64;

        // Seed a LEGACY entry directly (simulating a pre-V2 binary's write).
        let entry = cron_entry(&ks, db_id, 21, "SELECT pg_sleep(1)");
        let legacy_key = encode_worker_queue_key(
            entry.priority,
            fire,
            TaskType::Cron.to_bitmask(),
            &ks,
            db_id,
            21,
        )
        .unwrap();
        let mut txn = store.begin().await.unwrap();
        txn_put(
            &mut txn,
            store.key(&legacy_key),
            bincode::serialize(&entry).unwrap(),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let mut txn = store.begin().await.unwrap();
        assert!(store.legacy_queue_has_entries(&mut txn).await.unwrap());
        let due = store
            .scan_due_legacy_bytesafe(&mut txn, i64::MAX, 1000)
            .await
            .unwrap();
        assert!(due.iter().any(|(_, e)| e.keyspace == ks
            && e.task_id == 21
            && e.command == "SELECT pg_sleep(1)"));
        let legacy_cron = store
            .legacy_entries_for_db_type(&mut txn, &ks, db_id, TaskType::Cron)
            .await
            .unwrap();
        assert!(
            legacy_cron.iter().any(|(_, jid)| *jid == 21),
            "legacy_entries_for_db_type must still find the seeded legacy cron job"
        );
        assert!(
            store
                .index_rows_for_task(&mut txn, &ks, db_id, 21, TaskType::Cron)
                .await
                .unwrap()
                .is_empty(),
            "legacy entry must NOT be present in the V2 index"
        );
        txn.rollback().await.ok();

        let migrated = store.ensure_worker_queue_schema_v2().await.unwrap();
        assert_eq!(migrated, 1);

        let mut txn = store.begin().await.unwrap();
        assert!(!store.legacy_queue_has_entries(&mut txn).await.unwrap());
        assert!(store
            .task_has_pending(&mut txn, &ks, db_id, 21, TaskType::Cron)
            .await
            .unwrap());
        let rows = store
            .index_rows_for_task(&mut txn, &ks, db_id, 21, TaskType::Cron)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "migration must write the V2 identity index");
        let payload = store
            .get_task_payload_v2(&mut txn, TaskType::Cron.to_bitmask(), &ks, db_id, 21, fire)
            .await
            .unwrap()
            .expect("cron payload must be split out during migration");
        assert_eq!(payload.command, "SELECT pg_sleep(1)");
        txn.rollback().await.ok();

        let mut txn = store.begin().await.unwrap();
        let deleted = store
            .delete_task_all_layers(&mut txn, &ks, db_id, 21, TaskType::Cron)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(deleted >= 1);

        let mut txn = store.begin().await.unwrap();
        assert!(!store.legacy_queue_has_entries(&mut txn).await.unwrap());
        assert!(!store
            .task_has_pending(&mut txn, &ks, db_id, 21, TaskType::Cron)
            .await
            .unwrap());
        txn.rollback().await.ok();
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
