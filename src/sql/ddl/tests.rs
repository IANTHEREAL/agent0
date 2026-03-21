//! Unit tests for DDL helpers and utility functions.

use super::*;
use crate::worker::types::IndexState;

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
    let col = crate::model::ColumnDef {
        name: "data".to_string(),
        data_type: DataType::Text,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    };
    let result =
        coerce_value_for_type_change(Value::Jsonb(r#"{"b":1,"a":2}"#.to_string()), &col).unwrap();
    assert_eq!(result, Value::Text(r#"{"a": 2, "b": 1}"#.to_string()));
}

#[test]
fn coerce_json_to_text_preserves_raw_format() {
    let col = crate::model::ColumnDef {
        name: "data".to_string(),
        data_type: DataType::Text,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    };
    let result =
        coerce_value_for_type_change(Value::Json(r#"{"b":1,"a":2}"#.to_string()), &col).unwrap();
    // JSON preserves the original string verbatim
    assert_eq!(result, Value::Text(r#"{"b":1,"a":2}"#.to_string()));
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
    let schema = TableSchema {
        name: "public.t".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![
            CheckConstraint {
                name: None,
                expr: "age > 0".to_string(),
            },
            CheckConstraint {
                name: Some("explicit_ck".to_string()),
                expr: "score > 0".to_string(),
            },
        ],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
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
    let schema = TableSchema {
        name: "public.t".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: Some("t_pkey".to_string()),
        pk_indices: vec![0],
        indexes: vec![IndexDef {
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }],
        check_constraints: vec![CheckConstraint {
            name: None,
            expr: "age > 0".to_string(),
        }],
        foreign_keys: vec![ForeignKeyConstraint {
            name: "fk_t_p".to_string(),
            columns: vec![],
            ref_table: "public.p".to_string(),
            ref_columns: vec![],
            on_delete: ForeignKeyAction::NoAction,
            on_update: ForeignKeyAction::NoAction,
        }],
        owner: "postgres".to_string(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
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
fn prefix_end_increments_last_non_ff_byte() {
    assert_eq!(prefix_end(vec![0x01, 0x02]), vec![0x01, 0x03]);
    assert_eq!(prefix_end(vec![0x01, 0xFF]), vec![0x02]);
    assert_eq!(prefix_end(vec![0x00, 0x10, 0xFF, 0xFF]), vec![0x00, 0x11]);
}

#[test]
fn coerce_uuid_to_text_for_type_change() {
    let col = crate::model::ColumnDef {
        name: "id".to_string(),
        data_type: DataType::Text,
        nullable: false,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    };
    let bytes = *uuid::Uuid::nil().as_bytes();
    let out = coerce_value_for_type_change(Value::Uuid(bytes), &col).unwrap();
    assert_eq!(out, Value::Text(uuid::Uuid::nil().to_string()));
}
