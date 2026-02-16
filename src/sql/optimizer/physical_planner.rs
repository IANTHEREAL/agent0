//! Physical planner: `LogicalPlan → PhysicalPlan`.
//!
//! Phase 1 uses heuristic rules (no statistics):
//!
//! | Logical       | Physical        | Rule                    |
//! |---------------|-----------------|-------------------------|
//! | Scan          | SeqScan         | Always (index in Ph. 2) |
//! | Join          | HashJoin/NLJ    | HashJoin if equi-keys   |
//! | Aggregate     | HashAggregate   | Always                  |
//! | Sort + Limit  | TopNSort        | When limit+offset < 1K  |

use super::logical_plan::{LogicalNode, LogicalPlan};
use super::physical_plan::{PhysicalCost, PhysicalNode, PhysicalPlan};
use crate::sql::analyzer::types::TypedExprKind;

const DEFAULT_ESTIMATED_ROWS: usize = 1000;
const TOPN_THRESHOLD: usize = 1000;

/// Converts a [`LogicalPlan`] into a [`PhysicalPlan`].
pub struct PhysicalPlanner;

impl PhysicalPlanner {
    /// Plan a logical plan into a physical plan.
    pub fn plan(logical: &LogicalPlan) -> PhysicalPlan {
        Self::plan_node(logical)
    }

    fn plan_node(logical: &LogicalPlan) -> PhysicalPlan {
        match &logical.node {
            // ── Leaf nodes ──────────────────────────────
            LogicalNode::Scan { table_name, alias } => PhysicalPlan {
                node: PhysicalNode::SeqScan {
                    table_name: table_name.clone(),
                    alias: alias.clone(),
                },
                schema: logical.schema.clone(),
                cost: PhysicalCost {
                    startup: 0.0,
                    total: DEFAULT_ESTIMATED_ROWS as f64 * 0.01 + 1.0,
                    rows: DEFAULT_ESTIMATED_ROWS,
                },
            },

            LogicalNode::Empty => PhysicalPlan {
                node: PhysicalNode::Empty,
                schema: logical.schema.clone(),
                cost: PhysicalCost {
                    startup: 0.0,
                    total: 0.01,
                    rows: 1,
                },
            },

            LogicalNode::Values { rows } => PhysicalPlan {
                node: PhysicalNode::Values { rows: rows.clone() },
                schema: logical.schema.clone(),
                cost: PhysicalCost {
                    startup: 0.0,
                    total: rows.len() as f64 * 0.01,
                    rows: rows.len(),
                },
            },

            LogicalNode::TableFunction {
                function_name,
                args,
                alias,
            } => PhysicalPlan {
                node: PhysicalNode::TableFunction {
                    function_name: function_name.clone(),
                    args: args.clone(),
                    alias: alias.clone(),
                },
                schema: logical.schema.clone(),
                cost: PhysicalCost {
                    startup: 0.0,
                    total: 11.0,
                    rows: DEFAULT_ESTIMATED_ROWS,
                },
            },

            // ── Unary operators ─────────────────────────
            LogicalNode::Filter { predicate, input } => {
                let child = Self::plan_node(input);
                let rows = (child.cost.rows / 3).max(1); // heuristic: 33% selectivity
                let cost = PhysicalCost {
                    startup: child.cost.startup,
                    total: child.cost.total + rows as f64 * 0.01,
                    rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::Filter {
                        predicate: predicate.clone(),
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Project { projections, input } => {
                let child = Self::plan_node(input);
                let cost = PhysicalCost {
                    startup: child.cost.startup,
                    total: child.cost.total + child.cost.rows as f64 * 0.001,
                    rows: child.cost.rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::Project {
                        projections: projections.clone(),
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Aggregate {
                group_by,
                projections,
                input,
            } => {
                let child = Self::plan_node(input);
                let agg_rows = if group_by.is_empty() {
                    1
                } else {
                    (child.cost.rows / 10).max(1)
                };
                let cost = PhysicalCost {
                    startup: child.cost.total,
                    total: child.cost.total + agg_rows as f64 * 0.1,
                    rows: agg_rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::HashAggregate {
                        group_by: group_by.clone(),
                        projections: projections.clone(),
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Sort { order_by, input } => {
                let child = Self::plan_node(input);
                let sort_cost = child.cost.total
                    + (child.cost.rows as f64 * (child.cost.rows as f64).log2().max(1.0));
                let cost = PhysicalCost {
                    startup: sort_cost,
                    total: sort_cost,
                    rows: child.cost.rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::Sort {
                        order_by: order_by.clone(),
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Limit {
                limit,
                offset,
                input,
            } => {
                let child = Self::plan_node(input);

                // Check for TopN optimization: Sort + Limit with small limit
                if let (
                    Some(limit_expr),
                    PhysicalNode::Sort {
                        order_by,
                        input: sort_input,
                    },
                ) = (limit, &child.node)
                {
                    if let Some(limit_val) = extract_constant_usize(limit_expr) {
                        let offset_val = offset
                            .as_ref()
                            .and_then(extract_constant_usize)
                            .unwrap_or(0);
                        if limit_val + offset_val < TOPN_THRESHOLD {
                            let effective_limit = limit_val + offset_val;
                            let topn_cost = PhysicalCost {
                                startup: sort_input.cost.startup,
                                total: sort_input.cost.total + effective_limit as f64 * 0.01,
                                rows: limit_val,
                            };
                            return PhysicalPlan {
                                node: PhysicalNode::Limit {
                                    limit: limit.clone(),
                                    offset: offset.clone(),
                                    input: Box::new(PhysicalPlan {
                                        node: PhysicalNode::TopNSort {
                                            order_by: order_by.clone(),
                                            limit: effective_limit,
                                            input: sort_input.clone(),
                                        },
                                        schema: child.schema.clone(),
                                        cost: topn_cost.clone(),
                                    }),
                                },
                                schema: logical.schema.clone(),
                                cost: topn_cost,
                            };
                        }
                    }
                }

                let limited_rows = if let Some(limit_expr) = limit {
                    extract_constant_usize(limit_expr)
                        .map(|l| l.min(child.cost.rows))
                        .unwrap_or(child.cost.rows)
                } else {
                    child.cost.rows
                };
                let cost = PhysicalCost {
                    startup: child.cost.startup,
                    total: child.cost.startup + limited_rows as f64 * 0.01,
                    rows: limited_rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::Limit {
                        limit: limit.clone(),
                        offset: offset.clone(),
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Distinct { input } => {
                let child = Self::plan_node(input);
                let cost = PhysicalCost {
                    startup: child.cost.total,
                    total: child.cost.total + child.cost.rows as f64 * 0.01,
                    rows: (child.cost.rows / 2).max(1),
                };
                PhysicalPlan {
                    node: PhysicalNode::Distinct {
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::DistinctOn { on_exprs, input } => {
                let child = Self::plan_node(input);
                let cost = PhysicalCost {
                    startup: child.cost.total,
                    total: child.cost.total + child.cost.rows as f64 * 0.01,
                    rows: (child.cost.rows / 2).max(1),
                };
                PhysicalPlan {
                    node: PhysicalNode::DistinctOn {
                        on_exprs: on_exprs.clone(),
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Window { input } => {
                let child = Self::plan_node(input);
                let cost = child.cost.clone();
                PhysicalPlan {
                    node: PhysicalNode::Window {
                        input: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            // ── Binary operators ────────────────────────
            LogicalNode::Join {
                left,
                right,
                join_type,
                condition,
            } => {
                let left_phys = Self::plan_node(left);
                let right_phys = Self::plan_node(right);
                let total_rows = left_phys.cost.rows.saturating_mul(right_phys.cost.rows);
                let total_cost =
                    left_phys.cost.total + right_phys.cost.total + total_rows as f64 * 0.01;
                let cost = PhysicalCost {
                    startup: 0.0,
                    total: total_cost,
                    rows: total_rows.max(1),
                };

                // Phase 1: always NLJ. Hash join selection in Phase 2+.
                PhysicalPlan {
                    node: PhysicalNode::NestedLoopJoin {
                        left: Box::new(left_phys),
                        right: Box::new(right_phys),
                        join_type: *join_type,
                        condition: condition.clone(),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::SetOperation {
                op,
                all,
                left,
                right,
            } => {
                let left_phys = Self::plan_node(left);
                let right_phys = Self::plan_node(right);
                let rows = left_phys.cost.rows + right_phys.cost.rows;
                let cost = PhysicalCost {
                    startup: 0.0,
                    total: left_phys.cost.total + right_phys.cost.total + rows as f64 * 0.01,
                    rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::SetOperation {
                        op: *op,
                        all: *all,
                        left: Box::new(left_phys),
                        right: Box::new(right_phys),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }

            LogicalNode::Subquery { subplan, alias } => {
                let child = Self::plan_node(subplan);
                let cost = child.cost.clone();
                PhysicalPlan {
                    node: PhysicalNode::Subquery {
                        subplan: Box::new(child),
                        alias: alias.clone(),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }
        }
    }
}

/// Extract a constant integer value from a TypedExpr.
fn extract_constant_usize(expr: &crate::sql::analyzer::types::TypedExpr) -> Option<usize> {
    match &expr.kind {
        TypedExprKind::Constant(crate::types::Value::Int32(v)) => Some(*v as usize),
        TypedExprKind::Constant(crate::types::Value::Int64(v)) => Some(*v as usize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::*;
    use crate::sql::optimizer::logical_planner::LogicalPlanner;
    use crate::types::DataType;

    fn simple_column(name: &str, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: name.to_string(),
            },
            data_type: dt,
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

    /// End-to-end: AnalyzedQuery → LogicalPlan → PhysicalPlan
    #[test]
    fn test_single_table_physical() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("id", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "users".to_string(),
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

        let logical = LogicalPlanner::build(&query);
        let physical = PhysicalPlanner::plan(&logical);

        // Should be: Project → SeqScan
        assert!(matches!(physical.node, PhysicalNode::Project { .. }));
        if let PhysicalNode::Project { input, .. } = &physical.node {
            assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
        }
        assert!(physical.cost.total > 0.0);
    }

    /// TopN optimization: Sort + Limit with small limit → TopNSort
    #[test]
    fn test_topn_optimization() {
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

        let logical = LogicalPlanner::build(&query);
        let physical = PhysicalPlanner::plan(&logical);

        // Should contain TopNSort
        fn has_topn(plan: &PhysicalPlan) -> bool {
            match &plan.node {
                PhysicalNode::TopNSort { .. } => true,
                PhysicalNode::Limit { input, .. }
                | PhysicalNode::Filter { input, .. }
                | PhysicalNode::Project { input, .. }
                | PhysicalNode::Sort { input, .. } => has_topn(input),
                _ => false,
            }
        }
        assert!(has_topn(&physical), "expected TopNSort for small LIMIT");
    }

    /// Hash aggregate for GROUP BY
    #[test]
    fn test_hash_aggregate() {
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

        let logical = LogicalPlanner::build(&query);
        let physical = PhysicalPlanner::plan(&logical);

        assert!(matches!(physical.node, PhysicalNode::HashAggregate { .. }));
    }
}
