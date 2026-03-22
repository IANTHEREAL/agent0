//! Export snapshot lifecycle manager (TTL janitor + crash recovery).
//!
//! Runs as a background tokio task, periodically scanning the registry
//! for expired or stale snapshots and cleaning them up.

use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, info, warn};

use super::registry::{
    ExportSnapshotRegistry, ExportSnapshotState, SERVICE_SAFE_POINT_LEASE_TTL_SECS,
};
use crate::storage::backpressure::tikv_op;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Janitor tick interval (seconds).
const JANITOR_INTERVAL_SECS: u64 = 30;

/// How long to keep terminal (Released/Expired/Failed) records before deletion (ms).
const TERMINAL_RETENTION_MS: i64 = 3600 * 1000; // 1 hour

// ---------------------------------------------------------------------------
// Janitor loop
// ---------------------------------------------------------------------------

/// Start the export snapshot janitor background loop.
///
/// This should be spawned as a tokio task at server startup.
/// It runs forever and handles:
/// 1. Expiring active snapshots past their TTL.
/// 2. Removing service safe points for expired/failed snapshots.
/// 3. Cleaning up terminal records older than the retention period.
pub async fn export_snapshot_janitor_loop(registry: Arc<ExportSnapshotRegistry>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(JANITOR_INTERVAL_SECS));

    // Don't try to catch up if ticks are delayed.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        if let Err(e) = janitor_tick(&registry).await {
            warn!("export snapshot janitor tick error: {e}");
        }
    }
}

async fn janitor_tick(registry: &ExportSnapshotRegistry) -> anyhow::Result<()> {
    let client = registry.client();
    let now_ms = Utc::now().timestamp_millis();

    // Open a pessimistic transaction for the whole tick.
    let options = tikv_client::TransactionOptions::new_pessimistic()
        .drop_check(tikv_client::CheckLevel::Warn);
    let mut txn = tikv_op!(client.begin_with_options(options).await)
        .map_err(|e| anyhow::anyhow!("janitor txn begin: {e}"))?;

    let all_snapshots = registry.scan_all_snapshots(&mut txn).await?;

    let mut expired_count = 0u32;
    let mut cleaned_count = 0u32;
    let mut refreshed_count = 0u32;
    let mut lease_removed_count = 0u32;

    for snap in &all_snapshots {
        if snap.is_active() {
            if snap.is_expired_at(now_ms) {
                // 1. Expire active snapshots past TTL.
                if let Some(updated) = registry
                    .transition_snapshot_state(
                        &mut txn,
                        &snap.snapshot_id,
                        ExportSnapshotState::Expired,
                    )
                    .await?
                {
                    // Remove service safe point.
                    let service_id = updated.service_safe_point_id();
                    if let Err(e) = client
                        .update_service_safepoint(&service_id, 0, updated.snapshot_ts)
                        .await
                    {
                        warn!(
                            snapshot_id = %snap.snapshot_id,
                            service_id = %service_id,
                            "failed to remove service safe point on expiry: {e}"
                        );
                    }

                    info!(
                        snapshot_id = %snap.snapshot_id,
                        database_id = snap.database_id,
                        "export snapshot expired by janitor"
                    );
                    expired_count += 1;
                }
            } else {
                // 2. Heartbeat: refresh service safe point lease for active snapshots.
                // Re-register with the short TTL so PD keeps the lease alive.
                let service_id = snap.service_safe_point_id();
                if let Err(e) = client
                    .update_service_safepoint(
                        &service_id,
                        SERVICE_SAFE_POINT_LEASE_TTL_SECS,
                        snap.snapshot_ts,
                    )
                    .await
                {
                    warn!(
                        snapshot_id = %snap.snapshot_id,
                        service_id = %service_id,
                        "failed to refresh service safe point lease: {e}"
                    );
                } else {
                    refreshed_count += 1;
                }
            }
        }

        // 3. Terminal snapshots: actively retry lease deletion, then clean up old records.
        if snap.is_terminal() {
            // Best-effort: ensure service safe point is removed (retried each tick).
            let service_id = snap.service_safe_point_id();
            match client
                .update_service_safepoint(&service_id, 0, snap.snapshot_ts)
                .await
            {
                Ok(_) => {
                    lease_removed_count += 1;
                }
                Err(e) => {
                    warn!(
                        snapshot_id = %snap.snapshot_id,
                        service_id = %service_id,
                        "failed to remove service safe point for terminal snapshot: {e}"
                    );
                }
            }

            // Clean up terminal records past retention period.
            let age_ms = now_ms - snap.expires_at_ms;
            if age_ms > TERMINAL_RETENTION_MS {
                registry
                    .delete_snapshot(&mut txn, &snap.snapshot_id)
                    .await?;

                debug!(
                    snapshot_id = %snap.snapshot_id,
                    database_id = snap.database_id,
                    state = %snap.state,
                    "cleaned up terminal export snapshot record"
                );
                cleaned_count += 1;
            }
        }
    }

    tikv_op!(txn.commit().await).map_err(|e| anyhow::anyhow!("janitor commit: {e}"))?;

    if expired_count > 0 || cleaned_count > 0 || refreshed_count > 0 {
        info!(
            expired_count,
            cleaned_count,
            refreshed_count,
            lease_removed_count,
            total_snapshots = all_snapshots.len(),
            "export snapshot janitor tick completed"
        );
    }

    Ok(())
}

/// One-time crash recovery: scan for stale snapshots on startup.
///
/// This handles the case where the server crashed while holding active
/// export snapshots. On restart, any active snapshots that are past their
/// expiry are transitioned to `Expired` and their service safe points removed.
///
/// Should be called once at server startup before accepting connections.
///
/// Note on orphan service safe points: if the server crashes after
/// registering a PD service safe point but before committing the registry
/// record, the orphan lease auto-expires within ~120 seconds (the short
/// TTL used for service safe point registration). No explicit PD
/// reconciliation is needed.
pub async fn recover_stale_snapshots(registry: &ExportSnapshotRegistry) -> anyhow::Result<()> {
    let client = registry.client();
    let now_ms = Utc::now().timestamp_millis();

    let options = tikv_client::TransactionOptions::new_pessimistic()
        .drop_check(tikv_client::CheckLevel::Warn);
    let mut txn = tikv_op!(client.begin_with_options(options).await)
        .map_err(|e| anyhow::anyhow!("recovery txn begin: {e}"))?;

    let all_snapshots = registry.scan_all_snapshots(&mut txn).await?;
    let mut recovered = 0u32;

    for snap in &all_snapshots {
        if snap.is_active() && snap.is_expired_at(now_ms) {
            if let Some(updated) = registry
                .transition_snapshot_state(
                    &mut txn,
                    &snap.snapshot_id,
                    ExportSnapshotState::Expired,
                )
                .await?
            {
                let service_id = updated.service_safe_point_id();
                if let Err(e) = client
                    .update_service_safepoint(&service_id, 0, updated.snapshot_ts)
                    .await
                {
                    warn!(
                        snapshot_id = %snap.snapshot_id,
                        service_id = %service_id,
                        "failed to remove service safe point during recovery: {e}"
                    );
                }

                info!(
                    snapshot_id = %snap.snapshot_id,
                    database_id = snap.database_id,
                    owner_ref = %snap.owner_ref,
                    "recovered stale export snapshot on startup"
                );
                recovered += 1;
            }
        }
    }

    tikv_op!(txn.commit().await).map_err(|e| anyhow::anyhow!("recovery commit: {e}"))?;

    if recovered > 0 {
        info!(recovered, "recovered stale export snapshots on startup");
    } else {
        debug!("no stale export snapshots found during startup recovery");
    }

    Ok(())
}
