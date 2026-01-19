//! Session management for transactions

use crate::storage::TikvStore;
use crate::observability::TenantObservability;
use crate::txn::SavepointState;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

pub enum TransactionState {
    Idle,
    Active(Transaction),
}

pub struct Session {
    store: Arc<TikvStore>,
    observability: Arc<TenantObservability>,
    state: TransactionState,
    savepoints: Arc<SavepointState>,
    last_sequence_values: HashMap<String, i64>,
    search_path: Vec<String>,
    #[allow(dead_code)]
    current_user: Option<String>,
    #[allow(dead_code)]
    is_superuser: bool,
}

impl Session {
    pub fn new(store: Arc<TikvStore>, observability: Arc<TenantObservability>) -> Self {
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: HashMap::new(),
            search_path: vec!["public".to_string()],
            current_user: None,
            is_superuser: false,
        }
    }

    pub fn new_with_user(
        store: Arc<TikvStore>,
        observability: Arc<TenantObservability>,
        username: String,
        is_superuser: bool,
    ) -> Self {
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: HashMap::new(),
            search_path: vec!["public".to_string()],
            current_user: Some(username),
            is_superuser,
        }
    }

    #[allow(dead_code)]
    pub fn store(&self) -> Arc<TikvStore> {
        self.store.clone()
    }

    #[allow(dead_code)]
    pub fn current_user(&self) -> Option<&str> {
        self.current_user.as_deref()
    }

    #[allow(dead_code)]
    pub fn is_superuser(&self) -> bool {
        self.is_superuser
    }

    #[allow(dead_code)]
    pub fn set_user(&mut self, username: String, is_superuser: bool) {
        self.current_user = Some(username);
        self.is_superuser = is_superuser;
    }

    /// Check if currently in a transaction block
    pub fn is_in_transaction(&self) -> bool {
        matches!(self.state, TransactionState::Active(_))
    }

    pub(crate) fn savepoints(&self) -> Arc<SavepointState> {
        self.savepoints.clone()
    }

    /// Get mutable reference to active transaction
    pub fn get_mut_txn(&mut self) -> Option<&mut Transaction> {
        match &mut self.state {
            TransactionState::Active(txn) => Some(txn),
            _ => None,
        }
    }

    pub fn get_mut_txn_sequence_values_and_search_path(
        &mut self,
    ) -> Option<(&mut Transaction, &mut HashMap<String, i64>, &[String])> {
        match &mut self.state {
            TransactionState::Active(txn) => {
                Some((txn, &mut self.last_sequence_values, &self.search_path))
            }
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }

    pub fn set_search_path(&mut self, search_path: Vec<String>) {
        self.search_path = search_path;
    }

    pub fn create_savepoint(&mut self, name: String) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(anyhow!("SAVEPOINT can only be used in transaction blocks"));
        }
        self.savepoints.create(name)
    }

    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(anyhow!(
                "RELEASE SAVEPOINT can only be used in transaction blocks"
            ));
        }
        self.savepoints.release(name)
    }

    pub async fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(anyhow!(
                "ROLLBACK TO SAVEPOINT can only be used in transaction blocks"
            ));
        }

        let mut prepared = self.savepoints.prepare_rollback_to(name)?;
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
        Ok(())
    }

    /// Start a transaction block (BEGIN)
    pub async fn begin(&mut self) -> Result<()> {
        match self.state {
            TransactionState::Idle => {
                let txn = self.store.begin().await?;
                self.savepoints.reset()?;
                self.state = TransactionState::Active(txn);
                Ok(())
            }
            TransactionState::Active(_) => {
                // Already in transaction, ignore
                Ok(())
            }
        }
    }

    /// Commit a transaction block (COMMIT)
    pub async fn commit(&mut self) -> Result<()> {
        // Move txn out of state to take ownership
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset()?;
                txn.commit().await.map(|_| ()).map_err(|e| anyhow!(e))?;
                self.observability.record_commit();
                Ok(())
            }
            TransactionState::Idle => {
                Ok(()) // No-op
            }
        }
    }

    /// Rollback a transaction block (ROLLBACK)
    pub async fn rollback(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset()?;
                txn.rollback().await.map_err(|e| anyhow!(e))
            }
            TransactionState::Idle => {
                Ok(()) // No-op
            }
        }
    }
}
