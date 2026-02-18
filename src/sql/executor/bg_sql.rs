use crate::storage::TikvStore;
use crate::types::Value;
use crate::worker::get_system_store;
use crate::worker::types::{TaskQueueEntry, TaskType, TASK_TYPE_BG_SQL};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tikv_client::Transaction;

pub(crate) fn is_bg_sql_function(func_name: &str) -> bool {
    let upper = func_name.to_uppercase();
    upper == "PG_BACKGROUND_LAUNCH" || upper == "PG_BACKGROUND_RESULT"
}

pub(crate) fn try_execute_bg_sql_function(
    func_name: &str,
    _args: &[Value],
) -> Option<Result<Value>> {
    let upper = func_name.to_uppercase();
    match upper.as_str() {
        "PG_BACKGROUND_LAUNCH" => Some(Err(anyhow!(
            "pg_background_launch must be evaluated during execution"
        ))),
        "PG_BACKGROUND_RESULT" => Some(Err(anyhow!(
            "pg_background_result must be evaluated during execution"
        ))),
        _ => None,
    }
}

pub(crate) async fn execute_bg_sql_function(
    _store: &Arc<TikvStore>,
    _txn: &mut Transaction,
    db_id: u64,
    current_user: &str,
    func_name: &str,
    args: &[Value],
    keyspace: &str,
) -> Option<Result<Value>> {
    let upper = func_name.to_uppercase();
    match upper.as_str() {
        "PG_BACKGROUND_LAUNCH" => {
            Some(execute_bg_launch(db_id, current_user, keyspace, args).await)
        }
        "PG_BACKGROUND_RESULT" => Some(execute_bg_result(keyspace, db_id, args).await),
        _ => None,
    }
}

async fn execute_bg_launch(
    db_id: u64,
    current_user: &str,
    keyspace: &str,
    args: &[Value],
) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!(
            "pg_background_launch() requires exactly 1 argument, got {}",
            args.len()
        ));
    }

    let sql = match &args[0] {
        Value::Text(s) => s.clone(),
        Value::Null => return Err(anyhow!("pg_background_launch: sql must not be NULL")),
        _ => return Err(anyhow!("pg_background_launch: sql must be text")),
    };

    let system_store = get_system_store()
        .ok_or_else(|| anyhow!("pg_background_launch: worker engine not available"))?;

    let task_id = chrono::Utc::now().timestamp_millis();
    let fire_time = task_id;

    let entry = TaskQueueEntry::new(
        keyspace.to_string(),
        db_id,
        task_id,
        TaskType::BgSql,
        sql,
        current_user.to_string(),
        128,
    );

    let mut sys_txn = system_store.begin().await?;
    system_store
        .put_worker_queue_entry(&mut sys_txn, &entry, fire_time)
        .await?;
    system_store
        .update_registry_task_types(&mut sys_txn, keyspace, db_id, TASK_TYPE_BG_SQL, 0)
        .await?;
    sys_txn.commit().await?;

    Ok(Value::Int64(task_id))
}

async fn execute_bg_result(keyspace: &str, db_id: u64, args: &[Value]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!(
            "pg_background_result() requires exactly 1 argument, got {}",
            args.len()
        ));
    }

    let task_id = match &args[0] {
        Value::Int64(id) => *id,
        Value::Int32(id) => *id as i64,
        Value::Null => return Err(anyhow!("pg_background_result: task_id must not be NULL")),
        _ => return Err(anyhow!("pg_background_result: task_id must be bigint")),
    };

    let system_store = get_system_store()
        .ok_or_else(|| anyhow!("pg_background_result: worker engine not available"))?;

    let mut sys_txn = system_store.begin().await?;

    if let Some(result_text) = system_store
        .get_bg_result(&mut sys_txn, keyspace, db_id, task_id)
        .await?
    {
        sys_txn.commit().await?;
        return Ok(Value::Text(result_text));
    }

    let queue_keys = system_store
        .scan_queue_entries_for_task(&mut sys_txn, keyspace, db_id, task_id)
        .await?;
    sys_txn.commit().await?;

    if !queue_keys.is_empty() {
        Ok(Value::Text("pending".to_string()))
    } else {
        Ok(Value::Text("not found".to_string()))
    }
}
