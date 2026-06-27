use super::{
    cast_current_setting_value, get_skip_reason, get_unsupported_reason,
    is_current_setting_function, is_set_config_function, normalize_search_path_entries,
    parse_search_path_guc_value, set_variable_value_to_string, split_sql_statements,
    starts_with_ignore_ascii_case, try_parse_const_bool, try_parse_const_text,
    unwrap_top_level_cast,
};
use crate::model::Value;
use sqlparser::ast::{
    DataType, DateTimeField, Expr, Ident, Interval, ObjectName, Value as SqlValue,
};

#[test]
fn test_execute_statement_on_txn_signature_stays_boxed() {
    #[allow(clippy::type_complexity)]
    let _execute_statement_on_txn: for<'a> fn(
        &'a super::Executor,
        &'a mut tikv_client::Transaction,
        u64,
        &'a mut crate::sql::sequences::SequenceSession,
        &'a [String],
        &'a sqlparser::ast::Statement,
        Option<&'a str>,
        Option<&'a str>,
    ) -> super::BoxStmtFuture<'a> = super::Executor::execute_statement_on_txn;
}

#[test]
fn test_starts_with_ignore_ascii_case_is_byte_safe() {
    assert!(starts_with_ignore_ascii_case("rollback;", "ROLLBACK"));
    assert!(!starts_with_ignore_ascii_case("é💩€ROLLBACK", "ROLLBACK"));
}

#[test]
fn test_split_sql_statements_basic() {
    let sql = "CREATE EXTENSION http; SELECT 1;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements, vec!["CREATE EXTENSION http", "SELECT 1"]);
}

#[test]
fn test_split_sql_statements_ignores_semicolons_in_strings_and_comments() {
    let sql = "SELECT ';' as s; /* ; */ SELECT 1; -- ;\nSELECT 2;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| super::strip_leading_sql_comments(s).trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements, vec!["SELECT ';' as s", "SELECT 1", "SELECT 2"]);
}

#[test]
fn test_split_sql_statements_ignores_semicolons_in_dollar_quoted_strings() {
    let sql = "CREATE FUNCTION f() RETURNS void AS $$ BEGIN RAISE NOTICE 'x; y'; END; $$ LANGUAGE plpgsql; SELECT 1;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements.len(), 2);
    assert!(statements[0].starts_with("CREATE FUNCTION"));
    assert!(statements[0].contains("RAISE NOTICE 'x; y';"));
    assert!(statements[0].contains("END; $$"));
    assert_eq!(statements[1], "SELECT 1");
}

#[test]
fn test_split_sql_statements_ignores_semicolons_in_create_procedure_body() {
    let sql = "CREATE PROCEDURE p() AS BEGIN\nSELECT 1;\nSELECT 2\nEND; SELECT 3;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements.len(), 2);
    assert!(statements[0].starts_with("CREATE PROCEDURE"));
    assert!(statements[0].contains("SELECT 1;"));
    assert!(statements[0].contains("SELECT 2"));
    assert!(statements[0].contains("\nEND"));
    assert_eq!(statements[1], "SELECT 3");
}

#[test]
fn test_split_sql_statements_create_procedure_does_not_break_on_case_end() {
    let sql =
        "CREATE PROCEDURE p() AS BEGIN\nSELECT CASE WHEN 1=1 THEN 1 ELSE 2 END;\nSELECT 2;\nEND; SELECT 3;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements.len(), 2);
    assert!(statements[0].starts_with("CREATE PROCEDURE"));
    assert!(statements[0].contains("CASE WHEN"));
    assert!(statements[0].contains("ELSE 2 END;"));
    assert!(statements[0].contains("\nEND"));
    assert_eq!(statements[1], "SELECT 3");
}

#[test]
fn test_split_sql_statements_ignores_semicolons_in_create_or_replace_procedure_body() {
    let sql = "CREATE OR REPLACE PROCEDURE p() AS BEGIN\nSELECT 1;\nEND; SELECT 2;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements.len(), 2);
    assert!(statements[0].starts_with("CREATE OR REPLACE PROCEDURE"));
    assert!(statements[0].contains("SELECT 1;"));
    assert!(statements[0].contains("\nEND"));
    assert_eq!(statements[1], "SELECT 2");
}

#[test]
fn test_split_sql_statements_does_not_treat_identifier_dollars_as_dollar_quotes() {
    for sql in [
        "COMMENT ON TABLE a$$ IS 'x'; SELECT 1;",
        "COMMENT ON TABLE a$tag$ IS 'x'; SELECT 1;",
        "COMMENT ON TABLE 租户$$ IS 'x'; SELECT 1;",
    ] {
        let statements: Vec<&str> = split_sql_statements(sql)
            .into_iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            statements,
            vec![sql.split(';').next().unwrap().trim(), "SELECT 1"]
        );
    }
}

#[test]
fn test_split_sql_statements_ignores_semicolons_in_escape_string_literals() {
    let sql = r"SELECT E'it\'s fine; really' AS semi; SELECT 1;";
    let statements: Vec<&str> = split_sql_statements(sql)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(statements.len(), 2);
    assert!(statements[0].contains("fine; really"));
    assert_eq!(statements[1], "SELECT 1");
}

#[test]
fn test_set_variable_value_to_string_basic() {
    assert_eq!(
        set_variable_value_to_string(&[Expr::Value(SqlValue::Number("0".to_string(), false))])
            .unwrap(),
        "0"
    );
    assert_eq!(
        set_variable_value_to_string(&[Expr::Value(SqlValue::SingleQuotedString(
            "UTF8".to_string()
        ))])
        .unwrap(),
        "UTF8"
    );
    assert_eq!(
        set_variable_value_to_string(&[Expr::Value(SqlValue::Boolean(false))]).unwrap(),
        "off"
    );
    assert_eq!(
        set_variable_value_to_string(&[Expr::Identifier(Ident::new("on"))]).unwrap(),
        "on"
    );
    assert_eq!(
        set_variable_value_to_string(&[Expr::Identifier(Ident::new("OFF"))]).unwrap(),
        "off"
    );
}

#[test]
fn test_set_variable_value_to_string_timezone_interval() {
    let expr = Expr::Interval(Interval {
        value: Box::new(Expr::Value(SqlValue::SingleQuotedString(
            "+00:00".to_string(),
        ))),
        leading_field: Some(DateTimeField::Hour),
        leading_precision: None,
        last_field: Some(DateTimeField::Minute),
        fractional_seconds_precision: None,
    });
    assert_eq!(set_variable_value_to_string(&[expr]).unwrap(), "+00:00");
}

#[test]
fn test_parse_search_path_guc_value() {
    let parsed = parse_search_path_guc_value("public, \"$user\", \"MySchema\", foo");
    assert_eq!(
        parsed,
        vec![
            "public".to_string(),
            "$user".to_string(),
            "MySchema".to_string(),
            "foo".to_string()
        ]
    );
}

#[test]
fn test_normalize_search_path_entries_preserves_user_placeholder() {
    let normalized = normalize_search_path_entries(vec![
        "public".to_string(),
        "$user".to_string(),
        "myschema".to_string(),
    ])
    .unwrap();
    assert_eq!(
        normalized,
        vec![
            "public".to_string(),
            "$user".to_string(),
            "myschema".to_string()
        ]
    );
}

#[test]
fn test_normalize_search_path_entries_default_keyword() {
    let normalized = normalize_search_path_entries(vec!["default".to_string()]).unwrap();
    assert_eq!(normalized, vec!["$user".to_string(), "public".to_string()]);
}

#[test]
fn test_try_parse_const_text_and_bool() {
    let expr = Expr::Value(SqlValue::SingleQuotedString("search_path".to_string()));
    assert_eq!(try_parse_const_text(&expr).as_deref(), Some("search_path"));

    let cast_expr = Expr::Cast {
        expr: Box::new(Expr::Value(SqlValue::SingleQuotedString("x".to_string()))),
        data_type: DataType::Text,
        format: None,
    };
    assert_eq!(try_parse_const_text(&cast_expr).as_deref(), Some("x"));

    assert_eq!(
        try_parse_const_bool(&Expr::Identifier(Ident::new("true"))),
        Some(true)
    );
    assert_eq!(
        try_parse_const_bool(&Expr::Value(SqlValue::Boolean(false))),
        Some(false)
    );
}

#[test]
fn test_is_set_config_function() {
    assert!(is_set_config_function(&ObjectName(vec![Ident::new(
        "set_config"
    )])));
    assert!(is_set_config_function(&ObjectName(vec![
        Ident::new("pg_catalog"),
        Ident::new("set_config")
    ])));
    assert!(!is_set_config_function(&ObjectName(vec![Ident::new(
        "other"
    )])));
}

#[test]
fn test_is_current_setting_function() {
    assert!(is_current_setting_function(&ObjectName(vec![Ident::new(
        "current_setting"
    )])));
    assert!(is_current_setting_function(&ObjectName(vec![
        Ident::new("pg_catalog"),
        Ident::new("current_setting")
    ])));
    assert!(!is_current_setting_function(&ObjectName(vec![Ident::new(
        "other"
    )])));
}

#[test]
fn test_unwrap_top_level_cast() {
    let inner = Expr::Identifier(Ident::new("x"));
    let expr = Expr::Cast {
        expr: Box::new(Expr::Nested(Box::new(inner.clone()))),
        data_type: DataType::Int(None),
        format: None,
    };
    let (unwrapped, cast_to) = unwrap_top_level_cast(&expr);
    assert!(matches!(unwrapped, Expr::Identifier(_)));
    assert!(cast_to.is_some());
}

#[test]
fn test_cast_current_setting_value_integer() {
    let v = cast_current_setting_value(
        Value::Text("160000".to_string()),
        &crate::model::DataType::Int32,
    )
    .unwrap();
    assert_eq!(v, Value::Int32(160000));
}

mod write_conflict_retry_tests {
    use super::super::{extract_write_conflict_reason, is_retryable_tikv_error};
    use crate::sql::error::SqlError;
    use crate::storage::{StorageError, WriteConflictReason};
    use anyhow::Context;

    #[test]
    fn test_unrelated_tikv_error_not_retryable() {
        let tikv_err = tikv_client::Error::DuplicateKeyInsertion;
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_non_tikv_error_not_retryable() {
        let anyhow_err = anyhow::anyhow!("some random error");
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_region_error_not_retryable() {
        let tikv_err = tikv_client::Error::RegionForKeyNotFound { key: vec![1, 2, 3] };
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_operation_after_commit_not_retryable() {
        let tikv_err = tikv_client::Error::OperationAfterCommitError;
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_pessimistic_lock_with_non_conflict_inner_not_retryable() {
        let inner_err = tikv_client::Error::DuplicateKeyInsertion;
        let tikv_err = tikv_client::Error::PessimisticLockError {
            inner: Box::new(inner_err),
            success_keys: vec![],
        };
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_undetermined_with_non_conflict_inner_not_retryable() {
        let inner_err = tikv_client::Error::DuplicateKeyInsertion;
        let tikv_err = tikv_client::Error::UndeterminedError(Box::new(inner_err));
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_multiple_key_errors_all_non_conflict_not_retryable() {
        let err1 = tikv_client::Error::DuplicateKeyInsertion;
        let err2 = tikv_client::Error::NoPrimaryKey;
        let tikv_err = tikv_client::Error::MultipleKeyErrors(vec![err1, err2]);
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_extracted_errors_all_non_conflict_not_retryable() {
        let err1 = tikv_client::Error::DuplicateKeyInsertion;
        let tikv_err = tikv_client::Error::ExtractedErrors(vec![err1]);
        let anyhow_err = anyhow::Error::new(tikv_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_nested_pessimistic_with_non_conflict_not_retryable() {
        let inner_err = tikv_client::Error::NoPrimaryKey;
        let multi_err = tikv_client::Error::MultipleKeyErrors(vec![inner_err]);
        let pessimistic_err = tikv_client::Error::PessimisticLockError {
            inner: Box::new(multi_err),
            success_keys: vec![],
        };
        let anyhow_err = anyhow::Error::new(pessimistic_err);
        assert!(!is_retryable_tikv_error(&anyhow_err));
    }

    #[test]
    fn test_storage_write_conflict_retryable() {
        let optimistic = anyhow::Error::new(StorageError::WriteConflict {
            reason: WriteConflictReason::Optimistic,
        });
        assert!(is_retryable_tikv_error(&optimistic));
        assert_eq!(extract_write_conflict_reason(&optimistic), Some(1));

        let pessimistic = anyhow::Error::new(StorageError::WriteConflict {
            reason: WriteConflictReason::Pessimistic,
        });
        assert!(is_retryable_tikv_error(&pessimistic));
        assert_eq!(extract_write_conflict_reason(&pessimistic), Some(2));
    }

    #[test]
    fn test_retryable_tikv_error_survives_sql_error_context_chain() {
        let tikv_err =
            tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
                conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict {
                    reason: 2,
                    ..Default::default()
                }),
                ..Default::default()
            }));
        let sql_err = SqlError::Internal(anyhow::Error::new(tikv_err));
        let anyhow_err = Err::<(), SqlError>(sql_err)
            .context("failed to write HNSW delta")
            .unwrap_err();

        assert!(is_retryable_tikv_error(&anyhow_err));
        assert_eq!(extract_write_conflict_reason(&anyhow_err), Some(2));
    }

    #[test]
    fn test_storage_deadlock_retryable() {
        let anyhow_err = anyhow::Error::new(StorageError::Deadlock);
        assert!(is_retryable_tikv_error(&anyhow_err));
        assert_eq!(extract_write_conflict_reason(&anyhow_err), None);
    }

    #[test]
    fn test_storage_lock_conflict_not_retryable() {
        let anyhow_err = anyhow::Error::new(StorageError::LockConflict);
        assert!(!is_retryable_tikv_error(&anyhow_err));
        assert_eq!(extract_write_conflict_reason(&anyhow_err), None);
    }

    #[test]
    fn test_storage_capability_unavailable_not_retryable() {
        let anyhow_err = anyhow::Error::new(StorageError::CapabilityUnavailable("db9_cop"));
        assert!(!is_retryable_tikv_error(&anyhow_err));
        assert_eq!(extract_write_conflict_reason(&anyhow_err), None);
    }
}

#[test]
fn test_get_skip_reason() {
    assert!(get_skip_reason("DROP DATABASE test").is_none());
    assert!(get_skip_reason("CREATE DATABASE test").is_none());
    assert!(get_skip_reason("SELECT * FROM foo").is_none());
}

#[test]
fn test_get_unsupported_reason() {
    assert!(get_unsupported_reason("CREATE DOMAIN foo").is_some());
    assert!(get_unsupported_reason("SELECT * FROM foo").is_none());
    assert!(get_unsupported_reason("SELECT $$abc$$").is_none());
    assert!(get_unsupported_reason("SELECT $tag$abc$tag$").is_none());
}

#[test]
fn test_ephemeral_table_id_detection() {
    assert!(super::is_ephemeral_table_id(0));
    assert!(!super::is_ephemeral_table_id(1));
    assert!(!super::is_ephemeral_table_id(42));
}

#[test]
fn test_executor_getters_and_trigger_buffers() {
    let store = crate::storage::TikvStore::new_stub();
    let keyspace = "core_tests_executor_getters".to_string();
    let observability = crate::observability::registry().tenant(&keyspace);
    let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
    let rls_policy_cache = std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
    let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
    let executor = super::Executor::new(
        store.clone(),
        keyspace.clone(),
        observability.clone(),
        crate::pool::TenantMemoryAccountant::unlimited("core_tests".to_string()),
        trigger_cache.clone(),
        rls_policy_cache,
        stats_cache.clone(),
    );

    assert_eq!(executor.tenant_keyspace(), keyspace);
    assert!(std::sync::Arc::ptr_eq(&executor.store(), &store));
    assert!(std::sync::Arc::ptr_eq(
        executor.observability(),
        &observability
    ));
    assert!(std::sync::Arc::ptr_eq(
        executor.trigger_cache(),
        &trigger_cache
    ));
    assert!(std::sync::Arc::ptr_eq(executor.stats_cache(), &stats_cache));
    let _ = executor.auth_manager();
    let _ = executor.tenant_memory_accountant();

    assert_eq!(executor.pending_async_triggers.lock().unwrap().len(), 0);
    executor.push_pending_async_trigger(super::PendingAsyncTrigger {
        keyspace: "ks".to_string(),
        db_id: 1,
        command: "SELECT 1".to_string(),
    });
    assert_eq!(executor.pending_async_triggers.lock().unwrap().len(), 1);

    executor.clear_trigger_activations();
    assert_eq!(executor.pending_async_triggers.lock().unwrap().len(), 0);
}

#[test]
fn test_flush_trigger_activations_clears_buffers_even_without_system_store() {
    let store = crate::storage::TikvStore::new_stub();
    let keyspace = "core_tests_flush_trigger".to_string();
    let observability = crate::observability::registry().tenant(&keyspace);
    let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
    let rls_policy_cache = std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
    let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
    let executor = super::Executor::new(
        store,
        keyspace,
        observability,
        crate::pool::TenantMemoryAccountant::unlimited("core_tests".to_string()),
        trigger_cache,
        rls_policy_cache,
        stats_cache,
    );

    executor
        .pending_trigger_activations
        .lock()
        .unwrap()
        .insert("ks1".to_string());
    executor.push_pending_async_trigger(super::PendingAsyncTrigger {
        keyspace: "ks1".to_string(),
        db_id: 1,
        command: "SELECT 1".to_string(),
    });
    assert_eq!(executor.pending_async_triggers.lock().unwrap().len(), 1);
    assert_eq!(
        executor.pending_trigger_activations.lock().unwrap().len(),
        1
    );

    executor.flush_trigger_activations();

    assert_eq!(executor.pending_async_triggers.lock().unwrap().len(), 0);
    assert_eq!(
        executor.pending_trigger_activations.lock().unwrap().len(),
        0
    );
}

#[test]
fn test_hnsw_merge_task_id_is_stable_and_unique_for_common_pairs() {
    use crate::sql::hnsw::storage::hnsw_merge_task_id;
    let a = hnsw_merge_task_id(1, 1).unwrap();
    let b = hnsw_merge_task_id(1, 2).unwrap();
    let c = hnsw_merge_task_id(2, 1).unwrap();
    let d = hnsw_merge_task_id(2, 2).unwrap();

    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(a, d);
    assert_ne!(b, c);
    assert_ne!(b, d);
    assert_ne!(c, d);

    // Deterministic encoding: same pair must always produce same task_id.
    assert_eq!(a, hnsw_merge_task_id(1, 1).unwrap());
    assert_eq!(d, hnsw_merge_task_id(2, 2).unwrap());
}

#[test]
fn test_flush_pending_hnsw_merges_clears_buffer_without_system_store() {
    let store = crate::storage::TikvStore::new_stub();
    let keyspace = "core_tests_hnsw_merge_flush".to_string();
    let observability = crate::observability::registry().tenant(&keyspace);
    let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
    let rls_policy_cache = std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
    let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
    let executor = super::Executor::new(
        store,
        keyspace.clone(),
        observability,
        crate::pool::TenantMemoryAccountant::unlimited("core_tests".to_string()),
        trigger_cache,
        rls_policy_cache,
        stats_cache,
    );

    executor.push_pending_hnsw_merge(super::PendingHnswMerge {
        keyspace: keyspace.clone(),
        db_id: 1,
        table_id: 42,
        index_id: 7,
    });
    executor.push_pending_hnsw_merge(super::PendingHnswMerge {
        keyspace,
        db_id: 1,
        table_id: 42,
        index_id: 8,
    });
    assert_eq!(executor.pending_hnsw_merges.lock().unwrap().len(), 2);

    // Even if system_store is unavailable in unit-test env, flush must
    // drain the pending buffer to avoid stale accumulation.
    executor.flush_pending_hnsw_merges();
    assert_eq!(executor.pending_hnsw_merges.lock().unwrap().len(), 0);
}

#[test]
fn flush_pending_hnsw_merges_is_dropped_db_tombstone_fenced() {
    let source = include_str!("mod.rs");
    let flush_fn = source
        .split("pub(crate) fn flush_pending_hnsw_merges(&self)")
        .nth(1)
        .and_then(|rest| {
            rest.split("/// Mark that the `is_initialized` cache")
                .next()
        })
        .expect("flush_pending_hnsw_merges must exist");
    let fence_pos = flush_fn
        .find("dropped_db_tombstone_exists_for_update")
        .expect("HNSW merge producer must fence on dropped-DB tombstone");
    let put_pos = flush_fn
        .find("put_singleton_task_v2(&mut txn, &entry, fire_time_ms)")
        .expect("HNSW merge producer must write singleton queue row");
    let registry_pos = flush_fn
        .find("update_registry_task_types")
        .expect("HNSW merge producer must update registry");

    assert!(
        fence_pos < put_pos && put_pos < registry_pos,
        "HNSW merge queue and registry writes must happen after the tombstone fence"
    );
}

#[test]
fn storage_dirty_producer_path_is_removed_from_executor() {
    let source = include_str!("mod.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("core/mod.rs must contain #[cfg(test)]");
    assert!(
        !prod_source.contains("pending_storage_dirty_dbs")
            && !prod_source.contains("note_storage_dirty_if_tables_changed")
            && !prod_source.contains("flush_pending_storage_dirty")
            && !prod_source.contains("mark_storage_size_dirty"),
        "executor must not produce storage dirty markers; interval PD refresh is the automatic path"
    );
}

// ── Lock resolution backoff tests (#2156) ──────────────────

#[test]
fn lock_backoff_budget_does_not_exceed_20s() {
    use tikv_client::backoff::PESSIMISTIC_BACKOFF;

    let mut b = PESSIMISTIC_BACKOFF.clone();
    let mut total_ms = 0u64;
    let mut count = 0u32;
    while let Some(d) = b.next_delay_duration() {
        total_ms += d.as_millis() as u64;
        count += 1;
    }
    assert!(total_ms <= 20_000, "total {total_ms}ms exceeds 20s budget");
    assert!(
        total_ms >= 19_000,
        "total {total_ms}ms too far below 20s budget"
    );
    assert!(
        count > 10,
        "should use >10 attempts with budget, got {count}"
    );
}

#[test]
fn lock_backoff_budget_clamps_last_delay() {
    use tikv_client::backoff::Backoff;

    // Budget=10ms, base=2, cap=3000
    let mut b = Backoff::no_jitter_backoff_with_budget(2, 3000, 10);
    let d1 = b.next_delay_duration().unwrap().as_millis() as u64; // 2
    let d2 = b.next_delay_duration().unwrap().as_millis() as u64; // 4
    let d3 = b.next_delay_duration().unwrap().as_millis() as u64; // clamped to 4 (remaining)
    assert_eq!(d1, 2);
    assert_eq!(d2, 4);
    assert_eq!(d3, 4); // 10 - 2 - 4 = 4
    assert!(b.next_delay_duration().is_none()); // budget exhausted
}

#[test]
fn lock_backoff_count_based_unchanged() {
    use tikv_client::backoff::Backoff;

    // Count-based (max_total_ms=0) still works as before
    let mut b = Backoff::no_jitter_backoff(2, 500, 3);
    assert!(b.next_delay_duration().is_some());
    assert!(b.next_delay_duration().is_some());
    assert!(b.next_delay_duration().is_some());
    assert!(b.next_delay_duration().is_none());
}

// ── #2627 producer durability: decouple producers from local execution ──────
//
// TiKV-backed (CI `integration-tests` job, run with `-- --ignored`).
//
// Acceptance A3(ii): with local worker execution DISABLED
// (`execution_enabled() == false`, i.e. a SQL node whose `WorkerConfig.enabled =
// false`), the HNSW-merge, auto-ANALYZE, and async-trigger producers must STILL
// enqueue a durable V2 queue row via the always-on system store, so an
// execution-enabled node in the fleet performs the work.
//
// This drives the REAL producers — `flush_pending_hnsw_merges`,
// `maybe_enqueue_auto_analyze`, `flush_trigger_activations` — through a
// constructed `Executor`, NOT the storage primitives they call. A regression
// that adds `if !execution_enabled() { return; }` to any of the three producers
// would make this test fail, which the previous primitive-level test could not
// catch.

#[cfg(test)]
async fn producer_durability_live_system_store() -> std::sync::Arc<crate::storage::TikvStore> {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_producer_durable_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace,
        ..Default::default()
    };
    let store = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
        .await
        .expect("init live system store");
    // Register as the process-global system store the producers read via
    // `crate::worker::system_store()`. The OnceLock is set-once; if a prior test
    // in this binary already set one, that pre-existing live store is used
    // instead — which is fine, because read-back below also goes through
    // `crate::worker::system_store()`, and each producer writes under a UNIQUE
    // entry keyspace so rows never collide with another test's store contents.
    crate::worker::set_system_store(store.clone());
    crate::worker::system_store()
        .expect("a live system store must be registered for the producers")
        .clone()
}

/// Poll the system store until exactly one identity-index row exists for the
/// task, returning it. Producers spawn their durable write on a detached task,
/// so the row appears asynchronously.
#[cfg(test)]
async fn await_one_index_row(
    store: &crate::storage::TikvStore,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
    task_type: crate::worker::types::TaskType,
    what: &str,
) {
    for _ in 0..200 {
        let mut txn = store.begin().await.expect("begin");
        let rows = store
            .index_rows_for_task(&mut txn, keyspace, db_id, task_id, task_type)
            .await
            .expect("scan identity index");
        txn.rollback().await.ok();
        if rows.len() == 1 {
            return;
        }
        assert!(
            rows.len() <= 1,
            "{what}: producer must enqueue exactly one V2 row, found {}",
            rows.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{what}: producer did not enqueue a durable V2 row within the deadline");
}

#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn producers_enqueue_v2_row_even_when_worker_execution_disabled() {
    use crate::worker::types::TaskType;

    let store = producer_durability_live_system_store().await;
    let db_id = 1u64;

    // The execution flag is process-global; save/restore so this test does not
    // leak state into others sharing the binary.
    let prev_enabled = crate::worker::execution_enabled();
    crate::worker::set_worker_execution_enabled(false);
    assert!(
        !crate::worker::execution_enabled(),
        "execution must be disabled to exercise the decouple-from-execution contract"
    );

    // Unique per-producer entry keyspaces so the durable rows are isolated from
    // every other test's contents in the shared system store.
    let merge_ks = format!(
        "ks_prod_merge_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let analyze_ks = format!(
        "ks_prod_analyze_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let trigger_ks = format!(
        "ks_prod_trigger_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );

    let merge_table_id = 4242u64;
    let merge_index_id = 7u64;
    let merge_task_id =
        crate::sql::hnsw::storage::hnsw_merge_task_id(merge_table_id, merge_index_id)
            .expect("deterministic hnsw merge task_id");
    let analyze_table_id = 909u64;
    let analyze_task_id = analyze_table_id as i64;

    let outcome: anyhow::Result<()> = async {
        // ── Producer 1: HNSW merge (deterministic singleton via the REAL flush) ──
        {
            let observability = crate::observability::registry().tenant(&merge_ks);
            let executor = super::Executor::new(
                store.clone(),
                merge_ks.clone(),
                observability,
                crate::pool::TenantMemoryAccountant::unlimited("producer_durable".to_string()),
                std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new()),
                std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new()),
                std::sync::Arc::new(crate::sql::stats::TableStatsCache::new()),
            );
            executor.push_pending_hnsw_merge(super::PendingHnswMerge {
                keyspace: merge_ks.clone(),
                db_id,
                table_id: merge_table_id,
                index_id: merge_index_id,
            });
            // The production producer; gated on execution would silently drop it.
            executor.flush_pending_hnsw_merges();
            await_one_index_row(
                &store,
                &merge_ks,
                db_id,
                merge_task_id,
                TaskType::HnswMerge,
                "HNSW-merge",
            )
            .await;
        }

        // ── Producer 2: auto-ANALYZE (task_has_pending-guarded, REAL enqueue) ──
        {
            let observability = crate::observability::registry().tenant(&analyze_ks);
            let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
            let executor = super::Executor::new(
                store.clone(),
                analyze_ks.clone(),
                observability,
                crate::pool::TenantMemoryAccountant::unlimited("producer_durable".to_string()),
                std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new()),
                std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new()),
                stats_cache.clone(),
            );
            // Push the modification count above the auto-ANALYZE threshold so the
            // producer actually decides to enqueue (threshold base = 50).
            stats_cache.bump_mod_count(db_id, analyze_table_id, 10_000);
            assert!(
                stats_cache.needs_auto_analyze(db_id, analyze_table_id, 50),
                "stats priming must put the table over the auto-ANALYZE threshold"
            );
            // The production producer chooses keyspace = tenant_keyspace() and
            // task_id = table_id; both match what await_one_index_row reads back.
            executor.maybe_enqueue_auto_analyze(db_id, analyze_table_id, "t");
            await_one_index_row(
                &store,
                &analyze_ks,
                db_id,
                analyze_task_id,
                TaskType::AutoAnalyze,
                "auto-ANALYZE",
            )
            .await;
        }

        // ── Producer 3: async-trigger flush (plain put, REAL flush) ──
        let trigger_task_id;
        {
            let observability = crate::observability::registry().tenant(&trigger_ks);
            let executor = super::Executor::new(
                store.clone(),
                trigger_ks.clone(),
                observability,
                crate::pool::TenantMemoryAccountant::unlimited("producer_durable".to_string()),
                std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new()),
                std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new()),
                std::sync::Arc::new(crate::sql::stats::TableStatsCache::new()),
            );
            executor.push_pending_async_trigger(super::PendingAsyncTrigger {
                keyspace: trigger_ks.clone(),
                db_id,
                command: "SELECT extensions.http_get('http://x')".to_string(),
            });
            executor.flush_trigger_activations();
            // The trigger producer derives task_id from wall-clock ms, so discover
            // it by scanning the (keyspace, db_id, task_type) index instead.
            let mut found = None;
            for _ in 0..200 {
                let mut txn = store.begin().await?;
                let rows = store
                    .index_rows_for_db_type(&mut txn, &trigger_ks, db_id, TaskType::AsyncTrigger)
                    .await?;
                txn.rollback().await.ok();
                if rows.len() == 1 {
                    found = Some(rows[0].task_id);
                    break;
                }
                assert!(
                    rows.len() <= 1,
                    "async-trigger producer must enqueue exactly one V2 row, found {}",
                    rows.len()
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            trigger_task_id =
                found.expect("async-trigger producer did not enqueue a durable V2 row");
        }

        // Producer/consumer are decoupled: every durable producer row is visible
        // to the V2-only consumer scan, so an execution-enabled node dequeues it.
        let mut txn = store.begin().await?;
        let due = store.scan_due_v2(&mut txn, i64::MAX, 10_000).await?;
        txn.rollback().await.ok();
        for (ks, tid, tt) in [
            (&merge_ks, merge_task_id, TaskType::HnswMerge),
            (&analyze_ks, analyze_task_id, TaskType::AutoAnalyze),
            (&trigger_ks, trigger_task_id, TaskType::AsyncTrigger),
        ] {
            assert!(
                due.iter()
                    .any(|(_, d)| &d.keyspace == ks && d.task_id == tid && d.task_type == tt),
                "durable producer row ({ks}, {tid}, {tt:?}) must be visible to the V2 consumer scan"
            );
        }

        // Cleanup.
        for (ks, tid, tt) in [
            (&merge_ks, merge_task_id, TaskType::HnswMerge),
            (&analyze_ks, analyze_task_id, TaskType::AutoAnalyze),
            (&trigger_ks, trigger_task_id, TaskType::AsyncTrigger),
        ] {
            let mut txn = store.begin().await?;
            store
                .delete_task_all_layers(&mut txn, ks, db_id, tid, tt)
                .await?;
            txn.commit().await?;
        }
        Ok(())
    }
    .await;

    // Restore the global flag regardless of assertion outcome.
    crate::worker::set_worker_execution_enabled(prev_enabled);
    outcome.expect("producer-durability assertions must pass");
}
