//! Background worker for asynchronous AFTER triggers.

use super::queue::{
    encode_trigger_dlq_key, encode_trigger_queue_key, EventStatus, TriggerEvent,
};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::error::SqlError;
use crate::sql::executor::Executor;
use crate::storage::TikvStore;
use anyhow::Result;
use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::warn;

// Re-export for compat alias (trigger_worker::enqueue_after_triggers)
pub(crate) use super::enqueue::enqueue_after_triggers;

const MAX_CONCURRENT_KEYSPACES: usize = 4;

const DEFAULT_MAX_QUEUE_DEPTH: usize = 10_000;
const DEFAULT_MAX_EVENTS_PER_BATCH: usize = 10;
const DEFAULT_MAX_RETRIES: u8 = 3;

const DEFAULT_POLL_INTERVAL_MS: u64 = 100;
const DEFAULT_KEYSPACE_ERROR_BACKOFF_INITIAL_MS: u64 = 1_000;
const DEFAULT_KEYSPACE_ERROR_BACKOFF_MAX_MS: u64 = 60_000;
const DEFAULT_GC_INTERVAL_SEC: u64 = 60;
const DEFAULT_DONE_RETENTION_SEC: u64 = 3600;
const DEFAULT_DLQ_RETENTION_DAYS: u64 = 7;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;

#[derive(Debug, Clone)]
pub(crate) struct TriggerWorkerConfig {
    pub enabled: bool,
    pub poll_interval_ms: u64,
    pub idle_grace_ms: u64,
    pub gc_interval_sec: u64,
    pub done_retention_sec: u64,
    pub dlq_retention_days: u64,
    pub orphan_timeout_sec: u64,
    pub default_max_queue_depth: usize,
    pub default_max_events_per_batch: usize,
    pub default_max_retries: u8,
}

impl Default for TriggerWorkerConfig {
    fn default() -> Self {
        let poll_interval_ms = DEFAULT_POLL_INTERVAL_MS;
        // Default idle grace: max(10 * poll_interval, 1000ms) for reasonable buffer.
        let idle_grace_ms = poll_interval_ms.saturating_mul(10).max(1000);
        Self {
            enabled: true,
            poll_interval_ms,
            idle_grace_ms,
            gc_interval_sec: DEFAULT_GC_INTERVAL_SEC,
            done_retention_sec: DEFAULT_DONE_RETENTION_SEC,
            dlq_retention_days: DEFAULT_DLQ_RETENTION_DAYS,
            orphan_timeout_sec: DEFAULT_ORPHAN_TIMEOUT_SEC,
            default_max_queue_depth: DEFAULT_MAX_QUEUE_DEPTH,
            default_max_events_per_batch: DEFAULT_MAX_EVENTS_PER_BATCH,
            default_max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

impl TriggerWorkerConfig {
    fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = env::var("PGTIKV_TRIGGER_ENABLED") {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_POLL_MS") {
            cfg.poll_interval_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.poll_interval_ms);
            // Recompute idle_grace if poll_interval was overridden.
            cfg.idle_grace_ms = cfg.poll_interval_ms.saturating_mul(10).max(1000);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_IDLE_GRACE_MS") {
            cfg.idle_grace_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.idle_grace_ms);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_GC_INTERVAL_SEC") {
            cfg.gc_interval_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.gc_interval_sec);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_DONE_RETENTION_SEC") {
            cfg.done_retention_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.done_retention_sec);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_DLQ_RETENTION_DAYS") {
            cfg.dlq_retention_days = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.dlq_retention_days);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_ORPHAN_TIMEOUT_SEC") {
            cfg.orphan_timeout_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.orphan_timeout_sec);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_QUEUE_LIMIT") {
            cfg.default_max_queue_depth = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.default_max_queue_depth);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_BATCH_SIZE") {
            cfg.default_max_events_per_batch = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.default_max_events_per_batch);
        }
        if let Ok(v) = env::var("PGTIKV_TRIGGER_MAX_RETRIES") {
            cfg.default_max_retries = v
                .parse::<u8>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.default_max_retries);
        }

        cfg
    }
}

#[derive(Debug)]
pub(crate) struct KeyspaceQuota {
    pub max_queue_depth: usize,
    pub max_events_per_batch: usize,
    pub max_retries: u8,
    pub current_depth: AtomicUsize,
}

impl KeyspaceQuota {
    fn new(cfg: &TriggerWorkerConfig) -> Self {
        Self {
            max_queue_depth: cfg.default_max_queue_depth,
            max_events_per_batch: cfg.default_max_events_per_batch,
            max_retries: cfg.default_max_retries,
            current_depth: AtomicUsize::new(0),
        }
    }

    pub(super) fn dec_current_depth(&self) {
        let _ = self
            .current_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

pub(crate) struct TriggerWorker {
    pub(super) worker_id: String,
    // Single authoritative source: keyspace is active iff it has an entry here.
    // The Instant value is the timestamp when mark_active() was last called.
    pub(super) active_keyspaces: DashMap<String, Instant>,
    quotas: DashMap<String, Arc<KeyspaceQuota>>,
    keyspace_backoff: DashMap<String, KeyspaceBackoff>,
    pub(super) config: TriggerWorkerConfig,
    pub(super) shutdown: AtomicBool,
    default_search_path: Vec<String>,
    wake: tokio::sync::Notify,
}

#[derive(Debug, Clone, Copy)]
struct KeyspaceBackoff {
    next_retry_at: Instant,
    delay_ms: u64,
    failures: u32,
}

impl TriggerWorker {
    fn new() -> Self {
        Self {
            worker_id: format!("worker-{}", std::process::id()),
            active_keyspaces: DashMap::new(),
            quotas: DashMap::new(),
            keyspace_backoff: DashMap::new(),
            config: TriggerWorkerConfig::from_env(),
            shutdown: AtomicBool::new(false),
            default_search_path: vec!["public".to_string()],
            wake: tokio::sync::Notify::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn new_for_test() -> Self {
        Self::new()
    }

    pub(crate) fn config(&self) -> &TriggerWorkerConfig {
        &self.config
    }

    pub(crate) fn mark_active(&self, keyspace: &str) {
        self.active_keyspaces
            .insert(keyspace.to_string(), Instant::now());
        self.wake.notify_one();
    }

    pub(crate) fn take_active_keyspaces_snapshot(&self) -> Vec<String> {
        self.active_keyspaces
            .iter()
            .map(|r| r.key().clone())
            .collect()
    }

    /// Conditionally remove keyspace from active set only if the stored timestamp matches expected_at.
    /// This prevents clobbering concurrent mark_active() calls (CAS-style operation).
    pub(crate) fn remove_active_if_unchanged(&self, keyspace: &str, expected_at: Instant) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.active_keyspaces.entry(keyspace.to_string()) {
            Entry::Occupied(entry) => {
                if *entry.get() == expected_at {
                    entry.remove();
                    true
                } else {
                    false
                }
            }
            Entry::Vacant(_) => false,
        }
    }

    pub(crate) fn get_quota(&self, keyspace: &str) -> Arc<KeyspaceQuota> {
        if let Some(existing) = self.quotas.get(keyspace) {
            return existing.clone();
        }
        let quota = Arc::new(KeyspaceQuota::new(&self.config));
        self.quotas
            .entry(keyspace.to_string())
            .or_insert(quota)
            .clone()
    }

    fn should_process_keyspace(&self, keyspace: &str, now: Instant) -> bool {
        match self.keyspace_backoff.get(keyspace) {
            Some(backoff) => now >= backoff.next_retry_at,
            None => true,
        }
    }

    fn clear_keyspace_backoff(&self, keyspace: &str) {
        self.keyspace_backoff.remove(keyspace);
    }

    fn record_keyspace_failure(&self, keyspace: &str, now: Instant) -> (Duration, u32) {
        let mut entry =
            self.keyspace_backoff
                .entry(keyspace.to_string())
                .or_insert(KeyspaceBackoff {
                    next_retry_at: now,
                    delay_ms: 0,
                    failures: 0,
                });

        entry.failures = entry.failures.saturating_add(1);
        entry.delay_ms = match entry.delay_ms {
            0 => DEFAULT_KEYSPACE_ERROR_BACKOFF_INITIAL_MS,
            ms => ms
                .saturating_mul(2)
                .min(DEFAULT_KEYSPACE_ERROR_BACKOFF_MAX_MS),
        };
        let delay = Duration::from_millis(entry.delay_ms);
        entry.next_retry_at = now + delay;

        (delay, entry.failures)
    }

    pub(crate) async fn run(&self, pool: Arc<TikvClientPool>) {
        let safety_interval = Duration::from_secs(30);

        let gc_pool = pool.clone();
        let gc_handle = tokio::spawn(async move {
            trigger_worker().gc_loop(gc_pool).await;
        });

        while !self.shutdown.load(Ordering::Relaxed) {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(safety_interval) => {}
            }

            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let keyspaces = self.take_active_keyspaces_snapshot();
            if keyspaces.is_empty() {
                continue;
            }

            let now = Instant::now();
            let keyspaces_to_process: Vec<String> = keyspaces
                .into_iter()
                .filter(|ks| self.should_process_keyspace(ks, now))
                .collect();

            if keyspaces_to_process.is_empty() {
                continue;
            }

            let results: Vec<(String, Result<()>)> = stream::iter(keyspaces_to_process)
                .map(|keyspace| {
                    let pool = pool.clone();
                    async move {
                        let result = trigger_worker().process_keyspace(&pool, &keyspace).await;
                        (keyspace, result)
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_KEYSPACES)
                .collect()
                .await;

            for (keyspace, result) in results {
                match result {
                    Ok(()) => {
                        self.clear_keyspace_backoff(&keyspace);
                    }
                    Err(e) => {
                        let (delay, failures) =
                            self.record_keyspace_failure(&keyspace, Instant::now());
                        warn!(
                            "trigger worker error for {} (attempt {}, backoff {:?}): {}",
                            keyspace, failures, delay, e
                        );
                    }
                }
            }
        }

        gc_handle.abort();
    }

    async fn process_keyspace(&self, pool: &Arc<TikvClientPool>, keyspace: &str) -> Result<()> {
        let quota = self.get_quota(keyspace);
        // Use pool.acquire() so the tenant stays active during processing
        // (prevents reaper from evicting mid-work) and gives us cache access.
        let handle = pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();
        let trigger_cache = handle.trigger_cache().clone();
        let stats_cache = handle.stats_cache().clone();

        // Read the current activation timestamp before checking for events.
        // This establishes the "expected" value for conditional removal.
        let marked_at = self.active_keyspaces.get(keyspace).map(|v| *v);

        // Claim a fair batch (bounded per keyspace).
        let mut txn = store.begin().await?;
        let claim = self
            .claim_events(&mut txn, keyspace, &quota, quota.max_events_per_batch)
            .await?;
        if claim.claimed.is_empty() {
            if claim.quarantined > 0 {
                txn.commit().await?;
            } else {
                let _ = txn.rollback().await;
            }
            // Nothing pending; stop polling this keyspace until new enqueue.
            //
            // Race mitigation (Phase 1):
            // 1. Use configurable idle_grace derived from poll_interval (default: max(10*poll, 1s))
            // 2. Only remove if stored timestamp matches the value we read before checking events
            //    (prevents clobbering concurrent mark_active calls)
            //
            // Root cause: enqueue marks active in-transaction before commit. Worker may poll and see
            // zero events before the enqueue transaction commits, then remove the keyspace, causing
            // the committed event to be invisible until next activation.
            let idle_grace = Duration::from_millis(self.config.idle_grace_ms);
            if let Some(marked_at_instant) = marked_at {
                if marked_at_instant.elapsed() >= idle_grace {
                    self.remove_active_if_unchanged(keyspace, marked_at_instant);
                }
            }
            return Ok(());
        }
        txn.commit().await?;
        let events = claim.claimed;

        let exec = Executor::new(
            store.clone(),
            keyspace.to_string(),
            observability::registry().tenant(keyspace),
            trigger_cache,
            stats_cache,
        );

        for event in events {
            let result = self.execute_event(&exec, &store, keyspace, &event).await;
            self.update_event_status(&store, keyspace, &event, result)
                .await?;
        }

        Ok(())
    }

    async fn execute_event(
        &self,
        executor: &Executor,
        store: &Arc<TikvStore>,
        keyspace: &str,
        event: &TriggerEvent,
    ) -> Result<()> {
        // NOTE: This executes in its own transaction: eventual consistency by design.
        let mut txn = store.begin().await?;
        let db_id = event.db_id;

        let schema = store
            .get_schema(&mut txn, db_id, &event.table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(event.table_name.clone()))?;

        let trigger = store
            .get_trigger(&mut txn, db_id, &event.table_name, &event.trigger_name)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Trigger '{}' not found on '{}'",
                    event.trigger_name,
                    event.table_name
                )
            })?;

        // Best-effort guard: only process events that are still marked Processing for this worker.
        if event.status != EventStatus::Processing {
            return Ok(());
        }

        let func = store
            .get_function(&mut txn, db_id, &trigger.function)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Function '{}' not found", trigger.function))?;

        let mut sequence_values = HashMap::new();
        let start = Instant::now();
        // Wrap trigger body execution with extension context so that
        // extensions (e.g. HTTP) are accessible from trigger functions.
        // Trigger workers run as superuser since they are system-level, but must not
        // allow unsafe local filesystem access (fs9) without an actual session context.
        let ext_ctx = crate::extensions::context::ExtensionContextOpts {
            is_superuser: true,
            allow_local_fs: false,
        };
        crate::extensions::context::with_context_opts(ext_ctx, async {
            self.execute_trigger_body(
                executor,
                &mut txn,
                db_id,
                &mut sequence_values,
                &schema,
                &func.body,
                event.old_row.as_ref(),
                event.new_row.as_ref(),
                &self.default_search_path,
            )
            .await
        })
        .await?;

        txn.commit().await?;

        let elapsed = start.elapsed();
        if elapsed > Duration::from_secs(1) {
            warn!(
                "slow trigger event {} on {} ({}): {:?}",
                event.id, keyspace, event.trigger_name, elapsed
            );
        }

        Ok(())
    }

    async fn update_event_status(
        &self,
        store: &Arc<TikvStore>,
        keyspace: &str,
        event: &TriggerEvent,
        result: Result<()>,
    ) -> Result<()> {
        let quota = self.get_quota(keyspace);
        let mut txn = store.begin().await?;
        let key = encode_trigger_queue_key(event.id);

        match result {
            Ok(()) => {
                // Success: remove from queue.
                txn.delete(key.clone()).await?;
                quota.dec_current_depth();
            }
            Err(e) => {
                let Some(val) = txn.get(key.clone()).await? else {
                    // Event already removed by another worker/GC.
                    txn.commit().await?;
                    return Ok(());
                };
                let mut updated: TriggerEvent = bincode::deserialize(&val)?;
                updated.retry_count = updated.retry_count.saturating_add(1);
                updated.error_msg = Some(e.to_string());

                if updated.retry_count >= quota.max_retries {
                    updated.status = EventStatus::Failed;
                    updated.worker_id = None;
                    updated.claimed_at_ms = None;
                    let dlq_key = encode_trigger_dlq_key(updated.id);
                    txn.put(dlq_key, bincode::serialize(&updated)?).await?;
                    txn.delete(key).await?;
                    quota.dec_current_depth();
                } else {
                    updated.status = EventStatus::Pending;
                    updated.worker_id = None;
                    updated.claimed_at_ms = None;
                    txn.put(key, bincode::serialize(&updated)?).await?;
                    self.mark_active(keyspace);
                }
            }
        }

        txn.commit().await?;
        Ok(())
    }
}

static TRIGGER_WORKER: OnceLock<TriggerWorker> = OnceLock::new();

pub(crate) fn trigger_worker() -> &'static TriggerWorker {
    TRIGGER_WORKER.get_or_init(TriggerWorker::new)
}

static WORKER_STARTED: OnceLock<()> = OnceLock::new();

/// Spawn background tasks for asynchronous trigger processing.
pub(crate) fn spawn_trigger_worker(pool: Arc<TikvClientPool>) {
    let worker = trigger_worker();
    if !worker.config().enabled {
        return;
    }

    if WORKER_STARTED.set(()).is_err() {
        return;
    }

    tokio::spawn(async move {
        trigger_worker().run(pool).await;
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn keyspace_backoff_is_applied_and_cleared() {
        let worker = super::TriggerWorker::new();
        let keyspace = "ks1";
        let t0 = Instant::now();

        assert!(worker.should_process_keyspace(keyspace, t0));

        let (delay1, failures1) = worker.record_keyspace_failure(keyspace, t0);
        assert_eq!(failures1, 1);
        assert_eq!(
            delay1,
            Duration::from_millis(super::DEFAULT_KEYSPACE_ERROR_BACKOFF_INITIAL_MS)
        );
        assert!(!worker.should_process_keyspace(keyspace, t0));
        assert!(worker.should_process_keyspace(keyspace, t0 + delay1));

        let (delay2, failures2) = worker.record_keyspace_failure(keyspace, t0 + delay1);
        assert_eq!(failures2, 2);
        assert_eq!(
            delay2,
            Duration::from_millis(
                (super::DEFAULT_KEYSPACE_ERROR_BACKOFF_INITIAL_MS.saturating_mul(2))
                    .min(super::DEFAULT_KEYSPACE_ERROR_BACKOFF_MAX_MS)
            )
        );

        worker.clear_keyspace_backoff(keyspace);
        assert!(worker.should_process_keyspace(keyspace, t0));
    }

    #[test]
    fn keyspace_backoff_is_capped() {
        let worker = super::TriggerWorker::new();
        let keyspace = "ks2";
        let mut now = Instant::now();

        let mut delay = Duration::from_millis(0);
        for _ in 0..32 {
            (delay, _) = worker.record_keyspace_failure(keyspace, now);
            now += delay;
        }

        assert_eq!(
            delay,
            Duration::from_millis(super::DEFAULT_KEYSPACE_ERROR_BACKOFF_MAX_MS)
        );
    }

    #[test]
    fn current_depth_saturating_decrement_does_not_underflow() {
        let quota = super::KeyspaceQuota {
            max_queue_depth: 1,
            max_events_per_batch: 1,
            max_retries: 1,
            current_depth: AtomicUsize::new(0),
        };
        quota.dec_current_depth();
        assert_eq!(quota.current_depth.load(Ordering::Relaxed), 0);

        let quota = super::KeyspaceQuota {
            max_queue_depth: 1,
            max_events_per_batch: 1,
            max_retries: 1,
            current_depth: AtomicUsize::new(2),
        };
        quota.dec_current_depth();
        assert_eq!(quota.current_depth.load(Ordering::Relaxed), 1);
    }
}
