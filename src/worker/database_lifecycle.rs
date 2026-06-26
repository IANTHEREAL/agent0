use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::pool::TikvClientPool;
use crate::storage::{
    DatabaseDrainRequest, DatabaseDrainState, DatabaseNodeLease, FencedDatabase, TikvStore,
};
use crate::worker::config::WorkerConfig;

const NODE_LEASE_TTL_MS: i64 = 30_000;
const NODE_LEASE_GUARD_MS: i64 = 5_000;
const NODE_LEASE_PUBLISH_INTERVAL_MS: u64 = 10_000;
const NODE_LEASE_TSO_TIMEOUT_SEC: u64 = 30;
const DROP_COORDINATOR_CLAIM_LEASE_MS: i64 = 300_000;
const DRAIN_REGISTRY_SCAN_PAGE_SIZE: usize = 256;
const DRAIN_REQUEST_TTL_MS: i64 = 3_600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseLifecycleNodeIdentity {
    pub node_id: String,
    pub generation: u64,
}

static NODE_IDENTITY: OnceLock<DatabaseLifecycleNodeIdentity> = OnceLock::new();
static DATABASE_LIFECYCLE_ACCEPTS_TRAFFIC: AtomicBool = AtomicBool::new(true);

fn lifecycle_generation(config: &WorkerConfig) -> u64 {
    uuid::Uuid::parse_str(&config.gc_instance_id)
        .map(|uuid| {
            let bytes = uuid.as_bytes();
            u64::from_be_bytes(bytes[..8].try_into().expect("uuid has at least 8 bytes"))
        })
        .unwrap_or_else(|_| {
            let mut hash = 0xcbf29ce484222325_u64;
            for byte in config.gc_instance_id.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash.max(1)
        })
}

fn node_identity(config: &WorkerConfig) -> DatabaseLifecycleNodeIdentity {
    DatabaseLifecycleNodeIdentity {
        node_id: config.worker_id.clone(),
        generation: lifecycle_generation(config),
    }
}

fn remember_node_identity(config: &WorkerConfig) -> DatabaseLifecycleNodeIdentity {
    let identity = node_identity(config);
    let _ = NODE_IDENTITY.set(identity.clone());
    NODE_IDENTITY.get().cloned().unwrap_or(identity)
}

pub(crate) fn current_node_identity() -> Result<DatabaseLifecycleNodeIdentity> {
    NODE_IDENTITY
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("database lifecycle node identity has not been published"))
}

fn set_database_lifecycle_accepts_traffic(accepts: bool) {
    DATABASE_LIFECYCLE_ACCEPTS_TRAFFIC.store(accepts, Ordering::SeqCst);
}

pub(crate) fn ensure_database_lifecycle_accepts_traffic() -> Result<()> {
    if DATABASE_LIFECYCLE_ACCEPTS_TRAFFIC.load(Ordering::SeqCst) {
        return Ok(());
    }
    Err(crate::sql::error::SqlError::ObjectInUse {
        message: "this db9 node cannot serve database traffic because its lifecycle lease is not current; retry on another node".to_string(),
    }
    .into())
}

fn self_fence_delay_ms(lease: &DatabaseNodeLease, guard_ms: i64) -> u64 {
    lease
        .self_fence_deadline_ms(guard_ms)
        .saturating_sub(lease.published_at_ms)
        .max(0) as u64
}

async fn pd_now_ms(store: &TikvStore) -> Result<i64> {
    let client = store
        .transaction_client()
        .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;
    let ts = client
        .current_timestamp_with_timeout(Duration::from_secs(NODE_LEASE_TSO_TIMEOUT_SEC))
        .await
        .map_err(|e| anyhow::anyhow!("failed to get database lifecycle node lease TSO: {e}"))?;
    Ok(ts.physical)
}

/// Publish this SQL-serving process's database lifecycle lease once.
///
/// Startup calls this synchronously before SQL/API/FS listeners accept traffic,
/// so a drop coordinator can reason about all live nodes through TiKV metadata.
pub(crate) async fn publish_database_node_lease_once(
    store: &TikvStore,
    config: &WorkerConfig,
    accepts_sql: bool,
) -> Result<DatabaseNodeLease> {
    let identity = remember_node_identity(config);
    let now_ms = pd_now_ms(store).await?;
    let lease = DatabaseNodeLease {
        node_id: identity.node_id,
        generation: identity.generation,
        lease_until_ms: now_ms.saturating_add(NODE_LEASE_TTL_MS),
        published_at_ms: now_ms,
        accepts_sql,
    };

    let mut txn = store.begin().await?;
    store.publish_database_node_lease(&mut txn, &lease).await?;
    txn.commit()
        .await
        .context("failed to commit database lifecycle node lease")?;
    if accepts_sql {
        set_database_lifecycle_accepts_traffic(true);
    }
    Ok(lease)
}

pub(crate) async fn publish_database_drain_state_once(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
    active_old_epoch_ops: u64,
    drained: bool,
) -> Result<()> {
    let identity = current_node_identity()?;
    let now_ms = pd_now_ms(system_store).await?;
    let state = DatabaseDrainState {
        keyspace: crate::worker::canonical_registry_keyspace(keyspace),
        db_id,
        epoch,
        node_id: identity.node_id,
        generation: identity.generation,
        observed_at_ms: now_ms,
        active_old_epoch_ops,
        drained,
    };

    let mut txn = system_store.begin().await?;
    system_store
        .put_database_drain_state(&mut txn, &state)
        .await?;
    txn.commit()
        .await
        .context("failed to commit database lifecycle drain state")?;
    Ok(())
}

pub(crate) async fn request_database_drain_once(
    system_store: &TikvStore,
    keyspace: &str,
    fenced_databases: &[FencedDatabase],
) -> Result<()> {
    if fenced_databases.is_empty() {
        return Ok(());
    }

    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let now_ms = pd_now_ms(system_store).await?;
    let mut txn = system_store.begin().await?;
    for fenced in fenced_databases {
        system_store
            .put_database_drain_request(&mut txn, &keyspace, fenced.db_id, fenced.epoch, now_ms)
            .await?;
    }
    txn.commit()
        .await
        .context("failed to commit database drain requests")?;
    Ok(())
}

pub(crate) async fn clear_database_drain_requests_once(
    system_store: &TikvStore,
    keyspace: &str,
    fenced_databases: &[FencedDatabase],
) -> Result<()> {
    if fenced_databases.is_empty() {
        return Ok(());
    }

    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let mut txn = system_store.begin().await?;
    for fenced in fenced_databases {
        system_store
            .delete_database_drain_request(&mut txn, &keyspace, fenced.db_id, fenced.epoch)
            .await?;
    }
    txn.commit()
        .await
        .context("failed to clear database drain requests")?;
    Ok(())
}

async fn clear_database_drain_request_once(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
) -> Result<()> {
    let mut txn = system_store.begin().await?;
    system_store
        .delete_database_drain_request(&mut txn, keyspace, db_id, epoch)
        .await?;
    txn.commit()
        .await
        .context("failed to clear database drain request")?;
    Ok(())
}

fn drain_request_expired(now_ms: i64, requested_at_ms: i64) -> bool {
    now_ms.saturating_sub(requested_at_ms) > DRAIN_REQUEST_TTL_MS
}

pub(crate) async fn fenced_databases_read_drain_allows_delete(
    system_store: &TikvStore,
    keyspace: &str,
    fenced_databases: &[FencedDatabase],
) -> Result<bool> {
    for fenced in fenced_databases {
        if !database_read_drain_allows_drop(system_store, keyspace, fenced.db_id, fenced.epoch)
            .await?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) async fn publish_requested_database_drain_states_once(
    system_store: &TikvStore,
    client_pool: &TikvClientPool,
) -> Result<usize> {
    let mut requests_by_keyspace: HashMap<String, Vec<DatabaseDrainRequest>> = HashMap::new();
    for request in {
        let mut txn = system_store.begin_optimistic().await?;
        let requests = system_store.list_database_drain_requests(&mut txn).await?;
        txn.rollback().await.ok();
        requests
    } {
        requests_by_keyspace
            .entry(request.keyspace.clone())
            .or_default()
            .push(request);
    }
    let now_ms = pd_now_ms(system_store).await?;

    let registry = crate::admin::global_session_registry();
    let control = crate::admin::control::AdminControlService::new(registry);
    let mut published = 0usize;

    for (keyspace, requests) in requests_by_keyspace {
        let tenant_store = match client_pool
            .open_keyspace_without_bootstrap(keyspace.clone())
            .await
        {
            Ok(store) => store,
            Err(e) => {
                let missing_keyspace = e.to_string().contains("does not exist");
                for request in requests {
                    let expired = drain_request_expired(now_ms, request.requested_at_ms);
                    if missing_keyspace || expired {
                        warn!(
                            keyspace = %keyspace,
                            db_id = request.db_id,
                            epoch = request.epoch,
                            expired,
                            "clearing database drain request because tenant keyspace cannot be validated: {e}"
                        );
                        clear_database_drain_request_once(
                            system_store,
                            &keyspace,
                            request.db_id,
                            request.epoch,
                        )
                        .await?;
                    } else {
                        warn!(
                            keyspace = %keyspace,
                            db_id = request.db_id,
                            epoch = request.epoch,
                            "skipping database drain request until tenant keyspace can be validated: {e}"
                        );
                    }
                }
                continue;
            }
        };

        let mut valid_requests = Vec::new();
        for request in requests {
            if drain_request_expired(now_ms, request.requested_at_ms)
                || !tenant_store
                    .database_is_fencing_epoch(request.db_id, request.epoch)
                    .await?
            {
                clear_database_drain_request_once(
                    system_store,
                    &keyspace,
                    request.db_id,
                    request.epoch,
                )
                .await?;
            } else {
                valid_requests.push(request);
            }
        }
        if valid_requests.is_empty() {
            continue;
        }

        let active_sessions = registry.count_by_tenant(&keyspace) as u64;
        if active_sessions > 0 {
            if let Err(e) = control.terminate_all(
                &keyspace,
                "database lifecycle drain publisher",
                Some("tenant delete fence"),
            ) {
                for request in &valid_requests {
                    warn!(
                        keyspace = %keyspace,
                        db_id = request.db_id,
                        epoch = request.epoch,
                        "Database lifecycle drain publisher failed to terminate sessions: {e}"
                    );
                }
            }
        }

        for request in valid_requests {
            let active_ops = active_sessions
                + crate::sql::session::db_connections::db_connection_registry()
                    .active_operation_count(&keyspace, request.db_id) as u64;
            publish_database_drain_state_once(
                system_store,
                &keyspace,
                request.db_id,
                request.epoch,
                active_ops,
                active_ops == 0,
            )
            .await?;
            published += 1;
        }
    }

    Ok(published)
}

pub(crate) async fn publish_current_keyspace_fencing_drain_states_once(
    system_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
) -> Result<usize> {
    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let mut cursor: Option<Vec<u8>> = None;
    let mut published = 0usize;

    loop {
        let (entries, next_cursor) = {
            let mut txn = system_store.begin_optimistic().await?;
            let page = system_store
                .scan_worker_registry_page(
                    &mut txn,
                    cursor.as_deref(),
                    DRAIN_REGISTRY_SCAN_PAGE_SIZE,
                )
                .await?;
            txn.rollback().await.ok();
            page
        };

        if entries.is_empty() {
            break;
        }

        for entry in entries.iter().filter(|entry| entry.keyspace == keyspace) {
            let Some(epoch) = tenant_store.database_fencing_epoch(entry.db_id).await? else {
                continue;
            };
            let active_ops = crate::sql::session::db_connections::db_connection_registry()
                .active_operation_count(&keyspace, entry.db_id) as u64;
            publish_database_drain_state_once(
                system_store,
                &keyspace,
                entry.db_id,
                epoch,
                active_ops,
                active_ops == 0,
            )
            .await?;
            published += 1;
            if active_ops == 0 {
                match try_complete_fenced_database_drop(
                    system_store,
                    tenant_store,
                    &keyspace,
                    entry.db_id,
                    epoch,
                    "database lifecycle background coordinator",
                )
                .await
                {
                    Ok(true) => {
                        info!(
                            keyspace = %keyspace,
                            db_id = entry.db_id,
                            epoch,
                            "Completed fenced database drop from lifecycle coordinator"
                        );
                    }
                    Ok(false) => {}
                    Err(e) => warn!(
                        keyspace = %keyspace,
                        db_id = entry.db_id,
                        epoch,
                        "Database lifecycle coordinator could not complete fenced drop: {e}"
                    ),
                }
            }
        }

        cursor = next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(published)
}

fn live_undrained_leases<'a>(
    leases: &'a [DatabaseNodeLease],
    drains: &[DatabaseDrainState],
    now_ms: i64,
    guard_ms: i64,
) -> Vec<&'a DatabaseNodeLease> {
    let drained_by_generation: HashMap<(&str, u64), bool> = drains
        .iter()
        .filter(|state| state.is_drained())
        .map(|state| ((state.node_id.as_str(), state.generation), true))
        .collect();

    leases
        .iter()
        .filter(|lease| lease.accepts_sql)
        .filter(|lease| lease.is_live_at_ms(now_ms, guard_ms))
        .filter(|lease| {
            !drained_by_generation
                .get(&(lease.node_id.as_str(), lease.generation))
                .copied()
                .unwrap_or(false)
        })
        .collect()
}

pub(crate) async fn database_read_drain_allows_drop(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
) -> Result<bool> {
    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let now_ms = pd_now_ms(system_store).await?;
    let mut txn = system_store.begin_optimistic().await?;
    let leases = system_store.list_database_node_leases(&mut txn).await?;
    let drains = system_store
        .list_database_drain_states(&mut txn, &keyspace, db_id, epoch)
        .await?;
    txn.rollback().await.ok();
    Ok(live_undrained_leases(&leases, &drains, now_ms, NODE_LEASE_GUARD_MS).is_empty())
}

pub(crate) async fn claim_database_drop_coordinator(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
) -> Result<bool> {
    let identity = current_node_identity()?;
    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let now_ms = pd_now_ms(system_store).await?;
    let mut txn = system_store.begin().await?;
    let claimed = system_store
        .claim_database_drop_with_lease(
            &mut txn,
            &keyspace,
            db_id,
            epoch,
            &identity.node_id,
            identity.generation,
            now_ms,
            DROP_COORDINATOR_CLAIM_LEASE_MS,
        )
        .await?;
    txn.commit()
        .await
        .context("failed to commit database drop coordinator claim")?;
    Ok(claimed)
}

pub(crate) async fn release_database_drop_coordinator_claim(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
) -> Result<()> {
    let identity = current_node_identity()?;
    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let mut txn = system_store.begin().await?;
    system_store
        .release_database_drop_claim_if_owner(
            &mut txn,
            &keyspace,
            db_id,
            epoch,
            &identity.node_id,
            identity.generation,
        )
        .await?;
    txn.commit()
        .await
        .context("failed to release database drop coordinator claim")?;
    Ok(())
}

async fn delete_hnsw_text_keys(tenant_store: &TikvStore, db_id: u64) -> Result<()> {
    let prefix_start = crate::sql::hnsw::storage::hnsw_db_prefix(db_id);
    let prefix_end = crate::sql::hnsw::storage::hnsw_db_prefix_end(db_id);
    let mut cursor = prefix_start.clone();
    loop {
        let mut txn = tenant_store.begin().await?;
        let keys: Vec<tikv_client::Key> = txn
            .scan_keys(cursor.clone()..prefix_end.clone(), 1_000)
            .await?
            .collect();
        if keys.is_empty() {
            txn.rollback().await.ok();
            break;
        }
        let last: Vec<u8> = keys.last().expect("non-empty keys").clone().into();
        let mut next = last;
        next.push(0x00);
        cursor = next;
        for key in &keys {
            txn.delete(key.clone()).await?;
        }
        txn.commit().await?;
    }
    Ok(())
}

pub(crate) async fn try_complete_fenced_database_drop(
    system_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
    reason: &str,
) -> Result<bool> {
    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    if !database_read_drain_allows_drop(system_store, &keyspace, db_id, epoch).await? {
        return Ok(false);
    }

    if !claim_database_drop_coordinator(system_store, &keyspace, db_id, epoch).await? {
        return Ok(false);
    }

    let result = try_complete_claimed_fenced_database_drop(
        system_store,
        tenant_store,
        &keyspace,
        db_id,
        epoch,
        reason,
    )
    .await;
    if let Err(e) =
        release_database_drop_coordinator_claim(system_store, &keyspace, db_id, epoch).await
    {
        warn!(
            keyspace = %keyspace,
            db_id,
            epoch,
            reason,
            "Failed to release database drop coordinator claim: {e}"
        );
    }
    result
}

async fn try_complete_claimed_fenced_database_drop(
    system_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    epoch: u64,
    reason: &str,
) -> Result<bool> {
    delete_hnsw_text_keys(tenant_store, db_id).await?;

    match super::complete_hnsw_s3_db_prefix_cleanup_for_dropped_db(keyspace, db_id).await {
        Ok(deleted) if deleted > 0 => {
            info!(
                keyspace = %keyspace,
                db_id,
                deleted,
                reason,
                "Deleted HNSW S3 objects for dropped database"
            );
        }
        Ok(_) => {}
        Err(e) => warn!(
            keyspace = %keyspace,
            db_id,
            reason,
            "HNSW S3 cleanup failed; durable cleanup intent retained: {e}"
        ),
    }

    tenant_store.unsafe_destroy_database_data(db_id).await?;

    match system_store
        .reap_db_queue_entries_then_delete_worker_registry(keyspace, db_id)
        .await
    {
        Ok(n) if n > 0 => info!(
            keyspace = %keyspace,
            db_id,
            reaped = n,
            reason,
            "Reaped worker queue entries for dropped database"
        ),
        Ok(_) => {}
        Err(e) => warn!(
            keyspace = %keyspace,
            db_id,
            reason,
            "Worker cleanup failed; registry row retained for retry: {e}"
        ),
    }

    tenant_store
        .mark_database_dropped_if_fencing_epoch(db_id, epoch)
        .await
}

/// Keep the database lifecycle node lease fresh while this process serves traffic.
pub(crate) async fn run_database_node_lease_publisher_loop(
    system_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
    config: &WorkerConfig,
    client_pool: std::sync::Arc<TikvClientPool>,
) {
    let keyspace = crate::worker::canonical_registry_keyspace(keyspace);
    let mut self_fence_at = Some(
        Instant::now() + Duration::from_millis((NODE_LEASE_TTL_MS - NODE_LEASE_GUARD_MS) as u64),
    );
    info!(
        node_id = %config.worker_id,
        keyspace = %keyspace,
        lease_ttl_ms = NODE_LEASE_TTL_MS,
        publish_interval_ms = NODE_LEASE_PUBLISH_INTERVAL_MS,
        "Database lifecycle node lease publisher started"
    );

    let mut interval = tokio::time::interval(Duration::from_millis(NODE_LEASE_PUBLISH_INTERVAL_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match publish_database_node_lease_once(system_store, config, true).await {
            Ok(lease) => {
                self_fence_at = Some(
                    Instant::now()
                        + Duration::from_millis(self_fence_delay_ms(&lease, NODE_LEASE_GUARD_MS)),
                );
            }
            Err(e) => {
                warn!("Database lifecycle node lease publish failed: {e}");
                if self_fence_at.is_some_and(|deadline| Instant::now() >= deadline) {
                    set_database_lifecycle_accepts_traffic(false);
                    warn!(
                        "Database lifecycle node lease reached self-fence deadline; refusing database traffic until lease publish recovers"
                    );
                }
            }
        }
        if let Err(e) = publish_current_keyspace_fencing_drain_states_once(
            system_store,
            tenant_store,
            &keyspace,
        )
        .await
        {
            warn!("Database lifecycle drain-state publish failed: {e}");
        }
        if let Err(e) =
            publish_requested_database_drain_states_once(system_store, &client_pool).await
        {
            warn!("Database lifecycle requested drain-state publish failed: {e}");
        }
    }
}

/// Remove this process's lifecycle lease after traffic and workers have stopped.
pub(crate) async fn clear_database_node_lease(
    store: &TikvStore,
    config: &WorkerConfig,
) -> Result<()> {
    let mut txn = store.begin().await?;
    store
        .delete_database_node_lease(&mut txn, &config.worker_id)
        .await?;
    txn.commit()
        .await
        .context("failed to commit database lifecycle node lease removal")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_generation_uses_gc_instance_uuid_as_process_generation() {
        let cfg = WorkerConfig {
            gc_instance_id: "00112233-4455-6677-8899-aabbccddeeff".to_string(),
            ..Default::default()
        };
        assert_eq!(lifecycle_generation(&cfg), 0x0011_2233_4455_6677);
    }

    #[test]
    fn lifecycle_generation_falls_back_for_non_uuid_instance_id() {
        let cfg = WorkerConfig {
            gc_instance_id: "manual-instance-id".to_string(),
            ..Default::default()
        };
        assert_ne!(lifecycle_generation(&cfg), 0);
    }

    fn lease(node_id: &str, generation: u64, lease_until_ms: i64) -> DatabaseNodeLease {
        DatabaseNodeLease {
            node_id: node_id.to_string(),
            generation,
            lease_until_ms,
            published_at_ms: lease_until_ms - 1_000,
            accepts_sql: true,
        }
    }

    fn drain(node_id: &str, generation: u64, drained: bool) -> DatabaseDrainState {
        DatabaseDrainState {
            keyspace: "tenant-a".to_string(),
            db_id: 7,
            epoch: 3,
            node_id: node_id.to_string(),
            generation,
            observed_at_ms: 10_000,
            active_old_epoch_ops: if drained { 0 } else { 1 },
            drained,
        }
    }

    #[test]
    fn drain_decision_requires_live_generation_matched_ack() {
        let leases = vec![lease("node-a", 1, 20_000)];
        assert_eq!(
            live_undrained_leases(&leases, &[], 10_000, NODE_LEASE_GUARD_MS).len(),
            1
        );

        let stale_ack = vec![drain("node-a", 0, true)];
        assert_eq!(
            live_undrained_leases(&leases, &stale_ack, 10_000, NODE_LEASE_GUARD_MS).len(),
            1,
            "stale-generation drain acks must not count"
        );

        let current_ack = vec![drain("node-a", 1, true)];
        assert!(
            live_undrained_leases(&leases, &current_ack, 10_000, NODE_LEASE_GUARD_MS).is_empty(),
            "current-generation drained ack allows the coordinator to proceed"
        );
    }

    #[test]
    fn drain_decision_requires_every_live_node_to_ack_current_generation() {
        let leases = vec![lease("node-a", 1, 20_000), lease("node-b", 7, 20_000)];

        let only_one_node_acked = vec![drain("node-a", 1, true)];
        let undrained =
            live_undrained_leases(&leases, &only_one_node_acked, 10_000, NODE_LEASE_GUARD_MS);
        assert_eq!(undrained.len(), 1);
        assert_eq!(undrained[0].node_id, "node-b");

        let stale_generation_for_b = vec![drain("node-a", 1, true), drain("node-b", 6, true)];
        let undrained = live_undrained_leases(
            &leases,
            &stale_generation_for_b,
            10_000,
            NODE_LEASE_GUARD_MS,
        );
        assert_eq!(undrained.len(), 1);
        assert_eq!(undrained[0].node_id, "node-b");

        let mut active_old_epoch_work = drain("node-b", 7, true);
        active_old_epoch_work.active_old_epoch_ops = 1;
        let ack_with_active_work = vec![drain("node-a", 1, true), active_old_epoch_work];
        let undrained =
            live_undrained_leases(&leases, &ack_with_active_work, 10_000, NODE_LEASE_GUARD_MS);
        assert_eq!(undrained.len(), 1);
        assert_eq!(undrained[0].node_id, "node-b");

        let both_current_generations_drained =
            vec![drain("node-a", 1, true), drain("node-b", 7, true)];
        assert!(
            live_undrained_leases(
                &leases,
                &both_current_generations_drained,
                10_000,
                NODE_LEASE_GUARD_MS
            )
            .is_empty(),
            "DROP may proceed only after every live SQL-serving node drains current generation work"
        );
    }

    #[test]
    fn drain_decision_ignores_expired_or_inactive_leases() {
        let expired = vec![lease("node-a", 1, 4_000)];
        assert!(
            live_undrained_leases(&expired, &[], 10_000, NODE_LEASE_GUARD_MS).is_empty(),
            "coordinator may stop waiting after lease_until + guard"
        );

        let mut inactive = lease("node-b", 1, 20_000);
        inactive.accepts_sql = false;
        assert!(
            live_undrained_leases(&[inactive], &[], 10_000, NODE_LEASE_GUARD_MS).is_empty(),
            "nodes that no longer accept SQL do not block read drain"
        );
    }

    #[test]
    fn drain_request_ttl_is_bounded_and_saturating() {
        assert!(!drain_request_expired(10_000, 9_000));
        assert!(!drain_request_expired(
            10_000 + DRAIN_REQUEST_TTL_MS,
            10_000
        ));
        assert!(drain_request_expired(10_001 + DRAIN_REQUEST_TTL_MS, 10_000));
        assert!(
            !drain_request_expired(1_000, 10_000),
            "clock/TSO regressions must not make a fresh request expire"
        );
    }

    #[test]
    fn self_fence_delay_stops_before_lease_expiry() {
        let lease = DatabaseNodeLease {
            node_id: "node-a".to_string(),
            generation: 1,
            lease_until_ms: 30_000,
            published_at_ms: 10_000,
            accepts_sql: true,
        };
        assert_eq!(self_fence_delay_ms(&lease, 5_000), 15_000);

        let nearly_expired = DatabaseNodeLease {
            lease_until_ms: 11_000,
            ..lease
        };
        assert_eq!(
            self_fence_delay_ms(&nearly_expired, 5_000),
            0,
            "a node must self-fence immediately if lease_until - guard is already past published_at"
        );
    }
}
