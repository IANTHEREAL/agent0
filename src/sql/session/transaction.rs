//! Transaction management: begin/commit/rollback and savepoint operations.

use crate::sql::error::SqlError;
use crate::sql::query_context::XactAdvisoryLockRecord;
use anyhow::{anyhow, Result};
use std::sync::atomic::Ordering;
use tikv_client::TimestampExt;

use super::{Session, TransactionState};

impl Session {
    async fn reset_xact_advisory_savepoint_tracker(&self) {
        let mut tracker = self.xact_advisory_savepoint_tracker.lock().await;
        tracker.reset();
    }

    fn release_rolled_back_xact_advisory_locks(&self, locks: Vec<XactAdvisoryLockRecord>) {
        if locks.is_empty() {
            return;
        }
        let manager = crate::sql::advisory_locks::global_lock_manager();
        for lock in locks {
            let released =
                manager.release_xact(&lock.keyspace, lock.key, self.connection_id, lock.mode);
            debug_assert!(
                released,
                "expected rolled-back xact advisory lock to be held: conn_id={}, key={}",
                self.connection_id, lock.key
            );
        }
    }

    pub(super) fn release_xact_advisory_locks_if_needed(&self) {
        if self.has_xact_advisory_locks.swap(false, Ordering::AcqRel) {
            crate::sql::advisory_locks::global_lock_manager()
                .release_xact_locks(self.connection_id);
        }
    }

    pub async fn create_savepoint(&mut self, name: String) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }
        if self.is_transaction_failed() {
            return Err(SqlError::InFailedTransaction.into());
        }
        let name_for_settings = name.clone();
        let name_for_tracker = name.clone();
        let name_for_seq = name.clone();
        self.savepoints.create(name).await?;
        {
            let mut tracker = self.xact_advisory_savepoint_tracker.lock().await;
            tracker.create(name_for_tracker);
        }
        self.settings
            .push_settings_savepoint(name_for_settings.clone());
        self.push_extension_delta_savepoint(name_for_settings.clone());
        self.push_session_auth_savepoint(name_for_settings);
        self.last_sequence_values.push_savepoint(name_for_seq);
        Ok(())
    }

    pub async fn release_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "RELEASE SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }
        if self.is_transaction_failed() {
            return Err(SqlError::InFailedTransaction.into());
        }
        self.savepoints.release(name).await?;
        {
            let mut tracker = self.xact_advisory_savepoint_tracker.lock().await;
            tracker.release(name)?;
        }
        self.settings.release_settings_savepoint(name);
        self.release_extension_delta_savepoint(name);
        self.release_session_auth_savepoint(name);
        self.last_sequence_values.release_savepoint(name);
        Ok(())
    }

    pub async fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "ROLLBACK TO SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }

        let mut prepared = self.savepoints.prepare_rollback_to(name).await?;
        let rolled_back_xact_locks = {
            let mut tracker = self.xact_advisory_savepoint_tracker.lock().await;
            tracker.prepare_rollback_to(name)?
        };
        let res = {
            let txn = self.get_mut_txn().expect("transaction must be active");

            // Undo nested savepoints first, then the target savepoint itself.
            for sp in prepared.popped.iter_mut().rev() {
                let mut entries = Vec::with_capacity(sp.undo.len());
                entries.extend(sp.undo.drain());
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                for (key, prev) in entries {
                    match prev {
                        // SAFETY: Direct txn.put() intentionally bypasses both the
                        // size guard and savepoint undo tracking. These are undo
                        // records restoring values that already existed in TiKV —
                        // blocking ROLLBACK TO SAVEPOINT is worse than allowing the
                        // restore of a pre-existing large value.
                        #[allow(clippy::disallowed_methods)]
                        Some(val) => txn.put(key, val).await.map_err(|e| anyhow!(e))?,
                        None => txn.delete(key).await.map_err(|e| anyhow!(e))?,
                    }
                }
            }

            prepared.target_undo.sort_by(|a, b| a.key.cmp(&b.key));
            for rec in prepared.target_undo.drain(..) {
                match rec.prev {
                    // SAFETY: Same rationale as the nested-savepoint loop above —
                    // restoring pre-existing values during ROLLBACK TO SAVEPOINT.
                    #[allow(clippy::disallowed_methods)]
                    Some(val) => txn.put(rec.key, val).await.map_err(|e| anyhow!(e))?,
                    None => txn.delete(rec.key).await.map_err(|e| anyhow!(e))?,
                }
            }

            Ok::<(), anyhow::Error>(())
        };

        if let Err(e) = res {
            // If undo fails, the transaction is likely in an unknown state. Abort it.
            let _ = self.rollback().await;
            return Err(e);
        }
        // TODO(#601-followup): Regular SET (non-LOCAL) is not restored on savepoint rollback.
        // PostgreSQL restores it; tracking that session-state undo separately from SET LOCAL.
        self.settings.rollback_settings_to_savepoint(name);
        self.rollback_extension_delta_to_savepoint(name);
        self.rollback_session_auth_to_savepoint(name);
        self.last_sequence_values.rollback_to_savepoint(name);
        self.sync_plan_cache_settings();
        self.release_rolled_back_xact_advisory_locks(rolled_back_xact_locks);
        self.clear_failed_transaction();
        Ok(())
    }

    /// Start a transaction block (BEGIN)
    pub async fn begin(&mut self) -> Result<()> {
        match self.state {
            TransactionState::Idle => {
                // Capture the transaction start timestamp before awaiting store/savepoint
                // operations, so TiKV begin latency does not skew NOW()/TRANSACTION_TIMESTAMP().
                // Use the task-local statement timestamp when available (i.e. when called
                // from within execute_single's with_timestamps scope) to match PostgreSQL
                // semantics where transaction_timestamp = start of the BEGIN statement.
                let ts = crate::sql::statement_time::statement_timestamp_millis_or_now();
                let txn = self.store.begin().await?;
                self.savepoints.reset().await?;
                self.reset_xact_advisory_savepoint_tracker().await;
                // Register start_ts in GC active transaction registry before
                // setting state to Active. This ensures the GC safepoint
                // advancer sees this transaction before it can be affected.
                if let Some(ref registry) = self.active_txn_registry {
                    registry
                        .register_connection(self.connection_id, txn.start_timestamp().version());
                }
                self.state = TransactionState::Active(txn);
                self.extension_delta = super::ExtensionDelta::default();
                self.extension_delta_savepoints.clear();
                self.session_auth_savepoints.clear();
                self.transaction_timestamp_ms = Some(ts);
                self.tx_statement_count = 0;
                Ok(())
            }
            TransactionState::Active(_) | TransactionState::Failed(_) => {
                // Already in transaction, ignore
                Ok(())
            }
        }
    }

    /// Returns the transaction start timestamp, or None if not in an explicit transaction.
    pub fn transaction_timestamp_ms(&self) -> Option<i64> {
        self.transaction_timestamp_ms
    }

    /// Commit a transaction block (COMMIT)
    pub async fn commit(&mut self) -> Result<()> {
        self.transaction_timestamp_ms = None;
        self.tx_statement_count = 0;
        self.extension_delta = super::ExtensionDelta::default();
        self.extension_delta_savepoints.clear();
        self.session_auth_savepoints.clear();
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset().await?;
                self.reset_xact_advisory_savepoint_tracker().await;
                match txn.commit().await {
                    Ok(_) => {
                        self.clear_local_overrides();
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.apply_pending_drops();
                        self.observability.record_commit();
                        // Unregister only after definitive commit success.
                        if let Some(ref registry) = self.active_txn_registry {
                            registry.unregister_connection(self.connection_id);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        // State restored to Failed — start_ts still valid,
                        // keep registered in GC registry.
                        self.state = TransactionState::Failed(txn);
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset().await?;
                self.reset_xact_advisory_savepoint_tracker().await;
                match txn.rollback().await {
                    Ok(_) => {
                        self.clear_local_overrides();
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        // Unregister only after definitive rollback success.
                        if let Some(ref registry) = self.active_txn_registry {
                            registry.unregister_connection(self.connection_id);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Idle => Ok(()), // No-op
        }
    }

    /// Rollback a transaction block (ROLLBACK)
    pub async fn rollback(&mut self) -> Result<()> {
        self.transaction_timestamp_ms = None;
        self.tx_statement_count = 0;
        self.extension_delta = super::ExtensionDelta::default();
        self.extension_delta_savepoints.clear();
        self.session_auth_savepoints.clear();
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset().await?;
                self.reset_xact_advisory_savepoint_tracker().await;
                match txn.rollback().await {
                    Ok(_) => {
                        self.clear_local_overrides();
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        // Clear plan cache: DDL within the rolled-back transaction
                        // may have been optimistically invalidated, but the DDL
                        // itself was reverted — stale entries must not survive.
                        self.clear_plan_cache();
                        if let Some(ref registry) = self.active_txn_registry {
                            registry.unregister_connection(self.connection_id);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        self.clear_plan_cache();
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset().await?;
                self.reset_xact_advisory_savepoint_tracker().await;
                match txn.rollback().await {
                    Ok(_) => {
                        self.clear_local_overrides();
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        self.clear_plan_cache();
                        if let Some(ref registry) = self.active_txn_registry {
                            registry.unregister_connection(self.connection_id);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        self.release_xact_advisory_locks_if_needed();
                        self.last_sequence_values.discard_pending_drops();
                        self.clear_plan_cache();
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Idle => Ok(()), // No-op
        }
    }
}
