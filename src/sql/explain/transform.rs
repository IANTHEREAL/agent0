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
        | ScanType::GinIndexScan { index_name, .. } => index_name.clone(),
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
