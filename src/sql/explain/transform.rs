//! Physical plan to EXPLAIN plan node transformation.
//!
//! Converts the optimizer's `PhysicalPlan` tree into the display-oriented `PlanNode`
//! tree so that `EXPLAIN` shows the same plan that execution actually uses.

use super::{format_typed_expr, PlanCost, PlanNode, DEFAULT_ROW_WIDTH};
use crate::sql::analyzer::types::JoinCondition;
use crate::sql::optimizer::physical_plan::PhysicalNode;
use crate::sql::planner::ScanType;

/// Convert a PhysicalPlan tree into a PlanNode tree for EXPLAIN display.
pub fn physical_plan_to_plan_node(
    phys: &crate::sql::optimizer::physical_plan::PhysicalPlan,
) -> PlanNode {
    let cost = PlanCost {
        startup: phys.cost.startup,
        total: phys.cost.total,
        rows: phys.cost.rows,
        width: phys.schema.columns.len().saturating_mul(DEFAULT_ROW_WIDTH),
    };

    match &phys.node {
        PhysicalNode::SeqScan { table_name, alias } => PlanNode::SeqScan {
            table_name: table_name.clone(),
            alias: alias.clone(),
            filter: None,
            cost,
        },
        PhysicalNode::IndexScan {
            table_name,
            alias,
            scan_type,
        } => {
            let index_name = extract_index_name(scan_type);
            PlanNode::IndexScan {
                table_name: table_name.clone(),
                alias: alias.clone(),
                index_name,
                index_cond: None,
                filter: None,
                cost,
            }
        }
        PhysicalNode::HnswScan {
            table_name,
            alias,
            scan_type,
        } => {
            if let ScanType::HnswIndexScan {
                index_name,
                distance_metric,
                k,
                ..
            } = scan_type
            {
                PlanNode::HnswScan {
                    table_name: table_name.clone(),
                    alias: alias.clone(),
                    index_name: index_name.clone(),
                    distance_metric: distance_metric.as_str().to_string(),
                    k: *k,
                    cost,
                }
            } else {
                PlanNode::SeqScan {
                    table_name: table_name.clone(),
                    alias: alias.clone(),
                    filter: None,
                    cost,
                }
            }
        }
        PhysicalNode::Filter { predicate, input } => {
            if let PhysicalNode::IndexScan {
                table_name,
                alias,
                scan_type,
            } = &input.node
            {
                let index_name = extract_index_name(scan_type);
                return PlanNode::IndexScan {
                    table_name: table_name.clone(),
                    alias: alias.clone(),
                    index_name,
                    index_cond: Some(format_typed_expr(predicate)),
                    filter: None,
                    cost,
                };
            }

            let child = physical_plan_to_plan_node(input);
            PlanNode::Filter {
                condition: format_typed_expr(predicate),
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::Project { input, .. } => {
            // Project is implicit in EXPLAIN — show the child directly.
            physical_plan_to_plan_node(input)
        }
        PhysicalNode::NestedLoopJoin {
            left,
            right,
            join_type,
            ..
        } => {
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            PlanNode::NestedLoop {
                join_type: format!("{:?}", join_type),
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::HashJoin {
            left,
            right,
            join_type,
            condition,
            ..
        } => {
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            let cond_str = format_join_condition(condition);
            PlanNode::HashJoin {
                join_type: format!("{:?}", join_type),
                hash_cond: cond_str,
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::Sort { order_by, input } => {
            let child = physical_plan_to_plan_node(input);
            let sort_keys = format_order_by_keys(order_by);
            PlanNode::Sort {
                sort_key: sort_keys,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::TopNSort {
            order_by,
            limit,
            input,
        } => {
            let child = physical_plan_to_plan_node(input);
            let sort_keys = format_order_by_keys(order_by);
            let sorted = PlanNode::Sort {
                sort_key: sort_keys,
                cost: cost.clone(),
                child: Box::new(child),
            };
            PlanNode::Limit {
                count: *limit,
                cost,
                child: Box::new(sorted),
            }
        }
        PhysicalNode::Limit { input, .. } => {
            let child = physical_plan_to_plan_node(input);
            PlanNode::Limit {
                count: phys.cost.rows,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::HashAggregate {
            group_by, input, ..
        } => {
            let child = physical_plan_to_plan_node(input);
            let keys: Vec<String> = group_by.iter().map(format_typed_expr).collect();
            let strategy = if keys.is_empty() {
                "Plain".to_string()
            } else {
                "HashAggregate".to_string()
            };
            PlanNode::Aggregate {
                strategy,
                keys,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::StreamAggregate {
            group_by, input, ..
        } => {
            let child = physical_plan_to_plan_node(input);
            let keys: Vec<String> = group_by.iter().map(format_typed_expr).collect();
            PlanNode::Aggregate {
                strategy: "GroupAggregate".to_string(),
                keys,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::Distinct { input } | PhysicalNode::DistinctOn { input, .. } => {
            let child = physical_plan_to_plan_node(input);
            PlanNode::Aggregate {
                strategy: "Unique".to_string(),
                keys: vec![],
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::Window { input, .. } => {
            // Window functions don't have a dedicated PlanNode — show child.
            physical_plan_to_plan_node(input)
        }
        PhysicalNode::HashSemiJoin {
            left,
            right,
            anti,
            condition,
        } => {
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            let cond_str = format_join_condition(condition);
            PlanNode::SemiJoin {
                anti: *anti,
                hash_cond: cond_str,
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::SetOperation { left, right, .. } => {
            // Approximate as nested loop for display purposes.
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            PlanNode::NestedLoop {
                join_type: "SetOperation".to_string(),
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::Empty | PhysicalNode::Values { .. } => PlanNode::Result { cost },
        PhysicalNode::TableFunction {
            function_name,
            alias,
            ..
        } => PlanNode::TableFunctionScan {
            function_name: function_name.clone(),
            alias: alias.clone(),
            cost,
        },
        PhysicalNode::Subquery { subplan, .. } => physical_plan_to_plan_node(subplan),
    }
}

/// Extract the index name from a `ScanType`.
fn extract_index_name(scan_type: &ScanType) -> String {
    match scan_type {
        ScanType::IndexScan { index_name, .. }
        | ScanType::IndexRangeScan { index_name, .. }
        | ScanType::IndexBoundedRangeScan { index_name, .. }
        | ScanType::InListScan { index_name, .. }
        | ScanType::GinIndexScan { index_name, .. }
        | ScanType::HnswIndexScan { index_name, .. } => index_name.clone(),
        _ => "unknown".to_string(),
    }
}

/// Format a `JoinCondition` as a human-readable string.
fn format_join_condition(condition: &JoinCondition) -> Option<String> {
    match condition {
        JoinCondition::On(expr) => Some(format_typed_expr(expr)),
        JoinCondition::Using(cols) => Some(
            cols.iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        JoinCondition::None => None,
    }
}

/// Format order-by keys from `TypedOrderBy` items.
fn format_order_by_keys(order_by: &[crate::sql::analyzer::types::TypedOrderByExpr]) -> Vec<String> {
    order_by
        .iter()
        .map(|ob| {
            let dir = if ob.asc { "ASC" } else { "DESC" };
            format!("{} {}", format_typed_expr(&ob.expr), dir)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{
        FunctionKind, JoinCondition, JoinType, ResolvedFunction, ResolvedUsingColumn, SetOpKind,
        TypedExpr, TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
    };
    use crate::sql::operators::WindowFunctionExpr;
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::optimizer::physical_plan::{PhysicalCost, PhysicalNode, PhysicalPlan};
    use crate::sql::planner::ScanType;

    fn bool_const(v: bool) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(v)),
            DataType::Boolean,
        )
    }

    fn int_const(v: i32) -> TypedExpr {
        TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
    }

    fn one_col_schema(name: &str, ty: DataType) -> PlanSchema {
        PlanSchema::from_columns(vec![(name.to_string(), ty)])
    }

    fn empty_plan() -> PhysicalPlan {
        PhysicalPlan {
            node: PhysicalNode::Empty,
            schema: PlanSchema::from_columns(vec![]),
            cost: PhysicalCost::default(),
        }
    }

    #[test]
    fn extract_index_name_handles_all_index_scan_variants() {
        let idx = ScanType::IndexScan {
            index_id: 1,
            index_name: "idx_a".to_string(),
            values: vec![Value::Int32(1)],
        };
        assert_eq!(extract_index_name(&idx), "idx_a");

        let idx = ScanType::IndexRangeScan {
            index_id: 2,
            index_name: "idx_b".to_string(),
            prefix_values: vec![Value::Int32(2)],
        };
        assert_eq!(extract_index_name(&idx), "idx_b");

        let idx = ScanType::IndexBoundedRangeScan {
            index_id: 3,
            index_name: "idx_c".to_string(),
            prefix_values: vec![],
            range_start: Some(Value::Int32(1)),
            start_inclusive: true,
            range_end: Some(Value::Int32(10)),
            end_inclusive: false,
        };
        assert_eq!(extract_index_name(&idx), "idx_c");

        let idx = ScanType::InListScan {
            index_id: 4,
            index_name: "idx_d".to_string(),
            column_values: vec![vec![Value::Int32(1), Value::Int32(2)]],
        };
        assert_eq!(extract_index_name(&idx), "idx_d");

        assert_eq!(extract_index_name(&ScanType::FullTableScan), "unknown");
    }

    #[test]
    fn join_and_order_format_helpers_cover_variants() {
        let on = JoinCondition::On(bool_const(true));
        assert!(format_join_condition(&on).is_some());

        let using = JoinCondition::Using(vec![
            ResolvedUsingColumn {
                name: "id".to_string(),
                left_index: 0,
                right_index: 0,
                data_type: DataType::Int32,
                left_type: DataType::Int32,
                right_type: DataType::Int32,
            },
            ResolvedUsingColumn {
                name: "tenant_id".to_string(),
                left_index: 1,
                right_index: 1,
                data_type: DataType::Int32,
                left_type: DataType::Int32,
                right_type: DataType::Int32,
            },
        ]);
        assert_eq!(
            format_join_condition(&using),
            Some("id, tenant_id".to_string())
        );

        assert!(format_join_condition(&JoinCondition::None).is_none());

        let keys = format_order_by_keys(&[
            TypedOrderByExpr {
                expr: int_const(1),
                asc: true,
                nulls_first: false,
            },
            TypedOrderByExpr {
                expr: int_const(2),
                asc: false,
                nulls_first: true,
            },
        ]);
        assert_eq!(keys.len(), 2);
        assert!(keys[0].ends_with("ASC"));
        assert!(keys[1].ends_with("DESC"));
    }

    #[test]
    fn physical_plan_seq_scan_and_filter_on_index_scan_mapping() {
        let scan = PhysicalPlan {
            node: PhysicalNode::SeqScan {
                table_name: "public.t".to_string(),
                alias: Some("t".to_string()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost {
                startup: 1.0,
                total: 2.0,
                rows: 3,
            },
        };

        match physical_plan_to_plan_node(&scan) {
            PlanNode::SeqScan {
                table_name,
                alias,
                cost,
                ..
            } => {
                assert_eq!(table_name, "public.t");
                assert_eq!(alias, Some("t".to_string()));
                assert_eq!(cost.rows, 3);
                assert_eq!(cost.width, 40);
            }
            other => panic!("unexpected node: {:?}", other),
        }

        let index_scan = PhysicalPlan {
            node: PhysicalNode::IndexScan {
                table_name: "public.t".to_string(),
                alias: None,
                scan_type: ScanType::IndexScan {
                    index_id: 1,
                    index_name: "idx_t_id".to_string(),
                    values: vec![Value::Int32(1)],
                },
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        let filter = PhysicalPlan {
            node: PhysicalNode::Filter {
                predicate: bool_const(true),
                input: Box::new(index_scan),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };

        match physical_plan_to_plan_node(&filter) {
            PlanNode::IndexScan {
                table_name,
                index_name,
                index_cond,
                ..
            } => {
                assert_eq!(table_name, "public.t");
                assert_eq!(index_name, "idx_t_id");
                assert!(index_cond.is_some());
            }
            other => panic!("unexpected node: {:?}", other),
        }
    }

    #[test]
    fn physical_plan_topn_maps_to_limit_over_sort() {
        let child = PhysicalPlan {
            node: PhysicalNode::SeqScan {
                table_name: "public.t".to_string(),
                alias: None,
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };

        let topn = PhysicalPlan {
            node: PhysicalNode::TopNSort {
                order_by: vec![TypedOrderByExpr {
                    expr: int_const(1),
                    asc: false,
                    nulls_first: false,
                }],
                limit: 5,
                input: Box::new(child),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost {
                startup: 0.0,
                total: 10.0,
                rows: 5,
            },
        };

        match physical_plan_to_plan_node(&topn) {
            PlanNode::Limit { count, child, .. } => {
                assert_eq!(count, 5);
                match *child {
                    PlanNode::Sort { sort_key, .. } => {
                        assert_eq!(sort_key.len(), 1);
                        assert!(sort_key[0].ends_with("DESC"));
                    }
                    other => panic!("expected Sort child, got {:?}", other),
                }
            }
            other => panic!("unexpected node: {:?}", other),
        }

        let join = PhysicalPlan {
            node: PhysicalNode::HashJoin {
                left: Box::new(PhysicalPlan {
                    node: PhysicalNode::Empty,
                    schema: PlanSchema::from_columns(vec![]),
                    cost: PhysicalCost::default(),
                }),
                right: Box::new(PhysicalPlan {
                    node: PhysicalNode::Empty,
                    schema: PlanSchema::from_columns(vec![]),
                    cost: PhysicalCost::default(),
                }),
                join_type: JoinType::Inner,
                condition: JoinCondition::None,
                left_is_build: true,
            },
            schema: one_col_schema("x", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        match physical_plan_to_plan_node(&join) {
            PlanNode::HashJoin { join_type, .. } => assert_eq!(join_type, "Inner"),
            other => panic!("unexpected node: {:?}", other),
        }
    }

    #[test]
    fn physical_plan_covers_remaining_node_mappings() {
        let base = PhysicalPlan {
            node: PhysicalNode::SeqScan {
                table_name: "public.t".to_string(),
                alias: None,
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };

        let filter_non_index = PhysicalPlan {
            node: PhysicalNode::Filter {
                predicate: bool_const(false),
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&filter_non_index),
            PlanNode::Filter { .. }
        ));

        let project = PhysicalPlan {
            node: PhysicalNode::Project {
                projections: vec![],
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&project),
            PlanNode::SeqScan { .. }
        ));

        let limit = PhysicalPlan {
            node: PhysicalNode::Limit {
                limit: Some(int_const(2)),
                offset: Some(int_const(1)),
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost {
                startup: 0.0,
                total: 1.0,
                rows: 2,
            },
        };
        match physical_plan_to_plan_node(&limit) {
            PlanNode::Limit { count, .. } => assert_eq!(count, 2),
            other => panic!("unexpected node: {:?}", other),
        }

        let hash_agg = PhysicalPlan {
            node: PhysicalNode::HashAggregate {
                group_by: vec![],
                projections: vec![],
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        match physical_plan_to_plan_node(&hash_agg) {
            PlanNode::Aggregate { strategy, .. } => assert_eq!(strategy, "Plain"),
            other => panic!("unexpected node: {:?}", other),
        }

        let stream_agg = PhysicalPlan {
            node: PhysicalNode::StreamAggregate {
                group_by: vec![int_const(1)],
                projections: vec![],
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        match physical_plan_to_plan_node(&stream_agg) {
            PlanNode::Aggregate { strategy, .. } => assert_eq!(strategy, "GroupAggregate"),
            other => panic!("unexpected node: {:?}", other),
        }

        let distinct = PhysicalPlan {
            node: PhysicalNode::Distinct {
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&distinct),
            PlanNode::Aggregate { strategy, .. } if strategy == "Unique"
        ));

        let distinct_on = PhysicalPlan {
            node: PhysicalNode::DistinctOn {
                on_exprs: vec![int_const(1)],
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&distinct_on),
            PlanNode::Aggregate { strategy, .. } if strategy == "Unique"
        ));

        let window = PhysicalPlan {
            node: PhysicalNode::Window {
                window_functions: vec![WindowFunctionExpr {
                    func_name: "row_number".to_string(),
                    arg_expr: None,
                    partition_by: vec![],
                    order_by: vec![],
                    offset_expr: None,
                    default_value_expr: None,
                    window_frame: None,
                    filter_expr: None,
                    output_name: "rn".to_string(),
                    output_type: DataType::Int64,
                }],
                input: Box::new(base.clone()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&window),
            PlanNode::SeqScan { .. }
        ));

        let semi = PhysicalPlan {
            node: PhysicalNode::HashSemiJoin {
                left: Box::new(empty_plan()),
                right: Box::new(empty_plan()),
                anti: true,
                condition: JoinCondition::None,
            },
            schema: one_col_schema("x", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        match physical_plan_to_plan_node(&semi) {
            PlanNode::SemiJoin {
                anti, hash_cond, ..
            } => {
                assert!(anti);
                assert!(hash_cond.is_none());
            }
            other => panic!("unexpected node: {:?}", other),
        }

        let setop = PhysicalPlan {
            node: PhysicalNode::SetOperation {
                op: SetOpKind::Union,
                all: false,
                left: Box::new(empty_plan()),
                right: Box::new(empty_plan()),
            },
            schema: one_col_schema("x", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&setop),
            PlanNode::NestedLoop { join_type, .. } if join_type == "SetOperation"
        ));

        let values = PhysicalPlan {
            node: PhysicalNode::Values { rows: vec![] },
            schema: one_col_schema("x", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&values),
            PlanNode::Result { .. }
        ));

        let tf = PhysicalPlan {
            node: PhysicalNode::TableFunction {
                function_name: "generate_series".to_string(),
                args: vec![TypedFunctionArg::Positional(int_const(1))],
                alias: Some("g".to_string()),
            },
            schema: one_col_schema("g", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        match physical_plan_to_plan_node(&tf) {
            PlanNode::TableFunctionScan {
                function_name,
                alias,
                ..
            } => {
                assert_eq!(function_name, "generate_series");
                assert_eq!(alias, Some("g".to_string()));
            }
            other => panic!("unexpected node: {:?}", other),
        }

        let subquery = PhysicalPlan {
            node: PhysicalNode::Subquery {
                subplan: Box::new(base),
                alias: Some("sq".to_string()),
            },
            schema: one_col_schema("id", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&subquery),
            PlanNode::SeqScan { .. }
        ));

        // Also exercise NLJ formatting path.
        let nlj = PhysicalPlan {
            node: PhysicalNode::NestedLoopJoin {
                left: Box::new(empty_plan()),
                right: Box::new(empty_plan()),
                join_type: JoinType::Left,
                condition: JoinCondition::On(TypedExpr::new(
                    TypedExprKind::FunctionCall {
                        func: ResolvedFunction {
                            name: "test_fn".to_string(),
                            kind: FunctionKind::Builtin,
                            return_type: DataType::Boolean,
                        },
                        args: vec![],
                        order_by: vec![],
                        filter: None,
                    },
                    DataType::Boolean,
                )),
            },
            schema: one_col_schema("x", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        assert!(matches!(
            physical_plan_to_plan_node(&nlj),
            PlanNode::NestedLoop { join_type, .. } if join_type == "Left"
        ));
    }

    #[test]
    fn hash_aggregate_with_group_keys_uses_hashaggregate_strategy() {
        let child = PhysicalPlan {
            node: PhysicalNode::Empty,
            schema: PlanSchema::from_columns(vec![]),
            cost: PhysicalCost::default(),
        };
        let agg = PhysicalPlan {
            node: PhysicalNode::HashAggregate {
                group_by: vec![int_const(1)],
                projections: vec![],
                input: Box::new(child),
            },
            schema: one_col_schema("k", DataType::Int32),
            cost: PhysicalCost::default(),
        };
        match physical_plan_to_plan_node(&agg) {
            PlanNode::Aggregate { strategy, keys, .. } => {
                assert_eq!(strategy, "HashAggregate");
                assert_eq!(keys.len(), 1);
            }
            other => panic!("unexpected node: {:?}", other),
        }
    }

    #[test]
    fn plan_cost_width_scales_with_schema_columns() {
        let phys = PhysicalPlan {
            node: PhysicalNode::SeqScan {
                table_name: "public.w".to_string(),
                alias: None,
            },
            schema: PlanSchema::from_columns(vec![
                ("a".to_string(), DataType::Int32),
                ("b".to_string(), DataType::Text),
                ("c".to_string(), DataType::Boolean),
            ]),
            cost: PhysicalCost {
                startup: 1.5,
                total: 9.5,
                rows: 12,
            },
        };

        match physical_plan_to_plan_node(&phys) {
            PlanNode::SeqScan { cost, .. } => {
                assert_eq!(cost.startup, 1.5);
                assert_eq!(cost.total, 9.5);
                assert_eq!(cost.rows, 12);
                assert_eq!(cost.width, 3 * 40);
            }
            other => panic!("unexpected node: {:?}", other),
        }
    }
}
