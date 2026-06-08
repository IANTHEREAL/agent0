use super::*;
use crate::storage::backpressure::tikv_op;
use crate::worker::types::{
    TaskDescriptorV2, TaskPayloadV2, TaskQueueEntry, TaskRegistryEntry, TaskType, WorkerClaim,
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

const GC_INSTANCE_STATE_VALUE_LEN: usize = 17;
const LEGACY_GC_INSTANCE_STATE_VALUE_LEN: usize = 25;

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

    pub async fn delete_worker_registry(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_registry_key(keyspace, db_id));
        txn_delete(txn, key).await?;
        Ok(())
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

        if entry.task_types == 0 && entry.job_count == 0 {
            self.delete_worker_registry(txn, keyspace, db_id).await?;
        } else {
            self.put_worker_registry(txn, &entry).await?;
        }
        Ok(())
    }

    // ========================================================================
    // Queue methods
    // ========================================================================

    /// Delete a single due-queue key (legacy `_worker_queue_`). Used by the
    /// migration-window legacy paths and the worker-tick cleanup of a legacy
    /// entry. V2 deletes go through [`Self::delete_task_v2`].
    pub async fn delete_worker_queue_entry(&self, txn: &mut Transaction, key: &[u8]) -> Result<()> {
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

    /// Scan the V2 index under `logical_prefix`, returning each matching row with
    /// its reconstructed V2 due-queue key. Paged by count (values are 1 byte, so
    /// byte-safe regardless of command payload size). Never reads due-queue
    /// values.
    async fn scan_index_rows(
        &self,
        txn: &mut Transaction,
        logical_prefix: Vec<u8>,
    ) -> Result<Vec<WqIndexRow>> {
        let prefix = self.key(&logical_prefix);
        let upper = encode_prefix_end(&prefix);

        let mut rows = Vec::new();
        let mut cursor = prefix.clone();
        loop {
            let range: BoundRange = (cursor.clone()..upper.clone()).into();
            let pairs: Vec<_> =
                tikv_op!(txn.scan(range, Self::WQ_INDEX_SCAN_PAGE).await)?.collect();
            let page_len = pairs.len();
            if page_len == 0 {
                break;
            }
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
            if (page_len as u32) < Self::WQ_INDEX_SCAN_PAGE || last_key.is_empty() {
                break;
            }
            cursor = last_key;
            cursor.push(0);
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

    /// All index rows for one (keyspace, db_id), every task type — used by
    /// DROP DATABASE queue reap.
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
    // V2 due-queue dequeue + byte-safe legacy drain (issue #2576)
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
    /// one key/value pair per RPC. Used by the worker tick during the migration
    /// window to execute V1 entries an old binary may still enqueue; after V1
    /// drains this returns empty (the caller gates on `legacy_queue_has_entries`).
    pub async fn scan_due_legacy_bytesafe(
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

    /// Cheap existence check for any legacy (`_worker_queue_`) entry. Used to
    /// gate the byte-safe legacy paths so they cost a single key-only RPC once
    /// V1 has been drained.
    pub async fn legacy_queue_has_entries(&self, txn: &mut Transaction) -> Result<bool> {
        let prefix = encode_worker_queue_prefix();
        let end = encode_prefix_end(&prefix);
        let range: BoundRange = (prefix.clone()..end).into();
        let keys: Vec<Vec<u8>> = tikv_op!(txn.scan_keys(range, 1).await)?
            .map(Vec::from)
            .collect();
        Ok(keys.iter().any(|k| k.starts_with(&prefix)))
    }

    /// Byte-safe scan of legacy (`_worker_queue_`) entries matching `pred`:
    /// fetch one key/value pair per RPC, then filter. Used by targeted ops during
    /// the migration window; callers gate on `legacy_queue_has_entries` so this
    /// is never invoked once V1 is drained.
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

    /// Delete every queue entry for one task across BOTH layers: V2 (via index)
    /// and, during the migration window, legacy. Returns the number deleted.
    /// Used by cron schedule-replace and unschedule. No global due-queue scan.
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
    /// as `(legacy_due_key, task_id)`. Gated by `legacy_queue_has_entries` so it
    /// is a single key-only RPC once V1 is drained. Used by cron reconciliation
    /// to see pending pre-V2 entries that are intentionally never indexed into
    /// V2 — without this, reconcile would re-enqueue a V2 copy of a job that
    /// still has a legacy entry and double-fire it during a rolling deploy.
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

    /// Whether any pending queue entry exists for one task, across V2 and (gated)
    /// legacy. Used by bg_sql result polling and AutoAnalyze enqueue dedup. No
    /// global due-queue scan.
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
        if self.legacy_queue_has_entries(txn).await? {
            let rows = self
                .scan_legacy_filtered(txn, |e| {
                    e.task_id == task_id
                        && e.db_id == db_id
                        && e.keyspace == keyspace
                        && e.task_type == task_type
                })
                .await?;
            return Ok(!rows.is_empty());
        }
        Ok(false)
    }

    /// Delete every queue entry for an entire (keyspace, db_id) across both
    /// layers — used by DROP DATABASE so worker queue entries do not leak.
    /// Self-contained: collects the work-list with byte-safe reads, then deletes
    /// in BOUNDED batched transactions so a db with a large backlog cannot build
    /// one oversized 2PC write/lock set. Returns the number deleted. No global
    /// due-queue value scan.
    pub async fn reap_db_queue_entries(&self, keyspace: &str, db_id: u64) -> Result<usize> {
        const DELETE_BATCH: usize = 256;

        // Phase 1: collect the work-list (byte-safe: 1-byte index values + gated
        // one-value legacy scan).
        let (index_rows, legacy_keys) = {
            let mut txn = self.begin().await?;
            let idx = self.index_rows_for_db(&mut txn, keyspace, db_id).await?;
            let legacy = if self.legacy_queue_has_entries(&mut txn).await? {
                self.scan_legacy_filtered(&mut txn, |e| e.db_id == db_id && e.keyspace == keyspace)
                    .await?
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            txn.rollback().await.ok();
            (idx, legacy)
        };

        // Phase 2: delete in bounded batches (idempotent — a re-deleted key is a
        // no-op, so a retry after a partial failure is safe).
        let mut deleted = 0usize;
        for chunk in index_rows.chunks(DELETE_BATCH) {
            let mut txn = self.begin().await?;
            for r in chunk {
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
        for chunk in legacy_keys.chunks(DELETE_BATCH) {
            let mut txn = self.begin().await?;
            for key in chunk {
                self.delete_worker_queue_entry(&mut txn, key).await?;
                deleted += 1;
            }
            txn.commit().await?;
        }
        Ok(deleted)
    }

    /// Task IDs of all pending AsyncTrigger entries for a keyspace (every db),
    /// across V2 and (gated) legacy. Used by the trigger queue-stats metric;
    /// reads only identities, never command payloads.
    pub async fn pending_async_trigger_task_ids(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
    ) -> Result<Vec<i64>> {
        let async_mask = TaskType::AsyncTrigger.to_bitmask();
        let mut ids: Vec<i64> = self
            .index_rows_for_keyspace(txn, keyspace)
            .await?
            .into_iter()
            .filter(|r| r.task_type == async_mask)
            .map(|r| r.task_id)
            .collect();
        if self.legacy_queue_has_entries(txn).await? {
            let rows = self
                .scan_legacy_filtered(txn, |e| {
                    e.task_type == TaskType::AsyncTrigger && e.keyspace == keyspace
                })
                .await?;
            ids.extend(rows.into_iter().map(|(_, e)| e.task_id));
        }
        Ok(ids)
    }

    // ========================================================================
    // Claim methods
    // ========================================================================

    pub async fn try_claim_worker_task(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        claim: &WorkerClaim,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            claim.task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_ms,
        ));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Ok(false);
        }
        txn_put(
            txn,
            key,
            bincode::serialize(claim).context("Failed to serialize worker claim")?,
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
            let claim: WorkerClaim =
                bincode::deserialize(pair.value()).context("Failed to deserialize worker claim")?;
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
        let store = crate::worker::init_system_store(pd_endpoints, &cfg)
            .await
            .expect("failed to initialize system store for bg task ID concurrency test")
            .expect("worker system store should be present when enabled");

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
        crate::worker::init_system_store(pd_endpoints, &cfg)
            .await
            .expect("init system store")
            .expect("store present when enabled")
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

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn legacy_entry_is_handled_in_place_across_layers() {
        // V2 never MOVES legacy entries (that would let an old + new binary
        // execute the same task twice during a rolling deploy). Instead legacy
        // entries are visible to the byte-safe dual-read dequeue and to targeted
        // ops via the gated legacy scan, and are deleted in place.
        let store = v2_test_store().await;
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

        // Discoverable via the byte-safe legacy dequeue (no global value scan)
        // and via the gated targeted-pending check — but NOT via the V2 index
        // (it is never moved to V2).
        let mut txn = store.begin().await.unwrap();
        assert!(store.legacy_queue_has_entries(&mut txn).await.unwrap());
        let due = store
            .scan_due_legacy_bytesafe(&mut txn, i64::MAX, 1000)
            .await
            .unwrap();
        assert!(due.iter().any(|(_, e)| e.keyspace == ks
            && e.task_id == 21
            && e.command == "SELECT pg_sleep(1)"));
        assert!(store
            .task_has_pending(&mut txn, &ks, db_id, 21, TaskType::Cron)
            .await
            .unwrap());
        // Cron reconciliation relies on this to avoid re-enqueuing a V2 copy of a
        // job that still has a legacy entry (which would double-fire it).
        let legacy_cron = store
            .legacy_entries_for_db_type(&mut txn, &ks, db_id, TaskType::Cron)
            .await
            .unwrap();
        assert!(
            legacy_cron.iter().any(|(_, jid)| *jid == 21),
            "reconcile must see the legacy cron job via legacy_entries_for_db_type"
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

        // Targeted delete removes it across layers (here: the legacy layer).
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
}
