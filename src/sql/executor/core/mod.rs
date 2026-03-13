//! SQL executor core

mod alter;
mod analyze;
mod analyze_rewrite;
pub(crate) mod catalog_prefetch;
mod copy;
mod dispatch;
mod guc;
mod guc_engine;
mod misc;
mod observability;
pub(crate) mod plan_cache;
pub(crate) mod prepared_analysis;
pub(crate) mod prepared_stmt;
mod query_exec;
mod retry;
mod scan;
mod settings_tableless;
mod statement;
mod stmt_ddl;
mod stmt_dml;
mod stmt_query;
mod stmt_rbac;
mod timeout;
pub(crate) mod view_rewrite;

#[cfg(test)]
mod tests;

use guc::{
    normalize_search_path_entries, parse_search_path_guc_value, set_variable_value_to_string,
    try_parse_const_bool, try_parse_const_text,
};
pub(crate) use guc_engine::{
    check_reserved_guc_reset, check_reserved_guc_reset_with_original, check_reserved_guc_write,
    is_server_reserved_guc, session_auth_different_user_error,
    session_auth_different_user_error_sync,
};
pub(crate) use misc::starts_with_ignore_ascii_case;
use misc::{get_skip_reason, get_unsupported_reason, split_sql_statements};
use observability::{
    is_observability_system_query, is_observability_tableless_query, OBSERVABILITY_USER,
};
use retry::{autocommit_backoff, extract_write_conflict_reason, is_retryable_tikv_error};
#[cfg(test)]
use settings_tableless::{
    cast_current_setting_value, is_current_setting_function, is_set_config_function,
    unwrap_top_level_cast,
};
use settings_tableless::{try_execute_current_setting_select, try_execute_set_config_select};
use timeout::StatementTimeoutError;

use super::super::alter_owner;
use super::super::alter_sequence_owned_by;
use super::super::comment_on;
use super::super::ddl;
use super::super::dml;
use super::super::explain;
use super::super::names;
use super::super::names::normalize_ident;
use super::super::projection::fill_row_defaults;
use super::super::rbac;
use super::super::sequences;
use super::super::statement_time;
use super::super::stats::TableStatsCache;
use super::super::triggers::TriggerBodyCache;
use super::super::udt;
use super::super::value_coercion::parse_value_for_copy;
use super::super::{
    extract_create_index_with_params, parse_sql, ExecuteResult, ExecuteResults, Session,
};
use super::triggers::strip_leading_sql_comments;
use crate::auth::AuthManager;
use crate::model::{DataType, Row, TableSchema, Value};
use crate::observability::TenantObservability;
use crate::pool::TenantMemoryAccountant;
use crate::session_context;
use crate::sql::error::SqlError;
use crate::sql::optimizer::statistics::TableStatistics;
use crate::storage::{with_kv_read_stats, KvReadStatsSnapshot, TikvStore};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterIndexOperation, Expr, FunctionArg, FunctionArgExpr, Query, SelectItem, SetExpr, Statement,
    TableFactor, TransactionAccessMode, TransactionIsolationLevel, TransactionMode, Visit, Visitor,
};

use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tikv_client::Transaction;
use tracing::debug;

fn is_ephemeral_table_id(table_id: u64) -> bool {
    table_id == 0
}

#[derive(Debug, Clone)]
pub(crate) struct PendingAsyncTrigger {
    pub keyspace: String,
    pub db_id: u64,
    pub command: String,
}

/// Accumulated during DML, flushed after commit (same lifecycle as PendingAsyncTrigger).
#[derive(Debug, Clone)]
pub(crate) struct PendingHnswMerge {
    pub keyspace: String,
    pub db_id: u64,
    pub table_id: u64,
    pub index_id: u64,
}

pub struct Executor {
    store: Arc<TikvStore>,
    auth_manager: AuthManager,
    tenant_keyspace: String,
    observability: Arc<TenantObservability>,
    tenant_memory_accountant: TenantMemoryAccountant,
    trigger_cache: Arc<TriggerBodyCache>,
    stats_cache: Arc<TableStatsCache>,
    /// Keyspaces whose trigger workers need activation after the current
    /// transaction commits.  Accumulated during DML execution (inside the
    /// transaction) and flushed only on successful commit so that the trigger
    /// worker never sees uncommitted events.
    pending_trigger_activations: Mutex<HashSet<String>>,
    pending_async_triggers: Mutex<Vec<PendingAsyncTrigger>>,
    /// HNSW merge requests accumulated during DML, flushed after commit.
    pending_hnsw_merges: Mutex<Vec<PendingHnswMerge>>,
    /// Set when ALTER ROLE or DROP ROLE executes inside a transaction.
    /// Flushed after commit to call `invalidate_initialized` only once the
    /// role mutation is durable, avoiding a race where another session
    /// repopulates the stale cache before commit lands.
    pending_init_cache_invalidation: AtomicBool,
}

impl Executor {
    pub fn new(
        store: Arc<TikvStore>,
        tenant_keyspace: String,
        observability: Arc<TenantObservability>,
        tenant_memory_accountant: TenantMemoryAccountant,
        trigger_cache: Arc<TriggerBodyCache>,
        stats_cache: Arc<TableStatsCache>,
    ) -> Self {
        Self {
            store,
            auth_manager: AuthManager::new(),
            tenant_keyspace,
            observability,
            tenant_memory_accountant,
            trigger_cache,
            stats_cache,
            pending_trigger_activations: Mutex::new(HashSet::new()),
            pending_async_triggers: Mutex::new(Vec::new()),
            pending_hnsw_merges: Mutex::new(Vec::new()),
            pending_init_cache_invalidation: AtomicBool::new(false),
        }
    }

    pub fn store(&self) -> Arc<TikvStore> {
        self.store.clone()
    }

    pub fn tenant_keyspace(&self) -> &str {
        &self.tenant_keyspace
    }

    pub fn observability(&self) -> &Arc<TenantObservability> {
        &self.observability
    }

    pub fn tenant_memory_accountant(&self) -> &TenantMemoryAccountant {
        &self.tenant_memory_accountant
    }

    pub fn auth_manager(&self) -> &AuthManager {
        &self.auth_manager
    }

    pub fn trigger_cache(&self) -> &Arc<TriggerBodyCache> {
        &self.trigger_cache
    }

    pub fn stats_cache(&self) -> &Arc<TableStatsCache> {
        &self.stats_cache
    }

    /// Load full table statistics, checking the in-memory cache first and
    /// falling back to persisted statistics in TiKV.
    ///
    /// Returns `None` if no statistics exist (neither cached nor persisted).
    /// Skips TiKV lookup for `table_id == 0` (CTE references).
    pub(crate) async fn get_or_load_stats(
        &self,
        txn: &mut tikv_client::Transaction,
        db_id: u64,
        table_id: u64,
    ) -> anyhow::Result<Option<Arc<TableStatistics>>> {
        // Check in-memory cache first.
        if let Some(stats) = self.stats_cache.get_full_stats(db_id, table_id) {
            return Ok(Some(stats));
        }

        // CTE refs use table_id: 0 — no persisted stats to load.
        if is_ephemeral_table_id(table_id) {
            return Ok(None);
        }

        // Fall back to persisted statistics in TiKV.
        match self.store.load_statistics(txn, db_id, table_id).await? {
            Some(stats) => {
                let stats = Arc::new(stats);
                self.stats_cache
                    .update_full_stats(db_id, table_id, Arc::clone(&stats));
                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn push_pending_async_trigger(&self, trigger: PendingAsyncTrigger) {
        self.pending_async_triggers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(trigger);
    }

    pub(crate) fn push_pending_hnsw_merge(&self, merge: PendingHnswMerge) {
        self.pending_hnsw_merges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(merge);
    }

    pub(crate) fn flush_trigger_activations(&self) {
        let triggers: Vec<PendingAsyncTrigger> = self
            .pending_async_triggers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if !triggers.is_empty() {
            if let Some(system_store) = crate::worker::get_system_store() {
                let system_store = system_store.clone();
                tokio::spawn(async move {
                    let result = async {
                        let mut txn = system_store.begin().await?;
                        for (idx, trigger) in triggers.iter().enumerate() {
                            let now_ms = chrono::Utc::now().timestamp_millis();
                            let task_id = now_ms.saturating_add(idx as i64);
                            let entry = crate::worker::types::TaskQueueEntry::new(
                                trigger.keyspace.clone(),
                                trigger.db_id,
                                task_id,
                                crate::worker::types::TaskType::AsyncTrigger,
                                trigger.command.clone(),
                                "admin".to_string(),
                                200,
                            );
                            let fire_time = chrono::Utc::now().timestamp_millis();
                            system_store
                                .put_worker_queue_entry(&mut txn, &entry, fire_time)
                                .await?;
                            system_store
                                .update_registry_task_types(
                                    &mut txn,
                                    &trigger.keyspace,
                                    trigger.db_id,
                                    crate::worker::types::TASK_TYPE_ASYNC_TRIGGER,
                                    0,
                                )
                                .await?;
                        }
                        txn.commit().await?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(e) = result {
                        tracing::warn!("Failed to enqueue async triggers to worker: {}", e);
                    }
                    crate::worker::wake_worker();
                });
            } else {
                tracing::warn!(
                    "Dropping {} async trigger activation(s): worker subsystem is disabled. \
                     These triggers will not fire.",
                    triggers.len()
                );
            }
        }
        self.pending_trigger_activations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Flush accumulated HNSW merge requests after successful commit.
    /// Deduplicates by (table_id, index_id), enqueues a worker task via
    /// `put_worker_queue_entry` with a constant `fire_time_ms=0` so that
    /// repeated enqueues for the same index produce the same queue key
    /// (true idempotent overwrite). Best-effort: errors are logged and ignored.
    pub(crate) fn flush_pending_hnsw_merges(&self) {
        let merges: Vec<PendingHnswMerge> = self
            .pending_hnsw_merges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if merges.is_empty() {
            return;
        }

        // Deduplicate by (keyspace, db_id, table_id, index_id).
        let mut seen = std::collections::HashSet::new();
        let unique_merges: Vec<_> = merges
            .into_iter()
            .filter(|m| seen.insert((m.keyspace.clone(), m.db_id, m.table_id, m.index_id)))
            .collect();

        if let Some(system_store) = crate::worker::get_system_store() {
            let system_store = system_store.clone();
            tokio::spawn(async move {
                for merge in unique_merges {
                    let task_id = match crate::sql::hnsw::storage::hnsw_merge_task_id(
                        merge.table_id,
                        merge.index_id,
                    ) {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::debug!("HNSW merge task_id overflow: {}", e);
                            continue;
                        }
                    };
                    let result: Result<(), anyhow::Error> = async {
                        // Use fire_time_ms=0 so the queue key is deterministic for the
                        // same (priority, task_type, keyspace, db_id, task_id).  Repeated
                        // puts overwrite the same key — true idempotent dedup.
                        // fire_time=0 is always <= now, so the task is immediately "due".
                        let fire_time_ms = 0i64;
                        let mut entry = crate::worker::types::TaskQueueEntry::new(
                            merge.keyspace.clone(),
                            merge.db_id,
                            task_id,
                            crate::worker::types::TaskType::HnswMerge,
                            format!("__hnsw_merge {} {}", merge.table_id, merge.index_id),
                            "system".to_string(),
                            192, // Lower priority than BgDdl/AutoAnalyze=128
                        );
                        // Guarantee non-zero nonce so CAS delete can distinguish
                        // fresh entries from legacy entries with default nonce=0.
                        entry.nonce = rand::thread_rng().gen_range(1..=u64::MAX);
                        let mut txn = system_store.begin().await?;
                        system_store
                            .put_worker_queue_entry(&mut txn, &entry, fire_time_ms)
                            .await?;
                        system_store
                            .update_registry_task_types(
                                &mut txn,
                                &merge.keyspace,
                                merge.db_id,
                                crate::worker::types::TASK_TYPE_HNSW_MERGE,
                                0,
                            )
                            .await?;
                        txn.commit().await?;
                        crate::worker::wake_worker();
                        Ok(())
                    }
                    .await;
                    if let Err(e) = result {
                        tracing::debug!("HNSW merge enqueue failed (best-effort): {}", e);
                    }
                }
            });
        }
    }

    /// Mark that the `is_initialized` cache should be invalidated after the
    /// current transaction commits (ALTER ROLE / DROP ROLE may have removed
    /// the last superuser).
    pub(crate) fn mark_init_cache_invalidation_pending(&self) {
        self.pending_init_cache_invalidation
            .store(true, Ordering::Relaxed);
    }

    /// If a role mutation was executed in the committed transaction,
    /// invalidate the per-keyspace `is_initialized` cache so that
    /// `bootstrap()` can re-run if all superusers were removed.
    pub(crate) fn flush_pending_init_cache_invalidation(&self) {
        if self
            .pending_init_cache_invalidation
            .swap(false, Ordering::Relaxed)
        {
            let ks = self.store.keyspace().unwrap_or("");
            crate::auth::invalidate_initialized(ks);
        }
    }

    /// Discard pending activations without notifying trigger workers.
    /// Called after rollback so that events that were never committed
    /// don't cause unnecessary worker wake-ups.
    pub(crate) fn clear_trigger_activations(&self) {
        self.pending_trigger_activations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending_async_triggers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending_hnsw_merges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending_init_cache_invalidation
            .store(false, Ordering::Relaxed);
    }
}

/// Boxed future type for statement execution.
///
/// All entry points into the statement dispatch return this type to prevent
/// callers from embedding the large dispatch state machine into their own
/// async state machines.
pub(crate) type BoxStmtFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ExecuteResult>> + Send + 'a>>;
