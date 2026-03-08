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
            generation_expr: None,
            generation_expr_authorized_by: None,
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
            generation_expr: None,
            generation_expr_authorized_by: None,
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
            generation_expr: None,
            generation_expr_authorized_by: None,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
                hnsw_m: None,
                hnsw_ef_construction: None,
                hnsw_distance_metric: None,
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
                hnsw_m: None,
                hnsw_ef_construction: None,
                hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
            generation_expr: None,
            generation_expr_authorized_by: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
            generation_expr: None,
            generation_expr_authorized_by: None,
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

    let path = choose_btree_access_path_for_typed_filter(&schema, &filter, 10000, None);
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

use super::cost_model::CostModel;
use super::index_selection::compute_inlist_selectivity;
use crate::model::ColumnDef;
use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics};
use std::collections::HashMap;

fn build_single_column_schema(unique: bool) -> (TableSchema, IndexDef) {
    let index = IndexDef {
        id: 100,
        name: if unique {
            "idx_status_unique".to_string()
        } else {
            "idx_status".to_string()
        },
        columns: vec!["status".to_string()],
        unique,
        is_constraint: false,
        method: None,
        predicate: None,
        expressions: Vec::new(),
        state: crate::worker::types::IndexState::Ready,
        hnsw_m: None,
        hnsw_ef_construction: None,
        hnsw_distance_metric: None,
        cached_predicate_conjuncts: None,
    };

    let schema = TableSchema {
        name: "orders".to_string(),
        table_id: 1,
        columns: vec![ColumnDef {
            name: "status".to_string(),
            data_type: DataType::Text,
            nullable: true,
            primary_key: false,
            unique,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![index.clone()],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    (schema, index)
}

fn build_table_stats(column_name: &str, n_distinct: f64, null_fraction: f64) -> TableStatistics {
    let mut columns = HashMap::new();
    columns.insert(
        column_name.to_string(),
        ColumnStatistics {
            null_fraction,
            n_distinct,
            avg_width: 0,
            most_common_vals: Vec::new(),
            most_common_freqs: Vec::new(),
            histogram_bounds: Vec::new(),
            correlation: 0.0,
        },
    );

    TableStatistics {
        table_id: 1,
        row_count: 100000,
        last_analyzed: 0,
        columns,
    }
}

#[test]
fn test_compute_inlist_selectivity_ndv_positive() {
    let (schema, index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 3.0, 0.0);
    let in_values = vec![
        Value::Text("active".to_string()),
        Value::Text("pending".to_string()),
    ];

    let sel = compute_inlist_selectivity(&in_values, &index, &[], 100000, Some(&stats), &schema);
    assert!((sel - (2.0 / 3.0)).abs() < 1e-9, "expected 2/3, got {sel}");
}

#[test]
fn test_compute_inlist_selectivity_unique_full_prefix_no_floor() {
    let (schema, index) = build_single_column_schema(true);
    let in_values = vec![Value::Text("a".to_string()), Value::Text("b".to_string())];

    let sel = compute_inlist_selectivity(&in_values, &index, &[], 100000, None, &schema);
    assert!((sel - 0.00002).abs() < 1e-12, "expected 0.00002, got {sel}");
    assert!(sel < CostModel::MIN_SELECTIVITY_FLOOR);
}

#[test]
fn test_compute_inlist_selectivity_negative_ndistinct() {
    let (schema, index) = build_single_column_schema(false);
    let stats = build_table_stats("status", -0.1, 0.0);
    let in_values = vec![Value::Text("a".to_string()), Value::Text("b".to_string())];

    let sel = compute_inlist_selectivity(&in_values, &index, &[], 100000, Some(&stats), &schema);
    assert!((sel - 0.0002).abs() < 1e-12, "expected 0.0002, got {sel}");
}

#[test]
fn test_compute_inlist_selectivity_null_fraction_scaling() {
    let (schema, index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 5.0, 0.8);
    let in_values = vec![Value::Text("a".to_string()), Value::Text("b".to_string())];

    let sel = compute_inlist_selectivity(&in_values, &index, &[], 100000, Some(&stats), &schema);
    assert!((sel - 0.08).abs() < 1e-12, "expected 0.08, got {sel}");
}

#[test]
fn test_compute_inlist_selectivity_no_stats_fallback() {
    let (schema, index) = build_single_column_schema(false);
    let in_values = vec![
        Value::Text("a".to_string()),
        Value::Text("b".to_string()),
        Value::Text("c".to_string()),
    ];

    let sel = compute_inlist_selectivity(&in_values, &index, &[], 100000, None, &schema);
    assert!((sel - 0.3).abs() < 1e-12, "expected 0.3, got {sel}");
}

#[test]
fn test_compute_inlist_selectivity_normalized_input() {
    let (schema, index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 5.0, 0.0);
    let normalized_values = vec![Value::Text("a".to_string())];

    let sel = compute_inlist_selectivity(
        &normalized_values,
        &index,
        &[],
        100000,
        Some(&stats),
        &schema,
    );
    assert!((sel - 0.2).abs() < 1e-12, "expected 0.2, got {sel}");
}

#[test]
fn test_inlist_mixed_null_and_duplicates_matches_normalized_scan_keys_and_cost() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let index = IndexDef {
        id: 7,
        name: "idx_items_id".to_string(),
        columns: vec!["id".to_string()],
        unique: false,
        is_constraint: false,
        method: None,
        predicate: None,
        expressions: Vec::new(),
        state: crate::worker::types::IndexState::Ready,
        hnsw_m: None,
        hnsw_ef_construction: None,
        hnsw_distance_metric: None,
        cached_predicate_conjuncts: None,
    };
    let schema = TableSchema {
        name: "items".to_string(),
        table_id: 1,
        columns: vec![ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![index],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let stats = build_table_stats("id", 1000.0, 0.0);

    let mixed_filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("id", DataType::Int32)),
            list: vec![
                typed_constant(Value::Int32(1), DataType::Int32),
                typed_constant(Value::Int32(1), DataType::Int32),
                typed_constant(Value::Null, DataType::Int32),
                typed_constant(Value::Int32(2), DataType::Int32),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };
    let normalized_filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("id", DataType::Int32)),
            list: vec![
                typed_constant(Value::Int32(1), DataType::Int32),
                typed_constant(Value::Int32(2), DataType::Int32),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let mixed_path =
        choose_btree_access_path_for_typed_filter(&schema, &mixed_filter, 100_000, Some(&stats));
    let normalized_path = choose_btree_access_path_for_typed_filter(
        &schema,
        &normalized_filter,
        100_000,
        Some(&stats),
    );

    let mixed_keys = match &mixed_path.scan_type {
        ScanType::InListScan { column_values, .. } => column_values,
        other => panic!("expected InListScan for mixed IN list, got {:?}", other),
    };
    let normalized_keys = match &normalized_path.scan_type {
        ScanType::InListScan { column_values, .. } => column_values,
        other => panic!(
            "expected InListScan for normalized IN list, got {:?}",
            other
        ),
    };

    assert_eq!(
        mixed_keys, normalized_keys,
        "IN (1, 1, NULL, 2) must generate the same scan keys as IN (1, 2)"
    );
    let expected_keys = vec![vec![Value::Int32(1)], vec![Value::Int32(2)]];
    assert_eq!(
        mixed_keys, &expected_keys,
        "scan keys should be deduplicated and NULL-free"
    );
    assert!(
        (mixed_path.cost - normalized_path.cost).abs() < 1e-12,
        "IN (1, 1, NULL, 2) must have the same planned cost as IN (1, 2)"
    );
}

#[test]
fn test_inlist_mixed_sign_nan_deduplicates_to_one_effective_nan() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let index = IndexDef {
        id: 8,
        name: "idx_items_score".to_string(),
        columns: vec!["score".to_string()],
        unique: false,
        is_constraint: false,
        method: None,
        predicate: None,
        expressions: Vec::new(),
        state: crate::worker::types::IndexState::Ready,
        hnsw_m: None,
        hnsw_ef_construction: None,
        hnsw_distance_metric: None,
        cached_predicate_conjuncts: None,
    };
    let schema = TableSchema {
        name: "items".to_string(),
        table_id: 1,
        columns: vec![ColumnDef {
            name: "score".to_string(),
            data_type: DataType::Float64,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        }],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![index],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };
    let stats = build_table_stats("score", 10.0, 0.0);

    let neg_nan = -f64::NAN;

    let mixed_nan_filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("score", DataType::Float64)),
            list: vec![
                typed_constant(Value::Float64(neg_nan), DataType::Float64),
                typed_constant(Value::Float64(1.0), DataType::Float64),
                typed_constant(Value::Float64(f64::NAN), DataType::Float64),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };
    let normalized_filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("score", DataType::Float64)),
            list: vec![
                typed_constant(Value::Float64(1.0), DataType::Float64),
                typed_constant(Value::Float64(f64::NAN), DataType::Float64),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let mixed_path = choose_btree_access_path_for_typed_filter(
        &schema,
        &mixed_nan_filter,
        100_000,
        Some(&stats),
    );
    let normalized_path = choose_btree_access_path_for_typed_filter(
        &schema,
        &normalized_filter,
        100_000,
        Some(&stats),
    );

    let mixed_keys = match &mixed_path.scan_type {
        ScanType::InListScan { column_values, .. } => column_values,
        other => panic!("expected InListScan for mixed NaN list, got {:?}", other),
    };
    let normalized_keys = match &normalized_path.scan_type {
        ScanType::InListScan { column_values, .. } => column_values,
        other => panic!("expected InListScan for normalized list, got {:?}", other),
    };

    // assert_eq! uses PartialEq where NaN != NaN, so compare lengths instead.
    assert_eq!(
        mixed_keys.len(),
        normalized_keys.len(),
        "IN (-NaN, 1.0, NaN) must normalize to the same scan key count as IN (1.0, NaN)"
    );
    assert_eq!(mixed_keys.len(), 2);
    assert_eq!(mixed_keys[0].len(), 1);
    assert_eq!(mixed_keys[1].len(), 1);

    let nan_key_count = mixed_keys
        .iter()
        .filter(|key| matches!(key[0], Value::Float64(v) if v.is_nan()))
        .count();
    let one_key_count = mixed_keys
        .iter()
        .filter(|key| matches!(key[0], Value::Float64(v) if v == 1.0))
        .count();
    assert_eq!(
        nan_key_count, 1,
        "IN (-NaN, 1.0, NaN) should retain exactly one NaN key after dedup"
    );
    assert_eq!(one_key_count, 1, "IN list should retain the finite key 1.0");
    assert!(
        (mixed_path.cost - normalized_path.cost).abs() < 1e-12,
        "IN (-NaN, 1.0, NaN) must have the same planned cost as IN (1.0, NaN)"
    );
}

/// Motivating case from issue #1231: table with 100k rows, non-unique `status`
/// column with 3 distinct values, query `WHERE status IN ('active', 'pending')`.
/// Selectivity = 2/3 > 0.3 threshold → planner must choose FullTableScan.
#[test]
fn test_inlist_high_selectivity_prefers_full_scan() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let (schema, _index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 3.0, 0.0);
    let table_rows = 100_000;

    // Construct: status IN ('active', 'pending')
    let filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("status", DataType::Text)),
            list: vec![
                typed_constant(Value::Text("active".to_string()), DataType::Text),
                typed_constant(Value::Text("pending".to_string()), DataType::Text),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let path =
        choose_btree_access_path_for_typed_filter(&schema, &filter, table_rows, Some(&stats));
    assert!(
        matches!(path.scan_type, ScanType::FullTableScan),
        "expected FullTableScan for high-selectivity InList (sel=2/3), got {:?} with cost {}",
        path.scan_type,
        path.cost,
    );
}

/// Low-selectivity InList should still prefer index scan.
#[test]
fn test_inlist_low_selectivity_prefers_index_scan() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let (schema, _index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 1000.0, 0.0);
    let table_rows = 100_000;

    // Construct: status IN ('active', 'pending') — only 2 of 1000 distinct values
    let filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("status", DataType::Text)),
            list: vec![
                typed_constant(Value::Text("active".to_string()), DataType::Text),
                typed_constant(Value::Text("pending".to_string()), DataType::Text),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let path =
        choose_btree_access_path_for_typed_filter(&schema, &filter, table_rows, Some(&stats));
    assert!(
        matches!(path.scan_type, ScanType::InListScan { .. }),
        "expected InListScan for low-selectivity InList (sel=2/1000), got {:?} with cost {}",
        path.scan_type,
        path.cost,
    );
}

/// Boundary case: selectivity exactly equals threshold (0.3) and should NOT flip
/// to full scan because index_scan_cost uses a strict `>` comparison.
#[test]
fn test_inlist_selectivity_at_threshold_prefers_index_scan() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let (schema, _index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 10.0, 0.0);
    let table_rows = 1_000;

    // Selectivity = 3/10 = 0.3 (exact threshold)
    let filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("status", DataType::Text)),
            list: vec![
                typed_constant(Value::Text("a".to_string()), DataType::Text),
                typed_constant(Value::Text("b".to_string()), DataType::Text),
                typed_constant(Value::Text("c".to_string()), DataType::Text),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let path =
        choose_btree_access_path_for_typed_filter(&schema, &filter, table_rows, Some(&stats));
    assert!(
        matches!(path.scan_type, ScanType::InListScan { .. }),
        "expected InListScan at threshold (sel=0.3, strict >), got {:?} with cost {}",
        path.scan_type,
        path.cost,
    );
}

/// Boundary case just above threshold: selectivity > 0.3 should flip to full scan.
#[test]
fn test_inlist_selectivity_above_threshold_prefers_full_scan() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let (schema, _index) = build_single_column_schema(false);
    let stats = build_table_stats("status", 10.0, 0.0);
    let table_rows = 1_000;

    // Selectivity = 4/10 = 0.4 (> threshold)
    let filter = TypedExpr {
        kind: TypedExprKind::InList {
            expr: Box::new(typed_column("status", DataType::Text)),
            list: vec![
                typed_constant(Value::Text("a".to_string()), DataType::Text),
                typed_constant(Value::Text("b".to_string()), DataType::Text),
                typed_constant(Value::Text("c".to_string()), DataType::Text),
                typed_constant(Value::Text("d".to_string()), DataType::Text),
            ],
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let path =
        choose_btree_access_path_for_typed_filter(&schema, &filter, table_rows, Some(&stats));
    assert!(
        matches!(path.scan_type, ScanType::FullTableScan),
        "expected FullTableScan above threshold (sel=0.4), got {:?} with cost {}",
        path.scan_type,
        path.cost,
    );
}

/// Composite index (tenant_id, status): `tenant_id = ? AND status IN (a, b)`.
/// Without prefix selectivity factoring, the IN-list selectivity alone (2/5 = 0.4)
/// exceeds the 0.3 threshold and the planner incorrectly picks FullTableScan.
/// With prefix selectivity (1/100 for tenant_id), the combined selectivity is
/// 1/100 * 2/5 = 0.004, well below threshold → InListScan must be chosen.
#[test]
fn test_composite_index_inlist_factors_prefix_equality_selectivity() {
    use super::index_selection::choose_btree_access_path_for_typed_filter;

    let index = IndexDef {
        id: 10,
        name: "idx_tenant_status".to_string(),
        columns: vec!["tenant_id".to_string(), "status".to_string()],
        unique: false,
        is_constraint: false,
        method: None,
        predicate: None,
        expressions: Vec::new(),
        state: crate::worker::types::IndexState::Ready,
        hnsw_m: None,
        hnsw_ef_construction: None,
        hnsw_distance_metric: None,
        cached_predicate_conjuncts: None,
    };
    let schema = TableSchema {
        name: "events".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "tenant_id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
            ColumnDef {
                name: "status".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![index],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    // Stats: tenant_id has 100 distinct values, status has 5
    let mut columns = HashMap::new();
    columns.insert(
        "tenant_id".to_string(),
        ColumnStatistics {
            null_fraction: 0.0,
            n_distinct: 100.0,
            avg_width: 0,
            most_common_vals: Vec::new(),
            most_common_freqs: Vec::new(),
            histogram_bounds: Vec::new(),
            correlation: 0.0,
        },
    );
    columns.insert(
        "status".to_string(),
        ColumnStatistics {
            null_fraction: 0.0,
            n_distinct: 5.0,
            avg_width: 0,
            most_common_vals: Vec::new(),
            most_common_freqs: Vec::new(),
            histogram_bounds: Vec::new(),
            correlation: 0.0,
        },
    );
    let stats = TableStatistics {
        table_id: 1,
        row_count: 100_000,
        last_analyzed: 0,
        columns,
    };

    let table_rows = 100_000;

    // WHERE tenant_id = 42 AND status IN ('active', 'pending')
    let filter = typed_binop(
        typed_binop(
            typed_column("tenant_id", DataType::Int64),
            TypedBinaryOp::Eq,
            typed_constant(Value::Int64(42), DataType::Int64),
            DataType::Boolean,
        ),
        TypedBinaryOp::And,
        TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(typed_column("status", DataType::Text)),
                list: vec![
                    typed_constant(Value::Text("active".to_string()), DataType::Text),
                    typed_constant(Value::Text("pending".to_string()), DataType::Text),
                ],
                negated: false,
            },
            data_type: DataType::Boolean,
        },
        DataType::Boolean,
    );

    let path =
        choose_btree_access_path_for_typed_filter(&schema, &filter, table_rows, Some(&stats));
    assert!(
        matches!(path.scan_type, ScanType::InListScan { .. }),
        "expected InListScan for composite-index IN-list with prefix equality \
         (combined sel = 1/100 * 2/5 = 0.004), got {:?} with cost {}",
        path.scan_type,
        path.cost,
    );
}
