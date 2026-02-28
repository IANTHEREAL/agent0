//! Transaction-level helpers.
//!
//! This module implements PostgreSQL-like SAVEPOINT semantics on top of TiKV's
//! transactional API by recording "before images" for mutated keys.

mod savepoints;
mod state;

use anyhow::{anyhow, Result};
use std::future::Future;
use std::sync::Arc;
use tikv_client::transaction::Mutation;
use tikv_client::Transaction;

use crate::storage::backpressure::tikv_op;

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
    // See `sql::query_context::with_query_context` for rationale.
    #[cfg(debug_assertions)]
    {
        SAVEPOINTS.scope(savepoints, Box::pin(future)).await
    }

    #[cfg(not(debug_assertions))]
    {
        SAVEPOINTS.scope(savepoints, future).await
    }
}

/// TiKV `put` wrapper that records undo information when SAVEPOINT is active.
#[inline]
pub(crate) async fn txn_put(txn: &mut Transaction, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
    let savepoints = SAVEPOINTS.try_with(|sp| sp.clone()).ok();
    let should_record = match savepoints.as_ref() {
        Some(sp) => sp.should_record_key(&key).await?,
        None => false,
    };

    if should_record {
        let prev = tikv_op!(txn.get(key.clone()).await).map_err(|e| anyhow!(e))?;
        if let Some(sp) = savepoints {
            sp.record_prev_value(key.clone(), prev).await?;
        }
    }

    tikv_op!(txn.put(key, value).await).map_err(|e| anyhow!(e))
}

/// TiKV `batch_mutate` wrapper that records undo information when SAVEPOINT is
/// active and acquires pessimistic locks for **all** keys in a single RPC
/// (vs one lock RPC per key with individual `txn_put` calls).
#[inline]
pub(crate) async fn txn_batch_mutate(
    txn: &mut Transaction,
    mutations: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    if mutations.is_empty() {
        return Ok(());
    }

    let savepoints = SAVEPOINTS.try_with(|sp| sp.clone()).ok();

    if let Some(ref sp) = savepoints {
        for (key, _) in &mutations {
            if sp.should_record_key(key).await? {
                let prev = txn.get(key.clone()).await.map_err(|e| anyhow!(e))?;
                sp.record_prev_value(key.clone(), prev).await?;
            }
        }
    }

    let tikv_mutations: Vec<Mutation> = mutations
        .into_iter()
        .map(|(k, v)| Mutation::Put(k.into(), v))
        .collect();
    txn.batch_mutate(tikv_mutations)
        .await
        .map_err(|e| anyhow!(e))
}

/// TiKV `delete` wrapper that records undo information when SAVEPOINT is active.
#[inline]
pub(crate) async fn txn_delete(txn: &mut Transaction, key: Vec<u8>) -> Result<()> {
    let savepoints = SAVEPOINTS.try_with(|sp| sp.clone()).ok();
    let should_record = match savepoints.as_ref() {
        Some(sp) => sp.should_record_key(&key).await?,
        None => false,
    };

    if should_record {
        let prev = tikv_op!(txn.get(key.clone()).await).map_err(|e| anyhow!(e))?;
        if let Some(sp) = savepoints {
            sp.record_prev_value(key.clone(), prev).await?;
        }
    }

    tikv_op!(txn.delete(key).await).map_err(|e| anyhow!(e))
}
