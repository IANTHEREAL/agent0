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
            table_full_name.to_string(),
            op.clone(),
            old_row.cloned(),
            new_row.cloned(),
        );

        let key = encode_trigger_queue_key(ev.id);
        let val = bincode::serialize(&ev)?;
        txn.put(key, val).await?;

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

        let prefix = encode_trigger_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();

        let scan_limit: u32 = u32::try_from(limit.saturating_mul(4).max(16)).unwrap_or(u32::MAX);
        let mut candidate_keys: Vec<Vec<u8>> = Vec::new();
        for pair in txn.scan(range, scan_limit).await? {
            if candidate_keys.len() >= limit {
                break;
            }
            let ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if ev.status == EventStatus::Pending {
                let key_bytes: &[u8] = pair.key().as_ref().into();
                candidate_keys.push(key_bytes.to_vec());
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

        let schema = store
            .get_schema(&mut txn, &event.table_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Table '{}' not found", event.table_name))?;

        let trigger = store
            .get_trigger(&mut txn, &event.table_name, &event.trigger_name)
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
            .get_function(&mut txn, &trigger.function)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Function '{}' not found", trigger.function))?;

        let mut sequence_values = HashMap::new();
        let start = Instant::now();
        self.execute_trigger_body(
            executor,
            &mut txn,
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
                quota.current_depth.fetch_sub(1, Ordering::Relaxed);
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
                    quota.current_depth.fetch_sub(1, Ordering::Relaxed);
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
                    quota.current_depth.fetch_sub(1, Ordering::Relaxed);
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

        let body_upper = body.to_uppercase();
        let begin_pos = match body_upper.find("BEGIN") {
            Some(p) => p + 5,
            None => return Ok(()),
        };
        let end_pos = body_upper.rfind("END").unwrap_or(body.len());
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
        sequence_values: &mut HashMap<String, i64>,
        schema: &TableSchema,
        old_row: Option<&Row>,
        new_values: &mut [crate::types::Value],
        stmt: &str,
    ) -> Result<bool> {
        let upper = stmt.to_uppercase();
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
                .execute_statement_on_txn(txn, sequence_values, &self.default_search_path, s)
                .await?;
        }

        // Some trigger bodies end with a bare "NULL;" statement, treat it as no-op.
        if upper == "NULL" || matches!(stmt.trim(), "NULL" | "null") {
            return Ok(false);
        }

        Ok(false)
    }
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
        let value_str = value_to_sql_literal(&new_values[idx]);

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
            let value_str = value_to_sql_literal(&old.values[idx]);

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
    let s_upper = s.to_uppercase();
    let pattern_upper = pattern.to_uppercase();

    let mut result = String::new();
    let mut last_end = 0;

    for (start, _) in s_upper.match_indices(&pattern_upper) {
        result.push_str(&s[last_end..start]);
        result.push_str(replacement);
        last_end = start + pattern.len();
    }
    result.push_str(&s[last_end..]);

    result
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
        crate::types::Value::Bytes(b) => format!("'\\\\x{}'", hex::encode(b)),
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
    use super::parse_new_assignment;

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
}
