//! Unit tests for catalog prefetch extraction and resolution helpers.

use super::extraction::*;
use sqlparser::ast::{Query, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;

fn parse_stmt(sql: &str) -> Statement {
    let dialect = PostgreSqlDialect {};
    let mut stmts = Parser::parse_sql(&dialect, sql).unwrap();
    stmts.remove(0)
}

fn parse_query(sql: &str) -> Query {
    match parse_stmt(sql) {
        Statement::Query(q) => *q,
        other => panic!("expected Query, got {:?}", other),
    }
}

// ── extract_dml_table_names (Bug A regression tests) ──────────

#[test]
fn test_insert_target_table_is_collected() {
    let stmt = parse_stmt("INSERT INTO users(id, name) VALUES (1, 'alice')");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("users"),
        "INSERT target 'users' not found in {:?}",
        names
    );
}

#[test]
fn test_insert_schema_qualified_target() {
    let stmt = parse_stmt("INSERT INTO public.users(id) VALUES (1)");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("public.users"),
        "INSERT target 'public.users' not found in {:?}",
        names
    );
}

#[test]
fn test_insert_select_collects_both_tables() {
    let stmt = parse_stmt("INSERT INTO target SELECT * FROM source");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("target"),
        "missing INSERT target in {:?}",
        names
    );
    assert!(
        names.contains("source"),
        "missing SELECT source in {:?}",
        names
    );
}

#[test]
fn test_update_target_table_is_collected() {
    let stmt = parse_stmt("UPDATE orders SET status = 'done' WHERE id = 1");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("orders"),
        "UPDATE target 'orders' not found in {:?}",
        names
    );
}

#[test]
fn test_update_from_collects_both_tables() {
    let stmt = parse_stmt(
        "UPDATE orders SET total = s.amount FROM summaries s WHERE orders.id = s.order_id",
    );
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("orders"),
        "missing UPDATE target in {:?}",
        names
    );
    assert!(
        names.contains("summaries"),
        "missing FROM table in {:?}",
        names
    );
}

#[test]
fn test_delete_target_table_is_collected() {
    let stmt = parse_stmt("DELETE FROM logs WHERE created_at < '2020-01-01'");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("logs"),
        "DELETE target 'logs' not found in {:?}",
        names
    );
}

#[test]
fn test_delete_using_collects_both_tables() {
    let stmt = parse_stmt("DELETE FROM orders USING customers WHERE orders.cust_id = customers.id");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("orders"),
        "missing DELETE target in {:?}",
        names
    );
    assert!(
        names.contains("customers"),
        "missing USING table in {:?}",
        names
    );
}

// ── extract_table_names (SELECT queries) ──────────────────────

#[test]
fn test_select_from_single_table() {
    let query = parse_query("SELECT * FROM users");
    let names = extract_table_names(&query);
    assert!(names.contains("users"), "missing table in {:?}", names);
}

#[test]
fn test_select_join_collects_both_tables() {
    let query = parse_query("SELECT * FROM orders o JOIN customers c ON o.cust_id = c.id");
    let names = extract_table_names(&query);
    assert!(
        names.contains("orders"),
        "missing left table in {:?}",
        names
    );
    assert!(
        names.contains("customers"),
        "missing right table in {:?}",
        names
    );
}

#[test]
fn test_select_subquery_collects_inner_table() {
    let query = parse_query("SELECT * FROM (SELECT id FROM items) sub");
    let names = extract_table_names(&query);
    assert!(
        names.contains("items"),
        "missing subquery table in {:?}",
        names
    );
}

// ── INSERT with ON CONFLICT (test 119 scenario) ──────────────

#[test]
fn test_insert_on_conflict_collects_target() {
    let stmt = parse_stmt(
        "INSERT INTO t_child(id, parent_id) VALUES (1, 1) \
         ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id + 1",
    );
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("t_child"),
        "INSERT ON CONFLICT target not found in {:?}",
        names
    );
}

// ── INSERT with type coercion (test 155 scenario) ─────────────

#[test]
fn test_insert_with_cast_collects_target() {
    let stmt = parse_stmt("INSERT INTO t_int(id, i) VALUES (1, '{\"a\":1}'::jsonb)");
    let names = extract_dml_table_names(&stmt);
    assert!(
        names.contains("t_int"),
        "INSERT with CAST target not found in {:?}",
        names
    );
}

#[test]
fn test_extract_scalar_functions_from_query() {
    let query = parse_query(
        "SELECT basic_add(id, 1), upper(name) FROM users WHERE is_large(id) AND upper(name) <> ''",
    );
    let names = extract_scalar_function_names(&query);
    let keys: HashSet<String> = names
        .iter()
        .map(|name| {
            name.0
                .iter()
                .map(crate::sql::names::normalize_ident)
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect();
    assert!(keys.contains("basic_add"));
    assert!(keys.contains("upper"));
    assert!(keys.contains("is_large"));
    assert_eq!(keys.iter().filter(|k| k.as_str() == "upper").count(), 1);
}

#[test]
fn test_extract_scalar_functions_from_statement() {
    let stmt = parse_stmt("UPDATE t SET v = my_udf(v) WHERE is_large(v)");
    let names = extract_scalar_function_names_from_statement(&stmt);
    let keys: HashSet<String> = names
        .iter()
        .map(|name| {
            name.0
                .iter()
                .map(crate::sql::names::normalize_ident)
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect();
    assert!(keys.contains("my_udf"));
    assert!(keys.contains("is_large"));
}
