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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
            ColumnDef {
                name: "amount".to_string(),
                data_type: DataType::Int32,
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
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
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

fn const_int(v: i32) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::Constant(Value::Int32(v)),
        data_type: DataType::Int32,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
            ColumnDef {
                name: "grp".to_string(),
                data_type: DataType::Float64,
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
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
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
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            },
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
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
        pk_indices: vec![1],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
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
fn test_window_aggregates_compute_expected_results() {
    use rust_decimal::Decimal;

    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));
    let amount_col = col_ref(1, "amount", DataType::Int32);

    let window_funcs = vec![
        WindowFunctionExpr {
            func_name: "sum".to_string(),
            arg_expr: Some(amount_col.clone()),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "sum_v".to_string(),
            output_type: DataType::Numeric {
                precision: None,
                scale: None,
            },
        },
        WindowFunctionExpr {
            func_name: "count".to_string(),
            arg_expr: Some(amount_col.clone()),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "count_v".to_string(),
            output_type: DataType::Int64,
        },
        WindowFunctionExpr {
            func_name: "count".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "count_star".to_string(),
            output_type: DataType::Int64,
        },
        WindowFunctionExpr {
            func_name: "avg".to_string(),
            arg_expr: Some(amount_col.clone()),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "avg_v".to_string(),
            output_type: DataType::Numeric {
                precision: None,
                scale: None,
            },
        },
        WindowFunctionExpr {
            func_name: "min".to_string(),
            arg_expr: Some(amount_col.clone()),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "min_v".to_string(),
            output_type: DataType::Int32,
        },
        WindowFunctionExpr {
            func_name: "max".to_string(),
            arg_expr: Some(amount_col),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "max_v".to_string(),
            output_type: DataType::Int32,
        },
    ];

    let op = WindowOperator::new(child, window_funcs);
    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Null]),
    ];

    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    for row_result in results {
        assert_eq!(row_result[0], Value::Numeric(Decimal::from(30)));
        assert_eq!(row_result[1], Value::Int64(2));
        assert_eq!(row_result[2], Value::Int64(3));
        assert_eq!(row_result[3], Value::Numeric(Decimal::from(15)));
        assert_eq!(row_result[4], Value::Int32(10));
        assert_eq!(row_result[5], Value::Int32(20));
    }
}

#[test]
fn test_window_operator_rejects_unsupported_function_name() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));
    let window_funcs = vec![WindowFunctionExpr {
        func_name: "made_up_window_fn".to_string(),
        arg_expr: None,
        partition_by: vec![],
        order_by: vec![],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "x".to_string(),
        output_type: DataType::Int64,
    }];

    let op = WindowOperator::new(child, window_funcs);
    let rows = vec![Row::new(vec![Value::Int32(1), Value::Int32(10)])];
    let err = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap_err()
        .to_string();
    assert!(err.contains("Unsupported window function"));
}

#[test]
fn test_window_ntile_percent_rank_and_cume_dist() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));
    let id_col = col_ref(0, "id", DataType::Int32);

    let window_funcs = vec![
        WindowFunctionExpr {
            func_name: "ntile".to_string(),
            arg_expr: Some(const_int(3)),
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: id_col.clone(),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "tile".to_string(),
            output_type: DataType::Int64,
        },
        WindowFunctionExpr {
            func_name: "percent_rank".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: id_col.clone(),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "pr".to_string(),
            output_type: DataType::Float64,
        },
        WindowFunctionExpr {
            func_name: "cume_dist".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: id_col,
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "cd".to_string(),
            output_type: DataType::Float64,
        },
    ];

    let op = WindowOperator::new(child, window_funcs);
    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Int32(30)]),
        Row::new(vec![Value::Int32(4), Value::Int32(40)]),
        Row::new(vec![Value::Int32(5), Value::Int32(50)]),
    ];
    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    let expected_tiles = [1, 1, 2, 2, 3];
    let expected_percent_rank = [0.0, 0.25, 0.5, 0.75, 1.0];
    let expected_cume_dist = [0.2, 0.4, 0.6, 0.8, 1.0];
    for (i, row_result) in results.iter().enumerate() {
        assert_eq!(row_result[0], Value::Int64(expected_tiles[i]));
        assert_eq!(row_result[1], Value::Float64(expected_percent_rank[i]));
        assert_eq!(row_result[2], Value::Float64(expected_cume_dist[i]));
    }
}

#[test]
fn test_window_ntile_argument_validation_errors() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));
    let id_col = col_ref(0, "id", DataType::Int32);
    let rows = vec![Row::new(vec![Value::Int32(1), Value::Int32(10)])];

    let op_zero = WindowOperator::new(
        child,
        vec![WindowFunctionExpr {
            func_name: "ntile".to_string(),
            arg_expr: Some(const_int(0)),
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: id_col,
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "tile".to_string(),
            output_type: DataType::Int64,
        }],
    );
    let err = op_zero
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap_err()
        .to_string();
    assert!(err.contains("positive integer"));
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

#[test]
fn test_window_lag_and_lead_basic() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![
        WindowFunctionExpr {
            func_name: "lag".to_string(),
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
            output_name: "lag_amount".to_string(),
            output_type: DataType::Int32,
        },
        WindowFunctionExpr {
            func_name: "lead".to_string(),
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
            output_name: "lead_amount".to_string(),
            output_type: DataType::Int32,
        },
    ];

    let op = WindowOperator::new(child, window_funcs);
    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Int32(30)]),
    ];

    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    assert_eq!(results[0][0], Value::Null);
    assert_eq!(results[1][0], Value::Int32(10));
    assert_eq!(results[2][0], Value::Int32(20));

    assert_eq!(results[0][1], Value::Int32(20));
    assert_eq!(results[1][1], Value::Int32(30));
    assert_eq!(results[2][1], Value::Null);
}

#[test]
fn test_window_lag_with_offset_and_default_value() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "lag".to_string(),
        arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
        partition_by: vec![],
        order_by: vec![TypedOrderByExpr {
            expr: col_ref(0, "id", DataType::Int32),
            asc: true,
            nulls_first: false,
        }],
        offset_expr: Some(TypedExpr {
            kind: TypedExprKind::Constant(Value::Int32(2)),
            data_type: DataType::Int32,
        }),
        default_value_expr: Some(TypedExpr {
            kind: TypedExprKind::Constant(Value::Int32(999)),
            data_type: DataType::Int32,
        }),
        window_frame: None,
        filter_expr: None,
        output_name: "lag2_amount".to_string(),
        output_type: DataType::Int32,
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
    assert_eq!(results[0][0], Value::Int32(999));
    assert_eq!(results[1][0], Value::Int32(999));
    assert_eq!(results[2][0], Value::Int32(10));
}

#[test]
fn test_window_lag_negative_offset_errors() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let window_funcs = vec![WindowFunctionExpr {
        func_name: "lag".to_string(),
        arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
        partition_by: vec![],
        order_by: vec![],
        offset_expr: Some(TypedExpr {
            kind: TypedExprKind::Constant(Value::Int32(-1)),
            data_type: DataType::Int32,
        }),
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "bad_lag".to_string(),
        output_type: DataType::Int32,
    }];

    let op = WindowOperator::new(child, window_funcs);
    let rows = vec![Row::new(vec![Value::Int32(1), Value::Int32(10)])];

    let err = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap_err()
        .to_string();
    assert!(err.contains("LAG offset must be non-negative"));
}

#[test]
fn test_window_nth_value_basic_and_errors() {
    let schema = test_schema();

    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Int32(30)]),
    ];

    let ok_op = WindowOperator::new(
        Box::new(TableScanOperator::new(schema.clone())),
        vec![WindowFunctionExpr {
            func_name: "nth_value".to_string(),
            arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "id", DataType::Int32),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: Some(TypedExpr {
                kind: TypedExprKind::Constant(Value::Int32(2)),
                data_type: DataType::Int32,
            }),
            default_value_expr: None,
            window_frame: Some(WindowFrame {
                units: crate::sql::analyzer::types::WindowFrameUnits::Rows,
                start: crate::sql::analyzer::types::WindowFrameBound::Preceding(None),
                end: Some(crate::sql::analyzer::types::WindowFrameBound::Following(
                    None,
                )),
            }),
            filter_expr: None,
            output_name: "nth2".to_string(),
            output_type: DataType::Int32,
        }],
    );
    let ok_results = ok_op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();
    assert_eq!(ok_results[0][0], Value::Int32(20));
    assert_eq!(ok_results[1][0], Value::Int32(20));
    assert_eq!(ok_results[2][0], Value::Int32(20));

    let missing_arg_op = WindowOperator::new(
        Box::new(TableScanOperator::new(schema.clone())),
        vec![WindowFunctionExpr {
            func_name: "nth_value".to_string(),
            arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "nth_bad".to_string(),
            output_type: DataType::Int32,
        }],
    );
    let err = missing_arg_op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap_err()
        .to_string();
    assert!(err.contains("NTH_VALUE requires two arguments"));

    let non_positive_op = WindowOperator::new(
        Box::new(TableScanOperator::new(schema)),
        vec![WindowFunctionExpr {
            func_name: "nth_value".to_string(),
            arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
            partition_by: vec![],
            order_by: vec![],
            offset_expr: Some(TypedExpr {
                kind: TypedExprKind::Constant(Value::Int32(0)),
                data_type: DataType::Int32,
            }),
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "nth_bad2".to_string(),
            output_type: DataType::Int32,
        }],
    );
    let err = non_positive_op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap_err()
        .to_string();
    assert!(err.contains("must be a positive integer"));
}

#[test]
fn test_window_first_last_value_with_rows_frame() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));

    let frame = WindowFrame {
        units: crate::sql::analyzer::types::WindowFrameUnits::Rows,
        start: crate::sql::analyzer::types::WindowFrameBound::CurrentRow,
        end: Some(crate::sql::analyzer::types::WindowFrameBound::CurrentRow),
    };

    let window_funcs = vec![
        WindowFunctionExpr {
            func_name: "first_value".to_string(),
            arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "id", DataType::Int32),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: Some(frame.clone()),
            filter_expr: None,
            output_name: "fv".to_string(),
            output_type: DataType::Int32,
        },
        WindowFunctionExpr {
            func_name: "last_value".to_string(),
            arg_expr: Some(col_ref(1, "amount", DataType::Int32)),
            partition_by: vec![],
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "id", DataType::Int32),
                asc: true,
                nulls_first: false,
            }],
            offset_expr: None,
            default_value_expr: None,
            window_frame: Some(frame),
            filter_expr: None,
            output_name: "lv".to_string(),
            output_type: DataType::Int32,
        },
    ];

    let op = WindowOperator::new(child, window_funcs);
    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Int32(10)]),
        Row::new(vec![Value::Int32(2), Value::Int32(20)]),
        Row::new(vec![Value::Int32(3), Value::Int32(30)]),
    ];
    let results = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap();

    assert_eq!(results[0][0], Value::Int32(10));
    assert_eq!(results[1][0], Value::Int32(20));
    assert_eq!(results[2][0], Value::Int32(30));

    assert_eq!(results[0][1], Value::Int32(10));
    assert_eq!(results[1][1], Value::Int32(20));
    assert_eq!(results[2][1], Value::Int32(30));
}

#[test]
fn test_window_unknown_function_errors() {
    let schema = test_schema();
    let child = Box::new(TableScanOperator::new(schema));
    let op = WindowOperator::new(
        child,
        vec![WindowFunctionExpr {
            func_name: "no_such_window_fn".to_string(),
            arg_expr: None,
            partition_by: vec![],
            order_by: vec![],
            offset_expr: None,
            default_value_expr: None,
            window_frame: None,
            filter_expr: None,
            output_name: "x".to_string(),
            output_type: DataType::Int32,
        }],
    );
    let rows = vec![Row::new(vec![Value::Int32(1), Value::Int32(10)])];
    let err = op
        .compute_window_functions(&rows, &QueryContext::from_task_locals())
        .unwrap_err()
        .to_string();
    assert!(err.contains("Unsupported window function"));
}

#[test]
fn test_window_helper_value_to_decimal_and_filter() {
    let op = WindowOperator::new(Box::new(TableScanOperator::new(test_schema())), vec![]);

    assert_eq!(
        op.value_to_decimal(&Value::Int32(7)),
        Some(rust_decimal::Decimal::from(7))
    );
    assert_eq!(
        op.value_to_decimal(&Value::Int64(8)),
        Some(rust_decimal::Decimal::from(8))
    );
    assert_eq!(
        op.value_to_decimal(&Value::Numeric(rust_decimal::Decimal::from(9))),
        Some(rust_decimal::Decimal::from(9))
    );
    assert!(op.value_to_decimal(&Value::Text("x".to_string())).is_none());

    let row = Row::new(vec![Value::Int32(1)]);
    assert!(WindowOperator::passes_filter(&None, &row, &QueryContext::from_task_locals()).unwrap());

    let true_filter = Some(TypedExpr {
        kind: TypedExprKind::Constant(Value::Boolean(true)),
        data_type: DataType::Boolean,
    });
    let false_filter = Some(TypedExpr {
        kind: TypedExprKind::Constant(Value::Boolean(false)),
        data_type: DataType::Boolean,
    });
    assert!(
        WindowOperator::passes_filter(&true_filter, &row, &QueryContext::from_task_locals())
            .unwrap()
    );
    assert!(
        !WindowOperator::passes_filter(&false_filter, &row, &QueryContext::from_task_locals())
            .unwrap()
    );
}

#[test]
fn test_window_peer_group_helpers() {
    let schema = test_schema_with_float_partition();
    let child = Box::new(TableScanOperator::new(schema));
    let grp_col = col_ref(1, "grp", DataType::Float64);
    let wf = WindowFunctionExpr {
        func_name: "rank".to_string(),
        arg_expr: None,
        partition_by: vec![],
        order_by: vec![TypedOrderByExpr {
            expr: grp_col,
            asc: true,
            nulls_first: false,
        }],
        offset_expr: None,
        default_value_expr: None,
        window_frame: None,
        filter_expr: None,
        output_name: "r".to_string(),
        output_type: DataType::Int64,
    };
    let op = WindowOperator::new(child, vec![]);

    let rows = vec![
        Row::new(vec![Value::Int32(1), Value::Float64(1.0)]),
        Row::new(vec![Value::Int32(2), Value::Float64(1.0)]),
        Row::new(vec![Value::Int32(3), Value::Float64(2.0)]),
    ];
    let row_indices = vec![0, 1, 2];
    let groups = WindowOperator::compute_peer_groups(
        &rows,
        &wf,
        &row_indices,
        &QueryContext::from_task_locals(),
    )
    .unwrap();
    assert_eq!(groups, vec![(0, 2), (2, 3)]);
    assert_eq!(WindowOperator::peer_group_of(&groups, 0), 0);
    assert_eq!(WindowOperator::peer_group_of(&groups, 1), 0);
    assert_eq!(WindowOperator::peer_group_of(&groups, 2), 1);

    assert!(
        super::order_by_values_are_peers(&[Value::Int32(1)], &[Value::Int32(1)], &wf.order_by)
            .unwrap()
    );
    assert!(!super::order_by_values_are_peers(
        &[Value::Int32(1)],
        &[Value::Int32(2)],
        &wf.order_by
    )
    .unwrap());
    let _ = op;
}

#[test]
fn test_window_frame_bounds_rows_groups_and_errors() {
    let op = WindowOperator::new(Box::new(TableScanOperator::new(test_schema())), vec![]);
    let peer_groups = vec![(0, 2), (2, 3)];
    let qctx = QueryContext::from_task_locals();

    let wf_rows = WindowFunctionExpr {
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
        window_frame: Some(WindowFrame {
            units: crate::sql::analyzer::types::WindowFrameUnits::Rows,
            start: crate::sql::analyzer::types::WindowFrameBound::Preceding(Some(Box::new(
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
            ))),
            end: Some(crate::sql::analyzer::types::WindowFrameBound::Following(
                Some(Box::new(TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                })),
            )),
        }),
        filter_expr: None,
        output_name: "x".to_string(),
        output_type: DataType::Numeric {
            precision: None,
            scale: None,
        },
    };

    let (start, end) = op
        .get_frame_bounds(&wf_rows, 1, 3, &peer_groups, &qctx)
        .unwrap();
    assert_eq!((start, end), (0, 3));

    let wf_groups = WindowFunctionExpr {
        window_frame: Some(WindowFrame {
            units: crate::sql::analyzer::types::WindowFrameUnits::Groups,
            start: crate::sql::analyzer::types::WindowFrameBound::CurrentRow,
            end: None,
        }),
        ..wf_rows.clone()
    };
    let (start, end) = op
        .get_frame_bounds(&wf_groups, 0, 3, &peer_groups, &qctx)
        .unwrap();
    assert_eq!((start, end), (0, 2));

    let wf_bad = WindowFunctionExpr {
        window_frame: Some(WindowFrame {
            units: crate::sql::analyzer::types::WindowFrameUnits::Rows,
            start: crate::sql::analyzer::types::WindowFrameBound::Preceding(Some(Box::new(
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(-1)),
                    data_type: DataType::Int32,
                },
            ))),
            end: None,
        }),
        ..wf_rows
    };
    let err = op
        .get_frame_bounds(&wf_bad, 0, 3, &peer_groups, &qctx)
        .unwrap_err()
        .to_string();
    assert!(err.contains("Invalid window frame bound"));
}
