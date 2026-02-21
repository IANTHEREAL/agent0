//! Tests for logical planner.

use super::*;
use crate::sql::analyzer::types::*;
use crate::types::DataType;

fn simple_column(name: &str, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: 0,
            column_name: name.to_string(),
        },
        data_type: dt.clone(),
    }
}

fn simple_constant(v: crate::types::Value, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::Constant(v),
        data_type: dt,
    }
}

fn simple_projection(name: &str, dt: DataType) -> AnalyzedProjection {
    AnalyzedProjection {
        expr: simple_column(name, dt),
        output_name: name.to_string(),
    }
}

/// Single-table SELECT: SELECT id, name FROM users WHERE id = 1
#[test]
fn test_single_table_select() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![
                simple_projection("id", DataType::Int64),
                simple_projection("name", DataType::Text),
            ],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "users".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![
                            ("id".to_string(), DataType::Int64, false),
                            ("name".to_string(), DataType::Text, true),
                        ],
                    },
                },
                alias: None,
            }],
            where_clause: Some(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(simple_column("id", DataType::Int64)),
                    op: BinaryOp::Eq,
                    right: Box::new(simple_constant(
                        crate::types::Value::Int64(1),
                        DataType::Int64,
                    )),
                },
                data_type: DataType::Boolean,
            }),
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Project → Filter → Scan
    assert!(matches!(plan.node, LogicalNode::Project { .. }));
    if let LogicalNode::Project { input, .. } = &plan.node {
        assert!(matches!(input.node, LogicalNode::Filter { .. }));
        if let LogicalNode::Filter { input, .. } = &input.node {
            assert!(matches!(input.node, LogicalNode::Scan { .. }));
        }
    }
    assert_eq!(plan.schema.num_columns(), 2);
}

/// Tableless query: SELECT 1
#[test]
fn test_tableless_select() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: simple_constant(crate::types::Value::Int32(1), DataType::Int32),
                output_name: "?column?".to_string(),
            }],
            from: vec![],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32)],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Project → Empty
    assert!(matches!(plan.node, LogicalNode::Project { .. }));
    if let LogicalNode::Project { input, .. } = &plan.node {
        assert!(matches!(input.node, LogicalNode::Empty));
    }
}

/// Query with ORDER BY and LIMIT: SELECT * FROM t ORDER BY id LIMIT 10
#[test]
fn test_order_by_limit() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![simple_projection("id", DataType::Int64)],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "t".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![("id".to_string(), DataType::Int64, false)],
                    },
                },
                alias: None,
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![TypedOrderByExpr {
            expr: simple_column("id", DataType::Int64),
            asc: true,
            nulls_first: false,
        }],
        limit: Some(simple_constant(
            crate::types::Value::Int64(10),
            DataType::Int64,
        )),
        offset: None,
        output_schema: vec![("id".to_string(), DataType::Int64)],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Limit → Project → Sort → Scan
    assert!(matches!(plan.node, LogicalNode::Limit { .. }));
    if let LogicalNode::Limit { input, .. } = &plan.node {
        assert!(
            matches!(input.node, LogicalNode::Project { .. }),
            "expected Project, got {:?}",
            std::mem::discriminant(&input.node)
        );
        if let LogicalNode::Project { input, .. } = &input.node {
            assert!(
                matches!(input.node, LogicalNode::Sort { .. }),
                "expected Sort, got {:?}",
                std::mem::discriminant(&input.node)
            );
        }
    }
}

/// Set operation: SELECT id FROM a UNION SELECT id FROM b
#[test]
fn test_set_operation() {
    let left = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![simple_projection("id", DataType::Int64)],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "a".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![("id".to_string(), DataType::Int64, false)],
                    },
                },
                alias: None,
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("id".to_string(), DataType::Int64)],
    };
    let right = left.clone();

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::SetOperation {
            op: SetOpKind::Union,
            all: false,
            left: Box::new(left),
            right: Box::new(right),
        },
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("id".to_string(), DataType::Int64)],
    };

    let plan = LogicalPlanner::build(&query).unwrap();
    assert!(matches!(plan.node, LogicalNode::SetOperation { .. }));
}

/// Query with GROUP BY: SELECT status, count(*) FROM orders GROUP BY status
#[test]
fn test_group_by() {
    let count_agg = TypedExpr {
        kind: TypedExprKind::AggregateCall {
            func: ResolvedFunction {
                name: "count".to_string(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int64,
            },
            args: vec![],
            distinct: false,
            filter: None,
            order_by: vec![],
        },
        data_type: DataType::Int64,
    };

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![
                simple_projection("status", DataType::Text),
                AnalyzedProjection {
                    expr: count_agg,
                    output_name: "count".to_string(),
                },
            ],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "orders".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![
                            ("id".to_string(), DataType::Int64, false),
                            ("status".to_string(), DataType::Text, false),
                        ],
                    },
                },
                alias: None,
            }],
            where_clause: None,
            group_by: vec![simple_column("status", DataType::Text)],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![
            ("status".to_string(), DataType::Text),
            ("count".to_string(), DataType::Int64),
        ],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Aggregate → Scan (no separate Project when GROUP BY is present)
    assert!(matches!(plan.node, LogicalNode::Aggregate { .. }));
    if let LogicalNode::Aggregate { input, .. } = &plan.node {
        assert!(matches!(input.node, LogicalNode::Scan { .. }));
    }
}

/// DISTINCT query
#[test]
fn test_distinct() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![simple_projection("name", DataType::Text)],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "t".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![("name".to_string(), DataType::Text, true)],
                    },
                },
                alias: None,
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::Distinct,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("name".to_string(), DataType::Text)],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Distinct → Project → Scan
    assert!(matches!(plan.node, LogicalNode::Distinct { .. }));
}

/// Aggregate + ORDER BY: rewrite ORDER BY to post-aggregate indices
#[test]
fn test_aggregate_order_by_rewrite() {
    let count_agg = TypedExpr {
        kind: TypedExprKind::AggregateCall {
            func: ResolvedFunction {
                name: "count".to_string(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int64,
            },
            args: vec![],
            distinct: false,
            filter: None,
            order_by: vec![],
        },
        data_type: DataType::Int64,
    };

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![
                simple_projection("status", DataType::Text),
                AnalyzedProjection {
                    expr: count_agg.clone(),
                    output_name: "count".to_string(),
                },
            ],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "orders".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![
                            ("id".to_string(), DataType::Int64, false),
                            ("status".to_string(), DataType::Text, false),
                        ],
                    },
                },
                alias: None,
            }],
            where_clause: None,
            group_by: vec![simple_column("status", DataType::Text)],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        // ORDER BY count(*) DESC — uses scan-scope aggregate expr
        order_by: vec![TypedOrderByExpr {
            expr: count_agg,
            asc: false,
            nulls_first: false,
        }],
        limit: None,
        offset: None,
        output_schema: vec![
            ("status".to_string(), DataType::Text),
            ("count".to_string(), DataType::Int64),
        ],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Sort(rewritten) → Aggregate → Scan
    assert!(matches!(plan.node, LogicalNode::Sort { .. }));
    if let LogicalNode::Sort {
        order_by, input, ..
    } = &plan.node
    {
        assert_eq!(order_by.len(), 1);
        // The rewritten ORDER BY should be a ColumnRef to post-aggregate index 1
        // (group_by_count=1, agg_index=0 → 1+0 = 1)
        if let TypedExprKind::ColumnRef { column_index, .. } = &order_by[0].expr.kind {
            assert_eq!(*column_index, 1);
        } else {
            panic!("expected ColumnRef in rewritten ORDER BY");
        }
        assert!(matches!(input.node, LogicalNode::Aggregate { .. }));
    }
}

/// Aggregate + HAVING: rewrite HAVING to post-aggregate indices
#[test]
fn test_aggregate_having_rewrite() {
    let count_agg = TypedExpr {
        kind: TypedExprKind::AggregateCall {
            func: ResolvedFunction {
                name: "count".to_string(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int64,
            },
            args: vec![],
            distinct: false,
            filter: None,
            order_by: vec![],
        },
        data_type: DataType::Int64,
    };

    let having_expr = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(count_agg.clone()),
            op: BinaryOp::Gt,
            right: Box::new(simple_constant(
                crate::types::Value::Int64(5),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![
                simple_projection("status", DataType::Text),
                AnalyzedProjection {
                    expr: count_agg,
                    output_name: "count".to_string(),
                },
            ],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "orders".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![
                            ("id".to_string(), DataType::Int64, false),
                            ("status".to_string(), DataType::Text, false),
                        ],
                    },
                },
                alias: None,
            }],
            where_clause: None,
            group_by: vec![simple_column("status", DataType::Text)],
            having: Some(having_expr),
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![
            ("status".to_string(), DataType::Text),
            ("count".to_string(), DataType::Int64),
        ],
    };

    let plan = LogicalPlanner::build(&query).unwrap();

    // Should be: Filter(rewritten HAVING) → Aggregate → Scan
    assert!(matches!(plan.node, LogicalNode::Filter { .. }));
    if let LogicalNode::Filter {
        predicate, input, ..
    } = &plan.node
    {
        // HAVING predicate should be rewritten: COUNT(*) → ColumnRef(1)
        if let TypedExprKind::BinaryOp { left, .. } = &predicate.kind {
            if let TypedExprKind::ColumnRef { column_index, .. } = &left.kind {
                assert_eq!(*column_index, 1);
            } else {
                panic!("expected ColumnRef in rewritten HAVING left side");
            }
        } else {
            panic!("expected BinaryOp in rewritten HAVING");
        }
        assert!(matches!(input.node, LogicalNode::Aggregate { .. }));
    }
}
