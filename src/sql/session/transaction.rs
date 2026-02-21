//! Transaction management: begin/commit/rollback and savepoint operations.

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};

use super::{Session, TransactionState};

impl Session {
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
        self.savepoints.create(name).await
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
        self.savepoints.release(name).await
    }

    pub async fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "ROLLBACK TO SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }

        let mut prepared = self.savepoints.prepare_rollback_to(name).await?;
        let res = {
            let txn = self.get_mut_txn().expect("transaction must be active");

            // Undo nested savepoints first, then the target savepoint itself.
            for sp in prepared.popped.iter_mut().rev() {
                let mut entries = Vec::with_capacity(sp.undo.len());
                entries.extend(sp.undo.drain());
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                for (key, prev) in entries {
                    match prev {
                        Some(val) => txn.put(key, val).await.map_err(|e| anyhow!(e))?,
                        None => txn.delete(key).await.map_err(|e| anyhow!(e))?,
                    }
                }
            }

            prepared.target_undo.sort_by(|a, b| a.key.cmp(&b.key));
            for rec in prepared.target_undo.drain(..) {
                match rec.prev {
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
                self.state = TransactionState::Active(txn);
                self.transaction_timestamp_ms = Some(ts);
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
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset().await?;
                match txn.commit().await {
                    Ok(_) => {
                        self.observability.record_commit();
                        Ok(())
                    }
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset().await?;
                match txn.rollback().await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
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
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset().await?;
                match txn.rollback().await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset().await?;
                match txn.rollback().await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Idle => Ok(()), // No-op
        }
    }
}
