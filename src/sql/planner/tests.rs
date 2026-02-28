//! Tests for the query planner module.
//!
//! Tests exercise the TypedExpr-based index selection pipeline
//! (choose_btree_access_path_for_typed_filter) and the expression
//! normalization utilities.

use super::scan_type::{
    normalize_expr_for_match, normalize_expr_string, parse_predicate_expr,
    typed_expr_to_canonical_sql,
};
use super::*;
use crate::model::{IndexDef, TableSchema};

use crate::model::DataType;
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

fn orders_columns_with_region() -> Vec<crate::model::ColumnDef> {
    vec![
        crate::model::ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        },
        crate::model::ColumnDef {
            name: "status".to_string(),
            data_type: DataType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        },
        crate::model::ColumnDef {
            name: "region".to_string(),
            data_type: DataType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        },
    ]
}

fn gin_schema() -> TableSchema {
    TableSchema {
        name: "docs".to_string(),
        table_id: 1,
        columns: vec![
            crate::model::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::model::ColumnDef {
                name: "body".to_string(),
                data_type: DataType::Tsvector,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::model::ColumnDef {
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
                is_constraint: false,
                method: Some("gin".to_string()),
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
                cached_predicate_conjuncts: None,
            },
            IndexDef {
                id: 2,
                name: "idx_data_gin".to_string(),
                columns: vec!["data".to_string()],
                unique: false,
                is_constraint: false,
                method: Some("gin".to_string()),
                predicate: None,
                expressions: Vec::new(),
                state: crate::worker::types::IndexState::Ready,
                cached_predicate_conjuncts: None,
            },
        ],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

#[test]
fn test_gin_typed_tsmatch_selects_gin_scan() {
    let schema = gin_schema();
    // body @@ to_tsquery('hello')
    let filter = typed_binop(
        typed_column("body", DataType::Tsvector),
        TypedBinaryOp::TsMatch,
        typed_constant(Value::Tsquery("hello".to_string()), DataType::Tsquery),
        DataType::Boolean,
    );

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    assert!(
        matches!(path.scan_type, ScanType::GinIndexScan { ref index_name, .. } if index_name == "idx_body_gin"),
        "expected GinIndexScan on idx_body_gin, got {:?}",
        path.scan_type,
    );
}

#[test]
fn test_gin_typed_json_contains_selects_gin_scan() {
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    assert!(
        matches!(path.scan_type, ScanType::GinIndexScan { ref index_name, .. } if index_name == "idx_data_gin"),
        "expected GinIndexScan on idx_data_gin, got {:?}",
        path.scan_type,
    );
}

#[test]
fn test_gin_typed_no_gin_index_falls_back() {
    // Schema without GIN index
    let schema = TableSchema {
        name: "plain".to_string(),
        table_id: 1,
        columns: vec![crate::model::ColumnDef {
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    assert!(matches!(path.scan_type, ScanType::FullTableScan));
}

#[test]
fn test_expression_index_typed_lower() {
    // Schema with expression index on lower(name)
    let schema = TableSchema {
        name: "users".to_string(),
        table_id: 1,
        columns: vec![crate::model::ColumnDef {
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
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(name)".to_string()],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
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
            crate::model::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::model::ColumnDef {
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
            is_constraint: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
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
            crate::model::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            crate::model::ColumnDef {
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
            is_constraint: false,
            method: None,
            predicate: Some("status = 'active'".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    // Should NOT use the partial index since the predicate isn't satisfied
    assert!(
        matches!(path.scan_type, ScanType::FullTableScan),
        "expected FullTableScan when partial predicate not satisfied, got {:?}",
        path.scan_type
    );
}

#[test]
fn test_partial_index_typed_valid_cached_predicate() {
    let schema = TableSchema {
        name: "orders".to_string(),
        table_id: 1,
        columns: orders_columns_with_region(),
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_active_orders".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            // Intentionally unparseable: if the cached branch at
            // index_selection.rs:370 were broken, build_predicate_conjunct_cache
            // would fail to parse this and the index would be skipped.
            predicate: Some("$UNPARSEABLE$".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: Some(vec!["status = 'active'".to_string()]),
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

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
            typed_constant(Value::Int64(7), DataType::Int64),
            DataType::Boolean,
        ),
        DataType::Boolean,
    );

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    assert!(
        matches!(path.scan_type, ScanType::IndexScan { ref index_name, .. } if index_name == "idx_active_orders"),
        "expected cached partial index match, got {:?}",
        path.scan_type
    );
}

#[test]
fn test_partial_index_typed_malformed_predicate() {
    let schema = TableSchema {
        name: "orders".to_string(),
        table_id: 1,
        columns: orders_columns_with_region(),
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_bad_partial".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: Some("status = 'active' AND".to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

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
            typed_constant(Value::Int64(7), DataType::Int64),
            DataType::Boolean,
        ),
        DataType::Boolean,
    );

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    assert!(
        matches!(path.scan_type, ScanType::FullTableScan),
        "expected FullTableScan for malformed predicate without cache, got {:?}",
        path.scan_type
    );
}

#[test]
fn test_partial_index_typed_multi_conjunct_predicate() {
    let predicate = "status = 'active' AND region = 'us'";
    let schema = TableSchema {
        name: "orders".to_string(),
        table_id: 1,
        columns: orders_columns_with_region(),
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_active_us_orders".to_string(),
            columns: vec!["id".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: Some(predicate.to_string()),
            expressions: Vec::new(),
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
        }],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    let status_and_region = typed_binop(
        typed_binop(
            typed_column("status", DataType::Text),
            TypedBinaryOp::Eq,
            typed_constant(Value::Text("active".to_string()), DataType::Text),
            DataType::Boolean,
        ),
        TypedBinaryOp::And,
        typed_binop(
            typed_column("region", DataType::Text),
            TypedBinaryOp::Eq,
            typed_constant(Value::Text("us".to_string()), DataType::Text),
            DataType::Boolean,
        ),
        DataType::Boolean,
    );
    let filter = typed_binop(
        status_and_region,
        TypedBinaryOp::And,
        typed_binop(
            typed_column("id", DataType::Int64),
            TypedBinaryOp::Eq,
            typed_constant(Value::Int64(11), DataType::Int64),
            DataType::Boolean,
        ),
        DataType::Boolean,
    );

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000);
    assert!(
        matches!(path.scan_type, ScanType::IndexScan { ref index_name, .. } if index_name == "idx_active_us_orders"),
        "expected multi-conjunct cached partial index match, got {:?}",
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
