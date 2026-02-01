//! Background worker and enqueue path for asynchronous AFTER triggers.
//!
//! Phase 1 responsibilities (MVP):
//! - Provide a cheap enqueue API used by DML execution (in-transaction).
//! - Maintain per-keyspace quotas and an in-process "active keyspace" registry.
//!
//! The actual background processing loop is implemented in Phase 2/3 of the
//! design and is added incrementally to keep modules easy to reason about.

use super::trigger_queue::{encode_trigger_queue_key, TriggerEvent, TriggerOp};
use super::executor::Executor;
use super::parse_sql;
use crate::types::{Row, TriggerDef};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::storage::TikvStore;
use crate::types::TableSchema;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tikv_client::BoundRange;
use tikv_client::Transaction;
use tracing::{info, warn};

const DEFAULT_MAX_QUEUE_DEPTH: usize = 10_000;
const DEFAULT_MAX_EVENTS_PER_BATCH: usize = 10;
const DEFAULT_MAX_RETRIES: u8 = 3;

const DEFAULT_POLL_INTERVAL_MS: u64 = 100;
const DEFAULT_GC_INTERVAL_SEC: u64 = 60;
const DEFAULT_DONE_RETENTION_SEC: u64 = 3600;
const DEFAULT_DLQ_RETENTION_DAYS: u64 = 7;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;

#[derive(Debug, Clone)]
pub(crate) struct TriggerWorkerConfig {
    pub enabled: bool,
    pub poll_interval_ms: u64,
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
        Self {
            enabled: true,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
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

    fn dec_current_depth(&self) {
        let _ = self
            .current_depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

pub(crate) struct TriggerWorker {
    worker_id: String,
    active_keyspaces: Mutex<HashSet<String>>,
    quotas: Mutex<HashMap<String, Arc<KeyspaceQuota>>>,
    config: TriggerWorkerConfig,
    shutdown: AtomicBool,
    default_search_path: Vec<String>,
}

impl TriggerWorker {
    fn new() -> Self {
        Self {
            worker_id: format!("worker-{}", std::process::id()),
            active_keyspaces: Mutex::new(HashSet::new()),
            quotas: Mutex::new(HashMap::new()),
            config: TriggerWorkerConfig::from_env(),
            shutdown: AtomicBool::new(false),
            default_search_path: vec!["public".to_string()],
        }
    }

    pub(crate) fn config(&self) -> &TriggerWorkerConfig {
        &self.config
    }

    #[allow(dead_code)]
    pub(crate) fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub(crate) fn mark_active(&self, keyspace: &str) {
        let mut guard = self
            .active_keyspaces
            .lock()
            .expect("trigger worker active_keyspaces lock");
        if !guard.contains(keyspace) {
            guard.insert(keyspace.to_string());
        }
    }

    pub(crate) fn take_active_keyspaces_snapshot(&self) -> Vec<String> {
        let guard = self
            .active_keyspaces
            .lock()
            .expect("trigger worker active_keyspaces lock");
        guard.iter().cloned().collect()
    }

    pub(crate) fn remove_active(&self, keyspace: &str) {
        let mut guard = self
            .active_keyspaces
            .lock()
            .expect("trigger worker active_keyspaces lock");
        guard.remove(keyspace);
    }

    pub(crate) fn get_quota(&self, keyspace: &str) -> Arc<KeyspaceQuota> {
        let mut guard = self.quotas.lock().expect("trigger worker quotas lock");
        if let Some(existing) = guard.get(keyspace) {
            return existing.clone();
        }
        let quota = Arc::new(KeyspaceQuota::new(&self.config));
        guard.insert(keyspace.to_string(), quota.clone());
        quota
    }

    #[allow(dead_code)]
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub(crate) async fn run(&self, pool: Arc<TikvClientPool>) {
        let poll_interval = Duration::from_millis(self.config.poll_interval_ms);

        let gc_pool = pool.clone();
        let gc_handle = tokio::spawn(async move {
            trigger_worker().gc_loop(gc_pool).await;
        });

        let mut interval = tokio::time::interval(poll_interval);
        while !self.shutdown.load(Ordering::Relaxed) {
            interval.tick().await;

            let keyspaces = self.take_active_keyspaces_snapshot();
            if keyspaces.is_empty() {
                continue;
            }

            for keyspace in keyspaces {
                if let Err(e) = self.process_keyspace(&pool, &keyspace).await {
                    warn!("trigger worker error for {}: {}", keyspace, e);
                }
            }
        }

        gc_handle.abort();
    }

    async fn process_keyspace(&self, pool: &Arc<TikvClientPool>, keyspace: &str) -> Result<()> {
        let quota = self.get_quota(keyspace);
        let store = pool.get_client(Some(keyspace.to_string())).await?;

        // Claim a fair batch (bounded per keyspace).
        let mut txn = store.begin().await?;
        let events = self
            .claim_events(&mut txn, quota.max_events_per_batch)
            .await?;
        if events.is_empty() {
            // Nothing pending; stop polling this keyspace until new enqueue.
            self.remove_active(keyspace);
            return Ok(());
        }
        txn.commit().await?;

        let exec = Executor::new(
            store.clone(),
            keyspace.to_string(),
            observability::registry().tenant(keyspace),
        );

        for event in events {
            let result = self.execute_event(&exec, &store, keyspace, &event).await;
            self.update_event_status(&store, keyspace, &event, result)
                .await?;
        }

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

/// Enqueue AFTER-row trigger events in the same transaction as the DML.
///
/// Queue backpressure is best-effort: when the in-memory depth estimate reaches
/// the per-keyspace limit, enqueue is skipped to avoid blocking DML.
pub(crate) async fn enqueue_after_triggers(
    txn: &mut Transaction,
    db_id: u64,
    keyspace: &str,
    table_full_name: &str,
    op: TriggerOp,
    old_row: Option<&Row>,
    new_row: Option<&Row>,
    triggers: &[TriggerDef],
) -> Result<()> {
    let worker = trigger_worker();
    if !worker.config().enabled {
        return Ok(());
    }

    let quota = worker.get_quota(keyspace);
    let mut remaining = quota
        .max_queue_depth
        .saturating_sub(quota.current_depth.load(Ordering::Relaxed));
    if remaining == 0 {
        return Ok(());
    }

    let op_str = match op {
        TriggerOp::Insert => "INSERT",
        TriggerOp::Update => "UPDATE",
        TriggerOp::Delete => "DELETE",
    };

    let mut queued_any = false;
    for trigger in triggers.iter().filter(|t| {
        t.timing.eq_ignore_ascii_case("AFTER")
            && t.events.iter().any(|e| e.eq_ignore_ascii_case(op_str))
    }) {
        if remaining == 0 {
            break;
        }

        let ev = TriggerEvent::new_pending(
            trigger.name.clone(),
            db_id,
            table_full_name.to_string(),
            op.clone(),
            old_row.cloned(),
            new_row.cloned(),
        );

        let key = encode_trigger_queue_key(ev.id);
        let val = bincode::serialize(&ev)?;
        crate::txn::txn_put(txn, key, val).await?;

        quota.current_depth.fetch_add(1, Ordering::Relaxed);
        remaining -= 1;
        queued_any = true;
    }

    if queued_any {
        worker.mark_active(keyspace);
    }

    Ok(())
}

impl TriggerWorker {
    async fn claim_events(&self, txn: &mut Transaction, limit: usize) -> Result<Vec<TriggerEvent>> {
        use super::trigger_queue::{encode_trigger_queue_prefix, now_ms_i64, EventStatus};
        use std::ops::Bound;

        let prefix = encode_trigger_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);

        let scan_limit: u32 = u32::try_from(limit.saturating_mul(4).max(16)).unwrap_or(u32::MAX);
        let mut candidate_keys: Vec<Vec<u8>> = Vec::new();

        let mut start: Option<Vec<u8>> = None;
        loop {
            if candidate_keys.len() >= limit {
                break;
            }

            let range: BoundRange = match start.as_ref() {
                None => (prefix.clone()..end.clone()).into(),
                Some(last) => BoundRange::new(
                    Bound::Excluded(last.clone().into()),
                    Bound::Excluded(end.clone().into()),
                ),
            };

            let mut batch_last = None;
            let mut scanned = 0usize;
            for pair in txn.scan(range, scan_limit).await? {
                scanned += 1;

                if candidate_keys.len() >= limit {
                    break;
                }

                let key_slice: &[u8] = pair.key().as_ref().into();
                let key_bytes = key_slice.to_vec();
                batch_last = Some(key_bytes.clone());

                let ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if ev.status == EventStatus::Pending {
                    candidate_keys.push(key_bytes);
                }
            }

            if scanned < scan_limit as usize {
                break;
            }

            start = batch_last;
            if start.is_none() {
                break;
            }
        }

        if candidate_keys.is_empty() {
            return Ok(Vec::new());
        }

        txn.lock_keys(candidate_keys.clone()).await?;

        let mut claimed = Vec::with_capacity(limit);
        let now_ms = now_ms_i64();
        for key in candidate_keys {
            let Some(val) = txn.get(key.clone()).await? else {
                continue;
            };
            let mut ev: TriggerEvent = bincode::deserialize(&val)?;
            if ev.status != EventStatus::Pending {
                continue;
            }
            ev.status = EventStatus::Processing;
            ev.worker_id = Some(self.worker_id.clone());
            ev.claimed_at_ms = Some(now_ms);

            txn.put(key, bincode::serialize(&ev)?).await?;
            claimed.push(ev);
            if claimed.len() >= limit {
                break;
            }
        }

        Ok(claimed)
    }

    async fn execute_event(
        &self,
        executor: &Executor,
        store: &Arc<TikvStore>,
        keyspace: &str,
        event: &TriggerEvent,
    ) -> Result<()> {
        use super::trigger_queue::EventStatus;

        // NOTE: This executes in its own transaction: eventual consistency by design.
        let mut txn = store.begin().await?;
        let db_id = event.db_id;

        let schema = store
            .get_schema(&mut txn, db_id, &event.table_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Table '{}' not found", event.table_name))?;

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
        self.execute_trigger_body(
            executor,
            &mut txn,
            db_id,
            &mut sequence_values,
            &schema,
            &func.body,
            event.old_row.as_ref(),
            event.new_row.as_ref(),
        )
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
        use super::trigger_queue::{encode_trigger_dlq_key, EventStatus};

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

    async fn gc_loop(&self, pool: Arc<TikvClientPool>) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.config.gc_interval_sec));
        while !self.shutdown.load(Ordering::Relaxed) {
            interval.tick().await;

            let keyspaces = self.list_known_keyspaces();
            for keyspace in keyspaces {
                if let Err(e) = self.gc_keyspace(&pool, &keyspace).await {
                    warn!("trigger GC error for {}: {}", keyspace, e);
                }
            }
        }
    }

    fn list_known_keyspaces(&self) -> Vec<String> {
        let mut out = HashSet::new();
        {
            let guard = self
                .active_keyspaces
                .lock()
                .expect("trigger worker active_keyspaces lock");
            out.extend(guard.iter().cloned());
        }
        {
            let guard = self.quotas.lock().expect("trigger worker quotas lock");
            out.extend(guard.keys().cloned());
        }
        out.into_iter().collect()
    }

    async fn gc_keyspace(&self, pool: &Arc<TikvClientPool>, keyspace: &str) -> Result<()> {
        use super::trigger_queue::now_ms_i64;

        let store = pool.get_client(Some(keyspace.to_string())).await?;
        let quota = self.get_quota(keyspace);
        let cutoff_orphan_ms = now_ms_i64()
            .saturating_sub((self.config.orphan_timeout_sec.saturating_mul(1000)) as i64);
        let cutoff_dlq_ms = now_ms_i64().saturating_sub(
            (self.config.dlq_retention_days.saturating_mul(24 * 3600 * 1000)) as i64,
        );

        let mut txn = store.begin().await?;

        let recovered = self
            .recover_orphans(&mut txn, keyspace, cutoff_orphan_ms, &quota)
            .await?;
        let dlq_deleted = self.delete_old_dlq(&mut txn, cutoff_dlq_ms).await?;

        txn.commit().await?;

        if recovered > 0 || dlq_deleted > 0 {
            info!(
                "trigger GC for {}: recovered {}, deleted dlq {}",
                keyspace, recovered, dlq_deleted
            );
        }

        Ok(())
    }

    async fn recover_orphans(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        cutoff_claimed_ms: i64,
        quota: &Arc<KeyspaceQuota>,
    ) -> Result<usize> {
        use super::trigger_queue::{encode_trigger_dlq_key, encode_trigger_queue_prefix, EventStatus};
        use std::ops::Bound;

        let prefix = encode_trigger_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);

        let mut recovered = 0usize;
        let mut start: Option<Vec<u8>> = None;
        loop {
            let range: BoundRange = match start.as_ref() {
                None => (prefix.clone()..end.clone()).into(),
                Some(last) => BoundRange::new(
                    Bound::Excluded(last.clone().into()),
                    Bound::Excluded(end.clone().into()),
                ),
            };

            let mut batch_last = None;
            let mut scanned = 0usize;
            for pair in txn.scan(range, 256).await? {
                scanned += 1;
                let key_slice: &[u8] = pair.key().as_ref().into();
                let key = key_slice.to_vec();
                batch_last = Some(key.clone());
                let mut ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if ev.status != EventStatus::Processing {
                    continue;
                }
                let Some(claimed_at) = ev.claimed_at_ms else {
                    continue;
                };
                if claimed_at >= cutoff_claimed_ms {
                    continue;
                }

                ev.retry_count = ev.retry_count.saturating_add(1);
                if ev.retry_count >= quota.max_retries {
                    ev.status = EventStatus::Failed;
                    ev.worker_id = None;
                    ev.claimed_at_ms = None;
                    let dlq_key = encode_trigger_dlq_key(ev.id);
                    txn.put(dlq_key, bincode::serialize(&ev)?).await?;
                    txn.delete(key).await?;
                    quota.dec_current_depth();
                } else {
                    ev.status = EventStatus::Pending;
                    ev.worker_id = None;
                    ev.claimed_at_ms = None;
                    txn.put(key, bincode::serialize(&ev)?).await?;
                    self.mark_active(keyspace);
                }
                recovered += 1;
            }

            if scanned < 256 {
                break;
            }
            start = batch_last;
            if start.is_none() {
                break;
            }
        }

        Ok(recovered)
    }

    async fn delete_old_dlq(&self, txn: &mut Transaction, cutoff_ms: i64) -> Result<usize> {
        use super::trigger_queue::encode_trigger_dlq_prefix;
        use std::ops::Bound;

        let cutoff_ms_u64 = u64::try_from(cutoff_ms.max(0)).unwrap_or(0);
        let cutoff_id = cutoff_ms_u64 << 22;

        let prefix = encode_trigger_dlq_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);

        let mut deleted = 0usize;
        let mut start: Option<Vec<u8>> = None;
        loop {
            let range: BoundRange = match start.as_ref() {
                None => (prefix.clone()..end.clone()).into(),
                Some(last) => BoundRange::new(
                    Bound::Excluded(last.clone().into()),
                    Bound::Excluded(end.clone().into()),
                ),
            };

            let mut batch_last = None;
            let mut scanned = 0usize;
            for pair in txn.scan(range, 256).await? {
                scanned += 1;
                let key_slice: &[u8] = pair.key().as_ref().into();
                let key_bytes = key_slice.to_vec();
                batch_last = Some(key_bytes.clone());

                if key_bytes.len() < 8 {
                    continue;
                }
                let id_bytes = &key_bytes[key_bytes.len() - 8..];
                let id = u64::from_be_bytes(id_bytes.try_into().unwrap());
                if id < cutoff_id {
                    txn.delete(key_bytes).await?;
                    deleted += 1;
                }
            }

            if scanned < 256 {
                break;
            }
            start = batch_last;
            if start.is_none() {
                break;
            }
        }

        Ok(deleted)
    }

    async fn execute_trigger_body(
        &self,
        executor: &Executor,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        schema: &TableSchema,
        body: &str,
        old_row: Option<&Row>,
        new_row: Option<&Row>,
    ) -> Result<()> {
        // This is intentionally a small subset of PL/pgSQL tailored for triggers:
        // - `NEW.col := <expr>` assignments
        // - `RETURN <...>` terminators
        // - Everything else is treated as a SQL statement and executed.
        //
        // It matches the existing BEFORE-trigger executor in `triggers.rs`, but adds
        // SQL statement execution for AFTER triggers.

        let mut new_values = match new_row {
            Some(r) => r.values.clone(),
            None => vec![crate::types::Value::Null; schema.columns.len()],
        };
        if new_values.len() < schema.columns.len() {
            new_values.resize(schema.columns.len(), crate::types::Value::Null);
        }

        let Some((begin_pos, end_pos)) = plpgsql_outer_block_range(body) else {
            return Ok(());
        };
        let block = &body[begin_pos..end_pos];

        let mut stmt_buf = String::new();
        for raw_line in block.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with("--") {
                continue;
            }

            if !stmt_buf.is_empty() {
                stmt_buf.push(' ');
            }
            stmt_buf.push_str(line);

            if !line.ends_with(';') {
                continue;
            }

            let should_stop = {
                let stmt = stmt_buf.trim().trim_end_matches(';').trim();
                if stmt.is_empty() {
                    false
                } else {
                    self.execute_trigger_statement(
                        executor,
                        txn,
                        db_id,
                        sequence_values,
                        schema,
                        old_row,
                        &mut new_values,
                        stmt,
                    )
                    .await?
                }
            };
            stmt_buf.clear();
            if should_stop {
                return Ok(());
            }
        }

        if !stmt_buf.trim().is_empty() {
            let stmt = stmt_buf.trim();
            let _ = self
                .execute_trigger_statement(
                    executor,
                    txn,
                    db_id,
                    sequence_values,
                    schema,
                    old_row,
                    &mut new_values,
                    stmt,
                )
                .await?;
        }

        Ok(())
    }

    async fn execute_trigger_statement(
        &self,
        executor: &Executor,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        schema: &TableSchema,
        old_row: Option<&Row>,
        new_values: &mut [crate::types::Value],
        stmt: &str,
    ) -> Result<bool> {
        let stmt = stmt.trim().trim_end_matches(';').trim();
        let upper = stmt.to_uppercase();

        // PL/pgSQL no-op statement.
        if upper == "NULL" {
            return Ok(false);
        }
        if upper.starts_with("RETURN NEW") || upper.starts_with("RETURN OLD") || upper == "RETURN"
        {
            return Ok(true);
        }
        if upper.starts_with("RETURN NULL") {
            return Ok(true);
        }

        if upper.starts_with("NEW.") {
            if let Some((col, expr)) = parse_new_assignment(stmt) {
                if let Some(idx) = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col))
                {
                    let resolved_expr =
                        substitute_row_references(expr, schema, new_values, old_row);
                    let sql = format!("SELECT {}", resolved_expr);
                    if let Ok(stmts) = super::parse_sql(&sql) {
                        if let Some(sqlparser::ast::Statement::Query(query)) =
                            stmts.into_iter().next()
                        {
                            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                                    select.projection.into_iter().next()
                                {
                                    let store = executor.store();
                                    let value = if super::sequences::expr_needs_async_eval(&expr) {
                                        super::sequences::eval_expr_with_sequences(
                                            &store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            &self.default_search_path,
                                            &expr,
                                            None,
                                            None,
                                        )
                                        .await?
                                    } else {
                                        super::expr::eval_expr(&expr, None, None)?
                                    };

                                    let coerced = super::helpers::coerce_value_for_column(
                                        value,
                                        &schema.columns[idx],
                                    )?;
                                    new_values[idx] = coerced;
                                }
                            }
                        }
                    }
                }
            }
            return Ok(false);
        }

        // Treat as SQL statement.
        let substituted = substitute_row_references(stmt, schema, new_values, old_row);
        let statements = parse_sql(&substituted)?;
        for s in &statements {
            // Ignore result rows; errors propagate.
            let _ = executor
                .execute_statement_on_txn(
                    txn,
                    db_id,
                    sequence_values,
                    &self.default_search_path,
                    s,
                )
                .await?;
        }

        Ok(false)
    }
}

fn plpgsql_outer_block_range(body: &str) -> Option<(usize, usize)> {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum BlockKind {
        Begin,
        Case,
    }

    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<BlockKind> = Vec::new();
    let mut block_start: Option<usize> = None;

    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < bytes.len() {
        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single_quote {
            if bytes[i] == b'\'' {
                // SQL escapes single quotes by doubling them.
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_quote = false;
            }
            i += 1;
            continue;
        }
        if in_double_quote {
            if bytes[i] == b'"' {
                // SQL escapes double quotes by doubling them.
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_double_quote = false;
            }
            i += 1;
            continue;
        }

        // Enter comments/strings.
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'\'' {
            in_single_quote = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'"' {
            in_double_quote = true;
            i += 1;
            continue;
        }

        // Skip dollar-quoted strings ($$...$$ or $tag$...$tag$).
        if bytes[i] == b'$' {
            if let Some(tag_end) = bytes[i + 1..].iter().position(|b| *b == b'$') {
                let tag_end = i + 1 + tag_end;
                let tag = &body[i + 1..tag_end];
                if tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    let delim = &body[i..=tag_end];
                    if let Some(close_pos) = body[tag_end + 1..].find(delim) {
                        i = tag_end + 1 + close_pos + delim.len();
                        continue;
                    }
                }
            }
        }

        // Scan identifier-like tokens so `end2` doesn't match `END`.
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let b = bytes[i];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                    i += 1;
                } else {
                    break;
                }
            }
            let token = &body[start..i];
            if token.eq_ignore_ascii_case("BEGIN") {
                if stack.is_empty() {
                    block_start = Some(i);
                }
                stack.push(BlockKind::Begin);
                continue;
            }
            if token.eq_ignore_ascii_case("CASE") {
                if !stack.is_empty() {
                    stack.push(BlockKind::Case);
                }
                continue;
            }
            if token.eq_ignore_ascii_case("END") && !stack.is_empty() {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }

                // Handle `END <kw>;` forms like `END IF;`, `END LOOP;`, `END CASE;`.
                let mut next_token_end = j;
                let mut next_token: Option<&str> = None;
                if next_token_end < bytes.len()
                    && (bytes[next_token_end].is_ascii_alphabetic() || bytes[next_token_end] == b'_')
                {
                    let next_start = next_token_end;
                    next_token_end += 1;
                    while next_token_end < bytes.len() {
                        let b = bytes[next_token_end];
                        if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                            next_token_end += 1;
                        } else {
                            break;
                        }
                    }
                    next_token = Some(&body[next_start..next_token_end]);
                }

                if let Some(next) = next_token {
                    if next.eq_ignore_ascii_case("IF") || next.eq_ignore_ascii_case("LOOP") {
                        continue;
                    }
                    if next.eq_ignore_ascii_case("CASE") {
                        if matches!(stack.last(), Some(BlockKind::Case)) {
                            stack.pop();
                        }
                        // Skip the `CASE` token so it won't be treated as a new `CASE`.
                        i = next_token_end;
                        continue;
                    }
                }

                // `END` closes an innermost SQL `CASE ... END` expression even when it is followed
                // by `;` (e.g. `NEW.col := CASE ... END;`).
                if matches!(stack.last(), Some(BlockKind::Case)) {
                    stack.pop();
                    continue;
                }

                // `END;` (or `END <label>;`) closes a `BEGIN ... END` block.
                if matches!(stack.last(), Some(BlockKind::Begin)) {
                    if j < bytes.len() && bytes[j] == b';' {
                        stack.pop();
                        if stack.is_empty() {
                            return Some((block_start.unwrap_or(i), start));
                        }
                        continue;
                    }

                    if next_token.is_some() {
                        let mut k = next_token_end;
                        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < bytes.len() && bytes[k] == b';' {
                            stack.pop();
                            if stack.is_empty() {
                                return Some((block_start.unwrap_or(i), start));
                            }
                            continue;
                        }
                    }
                }
                continue;
            }

            continue;
        }

        i += 1;
    }

    block_start.map(|start| (start, body.len()))
}

fn parse_new_assignment(stmt: &str) -> Option<(&str, &str)> {
    // Accept both `:=` and `=` (common in test cases).
    let s = stmt.trim().trim_end_matches(';').trim();
    let rest = s.strip_prefix("NEW.").or_else(|| s.strip_prefix("new."))?;

    if let Some(pos) = rest.find(":=") {
        let col = rest[..pos].trim();
        let expr = rest[pos + 2..].trim();
        return Some((col, expr));
    }
    if let Some(pos) = rest.find('=') {
        // Skip `==`, `<=`, etc.
        let before = rest.as_bytes().get(pos.wrapping_sub(1)).copied();
        let after = rest.as_bytes().get(pos + 1).copied();
        if before == Some(b':')
            || before == Some(b'<')
            || before == Some(b'>')
            || before == Some(b'!')
            || after == Some(b'=')
        {
            return None;
        }
        let col = rest[..pos].trim();
        let expr = rest[pos + 1..].trim();
        return Some((col, expr));
    }
    None
}

fn substitute_row_references(
    expr: &str,
    schema: &TableSchema,
    new_values: &[crate::types::Value],
    old_row: Option<&Row>,
) -> String {
    let mut result = expr.to_string();

    for (idx, col) in schema.columns.iter().enumerate() {
        let patterns = [
            format!("NEW.{}", col.name.to_uppercase()),
            format!("new.{}", col.name.to_lowercase()),
            format!("NEW.{}", col.name),
            format!("new.{}", col.name),
        ];
        let value_str = new_values
            .get(idx)
            .map(value_to_sql_literal)
            .unwrap_or_else(|| "NULL".to_string());

        for pattern in &patterns {
            if result.to_uppercase().contains(&pattern.to_uppercase()) {
                result = case_insensitive_replace(&result, pattern, &value_str);
            }
        }
    }

    if let Some(old) = old_row {
        for (idx, col) in schema.columns.iter().enumerate() {
            let patterns = [
                format!("OLD.{}", col.name.to_uppercase()),
                format!("old.{}", col.name.to_lowercase()),
                format!("OLD.{}", col.name),
                format!("old.{}", col.name),
            ];
            let value_str = old
                .values
                .get(idx)
                .map(value_to_sql_literal)
                .unwrap_or_else(|| "NULL".to_string());

            for pattern in &patterns {
                if result.to_uppercase().contains(&pattern.to_uppercase()) {
                    result = case_insensitive_replace(&result, pattern, &value_str);
                }
            }
        }
    }

    result
}

fn case_insensitive_replace(s: &str, pattern: &str, replacement: &str) -> String {
    if pattern.is_empty() {
        return s.to_string();
    }

    // Replace only when `pattern` is not immediately followed by an identifier
    // continuation (avoid prefix matches like `NEW.id` inside `NEW.id2`).
    let re_pattern = format!("{}($|[^\\p{{XID_Continue}}$])", regex::escape(pattern));
    let Ok(re) = regex::RegexBuilder::new(&re_pattern)
        .case_insensitive(true)
        .build()
    else {
        return s.to_string();
    };

    re.replace_all(s, |caps: &regex::Captures| {
        let delim = caps.get(1).map_or("", |m| m.as_str());
        let mut out = String::with_capacity(replacement.len() + delim.len());
        out.push_str(replacement);
        out.push_str(delim);
        out
    })
    .into_owned()
}

fn value_to_sql_literal(value: &crate::types::Value) -> String {
    match value {
        crate::types::Value::Null => "NULL".to_string(),
        crate::types::Value::Boolean(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        crate::types::Value::Int32(n) => n.to_string(),
        crate::types::Value::Int64(n) => n.to_string(),
        crate::types::Value::Float64(f) => f.to_string(),
        crate::types::Value::Numeric(d) => d.to_string(),
        crate::types::Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        crate::types::Value::Timestamp(ts) => {
            let secs = ts / 1000;
            let millis = ts % 1000;
            let datetime = chrono::DateTime::from_timestamp(secs, (millis * 1_000_000) as u32)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
            format!("'{}'", datetime.format("%Y-%m-%d %H:%M:%S%.3f"))
        }
        crate::types::Value::Date(days) => match crate::types::date::format_date_days(*days) {
            Ok(s) => format!("'{}'", s),
            Err(_) => format!("'{}'", days),
        },
        crate::types::Value::Uuid(bytes) => {
            let u = uuid::Uuid::from_bytes(*bytes);
            format!("'{}'", u)
        }
        crate::types::Value::Bytes(b) => format!("'\\x{}'", hex::encode(b)),
        crate::types::Value::Json(s) | crate::types::Value::Jsonb(s) => {
            format!("'{}'", s.replace('\'', "''"))
        }
        crate::types::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(value_to_sql_literal).collect();
            format!("ARRAY[{}]", items.join(","))
        }
        crate::types::Value::Vector(v) => {
            let items: Vec<String> = v.iter().map(|f| f.to_string()).collect();
            format!("[{}]", items.join(","))
        }
        crate::types::Value::Interval(iv) => format!("INTERVAL '{}'", iv),
        crate::types::Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            format!("'{:02}:{:02}:{:02}'", hours, mins, secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        case_insensitive_replace, parse_new_assignment, plpgsql_outer_block_range,
        substitute_row_references, value_to_sql_literal,
    };

    #[test]
    fn parse_new_assignment_accepts_common_syntax() {
        assert_eq!(
            parse_new_assignment("NEW.updated_at := NOW();"),
            Some(("updated_at", "NOW()"))
        );
        assert_eq!(
            parse_new_assignment("new.updated_at = NOW();"),
            Some(("updated_at", "NOW()"))
        );
    }

    #[test]
    fn parse_new_assignment_rejects_comparisons() {
        assert_eq!(parse_new_assignment("NEW.a != 1"), None);
        assert_eq!(parse_new_assignment("NEW.a <= 1"), None);
        assert_eq!(parse_new_assignment("NEW.a >= 1"), None);
        assert_eq!(parse_new_assignment("NEW.a == 1"), None);
    }

    #[test]
    fn value_to_sql_literal_bytes_uses_single_backslash_x_prefix() {
        let value = Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(value_to_sql_literal(&value), "'\\xdeadbeef'");
    }

    #[test]
    fn case_insensitive_replace_unicode_offsets() {
        let s = "ıNEW.col";
        let result = case_insensitive_replace(s, "new.COL", "X");
        assert_eq!(result, "ıX");
    }

    #[test]
    fn case_insensitive_replace_does_not_expand_replacement() {
        let s = "NEW.col";
        let result = case_insensitive_replace(s, "new.COL", "$1");
        assert_eq!(result, "$1");
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

    #[test]
    fn substitute_row_references_does_not_prefix_match() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "id2".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let new_values = vec![Value::Int32(7), Value::Int32(3)];
        let old_row = Row::new(vec![Value::Int32(9), Value::Int32(11)]);

        let result = substitute_row_references("NEW.id2 + 1", &schema, &new_values, None);
        assert_eq!(result, "3 + 1");

        let result = substitute_row_references("NEW.id + NEW.id2", &schema, &new_values, None);
        assert_eq!(result, "7 + 3");

        let result = substitute_row_references(
            "OLD.id2 + OLD.id",
            &schema,
            &new_values,
            Some(&old_row),
        );
        assert_eq!(result, "11 + 9");
    }

    #[test]
    fn substitute_row_references_handles_schema_growth() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "id2".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "added".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        // Simulate an async trigger event queued before `ALTER TABLE .. ADD COLUMN`.
        let new_values = vec![Value::Int32(7), Value::Int32(3)];
        let old_row = Row::new(vec![Value::Int32(9), Value::Int32(11)]);

        let result = substitute_row_references("NEW.id + NEW.id2", &schema, &new_values, None);
        assert_eq!(result, "7 + 3");

        let result = substitute_row_references("NEW.added IS NULL", &schema, &new_values, None);
        assert_eq!(result, "NULL IS NULL");

        let result = substitute_row_references(
            "OLD.added IS NULL",
            &schema,
            &new_values,
            Some(&old_row),
        );
        assert_eq!(result, "NULL IS NULL");
    }

    #[test]
    fn plpgsql_outer_block_range_stops_at_end_semicolon() {
        let body = "DECLARE x INT;\nBEGIN\n  SELECT 1;\nEND;\n-- end\nSELECT 2;";
        let (start, end) = plpgsql_outer_block_range(body).expect("expected BEGIN..END;");
        let block = &body[start..end];
        assert!(block.contains("SELECT 1;"));
        assert!(!block.contains("END;"));
        assert!(!block.contains("-- end"));
        assert!(!block.contains("SELECT 2"));
    }

    #[test]
    fn plpgsql_outer_block_range_handles_nested_blocks() {
        let body = "BEGIN\n  BEGIN\n    SELECT 1;\n  END;\n  SELECT 2;\nEND;\n-- end";
        let (start, end) = plpgsql_outer_block_range(body).expect("expected BEGIN..END;");
        let block = &body[start..end];
        assert!(block.contains("BEGIN"));
        assert!(block.contains("END;"));
        assert!(block.contains("SELECT 2;"));
        assert!(!block.contains("-- end"));
    }

    #[test]
    fn plpgsql_outer_block_range_does_not_stop_at_case_end_semicolon() {
        let body = "BEGIN\n  NEW.col := CASE WHEN 1=1 THEN 2 ELSE 3 END;\n  SELECT 2;\nEND;\n-- end";
        let (start, end) = plpgsql_outer_block_range(body).expect("expected BEGIN..END;");
        let block = &body[start..end];
        assert!(block.contains("NEW.col := CASE"));
        assert!(block.contains("END;"));
        assert!(block.contains("SELECT 2;"));
        assert!(!block.contains("-- end"));
    }
}
