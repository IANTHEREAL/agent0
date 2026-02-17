//! Enqueue path for AFTER triggers.

use super::queue::{encode_trigger_queue_key, TriggerEvent, TriggerOp};
use super::worker::trigger_worker;
use crate::sql::executor::Executor;
use crate::storage::TikvStore;
use crate::types::{Row, TriggerDef};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

const ASYNC_TRIGGER_KEYWORDS: &[&str] = &[
    "http_get",
    "http_post",
    "http_put",
    "http_delete",
    "http_request",
    "extensions.http",
];

fn trigger_body_needs_async(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    ASYNC_TRIGGER_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Execute AFTER-row triggers, synchronously when possible.
///
/// Triggers whose function body contains HTTP/extension calls are enqueued
/// for asynchronous processing. All others execute in the current transaction.
pub(crate) async fn enqueue_after_triggers(
    txn: &mut Transaction,
    db_id: u64,
    keyspace: &str,
    table_full_name: &str,
    op: TriggerOp,
    old_row: Option<&Row>,
    new_row: Option<&Row>,
    triggers: &[TriggerDef],
    store: &Arc<TikvStore>,
    executor: &Executor,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
) -> Result<()> {
    let worker = trigger_worker();
    if !worker.config().enabled {
        return Ok(());
    }

    let op_str = match op {
        TriggerOp::Insert => "INSERT",
        TriggerOp::Update => "UPDATE",
        TriggerOp::Delete => "DELETE",
    };

    let after_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("AFTER")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(op_str))
        })
        .collect();

    if after_triggers.is_empty() {
        return Ok(());
    }

    let schema = store.get_schema(txn, db_id, table_full_name).await?;

    let quota = worker.get_quota(keyspace);
    let mut queued_any = false;

    for trigger in after_triggers {
        let func = store.get_function(txn, db_id, &trigger.function).await?;
        let Some(func) = func else {
            continue;
        };

        if trigger_body_needs_async(&func.body) {
            let remaining = quota
                .max_queue_depth
                .saturating_sub(quota.current_depth.load(std::sync::atomic::Ordering::Relaxed));
            if remaining == 0 {
                continue;
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
            quota
                .current_depth
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            queued_any = true;
        } else if let Some(schema) = &schema {
            Box::pin(worker.execute_trigger_body(
                executor,
                txn,
                db_id,
                sequence_values,
                schema,
                &func.body,
                old_row,
                new_row,
                search_path,
            ))
            .await?;
        }
    }

    if queued_any {
        executor.schedule_trigger_activation(keyspace);
    }

    Ok(())
}
