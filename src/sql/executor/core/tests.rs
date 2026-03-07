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
    use super::super::is_retryable_tikv_error;

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
    let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
    let executor = super::Executor::new(
        store.clone(),
        keyspace.clone(),
        observability.clone(),
        crate::pool::TenantMemoryAccountant::unlimited("core_tests".to_string()),
        trigger_cache.clone(),
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
    let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
    let executor = super::Executor::new(
        store,
        keyspace,
        observability,
        crate::pool::TenantMemoryAccountant::unlimited("core_tests".to_string()),
        trigger_cache,
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
    let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
    let executor = super::Executor::new(
        store,
        keyspace.clone(),
        observability,
        crate::pool::TenantMemoryAccountant::unlimited("core_tests".to_string()),
        trigger_cache,
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
