use super::*;
use std::future;

// --- GC safepoint computation tests ---

#[test]
fn compute_safepoint_basic() {
    // TSO version: physical_ms << 18 | logical
    // Simulate a current_version corresponding to ~1 hour of physical time.
    let physical_ms: u64 = 3_600_000; // 1 hour
    let current_version = physical_ms << 18;
    let life_time_sec = 600; // 10 minutes

    let sp = compute_safepoint_version(current_version, life_time_sec);
    // Expected: (3_600_000 - 600_000) << 18 = 3_000_000 << 18
    let expected = (physical_ms - life_time_sec * 1000) << 18;
    assert_eq!(sp, expected);
}

#[test]
fn compute_safepoint_saturates_to_zero() {
    // current_version is smaller than the life_time offset.
    let current_version = 1000_u64 << 18;
    let life_time_sec = 86400; // 24 hours — way more than 1 second of TSO

    let sp = compute_safepoint_version(current_version, life_time_sec);
    assert_eq!(sp, 0, "should saturate to 0, not underflow");
}

#[test]
fn compute_safepoint_zero_life_time() {
    let current_version = 999_999_u64 << 18;
    let sp = compute_safepoint_version(current_version, 0);
    assert_eq!(sp, current_version, "zero life_time means no offset");
}

#[test]
fn compute_safepoint_preserves_logical_bits() {
    // current_version with some logical bits set
    let physical_ms: u64 = 7_200_000;
    let logical: u64 = 42;
    let current_version = (physical_ms << 18) | logical;
    let life_time_sec = 3600; // 1 hour

    let sp = compute_safepoint_version(current_version, life_time_sec);
    // The subtraction is on the whole version, so logical bits are preserved
    let expected = current_version - ((life_time_sec * 1000) << 18);
    assert_eq!(sp, expected);
}

#[test]
fn cluster_safepoint_ignores_stale_instances() {
    let current_version = 10_000_000u64 << 18;
    let life_time_sec = 600;
    let time_based = compute_safepoint_version(current_version, life_time_sec);
    let stale_version = time_based.saturating_sub(1);

    let states = vec![GcInstanceState {
        instance_id: "stale".to_string(),
        min_start_ts: Some(time_based.saturating_sub(10_000)),
        updated_at_version: stale_version,
        legacy_max_untracked_timeout_sec: None,
    }];

    assert_eq!(
        compute_cluster_gc_safepoint(current_version, life_time_sec, life_time_sec, &states),
        time_based
    );
}

#[test]
fn cluster_safepoint_clamps_to_oldest_live_transaction() {
    let current_version = 10_000_000u64 << 18;
    let life_time_sec = 600;
    let hb_timeout = life_time_sec; // use same value as heartbeat timeout for this test
    let live_updated_at = current_version;

    let states = vec![
        GcInstanceState {
            instance_id: "a".to_string(),
            min_start_ts: Some((9_500_000u64 << 18) + 7),
            updated_at_version: live_updated_at,
            legacy_max_untracked_timeout_sec: None,
        },
        GcInstanceState {
            instance_id: "b".to_string(),
            min_start_ts: Some((9_300_000u64 << 18) + 9),
            updated_at_version: live_updated_at,
            legacy_max_untracked_timeout_sec: None,
        },
    ];

    assert_eq!(
        compute_cluster_gc_safepoint(current_version, life_time_sec, hb_timeout, &states),
        ((9_300_000u64 << 18) + 9).saturating_sub(1)
    );
}

#[test]
fn stale_gc_instance_ids_only_returns_stale_rows() {
    let current_version = 10_000_000u64 << 18;
    let life_time_sec = 600;
    let hb_timeout = life_time_sec;
    let live_updated_at = current_version;
    let stale_updated_at = compute_safepoint_version(current_version, hb_timeout).saturating_sub(1);

    let states = vec![
        GcInstanceState {
            instance_id: "live".to_string(),
            min_start_ts: None,
            updated_at_version: live_updated_at,
            legacy_max_untracked_timeout_sec: None,
        },
        GcInstanceState {
            instance_id: "stale".to_string(),
            min_start_ts: Some((9_300_000u64 << 18) + 9),
            updated_at_version: stale_updated_at,
            legacy_max_untracked_timeout_sec: None,
        },
    ];

    assert_eq!(
        stale_gc_instance_ids(current_version, hb_timeout, &states),
        vec!["stale".to_string()]
    );
}

#[test]
fn cluster_gc_life_time_honors_live_legacy_timeout_floor() {
    let current_version = 10_000_000u64 << 18;
    let life_time_sec = 600;
    let hb_timeout = life_time_sec;
    let states = vec![GcInstanceState {
        instance_id: "legacy".to_string(),
        min_start_ts: None,
        updated_at_version: current_version,
        legacy_max_untracked_timeout_sec: Some(3_600),
    }];

    assert_eq!(
        effective_cluster_gc_life_time_sec(current_version, life_time_sec, hb_timeout, &states),
        3_600
    );
    assert_eq!(
        compute_cluster_gc_safepoint(current_version, life_time_sec, hb_timeout, &states),
        compute_safepoint_version(current_version, 3_600)
    );
}

#[test]
fn cluster_gc_life_time_ignores_stale_legacy_timeout_floor() {
    let current_version = 10_000_000u64 << 18;
    let life_time_sec = 600;
    let hb_timeout = life_time_sec;
    let stale_updated_at = compute_safepoint_version(current_version, hb_timeout).saturating_sub(1);
    let states = vec![GcInstanceState {
        instance_id: "stale-legacy".to_string(),
        min_start_ts: None,
        updated_at_version: stale_updated_at,
        legacy_max_untracked_timeout_sec: Some(3_600),
    }];

    assert_eq!(
        effective_cluster_gc_life_time_sec(current_version, life_time_sec, hb_timeout, &states),
        life_time_sec
    );
    assert_eq!(
        compute_cluster_gc_safepoint(current_version, life_time_sec, hb_timeout, &states),
        compute_safepoint_version(current_version, life_time_sec)
    );
}

#[test]
fn test_orphan_timeout_calc() {
    let now_ms: i64 = 1_700_000_000_000;
    let orphan_timeout_sec: u64 = 300;
    let timeout_ms = (orphan_timeout_sec as i64).saturating_mul(1000);
    let cutoff = now_ms.saturating_sub(timeout_ms);

    let old_claim = now_ms - 301_000;
    assert!(
        old_claim < cutoff,
        "claim older than timeout should be detected as orphan"
    );

    let recent_claim = now_ms - 299_000;
    assert!(
        recent_claim >= cutoff,
        "claim within timeout should NOT be orphaned"
    );

    let edge_claim = cutoff;
    assert!(
        edge_claim >= cutoff,
        "claim exactly at cutoff boundary is not orphaned"
    );
}

#[test]
fn test_rand_jitter_secs_within_bounds() {
    for max in [1u64, 10, 60, 120, 3600] {
        let jitter = rand_jitter_secs(max);
        assert!(jitter < max, "jitter {} should be < max {}", jitter, max);
    }
}

#[test]
fn test_rand_jitter_secs_max_one() {
    let jitter = rand_jitter_secs(1);
    assert_eq!(jitter, 0, "jitter with max=1 must be 0");
}

#[test]
fn test_effective_cron_orphan_timeout_respects_worker_timeout() {
    let cron_cfg = CronConfig {
        orphan_timeout_sec: 300,
        ..Default::default()
    };

    let worker_cfg = WorkerConfig {
        cron_job_timeout_ms: 1_800_000,
        ..Default::default()
    };

    assert_eq!(
        effective_cron_orphan_timeout_sec(&cron_cfg, &worker_cfg),
        1_800
    );
}

#[test]
fn test_effective_cron_orphan_timeout_keeps_larger_cron_value() {
    let cron_cfg = CronConfig {
        orphan_timeout_sec: 7_200,
        ..Default::default()
    };

    let worker_cfg = WorkerConfig {
        cron_job_timeout_ms: 1_800_000,
        ..Default::default()
    };

    assert_eq!(
        effective_cron_orphan_timeout_sec(&cron_cfg, &worker_cfg),
        7_200
    );
}

#[test]
fn live_hnsw_s3_classification_treats_current_as_live_and_clears_stale_marker() {
    assert_eq!(
        classify_live_hnsw_s3_version(6, 6, false),
        LiveHnswS3VersionDisposition::Current {
            clear_stale_retired_marker: false
        }
    );
    assert_eq!(
        classify_live_hnsw_s3_version(6, 6, true),
        LiveHnswS3VersionDisposition::Current {
            clear_stale_retired_marker: true
        }
    );
}

#[test]
fn live_hnsw_s3_classification_treats_older_versions_as_retired_only_when_behind_current() {
    assert_eq!(
        classify_live_hnsw_s3_version(6, 5, false),
        LiveHnswS3VersionDisposition::HistoricalRetired
    );
    assert_eq!(
        classify_live_hnsw_s3_version(6, 5, true),
        LiveHnswS3VersionDisposition::HistoricalRetired
    );
}

#[test]
fn live_hnsw_s3_classification_never_retires_future_versions() {
    assert_eq!(
        classify_live_hnsw_s3_version(5, 6, false),
        LiveHnswS3VersionDisposition::FutureSpeculative {
            clear_stale_retired_marker: false
        }
    );
    assert_eq!(
        classify_live_hnsw_s3_version(5, 6, true),
        LiveHnswS3VersionDisposition::FutureSpeculative {
            clear_stale_retired_marker: true
        }
    );
    assert_eq!(
        classify_live_hnsw_s3_version(0, 1, false),
        LiveHnswS3VersionDisposition::FutureSpeculative {
            clear_stale_retired_marker: false
        }
    );
}

#[test]
fn gc_orphan_cleanup_uses_claim_deletion_api() {
    // Source-contract: cleanup_orphan_claims_batch must delete via the
    // claim-specific API (delete_worker_claim_by_raw_key), not the
    // queue-entry API (delete_worker_queue_entry).
    //
    // This test FAILS if someone changes the deletion call back to
    // delete_worker_queue_entry in gc.rs.
    let source = include_str!("../gc.rs");
    // Split at #[cfg(test)] to inspect only production code, avoiding
    // false positives from strings inside this very test module.
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    assert!(
        prod_source.contains("delete_worker_claim_by_raw_key"),
        "gc.rs must call delete_worker_claim_by_raw_key for orphan claim cleanup"
    );
    assert!(
        !prod_source.contains("delete_worker_queue_entry"),
        "gc.rs production code must NOT call delete_worker_queue_entry — \
             claim keys require delete_worker_claim_by_raw_key"
    );
}

#[test]
fn gc_tso_timeout_uses_dedicated_client_timeout_path() {
    let source = include_str!("../gc.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    assert!(
        prod_source.contains("current_timestamp_with_timeout"),
        "gc.rs must use TransactionClient::current_timestamp_with_timeout so \
             GC TSO probes stay on the dedicated timed path"
    );
    assert!(
            !prod_source.contains("tokio::time::timeout(\n        Duration::from_secs(TSO_TIMEOUT_SEC),\n        client.current_timestamp(),"),
            "gc.rs must not wrap client.current_timestamp() directly; that bypasses \
             the dedicated timed TSO path"
        );
}

#[test]
fn gc_update_safepoint_has_total_timeout_bound() {
    let source = include_str!("../gc.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    assert!(
        prod_source.contains("SAFEPOINT_UPDATE_TIMEOUT_SEC"),
        "gc.rs must define an explicit total timeout for update_safepoint"
    );
    assert!(
        prod_source.contains("tokio::time::timeout(")
            && prod_source.contains("client.update_safepoint(safepoint)"),
        "gc.rs must bound update_safepoint with an outer timeout"
    );
}

#[test]
fn gc_stale_reaper_rechecks_current_row_before_delete() {
    let source = include_str!("../gc.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    assert!(
        prod_source.contains("get_gc_instance_state_for_update"),
        "gc.rs stale-row reaping must re-read the current row under lock before delete"
    );
}

#[test]
fn gc_registry_loops_do_not_add_startup_jitter() {
    let source = include_str!("../gc.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    let publisher_fn = prod_source
        .split("pub async fn run_gc_publisher_loop")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub async fn publish_gc_instance_state_once")
                .next()
        })
        .expect("gc.rs must define run_gc_publisher_loop before publish_gc_instance_state_once");
    let advancer_fn = prod_source
        .split("pub async fn run_gc_advancer_loop")
        .nth(1)
        .and_then(|rest| {
            rest.split("/// Remove this process's GC registry row")
                .next()
        })
        .expect("gc.rs must define run_gc_advancer_loop");

    assert!(
            !publisher_fn.contains("rand_jitter_secs"),
            "GC registry publisher must not sleep behind startup jitter; heartbeat cadence is part of the safepoint contract"
        );
    assert!(
            !advancer_fn.contains("rand_jitter_secs"),
            "GC safepoint advancer must not sleep behind startup jitter; initial cadence must stay deterministic"
        );
}

#[test]
fn gc_publisher_loop_publishes_immediately_on_entry() {
    let source = include_str!("../gc.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    let publisher_fn = prod_source
        .split("pub async fn run_gc_publisher_loop")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub async fn publish_gc_instance_state_once")
                .next()
        })
        .expect("gc.rs must define run_gc_publisher_loop before publish_gc_instance_state_once");

    assert!(
        publisher_fn
            .contains("interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);"),
        "GC publisher loop must delay missed ticks instead of bursting multiple heartbeats"
    );

    // Extract the region between `interval` creation and `loop {` — this is
    // where a skip-first-tick call would live if someone re-added it.
    let between_interval_and_loop = publisher_fn
        .split("tokio::time::interval(")
        .nth(1)
        .and_then(|after_interval| after_interval.split("loop {").next())
        .expect(
            "run_gc_publisher_loop must contain a tokio::time::interval() call followed by loop {",
        );

    assert!(
        !between_interval_and_loop.contains("interval.tick().await"),
        "The publisher loop must not skip its first tick — doing so creates a \
             full-interval blind window on panic-restart because the supervisor \
             re-enters the loop without a prior synchronous publish."
    );
}

#[tokio::test]
async fn cleanup_orphan_claims_respects_batch_size() {
    let orphan_count = 7usize;
    let batch_size = 3usize;
    let expected_batches = orphan_count.div_ceil(batch_size);
    let all_claim_keys: Vec<Vec<u8>> = (0..orphan_count).map(|idx| vec![idx as u8]).collect();
    let mut processed_keys: Vec<Vec<u8>> = Vec::new();

    let (cleaned, batches) = run_claim_gc_batches(batch_size, |start_after, requested_size| {
        let start_index = start_after
            .as_ref()
            .and_then(|key| {
                all_claim_keys
                    .iter()
                    .position(|candidate_key| candidate_key == key)
            })
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let end_index = (start_index + requested_size).min(all_claim_keys.len());
        let page_keys = all_claim_keys[start_index..end_index].to_vec();
        processed_keys.extend(page_keys.iter().cloned());

        let scanned = page_keys.len();
        let last_key = page_keys.last().cloned();
        future::ready(Ok(ClaimGcBatch {
            scanned,
            cleaned: scanned as u32,
            last_key,
        }))
    })
    .await
    .expect("pagination loop should succeed");

    assert_eq!(batches, expected_batches);
    assert_eq!(cleaned, orphan_count as u32);
    assert_eq!(processed_keys, all_claim_keys);
}

#[tokio::test]
async fn run_claim_gc_batches_clamps_batch_size_above_u32_max() {
    let oversized_batch_size = (u32::MAX as usize) + 1;
    let mut call_count = 0usize;

    let (cleaned, batches) =
        run_claim_gc_batches(oversized_batch_size, |start_after, requested_size| {
            assert_eq!(
                requested_size,
                u32::MAX as usize,
                "batch size must be clamped at consumption point",
            );

            let result = match call_count {
                0 => {
                    assert_eq!(start_after, None);
                    ClaimGcBatch {
                        scanned: u32::MAX as usize,
                        cleaned: 0,
                        last_key: Some(vec![1]),
                    }
                }
                1 => {
                    assert_eq!(start_after, Some(vec![1]));
                    ClaimGcBatch {
                        scanned: 1,
                        cleaned: 1,
                        last_key: Some(vec![2]),
                    }
                }
                _ => panic!("loop should terminate after second batch"),
            };
            call_count += 1;
            future::ready(Ok(result))
        })
        .await
        .expect("pagination loop should succeed");

    assert_eq!(batches, 2, "must continue after first full capped batch");
    assert_eq!(cleaned, 1);
    assert_eq!(call_count, 2);
}

#[test]
fn clear_gc_instance_state_neutralizes_before_delete() {
    let source = include_str!("../gc.rs");
    let clear_fn = source
        .split("pub async fn clear_gc_instance_state")
        .nth(1)
        .and_then(|rest| rest.split("\npub ").next())
        .expect("gc.rs must define clear_gc_instance_state");

    // Phase 1: must publish min_start_ts=None to neutralize the row.
    let neutralize_pos = clear_fn
        .find("put_gc_instance_state")
        .expect("clear_gc_instance_state must publish a neutralizing heartbeat");
    assert!(
        clear_fn.contains("None, // no min_start_ts"),
        "neutralizing publish must pass min_start_ts = None"
    );

    // Phase 2: must delete the row after neutralizing.
    let delete_pos = clear_fn
        .find("delete_gc_instance_state")
        .expect("clear_gc_instance_state must delete the row");

    assert!(
        neutralize_pos < delete_pos,
        "must neutralize (publish None) before deleting the row"
    );
}

// ── End-to-end GC safepoint scenario tests ──────────────────
//
// These exercise the full chain: ActiveTxnRegistry → min_start_ts →
// compute_cluster_gc_safepoint, verifying each real-world scenario
// that the PR set out to protect against.

use crate::worker::active_txn_registry::ActiveTxnRegistry;

/// Helper: build a TSO version from millisecond timestamp.
fn tso(ms: u64) -> u64 {
    ms << 18
}

/// Helper: build a live GcInstanceState with the given min_start_ts.
fn live_state(instance_id: &str, min_start_ts: Option<u64>, updated_at_ms: u64) -> GcInstanceState {
    GcInstanceState {
        instance_id: instance_id.to_string(),
        min_start_ts,
        updated_at_version: tso(updated_at_ms),
        legacy_max_untracked_timeout_sec: None,
    }
}

// ── Scenario 1: Worker txn keeps safepoint clamped ──────────
//
// All e2e tests use txn start_ts values OLDER than gc_life_time so
// the time-based safepoint alone would NOT protect them. This proves
// the ActiveTxnRegistry is essential for protection.
//
// Timeline layout (gc_life_time = 600s = 600_000ms):
//   now            = tso(10_000_000)
//   time_based_sp  = tso(10_000_000 - 600_000) = tso(9_400_000)
//   old txn        = tso(9_000_000) — 1000s old, OUTSIDE gc_life_time
//
// Without registry: safepoint = tso(9_400_000) > tso(9_000_000) → txn exposed
// With registry:    safepoint = tso(9_000_000) - 1 → txn protected

#[test]
fn e2e_worker_txn_clamps_safepoint_then_releases_on_commit() {
    let registry = Arc::new(ActiveTxnRegistry::new());
    let life_time_sec = 600; // time_based_sp = tso(9_400_000)
    let now = tso(10_000_000);
    let time_based_sp = compute_safepoint_version(now, life_time_sec);

    // Worker begins a long-lived scan txn 1000s ago (outside gc_life_time).
    let worker_start_ts = tso(9_000_000);
    assert!(
        time_based_sp > worker_start_ts,
        "precondition: time-based safepoint must exceed txn start_ts to prove registry is needed"
    );

    let guard = registry.track_worker_txn(worker_start_ts);
    assert_eq!(registry.min_start_ts(), Some(worker_start_ts));

    // Simulate publisher: publish min_start_ts to cluster.
    let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);

    // Registry clamps safepoint below the worker txn.
    assert_eq!(safepoint, worker_start_ts - 1);

    // Worker commits — guard drops, unregisters.
    drop(guard);
    assert_eq!(registry.min_start_ts(), None);

    // Next publish: no txns → safepoint advances past old start_ts.
    let state_after = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint_after =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state_after]);
    assert!(
        safepoint_after > worker_start_ts,
        "safepoint {safepoint_after} should advance past old worker start_ts {worker_start_ts}"
    );
}

// ── Scenario 2: Session txn keeps safepoint clamped ─────────

#[test]
fn e2e_session_txn_clamps_safepoint_then_releases_on_commit() {
    let registry = Arc::new(ActiveTxnRegistry::new());
    let life_time_sec = 600;
    let now = tso(10_000_000);

    // Session with an old explicit transaction (outside gc_life_time).
    let session_start_ts = tso(9_000_000);
    registry.register_connection(42, session_start_ts);
    assert_eq!(registry.min_start_ts(), Some(session_start_ts));

    let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
    assert_eq!(safepoint, session_start_ts - 1);

    // Session commits — clean unregister.
    registry.unregister_connection(42);
    assert_eq!(registry.min_start_ts(), None);

    let state_after = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint_after =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state_after]);
    assert!(safepoint_after > session_start_ts);
}

// ── Scenario 3: Worker rollback failure → quarantine protects ─

#[test]
fn e2e_worker_rollback_failure_quarantine_keeps_safepoint_clamped() {
    let registry = Arc::new(ActiveTxnRegistry::new());
    let life_time_sec = 600;
    let now = tso(10_000_000);

    let worker_start_ts = tso(9_000_000);
    let mut guard = registry.track_worker_txn(worker_start_ts);

    // Commit fails, rollback also fails → quarantine the guard.
    guard.quarantine();
    drop(guard);

    // Even after guard drop, registry still has the entry (quarantined).
    assert_eq!(registry.min_start_ts(), Some(worker_start_ts));
    assert_eq!(registry.quarantined_len(), 1);

    // Safepoint still clamped by quarantined entry.
    let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
    assert_eq!(safepoint, worker_start_ts - 1);

    // After QUARANTINE_TTL, the publisher reaps the entry.
    let reaped = registry.reap_quarantined_with_ttl(std::time::Duration::ZERO);
    assert_eq!(reaped, 1);
    assert_eq!(registry.min_start_ts(), None);

    // Safepoint now advances.
    let state_after = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint_after =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state_after]);
    assert!(safepoint_after > worker_start_ts);
}

// ── Scenario 4: Session disconnect → quarantine protects ────

#[test]
fn e2e_session_disconnect_quarantine_keeps_safepoint_clamped() {
    let registry = Arc::new(ActiveTxnRegistry::new());
    let life_time_sec = 600;
    let now = tso(10_000_000);

    let session_start_ts = tso(9_000_000);
    registry.register_connection(99, session_start_ts);

    // Client disconnects — Session::drop calls quarantine_connection.
    registry.quarantine_connection(99);

    // Registry still holds the entry (quarantined).
    assert_eq!(registry.min_start_ts(), Some(session_start_ts));
    assert_eq!(registry.quarantined_len(), 1);

    let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
    assert_eq!(safepoint, session_start_ts - 1);

    // After TTL, publisher reaps.
    registry.reap_quarantined_with_ttl(std::time::Duration::ZERO);
    assert_eq!(registry.min_start_ts(), None);
}

// ── Scenario 5: Session commit then drop → quarantine is no-op

#[test]
fn e2e_clean_session_commit_then_drop_no_quarantine_leak() {
    let registry = Arc::new(ActiveTxnRegistry::new());

    let session_start_ts = tso(9_000_000);
    registry.register_connection(77, session_start_ts);
    assert_eq!(registry.min_start_ts(), Some(session_start_ts));

    // Session commits successfully — unregister.
    registry.unregister_connection(77);
    assert_eq!(registry.min_start_ts(), None);

    // Session::drop fires — quarantine_connection is a no-op since
    // the entry was already removed by commit.
    registry.quarantine_connection(77);
    assert_eq!(registry.quarantined_len(), 0);
    assert_eq!(registry.min_start_ts(), None);
}

// ── Scenario 6: Multi-instance cluster safepoint ────────────

#[test]
fn e2e_multi_instance_safepoint_clamps_to_global_minimum() {
    let life_time_sec = 600;
    let now = tso(10_000_000);

    // Instance A: has a long-running txn from 1000s ago (outside gc_life_time).
    let inst_a = live_state("inst-a", Some(tso(9_000_000)), 10_000_000);
    // Instance B: all txns are recent (within gc_life_time).
    let inst_b = live_state("inst-b", Some(tso(9_999_000)), 10_000_000);
    // Instance C: no active txns.
    let inst_c = live_state("inst-c", None, 10_000_000);

    let safepoint =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[inst_a, inst_b, inst_c]);

    // Must clamp to instance A's old txn (the global minimum).
    assert_eq!(safepoint, tso(9_000_000) - 1);
}

// ── Scenario 7: Stale instance row ignored by advancer ──────

#[test]
fn e2e_stale_instance_row_does_not_block_gc_advancement() {
    let life_time_sec = 600;
    let now = tso(10_000_000);

    // Instance A: live, no active txns.
    let inst_a = live_state("inst-a", None, 10_000_000);
    // Instance B: STALE — last heartbeat was far in the past.
    let inst_b = GcInstanceState {
        instance_id: "inst-b".to_string(),
        min_start_ts: Some(tso(1_000_000)), // very old start_ts
        updated_at_version: tso(1_000_000), // very old heartbeat
        legacy_max_untracked_timeout_sec: None,
    };

    let safepoint =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[inst_a, inst_b]);

    // Stale instance B must be ignored — safepoint is purely time-based.
    let time_based_safepoint = compute_safepoint_version(now, life_time_sec);
    assert_eq!(safepoint, time_based_safepoint);
}

// ── Scenario 8: Shutdown neutralize → row cannot clamp GC ───

#[test]
fn e2e_shutdown_neutralized_row_cannot_clamp_safepoint() {
    let life_time_sec = 600;
    let now = tso(10_000_000);

    // Before shutdown: instance has an active old txn.
    let before = live_state("inst-1", Some(tso(9_000_000)), 10_000_000);
    let safepoint_before =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[before]);
    assert_eq!(safepoint_before, tso(9_000_000) - 1);

    // Shutdown publishes min_start_ts=None (neutralize).
    let neutralized = live_state("inst-1", None, 10_000_000);
    let safepoint_after =
        compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[neutralized]);

    // Neutralized row doesn't clamp — safepoint is purely time-based.
    let time_based = compute_safepoint_version(now, life_time_sec);
    assert_eq!(safepoint_after, time_based);
    assert!(safepoint_after > tso(9_000_000));
}

// ── Scenario 9: Mixed connections + workers + quarantine ────

#[test]
fn e2e_mixed_connections_workers_quarantine_safepoint_is_global_min() {
    let registry = Arc::new(ActiveTxnRegistry::new());
    let life_time_sec = 600;
    let now = tso(10_000_000);

    // Session connection: start_ts = 9.1M (old, outside gc_life_time)
    registry.register_connection(1, tso(9_100_000));

    // Worker guard: start_ts = 9.2M
    let _guard = registry.track_worker_txn(tso(9_200_000));

    // Quarantined worker: start_ts = 9.0M (oldest)
    {
        let mut old_guard = registry.track_worker_txn(tso(9_000_000));
        old_guard.quarantine();
    }

    // min_start_ts should be the quarantined entry (oldest).
    assert_eq!(registry.min_start_ts(), Some(tso(9_000_000)));

    let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
    let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
    assert_eq!(safepoint, tso(9_000_000) - 1);
}

// ── Scenario 10: DDL txn rotation refreshes registration ────

#[test]
fn e2e_txn_rotation_updates_registry_to_new_start_ts() {
    let registry = Arc::new(ActiveTxnRegistry::new());

    // Session opens txn with old start_ts.
    let old_ts = tso(9_000_000);
    registry.register_connection(10, old_ts);
    assert_eq!(registry.min_start_ts(), Some(old_ts));

    // DDL rotation: commit old, begin new with fresh start_ts.
    // (Simulates begin_replacement_session_owned_txn)
    registry.unregister_connection(10); // clear old
    let new_ts = tso(9_999_000);
    registry.register_connection(10, new_ts); // refresh

    assert_eq!(registry.min_start_ts(), Some(new_ts));
    assert!(new_ts > old_ts, "refreshed start_ts should be newer");
}

// ── Scenario 11: Missed heartbeat exposes live txn ──────────

#[test]
fn e2e_missed_heartbeat_exposes_live_txn_to_gc() {
    let life_time_sec = 600;
    // Heartbeat timeout = 3 * publish_interval.
    // With default gc_safepoint_interval_sec=300, timeout=900s.
    // For this test we use a smaller value to keep the scenario compact.
    let hb_timeout_sec: u64 = 900; // 3 * 300
    let heartbeat_timeout_ms = hb_timeout_sec * 1000;
    let publish_time_ms: u64 = 10_000_000;
    let txn_start_ts = tso(9_000_000);

    let state = GcInstanceState {
        instance_id: "inst-stuck".to_string(),
        min_start_ts: Some(txn_start_ts),
        updated_at_version: tso(publish_time_ms),
        legacy_max_untracked_timeout_sec: None,
    };

    // Phase 1: t=(timeout - 1s) — just inside heartbeat timeout, txn IS protected.
    let now_inside = tso(publish_time_ms + heartbeat_timeout_ms - 1000);
    assert!(is_live_gc_instance_state(
        now_inside,
        hb_timeout_sec,
        &state
    ));
    let sp_inside = compute_cluster_gc_safepoint(
        now_inside,
        life_time_sec,
        hb_timeout_sec,
        std::slice::from_ref(&state),
    );
    assert_eq!(
        sp_inside,
        txn_start_ts - 1,
        "txn must be protected while live"
    );

    // Phase 2: t=(timeout + 1s) — just outside heartbeat timeout (missed heartbeat).
    let now_outside = tso(publish_time_ms + heartbeat_timeout_ms + 1000);
    assert!(!is_live_gc_instance_state(
        now_outside,
        hb_timeout_sec,
        &state
    ));
    let sp_outside = compute_cluster_gc_safepoint(
        now_outside,
        life_time_sec,
        hb_timeout_sec,
        std::slice::from_ref(&state),
    );
    assert!(
        sp_outside > txn_start_ts,
        "VULNERABILITY: safepoint {sp_outside} exceeds live txn {txn_start_ts} \
             — missed heartbeat exposed the txn to GC"
    );
}

// ── Scenario 12: Exact heartbeat timeout boundary (>= edge) ─

#[test]
fn e2e_missed_heartbeat_boundary_exact_life_time_edge() {
    let life_time_sec = 600;
    // Heartbeat timeout derived from 3 * gc_safepoint_interval_sec.
    let hb_timeout_sec: u64 = 900;
    let heartbeat_timeout_ms = hb_timeout_sec * 1000;
    let publish_time_ms: u64 = 10_000_000;
    let txn_start_ts = tso(9_000_000);

    let state = GcInstanceState {
        instance_id: "inst-edge".to_string(),
        min_start_ts: Some(txn_start_ts),
        updated_at_version: tso(publish_time_ms),
        legacy_max_untracked_timeout_sec: None,
    };

    // Exact boundary: updated_at + heartbeat_timeout — still live (>= check).
    let now_exact = tso(publish_time_ms + heartbeat_timeout_ms);
    assert!(is_live_gc_instance_state(now_exact, hb_timeout_sec, &state));
    let sp_exact = compute_cluster_gc_safepoint(
        now_exact,
        life_time_sec,
        hb_timeout_sec,
        std::slice::from_ref(&state),
    );
    assert_eq!(sp_exact, txn_start_ts - 1, "protected at exact boundary");

    // One ms past boundary — stale.
    let now_past = tso(publish_time_ms + heartbeat_timeout_ms + 1);
    assert!(!is_live_gc_instance_state(now_past, hb_timeout_sec, &state));
    let sp_past = compute_cluster_gc_safepoint(now_past, life_time_sec, hb_timeout_sec, &[state]);
    assert!(sp_past > txn_start_ts, "1ms past boundary: txn exposed");
}

// ── Scenario 13: Multi-instance, one stale ──────────────────

#[test]
fn e2e_missed_heartbeat_multi_instance_one_stale_exposes_its_txn() {
    let life_time_sec = 600;
    let hb_timeout_sec: u64 = life_time_sec; // use life_time as timeout for this test
    let now = tso(10_601_000); // 601s after inst-a's last heartbeat
    let txn_a = tso(9_000_000);
    let txn_b = tso(9_500_000);

    // inst-a: stale (last heartbeat 601s ago, > hb_timeout_sec)
    let inst_a = GcInstanceState {
        instance_id: "inst-a".to_string(),
        min_start_ts: Some(txn_a),
        updated_at_version: tso(10_000_000),
        legacy_max_untracked_timeout_sec: None,
    };
    // inst-b: live (just heartbeated)
    let inst_b = live_state("inst-b", Some(txn_b), 10_601_000);

    assert!(!is_live_gc_instance_state(now, hb_timeout_sec, &inst_a));
    assert!(is_live_gc_instance_state(now, hb_timeout_sec, &inst_b));

    let sp = compute_cluster_gc_safepoint(now, life_time_sec, hb_timeout_sec, &[inst_a, inst_b]);
    assert_eq!(sp, txn_b - 1, "clamped to live inst-b only");
    assert!(sp > txn_a, "inst-a's txn exposed: its row went stale");
    assert!(sp < txn_b, "inst-b's txn still protected");
}

// ── Contract: publisher retries on failure ───────────────────

#[test]
fn gc_publisher_loop_retries_on_failure_with_backoff() {
    let source = include_str!("../gc.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("gc.rs must contain #[cfg(test)]");
    let publisher_fn = prod_source
        .split("pub async fn run_gc_publisher_loop")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub async fn publish_gc_instance_state_once")
                .next()
        })
        .expect("run_gc_publisher_loop must exist");

    assert!(
        publisher_fn.contains("backoff"),
        "publisher must retry with exponential backoff on failure, not wait a full interval"
    );
    assert!(
        publisher_fn.contains("tokio::time::sleep(backoff)"),
        "publisher retry must use sleep-based backoff between attempts"
    );
}

// ── heartbeat_timeout_sec adapts to config ──────────────────

#[test]
fn heartbeat_timeout_is_3x_publish_interval() {
    let mut config = WorkerConfig::default();
    assert_eq!(config.gc_safepoint_interval_sec, 300);
    assert_eq!(heartbeat_timeout_sec(&config), 900);

    config.gc_safepoint_interval_sec = 60;
    assert_eq!(heartbeat_timeout_sec(&config), 180);

    config.gc_safepoint_interval_sec = 30; // minimum allowed
    assert_eq!(heartbeat_timeout_sec(&config), 90);
}

#[test]
fn heartbeat_timeout_always_less_than_default_gc_life_time() {
    // The config validator enforces gc_life_time >= 3 * interval,
    // so heartbeat_timeout (= 3 * interval) <= gc_life_time.
    let config = WorkerConfig::default();
    assert!(
        heartbeat_timeout_sec(&config) <= config.gc_life_time_sec,
        "heartbeat_timeout {} must not exceed gc_life_time {}",
        heartbeat_timeout_sec(&config),
        config.gc_life_time_sec,
    );
}

#[test]
fn heartbeat_timeout_exceeds_publish_interval() {
    // Core invariant: timeout > interval, otherwise healthy instances look dead.
    let config = WorkerConfig::default();
    assert!(
        heartbeat_timeout_sec(&config) > config.gc_safepoint_interval_sec,
        "heartbeat_timeout {} must exceed publish interval {}",
        heartbeat_timeout_sec(&config),
        config.gc_safepoint_interval_sec,
    );
}

// --- SweepBackoff tests ---

#[test]
fn sweep_backoff_new_failed_sets_retry_in_future() {
    let b = SweepBackoff::new_failed(600);
    assert_eq!(b.consecutive_failures, 1);
    assert!(
        b.should_skip(),
        "should skip immediately after first failure"
    );
}

#[test]
fn sweep_backoff_escalates_exponentially() {
    let mut b = SweepBackoff::new_failed(10); // 10s interval for fast test
                                              // After 1st failure: skip 1 interval (10s)
    assert_eq!(b.consecutive_failures, 1);

    b.record_failure(10);
    assert_eq!(b.consecutive_failures, 2);
    // After 2nd: skip 2 intervals (20s)

    b.record_failure(10);
    assert_eq!(b.consecutive_failures, 3);
    // After 3rd: skip 4 intervals (40s)

    b.record_failure(10);
    assert_eq!(b.consecutive_failures, 4);
    // After 4th: skip 8 intervals (80s)
}

#[test]
fn sweep_backoff_caps_at_max_shift() {
    let mut b = SweepBackoff::new_failed(10);
    for _ in 0..20 {
        b.record_failure(10);
    }
    assert_eq!(b.consecutive_failures, 21);
    // Even after 21 failures, backoff is capped at 2^5 = 32 intervals
    assert!(b.should_skip());
}

#[test]
fn sweep_backoff_should_skip_returns_false_after_delay() {
    let b = SweepBackoff {
        consecutive_failures: 1,
        retry_after: Instant::now() - Duration::from_secs(1), // already past
    };
    assert!(
        !b.should_skip(),
        "should not skip when retry_after is in the past"
    );
}
