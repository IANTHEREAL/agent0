use crate::storage::{LifecycleTenantRecord, LifecycleTenantStatus, TikvStore};
use crate::worker::config::WorkerConfig;
use anyhow::Result;
use parking_lot::Mutex;
use pgwire::tokio::CancellationToken;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::TimestampExt;
use tracing::{error, info, warn};

/// Bound a single lifecycle publish attempt so a wedged TiKV/PD call cannot
/// leave a serving process past its local self-fence deadline.
const LIFECYCLE_PROCESS_LIVENESS_PUBLISH_TIMEOUT_SEC: u64 = 30;
const TENANT_LIFECYCLE_BACKFILL_PAGE_SIZE: usize = 256;

fn should_publish_tenant_lifecycle_status(
    current: Option<&LifecycleTenantRecord>,
    incarnation: u64,
    status: LifecycleTenantStatus,
) -> bool {
    let Some(current) = current else {
        return true;
    };
    if current.incarnation > incarnation {
        return false;
    }
    if current.incarnation < incarnation {
        return true;
    }
    !(current.status == LifecycleTenantStatus::Dropped && status == LifecycleTenantStatus::Live)
}

/// Publish this process's DB lifecycle liveness before SQL admission starts.
///
/// This is a process-incarnation heartbeat keyed by `gc_instance_id`, not the
/// Phase 2 stable node lease/drain-state row. It intentionally has no reader in
/// this slice; Phase 2 must introduce stable node identity and decode its own
/// node-lease format before making drain/drop decisions.
pub async fn publish_lifecycle_process_liveness_once(
    store: &TikvStore,
    config: &WorkerConfig,
) -> Result<()> {
    publish_lifecycle_process_liveness_with_timeout(
        store,
        config,
        lifecycle_liveness_self_fence_after(config),
    )
    .await?;
    Ok(())
}

/// DB lifecycle liveness publisher loop for every SQL-serving process.
///
/// This loop is independent of `DB9_WORKER_ENABLED`: lifecycle liveness is Core
/// serving state, not background execution. If publishing fails past the local
/// monotonic deadline, the process self-fences and main stops accepting SQL.
pub async fn run_lifecycle_publisher_loop(
    store: Arc<TikvStore>,
    config: WorkerConfig,
    self_fence: CancellationToken,
    last_successful_publish: Arc<Mutex<Instant>>,
) {
    info!(
        interval_sec = config.db_lifecycle_publish_interval_sec,
        "DB lifecycle process-liveness publisher started"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(
        config.db_lifecycle_publish_interval_sec,
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let max_retry_delay =
        Duration::from_secs((config.db_lifecycle_publish_interval_sec / 2).max(1));

    loop {
        interval.tick().await;
        let last_success = read_lifecycle_liveness_last_success(&last_successful_publish);
        let remaining = remaining_until_lifecycle_self_fence(last_success, &config);
        if remaining.is_zero() {
            trigger_lifecycle_liveness_self_fence(
                &self_fence,
                last_success,
                &config,
                &anyhow::anyhow!("DB lifecycle process-liveness deadline elapsed"),
            );
            park_lifecycle_publisher_after_self_fence().await;
        }

        match publish_lifecycle_process_liveness_with_timeout(&store, &config, remaining).await {
            Ok(_) => {
                record_lifecycle_liveness_publish_success(&last_successful_publish);
                metrics::counter!("db9_server_lifecycle_publish_ok").increment(1);
            }
            Err(e) => {
                metrics::counter!("db9_server_lifecycle_publish_err").increment(1);
                warn!("DB lifecycle process-liveness publish failed: {e}");
                let last_success = read_lifecycle_liveness_last_success(&last_successful_publish);
                if lifecycle_liveness_self_fence_elapsed(last_success, &config) {
                    trigger_lifecycle_liveness_self_fence(&self_fence, last_success, &config, &e);
                    park_lifecycle_publisher_after_self_fence().await;
                }

                let mut backoff = Duration::from_secs(1);
                loop {
                    let last_success =
                        read_lifecycle_liveness_last_success(&last_successful_publish);
                    let remaining = remaining_until_lifecycle_self_fence(last_success, &config);
                    if remaining.is_zero() {
                        trigger_lifecycle_liveness_self_fence(
                            &self_fence,
                            last_success,
                            &config,
                            &e,
                        );
                        park_lifecycle_publisher_after_self_fence().await;
                    }

                    tokio::time::sleep(backoff.min(remaining)).await;
                    let last_success =
                        read_lifecycle_liveness_last_success(&last_successful_publish);
                    let remaining = remaining_until_lifecycle_self_fence(last_success, &config);
                    if remaining.is_zero() {
                        trigger_lifecycle_liveness_self_fence(
                            &self_fence,
                            last_success,
                            &config,
                            &e,
                        );
                        park_lifecycle_publisher_after_self_fence().await;
                    }

                    match publish_lifecycle_process_liveness_with_timeout(
                        &store, &config, remaining,
                    )
                    .await
                    {
                        Ok(_) => {
                            record_lifecycle_liveness_publish_success(&last_successful_publish);
                            metrics::counter!("db9_server_lifecycle_publish_ok").increment(1);
                            info!("DB lifecycle process-liveness publish succeeded after retry");
                            break;
                        }
                        Err(retry_err) => {
                            metrics::counter!("db9_server_lifecycle_publish_err").increment(1);
                            warn!(
                                "DB lifecycle process-liveness publish retry failed \
                                 (backoff={backoff:?}): {retry_err}"
                            );
                            let last_success =
                                read_lifecycle_liveness_last_success(&last_successful_publish);
                            if lifecycle_liveness_self_fence_elapsed(last_success, &config) {
                                trigger_lifecycle_liveness_self_fence(
                                    &self_fence,
                                    last_success,
                                    &config,
                                    &retry_err,
                                );
                                park_lifecycle_publisher_after_self_fence().await;
                            }
                            backoff = (backoff * 2).min(max_retry_delay);
                            if backoff >= max_retry_delay {
                                warn!(
                                    "DB lifecycle process-liveness retries exhausted; \
                                     will retry on next tick"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn read_lifecycle_liveness_last_success(clock: &Arc<Mutex<Instant>>) -> Instant {
    *clock.lock()
}

fn record_lifecycle_liveness_publish_success(clock: &Arc<Mutex<Instant>>) {
    *clock.lock() = Instant::now();
}

pub(crate) fn lifecycle_liveness_self_fence_after(config: &WorkerConfig) -> Duration {
    Duration::from_secs(config.db_lifecycle_publish_interval_sec).saturating_add(
        Duration::from_secs(LIFECYCLE_PROCESS_LIVENESS_PUBLISH_TIMEOUT_SEC),
    )
}

fn lifecycle_liveness_self_fence_elapsed(
    last_successful_publish: Instant,
    config: &WorkerConfig,
) -> bool {
    remaining_until_lifecycle_self_fence(last_successful_publish, config).is_zero()
}

fn remaining_until_lifecycle_self_fence(
    last_successful_publish: Instant,
    config: &WorkerConfig,
) -> Duration {
    lifecycle_liveness_self_fence_after(config).saturating_sub(last_successful_publish.elapsed())
}

fn trigger_lifecycle_liveness_self_fence(
    self_fence: &CancellationToken,
    last_successful_publish: Instant,
    config: &WorkerConfig,
    err: &anyhow::Error,
) {
    if !self_fence.is_cancelled() {
        let stale_for_ms = last_successful_publish.elapsed().as_millis() as u64;
        error!(
            stale_for_ms,
            self_fence_after_ms = lifecycle_liveness_self_fence_after(config).as_millis() as u64,
            error = %err,
            "DB lifecycle process-liveness publish failed past self-fence deadline; \
             stopping SQL admission"
        );
        metrics::gauge!("db9_server_lifecycle_self_fenced").set(1.0);
        self_fence.cancel();
    }
}

async fn park_lifecycle_publisher_after_self_fence() {
    // Keep this task parked until main's shutdown path aborts it. Returning
    // would make the supervisor restart the publisher while the process is
    // intentionally self-fenced.
    std::future::pending::<()>().await;
}

/// Remove this process's lifecycle liveness row during graceful shutdown.
pub async fn clear_lifecycle_process_liveness(
    store: &TikvStore,
    config: &WorkerConfig,
) -> Result<()> {
    let mut txn = store.begin().await?;
    store
        .delete_lifecycle_process_liveness(&mut txn, &config.gc_instance_id)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Allocate a never-reused tenant incarnation from the Core lifecycle domain.
///
/// Gaps are expected: once this transaction commits, callers must not reuse the
/// value even if a later tenant transaction aborts. Reuse would be worse than a
/// gap because background effect validation treats incarnation as ownership.
pub async fn allocate_tenant_incarnation(store: &TikvStore) -> Result<u64> {
    let mut txn = store.begin().await?;
    let incarnation = store
        .allocate_lifecycle_tenant_incarnation(&mut txn)
        .await?;
    txn.commit().await?;
    Ok(incarnation)
}

/// Publish this tenant DB's lifecycle inventory row in the Core lifecycle
/// domain.
///
/// This row is an authoritative inventory row for repair/discovery, but worker
/// effects must still validate the tenant-local incarnation stamp in the tenant
/// transaction before making changes. That keeps correctness simple across the
/// unavoidable cross-domain commit window.
pub async fn publish_tenant_lifecycle_status(
    store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    incarnation: u64,
    status: LifecycleTenantStatus,
) -> Result<u64> {
    let current_version = current_lifecycle_timestamp(store).await?.version();
    let mut txn = store.begin().await?;
    let current = store
        .get_lifecycle_tenant_record_for_update(&mut txn, keyspace, db_id)
        .await?;
    if !should_publish_tenant_lifecycle_status(current.as_ref(), incarnation, status) {
        txn.rollback().await.ok();
        return Ok(current
            .map(|record| record.updated_at_version)
            .unwrap_or(current_version));
    }
    store
        .put_lifecycle_tenant_record(
            &mut txn,
            keyspace,
            db_id,
            incarnation,
            status,
            current_version,
        )
        .await?;
    txn.commit().await?;
    Ok(current_version)
}

/// Ensure one live tenant DB has a tenant-local incarnation stamp and a Core
/// lifecycle `Live` inventory row.
///
/// This is the migration/backfill path for databases created before lifecycle
/// identity existed. It never guesses ownership from legacy worker rows; it
/// derives identity only from the currently live tenant catalog row and writes
/// the stamp in that tenant keyspace.
pub async fn ensure_tenant_lifecycle_identity(
    lifecycle_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
) -> Result<Option<u64>> {
    let incarnation = {
        let mut tenant_txn = tenant_store.begin().await?;
        if !tenant_store
            .database_alive_for_update(&mut tenant_txn, db_id)
            .await?
        {
            tenant_txn.rollback().await.ok();
            return Ok(None);
        }

        if let Some(existing) = tenant_store
            .get_tenant_incarnation_stamp_for_update(&mut tenant_txn, db_id)
            .await?
        {
            tenant_txn.rollback().await.ok();
            existing
        } else {
            let allocated = allocate_tenant_incarnation(lifecycle_store).await?;
            tenant_store
                .put_tenant_incarnation_stamp(&mut tenant_txn, db_id, allocated)
                .await?;
            tenant_txn.commit().await?;
            allocated
        }
    };

    publish_tenant_lifecycle_status(
        lifecycle_store,
        keyspace,
        db_id,
        incarnation,
        LifecycleTenantStatus::Live,
    )
    .await?;
    Ok(Some(incarnation))
}

/// Backfill lifecycle identity for all currently live DBs in a tenant keyspace.
pub async fn backfill_tenant_lifecycle_inventory(
    lifecycle_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
) -> Result<usize> {
    let mut cursor: Option<Vec<u8>> = None;
    let mut backfilled = 0usize;

    loop {
        let (databases, next_cursor) = {
            let mut tenant_txn = tenant_store.begin().await?;
            let page = tenant_store
                .scan_databases_page(
                    &mut tenant_txn,
                    cursor.as_deref(),
                    TENANT_LIFECYCLE_BACKFILL_PAGE_SIZE,
                )
                .await?;
            tenant_txn.rollback().await.ok();
            page
        };

        if databases.is_empty() {
            break;
        }

        for db in databases {
            if ensure_tenant_lifecycle_identity(lifecycle_store, tenant_store, keyspace, db.id)
                .await?
                .is_some()
            {
                backfilled = backfilled.saturating_add(1);
            }
        }

        match next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    Ok(backfilled)
}

/// Repair lifecycle rows whose post-commit Dropped publish was missed.
///
/// The tenant catalog is the effect-time truth. If lifecycle still says `Live`
/// but the tenant DB metadata row is gone, the DROP transaction has already
/// committed and this inventory row must become `Dropped` so later migration and
/// repair code do not treat it as a live tenant.
pub async fn repair_dropped_tenant_lifecycle_inventory(
    lifecycle_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
) -> Result<usize> {
    let mut cursor: Option<Vec<u8>> = None;
    let mut repaired = 0usize;

    loop {
        let (records, next_cursor) = {
            let mut lifecycle_txn = lifecycle_store.begin().await?;
            let page = lifecycle_store
                .scan_lifecycle_tenant_records_page(
                    &mut lifecycle_txn,
                    cursor.as_deref(),
                    TENANT_LIFECYCLE_BACKFILL_PAGE_SIZE,
                )
                .await?;
            lifecycle_txn.rollback().await.ok();
            page
        };

        if records.is_empty() {
            match next_cursor {
                Some(next) => {
                    cursor = Some(next);
                    continue;
                }
                None => break,
            }
        }

        for record in records {
            if record.keyspace != keyspace || record.status == LifecycleTenantStatus::Dropped {
                continue;
            }

            let db_exists = {
                let mut tenant_txn = tenant_store.begin().await?;
                let exists = tenant_store
                    .get_database_by_id(&mut tenant_txn, record.db_id)
                    .await?
                    .is_some();
                tenant_txn.rollback().await.ok();
                exists
            };

            if !db_exists {
                publish_tenant_lifecycle_status(
                    lifecycle_store,
                    keyspace,
                    record.db_id,
                    record.incarnation,
                    LifecycleTenantStatus::Dropped,
                )
                .await?;
                repaired = repaired.saturating_add(1);
            }
        }

        match next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    Ok(repaired)
}

async fn publish_lifecycle_process_liveness_with_timeout(
    store: &TikvStore,
    config: &WorkerConfig,
    remaining_until_self_fence: Duration,
) -> Result<u64> {
    let timeout = Duration::from_secs(LIFECYCLE_PROCESS_LIVENESS_PUBLISH_TIMEOUT_SEC)
        .min(remaining_until_self_fence);
    tokio::time::timeout(timeout, publish_lifecycle_process_liveness(store, config))
        .await
        .map_err(|_| anyhow::anyhow!("DB lifecycle publish timed out after {:?}", timeout))?
}

async fn publish_lifecycle_process_liveness(
    store: &TikvStore,
    config: &WorkerConfig,
) -> Result<u64> {
    let current_ts = current_lifecycle_timestamp(store).await?;

    let mut txn = store.begin().await?;
    store
        .put_lifecycle_process_liveness(
            &mut txn,
            &config.gc_instance_id,
            current_ts.version(),
            lifecycle_liveness_self_fence_after(config).as_millis() as u64,
        )
        .await?;
    txn.commit().await?;
    Ok(current_ts.version())
}

async fn current_lifecycle_timestamp(store: &TikvStore) -> Result<tikv_client::Timestamp> {
    let client = store
        .transaction_client()
        .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;

    client
        .current_timestamp_with_timeout(Duration::from_secs(
            LIFECYCLE_PROCESS_LIVENESS_PUBLISH_TIMEOUT_SEC,
        ))
        .await
        .map_err(|e| anyhow::anyhow!("failed to get lifecycle timestamp from PD: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_self_fence_uses_local_monotonic_deadline() {
        let cfg = WorkerConfig {
            db_lifecycle_publish_interval_sec: 30,
            ..Default::default()
        };

        assert_eq!(
            lifecycle_liveness_self_fence_after(&cfg),
            Duration::from_secs(60)
        );

        let source = include_str!("lifecycle.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("lifecycle.rs must contain #[cfg(test)]");
        assert!(
            prod_source.contains("record_lifecycle_liveness_publish_success")
                && prod_source.contains("last_successful_publish.elapsed()"),
            "lifecycle self-fence must use a local monotonic deadline, not a remote stale-row scan"
        );
        assert!(
            prod_source.contains("last_successful_publish: Arc<Mutex<Instant>>")
                && prod_source.contains("read_lifecycle_liveness_last_success")
                && !prod_source.contains("let mut last_successful_publish = Instant::now()"),
            "lifecycle self-fence clock must be shared outside the supervised future so restarts cannot reset the deadline"
        );
        assert!(
            prod_source.contains("backoff.min(remaining)")
                && prod_source.contains("remaining_until_lifecycle_self_fence"),
            "retry sleeps and publish attempts must not drift past the local self-fence deadline"
        );
        assert!(
            prod_source.contains("self_fence.cancel()")
                && prod_source.contains("db9_server_lifecycle_self_fenced"),
            "publish failure past the deadline must trigger the process self-fence"
        );
    }

    #[test]
    fn lifecycle_publisher_is_independent_of_worker_execution_flag() {
        let source = include_str!("lifecycle.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("lifecycle.rs must contain #[cfg(test)]");

        assert!(
            !prod_source.contains("config.enabled"),
            "lifecycle liveness is Core SQL-serving state and must not be gated by DB9_WORKER_ENABLED"
        );
    }

    #[test]
    fn lifecycle_backfill_derives_identity_from_tenant_catalog_not_worker_queue() {
        let source = include_str!("lifecycle.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("lifecycle.rs must contain #[cfg(test)]");
        let ensure_fn = prod_source
            .split("pub async fn ensure_tenant_lifecycle_identity(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub async fn backfill_tenant_lifecycle_inventory(")
                    .next()
            })
            .expect("ensure_tenant_lifecycle_identity must exist before backfill");

        assert!(
            ensure_fn.contains("database_alive_for_update")
                && ensure_fn.contains("get_tenant_incarnation_stamp_for_update")
                && ensure_fn.contains("put_tenant_incarnation_stamp"),
            "lifecycle backfill must lock tenant catalog/stamp before assigning identity"
        );
        assert!(
            !ensure_fn.contains("TaskQueueEntry")
                && !ensure_fn.contains("scan_due_v2")
                && !ensure_fn.contains("wq"),
            "lifecycle backfill must not infer tenant incarnation from legacy worker queue rows"
        );
    }

    #[test]
    fn lifecycle_repair_publishes_dropped_when_tenant_catalog_row_is_gone() {
        let source = include_str!("lifecycle.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("lifecycle.rs must contain #[cfg(test)]");
        let repair_fn = prod_source
            .split("pub async fn repair_dropped_tenant_lifecycle_inventory(")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn publish_lifecycle_process_liveness")
                    .next()
            })
            .expect("dropped lifecycle repair helper must exist");

        assert!(
            repair_fn.contains("scan_lifecycle_tenant_records_page")
                && repair_fn.contains("get_database_by_id")
                && repair_fn.contains("LifecycleTenantStatus::Dropped"),
            "lifecycle repair must scan inventory, check tenant catalog truth, and publish Dropped for missing DBs"
        );
        assert!(
            !repair_fn.contains("scan_due_v2") && !repair_fn.contains("TaskQueueEntry"),
            "lifecycle repair must not infer dropped state from worker queue residue"
        );
    }

    #[test]
    fn lifecycle_repair_continues_past_empty_decoded_pages() {
        let source = include_str!("lifecycle.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("lifecycle.rs must contain #[cfg(test)]");
        let repair_fn = prod_source
            .split("pub async fn repair_dropped_tenant_lifecycle_inventory(")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn publish_lifecycle_process_liveness")
                    .next()
            })
            .expect("dropped lifecycle repair helper must exist");

        let empty_page_pos = repair_fn
            .find("if records.is_empty()")
            .expect("lifecycle repair must handle empty decoded pages");
        let cursor_pos = repair_fn
            .find("cursor = Some(next)")
            .expect("lifecycle repair must continue when the scan has a next cursor");
        let continue_pos = repair_fn
            .find("continue;")
            .expect("lifecycle repair must continue past malformed-only pages");
        assert!(
            empty_page_pos < cursor_pos && cursor_pos < continue_pos,
            "lifecycle repair must not stop when a scanned page contains only skipped/malformed rows"
        );
    }

    #[test]
    fn tenant_lifecycle_publish_is_monotonic_and_dropped_terminal() {
        let current_live = LifecycleTenantRecord {
            keyspace: "tenant_a".to_string(),
            db_id: 7,
            incarnation: 42,
            status: LifecycleTenantStatus::Live,
            updated_at_version: 100,
        };
        let current_dropped = LifecycleTenantRecord {
            status: LifecycleTenantStatus::Dropped,
            ..current_live.clone()
        };
        let newer_live = LifecycleTenantRecord {
            incarnation: 43,
            ..current_live.clone()
        };

        assert!(should_publish_tenant_lifecycle_status(
            None,
            42,
            LifecycleTenantStatus::Live
        ));
        assert!(should_publish_tenant_lifecycle_status(
            Some(&current_live),
            42,
            LifecycleTenantStatus::Dropped
        ));
        assert!(!should_publish_tenant_lifecycle_status(
            Some(&current_dropped),
            42,
            LifecycleTenantStatus::Live
        ));
        assert!(should_publish_tenant_lifecycle_status(
            Some(&current_dropped),
            43,
            LifecycleTenantStatus::Live
        ));
        assert!(!should_publish_tenant_lifecycle_status(
            Some(&newer_live),
            42,
            LifecycleTenantStatus::Dropped
        ));
    }
}
