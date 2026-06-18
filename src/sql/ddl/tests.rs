//! Unit tests for DDL helpers and utility functions.

use super::*;
use crate::worker::types::IndexState;
use std::sync::Arc;

fn parse_expr(sql: &str) -> sqlparser::ast::Expr {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let sql = format!("SELECT {sql}");
    let ast = Parser::parse_sql(&PostgreSqlDialect {}, &sql).unwrap();
    let sqlparser::ast::Statement::Query(query) = ast.into_iter().next().unwrap() else {
        panic!("expected query");
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        panic!("expected select");
    };
    let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) = select.projection.into_iter().next()
    else {
        panic!("expected expression projection");
    };
    expr
}

#[test]
fn serial_resolves_in_ddl_column_context_only() {
    use crate::sql::types::{resolve_custom_type, TypeResolutionContext};
    use sqlparser::ast::{Ident, ObjectName};

    let name = ObjectName(vec![Ident::new("SERIAL")]);

    // In DdlColumn context, serial expands to Int32.
    let (dt, is_serial) =
        resolve_custom_type(TypeResolutionContext::DdlColumn, &name, &[], None).unwrap();
    assert_eq!(dt, DataType::Int32);
    assert!(is_serial);

    // In DDL-nested and NonDdl contexts, serial is not special.
    let err = resolve_custom_type(TypeResolutionContext::DdlOther, &name, &[], None);
    assert!(err.is_err());
    let err = resolve_custom_type(TypeResolutionContext::NonDdl, &name, &[], None);
    assert!(err.is_err());
}

#[test]
fn serial_udt_can_resolve_in_non_column_ddl_context() {
    use crate::sql::types::{resolve_custom_type, TypeResolutionContext};
    use sqlparser::ast::{Ident, ObjectName};

    let name = ObjectName(vec![Ident::new("serial")]);
    let (dt, is_serial) = resolve_custom_type(
        TypeResolutionContext::DdlOther,
        &name,
        &[],
        Some(DataType::UserDefined("serial".to_string())),
    )
    .expect("ALTER COLUMN TYPE serial should resolve catalog UDT, not pseudo-type");

    assert_eq!(dt, DataType::UserDefined("serial".to_string()));
    assert!(!is_serial);
}

#[test]
fn create_table_default_current_timestamp_precision_is_preserved() {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let dialect = PostgreSqlDialect {};
    let ast = Parser::parse_sql(
        &dialect,
        "CREATE TABLE ts_precision (id INT PRIMARY KEY, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP(0));",
    )
    .unwrap();

    let sqlparser::ast::Statement::CreateTable { columns, .. } = &ast[0] else {
        panic!("expected CREATE TABLE");
    };

    let ts_col = columns
        .iter()
        .find(|c| c.name.value.eq_ignore_ascii_case("ts"))
        .expect("ts column must exist");

    let mut default_expr = None;
    for opt in &ts_col.options {
        if let sqlparser::ast::ColumnOption::Default(expr) = &opt.option {
            if let sqlparser::ast::Expr::Function(func) = expr {
                assert_eq!(func.args.len(), 1);
            }
            default_expr = Some(expr.to_string());
        }
    }

    assert_eq!(default_expr.unwrap(), "CURRENT_TIMESTAMP(0)");
}

#[test]
fn check_expr_reference_ignores_string_literals() {
    assert!(!check_expr_references_column("note = 'age'", "age").unwrap());
    assert!(check_expr_references_column("age > 0 AND note = 'age'", "age").unwrap());
}

#[test]
fn index_prefix_range_includes_all_index_entries() {
    let (start, end) = index_prefix_range(5, 42, 7);

    for suffix in [
        &[0x00][..],
        &[0x01][..],
        &[0x5F][..],
        &[0x60][..],
        &[0x7F][..],
        &[0xFF][..],
        &[0xFF, 0x00][..],
    ] {
        let mut key = start.clone();
        key.extend_from_slice(suffix);
        assert!(key >= start);
        assert!(key < end);
    }

    let (next_start, _) = index_prefix_range(5, 42, 8);
    assert!(next_start >= end);
}

#[test]
fn rewrite_check_expr_column_rewrites_identifiers_only() {
    let out = rewrite_check_expr_column(
        "age > 0 AND note = 'age' AND t.age < 10",
        "age",
        "years",
        None,
    )
    .unwrap();
    assert!(out.contains("years > 0"));
    assert!(out.contains("t.years"));
    assert!(out.contains("'age'"));
}

#[test]
fn rewrite_check_expr_column_respects_quote_style() {
    let out = rewrite_check_expr_column("age > 0", "age", "Years", Some('"')).unwrap();
    assert!(out.contains("\"Years\""));
}

// --- has_legacy_name_conflict tests ---

fn test_index(name: &str) -> IndexDef {
    IndexDef {
        name: name.to_string(),
        id: 1,
        columns: vec!["col1".to_string()],
        unique: false,
        is_constraint: false,
        method: None,
        predicate: None,
        expressions: vec![],
        state: IndexState::Ready,
        cached_predicate_conjuncts: None,
        deferrable: false,
        initially_deferred: false,
        hnsw_m: None,
        hnsw_ef_construction: None,
        hnsw_distance_metric: None,
    }
}

fn test_schema_with_index(table_name: &str, idx_name: &str) -> (String, TableSchema) {
    let mut schema = TableSchema::new(table_name.to_string(), 1, vec![], vec![]);
    schema.indexes.push(test_index(idx_name));
    (table_name.to_string(), schema)
}

fn test_schema_with_pk(table_name: &str, pk_name: Option<&str>) -> (String, TableSchema) {
    let mut schema = TableSchema::new(table_name.to_string(), 1, vec![], vec![0]);
    schema.pk_constraint_name = pk_name.map(|n| n.to_string());
    (table_name.to_string(), schema)
}

#[test]
fn test_legacy_scan_detects_index_conflict() {
    let schemas = [test_schema_with_index("public.t1", "idx_shared")];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_shared",
        None,
    ));
}

#[test]
fn test_legacy_scan_detects_explicit_pk() {
    let schemas = [test_schema_with_pk("public.t1", Some("my_pk"))];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "my_pk",
        None,
    ));
}

#[test]
fn test_legacy_scan_detects_default_pk() {
    // pk_constraint_name = None, pk_indices = [0] -> effective name = "t1_pkey"
    let schemas = [test_schema_with_pk("public.t1", None)];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "t1_pkey",
        None,
    ));
}

#[test]
fn test_legacy_scan_ignores_other_schema() {
    let schemas = [test_schema_with_index("other.t1", "idx_shared")];
    assert!(!create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_shared",
        None,
    ));
}

#[test]
fn test_legacy_scan_no_conflict() {
    let schemas = [test_schema_with_index("public.t1", "idx_a")];
    assert!(!create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_b",
        None,
    ));
}

#[test]
fn test_legacy_scan_multi_table() {
    let schemas = [
        test_schema_with_index("public.t1", "idx_shared"),
        test_schema_with_index("public.t2", "idx_other"),
    ];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_shared",
        None,
    ));
}

#[test]
fn test_legacy_scan_excludes_owning_table() {
    // When exclude_table matches, the table's own PK should not trigger a conflict.
    // This is the CREATE TABLE scenario: the table was just created with its PK,
    // and the legacy scan must skip it to avoid a false self-conflict.
    let schemas = [test_schema_with_pk("public.t1", Some("t1_pkey"))];
    assert!(!create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "t1_pkey",
        Some("public.t1"),
    ));
}

#[tokio::test]
async fn session_txn_tracker_refresh_overwrites_previous_start_ts() {
    let registry = Arc::new(crate::worker::active_txn_registry::ActiveTxnRegistry::new());
    let tracker = Arc::new(crate::session_context::SessionTxnTracker::new(
        42,
        registry.clone(),
    ));

    crate::session_context::with_session_txn_tracker(Some(tracker), async {
        crate::session_context::current_session_txn_tracker()
            .expect("tracker must be scoped")
            .refresh(100);
        assert_eq!(registry.min_start_ts(), Some(100));

        crate::session_context::current_session_txn_tracker()
            .expect("tracker must still be scoped")
            .refresh(250);
        assert_eq!(registry.min_start_ts(), Some(250));

        crate::session_context::current_session_txn_tracker()
            .expect("tracker must still be scoped")
            .clear();
        assert_eq!(registry.min_start_ts(), None);
    })
    .await;
}

#[test]
fn maybe_rotate_backfill_txn_clears_then_refreshes_session_registration() {
    let source = include_str!("mod.rs");
    let prod_source = source
        .split("// ── Parse foreign key action")
        .next()
        .expect("ddl/mod.rs must contain parse_referential_action marker");
    let rotate_fn = prod_source
        .split("pub(super) async fn maybe_rotate_backfill_txn")
        .nth(1)
        .and_then(|rest| rest.split("pub(super) fn track_active_worker_txn").next())
        .expect("ddl/mod.rs must define maybe_rotate_backfill_txn before track_active_worker_txn");

    let commit_pos = rotate_fn
        .find("txn.commit().await?")
        .expect("rotation helper must commit the old transaction");
    let lease_fence_pos = rotate_fn
        .find("lease_cancel.bail_if_cancelled()?;")
        .expect("rotation helper must fence the claim lease before committing a batch");
    assert!(
        lease_fence_pos < commit_pos,
        "lease fence must precede the per-batch commit so a lost claim abandons the batch uncommitted"
    );
    let fence_pos = rotate_fn
        .find("assert_database_alive_for_update(txn, db_id)")
        .expect("rotation helper must lock/read DB liveness before commit");
    let clear_pos = rotate_fn
        .find("crate::session_context::clear_current_session_txn_registration();")
        .expect("rotation helper must clear the old session registration");
    let begin_pos = rotate_fn
        .find("crate::session_context::begin_replacement_session_owned_txn(store, txn).await?;")
        .expect("rotation helper must start a fresh transaction via the shared helper");
    let worker_guard_pos = rotate_fn
        .find("*txn_guard = track_active_worker_txn(txn);")
        .expect("rotation helper must refresh the worker txn guard for the new transaction");

    assert!(
        fence_pos < commit_pos
            && commit_pos < clear_pos
            && clear_pos < begin_pos
            && begin_pos < worker_guard_pos,
        "rotation helper must fence before commit, clear the old session registration after commit, reopen the session-owned txn via the shared helper, then track the new worker txn"
    );
    assert!(
        !rotate_fn.contains("refresh_active_session_txn_registration(txn);")
            && !rotate_fn.contains("*txn = store.begin().await?;"),
        "rotation helper must not inline session GC re-registration logic outside the shared helper"
    );
}

#[test]
fn test_legacy_scan_exclude_does_not_suppress_other_table() {
    // Excluding t1 should NOT suppress a conflict found on t2.
    let schemas = [
        test_schema_with_index("public.t1", "idx_shared"),
        test_schema_with_index("public.t2", "idx_shared"),
    ];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_shared",
        Some("public.t1"),
    ));
}

#[test]
fn drop_column_stats_invalidation_matches_schema_change() {
    assert!(alter_table::should_invalidate_stats_for_drop_column(true));
    assert!(!alter_table::should_invalidate_stats_for_drop_column(false));
}

#[test]
fn set_data_type_stats_invalidation_matches_type_change() {
    assert!(!alter_table::should_invalidate_stats_for_type_change(
        &DataType::Int32,
        &DataType::Int32
    ));
    assert!(alter_table::should_invalidate_stats_for_type_change(
        &DataType::Int32,
        &DataType::Int64
    ));
}

#[test]
fn coerce_jsonb_to_text_produces_canonical_output() {
    let col = crate::model::ColumnDef::new("data", DataType::Text, true);
    let result =
        coerce_value_for_type_change(Value::Jsonb(r#"{"b":1,"a":2}"#.to_string()), &col).unwrap();
    assert_eq!(result, Value::Text(r#"{"a": 2, "b": 1}"#.to_string()));
}

#[test]
fn coerce_json_to_text_preserves_raw_format() {
    let col = crate::model::ColumnDef::new("data", DataType::Text, true);
    let result =
        coerce_value_for_type_change(Value::Json(r#"{"b":1,"a":2}"#.to_string()), &col).unwrap();
    // JSON preserves the original string verbatim
    assert_eq!(result, Value::Text(r#"{"b":1,"a":2}"#.to_string()));
}

#[test]
fn validate_static_enum_subexpressions_rejects_nested_invalid_enum_casts() {
    use crate::model::UserTypeKind;
    use crate::sql::analyzer::catalog::MockCatalog;

    let catalog = MockCatalog::builder()
        .user_defined_type(
            "public",
            "mood",
            UserTypeKind::Enum {
                labels: vec!["happy".to_string(), "sad".to_string()],
            },
        )
        .build();
    let qctx = QueryContext::from_task_locals();
    let typed = Analyzer::analyze_expr_with_scope(
        &catalog,
        Scope::new(),
        &parse_expr("coalesce(('bogus'::mood)::text, 'fallback')"),
    )
    .unwrap();

    let err = validate_static_enum_subexpressions(&typed, &catalog, &qctx)
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid input value for enum mood: \"bogus\""));
}

#[test]
fn assign_generated_check_names_avoids_existing_constraint_names() {
    let mut checks = vec![
        CheckConstraint {
            name: None,
            expr: "age > 0".to_string(),
        },
        CheckConstraint {
            name: None,
            expr: "age < 200".to_string(),
        },
        CheckConstraint {
            name: Some("already_named".to_string()),
            expr: "score >= 0".to_string(),
        },
    ];
    assign_generated_check_constraint_names(
        "users",
        true,
        &[IndexDef {
            name: "users_age_check".to_string(),
            id: 1,
            columns: vec![],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }],
        &[ForeignKeyConstraint {
            name: "users_age_check1".to_string(),
            columns: vec![],
            ref_table: "public.p".to_string(),
            ref_columns: vec![],
            on_delete: ForeignKeyAction::NoAction,
            on_update: ForeignKeyAction::NoAction,
            deferrable: false,
            initially_deferred: false,
        }],
        &mut checks,
    );

    let c0 = checks[0].name.clone().unwrap();
    let c1 = checks[1].name.clone().unwrap();
    assert_ne!(c0, c1);
    assert!(c0.starts_with("users_age_check"));
    assert!(c1.starts_with("users_age_check"));
    assert_eq!(checks[2].name.as_deref(), Some("already_named"));
}

#[test]
fn extract_first_column_skips_keywords_types_and_literals() {
    assert_eq!(
        extract_first_column_from_check_expr("age > 0 AND name <> ''"),
        Some("age".to_string())
    );
    assert_eq!(
        extract_first_column_from_check_expr("CHECK (text IS NOT NULL)"),
        None
    );
    assert_eq!(
        extract_first_column_from_check_expr("123 > 0 OR true"),
        None
    );
}

#[test]
fn check_constraint_effective_name_and_find_index_work_for_generated_names() {
    let schema = {
        let mut s = TableSchema::new("public.t".to_string(), 1, vec![], vec![]);
        s.check_constraints = vec![
            CheckConstraint {
                name: None,
                expr: "age > 0".to_string(),
            },
            CheckConstraint {
                name: Some("explicit_ck".to_string()),
                expr: "score > 0".to_string(),
            },
        ];
        s
    };

    assert_eq!(
        check_constraint_effective_name("t", 0, &schema.check_constraints[0]),
        "t_age_check"
    );
    assert_eq!(
        find_check_constraint_index(&schema, "t", "t_age_check"),
        Some(0)
    );
    assert_eq!(
        find_check_constraint_index(&schema, "t", "explicit_ck"),
        Some(1)
    );
    assert_eq!(find_check_constraint_index(&schema, "t", "missing"), None);
}

#[test]
fn constraint_name_exists_checks_pk_fk_index_and_checks() {
    let schema = {
        let mut s = TableSchema::new("public.t".to_string(), 1, vec![], vec![0]);
        s.indexes = vec![IndexDef {
            name: "idx_t_a".to_string(),
            id: 1,
            columns: vec!["a".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }];
        s.check_constraints = vec![CheckConstraint {
            name: None,
            expr: "age > 0".to_string(),
        }];
        s.foreign_keys = vec![ForeignKeyConstraint {
            name: "fk_t_p".to_string(),
            columns: vec![],
            ref_table: "public.p".to_string(),
            ref_columns: vec![],
            on_delete: ForeignKeyAction::NoAction,
            on_update: ForeignKeyAction::NoAction,
            deferrable: false,
            initially_deferred: false,
        }];
        s
    };

    assert!(constraint_name_exists(&schema, "t", "t_pkey"));
    assert!(constraint_name_exists(&schema, "t", "idx_t_a"));
    assert!(constraint_name_exists(&schema, "t", "fk_t_p"));
    assert!(constraint_name_exists(&schema, "t", "t_age_check"));
    assert!(!constraint_name_exists(&schema, "t", "not_exist"));
}

#[test]
fn parse_referential_action_maps_all_variants() {
    use sqlparser::ast::ReferentialAction;
    assert_eq!(
        parse_referential_action(&Some(ReferentialAction::Cascade)),
        ForeignKeyAction::Cascade
    );
    assert_eq!(
        parse_referential_action(&Some(ReferentialAction::SetNull)),
        ForeignKeyAction::SetNull
    );
    assert_eq!(
        parse_referential_action(&Some(ReferentialAction::SetDefault)),
        ForeignKeyAction::SetDefault
    );
    assert_eq!(
        parse_referential_action(&Some(ReferentialAction::Restrict)),
        ForeignKeyAction::Restrict
    );
    assert_eq!(
        parse_referential_action(&Some(ReferentialAction::NoAction)),
        ForeignKeyAction::NoAction
    );
    assert_eq!(parse_referential_action(&None), ForeignKeyAction::NoAction);
}

#[test]
fn was_cascade_dropped_resolves_qualified_and_search_path_names() {
    use sqlparser::ast::{Ident, ObjectName};
    let dropped: std::collections::HashSet<String> =
        ["public.v1".to_string(), "app.v2".to_string()]
            .into_iter()
            .collect();

    assert!(was_cascade_dropped(
        &ObjectName(vec![Ident::new("public"), Ident::new("v1")]),
        &["public".to_string()],
        &dropped
    ));
    assert!(was_cascade_dropped(
        &ObjectName(vec![Ident::new("v2")]),
        &["app".to_string(), "public".to_string()],
        &dropped
    ));
    assert!(!was_cascade_dropped(
        &ObjectName(vec![Ident::new("v3")]),
        &["public".to_string()],
        &dropped
    ));
}

#[test]
fn encode_prefix_end_increments_last_non_ff_byte() {
    use crate::storage::encode_prefix_end;
    assert_eq!(encode_prefix_end(&[0x01, 0x02]), vec![0x01, 0x03]);
    assert_eq!(encode_prefix_end(&[0x01, 0xFF]), vec![0x02]);
    assert_eq!(
        encode_prefix_end(&[0x00, 0x10, 0xFF, 0xFF]),
        vec![0x00, 0x11]
    );
    // All-0xFF gracefully appends rather than panicking
    assert_eq!(encode_prefix_end(&[0xFF, 0xFF]), vec![0xFF, 0xFF, 0xFF]);
}

#[test]
fn coerce_uuid_to_text_for_type_change() {
    let col = crate::model::ColumnDef::new("id", DataType::Text, false);
    let bytes = *uuid::Uuid::nil().as_bytes();
    let out = coerce_value_for_type_change(Value::Uuid(bytes), &col).unwrap();
    assert_eq!(out, Value::Text(uuid::Uuid::nil().to_string()));
}

/// Single test to avoid env-var race conditions under parallel test execution.
#[test]
fn alter_table_byte_limit_respects_env_var() {
    // Default when env unset
    std::env::remove_var("DB9_ALTER_TABLE_BYTE_LIMIT");
    assert_eq!(alter_table_byte_limit(), DEFAULT_ALTER_TABLE_BYTE_LIMIT);
    assert_eq!(alter_table_byte_limit(), 80 * 1024 * 1024);

    // Zero disables
    std::env::set_var("DB9_ALTER_TABLE_BYTE_LIMIT", "0");
    assert_eq!(alter_table_byte_limit(), 0);

    // Custom value
    std::env::set_var("DB9_ALTER_TABLE_BYTE_LIMIT", "50000000");
    assert_eq!(alter_table_byte_limit(), 50_000_000);

    // Invalid falls back to default
    std::env::set_var("DB9_ALTER_TABLE_BYTE_LIMIT", "not_a_number");
    assert_eq!(alter_table_byte_limit(), DEFAULT_ALTER_TABLE_BYTE_LIMIT);

    // Cleanup
    std::env::remove_var("DB9_ALTER_TABLE_BYTE_LIMIT");
}

// ── Claim-lease cancellation contract ───────────────────────────────────────

/// Behavioral contract test for the shared cancellation primitive that the three
/// specialized long-running task paths (HNSW merge, CIC backfill, storage scan)
/// use before every tenant commit. Pure (no TiKV): proves the gate itself.
#[test]
fn lease_cancel_bails_only_when_token_is_cancelled() {
    use crate::worker::LeaseCancel;
    use pgwire::tokio::CancellationToken;

    // No token (foreground DDL): never bails.
    assert!(LeaseCancel::none().bail_if_cancelled().is_ok());
    assert!(LeaseCancel::new(None).bail_if_cancelled().is_ok());

    // Live, uncancelled token: does not bail.
    let token = CancellationToken::new();
    let lease = LeaseCancel::new(Some(token.clone()));
    assert!(lease.bail_if_cancelled().is_ok());

    // After the lease-renewer cancels (lost/stolen claim): bails with the
    // canonical claim-cancelled error — the SAME string run_with_guards emits.
    token.cancel();
    let err = lease
        .bail_if_cancelled()
        .expect_err("a cancelled lease must bail before any commit");
    assert!(
        err.to_string().contains("cancelled by administrator"),
        "must surface the shared cancellation error, got: {err}"
    );
}

/// Behavioral, TiKV-backed regression for the P1 this fix closes: the shared
/// per-batch commit choke point (`maybe_rotate_backfill_txn`, used by both CIC
/// backfill phases and reconcile) must, when the claim lease is cancelled
/// MID-RUN, abort BEFORE committing the in-flight batch — leaving the tenant
/// write uncommitted for the new owner. Drives the real production helper with a
/// real TiKV transaction and a real (cancelled) lease token, then proves the
/// staged write never landed.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn rotate_backfill_txn_abandons_batch_when_lease_cancelled_midrun() {
    use crate::worker::LeaseCancel;
    use pgwire::tokio::CancellationToken;

    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let keyspace = format!(
        "leasecancel_rotate_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let pool = crate::pool::TikvClientPool::new(pd_endpoints);
    let store = pool
        .acquire(Some(keyspace))
        .await
        .expect("acquire tenant handle")
        .store()
        .clone();

    // A unique probe key that the (to-be-abandoned) batch would otherwise commit.
    let probe_key: Vec<u8> = format!(
        "_lease_cancel_probe_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
    .into_bytes();
    let db_id = 1_u64;

    // Stage a tenant write inside an open backfill-style txn.
    let mut txn = store.begin().await.expect("begin backfill txn");
    let mut txn_guard = super::track_active_worker_txn(&txn);
    crate::txn::txn_put(&mut txn, probe_key.clone(), b"staged".to_vec())
        .await
        .expect("stage tenant write");

    // Simulate the renewer cancelling our claim mid-run (lost/stolen lease).
    let token = CancellationToken::new();
    token.cancel();
    let lease = LeaseCancel::new(Some(token));

    // Force the rotation path (>= commit threshold) so the lease fence is reached.
    let mut current_batch_writes = super::DDL_BACKFILL_COMMIT_SIZE;
    let mut has_committed_batches = false;
    let result = super::maybe_rotate_backfill_txn(
        &store,
        &mut txn,
        db_id,
        &mut txn_guard,
        &mut current_batch_writes,
        &mut has_committed_batches,
        &lease,
    )
    .await;

    let err = result.expect_err("a cancelled lease must abort the rotation before commit");
    assert!(
        err.to_string().contains("cancelled by administrator"),
        "rotation must fail with the claim-cancelled error, got: {err}"
    );
    assert!(
        !has_committed_batches,
        "no batch may be marked committed after a cancelled rotation"
    );

    // Roll back the abandoned txn (as the production error path would) and prove
    // the staged tenant write was NEVER committed — the new owner sees nothing.
    txn.rollback().await.ok();
    let mut verify = store.begin().await.expect("begin verify txn");
    let seen = verify.get(probe_key).await.expect("get probe key");
    verify.rollback().await.ok();
    assert!(
        seen.is_none(),
        "cancelled rotation must NOT commit the staged tenant write"
    );
}
