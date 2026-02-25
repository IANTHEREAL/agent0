//! Unit tests for DDL helpers and utility functions.

use super::*;
use crate::worker::types::IndexState;

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
        method: None,
        predicate: None,
        expressions: vec![],
        state: IndexState::Ready,
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
    let schemas = vec![test_schema_with_index("public.t1", "idx_shared")];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_shared",
        None,
    ));
}

#[test]
fn test_legacy_scan_detects_explicit_pk() {
    let schemas = vec![test_schema_with_pk("public.t1", Some("my_pk"))];
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
    let schemas = vec![test_schema_with_pk("public.t1", None)];
    assert!(create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "t1_pkey",
        None,
    ));
}

#[test]
fn test_legacy_scan_ignores_other_schema() {
    let schemas = vec![test_schema_with_index("other.t1", "idx_shared")];
    assert!(!create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_shared",
        None,
    ));
}

#[test]
fn test_legacy_scan_no_conflict() {
    let schemas = vec![test_schema_with_index("public.t1", "idx_a")];
    assert!(!create_table::has_legacy_name_conflict(
        schemas.iter().map(|(n, s)| (n.as_str(), s)),
        "public",
        "idx_b",
        None,
    ));
}

#[test]
fn test_legacy_scan_multi_table() {
    let schemas = vec![
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
    let schemas = vec![test_schema_with_pk("public.t1", Some("t1_pkey"))];
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
    let schemas = vec![
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
        collation: None,
    };
    let result =
        coerce_value_for_type_change(Value::Json(r#"{"b":1,"a":2}"#.to_string()), &col).unwrap();
    // JSON preserves the original string verbatim
    assert_eq!(result, Value::Text(r#"{"b":1,"a":2}"#.to_string()));
}
