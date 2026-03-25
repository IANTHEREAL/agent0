use super::{
    advisory_lock_timeout, find_text_column, format_visible_name, hstore_extension_oid_for_name,
    is_pg_get_serial_sequence_function_name,
    non_pg_catalog_qualified_pg_get_serial_sequence_signature,
    pg_get_serial_sequence_accepts_text_arg, pg_get_serial_sequence_arg_type_name,
    regclass_lookup_parts, regtype_search_path_schemas, value_to_bool_strict, value_to_i64,
    value_to_i64_strict,
};
use crate::model::{ColumnDef, DataType, TableSchema, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::expr::functions::pg_compat::strip_regtype_array_dims;
use std::collections::HashMap;
use std::time::Duration;

#[test]
fn test_advisory_lock_timeout_reads_lock_timeout_guc_when_typed_missing() {
    let mut settings = HashMap::new();
    settings.insert("statement_timeout".to_string(), "30s".to_string());
    settings.insert("lock_timeout".to_string(), "100ms".to_string());
    assert_eq!(
        advisory_lock_timeout(None, Some(&settings)),
        Some(Duration::from_millis(100))
    );
}

#[test]
fn test_advisory_lock_timeout_ignores_zero_and_invalid_values() {
    let mut settings = HashMap::new();
    settings.insert("lock_timeout".to_string(), "0".to_string());
    assert_eq!(advisory_lock_timeout(None, Some(&settings)), None);

    settings.insert("lock_timeout".to_string(), "not_a_timeout".to_string());
    assert_eq!(advisory_lock_timeout(None, Some(&settings)), None);
}

#[test]
fn test_advisory_lock_timeout_prefers_typed_query_context_timeout() {
    let mut settings = HashMap::new();
    settings.insert("lock_timeout".to_string(), "10ms".to_string());
    assert_eq!(
        advisory_lock_timeout(Some(Duration::from_millis(250)), Some(&settings)),
        Some(Duration::from_millis(250))
    );
}

#[test]
fn test_advisory_lock_timeout_handles_missing_sources() {
    assert_eq!(advisory_lock_timeout(None, None), None);
    assert_eq!(
        advisory_lock_timeout(Some(Duration::from_millis(1)), None),
        Some(Duration::from_millis(1))
    );
}

#[test]
fn test_advisory_lock_timeout_returns_none_when_lock_timeout_missing() {
    let mut settings = HashMap::new();
    settings.insert("statement_timeout".to_string(), "5s".to_string());
    assert_eq!(advisory_lock_timeout(None, Some(&settings)), None);
}

#[test]
fn test_advisory_lock_timeout_parses_non_ms_unit_values() {
    let mut settings = HashMap::new();
    settings.insert("lock_timeout".to_string(), "2s".to_string());
    assert_eq!(
        advisory_lock_timeout(None, Some(&settings)),
        Some(Duration::from_secs(2))
    );
}

#[test]
fn test_advisory_lock_timeout_parses_minute_unit_values() {
    let mut settings = HashMap::new();
    settings.insert("lock_timeout".to_string(), "1min".to_string());
    assert_eq!(
        advisory_lock_timeout(None, Some(&settings)),
        Some(Duration::from_secs(60))
    );
}

#[test]
fn test_advisory_lock_timeout_parses_plain_milliseconds_value() {
    let mut settings = HashMap::new();
    settings.insert("lock_timeout".to_string(), "2500".to_string());
    assert_eq!(
        advisory_lock_timeout(None, Some(&settings)),
        Some(Duration::from_millis(2500))
    );
}

#[test]
fn test_advisory_lock_timeout_keeps_zero_typed_timeout() {
    let mut settings = HashMap::new();
    settings.insert("lock_timeout".to_string(), "10s".to_string());
    assert_eq!(
        advisory_lock_timeout(Some(Duration::ZERO), Some(&settings)),
        Some(Duration::ZERO)
    );
}

#[test]
fn regclass_lookup_parts_accepts_current_database_qualification() {
    let parsed = crate::sql::names::parse_regclass_input("postgres.public.rel").unwrap();
    let (schema, name) = regclass_lookup_parts(&parsed, "postgres", "postgres.public.rel").unwrap();
    assert_eq!(schema, Some("public"));
    assert_eq!(name, "rel");
}

#[test]
fn regclass_lookup_parts_rejects_foreign_database_qualification() {
    let parsed = crate::sql::names::parse_regclass_input("other.public.rel").unwrap();
    let err = regclass_lookup_parts(&parsed, "postgres", "other.public.rel").unwrap_err();
    assert!(err
        .to_string()
        .contains("cross-database references are not implemented: \"other.public.rel\""));
}

#[test]
fn test_value_to_i64_strict_accepts_integer_text() {
    assert_eq!(
        value_to_i64_strict(&Value::Text("42".to_string()), "column_no").unwrap(),
        42
    );
    assert_eq!(
        value_to_i64_strict(&Value::Int32(7), "column_no").unwrap(),
        7
    );
    assert_eq!(
        value_to_i64_strict(&Value::Int64(8), "column_no").unwrap(),
        8
    );
}

#[test]
fn test_value_to_i64_strict_rejects_non_integer_text() {
    assert!(value_to_i64_strict(&Value::Text("not_an_int".to_string()), "column_no").is_err());
}

#[test]
fn test_value_to_i64_strict_rejects_float64() {
    assert!(value_to_i64_strict(&Value::Float64(1.9), "column_no").is_err());
}

#[test]
fn test_value_to_i64_strict_error_mentions_argument_name() {
    let err = value_to_i64_strict(&Value::Boolean(false), "column_no").unwrap_err();
    assert!(err.to_string().contains("column_no"));
}

#[test]
fn test_value_to_i64_strict_accepts_negative_integer_text_with_spaces() {
    assert_eq!(
        value_to_i64_strict(&Value::Text("  -42 ".to_string()), "column_no").unwrap(),
        -42
    );
}

#[test]
fn test_value_to_i64_strict_rejects_null_value() {
    assert!(value_to_i64_strict(&Value::Null, "column_no").is_err());
}

#[test]
fn test_value_to_bool_strict_accepts_boolean() {
    assert!(value_to_bool_strict(&Value::Boolean(true), "pretty").unwrap());
}

#[test]
fn test_value_to_bool_strict_rejects_int32() {
    assert!(value_to_bool_strict(&Value::Int32(1), "pretty").is_err());
}

#[test]
fn test_value_to_bool_strict_error_mentions_argument_name() {
    let err = value_to_bool_strict(&Value::Text("true".to_string()), "pretty").unwrap_err();
    assert!(err.to_string().contains("pretty"));
}

#[test]
fn test_value_to_bool_strict_rejects_null_value() {
    assert!(value_to_bool_strict(&Value::Null, "pretty").is_err());
}

#[test]
fn test_find_text_column_case_insensitive_and_none_cases() {
    let schema = TableSchema {
        name: "t".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "OID".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
            ColumnDef {
                name: "typname".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    };

    let (_, idx) = find_text_column(Some(&schema), "TypName").expect("column should match");
    assert_eq!(idx, 1);
    assert!(find_text_column(Some(&schema), "missing").is_none());
    assert!(find_text_column(None, "typname").is_none());
}

#[test]
fn test_find_text_column_returns_first_match_when_duplicate_names_exist() {
    let schema = TableSchema {
        name: "dup".to_string(),
        table_id: 2,
        columns: vec![
            ColumnDef {
                name: "typname".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
            ColumnDef {
                name: "TyPnAmE".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    };

    let (_, idx) = find_text_column(Some(&schema), "typname").expect("column should match");
    assert_eq!(idx, 0);
}

#[test]
fn test_value_to_i64_accepts_numeric_and_text_inputs() {
    assert_eq!(value_to_i64(&Value::Int32(7)), Some(7));
    assert_eq!(value_to_i64(&Value::Int64(8)), Some(8));
    assert_eq!(value_to_i64(&Value::Float64(9.9)), Some(9));
    assert_eq!(value_to_i64(&Value::Float64(-9.1)), Some(-9));
    assert_eq!(value_to_i64(&Value::Text("+12".to_string())), Some(12));
    assert_eq!(value_to_i64(&Value::Text(" 10 ".to_string())), Some(10));
    assert_eq!(value_to_i64(&Value::Text(" -11 ".to_string())), Some(-11));
    assert_eq!(value_to_i64(&Value::Text("12.5".to_string())), None);
    assert_eq!(value_to_i64(&Value::Text("bad".to_string())), None);
    assert_eq!(value_to_i64(&Value::Null), None);
    assert_eq!(value_to_i64(&Value::Boolean(true)), None);
}

#[test]
fn test_strip_regtype_array_dims() {
    assert_eq!(
        strip_regtype_array_dims("integer[]"),
        ("integer".to_string(), true)
    );
    assert_eq!(
        strip_regtype_array_dims("\"MyType\"[][]"),
        ("\"MyType\"".to_string(), true)
    );
}

#[test]
fn test_regtype_search_path_schemas_prepends_implicit_pg_catalog() {
    let path = vec!["$user".to_string(), "s1".to_string(), "public".to_string()];
    let schemas = regtype_search_path_schemas(&path);
    assert_eq!(schemas, vec!["pg_catalog", "s1", "public"]);
    assert_eq!(
        regtype_search_path_schemas(&[]),
        vec!["pg_catalog", "public"]
    );
}

#[test]
fn test_hstore_extension_oid_for_name_respects_identifier_and_array_rules() {
    assert_eq!(
        hstore_extension_oid_for_name("hstore", false),
        Some(crate::sql::pg_types::OID_HSTORE)
    );
    assert_eq!(
        hstore_extension_oid_for_name("hstore", true),
        Some(crate::sql::pg_types::OID_HSTORE_ARRAY)
    );

    assert_eq!(
        hstore_extension_oid_for_name("_hstore", false),
        Some(crate::sql::pg_types::OID_HSTORE_ARRAY)
    );
    assert_eq!(hstore_extension_oid_for_name("_hstore", true), None);

    assert_eq!(hstore_extension_oid_for_name("HSTORE", false), None);
}

#[test]
fn test_format_visible_name_quotes_mixed_case_and_qualifies_hidden_objects() {
    assert_eq!(
        format_visible_name("public", "simple_name", true),
        "simple_name"
    );
    assert_eq!(
        format_visible_name("public", "CamelType", true),
        "\"CamelType\""
    );
    assert_eq!(
        format_visible_name("pr1659_regtype", "CamelType", false),
        "pr1659_regtype.\"CamelType\""
    );
}

#[test]
fn test_pg_get_serial_sequence_name_match_accepts_pg_catalog_and_case_variants() {
    assert!(is_pg_get_serial_sequence_function_name(
        "pg_get_serial_sequence"
    ));
    assert!(is_pg_get_serial_sequence_function_name(
        "PG_GET_SERIAL_SEQUENCE"
    ));
    assert!(is_pg_get_serial_sequence_function_name(
        "pg_catalog.pg_get_serial_sequence"
    ));
    assert!(is_pg_get_serial_sequence_function_name(
        "PG_CATALOG.PG_GET_SERIAL_SEQUENCE"
    ));
    assert!(!is_pg_get_serial_sequence_function_name(
        "public.pg_get_serial_sequence"
    ));
}

#[test]
fn test_pg_get_serial_sequence_arg_type_guard_matches_pg_signature_surface() {
    let int_arg = TypedExpr {
        kind: TypedExprKind::Constant(Value::Int32(1)),
        data_type: DataType::Int32,
    };
    let text_arg = TypedExpr {
        kind: TypedExprKind::Constant(Value::Text("t".to_string())),
        data_type: DataType::Text,
    };
    let null_arg = TypedExpr {
        kind: TypedExprKind::Constant(Value::Null),
        data_type: DataType::Text,
    };
    let varchar_arg = TypedExpr {
        kind: TypedExprKind::Parameter { index: 0 },
        data_type: DataType::Varchar(32),
    };

    let name_arg = TypedExpr {
        kind: TypedExprKind::Constant(Value::Text("t".to_string())),
        data_type: DataType::Name,
    };

    assert!(!pg_get_serial_sequence_accepts_text_arg(&int_arg));
    assert!(pg_get_serial_sequence_accepts_text_arg(&text_arg));
    assert!(pg_get_serial_sequence_accepts_text_arg(&null_arg));
    assert!(pg_get_serial_sequence_accepts_text_arg(&varchar_arg));
    assert!(pg_get_serial_sequence_accepts_text_arg(&name_arg));

    assert_eq!(pg_get_serial_sequence_arg_type_name(&int_arg), "integer");
    assert_eq!(pg_get_serial_sequence_arg_type_name(&text_arg), "unknown");
    assert_eq!(pg_get_serial_sequence_arg_type_name(&null_arg), "unknown");
}

#[test]
fn test_non_pg_catalog_qualified_pg_get_serial_sequence_returns_function_signature() {
    let text_arg = TypedExpr {
        kind: TypedExprKind::Constant(Value::Text("t".to_string())),
        data_type: DataType::Text,
    };
    let null_arg = TypedExpr {
        kind: TypedExprKind::Constant(Value::Null),
        data_type: DataType::Text,
    };
    let signature = non_pg_catalog_qualified_pg_get_serial_sequence_signature(
        "PUBLIC.PG_GET_SERIAL_SEQUENCE",
        &[text_arg, null_arg],
    );
    assert_eq!(
        signature,
        Some("public.pg_get_serial_sequence(unknown, unknown)".to_string())
    );

    assert_eq!(
        non_pg_catalog_qualified_pg_get_serial_sequence_signature(
            "pg_catalog.pg_get_serial_sequence",
            &[],
        ),
        None
    );
    assert_eq!(
        non_pg_catalog_qualified_pg_get_serial_sequence_signature("pg_get_serial_sequence", &[],),
        None
    );
}
