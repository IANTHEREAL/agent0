//! Physical planner: `LogicalPlan → PhysicalPlan`.
//!
//! Two-tier estimation:
//! - **No stats** (table never ANALYZEd): exact legacy heuristics — `rows/3` for
//!   filter, `rows/10` for aggregate, `DEFAULT_ESTIMATED_ROWS` for scan.
//! - **Stats available**: selectivity estimation via `selectivity.rs`, histogram-
//!   based range estimates, and n_distinct-based GROUP BY estimates.
//!
//! | Logical       | Physical        | Rule                    |
//! |---------------|-----------------|-------------------------|
//! | Scan          | SeqScan         | Always (index in Ph. 2) |
//! | Join          | HashJoin/NLJ    | HashJoin if equi-keys   |
//! | Aggregate     | HashAggregate   | Always                  |
//! | Sort + Limit  | TopNSort        | When limit+offset < 1K  |

use super::logical_plan::{LogicalNode, LogicalPlan};
use super::physical_plan::{PhysicalCost, PhysicalNode, PhysicalPlan};
use super::selectivity;
use super::statistics::TableStatistics;
use crate::sql::analyzer::types::TypedExprKind;
use std::collections::HashMap;
use std::sync::Arc;

const DEFAULT_ESTIMATED_ROWS: usize = 1000;
const TOPN_THRESHOLD: usize = 1000;

/// Context for physical planning, carrying table statistics.
///
/// Built by the executor before calling `PhysicalPlanner::plan()`.
/// When no statistics are available (empty context), the planner
/// falls back to exact legacy heuristics.
pub struct PlanningContext {
    pub table_stats: HashMap<String, Arc<TableStatistics>>,
}

impl PlanningContext {
    /// Create an empty context (no statistics — legacy behavior).
    pub fn empty() -> Self {
        Self {
            table_stats: HashMap::new(),
        }
    }

    /// Look up table statistics by name.
    pub fn get_stats(&self, table_name: &str) -> Option<&TableStatistics> {
        self.table_stats.get(table_name).map(|arc| arc.as_ref())
    }
}

/// Converts a [`LogicalPlan`] into a [`PhysicalPlan`].
pub struct PhysicalPlanner;

impl PhysicalPlanner {
    /// Plan a logical plan into a physical plan.
    pub fn plan(logical: &LogicalPlan, ctx: &PlanningContext) -> PhysicalPlan {
        Self::plan_node(logical, ctx)
    }

    /// Walk a logical subtree to find base-table stats.
    ///
    /// Returns `None` if no Scan is reachable, or if an Aggregate blocks
    /// propagation (prevents HAVING filters from inheriting base-table stats).
    fn resolve_stats<'a>(
        logical: &LogicalPlan,
        ctx: &'a PlanningContext,
    ) -> Option<&'a TableStatistics> {
        match &logical.node {
            LogicalNode::Scan { table_name, .. } => ctx.get_stats(table_name),
            // Aggregate output schema ≠ base table → block propagation.
            LogicalNode::Aggregate { .. } => None,
            // Transparent unary operators — recurse through.
            LogicalNode::Filter { input, .. }
            | LogicalNode::Project { input, .. }
            | LogicalNode::Sort { input, .. }
            | LogicalNode::Limit { input, .. }
            | LogicalNode::Distinct { input }
            | LogicalNode::DistinctOn { input, .. }
            | LogicalNode::Window { input } => Self::resolve_stats(input, ctx),
            // Multi-input / opaque → no stats.
            _ => None,
        }
    }

    fn plan_node(logical: &LogicalPlan, ctx: &PlanningContext) -> PhysicalPlan {
        match &logical.node {
            // ── Leaf nodes ──────────────────────────────
            LogicalNode::Scan { table_name, alias } => {
                let rows = ctx
                    .get_stats(table_name)
                    .map(|s| s.row_count)
                    .unwrap_or(DEFAULT_ESTIMATED_ROWS);
                PhysicalPlan {
                    node: PhysicalNode::SeqScan {
                        table_name: table_name.clone(),
                        alias: alias.clone(),
                    },
                    schema: logical.schema.clone(),
                    cost: PhysicalCost {
                        startup: 0.0,
                        total: rows as f64 * 0.01 + 1.0,
                        rows,
                    },
                }
            }

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
                let child = Self::plan_node(input, ctx);
                let child_stats = Self::resolve_stats(input, ctx);
                let rows = if let Some(stats) = child_stats {
                    let sel = selectivity::estimate_selectivity(predicate, stats);
                    (child.cost.rows as f64 * sel).ceil() as usize
                } else {
                    (child.cost.rows / 3).max(1) // exact legacy
                };
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
                let child = Self::plan_node(input, ctx);
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
                let child = Self::plan_node(input, ctx);
                let child_stats = Self::resolve_stats(input, ctx);
                let agg_rows = if group_by.is_empty() {
                    1
                } else if let Some(stats) = child_stats {
                    selectivity::estimate_group_by_rows(group_by, stats, child.cost.rows)
                } else {
                    (child.cost.rows / 10).max(1) // exact legacy
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
                let child = Self::plan_node(input, ctx);
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
                let child = Self::plan_node(input, ctx);

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
                let child = Self::plan_node(input, ctx);
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
                let child = Self::plan_node(input, ctx);
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
                let child = Self::plan_node(input, ctx);
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
                let left_phys = Self::plan_node(left, ctx);
                let right_phys = Self::plan_node(right, ctx);
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
                let left_phys = Self::plan_node(left, ctx);
                let right_phys = Self::plan_node(right, ctx);
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
                let child = Self::plan_node(subplan, ctx);
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
    use crate::sql::optimizer::logical_plan::{LogicalPlan, PlanSchema};
    use crate::sql::optimizer::logical_planner::LogicalPlanner;
    use crate::sql::optimizer::statistics::ColumnStatistics;
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

    fn make_table_stats(
        row_count: usize,
        columns: HashMap<String, ColumnStatistics>,
    ) -> Arc<TableStatistics> {
        Arc::new(TableStatistics {
            table_id: 1,
            row_count,
            last_analyzed: 1000,
            columns,
        })
    }

    fn make_col_stats(null_fraction: f64, n_distinct: f64) -> ColumnStatistics {
        ColumnStatistics {
            null_fraction,
            n_distinct,
            avg_width: 4,
            most_common_vals: vec![],
            most_common_freqs: vec![],
            histogram_bounds: vec![],
            correlation: 0.0,
        }
    }

    // ── Test 35: SeqScan with stats ──────────────────────

    #[test]
    fn test_seqscan_with_stats() {
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("users".to_string(), make_table_stats(5000, HashMap::new()));
        let scan = LogicalPlan::scan(
            "users".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let physical = PhysicalPlanner::plan(&scan, &ctx);
        assert_eq!(physical.cost.rows, 5000);
    }

    // ── Test 36: SeqScan without stats ───────────────────

    #[test]
    fn test_seqscan_without_stats() {
        let ctx = PlanningContext::empty();
        let scan = LogicalPlan::scan(
            "users".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let physical = PhysicalPlanner::plan(&scan, &ctx);
        assert_eq!(physical.cost.rows, DEFAULT_ESTIMATED_ROWS);
    }

    // ── Test 37: Filter with stats ───────────────────────

    #[test]
    fn test_filter_with_stats() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnStatistics {
                null_fraction: 0.0,
                n_distinct: 10000.0,
                avg_width: 4,
                most_common_vals: vec![],
                most_common_freqs: vec![],
                histogram_bounds: vec![],
                correlation: 0.0,
            },
        );
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let predicate = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        };
        let filter = scan.filter(predicate);
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        // sel = 1/10000 = 0.0001, rows = ceil(10000 * 0.0001) = 1
        assert_eq!(
            physical.cost.rows, 1,
            "filter with stats: got {}",
            physical.cost.rows
        );
    }

    // ── Test 38: Filter without stats ────────────────────

    #[test]
    fn test_filter_without_stats() {
        let ctx = PlanningContext::empty();
        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let predicate = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        };
        let filter = scan.filter(predicate);
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        // Legacy: rows/3 = 1000/3 = 333
        assert_eq!(
            physical.cost.rows, 333,
            "filter without stats: got {}",
            physical.cost.rows
        );
    }

    // ── Test 39: HAVING filter (Filter above Aggregate) ──

    #[test]
    fn test_having_filter_uses_legacy() {
        let mut cols = HashMap::new();
        cols.insert("id".to_string(), make_col_stats(0.0, 10000.0));
        cols.insert("status".to_string(), make_col_stats(0.0, 50.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![
                ("id".to_string(), DataType::Int64),
                ("status".to_string(), DataType::Text),
            ]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        // HAVING filter sits above Aggregate
        let having_pred = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("cnt", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(5),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        };
        let having = agg.filter(having_pred);
        let physical = PhysicalPlanner::plan(&having, &ctx);

        // Aggregate should use stats (50 groups), but HAVING filter should use
        // legacy /3 because resolve_stats returns None through Aggregate.
        // Aggregate rows = 50, HAVING rows = 50/3 = 16
        assert_eq!(
            physical.cost.rows,
            (50 / 3).max(1),
            "HAVING: got {}",
            physical.cost.rows
        );
    }

    // ── Test 40: Aggregate with stats ────────────────────

    #[test]
    fn test_aggregate_with_stats() {
        let mut cols = HashMap::new();
        cols.insert("status".to_string(), make_col_stats(0.0, 50.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let physical = PhysicalPlanner::plan(&agg, &ctx);
        assert_eq!(
            physical.cost.rows, 50,
            "agg with stats: got {}",
            physical.cost.rows
        );
    }

    // ── Test 41: Aggregate null-group ────────────────────

    #[test]
    fn test_aggregate_null_group() {
        let mut cols = HashMap::new();
        cols.insert("status".to_string(), make_col_stats(0.1, 50.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let physical = PhysicalPlanner::plan(&agg, &ctx);
        // 50 non-null groups + 1 null group = 51
        assert_eq!(
            physical.cost.rows, 51,
            "agg null group: got {}",
            physical.cost.rows
        );
    }

    // ── Test 42: Aggregate all-NULL column ───────────────

    #[test]
    fn test_aggregate_all_null() {
        let mut cols = HashMap::new();
        cols.insert("status".to_string(), make_col_stats(1.0, 0.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let physical = PhysicalPlanner::plan(&agg, &ctx);
        // n_distinct=0, null_frac=1.0 → 0 non-null groups + 1 null group = 1
        assert_eq!(
            physical.cost.rows, 1,
            "agg all-null: got {}",
            physical.cost.rows
        );
    }

    // ── Test 43: Aggregate without stats ─────────────────

    #[test]
    fn test_aggregate_without_stats() {
        let ctx = PlanningContext::empty();
        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let physical = PhysicalPlanner::plan(&agg, &ctx);
        // Legacy: 1000/10 = 100
        assert_eq!(
            physical.cost.rows, 100,
            "agg no stats: got {}",
            physical.cost.rows
        );
    }

    // ── Test 44: End-to-end with stats ───────────────────

    #[test]
    fn test_end_to_end_with_stats() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnStatistics {
                null_fraction: 0.0,
                n_distinct: 5000.0,
                avg_width: 4,
                most_common_vals: vec![],
                most_common_freqs: vec![],
                histogram_bounds: vec![],
                correlation: 0.0,
            },
        );

        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("users".to_string(), make_table_stats(5000, cols));

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
                where_clause: Some(TypedExpr {
                    kind: TypedExprKind::BinaryOp {
                        left: Box::new(simple_column("id", DataType::Int64)),
                        op: BinaryOp::Eq,
                        right: Box::new(simple_constant(
                            crate::types::Value::Int32(42),
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
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };

        let logical = LogicalPlanner::build(&query);
        let physical = PhysicalPlanner::plan(&logical, &ctx);

        // Scan should have 5000 rows (from stats)
        // Filter (eq on 5000 distinct) → sel = 1/5000 → 1 row
        // Verify the plan is sensible
        assert!(
            physical.cost.rows < 100,
            "e2e: rows should be small, got {}",
            physical.cost.rows
        );
    }

    // ── Test 45: Existing tests still pass with empty ctx ─

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
        let physical = PhysicalPlanner::plan(&logical, &PlanningContext::empty());

        assert!(matches!(physical.node, PhysicalNode::Project { .. }));
        if let PhysicalNode::Project { input, .. } = &physical.node {
            assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
        }
        assert!(physical.cost.total > 0.0);
    }

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
        let physical = PhysicalPlanner::plan(&logical, &PlanningContext::empty());

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
        let physical = PhysicalPlanner::plan(&logical, &PlanningContext::empty());

        assert!(matches!(physical.node, PhysicalNode::HashAggregate { .. }));
    }

    // ── Gate tests ───────────────────────────────────────

    #[test]
    fn test_gate1_stats_improve_estimates() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnStatistics {
                null_fraction: 0.0,
                n_distinct: 100.0,
                avg_width: 4,
                most_common_vals: vec![],
                most_common_freqs: vec![],
                histogram_bounds: vec![],
                correlation: 0.0,
            },
        );
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let predicate = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        };
        let filter = scan.filter(predicate);
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        // sel = 1/100 = 0.01, rows = ceil(10000 * 0.01) = 100
        assert_eq!(physical.cost.rows, 100, "gate1: got {}", physical.cost.rows);
    }

    #[test]
    fn test_gate2_no_stats_exact_legacy() {
        let ctx = PlanningContext::empty();
        // Scan
        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let scan_phys = PhysicalPlanner::plan(&scan, &ctx);
        assert_eq!(scan_phys.cost.rows, 1000);

        // Filter
        let scan2 = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let filter = scan2.filter(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        });
        let filter_phys = PhysicalPlanner::plan(&filter, &ctx);
        assert_eq!(filter_phys.cost.rows, 333);

        // Aggregate
        let scan3 = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan3.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg_phys = PhysicalPlanner::plan(&agg, &ctx);
        assert_eq!(agg_phys.cost.rows, 100);
    }

    #[test]
    fn test_gate3_selectivity_bounds() {
        let mut cols = HashMap::new();
        cols.insert("id".to_string(), make_col_stats(0.0, 100.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(100, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let filter = scan.filter(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        });
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        // Rows can be 0 — no forced .max(1)
        assert!(physical.cost.rows <= 100);
    }

    #[test]
    fn test_gate4_negative_n_distinct() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnStatistics {
                null_fraction: 0.0,
                n_distinct: -0.5,
                avg_width: 4,
                most_common_vals: vec![],
                most_common_freqs: vec![],
                histogram_bounds: vec![],
                correlation: 0.0,
            },
        );
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(2000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let filter = scan.filter(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        });
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        // eff = 0.5 * 2000 = 1000, sel = 1/1000, rows = ceil(2000 * 0.001) = 2
        assert_eq!(physical.cost.rows, 2, "gate4: got {}", physical.cost.rows);
    }

    #[test]
    fn test_gate5_having_isolation() {
        let mut cols = HashMap::new();
        cols.insert("status".to_string(), make_col_stats(0.0, 50.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg_phys = PhysicalPlanner::plan(&agg, &ctx);
        assert_eq!(agg_phys.cost.rows, 50, "agg uses stats");

        // HAVING filter above aggregate uses legacy
        let having = agg.filter(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("cnt", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(5),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        });
        let having_phys = PhysicalPlanner::plan(&having, &ctx);
        assert_eq!(
            having_phys.cost.rows,
            (50 / 3).max(1),
            "HAVING uses legacy /3"
        );
    }

    #[test]
    fn test_gate6_null_constant_zero_selectivity() {
        let mut cols = HashMap::new();
        cols.insert("id".to_string(), make_col_stats(0.1, 100.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(1000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let filter = scan.filter(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(simple_constant(crate::types::Value::Null, DataType::Int64)),
            },
            data_type: DataType::Boolean,
        });
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        assert_eq!(physical.cost.rows, 0, "NULL eq = 0 rows");
    }

    #[test]
    fn test_gate7_group_by_null_group() {
        let mut cols = HashMap::new();
        cols.insert("status".to_string(), make_col_stats(1.0, 0.0));
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(10000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let agg = scan.aggregate(
            vec![simple_column("status", DataType::Text)],
            vec![simple_projection("status", DataType::Text)],
            PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
        );
        let physical = PhysicalPlanner::plan(&agg, &ctx);
        assert_eq!(
            physical.cost.rows, 1,
            "null-group: got {}",
            physical.cost.rows
        );
    }

    #[test]
    fn test_gate8_negated_predicates_null_safe() {
        let mut cols = HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnStatistics {
                null_fraction: 0.2,
                n_distinct: 100.0,
                avg_width: 4,
                most_common_vals: vec![crate::types::Value::Int32(1)],
                most_common_freqs: vec![0.1],
                histogram_bounds: vec![],
                correlation: 0.0,
            },
        );
        let mut ctx = PlanningContext::empty();
        ctx.table_stats
            .insert("t".to_string(), make_table_stats(1000, cols));

        let scan = LogicalPlan::scan(
            "t".to_string(),
            None,
            PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        );
        let filter = scan.filter(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(simple_column("id", DataType::Int64)),
                op: BinaryOp::NotEq,
                right: Box::new(simple_constant(
                    crate::types::Value::Int32(1),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        });
        let physical = PhysicalPlanner::plan(&filter, &ctx);
        // sel ≈ (1.0 - 0.2) - 0.1 ≈ 0.7, rows ≈ 700 (ceil may round up by 1
        // due to IEEE 754 intermediate rounding)
        assert!(
            (700..=701).contains(&physical.cost.rows),
            "gate8: got {}",
            physical.cost.rows
        );
    }
}
