//! SQL executor core

mod alter;
mod analyze;
mod analyze_rewrite;
pub(crate) mod catalog_prefetch;
mod copy;
mod dispatch;
mod guc;
mod misc;
mod observability;
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
    parse_search_path_guc_value, set_variable_value_to_string, try_parse_const_bool,
    try_parse_const_text,
};
use misc::{
    get_skip_reason, get_unsupported_reason, split_sql_statements, starts_with_ignore_ascii_case,
};
use observability::{
    is_observability_system_query, is_observability_tableless_query, OBSERVABILITY_USER,
};
use retry::is_retryable_tikv_error;
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
use super::super::{parse_sql, ExecuteResult, ExecuteResults, InFailedSqlTransaction, Session};
use super::triggers::strip_leading_sql_comments;
use crate::auth::AuthManager;
use crate::observability::TenantObservability;
use crate::session_context;
use crate::sql::error::SqlError;
use crate::sql::optimizer::statistics::TableStatistics;
use crate::storage::{with_kv_read_stats, KvReadStatsSnapshot, TikvStore};
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterIndexOperation, Expr, FunctionArg, FunctionArgExpr, Query, SelectItem, SetExpr, Statement,
    TableFactor, TransactionAccessMode, TransactionIsolationLevel, TransactionMode, Visit, Visitor,
};

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
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

pub struct Executor {
    store: Arc<TikvStore>,
    auth_manager: AuthManager,
    tenant_keyspace: String,
    observability: Arc<TenantObservability>,
    trigger_cache: Arc<TriggerBodyCache>,
    stats_cache: Arc<TableStatsCache>,
    /// Keyspaces whose trigger workers need activation after the current
    /// transaction commits.  Accumulated during DML execution (inside the
    /// transaction) and flushed only on successful commit so that the trigger
    /// worker never sees uncommitted events.
    pending_trigger_activations: Mutex<HashSet<String>>,
    pending_async_triggers: Mutex<Vec<PendingAsyncTrigger>>,
}

impl Executor {
    pub fn new(
        store: Arc<TikvStore>,
        tenant_keyspace: String,
        observability: Arc<TenantObservability>,
        trigger_cache: Arc<TriggerBodyCache>,
        stats_cache: Arc<TableStatsCache>,
    ) -> Self {
        Self {
            store,
            auth_manager: AuthManager::new(),
            tenant_keyspace,
            observability,
            trigger_cache,
            stats_cache,
            pending_trigger_activations: Mutex::new(HashSet::new()),
            pending_async_triggers: Mutex::new(Vec::new()),
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
        self.pending_async_triggers.lock().unwrap().push(trigger);
    }

    pub(crate) fn flush_trigger_activations(&self) {
        let triggers: Vec<PendingAsyncTrigger> = self
            .pending_async_triggers
            .lock()
            .unwrap()
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
            }
        }
        self.pending_trigger_activations.lock().unwrap().clear();
    }

    /// Discard pending activations without notifying trigger workers.
    /// Called after rollback so that events that were never committed
    /// don't cause unnecessary worker wake-ups.
    pub(crate) fn clear_trigger_activations(&self) {
        self.pending_trigger_activations.lock().unwrap().clear();
        self.pending_async_triggers.lock().unwrap().clear();
    }
}

/// Boxed future type for statement execution.
///
/// All entry points into the statement dispatch return this type to prevent
/// callers from embedding the large dispatch state machine into their own
/// async state machines.
pub(crate) type BoxStmtFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ExecuteResult>> + Send + 'a>>;
