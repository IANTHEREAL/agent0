//! Tests for the window function operator.

use super::*;
use crate::sql::analyzer::types::TypedExprKind;
use crate::sql::operators::scan::TableScanOperator;

fn test_schema() -> TableSchema {
    TableSchema {
        name: "sales".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "amount".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
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

/// Helper: creates a TypedExpr::ColumnRef for the given column index and name.
fn col_ref(index: usize, name: &str, data_type: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: index,
            column_name: name.to_string(),
        },
        data_type,
    }
}

#[test]
fn test_window_operator_creation() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "row_number".to_string(),
        arg_expr: None,
        partition_by: vec![],
        order_by: vec![],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "row_num".to_string(),
        output_type: DataType::Int64,
    }];

    let op = WindowOperator::new(child, window_funcs);

    assert_eq!(op.name(), "Window");
    assert_eq!(op.schema().columns.len(), 3); // 2 input + 1 window
    assert_eq!(op.schema().columns[2].name, "row_num");
}

#[test]
fn test_window_operator_explain() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![
        WindowFunctionExpr {
            func_name: "row_number".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "row_num".to_string(),
            output_type: DataType::Int64,
        },
        WindowFunctionExpr {
            func_name: "sum".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "total".to_string(),
            output_type: DataType::Numeric {
                precision: None,
                scale: None,
            },
        },
    ];

    let op = WindowOperator::new(child, window_funcs);

    let info = op.explain_info().unwrap();
    assert!(info.contains("row_number"));
    assert!(info.contains("sum"));
}

fn test_schema_with_float_partition() -> TableSchema {
    TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "grp".to_string(),
                data_type: DataType::Float64,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
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

fn test_schema_with_numeric_partition() -> TableSchema {
    TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "grp".to_string(),
                data_type: DataType::Numeric {
                    precision: None,
                    scale: Some(2),
                },
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
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
fn test_window_operator_partition_by_canonicalizes_float_keys() {
    let schema = test_schema_with_float_partition();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "row_number".to_string(),
        arg_expr: None,
        partition_by: vec![col_ref(1, "grp", DataType::Float64)],
        order_by: vec![],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "row_num".to_string(),
        output_type: DataType::Int64,
    }];

    let op = WindowOperator::new(child, window_funcs);

    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Float64(-0.0)]),
        Row::new(vec![Value::Int32(2), Value::Float64(0.0)]),
    ];
    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();
    assert_eq!(results[0][0], Value::Int64(1));
    assert_eq!(results[1][0], Value::Int64(2));

    let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
    let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
    assert!(nan1.is_nan() && nan2.is_nan());
    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
        Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
    ];
    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();
    assert_eq!(results[0][0], Value::Int64(1));
    assert_eq!(results[1][0], Value::Int64(2));
}

#[test]
fn test_window_operator_partition_by_canonicalizes_numeric_keys() {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let schema = test_schema_with_numeric_partition();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "row_number".to_string(),
        arg_expr: None,
        partition_by: vec![col_ref(
            1,
            "grp",
            DataType::Numeric {
                precision: None,
                scale: Some(2),
            },
        )],
        order_by: vec![],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "row_num".to_string(),
        output_type: DataType::Int64,
    }];

    let op = WindowOperator::new(child, window_funcs);

    let rows = vec![
        Row::new(vec![
            Value::Int32(1),
            Value::Numeric(Decimal::from_str("1.0").unwrap()),
        ]),
        Row::new(vec![
            Value::Int32(2),
            Value::Numeric(Decimal::from_str("1.00").unwrap()),
        ]),
    ];
    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();
    assert_eq!(results[0][0], Value::Int64(1));
    assert_eq!(results[1][0], Value::Int64(2));
}

#[test]
fn test_window_operator_rank_dense_rank_treat_nan_order_keys_as_peers() {
    let schema = test_schema_with_float_partition();
    let child = Box::new(TableScanOperator::new(schema));

    let grp_col = col_ref(1, "grp", DataType::Float64);

    let window_funcs = vec![
        WindowFunctionExpr {
            func_name: "rank".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: grp_col.clone(),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "rank_val".to_string(),
            output_type: DataType::Int64,
        },
        WindowFunctionExpr {
            func_name: "dense_rank".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: grp_col.clone(),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "dense_rank_val".to_string(),
            output_type: DataType::Int64,
        },
    ];

    let op = WindowOperator::new(child, window_funcs);

    let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
    let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
    assert!(nan1.is_nan() && nan2.is_nan());

    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Float64(nan1)]),
        Row::new(vec![Value::Int32(2), Value::Float64(nan2)]),
        Row::new(vec![Value::Int32(3), Value::Float64(1.0)]),
        Row::new(vec![Value::Int32(4), Value::Float64(2.0)]),
    ];

    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    // ORDER BY grp ASC sorts NaNs last; both NaNs are peers.
    assert_eq!(results[0][0], Value::Int64(3));
    assert_eq!(results[1][0], Value::Int64(3));
    assert_eq!(results[2][0], Value::Int64(1));
    assert_eq!(results[3][0], Value::Int64(2));

    assert_eq!(results[0][1], Value::Int64(3));
    assert_eq!(results[1][1], Value::Int64(3));
    assert_eq!(results[2][1], Value::Int64(1));
    assert_eq!(results[3][1], Value::Int64(2));
}

#[test]
fn test_window_row_number_partitioned() {
    let schema = TableSchema {
        name: "test".to_string(),
        table_id: 1,
        columns: vec![
            ColumnDef {
                name: "dept".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![1],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    };

    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "row_number".to_string(),
        arg_expr: None,
        partition_by: vec![col_ref(0, "dept", DataType::Text)],
        order_by: vec![TypedOrderByExpr {
            expr: col_ref(1, "id", DataType::Int32),
            asc: true,
            nulls_first: false,
        }],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "rn".to_string(),
        output_type: DataType::Int64,
    }];

    let op = WindowOperator::new(child, window_funcs);

    let rows = vec![
        Row::new(vec![Value::Text("A".to_string()), Value::Int32(1)]),
        Row::new(vec![Value::Text("A".to_string()), Value::Int32(2)]),
        Row::new(vec![Value::Text("B".to_string()), Value::Int32(3)]),
        Row::new(vec![Value::Text("B".to_string()), Value::Int32(4)]),
        Row::new(vec![Value::Text("B".to_string()), Value::Int32(5)]),
    ];

    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    // Partition A: row_number 1,2; Partition B: row_number 1,2,3
    assert_eq!(results[0][0], Value::Int64(1));
    assert_eq!(results[1][0], Value::Int64(2));
    assert_eq!(results[2][0], Value::Int64(1));
    assert_eq!(results[3][0], Value::Int64(2));
    assert_eq!(results[4][0], Value::Int64(3));
}

#[test]
fn test_window_sum_aggregate() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "sum".to_string(),
        arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
        partition_by: vec![],
        order_by: vec![TypedOrderByExpr {
            expr: col_ref(0, "id", DataType::Int32),
            asc: true,
            nulls_first: false,
        }],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "running_sum".to_string(),
        output_type: DataType::Numeric {
            precision: None,
            scale: None,
        },
    }];

    let op = WindowOperator::new(child, window_funcs);

    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Int32(30)]),
    ];

    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    // Running sum: 10, 30, 60
    assert_eq!(
        results[0][0],
        Value::Numeric(rust_decimal::Decimal::from(10))
    );
    assert_eq!(
        results[1][0],
        Value::Numeric(rust_decimal::Decimal::from(30))
    );
    assert_eq!(
        results[2][0],
        Value::Numeric(rust_decimal::Decimal::from(60))
    );
}

#[test]
fn test_window_count_no_order_by() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "count".to_string(),
        arg_expr: None,
        partition_by: vec![],
        order_by: vec![],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "total_count".to_string(),
        output_type: DataType::Int64,
    }];

    let op = WindowOperator::new(child, window_funcs);

    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Int32(30)]),
    ];

    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    // Without ORDER BY, COUNT(*) OVER() returns total count for all rows
    assert_eq!(results[0][0], Value::Int64(3));
    assert_eq!(results[1][0], Value::Int64(3));
    assert_eq!(results[2][0], Value::Int64(3));
}
