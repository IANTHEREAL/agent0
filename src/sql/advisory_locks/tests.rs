use super::*;
use std::sync::Arc;
use std::time::Duration;

fn ks(s: &str) -> Arc<str> {
    Arc::from(s)
}

#[test]
fn test_exclusive_blocks_other_exclusive() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_distinct_large_connection_ids_do_not_collide() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_conn_id_64");
    let conn_a = i64::from(i32::MAX) + 1;
    let conn_b = conn_a + (1_i64 << 32);

    assert!(mgr.try_acquire(
        &k,
        1,
        conn_a,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(
        !mgr.try_acquire(
            &k,
            1,
            conn_b,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ),
        "different i64 connection ids must never be treated as the same holder"
    );

    mgr.release_all_for_connection(conn_a);
    assert!(mgr.try_acquire(
        &k,
        1,
        conn_b,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_exclusive_blocks_other_shared() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_shared_allows_other_shared() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_shared_blocks_other_exclusive() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_same_session_reentrant_exclusive() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_same_session_reentrant_shared() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_same_session_exclusive_then_shared() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_same_session_shared_then_exclusive() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_stacking_requires_equal_unlocks() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );

    assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_session_scope_survives_xact_release() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.release_xact_locks(100);
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_xact_scope_released_by_xact_release() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    mgr.release_xact_locks(100);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_xact_release_does_not_affect_session() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    mgr.release_xact_locks(100);
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_session_release_does_not_affect_xact() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
}

#[test]
fn test_release_xact_releases_single_xact_stack_entry() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_release_single_xact");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction
    ));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    assert!(mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    assert!(mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
}

#[test]
fn test_release_xact_does_not_release_session_entry() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_release_xact_only");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    assert!(!mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_release_all_for_connection() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        2,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Transaction,
    );
    mgr.try_acquire(
        &k,
        3,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.release_all_for_connection(100);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        2,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        3,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_release_all_session_locks() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        2,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        3,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    mgr.release_all_session_locks(100);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        2,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        3,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_release_xact_locks() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    mgr.try_acquire(
        &k,
        2,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Transaction,
    );
    mgr.try_acquire(
        &k,
        3,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.release_xact_locks(100);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        2,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        3,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_connection_key_index_is_updated_by_xact_release() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_index");
    let key_session = (k.clone(), 10_i64);
    let key_xact_1 = (k.clone(), 20_i64);
    let key_xact_2 = (k.clone(), 30_i64);

    assert!(mgr.try_acquire(
        &k,
        10,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        20,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction
    ));
    assert!(mgr.try_acquire(
        &k,
        30,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Transaction
    ));

    {
        let state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
        let keys = state
            .connection_keys
            .get(&100)
            .expect("connection key index should exist");
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&key_session));
        assert!(keys.contains(&key_xact_1));
        assert!(keys.contains(&key_xact_2));
    }

    mgr.release_xact_locks(100);

    {
        let state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
        let keys = state
            .connection_keys
            .get(&100)
            .expect("session-scoped key should keep index entry");
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&key_session));
        assert!(!keys.contains(&key_xact_1));
        assert!(!keys.contains(&key_xact_2));
    }
}

#[test]
fn test_two_int_key_encoding() {
    let classid: i32 = 1;
    let objid: i32 = 2;
    let key = ((classid as i64) << 32) | (objid as u32 as i64);
    assert_eq!(key, (1_i64 << 32) | 2);

    let objid_neg: i32 = -1;
    let key_neg = ((classid as i64) << 32) | (objid_neg as u32 as i64);
    assert_eq!(key_neg, (1_i64 << 32) | 0xFFFFFFFF);
}

#[test]
fn test_is_advisory_lock_function() {
    assert!(is_advisory_lock_function("pg_advisory_lock"));
    assert!(is_advisory_lock_function("PG_ADVISORY_LOCK"));
    assert!(is_advisory_lock_function("pg_advisory_lock_shared"));
    assert!(is_advisory_lock_function("pg_advisory_xact_lock"));
    assert!(is_advisory_lock_function("pg_advisory_xact_lock_shared"));
    assert!(is_advisory_lock_function("pg_try_advisory_lock"));
    assert!(is_advisory_lock_function("pg_try_advisory_lock_shared"));
    assert!(is_advisory_lock_function("pg_try_advisory_xact_lock"));
    assert!(is_advisory_lock_function(
        "pg_try_advisory_xact_lock_shared"
    ));
    assert!(is_advisory_lock_function("pg_advisory_unlock"));
    assert!(is_advisory_lock_function("pg_advisory_unlock_shared"));
    assert!(is_advisory_lock_function("pg_advisory_unlock_all"));
    assert!(!is_advisory_lock_function("pg_advisory_lck"));
    assert!(!is_advisory_lock_function("advisory_lock"));
    assert!(!is_advisory_lock_function("pg_sleep"));
}

#[tokio::test]
async fn test_blocking_acquire_wakes_on_release() {
    let mgr = Arc::new(AdvisoryLockManager::new());
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    let mgr2 = mgr.clone();
    let k2 = k.clone();
    let handle = tokio::spawn(async move {
        mgr2.acquire(
            &k2,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
            None,
        )
        .await
        .unwrap();
    });

    tokio::task::yield_now().await;
    mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive);

    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("blocking acquire should complete after release")
        .expect("task should not panic");

    assert!(!mgr.try_acquire(
        &k,
        1,
        300,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_multi_session_shared_then_exclusive_conflict() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        1,
        300,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    mgr.release_session(&k, 1, 100, AdvisoryLockMode::Shared);
    assert!(!mgr.try_acquire(
        &k,
        1,
        300,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    mgr.release_session(&k, 1, 200, AdvisoryLockMode::Shared);
    assert!(mgr.try_acquire(
        &k,
        1,
        300,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_xact_lock_released_on_rollback_simulation() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    mgr.release_xact_locks(100);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_unlock_xact_scoped_via_session_unlock_returns_false() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    assert!(!mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    assert!(!mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_tenant_isolation() {
    let mgr = AdvisoryLockManager::new();
    let t1 = ks("tenant_a");
    let t2 = ks("tenant_b");
    assert!(mgr.try_acquire(
        &t1,
        42,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &t2,
        42,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &t1,
        42,
        300,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_unlock_all_does_not_release_xact_locks() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t1");
    mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    mgr.try_acquire(
        &k,
        2,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    mgr.release_all_session_locks(100);
    assert!(mgr.try_acquire(
        &k,
        1,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        2,
        200,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[tokio::test]
async fn test_acquire_timeout() {
    let mgr = Arc::new(AdvisoryLockManager::new());
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    let result = mgr
        .acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
            Some(Duration::from_millis(50)),
        )
        .await;
    assert!(result.is_err());

    assert!(!mgr.release_session(&k, 1, 200, AdvisoryLockMode::Exclusive));
    assert!(!mgr.try_acquire(
        &k,
        1,
        300,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
}

#[test]
fn test_try_acquire_respects_lock_limit() {
    let mgr = AdvisoryLockManager::with_max_locks_per_connection(Some(2));
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        2,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(mgr.try_acquire(
        &k,
        2,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    assert!(!mgr.try_acquire(
        &k,
        3,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    let err = mgr
        .try_acquire_checked(
            &k,
            4,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        )
        .expect_err("checked try-acquire should surface lock-cap violations");
    assert_eq!(err, AcquireError::LockLimitExceeded { limit: 2 });
}

#[tokio::test]
async fn test_blocking_acquire_returns_lock_limit_error() {
    let mgr = AdvisoryLockManager::with_max_locks_per_connection(Some(1));
    let k = ks("t1");
    assert!(mgr.try_acquire(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));
    let err = mgr
        .acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
            Some(Duration::from_millis(500)),
        )
        .await
        .expect_err("second distinct key should hit lock limit");
    assert_eq!(err, AcquireError::LockLimitExceeded { limit: 1 });
}

#[tokio::test]
async fn test_release_cleans_unused_key_notifier_after_waiter_timeout() {
    let mgr = AdvisoryLockManager::new();
    let keyspace = ks("tenant_notifier_cleanup");
    let key = 42_i64;
    let lk = (keyspace.clone(), key);

    assert!(mgr.try_acquire(
        &keyspace,
        key,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    let timeout_err = mgr
        .acquire(
            &keyspace,
            key,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
            Some(Duration::from_millis(10)),
        )
        .await
        .expect_err("waiter should time out while lock is still held");
    assert_eq!(timeout_err, AcquireError::Timeout);

    assert!(mgr.release_session(&keyspace, key, 100, AdvisoryLockMode::Exclusive));

    let state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
    assert!(!state.locks.contains_key(&lk));
    assert!(!state.key_notifiers.contains_key(&lk));
}

fn force_session_count(
    mgr: &AdvisoryLockManager,
    keyspace: &Arc<str>,
    key: i64,
    conn_id: i64,
    mode: AdvisoryLockMode,
    count: u32,
) {
    let mut state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
    let lk = (keyspace.clone(), key);
    let lock_state = state.locks.entry(lk.clone()).or_insert_with(LockState::new);
    let holders = match mode {
        AdvisoryLockMode::Exclusive => &mut lock_state.exclusive_holders,
        AdvisoryLockMode::Shared => &mut lock_state.shared_holders,
    };
    let info = holders.entry(conn_id).or_default();
    info.session_count = count;
    state.connection_keys.entry(conn_id).or_default().insert(lk);
}

fn force_xact_count(
    mgr: &AdvisoryLockManager,
    keyspace: &Arc<str>,
    key: i64,
    conn_id: i64,
    mode: AdvisoryLockMode,
    count: u32,
) {
    let mut state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
    let lk = (keyspace.clone(), key);
    let lock_state = state.locks.entry(lk.clone()).or_insert_with(LockState::new);
    let holders = match mode {
        AdvisoryLockMode::Exclusive => &mut lock_state.exclusive_holders,
        AdvisoryLockMode::Shared => &mut lock_state.shared_holders,
    };
    let info = holders.entry(conn_id).or_default();
    info.xact_count = count;
    state.connection_keys.entry(conn_id).or_default().insert(lk);
}

#[test]
fn test_session_counter_overflow_returns_error() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_overflow_session");

    force_session_count(&mgr, &k, 1, 100, AdvisoryLockMode::Exclusive, u32::MAX - 1);

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    assert_eq!(result, Ok(true));

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session,
    );
    assert_eq!(result, Err(AcquireError::CounterOverflow));
}

#[test]
fn test_xact_counter_overflow_returns_error() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_overflow_xact");

    force_xact_count(&mgr, &k, 1, 100, AdvisoryLockMode::Exclusive, u32::MAX - 1);

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    assert_eq!(result, Ok(true));

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Transaction,
    );
    assert_eq!(result, Err(AcquireError::CounterOverflow));
}

#[test]
fn test_session_counter_overflow_shared_mode() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_overflow_session_shared");

    force_session_count(&mgr, &k, 1, 100, AdvisoryLockMode::Shared, u32::MAX - 1);

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session,
    );
    assert_eq!(result, Ok(true));

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Session,
    );
    assert_eq!(result, Err(AcquireError::CounterOverflow));
}

#[test]
fn test_xact_counter_overflow_shared_mode() {
    let mgr = AdvisoryLockManager::new();
    let k = ks("t_overflow_xact_shared");

    force_xact_count(&mgr, &k, 1, 100, AdvisoryLockMode::Shared, u32::MAX - 1);

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Transaction,
    );
    assert_eq!(result, Ok(true));

    let result = mgr.try_acquire_checked(
        &k,
        1,
        100,
        AdvisoryLockMode::Shared,
        AdvisoryLockScope::Transaction,
    );
    assert_eq!(result, Err(AcquireError::CounterOverflow));
}

#[test]
fn test_total_does_not_overflow() {
    let info = state::HolderInfo {
        session_count: u32::MAX,
        xact_count: 1,
    };
    assert_eq!(info.total(), u32::MAX);
}

#[tokio::test]
async fn test_timeout_error_path_cleans_orphan_notifier() {
    use std::time::Instant;

    let mgr = Arc::new(AdvisoryLockManager::new());
    let keyspace = ks("tenant_timeout_orphan_cleanup");
    let key = 314_i64;
    let lk = (keyspace.clone(), key);

    assert!(mgr.try_acquire(
        &keyspace,
        key,
        100,
        AdvisoryLockMode::Exclusive,
        AdvisoryLockScope::Session
    ));

    let mgr_waiter = mgr.clone();
    let keyspace_waiter = keyspace.clone();
    let waiter = tokio::spawn(async move {
        mgr_waiter
            .acquire(
                &keyspace_waiter,
                key,
                200,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
                Some(Duration::from_millis(20)),
            )
            .await
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let has_waiter_clone = {
            let state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .key_notifiers
                .get(&lk)
                .is_some_and(|notify| Arc::strong_count(notify) >= 2)
        };
        if has_waiter_clone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "waiter did not register key notifier clone in time"
        );
        tokio::task::yield_now().await;
    }

    {
        let mut state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
        state.locks.remove(&lk);
        state.connection_keys.remove(&100);
        assert!(state.key_notifiers.contains_key(&lk));
    }

    let err = waiter
        .await
        .expect("waiter task should not panic")
        .expect_err("waiter should time out");
    assert_eq!(err, AcquireError::Timeout);

    let state = mgr.state.lock().unwrap_or_else(|e| e.into_inner());
    assert!(!state.locks.contains_key(&lk));
    assert!(
        !state.key_notifiers.contains_key(&lk),
        "timeout error path should cleanup orphan notifier entries"
    );
}
