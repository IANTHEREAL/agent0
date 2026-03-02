use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::model::Value;
use crate::sql::advisory_locks::{
    global_lock_manager, AcquireError, AdvisoryLockManager, AdvisoryLockMode, AdvisoryLockScope,
};
use crate::sql::error::SqlError;
use crate::sql::query_context::{XactAdvisoryLockRecord, XactAdvisorySavepointTracker};
use anyhow::{anyhow, Result};

fn parse_lock_key(args: &[Value]) -> Result<Option<i64>> {
    match args.len() {
        0 => Ok(None),
        1 => match &args[0] {
            Value::Null => Ok(None),
            Value::Int64(n) => Ok(Some(*n)),
            Value::Int32(n) => Ok(Some(*n as i64)),
            _ => Err(anyhow!("advisory lock key must be bigint")),
        },
        2 => {
            let classid = match &args[0] {
                Value::Null => return Ok(None),
                Value::Int32(n) => *n,
                _ => return Err(anyhow!("advisory lock classid must be integer")),
            };
            let objid = match &args[1] {
                Value::Null => return Ok(None),
                Value::Int32(n) => *n,
                _ => return Err(anyhow!("advisory lock objid must be integer")),
            };
            Ok(Some(((classid as i64) << 32) | (objid as u32 as i64)))
        }
        _ => Err(anyhow!(
            "wrong number of arguments for advisory lock function"
        )),
    }
}

pub(crate) async fn execute_advisory_lock_function(
    keyspace: &Arc<str>,
    conn_id: i64,
    func_name: &str,
    args: &[Value],
    lock_timeout: Option<Duration>,
    xact_advisory_lock_used: Option<Arc<AtomicBool>>,
    xact_advisory_savepoint_tracker: Option<Arc<tokio::sync::Mutex<XactAdvisorySavepointTracker>>>,
) -> Option<Result<Value>> {
    execute_advisory_lock_function_with_manager(
        global_lock_manager(),
        keyspace,
        conn_id,
        func_name,
        args,
        lock_timeout,
        xact_advisory_lock_used,
        xact_advisory_savepoint_tracker,
    )
    .await
}

fn map_acquire_error(err: AcquireError) -> anyhow::Error {
    match err {
        AcquireError::Timeout => SqlError::LockTimeout.into(),
        AcquireError::LockLimitExceeded { limit } => {
            SqlError::AdvisoryLockLimitExceeded { limit }.into()
        }
        AcquireError::CounterOverflow => SqlError::AdvisoryLockCounterOverflow.into(),
    }
}

async fn execute_advisory_lock_function_with_manager(
    manager: &AdvisoryLockManager,
    keyspace: &Arc<str>,
    conn_id: i64,
    func_name: &str,
    args: &[Value],
    lock_timeout: Option<Duration>,
    xact_advisory_lock_used: Option<Arc<AtomicBool>>,
    xact_advisory_savepoint_tracker: Option<Arc<tokio::sync::Mutex<XactAdvisorySavepointTracker>>>,
) -> Option<Result<Value>> {
    let mark_xact_lock_used = || {
        if let Some(flag) = &xact_advisory_lock_used {
            flag.store(true, Ordering::Release);
        }
    };

    let upper = func_name.to_ascii_uppercase();

    let result = match upper.as_str() {
        "PG_ADVISORY_LOCK" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Text(String::new()))),
                Err(e) => return Some(Err(e)),
            };
            if let Err(e) = manager
                .acquire(
                    keyspace,
                    key,
                    conn_id,
                    AdvisoryLockMode::Exclusive,
                    AdvisoryLockScope::Session,
                    lock_timeout,
                )
                .await
            {
                return Some(Err(map_acquire_error(e)));
            }
            Ok(Value::Text(String::new()))
        }
        "PG_ADVISORY_LOCK_SHARED" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Text(String::new()))),
                Err(e) => return Some(Err(e)),
            };
            if let Err(e) = manager
                .acquire(
                    keyspace,
                    key,
                    conn_id,
                    AdvisoryLockMode::Shared,
                    AdvisoryLockScope::Session,
                    lock_timeout,
                )
                .await
            {
                return Some(Err(map_acquire_error(e)));
            }
            Ok(Value::Text(String::new()))
        }
        "PG_TRY_ADVISORY_LOCK" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Null)),
                Err(e) => return Some(Err(e)),
            };
            match manager.try_acquire_checked(
                keyspace,
                key,
                conn_id,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
            ) {
                Ok(acquired) => Ok(Value::Boolean(acquired)),
                Err(e) => return Some(Err(map_acquire_error(e))),
            }
        }
        "PG_TRY_ADVISORY_LOCK_SHARED" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Null)),
                Err(e) => return Some(Err(e)),
            };
            match manager.try_acquire_checked(
                keyspace,
                key,
                conn_id,
                AdvisoryLockMode::Shared,
                AdvisoryLockScope::Session,
            ) {
                Ok(acquired) => Ok(Value::Boolean(acquired)),
                Err(e) => return Some(Err(map_acquire_error(e))),
            }
        }
        "PG_ADVISORY_UNLOCK" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Null)),
                Err(e) => return Some(Err(e)),
            };
            Ok(Value::Boolean(manager.release_session(
                keyspace,
                key,
                conn_id,
                AdvisoryLockMode::Exclusive,
            )))
        }
        "PG_ADVISORY_UNLOCK_SHARED" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Null)),
                Err(e) => return Some(Err(e)),
            };
            Ok(Value::Boolean(manager.release_session(
                keyspace,
                key,
                conn_id,
                AdvisoryLockMode::Shared,
            )))
        }
        "PG_ADVISORY_UNLOCK_ALL" => {
            manager.release_all_session_locks(conn_id);
            Ok(Value::Text(String::new()))
        }
        "PG_ADVISORY_XACT_LOCK" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Text(String::new()))),
                Err(e) => return Some(Err(e)),
            };
            if let Err(e) = manager
                .acquire(
                    keyspace,
                    key,
                    conn_id,
                    AdvisoryLockMode::Exclusive,
                    AdvisoryLockScope::Transaction,
                    lock_timeout,
                )
                .await
            {
                return Some(Err(map_acquire_error(e)));
            }
            mark_xact_lock_used();
            if let Some(tracker) = &xact_advisory_savepoint_tracker {
                let mut t = tracker.lock().await;
                t.record_acquired_lock(XactAdvisoryLockRecord {
                    keyspace: keyspace.clone(),
                    key,
                    mode: AdvisoryLockMode::Exclusive,
                });
            }
            Ok(Value::Text(String::new()))
        }
        "PG_ADVISORY_XACT_LOCK_SHARED" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Text(String::new()))),
                Err(e) => return Some(Err(e)),
            };
            if let Err(e) = manager
                .acquire(
                    keyspace,
                    key,
                    conn_id,
                    AdvisoryLockMode::Shared,
                    AdvisoryLockScope::Transaction,
                    lock_timeout,
                )
                .await
            {
                return Some(Err(map_acquire_error(e)));
            }
            mark_xact_lock_used();
            if let Some(tracker) = &xact_advisory_savepoint_tracker {
                let mut t = tracker.lock().await;
                t.record_acquired_lock(XactAdvisoryLockRecord {
                    keyspace: keyspace.clone(),
                    key,
                    mode: AdvisoryLockMode::Shared,
                });
            }
            Ok(Value::Text(String::new()))
        }
        "PG_TRY_ADVISORY_XACT_LOCK" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Null)),
                Err(e) => return Some(Err(e)),
            };
            match manager.try_acquire_checked(
                keyspace,
                key,
                conn_id,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Transaction,
            ) {
                Ok(acquired) => {
                    if acquired {
                        mark_xact_lock_used();
                        if let Some(tracker) = &xact_advisory_savepoint_tracker {
                            let mut t = tracker.lock().await;
                            t.record_acquired_lock(XactAdvisoryLockRecord {
                                keyspace: keyspace.clone(),
                                key,
                                mode: AdvisoryLockMode::Exclusive,
                            });
                        }
                    }
                    Ok(Value::Boolean(acquired))
                }
                Err(e) => return Some(Err(map_acquire_error(e))),
            }
        }
        "PG_TRY_ADVISORY_XACT_LOCK_SHARED" => {
            let key = match parse_lock_key(args) {
                Ok(Some(k)) => k,
                Ok(None) => return Some(Ok(Value::Null)),
                Err(e) => return Some(Err(e)),
            };
            match manager.try_acquire_checked(
                keyspace,
                key,
                conn_id,
                AdvisoryLockMode::Shared,
                AdvisoryLockScope::Transaction,
            ) {
                Ok(acquired) => {
                    if acquired {
                        mark_xact_lock_used();
                        if let Some(tracker) = &xact_advisory_savepoint_tracker {
                            let mut t = tracker.lock().await;
                            t.record_acquired_lock(XactAdvisoryLockRecord {
                                keyspace: keyspace.clone(),
                                key,
                                mode: AdvisoryLockMode::Shared,
                            });
                        }
                    }
                    Ok(Value::Boolean(acquired))
                }
                Err(e) => return Some(Err(map_acquire_error(e))),
            }
        }
        _ => return None,
    };

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn test_lock_timeout_maps_to_sqlstate_55p03() {
        let manager = AdvisoryLockManager::new();
        let keyspace: Arc<str> = Arc::from("tenant_timeout");
        assert!(manager.try_acquire(
            &keyspace,
            42,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        let result = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            200,
            "pg_advisory_lock",
            &[Value::Int64(42)],
            Some(Duration::from_millis(25)),
            None,
            None,
        )
        .await
        .expect("advisory lock function should be recognized");

        let err = result.expect_err("second connection should hit lock timeout");
        let sql = err
            .downcast_ref::<SqlError>()
            .expect("timeout should map to SqlError");
        assert!(matches!(sql, SqlError::LockTimeout));
        assert_eq!(sql.sqlstate(), "55P03");
    }

    #[tokio::test]
    async fn test_lock_limit_maps_to_program_limit_exceeded() {
        let manager = AdvisoryLockManager::with_max_locks_per_connection(Some(1));
        let keyspace: Arc<str> = Arc::from("tenant_limit");

        let first = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_advisory_lock",
            &[Value::Int64(1)],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .expect("first lock acquire should succeed");
        assert_eq!(first, Value::Text(String::new()));

        let second = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_advisory_lock",
            &[Value::Int64(2)],
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let err = second.expect_err("second distinct key should exceed lock cap");
        let sql = err
            .downcast_ref::<SqlError>()
            .expect("lock cap should map to SqlError");
        assert!(matches!(
            sql,
            SqlError::AdvisoryLockLimitExceeded { limit: 1 }
        ));
        assert_eq!(sql.sqlstate(), "54000");

        let try_lock = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_try_advisory_lock",
            &[Value::Int64(3)],
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let err = try_lock.expect_err("try lock should error once lock cap is exceeded");
        let sql = err
            .downcast_ref::<SqlError>()
            .expect("lock cap should map to SqlError");
        assert!(matches!(
            sql,
            SqlError::AdvisoryLockLimitExceeded { limit: 1 }
        ));
        assert_eq!(sql.sqlstate(), "54000");
    }

    #[tokio::test]
    async fn test_xact_lock_marks_transaction_flag_on_success() {
        let manager = AdvisoryLockManager::new();
        let keyspace: Arc<str> = Arc::from("tenant_xact_flag");
        let used = Arc::new(AtomicBool::new(false));

        let result = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_try_advisory_xact_lock",
            &[Value::Int64(7)],
            None,
            Some(used.clone()),
            None,
        )
        .await
        .expect("function should be recognized")
        .expect("try xact lock should not error");
        assert_eq!(result, Value::Boolean(true));
        assert!(used.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_xact_lock_records_savepoint_tracker_on_success() {
        let manager = AdvisoryLockManager::new();
        let keyspace: Arc<str> = Arc::from("tenant_xact_tracker");
        let used = Arc::new(AtomicBool::new(false));
        let tracker = Arc::new(tokio::sync::Mutex::new(
            XactAdvisorySavepointTracker::default(),
        ));
        {
            let mut locked = tracker.lock().await;
            locked.create("sp1".to_string());
        }

        let result = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_try_advisory_xact_lock_shared",
            &[Value::Int64(17)],
            None,
            Some(used.clone()),
            Some(tracker.clone()),
        )
        .await
        .expect("function should be recognized")
        .expect("try xact lock should not error");
        assert_eq!(result, Value::Boolean(true));
        assert!(used.load(Ordering::Acquire));

        let released = {
            let mut locked = tracker.lock().await;
            locked
                .prepare_rollback_to("sp1")
                .expect("savepoint should exist")
        };
        assert_eq!(released.len(), 1);
        let lock = &released[0];
        assert_eq!(lock.keyspace.as_ref(), keyspace.as_ref());
        assert_eq!(lock.key, 17);
        assert_eq!(lock.mode, AdvisoryLockMode::Shared);
    }

    #[tokio::test]
    async fn test_xact_lock_does_not_mark_flag_on_conflict_false() {
        let manager = AdvisoryLockManager::new();
        let keyspace: Arc<str> = Arc::from("tenant_xact_flag_conflict");
        assert!(manager.try_acquire(
            &keyspace,
            11,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction
        ));

        let used = Arc::new(AtomicBool::new(false));
        let result = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            200,
            "pg_try_advisory_xact_lock",
            &[Value::Int64(11)],
            None,
            Some(used.clone()),
            None,
        )
        .await
        .expect("function should be recognized")
        .expect("try xact lock should return scalar");
        assert_eq!(result, Value::Boolean(false));
        assert!(!used.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_two_arg_bigint_values_are_rejected() {
        let manager = AdvisoryLockManager::new();
        let keyspace: Arc<str> = Arc::from("tenant_two_arg_bigint");
        let result = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_try_advisory_lock",
            &[Value::Int64(1), Value::Int64(2)],
            None,
            None,
            None,
        )
        .await
        .expect("function should be recognized");
        let err = result.expect_err("two-arg advisory lock requires integer,integer");
        assert!(err
            .to_string()
            .contains("advisory lock classid must be integer"));
    }

    #[tokio::test]
    async fn test_counter_overflow_maps_to_sqlstate_54000() {
        let manager = AdvisoryLockManager::new();
        let keyspace: Arc<str> = Arc::from("tenant_overflow_e2e");

        // Force session_count to MAX so next acquire overflows
        manager.force_session_count_for_test(
            &keyspace,
            1,
            100,
            crate::sql::advisory_locks::AdvisoryLockMode::Exclusive,
            u32::MAX,
        );

        let result = execute_advisory_lock_function_with_manager(
            &manager,
            &keyspace,
            100,
            "pg_advisory_lock",
            &[Value::Int64(1)],
            Some(Duration::from_millis(100)),
            None,
            None,
        )
        .await
        .expect("function should be recognized");

        let err = result.expect_err("should fail with counter overflow");
        let sql = err
            .downcast_ref::<SqlError>()
            .expect("overflow should map to SqlError");
        assert!(matches!(sql, SqlError::AdvisoryLockCounterOverflow));
        assert_eq!(sql.sqlstate(), "54000");
    }
}
