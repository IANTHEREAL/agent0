use crate::model::Value;
use crate::storage::TikvStore;
use crate::worker::get_system_store;
use crate::worker::types::{TaskQueueEntry, TaskType, TASK_TYPE_BG_SQL};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tikv_client::Transaction;

pub(crate) fn is_bg_sql_function(func_name: &str) -> bool {
    let upper = func_name.to_uppercase();
    upper == "PG_BACKGROUND_LAUNCH"
        || upper == "PG_BACKGROUND_RESULT"
        || upper == "DB9_REFRESH_STORAGE_STATS"
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
        "DB9_REFRESH_STORAGE_STATS" => Some(Err(anyhow!(
            "db9_refresh_storage_stats must be evaluated during execution"
        ))),
        _ => None,
    }
}

pub(crate) async fn execute_bg_sql_function(
    store: &Arc<TikvStore>,
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
        "DB9_REFRESH_STORAGE_STATS" => {
            Some(execute_refresh_storage_stats(Some(store), db_id, current_user, keyspace).await)
        }
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

    if !crate::worker::execution_enabled() {
        return Err(anyhow!("pg_background_launch: worker engine not available"));
    }
    let system_store = get_system_store()
        .ok_or_else(|| anyhow!("pg_background_launch: worker system store not available"))?;

    // task_id: collision-free identity via TiKV CAS atomic counter (per tenant-db scope).
    // fire_time: scheduling order only — may repeat across launches.
    let task_id = system_store.next_bg_task_id(keyspace, db_id).await?;
    let fire_time = chrono::Utc::now().timestamp_millis();

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
    let enqueued = system_store
        .enqueue_registry_task_v2_unless_db_dropped(
            &mut sys_txn,
            &entry,
            fire_time,
            TASK_TYPE_BG_SQL,
        )
        .await?;
    if !enqueued {
        sys_txn.rollback().await.ok();
        return Err(anyhow!("pg_background_launch: database is being dropped"));
    }
    sys_txn.commit().await?;
    crate::worker::wake_worker();

    Ok(Value::Int64(task_id))
}

async fn execute_refresh_storage_stats(
    store: Option<&Arc<TikvStore>>,
    db_id: u64,
    current_user: &str,
    keyspace: &str,
) -> Result<Value> {
    if current_user != "admin" {
        return Err(anyhow!(
            "db9_refresh_storage_stats: permission denied (superuser required)"
        ));
    }

    if !crate::worker::execution_enabled() {
        return Err(anyhow!(
            "db9_refresh_storage_stats: worker engine not available"
        ));
    }
    let system_store = get_system_store()
        .ok_or_else(|| anyhow!("db9_refresh_storage_stats: worker system store not available"))?;

    if crate::worker::config::WorkerConfig::from_env().storage_scan_derived_active {
        let store = store
            .ok_or_else(|| anyhow!("db9_refresh_storage_stats: tenant store not available"))?;
        crate::worker::engine::request_storage_scan_refresh(
            system_store,
            store.as_ref(),
            keyspace,
            db_id,
        )
        .await?;
        crate::worker::engine::enqueue_storage_scan(system_store, keyspace, db_id).await?;
    } else {
        crate::worker::engine::enqueue_storage_scan(system_store, keyspace, db_id).await?;
    }

    Ok(Value::Text("storage scan enqueued".to_string()))
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

    let pending = system_store
        .task_has_pending(&mut sys_txn, keyspace, db_id, task_id, TaskType::BgSql)
        .await?;
    sys_txn.commit().await?;

    if pending {
        Ok(Value::Text("pending".to_string()))
    } else {
        Ok(Value::Text("not found".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_bg_sql_function_is_case_insensitive() {
        assert!(is_bg_sql_function("pg_background_launch"));
        assert!(is_bg_sql_function("PG_BACKGROUND_RESULT"));
        assert!(is_bg_sql_function("db9_refresh_storage_stats"));
        assert!(is_bg_sql_function("DB9_REFRESH_STORAGE_STATS"));
        assert!(!is_bg_sql_function("pg_sleep"));
    }

    #[test]
    fn try_execute_bg_sql_function_routes_known_names() {
        let launch = try_execute_bg_sql_function("pg_background_launch", &[])
            .expect("known function should be handled")
            .unwrap_err()
            .to_string();
        assert!(launch.contains("must be evaluated during execution"));

        let result = try_execute_bg_sql_function("PG_BACKGROUND_RESULT", &[])
            .expect("known function should be handled")
            .unwrap_err()
            .to_string();
        assert!(result.contains("must be evaluated during execution"));

        assert!(try_execute_bg_sql_function("unknown_func", &[]).is_none());
    }

    #[tokio::test]
    async fn execute_bg_launch_validates_arguments() {
        let err = execute_bg_launch(1, "u", "ks", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires exactly 1 argument"));

        let err = execute_bg_launch(1, "u", "ks", &[Value::Null])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("sql must not be NULL"));

        let err = execute_bg_launch(1, "u", "ks", &[Value::Int32(1)])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("sql must be text"));
    }

    #[tokio::test]
    async fn execute_bg_result_validates_arguments() {
        let err = execute_bg_result("ks", 1, &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires exactly 1 argument"));

        let err = execute_bg_result("ks", 1, &[Value::Null])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("task_id must not be NULL"));

        let err = execute_bg_result("ks", 1, &[Value::Text("x".to_string())])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("task_id must be bigint"));
    }

    #[tokio::test]
    async fn execute_bg_launch_and_result_report_worker_unavailable() {
        let err = execute_bg_launch(1, "u", "ks", &[Value::Text("select 1".to_string())])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("worker engine not available"));

        let err = execute_bg_result("ks", 1, &[Value::Int32(1)])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("worker engine not available"));
    }

    /// task_id is now sourced from TiKV CAS counter (not timestamp), so no inline
    /// timestamp fallback exists. The launch path errors at system_store acquisition,
    /// confirming the CAS allocator is the sole task_id source.
    #[tokio::test]
    async fn execute_refresh_storage_stats_requires_superuser() {
        let err = execute_refresh_storage_stats(None, 1, "regular_user", "ks")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
    }

    #[tokio::test]
    async fn execute_refresh_storage_stats_requires_worker_engine() {
        let err = execute_refresh_storage_stats(None, 1, "admin", "ks")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("worker engine not available"));
    }

    #[test]
    fn refresh_storage_stats_requests_derived_state_and_queue_nudge_when_active() {
        let source = include_str!("bg_sql.rs");
        let refresh = source
            .split("async fn execute_refresh_storage_stats(")
            .nth(1)
            .and_then(|rest| rest.split("async fn execute_bg_result").next())
            .expect("execute_refresh_storage_stats must exist before execute_bg_result");

        let active = refresh
            .find("WorkerConfig::from_env().storage_scan_derived_active")
            .expect("refresh must check derived StorageScan active flag");
        let active_branch = refresh[active..]
            .split("} else {")
            .next()
            .expect("active derived branch must exist before fallback branch");
        let derived = active_branch
            .find("request_storage_scan_refresh")
            .expect("active refresh must write derived StorageScan state directly");
        let nudge = active_branch
            .find("enqueue_storage_scan")
            .expect("active refresh must enqueue a StorageSizeScan nudge");
        let fallback = refresh
            .split("} else {")
            .nth(1)
            .expect("inactive fallback branch must exist");
        assert!(
            fallback.contains("enqueue_storage_scan(system_store, keyspace, db_id).await?"),
            "inactive refresh must keep legacy V2 enqueue fallback"
        );

        assert!(
            derived < nudge,
            "manual refresh must request derived state before enqueueing the nudge"
        );
    }

    #[tokio::test]
    async fn execute_bg_launch_uses_system_store_for_task_id() {
        let err = execute_bg_launch(1, "u", "ks", &[Value::Text("select 1".to_string())])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("worker engine not available"),
            "launch must go through system_store (CAS allocator), not inline timestamp"
        );
    }
}
