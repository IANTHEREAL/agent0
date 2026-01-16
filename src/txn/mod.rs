//! Transaction-level helpers.
//!
//! This module implements PostgreSQL-like SAVEPOINT semantics on top of TiKV's
//! transactional API by recording "before images" for mutated keys.

mod savepoints;
mod state;

use anyhow::{anyhow, Result};
use std::future::Future;
use std::sync::Arc;
use tikv_client::Transaction;

pub(crate) use state::SavepointState;

tokio::task_local! {
    /// Session-scoped savepoint state for the currently executing query.
    static SAVEPOINTS: Arc<SavepointState>;
}

/// Run `future` with the given savepoint manager set as task-local context.
pub(crate) async fn with_savepoints<R>(
    savepoints: Arc<SavepointState>,
    future: impl Future<Output = R>,
) -> R {
    SAVEPOINTS.scope(savepoints, future).await
}

/// TiKV `put` wrapper that records undo information when SAVEPOINT is active.
#[inline]
pub(crate) async fn txn_put(txn: &mut Transaction, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
    let should_record = match SAVEPOINTS.try_with(|sp| sp.should_record_key(&key)) {
        Ok(res) => res?,
        Err(_) => false,
    };

    if should_record {
        let prev = txn.get(key.clone()).await.map_err(|e| anyhow!(e))?;
        if let Ok(res) = SAVEPOINTS.try_with(|sp| sp.record_prev_value(key.clone(), prev)) {
            res?;
        }
    }

    txn.put(key, value).await.map_err(|e| anyhow!(e))
}

/// TiKV `delete` wrapper that records undo information when SAVEPOINT is active.
#[inline]
pub(crate) async fn txn_delete(txn: &mut Transaction, key: Vec<u8>) -> Result<()> {
    let should_record = match SAVEPOINTS.try_with(|sp| sp.should_record_key(&key)) {
        Ok(res) => res?,
        Err(_) => false,
    };

    if should_record {
        let prev = txn.get(key.clone()).await.map_err(|e| anyhow!(e))?;
        if let Ok(res) = SAVEPOINTS.try_with(|sp| sp.record_prev_value(key.clone(), prev)) {
            res?;
        }
    }

    txn.delete(key).await.map_err(|e| anyhow!(e))
}
