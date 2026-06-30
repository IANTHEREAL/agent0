use super::*;

/// Maximum retries for TiKV region errors (RegionNotFound, EpochNotMatch, etc.)
/// that can occur after region split/merge operations.
pub(crate) const REGION_ERROR_MAX_RETRIES: u32 = 3;

/// Returns `true` if the error originated from a TiKV region routing issue
/// (split, merge, leader transfer) that is expected to resolve on retry with
/// a fresh transaction whose region cache has been refreshed.
///
/// Excludes non-routing `RegionError` variants that tikv-client surfaces
/// without internal retry: `server_is_busy` (handled by AIMD backpressure),
/// `raft_entry_too_large` (deterministic, won't resolve on retry),
/// `max_timestamp_not_synced`, and `disk_full`.
pub(crate) fn is_retryable_region_error(err: &anyhow::Error) -> bool {
    fn is_retryable_region(err: &tikv_client::Error) -> bool {
        match err {
            tikv_client::Error::RegionError(re) => {
                // tikv-client internally retries most routing errors
                // (not_leader, epoch_not_match, region_not_found, stale_command).
                // It only surfaces these non-routing errors as RegionError:
                re.server_is_busy.is_none()
                    && re.raft_entry_too_large.is_none()
                    && re.max_timestamp_not_synced.is_none()
                    && re.disk_full.is_none()
            }
            tikv_client::Error::UndeterminedError(inner) => is_retryable_region(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                // all(): if ANY error in the batch is non-retryable (e.g. DiskFull),
                // do not retry. Matches TiKV's own aggregate error predicate semantics.
                !errors.is_empty() && errors.iter().all(is_retryable_region)
            }
            _ => false,
        }
    }

    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(is_retryable_region)
    })
}

/// Backoff sleep for region error retries: 500ms, 1s, 2s, ...
pub(crate) async fn region_error_backoff(attempt: u32) {
    let ms = 500u64 * (1u64 << attempt.min(4));
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

pub(super) fn should_start_cic_backfill(state: IndexState) -> bool {
    matches!(state, IndexState::Building)
}

pub(super) fn parse_backfill_index_command(command: &str) -> Result<(String, String)> {
    let args = command
        .strip_prefix("__backfill_index ")
        .ok_or_else(|| anyhow!("invalid backfill command: {}", command))?;
    let mut parts = args.split_whitespace();
    let table_name = parts
        .next()
        .ok_or_else(|| anyhow!("missing table name in backfill command"))?;
    let index_name = parts
        .next()
        .ok_or_else(|| anyhow!("missing index name in backfill command"))?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid backfill command args: {}", command));
    }
    Ok((table_name.to_string(), index_name.to_string()))
}

pub(super) fn parse_hnsw_merge_command(command: &str) -> Result<(u64, u64)> {
    let args = command
        .strip_prefix("__hnsw_merge ")
        .ok_or_else(|| anyhow!("invalid hnsw_merge command: {}", command))?;
    let mut parts = args.split_whitespace();
    let table_id: u64 = parts
        .next()
        .ok_or_else(|| anyhow!("missing table_id in hnsw_merge command"))?
        .parse()?;
    let index_id: u64 = parts
        .next()
        .ok_or_else(|| anyhow!("missing index_id in hnsw_merge command"))?
        .parse()?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid hnsw_merge command args: {}", command));
    }
    Ok((table_id, index_id))
}

/// Build the extension context a worker statement runs in.
///
/// `username` is the originating principal captured at enqueue time
/// (`TaskQueueEntry.username`). It's the same value the worker passes
/// into `QueryContext` for `current_user`; threading it here also lets
/// `init_juicefs_backend` derive the fs-plane scp without forcing every
/// trigger / background-SQL call site to know the rule. Cron tasks
/// don't carry a principal — they can't reach JuiceFS by design.
pub(super) fn background_statement_extension_context(
    is_cron: bool,
    keyspace: &str,
    username: &str,
    tikv_client: Option<Arc<tikv_client::TransactionClient>>,
) -> ExtensionContextOpts {
    if is_cron {
        ExtensionContextOpts::cron(keyspace).with_tikv_client(tikv_client)
    } else {
        ExtensionContextOpts::statement(true, true, keyspace)
            .with_tikv_client(tikv_client)
            .with_authenticated_role(Some(username.to_string()))
    }
}

/// Maximum deltas to process in a single merge transaction.
const MERGE_BATCH_SIZE: usize = 5000;

/// Maximum serialized graph size (bytes) before freezing the index.
/// Set below TiKV's default `raft-entry-max-size` (16 MB) with margin.
pub(crate) const HNSW_GRAPH_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Returns `true` if the merge should be skipped because the index is frozen.
/// Used at the top of `execute_hnsw_merge` and testable independently.
///
/// When S3 offload is enabled, frozen indexes can be merged again because
/// the graph blob goes to S3 (no TiKV raft-entry-max-size concern).
pub(crate) fn should_skip_frozen_merge(meta: &crate::sql::hnsw::storage::HnswMeta) -> bool {
    if crate::sql::hnsw::s3::hnsw_s3_client().is_some() {
        return false;
    }
    meta.frozen
}

/// Checks whether a serialized graph exceeds the safe size limit and should
/// trigger a freeze. Returns `Some(frozen_meta_bytes)` if the index must be
/// frozen (caller should persist these bytes and abort the merge), or `None`
/// if the graph is within limits.
pub(crate) fn check_graph_oversize_freeze(
    graph_len: usize,
    meta: &crate::sql::hnsw::storage::HnswMeta,
) -> Option<Vec<u8>> {
    if graph_len <= HNSW_GRAPH_MAX_BYTES {
        return None;
    }
    let mut frozen_meta = meta.clone();
    frozen_meta.frozen = true;
    serde_json::to_vec(&frozen_meta).ok()
}

/// Allocate an HNSW S3 graph object version from the source transaction's PD
/// TSO. The version is part of the external object key, so it must be unique
/// per writer and monotonic relative to the currently published graph.
pub(crate) fn hnsw_s3_graph_version_for_txn(
    txn: &tikv_client::Transaction,
    previous_graph_version: u64,
) -> Result<u64> {
    let version = txn.start_timestamp().version();
    if version <= previous_graph_version {
        return Err(anyhow!(
            "HNSW S3 graph version invariant violated: transaction TSO {} is not newer than \
             current graph_version {}",
            version,
            previous_graph_version
        ));
    }
    Ok(version)
}

pub(crate) fn should_delete_legacy_tikv_graph_after_s3_migration(
    previous_graph_version: u64,
) -> bool {
    previous_graph_version == 0
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn put_hnsw_s3_graph_with_intent(
    tenant_store: &TikvStore,
    tenant_txn: &mut tikv_client::Transaction,
    keyspace: &str,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    version: u64,
    data: bytes::Bytes,
    reason: &str,
) -> Result<()> {
    let s3 = crate::sql::hnsw::s3::hnsw_s3_client()
        .ok_or_else(|| anyhow!("HNSW S3 client not available"))?;
    let system_store = crate::worker::system_store()?;
    let intent = crate::worker::types::HnswS3GraphUploadIntent::new(
        keyspace.to_string(),
        db_id,
        table_id,
        index_id,
        version,
        tenant_txn.start_timestamp().version(),
        reason.to_string(),
    );

    let mut sys_txn = system_store.begin().await?;
    system_store
        .put_hnsw_s3_graph_upload_intent(&mut sys_txn, &intent)
        .await?;
    sys_txn.commit().await?;

    tenant_store
        .assert_database_alive_for_update(tenant_txn, db_id)
        .await?;

    if let Err(e) = s3
        .put_graph(keyspace, db_id, table_id, index_id, version, data)
        .await
    {
        delete_hnsw_s3_graph_upload_intent_best_effort(
            keyspace, db_id, table_id, index_id, version,
        )
        .await;
        return Err(anyhow!("HNSW S3 put_graph failed: {}", e));
    }

    Ok(())
}

pub(crate) async fn cleanup_hnsw_s3_graph_upload_after_failed_txn(
    keyspace: &str,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    version: u64,
) {
    let Some(s3) = crate::sql::hnsw::s3::hnsw_s3_client() else {
        return;
    };

    match s3
        .delete_graph(keyspace, db_id, table_id, index_id, version)
        .await
    {
        Ok(()) => {
            delete_hnsw_s3_graph_upload_intent_best_effort(
                keyspace, db_id, table_id, index_id, version,
            )
            .await;
        }
        Err(e) => {
            warn!(
                keyspace,
                db_id,
                table_id,
                index_id,
                version,
                error = %e,
                "HNSW S3 speculative graph cleanup failed; durable intent retained for GC"
            );
        }
    }
}

async fn delete_hnsw_s3_graph_upload_intent_best_effort(
    keyspace: &str,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    version: u64,
) {
    let Ok(system_store) = crate::worker::system_store() else {
        return;
    };
    let Ok(mut txn) = system_store.begin().await else {
        return;
    };
    if system_store
        .delete_hnsw_s3_graph_upload_intent(&mut txn, keyspace, db_id, table_id, index_id, version)
        .await
        .is_ok()
    {
        let _ = txn.commit().await;
    } else {
        let _ = txn.rollback().await;
    }
}

async fn cleanup_uploaded_hnsw_s3_graph_after_failed_batch(
    keyspace: &str,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    uploaded_version: Option<u64>,
) {
    let Some(version) = uploaded_version else {
        return;
    };
    cleanup_hnsw_s3_graph_upload_after_failed_txn(keyspace, db_id, table_id, index_id, version)
        .await;
}

fn retain_uploaded_hnsw_s3_graph_after_uncertain_commit(
    keyspace: &str,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    uploaded_version: Option<u64>,
    error: &anyhow::Error,
) {
    let Some(version) = uploaded_version else {
        return;
    };
    warn!(
        keyspace,
        db_id,
        table_id,
        index_id,
        version,
        error = %error,
        "HNSW S3 graph upload retained after TiKV commit error; durable intent retained for safepoint GC reconciliation"
    );
}

/// Execute HNSW merge in batches. Each batch is a separate TiKV transaction
/// that processes up to MERGE_BATCH_SIZE deltas, writes the consolidated base
/// graph, deletes consumed deltas, and updates meta — all atomically.
///
/// If more deltas remain after a batch, loops with a new transaction.
/// Crashes between batches lose no data (uncommitted deltas remain in TiKV).
pub(super) async fn execute_hnsw_merge(
    store: &Arc<TikvStore>,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    lease_cancel: &crate::worker::LeaseCancel,
) -> Result<()> {
    use crate::sql::hnsw::storage::{
        create_empty_hnsw_index, delete_delta_keys, hnsw_delta_prefix, hnsw_delta_prefix_end,
        hnsw_dirty_key, hnsw_graph_key, hnsw_meta_key, hnsw_s3_retired_version_key,
        load_base_graph, serialize_hnsw_snapshot, HnswDelta, HnswMeta, HnswS3RetiredVersionGc,
    };
    use crate::txn::{txn_delete, txn_put};
    use tikv_client::BoundRange;

    let merge_start = std::time::Instant::now();
    let mut total_deltas_merged: usize = 0;

    // Capability pre-check: if this index requires S3 but this worker
    // doesn't have S3 configured, fail early before acquiring any locks.
    // This prevents a non-S3 worker from holding a pessimistic lock on
    // the meta key while it discovers it can't serve the merge.
    if crate::sql::hnsw::s3::hnsw_s3_client().is_none() {
        let mut pre_txn = store.begin().await?;
        let pre_key = hnsw_meta_key(db_id, table_id, index_id);
        if let Some(pre_bytes) = pre_txn.get(pre_key).await? {
            if let Ok(pre_meta) = serde_json::from_slice::<HnswMeta>(&pre_bytes) {
                if pre_meta.storage_version >= 2 || pre_meta.graph_version > 0 {
                    pre_txn.rollback().await.ok();
                    tracing::debug!(
                        table_id,
                        index_id,
                        graph_version = pre_meta.graph_version,
                        "Skipping HNSW merge: index requires S3 but this worker has no S3 client"
                    );
                    return Ok(());
                }
            }
        }
        pre_txn.rollback().await.ok();
    }

    let mut region_retries = 0u32;
    loop {
        // Loop-top lease fence (defense-in-depth). Each iteration opens a fresh
        // txn and commits one batch (graph blob + meta) at helpers.rs §7. A merge
        // can run for many batches, long enough for the claim lease to lapse
        // mid-run; if the renewer already cancelled `lease_cancel` we abandon this
        // batch WITHOUT starting its txn and leave the remaining deltas for the new
        // owner. This check alone is NOT load-bearing: the lease can also lapse
        // AFTER this point, during the iteration's long work (delta scan, graph
        // build/serialize, S3 upload, TiKV mutations). The load-bearing fences are
        // the COMMIT-ADJACENT checks immediately before each tenant commit below
        // (oversize-freeze early commit + normal batch commit), which run after all
        // long work and clean up any speculative S3 upload on bail. Deltas already
        // merged in prior committed batches stay merged (idempotent: their delta
        // keys were deleted).
        lease_cancel.bail_if_cancelled()?;

        // Each iteration processes one batch inside a fresh transaction.
        // Wrap in an async block so region errors can be caught and retried
        // with a new transaction (fresh region cache) instead of propagating
        // permanently — fixes #2271.
        let batch_result: Result<Option<usize>> = async {
            let mut txn = store.begin().await?;
            let mut txn_guard = crate::worker::active_txn_registry::global_registry()
                .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));

            // 1. Read meta with pessimistic lock.
            // get_for_update on the HNSW meta key is the primary per-index merge
            // serialization guard. Worker claims and descriptor backoff reduce
            // duplicate dispatch, but this lock is the source of truth for
            // graph_version and delta application ordering.
            let meta_key = hnsw_meta_key(db_id, table_id, index_id);
            let dirty_key = hnsw_dirty_key(db_id, table_id, index_id);
            let dirty_marker_at_start = txn.get(dirty_key.clone()).await?;
            let Some(meta_bytes) = txn.get_for_update(meta_key.clone()).await? else {
                // Index metadata missing — index was dropped. Abort silently.
                if txn.get_for_update(dirty_key.clone()).await? == dirty_marker_at_start {
                    txn_delete(&mut txn, dirty_key).await?;
                }
                if txn.commit().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                return Ok(None);
            };
            let meta: HnswMeta = serde_json::from_slice(&meta_bytes)?;
            if should_skip_frozen_merge(&meta) {
                if txn.rollback().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                info!(table_id, index_id, "HNSW merge skipped: index is frozen");
                return Ok(None);
            }
            if meta.storage_version != 1 && meta.storage_version != 2 {
                if txn.rollback().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                return Err(anyhow!(
                    "HNSW index has unsupported storage_version={}; only v1/v2 supported",
                    meta.storage_version
                ));
            }

            // 2. Scan up to MERGE_BATCH_SIZE delta keys.
            let prefix = hnsw_delta_prefix(db_id, table_id, index_id);
            let end = hnsw_delta_prefix_end(db_id, table_id, index_id);
            let mut batch_keys: Vec<Vec<u8>> = Vec::new();
            let mut batch_deltas: Vec<HnswDelta> = Vec::new();
            let mut scan_start = prefix.clone();

            while batch_deltas.len() < MERGE_BATCH_SIZE {
                let remaining = (MERGE_BATCH_SIZE - batch_deltas.len()) as u32;
                let scan_limit = remaining.min(1024);
                let range: BoundRange = (scan_start.clone()..end.clone()).into();
                let pairs: Vec<tikv_client::KvPair> = txn.scan(range, scan_limit).await?.collect();
                let page_count = pairs.len();
                if page_count == 0 {
                    break;
                }

                for pair in pairs {
                    let k: &[u8] = pair.key().as_ref().into();
                    let key: Vec<u8> = k.to_vec();
                    if !key.starts_with(&prefix) {
                        break;
                    }
                    let delta: HnswDelta = bincode::deserialize(pair.value())?;
                    scan_start = key.clone();
                    scan_start.push(0x00);
                    batch_keys.push(key);
                    batch_deltas.push(delta);
                    if batch_deltas.len() >= MERGE_BATCH_SIZE {
                        break;
                    }
                }
                if (page_count as u32) < scan_limit {
                    break;
                }
            }

            if batch_deltas.is_empty() {
                if txn.get_for_update(dirty_key.clone()).await? == dirty_marker_at_start {
                    txn_delete(&mut txn, dirty_key).await?;
                }
                if txn.commit().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                return Ok(None); // No more deltas — merge complete.
            }

            let batch_count = batch_deltas.len();

            // 3. Load base graph (or create empty if none exists yet).
            let keyspace = store.keyspace().unwrap_or("default");
            let (index, _): (crate::sql::hnsw::HnswIndexHandle, _) = match load_base_graph(
                &mut txn, db_id, table_id, index_id, &meta, keyspace,
            )
            .await?
            {
                Some(pair) => pair,
                None => create_empty_hnsw_index(
                    meta.dimensions,
                    &meta.distance_metric,
                    meta.m,
                    meta.ef_construction,
                )?,
            };

            // 4. Reserve capacity + apply deltas.
            let needed = index.size() as u64 + batch_count as u64;
            if needed > index.capacity() as u64 {
                let next_cap = needed.saturating_mul(2).max(1);
                index
                    .reserve(next_cap as usize)
                    .map_err(|e| anyhow!("HNSW reserve failed: {}", e))?;
            }
            for delta in &batch_deltas {
                index
                    .add(delta.label, &delta.vector)
                    .map_err(|e| anyhow!("HNSW add failed: {}", e))?;
            }

            // 5. Serialize new base graph.
            let mut updated_meta = meta.clone();
            updated_meta.count = index.size() as u64;
            updated_meta.capacity = index.capacity() as u64;
            let (graph_bytes, _meta_bytes_tikv) =
                serialize_hnsw_snapshot(db_id, table_id, index_id, index.deref(), &updated_meta)?;

            // 5a. S3 vs TiKV write path for the graph blob.
            let mut uploaded_s3_graph_version: Option<u64> = None;
            if crate::sql::hnsw::s3::hnsw_s3_client().is_some() {
                // S3 path: upload graph to S3, increment graph_version.
                let previous_version = updated_meta.graph_version;
                let new_version = hnsw_s3_graph_version_for_txn(&txn, previous_version)?;
                put_hnsw_s3_graph_with_intent(
                    store,
                    &mut txn,
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    new_version,
                    bytes::Bytes::from(graph_bytes),
                    "hnsw_merge",
                )
                .await
                .map_err(|e| anyhow!("HNSW S3 merge upload failed: {}", e))?;
                uploaded_s3_graph_version = Some(new_version);
                updated_meta.graph_version = new_version;
                // First S3 write: upgrade storage_version to 2.
                if updated_meta.storage_version == 1 {
                    updated_meta.storage_version = 2;
                }
                // Unfreeze if previously frozen (S3 has no size limit concern).
                updated_meta.frozen = false;
                // Re-serialize meta with updated graph_version/storage_version.
                let meta_bytes_s3 = serde_json::to_vec(&updated_meta)?;
                let s3_tikv_mutation_result: Result<()> = async {
                    txn_put(&mut txn, meta_key, meta_bytes_s3).await?;
                    if previous_version > 0 {
                        let marker = HnswS3RetiredVersionGc {
                            delete_after_safepoint: None,
                        };
                        txn_put(
                            &mut txn,
                            hnsw_s3_retired_version_key(
                                db_id,
                                table_id,
                                index_id,
                                previous_version,
                            ),
                            serde_json::to_vec(&marker)?,
                        )
                        .await?;
                    }
                    // On first migration (old graph was in TiKV), delete the stale
                    // TiKV graph blob. Safe: concurrent queries at older snapshots
                    // still see it via MVCC; no future query will read it since
                    // graph_version > 0 routes to S3.
                    if should_delete_legacy_tikv_graph_after_s3_migration(previous_version) {
                        let graph_key =
                            crate::sql::hnsw::storage::hnsw_graph_key(db_id, table_id, index_id);
                        crate::txn::txn_delete(&mut txn, graph_key).await?;
                    }
                    Ok(())
                }
                .await;
                if let Err(e) = s3_tikv_mutation_result {
                    cleanup_uploaded_hnsw_s3_graph_after_failed_batch(
                        keyspace,
                        db_id,
                        table_id,
                        index_id,
                        uploaded_s3_graph_version.take(),
                    )
                    .await;
                    return Err(e);
                }
                // Skip TiKV graph write and oversize check — graph is in S3.
            } else {
                // TiKV path: check oversize freeze, then write graph to TiKV.
                if let Some(frozen_meta_bytes) =
                    check_graph_oversize_freeze(graph_bytes.len(), &updated_meta)
                {
                    warn!(
                        table_id,
                        index_id,
                        graph_bytes = graph_bytes.len(),
                        limit = HNSW_GRAPH_MAX_BYTES,
                        "HNSW graph exceeds size limit — freezing index"
                    );
                    txn_put(&mut txn, meta_key, frozen_meta_bytes).await?;
                    // Commit-adjacent lease fence: the work above (delta scan, graph
                    // build/serialize) can outlive the claim lease. Check the cancel
                    // token IMMEDIATELY before this tenant commit — the loop-top check
                    // is not load-bearing here because the lease can lapse during that
                    // long work. On cancellation we abandon WITHOUT committing and
                    // leave the task for the new owner (at-most-once). This branch is
                    // the TiKV write path, so no S3 graph was speculatively uploaded
                    // (`uploaded_s3_graph_version` is always None here). Roll the
                    // pessimistic txn back so its meta `get_for_update` lock is
                    // released IMMEDIATELY (not left to lock-TTL), letting the new
                    // owner re-run the merge without waiting; on rollback failure
                    // quarantine the tracked txn so GC does not advance past it.
                    if let Err(e) = lease_cancel.bail_if_cancelled() {
                        if txn.rollback().await.is_err() {
                            if let Some(g) = txn_guard.as_mut() {
                                g.quarantine();
                            }
                        }
                        cleanup_uploaded_hnsw_s3_graph_after_failed_batch(
                            keyspace,
                            db_id,
                            table_id,
                            index_id,
                            uploaded_s3_graph_version.take(),
                        )
                        .await;
                        return Err(e);
                    }
                    store
                        .assert_database_alive_for_update(&mut txn, db_id)
                        .await?;
                    txn.commit().await?;
                    return Ok(None);
                }

                // 6. Atomic write: new base graph + update meta.
                txn_put(
                    &mut txn,
                    hnsw_graph_key(db_id, table_id, index_id),
                    graph_bytes,
                )
                .await?;
                let meta_bytes_new = serde_json::to_vec(&updated_meta)?;
                txn_put(&mut txn, meta_key, meta_bytes_new).await?;
            }
            if let Err(e) = delete_delta_keys(&mut txn, &batch_keys).await {
                cleanup_uploaded_hnsw_s3_graph_after_failed_batch(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    uploaded_s3_graph_version.take(),
                )
                .await;
                return Err(e.into());
            }
            if batch_count < MERGE_BATCH_SIZE
                && txn.get_for_update(dirty_key.clone()).await? == dirty_marker_at_start
            {
                txn_delete(&mut txn, dirty_key).await?;
            }

            // 7. Commit.
            // Commit-adjacent lease fence: all long-running work for this batch
            // (delta scan, graph build/serialize, S3 upload, TiKV mutations) is
            // done. The claim lease can lapse DURING that work — most likely in the
            // S3-upload window — so the loop-top check is not load-bearing here.
            // Check the cancel token IMMEDIATELY before the tenant commit. A bail is
            // a DEFINITE non-commit (txn.commit has not run), which is categorically
            // different from the commit-ERROR path below: there we retain the upload
            // (the commit is AMBIGUOUS and may have landed); here we MUST clean up
            // the speculative S3 graph (it will never be referenced by committed
            // meta) so the new owner can re-run the merge and re-upload without
            // leaving a no-meta S3 orphan. Mirrors the delete_delta_keys failure
            // path. The canonical claim-cancelled error makes is_claim_cancelled_error
            // classify it so the caller does not mark anything invalid. Roll the
            // pessimistic txn back so its meta `get_for_update` lock is released
            // IMMEDIATELY (not left to lock-TTL) for the new owner; quarantine on
            // rollback failure so GC does not advance past the tracked txn.
            if let Err(e) = lease_cancel.bail_if_cancelled() {
                if txn.rollback().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                cleanup_uploaded_hnsw_s3_graph_after_failed_batch(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    uploaded_s3_graph_version.take(),
                )
                .await;
                return Err(e);
            }

            // Pre-commit liveness fence. DROP DATABASE removes the database
            // metadata row before destroying its key range; this get_for_update
            // read conflicts with that drop and aborts the merge if the database
            // is gone. This MUST run as a separate fallible step BEFORE
            // txn.commit(): an error here is a DEFINITE non-commit (the database
            // is dropped, or get_for_update itself failed — either way commit has
            // not run), categorically the same as the bail / delete-delta-keys
            // failure paths above. So we clean up the speculative S3 graph (it
            // will never be referenced by committed meta) rather than retaining
            // it, which would leave a no-meta S3 orphan after a DROP DATABASE
            // race until safepoint GC. Only txn.commit() itself is ambiguous, so
            // only its error reaches the retain path below.
            if let Err(e) = store
                .assert_database_alive_for_update(&mut txn, db_id)
                .await
            {
                if txn.rollback().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                cleanup_uploaded_hnsw_s3_graph_after_failed_batch(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    uploaded_s3_graph_version.take(),
                )
                .await;
                return Err(e);
            }

            if let Err(e) = txn.commit().await {
                let e = anyhow::Error::from(e);
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
                // Commit errors can be ambiguous: TiKV may have committed even
                // when the client sees an error. Do not delete the external
                // object or its durable intent here; GC resolves it after the
                // source txn crosses the safepoint by reading committed meta.
                retain_uploaded_hnsw_s3_graph_after_uncertain_commit(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    uploaded_s3_graph_version.take(),
                    &e,
                );
                return Err(e);
            }

            info!(
                table_id,
                index_id,
                batch_count,
                graph_size = index.size(),
                "HNSW merge batch committed"
            );

            Ok(Some(batch_count))
        }
        .await;

        match batch_result {
            Ok(Some(batch_count)) => {
                region_retries = 0;
                total_deltas_merged += batch_count;
                // If we got fewer than MERGE_BATCH_SIZE deltas, no more remain.
                if batch_count < MERGE_BATCH_SIZE {
                    break;
                }
            }
            Ok(None) => break,
            Err(e)
                if is_retryable_region_error(&e) && region_retries < REGION_ERROR_MAX_RETRIES =>
            {
                region_retries += 1;
                warn!(
                    table_id,
                    index_id,
                    attempt = region_retries,
                    max_retries = REGION_ERROR_MAX_RETRIES,
                    "HNSW merge: region error, retrying with fresh transaction: {e}"
                );
                region_error_backoff(region_retries - 1).await;
            }
            Err(e) => return Err(e),
        }
    }

    if total_deltas_merged > 0 {
        info!(
            table_id,
            index_id,
            total_deltas_merged,
            elapsed_ms = merge_start.elapsed().as_millis() as u64,
            "HNSW merge complete"
        );
    }
    Ok(())
}
