//! Unit tests for the build module.

use super::*;
use crate::model::{ColumnDef, DataType, Value};
use crate::sql::analyzer::types::{
    AnalyzedProjection, AnalyzedTableRef, AnalyzedTableRefKind, BinaryOp as TypedBinaryOp,
    JoinCondition, SetOpKind, TableRefSchema, TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::optimizer::logical_plan::PlanSchema;
use crate::sql::optimizer::physical_plan::{PhysicalCost, PhysicalNode, PhysicalPlan};
use crate::sql::types::CastContext;

fn test_table_schema() -> TableSchema {
    TableSchema::new(
        "test_table".to_string(),
        1,
        vec![
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
            },
            ColumnDef {
                name: "name".to_string(),
                data_type: DataType::Text,
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
        vec![0],
    )
}

fn test_ctx() -> BuildContext {
    BuildContext::new().with_schema("test_table".to_string(), test_table_schema())
}

fn make_schema(cols: &[(&str, DataType)]) -> PlanSchema {
    PlanSchema::from_columns(
        cols.iter()
            .map(|(n, t)| (n.to_string(), t.clone()))
            .collect(),
    )
}

#[test]
fn test_seq_scan() {
    let plan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.schema().columns.len(), 2);
    assert_eq!(op.name(), "TableScan");
}

#[test]
fn test_seq_scan_with_alias() {
    let plan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: Some("t".to_string()),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    // Key by composite "table_name\0alias" for scope-safe lookup.
    let key = crate::sql::optimizer::schema_map_key("test_table", Some("t"));
    let ctx = BuildContext::new().with_schema(key, test_table_schema());
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.schema().from_alias, Some("t".to_string()));
}

#[test]
fn test_filter() {
    let scan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::Filter {
            predicate: TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int32,
                    }),
                    op: TypedBinaryOp::Gt,
                    right: Box::new(TypedExpr {
                        kind: TypedExprKind::Constant(Value::Int32(5)),
                        data_type: DataType::Int32,
                    }),
                },
                data_type: DataType::Boolean,
            },
            input: Box::new(scan),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "Filter");
}

#[test]
fn test_project() {
    let scan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::Project {
            projections: vec![AnalyzedProjection {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                },
                output_name: "id".to_string(),
            }],
            input: Box::new(scan),
        },
        schema: make_schema(&[("id", DataType::Int32)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "Project");
    assert_eq!(op.schema().columns.len(), 1);
}

#[test]
fn test_sort_limit() {
    let scan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let sorted = PhysicalPlan {
        node: PhysicalNode::Sort {
            order_by: vec![TypedOrderByExpr {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                },
                asc: true,
                nulls_first: false,
            }],
            input: Box::new(scan),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::Limit {
            limit: Some(TypedExpr {
                kind: TypedExprKind::Constant(Value::Int64(10)),
                data_type: DataType::Int64,
            }),
            offset: Some(TypedExpr {
                kind: TypedExprKind::Constant(Value::Int64(5)),
                data_type: DataType::Int64,
            }),
            input: Box::new(sorted),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "Limit");
}

#[test]
fn test_parameterized_limit_builds_without_constant_folding() {
    let scan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::Limit {
            limit: Some(TypedExpr {
                kind: TypedExprKind::Parameter { index: 0 },
                data_type: DataType::Int64,
            }),
            offset: Some(TypedExpr {
                kind: TypedExprKind::Parameter { index: 1 },
                data_type: DataType::Int64,
            }),
            input: Box::new(scan),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "Limit");
}

#[test]
fn test_topn_sort() {
    let scan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::TopNSort {
            order_by: vec![TypedOrderByExpr {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "id".to_string(),
                    },
                    data_type: DataType::Int32,
                },
                asc: true,
                nulls_first: false,
            }],
            limit: 10,
            input: Box::new(scan),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    // TopN becomes Sort + Limit, so outermost is Limit.
    assert_eq!(op.name(), "Limit");
}

#[test]
fn test_distinct() {
    let scan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::Distinct {
            input: Box::new(scan),
        },
        schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "Distinct");
}

#[test]
fn test_set_operation() {
    let left = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32)]),
        cost: PhysicalCost::default(),
    };
    let right = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "test_table".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32)]),
        cost: PhysicalCost::default(),
    };
    let plan = PhysicalPlan {
        node: PhysicalNode::SetOperation {
            op: SetOpKind::Union,
            all: false,
            left: Box::new(left),
            right: Box::new(right),
        },
        schema: make_schema(&[("id", DataType::Int32)]),
        cost: PhysicalCost::default(),
    };
    let ctx = test_ctx();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "Union");
}

#[test]
fn test_nlj() {
    let left_schema = test_table_schema();
    let right_schema = TableSchema::new(
        "other_table".to_string(),
        2,
        vec![
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
            },
            ColumnDef {
                name: "val".to_string(),
                data_type: DataType::Text,
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
        vec![0],
    );
    let ctx = BuildContext::new()
        .with_schema("test_table".to_string(), left_schema)
        .with_schema("other_table".to_string(), right_schema);

    let plan = PhysicalPlan {
        node: PhysicalNode::NestedLoopJoin {
            left: Box::new(PhysicalPlan {
                node: PhysicalNode::SeqScan {
                    table_name: "test_table".to_string(),
                    alias: None,
                },
                schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
                cost: PhysicalCost::default(),
            }),
            right: Box::new(PhysicalPlan {
                node: PhysicalNode::SeqScan {
                    table_name: "other_table".to_string(),
                    alias: None,
                },
                schema: make_schema(&[("id", DataType::Int32), ("val", DataType::Text)]),
                cost: PhysicalCost::default(),
            }),
            join_type: JoinType::Inner,
            condition: JoinCondition::On(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int32,
                    }),
                    op: TypedBinaryOp::Eq,
                    right: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 2,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int32,
                    }),
                },
                data_type: DataType::Boolean,
            }),
        },
        schema: make_schema(&[
            ("id", DataType::Int32),
            ("name", DataType::Text),
            ("id", DataType::Int32),
            ("val", DataType::Text),
        ]),
        cost: PhysicalCost::default(),
    };

    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "NestedLoopJoin");
}

#[test]
fn test_empty_select() {
    let plan = PhysicalPlan {
        node: PhysicalNode::Empty,
        schema: PlanSchema::empty(),
        cost: PhysicalCost::default(),
    };
    let ctx = BuildContext::new();
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "TableScan"); // Empty uses single-row TableScan
}

#[test]
fn test_missing_schema_error() {
    let plan = PhysicalPlan {
        node: PhysicalNode::SeqScan {
            table_name: "nonexistent".to_string(),
            alias: None,
        },
        schema: make_schema(&[("id", DataType::Int32)]),
        cost: PhysicalCost::default(),
    };
    let ctx = BuildContext::new();
    assert!(plan.build_operators(&ctx).is_err());
}

// ── Aggregate identity matching tests ──────────────────────

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, FunctionKind,
    ResolvedFunction,
};
use crate::sql::operators::AggregateExpr;

fn make_resolved_func(name: &str) -> ResolvedFunction {
    ResolvedFunction {
        name: name.to_string(),
        kind: FunctionKind::Builtin,
        return_type: DataType::Int64,
    }
}

fn make_col_ref(idx: usize, name: &str, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: idx,
            column_name: name.to_string(),
        },
        data_type: dt,
    }
}

#[test]
fn test_aggregate_filter_gets_distinct_slots() {
    // COUNT(*) FILTER (WHERE x > 0) vs COUNT(*) should get distinct slots.
    let filter_expr = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(make_col_ref(0, "x", DataType::Int32)),
            op: TypedBinaryOp::Gt,
            right: Box::new(TypedExpr {
                kind: TypedExprKind::Constant(Value::Int32(0)),
                data_type: DataType::Int32,
            }),
        },
        data_type: DataType::Boolean,
    };

    let func = make_resolved_func("count");

    // Build two AggregateCall projections:
    // 1. COUNT(*) FILTER (WHERE x > 0)
    // 2. COUNT(*)
    let proj1 = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![],
                distinct: false,
                filter: Some(Box::new(filter_expr)),
                order_by: vec![],
            },
            data_type: DataType::Int64,
        },
        output_name: "count_filtered".to_string(),
    };
    let proj2 = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        },
        output_name: "count_all".to_string(),
    };

    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();

    collect_agg_exprs_from(
        &proj1.expr,
        &proj1.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );
    collect_agg_exprs_from(
        &proj2.expr,
        &proj2.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );

    // Should have 2 distinct aggregate slots.
    assert_eq!(
        agg_exprs.len(),
        2,
        "FILTER difference must produce distinct slots"
    );
    assert!(agg_exprs[0].filter.is_some());
    assert!(agg_exprs[1].filter.is_none());
}

#[test]
fn test_aggregate_delimiter_gets_distinct_slots() {
    // string_agg(col, ',') vs string_agg(col, ';') should get distinct slots.
    let func = make_resolved_func("string_agg");
    let col = make_col_ref(0, "col", DataType::Text);

    let proj1 = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![
                    col.clone(),
                    TypedExpr {
                        kind: TypedExprKind::Constant(Value::Text(",".to_string())),
                        data_type: DataType::Text,
                    },
                ],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Text,
        },
        output_name: "agg_comma".to_string(),
    };
    let proj2 = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![
                    col.clone(),
                    TypedExpr {
                        kind: TypedExprKind::Constant(Value::Text(";".to_string())),
                        data_type: DataType::Text,
                    },
                ],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Text,
        },
        output_name: "agg_semi".to_string(),
    };

    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();

    collect_agg_exprs_from(
        &proj1.expr,
        &proj1.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );
    collect_agg_exprs_from(
        &proj2.expr,
        &proj2.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );

    assert_eq!(
        agg_exprs.len(),
        2,
        "different delimiters must produce distinct slots"
    );
    assert_eq!(agg_exprs[0].delimiter, Some(",".to_string()));
    assert_eq!(agg_exprs[1].delimiter, Some(";".to_string()));
}

#[test]
fn test_aggregate_cast_text_delimiter_is_unwrapped() {
    let func = make_resolved_func("string_agg");
    let col = make_col_ref(0, "col", DataType::Text);

    let proj = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![
                    col.clone(),
                    TypedExpr {
                        kind: TypedExprKind::Cast {
                            expr: Box::new(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Text(";".to_string())),
                                data_type: DataType::Text,
                            }),
                            target_type: DataType::Text,
                            cast_context: CastContext::Explicit,
                        },
                        data_type: DataType::Text,
                    },
                ],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Text,
        },
        output_name: "agg_cast_delim".to_string(),
    };

    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();
    collect_agg_exprs_from(
        &proj.expr,
        &proj.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );

    assert_eq!(agg_exprs.len(), 1);
    assert_eq!(agg_exprs[0].delimiter, Some(";".to_string()));

    if let TypedExprKind::AggregateCall {
        func,
        args,
        distinct,
        filter,
        order_by,
    } = &proj.expr.kind
    {
        assert!(aggregate_identity_matches(
            &agg_exprs[0],
            func,
            args,
            *distinct,
            filter,
            order_by
        ));
    } else {
        panic!("expected AggregateCall");
    }
}

#[test]
fn test_aggregate_cast_null_delimiter_keeps_no_separator_sentinel() {
    let func = make_resolved_func("string_agg");
    let col = make_col_ref(0, "col", DataType::Text);

    let proj = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![
                    col.clone(),
                    TypedExpr {
                        kind: TypedExprKind::Cast {
                            expr: Box::new(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Null),
                                data_type: DataType::Text,
                            }),
                            target_type: DataType::Text,
                            cast_context: CastContext::Explicit,
                        },
                        data_type: DataType::Text,
                    },
                ],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Text,
        },
        output_name: "agg_cast_null_delim".to_string(),
    };

    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();
    collect_agg_exprs_from(
        &proj.expr,
        &proj.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );

    assert_eq!(agg_exprs.len(), 1);
    assert_eq!(agg_exprs[0].delimiter, Some(String::new()));

    if let TypedExprKind::AggregateCall {
        func,
        args,
        distinct,
        filter,
        order_by,
    } = &proj.expr.kind
    {
        assert!(aggregate_identity_matches(
            &agg_exprs[0],
            func,
            args,
            *distinct,
            filter,
            order_by
        ));
    } else {
        panic!("expected AggregateCall");
    }
}

#[test]
fn test_aggregate_order_by_gets_distinct_slots() {
    // SUM(x ORDER BY y ASC) vs SUM(x ORDER BY y DESC) should get distinct slots.
    let func = make_resolved_func("sum");
    let x = make_col_ref(0, "x", DataType::Int32);
    let y = make_col_ref(1, "y", DataType::Int32);

    let proj1 = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![x.clone()],
                distinct: false,
                filter: None,
                order_by: vec![TypedOrderByExpr {
                    expr: y.clone(),
                    asc: true,
                    nulls_first: false,
                }],
            },
            data_type: DataType::Int64,
        },
        output_name: "sum_asc".to_string(),
    };
    let proj2 = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![x.clone()],
                distinct: false,
                filter: None,
                order_by: vec![TypedOrderByExpr {
                    expr: y.clone(),
                    asc: false,
                    nulls_first: false,
                }],
            },
            data_type: DataType::Int64,
        },
        output_name: "sum_desc".to_string(),
    };

    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();

    collect_agg_exprs_from(
        &proj1.expr,
        &proj1.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );
    collect_agg_exprs_from(
        &proj2.expr,
        &proj2.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );

    assert_eq!(
        agg_exprs.len(),
        2,
        "different ORDER BY must produce distinct slots"
    );
    assert!(agg_exprs[0].order_by[0].asc);
    assert!(!agg_exprs[1].order_by[0].asc);
}

/// Regression test (B1): HashJoin build path must receive right key indices
/// that are local to the right child (0-based), not combined-schema indices.
/// Prior to the fix, col[0]=col[2] with left_width=2 would pass right_index=2
/// instead of 0, causing out-of-bounds hash lookups.
#[test]
fn test_hash_join_right_keys_are_local() {
    let left_schema = test_table_schema();
    let right_schema = TableSchema::new(
        "other_table".to_string(),
        2,
        vec![
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
            },
            ColumnDef {
                name: "val".to_string(),
                data_type: DataType::Text,
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
        vec![0],
    );
    let ctx = BuildContext::new()
        .with_schema("test_table".to_string(), left_schema)
        .with_schema("other_table".to_string(), right_schema);

    // ON test_table.id (col[0]) = other_table.id (col[2] in combined schema)
    // left_width = 2 (test_table has 2 columns: id, name)
    let plan = PhysicalPlan {
        node: PhysicalNode::HashJoin {
            left: Box::new(PhysicalPlan {
                node: PhysicalNode::SeqScan {
                    table_name: "test_table".to_string(),
                    alias: None,
                },
                schema: make_schema(&[("id", DataType::Int32), ("name", DataType::Text)]),
                cost: PhysicalCost::default(),
            }),
            right: Box::new(PhysicalPlan {
                node: PhysicalNode::SeqScan {
                    table_name: "other_table".to_string(),
                    alias: None,
                },
                schema: make_schema(&[("id", DataType::Int32), ("val", DataType::Text)]),
                cost: PhysicalCost::default(),
            }),
            join_type: JoinType::Inner,
            condition: JoinCondition::On(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int32,
                    }),
                    op: TypedBinaryOp::Eq,
                    right: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 2,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int32,
                    }),
                },
                data_type: DataType::Boolean,
            }),
            left_is_build: true,
        },
        schema: make_schema(&[
            ("id", DataType::Int32),
            ("name", DataType::Text),
            ("id", DataType::Int32),
            ("val", DataType::Text),
        ]),
        cost: PhysicalCost::default(),
    };

    // Build must succeed (not panic from out-of-bounds index)
    let op = plan.build_operators(&ctx).unwrap();
    assert_eq!(op.name(), "HashJoin");
}

#[test]
fn test_duplicate_aggregate_produces_correct_output_width() {
    // SELECT SUM(a), SUM(a) FROM t — dedup yields 1 slot, but output must have 2 columns.
    let func = make_resolved_func("sum");
    let col_a = make_col_ref(0, "a", DataType::Int32);

    let proj = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![col_a.clone()],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        },
        output_name: "sum".to_string(),
    };
    let projections = vec![proj.clone(), proj];
    let group_by: Vec<TypedExpr> = vec![];

    // Build a dummy child operator (single-row empty scan).
    let schema = TableSchema::new(
        "t".to_string(),
        1,
        vec![ColumnDef {
            name: "a".to_string(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        }],
        vec![0],
    );
    let child: BoxedOperator = Box::new(TableScanOperator::new(schema));

    let op = aggregate::build_hash_aggregate(child, &group_by, &projections).unwrap();
    // Must be wrapped in a Project to duplicate the single aggregate slot into 2 columns.
    assert_eq!(op.name(), "Project");
    assert_eq!(op.schema().columns.len(), 2);
}

// ── Subquery-bearing expression regression tests ───────

/// Helper: build a minimal AnalyzedQuery representing `(SELECT 1)`.
fn make_scalar_subquery_1() -> AnalyzedQuery {
    AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
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
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    }
}

/// Regression: SUM((SELECT 1)) must dedup to 1 slot when referenced twice.
/// Before the fix, AnalyzedQuery::eq always returned false, so each occurrence
/// created a separate aggregate slot, and post-aggregate rewrite would fail to
/// find the matching slot for the second occurrence.
#[test]
fn test_subquery_bearing_aggregate_arg_deduplicates() {
    let func = make_resolved_func("sum");
    let subquery_expr = TypedExpr {
        kind: TypedExprKind::ScalarSubquery(Box::new(make_scalar_subquery_1())),
        data_type: DataType::Int32,
    };

    let proj = AnalyzedProjection {
        expr: TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: func.clone(),
                args: vec![subquery_expr],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        },
        output_name: "sum_subq".to_string(),
    };

    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();

    // Collect twice (simulates SELECT SUM((SELECT 1)), SUM((SELECT 1)))
    collect_agg_exprs_from(
        &proj.expr,
        &proj.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );
    collect_agg_exprs_from(
        &proj.expr,
        &proj.output_name,
        &mut agg_exprs,
        &mut agg_names,
        &mut agg_types,
    );

    assert_eq!(
        agg_exprs.len(),
        1,
        "identical subquery-bearing aggregate args must dedup to 1 slot"
    );
}

/// Regression: aggregate_identity_matches must return true for identical
/// subquery-bearing arguments (not always false).
#[test]
fn test_subquery_bearing_aggregate_identity_matches() {
    let func = make_resolved_func("sum");
    let subquery_expr = TypedExpr {
        kind: TypedExprKind::ScalarSubquery(Box::new(make_scalar_subquery_1())),
        data_type: DataType::Int32,
    };

    let ae = AggregateExpr {
        func_name: "sum".to_string(),
        arg: Some(subquery_expr.clone()),
        distinct: false,
        delimiter: None,
        filter: None,
        order_by: vec![],
    };

    assert!(
        aggregate_identity_matches(&ae, &func, &[subquery_expr], false, &None, &[]),
        "aggregate_identity_matches must return true for identical subquery args"
    );
}

/// Regression: find_matching_group_by must match a GROUP BY expression
/// containing a scalar subquery against the same expression in the projection.
#[test]
fn test_subquery_bearing_group_by_matches() {
    let subquery_expr = TypedExpr {
        kind: TypedExprKind::ScalarSubquery(Box::new(make_scalar_subquery_1())),
        data_type: DataType::Int32,
    };

    let group_by = vec![subquery_expr.clone()];
    let result = find_matching_group_by(&subquery_expr, &group_by);
    assert_eq!(
        result,
        Some(0),
        "find_matching_group_by must match identical subquery expressions"
    );
}

/// Regression: COALESCE(SUM((SELECT 1)), 0) must successfully rewrite the
/// post-aggregate projection, finding the aggregate slot for SUM((SELECT 1)).
#[test]
fn test_coalesce_wrapping_subquery_aggregate_rewrites_correctly() {
    let func = make_resolved_func("sum");
    let subquery_expr = TypedExpr {
        kind: TypedExprKind::ScalarSubquery(Box::new(make_scalar_subquery_1())),
        data_type: DataType::Int32,
    };

    // Build the aggregate expression (slot 0)
    let ae = AggregateExpr {
        func_name: "sum".to_string(),
        arg: Some(subquery_expr.clone()),
        distinct: false,
        delimiter: None,
        filter: None,
        order_by: vec![],
    };

    // Build COALESCE(SUM((SELECT 1)), 0)
    let coalesce_expr = TypedExpr {
        kind: TypedExprKind::Coalesce(vec![
            TypedExpr {
                kind: TypedExprKind::AggregateCall {
                    func: func.clone(),
                    args: vec![subquery_expr],
                    distinct: false,
                    filter: None,
                    order_by: vec![],
                },
                data_type: DataType::Int64,
            },
            TypedExpr {
                kind: TypedExprKind::Constant(Value::Int64(0)),
                data_type: DataType::Int64,
            },
        ]),
        data_type: DataType::Int64,
    };

    // Rewrite must succeed (not error with "no matching slot")
    let result = aggregate::rewrite_post_aggregate_expr(&coalesce_expr, &[], 0, &[ae]);
    assert!(
        result.is_ok(),
        "rewrite_post_aggregate_expr must find the slot for subquery-bearing aggregate: {:?}",
        result.err()
    );
}

// ── CTE reference PartialEq regression tests ──────────────

/// Helper: build a CTE table ref (table_id = 0) with the given CTE name.
fn make_cte_table_ref(cte_name: &str) -> AnalyzedTableRef {
    AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Table {
            name: cte_name.to_string(),
            schema: TableRefSchema {
                table_id: 0, // CTE sentinel
                columns: vec![("x".to_string(), DataType::Int32, true)],
            },
        },
        alias: Some(cte_name.to_string()),
    }
}

/// Negative: FROM c1 and FROM c2 must NOT compare equal even if their column
/// schemas match, because CTE names are semantic identity (table_id == 0).
///
/// This is the regression reported in QG review: without this fix,
/// `SUM((SELECT x FROM c1))` and `SUM((SELECT x FROM c2))` would collapse
/// to one aggregate slot.
#[test]
fn test_different_cte_refs_are_not_equal() {
    let ref_c1 = make_cte_table_ref("c1");
    let ref_c2 = make_cte_table_ref("c2");
    assert_ne!(
        ref_c1, ref_c2,
        "CTE refs with different names must NOT compare equal"
    );
}

/// Positive: FROM c1 and FROM c1 (same CTE name, same schema) must compare equal.
#[test]
fn test_same_cte_refs_are_equal() {
    let ref1 = make_cte_table_ref("c1");
    let ref2 = make_cte_table_ref("c1");
    assert_eq!(
        ref1, ref2,
        "CTE refs with the same name and schema must compare equal"
    );
}

/// Positive: same CTE with different inner column names should compare equal,
/// because column names are binder-local (only types + nullability matter).
#[test]
fn test_cte_alpha_equiv() {
    let ref1 = AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Table {
            name: "c1".to_string(),
            schema: TableRefSchema {
                table_id: 0,
                columns: vec![("x".to_string(), DataType::Int32, true)],
            },
        },
        alias: Some("c1".to_string()),
    };
    let ref2 = AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Table {
            name: "c1".to_string(),
            schema: TableRefSchema {
                table_id: 0,
                columns: vec![("renamed_x".to_string(), DataType::Int32, true)],
            },
        },
        alias: Some("c1".to_string()),
    };
    assert_eq!(
        ref1, ref2,
        "same CTE with renamed inner columns must compare equal (alpha equivalence)"
    );
}

/// Positive: base table refs (table_id != 0) should still ignore the name field,
/// since table_id is the authoritative identifier for real tables.
#[test]
fn test_base_table_refs_ignore_name() {
    let ref1 = AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Table {
            name: "public.users".to_string(),
            schema: TableRefSchema {
                table_id: 42,
                columns: vec![("id".to_string(), DataType::Int32, false)],
            },
        },
        alias: Some("u".to_string()),
    };
    let ref2 = AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Table {
            name: "users".to_string(), // different name, same table_id
            schema: TableRefSchema {
                table_id: 42,
                columns: vec![("id".to_string(), DataType::Int32, false)],
            },
        },
        alias: Some("v".to_string()), // different alias (ignored)
    };
    assert_eq!(
        ref1, ref2,
        "base table refs with same table_id must compare equal regardless of name/alias"
    );
}
