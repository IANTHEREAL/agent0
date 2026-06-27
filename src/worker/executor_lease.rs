use crate::storage::TikvStore;
use crate::worker::config::WorkerConfig;
use crate::worker::now_epoch_ms;
use crate::worker::types::{WorkerExecutorLease, WorkerExecutorLeaseResult};
use pgwire::tokio::CancellationToken;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const MIN_RENEW_INTERVAL_MS: i64 = 1_000;
const MAX_RENEW_INTERVAL_MS: i64 = 10_000;
const MAX_STANDBY_PROBE_INTERVAL_MS: i64 = 5_000;
const RENEW_FAILURE_RETRY_MS: i64 = 1_000;
const EXPIRED_STORAGE_RESULT_IMMEDIATE_RETRIES: usize = 1;

#[derive(Default)]
struct LeaseState {
    held: bool,
    generation: u64,
    lease_until_ms: i64,
    next_renew_at_ms: i64,
    next_probe_at_ms: i64,
    last_seen_owner: Option<String>,
    last_seen_generation: u64,
}

pub struct WorkerExecutorLeaseCoordinator {
    store: Arc<TikvStore>,
    owner_id: String,
    lease_ms: i64,
    state: Mutex<LeaseState>,
}

impl WorkerExecutorLeaseCoordinator {
    pub fn new(store: Arc<TikvStore>, config: WorkerConfig) -> Self {
        let lease_ms = i64::try_from(config.executor_lease_ms).unwrap_or(i64::MAX);
        let owner_id = executor_owner_id(&config);
        Self {
            store,
            owner_id,
            lease_ms,
            state: Mutex::new(LeaseState::default()),
        }
    }

    pub async fn ensure_current_executor(&self) -> bool {
        let now_ms = now_epoch_ms();
        {
            let state = self.state.lock().await;
            if state.held && now_ms < state.next_renew_at_ms && now_ms < state.lease_until_ms {
                return true;
            }
            if !state.held && now_ms < state.next_probe_at_ms {
                return false;
            }
        }

        let mut state = self.state.lock().await;
        let now_ms = now_epoch_ms();
        if state.held && now_ms < state.next_renew_at_ms && now_ms < state.lease_until_ms {
            return true;
        }
        if !state.held && now_ms < state.next_probe_at_ms {
            return false;
        }

        let mut expired_result_retries = 0;
        loop {
            match self
                .store
                .try_acquire_or_renew_worker_executor_lease(&self.owner_id, self.lease_ms)
                .await
            {
                Ok(WorkerExecutorLeaseResult::Held(lease)) => {
                    let completed_at_ms = now_epoch_ms();
                    if !returned_lease_is_live_at_completion(&lease, completed_at_ms) {
                        warn!(
                            owner_id = %self.owner_id,
                            generation = lease.generation,
                            lease_until_ms = lease.lease_until_ms,
                            completed_at_ms,
                            "Worker executor lease acquire/renew returned an expired lease; retrying"
                        );
                        metrics::counter!(
                            "db9_server_worker_executor_lease_errors_total",
                            "phase" => "expired_after_storage"
                        )
                        .increment(1);
                        clear_current_lease_state(&mut state);
                        state.next_probe_at_ms = 0;
                        metrics::gauge!("db9_server_worker_executor_active").set(0.0);
                        if expired_result_retries < EXPIRED_STORAGE_RESULT_IMMEDIATE_RETRIES {
                            expired_result_retries += 1;
                            continue;
                        }
                        return false;
                    }

                    let newly_held = !state.held || state.generation != lease.generation;
                    state.held = true;
                    state.generation = lease.generation;
                    state.lease_until_ms = lease.lease_until_ms;
                    state.next_renew_at_ms = successful_lease_next_renew_at_ms(
                        completed_at_ms,
                        lease.lease_until_ms,
                        self.renew_interval_ms(),
                    );
                    state.next_probe_at_ms = 0;
                    state.last_seen_owner = Some(self.owner_id.clone());
                    state.last_seen_generation = lease.generation;
                    metrics::gauge!("db9_server_worker_executor_active").set(1.0);
                    if newly_held {
                        metrics::counter!("db9_server_worker_executor_lease_acquisitions_total")
                            .increment(1);
                        info!(
                            owner_id = %self.owner_id,
                            generation = lease.generation,
                            lease_until_ms = lease.lease_until_ms,
                            "Worker executor lease acquired"
                        );
                    } else {
                        debug!(
                            owner_id = %self.owner_id,
                            generation = lease.generation,
                            lease_until_ms = lease.lease_until_ms,
                            "Worker executor lease renewed"
                        );
                    }
                    return true;
                }
                Ok(WorkerExecutorLeaseResult::HeldByOther(lease)) => {
                    let completed_at_ms = now_epoch_ms();
                    let holder_live = returned_lease_is_live_at_completion(&lease, completed_at_ms);
                    if !holder_live {
                        debug!(
                            holder = %lease.owner_id,
                            holder_generation = lease.generation,
                            holder_lease_until_ms = lease.lease_until_ms,
                            completed_at_ms,
                            "Worker executor observed expired holder after storage call; retrying"
                        );
                    } else if state.held {
                        warn!(
                            previous_generation = state.generation,
                            holder = %lease.owner_id,
                            holder_generation = lease.generation,
                            holder_lease_until_ms = lease.lease_until_ms,
                            "Worker executor lease lost; standing by"
                        );
                    } else if state.last_seen_owner.as_deref() != Some(lease.owner_id.as_str())
                        || state.last_seen_generation != lease.generation
                    {
                        info!(
                            holder = %lease.owner_id,
                            holder_generation = lease.generation,
                            holder_lease_until_ms = lease.lease_until_ms,
                            "Worker executor standby; another worker holds lease"
                        );
                    }
                    clear_current_lease_state(&mut state);
                    state.next_probe_at_ms = if holder_live {
                        completed_at_ms.saturating_add(self.standby_probe_interval_ms())
                    } else {
                        0
                    };
                    state.last_seen_owner = Some(lease.owner_id);
                    state.last_seen_generation = lease.generation;
                    metrics::gauge!("db9_server_worker_executor_active").set(0.0);
                    if !holder_live
                        && expired_result_retries < EXPIRED_STORAGE_RESULT_IMMEDIATE_RETRIES
                    {
                        expired_result_retries += 1;
                        continue;
                    }
                    return false;
                }
                Err(err) => {
                    let completed_at_ms = now_epoch_ms();
                    if state.held && completed_at_ms < state.lease_until_ms {
                        warn!(
                            error = %err,
                            lease_until_ms = state.lease_until_ms,
                            "Worker executor lease renew failed; continuing until current lease expires"
                        );
                        metrics::counter!(
                            "db9_server_worker_executor_lease_errors_total",
                            "phase" => "renew"
                        )
                        .increment(1);
                        let retry_ms = RENEW_FAILURE_RETRY_MS
                            .min(state.lease_until_ms.saturating_sub(completed_at_ms).max(1));
                        state.next_renew_at_ms = completed_at_ms.saturating_add(retry_ms);
                        return true;
                    }

                    if state.held {
                        warn!(
                            error = %err,
                            "Worker executor lease expired after renew failure; standing by"
                        );
                    } else {
                        warn!(
                            error = %err,
                            "Worker executor lease acquire failed; standing by"
                        );
                    }
                    metrics::counter!(
                        "db9_server_worker_executor_lease_errors_total",
                        "phase" => "acquire_or_renew"
                    )
                    .increment(1);
                    clear_current_lease_state(&mut state);
                    state.next_probe_at_ms =
                        completed_at_ms.saturating_add(self.standby_probe_interval_ms());
                    metrics::gauge!("db9_server_worker_executor_active").set(0.0);
                    return false;
                }
            }
        }
    }

    pub async fn run_keepalive_loop(&self, shutdown: CancellationToken) {
        info!(
            owner_id = %self.owner_id,
            lease_ms = self.lease_ms,
            "Worker executor lease keepalive loop starting"
        );
        let mut interval =
            tokio::time::interval(Duration::from_millis(MIN_RENEW_INTERVAL_MS as u64));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("Worker executor lease keepalive shutdown requested");
                    break;
                }
                _ = interval.tick() => {
                    self.ensure_current_executor().await;
                }
            }
        }
    }

    pub async fn release_if_owned(&self) {
        match self
            .store
            .release_worker_executor_lease_if_owned(&self.owner_id)
            .await
        {
            Ok(true) => {
                info!(owner_id = %self.owner_id, "Worker executor lease released");
            }
            Ok(false) => {}
            Err(err) => {
                metrics::counter!(
                    "db9_server_worker_executor_lease_errors_total",
                    "phase" => "release"
                )
                .increment(1);
                warn!(error = %err, "Worker executor lease release failed");
            }
        }
        let mut state = self.state.lock().await;
        state.held = false;
        state.generation = 0;
        state.lease_until_ms = 0;
        state.next_renew_at_ms = 0;
        metrics::gauge!("db9_server_worker_executor_active").set(0.0);
    }

    fn renew_interval_ms(&self) -> i64 {
        (self.lease_ms / 3).clamp(MIN_RENEW_INTERVAL_MS, MAX_RENEW_INTERVAL_MS)
    }

    fn standby_probe_interval_ms(&self) -> i64 {
        (self.lease_ms / 3).clamp(MIN_RENEW_INTERVAL_MS, MAX_STANDBY_PROBE_INTERVAL_MS)
    }
}

fn executor_owner_id(config: &WorkerConfig) -> String {
    // DB9_WORKER_ID is operator-overridable and may be reused accidentally.
    // Add the per-process GC instance UUID so executor lease ownership stays
    // unique across SQL-serving pods.
    format!("{}:{}", config.worker_id, config.gc_instance_id)
}

fn clear_current_lease_state(state: &mut LeaseState) {
    state.held = false;
    state.generation = 0;
    state.lease_until_ms = 0;
    state.next_renew_at_ms = 0;
}

fn returned_lease_is_live_at_completion(lease: &WorkerExecutorLease, completed_at_ms: i64) -> bool {
    lease.is_live_at(completed_at_ms)
}

fn successful_lease_next_renew_at_ms(
    completed_at_ms: i64,
    lease_until_ms: i64,
    renew_interval_ms: i64,
) -> i64 {
    let remaining_ms = lease_until_ms.saturating_sub(completed_at_ms);
    let renew_delay_ms = renew_interval_ms
        .max(1)
        .min(remaining_ms.saturating_sub(1).max(1));
    completed_at_ms.saturating_add(renew_delay_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_owner_id_stays_unique_when_worker_id_is_reused() {
        let mut a = WorkerConfig {
            worker_id: "worker".to_string(),
            gc_instance_id: "instance-a".to_string(),
            ..Default::default()
        };
        let b = WorkerConfig {
            worker_id: "worker".to_string(),
            gc_instance_id: "instance-b".to_string(),
            ..Default::default()
        };

        assert_ne!(executor_owner_id(&a), executor_owner_id(&b));
        a.gc_instance_id = b.gc_instance_id.clone();
        assert_eq!(executor_owner_id(&a), executor_owner_id(&b));
    }

    #[test]
    fn returned_lease_must_be_live_at_storage_completion() {
        let lease = WorkerExecutorLease::new("worker".to_string(), 1_000, 30_000, 1);

        assert!(returned_lease_is_live_at_completion(&lease, 30_999));
        assert!(!returned_lease_is_live_at_completion(&lease, 31_000));
        assert!(!returned_lease_is_live_at_completion(&lease, 31_001));
    }

    #[test]
    fn successful_lease_renewal_is_not_scheduled_after_expiry() {
        assert_eq!(
            successful_lease_next_renew_at_ms(10_000, 40_000, 10_000),
            20_000
        );
        assert_eq!(
            successful_lease_next_renew_at_ms(10_000, 10_500, 10_000),
            10_499
        );
    }
}
