//! Tests for the EXPLAIN module.

use super::*;
use crate::types::{ColumnDef, DataType, IndexDef};

fn dummy_schema_lookup(name: &str) -> Option<TableSchema> {
    if name == "users" {
        Some(TableSchema {
            name: "users".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: true,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("users_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![IndexDef {
                id: 1,
                name: "users_pkey".to_string(),
                columns: vec!["id".to_string()],
                unique: true,
                method: None,
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        })
    } else {
        None
    }
}

fn dummy_row_count(_name: &str) -> usize {
    1000
}

#[test]
fn test_seq_scan_plan() {
    let sql = "SELECT * FROM users WHERE name = 'Alice'";
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

    let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
    let output = format_plan_text(&plan, 0);

    assert!(output.contains("Seq Scan on users"));
    assert!(output.contains("Filter:"));
}

#[test]
fn test_index_scan_plan() {
    let sql = "SELECT * FROM users WHERE id = 1";
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

    let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
    let output = format_plan_text(&plan, 0);

    assert!(output.contains("Index Scan using users_pkey on users"));
    assert!(output.contains("Index Cond:"));
}

#[test]
fn test_table_function_plan() {
    let sql = "SELECT * FROM extensions.fs9('./*.rs')";
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

    let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
    let output = format_plan_text(&plan, 0);

    assert!(
        output.contains("Function Scan on fs9"),
        "EXPLAIN should show Function Scan for table functions, got: {}",
        output
    );
}

#[test]
fn test_simple_select_plan() {
    let sql = "SELECT 1";
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

    let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
    let output = format_plan_text(&plan, 0);

    assert!(output.contains("Result"));
}
