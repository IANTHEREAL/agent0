//! Tests for the query planner module.

use super::index_selection::{choose_best_access_path, extract_gin_contains_predicate};
use super::predicate::collect_predicates;
use super::scan_type::{
    normalize_expr_for_match, normalize_expr_string, parse_predicate_expr,
    typed_expr_to_canonical_sql,
};
use super::*;
use crate::types::IndexDef;
use sqlparser::ast::Ident;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn make_eq_expr(col: &str, val: i32) -> Expr {
    Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new(col))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
            val.to_string(),
            false,
        ))),
    }
}

fn schema_with_cols(name: &str, cols: &[&str]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        table_id: 1,
        columns: cols
            .iter()
            .enumerate()
            .map(|(i, c)| crate::types::ColumnDef {
                name: c.to_string(),
                data_type: crate::types::DataType::Int32,
                nullable: true,
                primary_key: i == 0,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            })
            .collect(),
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

#[test]
fn test_extract_equi_join_keys_single() {
    let left = schema_with_cols("l", &["id", "v"]);
    let right = schema_with_cols("r", &["user_id", "v"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("user_id"))),
    };

    let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
}

#[test]
fn test_extract_equi_join_keys_swapped_sides() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["user_id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("user_id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("id"))),
    };

    let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
}

#[test]
fn test_extract_equi_join_keys_matches_qualified_output_schema() {
    let left = schema_with_cols("join", &["a.id", "a.v", "b.user_id"]);
    let right = schema_with_cols("c", &["id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("a"),
            Ident::new("id"),
        ])),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("c"),
            Ident::new("id"),
        ])),
    };

    let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
}

#[test]
fn test_extract_equi_join_keys_multi_key_and() {
    let left = schema_with_cols("l", &["a", "b"]);
    let right = schema_with_cols("r", &["x", "y"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("a"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("x"))),
        }),
        op: BinaryOperator::And,
        right: Box::new(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("b"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("y"))),
        }),
    };

    let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0, 1]);
    assert_eq!(rk, vec![0, 1]);
}

#[test]
fn test_extract_equi_join_keys_qualified_refs_against_unqualified_schemas() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["user_id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("l"),
            Ident::new("id"),
        ])),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("r"),
            Ident::new("user_id"),
        ])),
    };

    let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
}

#[test]
fn test_extract_equi_join_keys_unqualified_ref_against_qualified_schema_unique() {
    let left = schema_with_cols("join", &["a.id", "a.v"]);
    let right = schema_with_cols("c", &["id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("c"),
            Ident::new("id"),
        ])),
    };

    let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
}

#[test]
fn test_choose_join_algorithm_with_qualified_left_schema() {
    let left = schema_with_cols("join", &["a.id", "a.v", "b.id"]);
    let right = schema_with_cols("c", &["id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("a"),
            Ident::new("id"),
        ])),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("c"),
            Ident::new("id"),
        ])),
    };

    let cfg = HashJoinConfig {
        max_memory_bytes: 1,
        min_rows_threshold: 1,
    };

    match choose_join_algorithm(Some(&expr), &left, &right, 1000, 1000, &cfg) {
        JoinAlgorithmChoice::HashJoin {
            left_key_indices,
            right_key_indices,
            ..
        } => {
            assert_eq!(left_key_indices, vec![0]);
            assert_eq!(right_key_indices, vec![0]);
        }
        other => panic!("expected HashJoin, got {:?}", other),
    }
}

#[test]
fn test_choose_join_algorithm_threshold_and_build_side() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("id"))),
    };

    let cfg = HashJoinConfig {
        max_memory_bytes: 1,
        min_rows_threshold: 100,
    };

    assert_eq!(
        choose_join_algorithm(Some(&expr), &left, &right, 10, 10, &cfg),
        JoinAlgorithmChoice::NestedLoop
    );

    match choose_join_algorithm(Some(&expr), &left, &right, 1000, 10, &cfg) {
        JoinAlgorithmChoice::HashJoin { left_is_build, .. } => assert!(!left_is_build),
        other => panic!("expected HashJoin, got {:?}", other),
    }
}

#[test]
fn test_choose_join_algorithm_type_mismatch_falls_back() {
    let left = TableSchema::new(
        "l".to_string(),
        1,
        vec![crate::types::ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Text,
            nullable: true,
            primary_key: true,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        vec![0],
    );
    let right = TableSchema::new(
        "r".to_string(),
        2,
        vec![crate::types::ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: true,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        vec![0],
    );

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("id"))),
    };

    let cfg = HashJoinConfig {
        max_memory_bytes: 1,
        min_rows_threshold: 0,
    };

    assert_eq!(
        choose_join_algorithm(Some(&expr), &left, &right, 1000, 1000, &cfg),
        JoinAlgorithmChoice::NestedLoop
    );
}

#[test]
fn test_analyze_predicates_simple_eq() {
    let expr = make_eq_expr("id", 42);
    let predicates = analyze_predicates(&expr);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].column, "id");
    assert_eq!(predicates[0].op, PredicateOp::Eq);
}

#[test]
fn test_analyze_predicates_and() {
    let expr = Expr::BinaryOp {
        left: Box::new(make_eq_expr("a", 1)),
        op: BinaryOperator::And,
        right: Box::new(make_eq_expr("b", 2)),
    };
    let predicates = analyze_predicates(&expr);
    assert_eq!(predicates.len(), 2);
}

#[test]
fn test_choose_full_scan_no_index() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let path = choose_best_access_path(&schema, &[], 1000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_choose_index_scan() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_a".to_string(),
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "a".to_string(),
        op: PredicateOp::Eq,
        value: Value::Int32(1),
        in_values: Vec::new(),
    }];
    let path = choose_best_access_path(&schema, &predicates, 1000);
    assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
}

#[test]
fn test_skip_partial_index_scan() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_a_partial".to_string(),
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: Some("a = 10".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "a".to_string(),
        op: PredicateOp::Eq,
        value: Value::Int32(1),
        in_values: Vec::new(),
    }];
    let path = choose_best_access_path(&schema, &predicates, 1000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

fn schema_for_index_filter_tests(
    columns: Vec<(&str, DataType)>,
    indexes: Vec<IndexDef>,
) -> TableSchema {
    TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: columns
            .into_iter()
            .map(|(name, data_type)| crate::types::ColumnDef {
                name: name.to_string(),
                data_type,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            })
            .collect(),
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

#[test]
fn test_partial_index_exact_predicate_used() {
    let schema = schema_for_index_filter_tests(
        vec![("name", DataType::Text), ("status", DataType::Text)],
        vec![IndexDef {
            id: 1,
            name: "idx_name_active".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE status = 'active' AND name = 'foo'");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
}

#[test]
fn test_partial_index_missing_predicate_not_used() {
    let schema = schema_for_index_filter_tests(
        vec![("name", DataType::Text), ("status", DataType::Text)],
        vec![IndexDef {
            id: 1,
            name: "idx_name_active".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE name = 'foo'");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_partial_index_wrong_value_not_used() {
    let schema = schema_for_index_filter_tests(
        vec![("name", DataType::Text), ("status", DataType::Text)],
        vec![IndexDef {
            id: 1,
            name: "idx_name_active".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE status = 'inactive' AND name = 'foo'");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_partial_index_conjunct_subset_used() {
    let schema = schema_for_index_filter_tests(
        vec![
            ("a", DataType::Int32),
            ("b", DataType::Int32),
            ("c", DataType::Int32),
        ],
        vec![IndexDef {
            id: 1,
            name: "idx_a_partial".to_string(),
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: Some("a = 1 AND b = 2".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE a = 1 AND b = 2 AND c = 3");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
}

#[test]
fn test_partial_index_incomplete_conjunct_not_used() {
    let schema = schema_for_index_filter_tests(
        vec![("a", DataType::Int32), ("b", DataType::Int32)],
        vec![IndexDef {
            id: 1,
            name: "idx_a_partial".to_string(),
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: Some("a = 1 AND b = 2".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE a = 1");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_expression_index_lower_used() {
    let schema = schema_for_index_filter_tests(
        vec![("name", DataType::Text)],
        vec![IndexDef {
            id: 1,
            name: "idx_lower_name".to_string(),
            columns: vec![],
            unique: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(name)".to_string()],
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE lower(name) = 'foo'");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
}

#[test]
fn test_expression_index_case_insensitive_match() {
    let schema = schema_for_index_filter_tests(
        vec![("name", DataType::Text)],
        vec![IndexDef {
            id: 1,
            name: "idx_lower_name".to_string(),
            columns: vec![],
            unique: false,
            method: None,
            predicate: None,
            expressions: vec!["LOWER(name)".to_string()],
            state: crate::worker::types::IndexState::Ready,
        }],
    );
    let filter = parse_where_expr("SELECT * FROM t WHERE lower(name) = 'bar'");
    let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
    assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
}

#[test]
fn test_gin_index_skipped_without_operator() {
    use crate::types::ColumnDef;
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![ColumnDef {
            name: "metadata".to_string(),
            data_type: DataType::Jsonb,
            nullable: true,
            default_expr: None,
            collation: None,
            primary_key: false,
            unique: false,
            is_serial: false,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 7,
            name: "idx_meta".to_string(),
            columns: vec!["metadata".to_string()],
            unique: false,
            method: Some("gin".to_string()),
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    let dialect = PostgreSqlDialect {};
    let statements = Parser::parse_sql(
        &dialect,
        "SELECT * FROM t WHERE metadata @> '{\"type\":\"pdf\"}'",
    )
    .unwrap();
    let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
        panic!("expected query");
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        panic!("expected select");
    };
    let filter = select.selection.as_ref().expect("WHERE exists");

    let path = choose_best_access_path_for_filter(0, &schema, Some(filter), 1000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_gin_predicate_selection_single() {
    let filter = parse_where_expr("SELECT * FROM t WHERE metadata @> '{\"type\":\"pdf\"}'");
    let (column, _) = extract_gin_contains_predicate(&filter).expect("expected GIN predicate");
    assert_eq!(column, "metadata");
}

#[test]
fn test_gin_predicate_selection_prefers_fts() {
    let filter = parse_where_expr(
        "SELECT * FROM t WHERE metadata @> '{\"type\":\"pdf\"}' AND document @@ to_tsquery('invoice')",
    );
    let (column, pattern) =
        extract_gin_contains_predicate(&filter).expect("expected GIN predicate");
    assert_eq!(column, "document");
    assert!(matches!(pattern, Value::Tsquery(_)));
}

#[test]
fn test_analyze_predicates_comparison_ops() {
    let lt_expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("x"))),
        op: BinaryOperator::Lt,
        right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
            "10".to_string(),
            false,
        ))),
    };
    let predicates = analyze_predicates(&lt_expr);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].op, PredicateOp::Lt);

    let gt_expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("y"))),
        op: BinaryOperator::Gt,
        right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
            "5".to_string(),
            false,
        ))),
    };
    let predicates = analyze_predicates(&gt_expr);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].op, PredicateOp::Gt);
}

fn parse_where_expr(sql: &str) -> Expr {
    let dialect = PostgreSqlDialect {};
    let statements = Parser::parse_sql(&dialect, sql).unwrap();
    let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
        panic!("expected query");
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        panic!("expected select");
    };
    select.selection.expect("WHERE exists")
}

fn is_int_value(v: &Value, expected: i64) -> bool {
    matches!(v, Value::Int32(n) if i64::from(*n) == expected)
        || matches!(v, Value::Int64(n) if *n == expected)
}

#[test]
fn test_analyze_predicates_between() {
    let expr = parse_where_expr("SELECT * FROM t WHERE x BETWEEN 5 AND 10");
    let mut predicates = Vec::new();
    collect_predicates(&expr, &mut predicates);

    let ge = predicates
        .iter()
        .find(|p| p.column == "x" && p.op == PredicateOp::Ge)
        .expect("missing x >= 5");
    assert!(is_int_value(&ge.value, 5));

    let le = predicates
        .iter()
        .find(|p| p.column == "x" && p.op == PredicateOp::Le)
        .expect("missing x <= 10");
    assert!(is_int_value(&le.value, 10));
}

#[test]
fn test_analyze_predicates_in_list() {
    let expr = parse_where_expr("SELECT * FROM t WHERE x IN (1, 2, 3)");
    let mut predicates = Vec::new();
    collect_predicates(&expr, &mut predicates);

    let in_pred = predicates
        .iter()
        .find(|p| p.column == "x" && p.op == PredicateOp::In)
        .expect("missing x IN predicate");
    assert_eq!(in_pred.in_values.len(), 3);
    assert!(is_int_value(&in_pred.in_values[0], 1));
    assert!(is_int_value(&in_pred.in_values[1], 2));
    assert!(is_int_value(&in_pred.in_values[2], 3));
}

#[test]
fn test_analyze_predicates_nested() {
    let nested = Expr::Nested(Box::new(make_eq_expr("id", 1)));
    let predicates = analyze_predicates(&nested);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].column, "id");
}

#[test]
fn test_analyze_predicates_is_null() {
    let is_null = Expr::IsNull(Box::new(Expr::Identifier(Ident::new("col"))));
    let predicates = analyze_predicates(&is_null);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].op, PredicateOp::IsNull);
}

#[test]
fn test_analyze_predicates_is_not_null() {
    let is_not_null = Expr::IsNotNull(Box::new(Expr::Identifier(Ident::new("col"))));
    let predicates = analyze_predicates(&is_not_null);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].op, PredicateOp::IsNotNull);
}

#[test]
fn test_choose_unique_index_over_non_unique() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![
            IndexDef {
                id: 1,
                name: "idx_a".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            },
            IndexDef {
                id: 2,
                name: "idx_a_unique".to_string(),
                columns: vec!["a".to_string()],
                unique: true,
                method: None,
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            },
        ],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "a".to_string(),
        op: PredicateOp::Eq,
        value: Value::Int32(1),
        in_values: Vec::new(),
    }];
    let path = choose_best_access_path(&schema, &predicates, 10000);
    if let ScanType::IndexScan { index_name, .. } = path.scan_type {
        assert_eq!(index_name, "idx_a_unique");
    } else {
        panic!("Expected IndexScan");
    }
}

#[test]
fn test_choose_composite_index() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![
            IndexDef {
                id: 1,
                name: "idx_a".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            },
            IndexDef {
                id: 2,
                name: "idx_ab".to_string(),
                columns: vec!["a".to_string(), "b".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            },
        ],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![
        PredicateInfo {
            column: "a".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(1),
            in_values: Vec::new(),
        },
        PredicateInfo {
            column: "b".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(2),
            in_values: Vec::new(),
        },
    ];
    let path = choose_best_access_path(&schema, &predicates, 10000);
    if let ScanType::IndexScan {
        index_name, values, ..
    } = path.scan_type
    {
        assert_eq!(index_name, "idx_ab");
        assert_eq!(values.len(), 2);
    } else {
        panic!("Expected IndexScan on composite index");
    }
}

#[test]
fn test_index_range_scan_partial_match() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_abc".to_string(),
            columns: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "a".to_string(),
        op: PredicateOp::Eq,
        value: Value::Int32(1),
        in_values: Vec::new(),
    }];
    let path = choose_best_access_path(&schema, &predicates, 10000);
    match path.scan_type {
        ScanType::IndexRangeScan { prefix_values, .. } => {
            assert_eq!(prefix_values.len(), 1);
        }
        _ => panic!("Expected IndexRangeScan for partial match"),
    }
}

#[test]
fn test_choose_range_scan_gt() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![crate::types::ColumnDef {
            name: "val".to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_val".to_string(),
            columns: vec!["val".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "val".to_string(),
        op: PredicateOp::Gt,
        value: Value::Int32(5),
        in_values: Vec::new(),
    }];

    let path = choose_best_access_path(&schema, &predicates, 100000);
    assert!(matches!(
        path.scan_type,
        ScanType::IndexBoundedRangeScan { .. }
    ));
}

#[test]
fn test_choose_range_scan_between() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![crate::types::ColumnDef {
            name: "val".to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_val".to_string(),
            columns: vec!["val".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![
        PredicateInfo {
            column: "val".to_string(),
            op: PredicateOp::Ge,
            value: Value::Int32(5),
            in_values: Vec::new(),
        },
        PredicateInfo {
            column: "val".to_string(),
            op: PredicateOp::Le,
            value: Value::Int32(10),
            in_values: Vec::new(),
        },
    ];

    let path = choose_best_access_path(&schema, &predicates, 100000);
    match path.scan_type {
        ScanType::IndexBoundedRangeScan {
            range_start,
            range_end,
            ..
        } => {
            assert!(matches!(range_start, Some(Value::Int32(5))));
            assert!(matches!(range_end, Some(Value::Int32(10))));
        }
        other => panic!("expected IndexBoundedRangeScan, got {:?}", other),
    }
}

#[test]
fn test_choose_range_scan_composite_prefix_plus_range() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![
            crate::types::ColumnDef {
                name: "a".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::types::ColumnDef {
                name: "b".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_ab".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![
        PredicateInfo {
            column: "a".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(1),
            in_values: Vec::new(),
        },
        PredicateInfo {
            column: "b".to_string(),
            op: PredicateOp::Gt,
            value: Value::Int32(5),
            in_values: Vec::new(),
        },
    ];

    let path = choose_best_access_path(&schema, &predicates, 100000);
    match path.scan_type {
        ScanType::IndexBoundedRangeScan {
            prefix_values,
            range_start,
            range_end,
            ..
        } => {
            assert_eq!(prefix_values.len(), 1);
            assert!(matches!(prefix_values[0], Value::Int32(1)));
            assert!(matches!(range_start, Some(Value::Int32(5))));
            assert!(range_end.is_none());
        }
        other => panic!("expected IndexBoundedRangeScan, got {:?}", other),
    }
}

#[test]
fn test_choose_in_list_scan() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![crate::types::ColumnDef {
            name: "val".to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_val".to_string(),
            columns: vec!["val".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "val".to_string(),
        op: PredicateOp::In,
        value: Value::Null,
        in_values: vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)],
    }];

    let path = choose_best_access_path(&schema, &predicates, 100000);
    match path.scan_type {
        ScanType::InListScan { column_values, .. } => {
            assert_eq!(column_values.len(), 3);
            assert!(matches!(column_values[0][0], Value::Int32(1)));
            assert!(matches!(column_values[1][0], Value::Int32(2)));
            assert!(matches!(column_values[2][0], Value::Int32(3)));
        }
        other => panic!("expected InListScan, got {:?}", other),
    }
}

#[test]
fn test_range_scan_cost_less_than_full_scan() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![crate::types::ColumnDef {
            name: "val".to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_val".to_string(),
            columns: vec!["val".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let predicates = vec![PredicateInfo {
        column: "val".to_string(),
        op: PredicateOp::Gt,
        value: Value::Int32(5),
        in_values: Vec::new(),
    }];

    let path = choose_best_access_path(&schema, &predicates, 100000);
    assert!(matches!(
        path.scan_type,
        ScanType::IndexBoundedRangeScan { .. }
    ));
    assert!(path.cost < 100000_f64);
}

#[test]
fn test_predicate_op_reversed() {
    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Value(sqlparser::ast::Value::Number(
            "10".to_string(),
            false,
        ))),
        op: BinaryOperator::Lt,
        right: Box::new(Expr::Identifier(Ident::new("x"))),
    };
    let predicates = analyze_predicates(&expr);
    assert_eq!(predicates.len(), 1);
    assert_eq!(predicates[0].column, "x");
    assert_eq!(predicates[0].op, PredicateOp::Gt);
}

#[test]
fn test_choose_join_algorithm_no_condition() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["id"]);
    let cfg = HashJoinConfig::default();

    assert_eq!(
        choose_join_algorithm(None, &left, &right, 1000, 1000, &cfg),
        JoinAlgorithmChoice::NestedLoop
    );
}

#[test]
fn test_choose_join_algorithm_non_equi_returns_nested_loop() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["id"]);
    let cfg = HashJoinConfig::default();

    let gt_expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Gt,
        right: Box::new(Expr::Identifier(Ident::new("id"))),
    };

    assert_eq!(
        choose_join_algorithm(Some(&gt_expr), &left, &right, 1000, 1000, &cfg),
        JoinAlgorithmChoice::NestedLoop
    );
}

#[test]
fn test_choose_join_algorithm_smaller_table_is_build() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["id"]);
    let cfg = HashJoinConfig {
        max_memory_bytes: usize::MAX,
        min_rows_threshold: 0,
    };

    let eq_expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("id"))),
    };

    match choose_join_algorithm(Some(&eq_expr), &left, &right, 100, 10000, &cfg) {
        JoinAlgorithmChoice::HashJoin { left_is_build, .. } => assert!(left_is_build),
        other => panic!("expected HashJoin, got {:?}", other),
    }

    match choose_join_algorithm(Some(&eq_expr), &left, &right, 10000, 100, &cfg) {
        JoinAlgorithmChoice::HashJoin { left_is_build, .. } => assert!(!left_is_build),
        other => panic!("expected HashJoin, got {:?}", other),
    }
}

#[test]
fn test_extract_equi_join_keys_non_eq_returns_none() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["id"]);

    let gt_expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Gt,
        right: Box::new(Expr::Identifier(Ident::new("id"))),
    };

    assert!(extract_equi_join_keys(&gt_expr, &left, &right).is_none());
}

#[test]
fn test_extract_equi_join_keys_nested_expression() {
    let left = schema_with_cols("l", &["id"]);
    let right = schema_with_cols("r", &["user_id"]);

    let nested = Expr::Nested(Box::new(Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("user_id"))),
    }));

    let (lk, rk) = extract_equi_join_keys(&nested, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
}

#[test]
fn test_full_table_scan_better_for_high_selectivity() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_a".to_string(),
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let path_no_pred = choose_best_access_path(&schema, &[], 10);
    assert!(matches!(path_no_pred.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_multi_join_hash_join_selection_with_qualified_left_schema() {
    // Simulate multi-join scenario: after first join (a JOIN b), the left schema
    // has qualified columns like "a.id", "a.v", "b.id", "b.user_id".
    // The second join (... JOIN c ON b.id = c.b_id) should still be able to
    // extract equi-join keys and select hash join.
    let left_after_first_join = schema_with_cols("join", &["a.id", "a.v", "b.id", "b.user_id"]);
    let right_c = schema_with_cols("c", &["id", "b_id"]);

    // Original condition: b.id = c.b_id
    let original_condition = Expr::BinaryOp {
        left: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("b"),
            Ident::new("id"),
        ])),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::CompoundIdentifier(vec![
            Ident::new("c"),
            Ident::new("b_id"),
        ])),
    };

    let cfg = HashJoinConfig {
        max_memory_bytes: 1,
        min_rows_threshold: 1,
    };

    // This should return HashJoin, not NestedLoop
    match choose_join_algorithm(
        Some(&original_condition),
        &left_after_first_join,
        &right_c,
        1000,
        1000,
        &cfg,
    ) {
        JoinAlgorithmChoice::HashJoin {
            left_key_indices,
            right_key_indices,
            ..
        } => {
            // b.id is at index 2 in the left schema
            assert_eq!(left_key_indices, vec![2]);
            // b_id is at index 1 in the right schema
            assert_eq!(right_key_indices, vec![1]);
        }
        other => panic!(
            "Expected HashJoin for multi-join with qualified left schema, got {:?}",
            other
        ),
    }
}

#[test]
fn test_split_join_condition_pure_equi_join() {
    let left = schema_with_cols("l", &["id", "val"]);
    let right = schema_with_cols("r", &["user_id"]);

    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("id"))),
        op: BinaryOperator::Eq,
        right: Box::new(Expr::Identifier(Ident::new("user_id"))),
    };

    let (lk, rk, residual) = split_join_condition(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
    assert!(residual.is_none());
}

#[test]
fn test_split_join_condition_with_residual() {
    let left = schema_with_cols("l", &["id", "val"]);
    let right = schema_with_cols("r", &["user_id", "amount"]);

    // id = user_id AND val > 10
    let expr = Expr::BinaryOp {
        left: Box::new(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("user_id"))),
        }),
        op: BinaryOperator::And,
        right: Box::new(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("val"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "10".to_string(),
                false,
            ))),
        }),
    };

    let (lk, rk, residual) = split_join_condition(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0]);
    assert_eq!(rk, vec![0]);
    assert!(residual.is_some());
}

#[test]
fn test_split_join_condition_multi_key_with_residual() {
    let left = schema_with_cols("l", &["id", "val", "category"]);
    let right = schema_with_cols("r", &["user_id", "amount", "cat"]);

    // id = user_id AND category = cat AND val > 10
    let expr = Expr::BinaryOp {
        left: Box::new(Expr::BinaryOp {
            left: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("id"))),
                op: BinaryOperator::Eq,
                right: Box::new(Expr::Identifier(Ident::new("user_id"))),
            }),
            op: BinaryOperator::And,
            right: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("category"))),
                op: BinaryOperator::Eq,
                right: Box::new(Expr::Identifier(Ident::new("cat"))),
            }),
        }),
        op: BinaryOperator::And,
        right: Box::new(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("val"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "10".to_string(),
                false,
            ))),
        }),
    };

    let (lk, rk, residual) = split_join_condition(&expr, &left, &right).unwrap();
    assert_eq!(lk, vec![0, 2]);
    assert_eq!(rk, vec![0, 2]);
    assert!(residual.is_some());
}

#[test]
fn test_split_join_condition_no_equi_keys_returns_none() {
    let left = schema_with_cols("l", &["id", "val"]);
    let right = schema_with_cols("r", &["user_id"]);

    // Only a non-equi predicate: val > 10
    let expr = Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("val"))),
        op: BinaryOperator::Gt,
        right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
            "10".to_string(),
            false,
        ))),
    };

    assert!(split_join_condition(&expr, &left, &right).is_none());
}

// ---- Cross-path parity tests (TypedExpr vs AST) ----

use crate::sql::analyzer::types::{
    BinaryOp as TypedBinaryOp, ResolvedFunction, TypedExpr, TypedExprKind,
};

fn typed_constant(v: Value, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::Constant(v),
        data_type: dt,
    }
}

fn typed_column(name: &str, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: 0,
            column_name: name.to_string(),
        },
        data_type: dt,
    }
}

fn typed_binop(left: TypedExpr, op: TypedBinaryOp, right: TypedExpr, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        },
        data_type: dt,
    }
}

fn gin_schema() -> TableSchema {
    TableSchema {
        name: "docs".to_string(),
        table_id: 1,
        columns: vec![
            crate::types::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::types::ColumnDef {
                name: "body".to_string(),
                data_type: DataType::Tsvector,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::types::ColumnDef {
                name: "data".to_string(),
                data_type: DataType::Jsonb,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![
            IndexDef {
                id: 1,
                name: "idx_body_gin".to_string(),
                columns: vec!["body".to_string()],
                unique: false,
                method: Some("gin".to_string()),
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            },
            IndexDef {
                id: 2,
                name: "idx_data_gin".to_string(),
                columns: vec!["data".to_string()],
                unique: false,
                method: Some("gin".to_string()),
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
            },
        ],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

#[test]
fn test_gin_typed_tsmatch_skipped_without_operator() {
    let schema = gin_schema();
    // body @@ to_tsquery('hello')
    let filter = typed_binop(
        typed_column("body", DataType::Tsvector),
        TypedBinaryOp::TsMatch,
        typed_constant(Value::Tsquery("hello".to_string()), DataType::Tsquery),
        DataType::Boolean,
    );

    let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_gin_typed_json_contains_skipped_without_operator() {
    let schema = gin_schema();
    // data @> '{"key": "val"}'::jsonb
    let filter = typed_binop(
        typed_column("data", DataType::Jsonb),
        TypedBinaryOp::JsonContains,
        typed_constant(
            Value::Jsonb(r#"{"key": "val"}"#.to_string()),
            DataType::Jsonb,
        ),
        DataType::Boolean,
    );

    let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_gin_typed_no_gin_index_falls_back() {
    // Schema without GIN index
    let schema = TableSchema {
        name: "plain".to_string(),
        table_id: 1,
        columns: vec![crate::types::ColumnDef {
            name: "body".to_string(),
            data_type: DataType::Tsvector,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    let filter = typed_binop(
        typed_column("body", DataType::Tsvector),
        TypedBinaryOp::TsMatch,
        typed_constant(Value::Tsquery("hello".to_string()), DataType::Tsquery),
        DataType::Boolean,
    );

    let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_expression_index_typed_lower() {
    // Schema with expression index on lower(name)
    let schema = TableSchema {
        name: "users".to_string(),
        table_id: 1,
        columns: vec![crate::types::ColumnDef {
            name: "name".to_string(),
            data_type: DataType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_lower_name".to_string(),
            columns: vec![],
            unique: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(name)".to_string()],
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    // WHERE lower(name) = 'alice'
    let lower_call = TypedExpr {
        kind: TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "lower".to_string(),
                kind: crate::sql::analyzer::types::FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![typed_column("name", DataType::Text)],
            order_by: vec![],
            filter: None,
        },
        data_type: DataType::Text,
    };
    let filter = typed_binop(
        lower_call,
        TypedBinaryOp::Eq,
        typed_constant(Value::Text("alice".to_string()), DataType::Text),
        DataType::Boolean,
    );

    let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
    match &path.scan_type {
        ScanType::IndexScan {
            index_name, values, ..
        } => {
            assert_eq!(index_name, "idx_lower_name");
            assert_eq!(values, &[Value::Text("alice".to_string())]);
        }
        other => panic!("expected IndexScan for expression index, got {:?}", other),
    }
}

#[test]
fn test_partial_index_typed_exact_predicate() {
    // Schema with partial index: WHERE status = 'active'
    let schema = TableSchema {
        name: "orders".to_string(),
        table_id: 1,
        columns: vec![
            crate::types::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::types::ColumnDef {
                name: "status".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_active_orders".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    // WHERE status = 'active' AND id = 42
    let filter = typed_binop(
        typed_binop(
            typed_column("status", DataType::Text),
            TypedBinaryOp::Eq,
            typed_constant(Value::Text("active".to_string()), DataType::Text),
            DataType::Boolean,
        ),
        TypedBinaryOp::And,
        typed_binop(
            typed_column("id", DataType::Int64),
            TypedBinaryOp::Eq,
            typed_constant(Value::Int64(42), DataType::Int64),
            DataType::Boolean,
        ),
        DataType::Boolean,
    );

    let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
    match &path.scan_type {
        ScanType::IndexScan {
            index_name, values, ..
        } => {
            assert_eq!(index_name, "idx_active_orders");
            assert_eq!(values, &[Value::Int64(42)]);
        }
        other => panic!("expected IndexScan for partial index, got {:?}", other),
    }
}

#[test]
fn test_partial_index_typed_missing_predicate() {
    // Same schema as above, but query doesn't include the partial predicate
    let schema = TableSchema {
        name: "orders".to_string(),
        table_id: 1,
        columns: vec![
            crate::types::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::types::ColumnDef {
                name: "status".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_active_orders".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    // WHERE id = 42 (missing status = 'active')
    let filter = typed_binop(
        typed_column("id", DataType::Int64),
        TypedBinaryOp::Eq,
        typed_constant(Value::Int64(42), DataType::Int64),
        DataType::Boolean,
    );

    let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
    // Should NOT use the partial index since the predicate isn't satisfied
    assert!(
        matches!(path.scan_type, ScanType::FullTableScan),
        "expected FullTableScan when partial predicate not satisfied, got {:?}",
        path.scan_type
    );
}

#[test]
fn test_canonicalizer_parity_with_ast_lower() {
    // Verify that typed_expr_to_canonical_sql produces strings that normalize
    // to the same value as AST normalize_expr_for_match for common patterns
    let lower_call = TypedExpr {
        kind: TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "lower".to_string(),
                kind: crate::sql::analyzer::types::FunctionKind::Builtin,
                return_type: DataType::Text,
            },
            args: vec![typed_column("name", DataType::Text)],
            order_by: vec![],
            filter: None,
        },
        data_type: DataType::Text,
    };

    let typed_canonical = normalize_expr_string(typed_expr_to_canonical_sql(&lower_call));
    let ast_expr = parse_predicate_expr("lower(name)").unwrap();
    let ast_canonical = normalize_expr_for_match(&ast_expr);
    assert_eq!(typed_canonical, ast_canonical);
}

#[test]
fn test_canonicalizer_parity_with_ast_cast() {
    // CAST(x AS TEXT)
    let cast_expr = TypedExpr {
        kind: TypedExprKind::Cast {
            expr: Box::new(typed_column("x", DataType::Int64)),
            target_type: DataType::Text,
            cast_context: crate::sql::types::CastContext::Explicit,
        },
        data_type: DataType::Text,
    };

    let typed_canonical = normalize_expr_string(typed_expr_to_canonical_sql(&cast_expr));
    // AST: CAST(x AS TEXT) -- sqlparser emits this same form
    let ast_expr = parse_predicate_expr("CAST(x AS TEXT)").unwrap();
    let ast_canonical = normalize_expr_for_match(&ast_expr);
    assert_eq!(typed_canonical, ast_canonical);
}
