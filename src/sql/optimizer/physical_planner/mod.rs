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
use super::statistics::TableStatistics;
use super::{
    extract_constant_usize, join_keys, selectivity, DEFAULT_ESTIMATED_ROWS, DEFAULT_JOIN_SEL,
};
use crate::model::{DataType, TableSchema};
use crate::sql::analyzer::types::{JoinCondition, JoinType};
use crate::sql::planner::hnsw_predicate::{detect_hnsw_scan_opportunity, estimate_hnsw_scan_cost};
use std::collections::HashMap;
use std::sync::Arc;

const TOPN_THRESHOLD: usize = 1000;

/// Context for physical planning, carrying table statistics and schemas.
///
/// Built by the executor before calling `PhysicalPlanner::plan()`.
/// When no statistics are available (empty context), the planner
/// falls back to exact legacy heuristics.
///
/// `table_schemas` carries full [`TableSchema`] (including index metadata)
/// for access-path selection. Pre-loaded by the executor and shared with
/// [`BuildContext`](super::BuildContext) to eliminate redundant catalog reads.
pub struct PlanningContext {
    pub table_stats: HashMap<String, Arc<TableStatistics>>,
    pub table_schemas: HashMap<String, TableSchema>,
    pub enable_db9_cop_pushdown: bool,
    pub txn_dirty_table_ids: std::collections::HashSet<u64>,
    pub statement_dirty_table_ids: std::collections::HashSet<u64>,
}

impl PlanningContext {
    /// Create an empty context (no statistics or schemas — legacy behavior).
    pub fn empty() -> Self {
        Self {
            table_stats: HashMap::new(),
            table_schemas: HashMap::new(),
            enable_db9_cop_pushdown:
                crate::sql::query_context::QueryContext::current_setting_snapshot(
                    "db9.enable_cop_pushdown",
                )
                .map(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "on" | "true" | "yes" | "1"
                    )
                })
                .unwrap_or(false),
            txn_dirty_table_ids: (*crate::session_context::current_txn_dirty_table_ids()).clone(),
            statement_dirty_table_ids: crate::session_context::current_statement_dirty_table_ids(),
        }
    }

    /// Look up table statistics by name.
    pub fn get_stats(&self, table_name: &str) -> Option<&TableStatistics> {
        self.table_stats.get(table_name).map(|arc| arc.as_ref())
    }

    /// Look up table schema by name.
    pub fn get_schema(&self, table_name: &str) -> Option<&TableSchema> {
        self.table_schemas.get(table_name)
    }
}

fn table_schema_contains_vector_columns(schema: &TableSchema) -> bool {
    schema
        .columns
        .iter()
        .any(|column| data_type_contains_vector(&column.data_type))
}

fn data_type_contains_vector(data_type: &DataType) -> bool {
    match data_type {
        DataType::Vector(_) => true,
        DataType::Array(elem_type) => data_type_contains_vector(elem_type),
        _ => false,
    }
}

/// Converts a [`LogicalPlan`] into a [`PhysicalPlan`].
pub struct PhysicalPlanner;

impl PhysicalPlanner {
    /// Plan a logical plan into a physical plan.
    pub fn plan(logical: &LogicalPlan, ctx: &PlanningContext) -> PhysicalPlan {
        let plan = Self::plan_node(logical, ctx);
        if ctx.enable_db9_cop_pushdown {
            let base_table_keys: std::collections::HashSet<String> = ctx
                .table_schemas
                .iter()
                .filter(|(_, schema)| {
                    !ctx.txn_dirty_table_ids.contains(&schema.table_id)
                        && !ctx.statement_dirty_table_ids.contains(&schema.table_id)
                        && !table_schema_contains_vector_columns(schema)
                })
                .map(|(key, _)| key.clone())
                .collect();
            if base_table_keys.is_empty() {
                plan
            } else {
                super::pushdown::apply_db9_cop_folding_for_base_tables(plan, &base_table_keys)
            }
        } else {
            plan
        }
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
            LogicalNode::Scan { table_name, alias } => {
                let key = super::schema_map_key(table_name, alias.as_deref());
                ctx.get_stats(&key)
            }
            // Aggregate output schema ≠ base table → block propagation.
            LogicalNode::Aggregate { .. } => None,
            // Transparent unary operators — recurse through.
            LogicalNode::Filter { input, .. }
            | LogicalNode::Project { input, .. }
            | LogicalNode::Sort { input, .. }
            | LogicalNode::Limit { input, .. }
            | LogicalNode::Distinct { input }
            | LogicalNode::DistinctOn { input, .. }
            | LogicalNode::Window { input, .. } => Self::resolve_stats(input, ctx),
            // Multi-input / opaque → no stats.
            _ => None,
        }
    }

    /// Compute raw equi-join selectivity from pre-resolved column-name pairs.
    ///
    /// `key_col_pairs` is a slice of `(left_col_name, right_col_name)` for each
    /// equi-join key, resolved from the schema before calling.  When either side
    /// lacks statistics the function falls back to `DEFAULT_JOIN_SEL`.  The
    /// `has_residual` flag indicates that a non-equi residual predicate is also
    /// present; when `true` an additional `DEFAULT_JOIN_SEL` factor is applied.
    ///
    /// Returns a raw `f64` — callers apply join-type row lower-bounds or
    /// selectivity clamping as needed.
    fn compute_equi_join_sel(
        left_stats: Option<&TableStatistics>,
        right_stats: Option<&TableStatistics>,
        key_col_pairs: &[(Option<&str>, Option<&str>)],
        has_residual: bool,
    ) -> f64 {
        let equi_sel = match (left_stats, right_stats) {
            (Some(ls), Some(rs)) => {
                let mut sel = 1.0_f64;
                for &(left_col_name, right_col_name) in key_col_pairs {
                    let left_ndv = left_col_name
                        .and_then(|n| selectivity::get_column_stats(ls, n))
                        .map(|c| selectivity::n_distinct_raw(c, ls.row_count));
                    let right_ndv = right_col_name
                        .and_then(|n| selectivity::get_column_stats(rs, n))
                        .map(|c| selectivity::n_distinct_raw(c, rs.row_count));
                    sel *= match (left_ndv, right_ndv) {
                        (Some(l), Some(r)) => 1.0 / l.max(r).max(1.0),
                        _ => DEFAULT_JOIN_SEL,
                    };
                }
                sel
            }
            _ => DEFAULT_JOIN_SEL,
        };
        if has_residual {
            equi_sel * DEFAULT_JOIN_SEL
        } else {
            equi_sel
        }
    }

    /// Estimate join output rows.
    ///
    /// Strategy:
    /// 1. Equi-joins with stats on BOTH sides: 1/max(NDV_left, NDV_right) per key pair.
    ///    Column lookup uses `selectivity::get_column_stats` (case-insensitive, ambiguity-safe).
    /// 2. Equi-joins without stats on one/both sides: DEFAULT_JOIN_SEL.
    /// 3. Non-equi / cross joins: Cartesian product (selectivity = 1.0).
    /// 4. Clamp to join-type semantic lower bound.
    fn estimate_join_rows(
        left_logical: &LogicalPlan,
        right_logical: &LogicalPlan,
        left_rows: usize,
        right_rows: usize,
        join_type: &JoinType,
        condition: &JoinCondition,
        ctx: &PlanningContext,
    ) -> usize {
        let left_width = left_logical.schema.columns.len();

        let selectivity = if let Some((left_keys, right_keys, residual_filter)) =
            join_keys::extract_equi_keys_with_residual(condition, left_width)
        {
            let left_stats = Self::resolve_stats(left_logical, ctx);
            let right_stats = Self::resolve_stats(right_logical, ctx);
            let key_col_pairs: Vec<(Option<&str>, Option<&str>)> = left_keys
                .iter()
                .zip(right_keys.iter())
                .map(|(&lk, &rk)| {
                    let lname = left_logical.schema.columns.get(lk).map(|(n, _)| n.as_str());
                    let rname = right_logical
                        .schema
                        .columns
                        .get(rk)
                        .map(|(n, _)| n.as_str());
                    (lname, rname)
                })
                .collect();
            Self::compute_equi_join_sel(
                left_stats,
                right_stats,
                &key_col_pairs,
                residual_filter.is_some(),
            )
        } else {
            1.0 // Non-equi or cross — Cartesian
        };

        let inner_est = ((left_rows as f64) * (right_rows as f64) * selectivity).ceil() as usize;

        let min_rows = match join_type {
            JoinType::Left => left_rows,
            JoinType::Right => right_rows,
            JoinType::Full => left_rows.max(right_rows),
            _ => 0, // Inner, Cross: no structural minimum
        };

        inner_est.max(min_rows).max(1)
    }

    /// Estimate join selectivity for semi/anti join cost estimation.
    fn estimate_join_selectivity(
        left: &LogicalPlan,
        right: &LogicalPlan,
        condition: &JoinCondition,
        ctx: &PlanningContext,
    ) -> f64 {
        let left_width = left.schema.columns.len();
        if let Some((left_keys, right_keys, residual_filter)) =
            join_keys::extract_equi_keys_with_residual(condition, left_width)
        {
            let left_stats = Self::resolve_stats(left, ctx);
            let right_stats = Self::resolve_stats(right, ctx);
            let key_col_pairs: Vec<(Option<&str>, Option<&str>)> = left_keys
                .iter()
                .zip(right_keys.iter())
                .map(|(&lk, &rk)| {
                    let lname = left.schema.columns.get(lk).map(|(n, _)| n.as_str());
                    let rname = right.schema.columns.get(rk).map(|(n, _)| n.as_str());
                    (lname, rname)
                })
                .collect();
            let raw_sel = Self::compute_equi_join_sel(
                left_stats,
                right_stats,
                &key_col_pairs,
                residual_filter.is_some(),
            );
            if residual_filter.is_some() {
                raw_sel.clamp(0.0, 1.0)
            } else {
                raw_sel
            }
        } else {
            DEFAULT_JOIN_SEL
        }
    }

    fn plan_node(logical: &LogicalPlan, ctx: &PlanningContext) -> PhysicalPlan {
        match &logical.node {
            // ── Leaf nodes ──────────────────────────────
            LogicalNode::Scan { table_name, alias } => {
                let key = super::schema_map_key(table_name, alias.as_deref());
                let rows = ctx
                    .get_stats(&key)
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

                // Access-path selection: when Filter sits above SeqScan and
                // we have index metadata, choose the best B-tree/GIN/SeqScan path.
                let scan_node = if let PhysicalNode::SeqScan { table_name, alias } = &child.node {
                    let scan_key = super::schema_map_key(table_name, alias.as_deref());
                    if let Some(schema) = ctx.get_schema(&scan_key) {
                        let access_path =
                            crate::sql::planner::choose_btree_access_path_for_typed_filter(
                                schema,
                                predicate,
                                child.cost.rows,
                                ctx.get_stats(&scan_key),
                            );
                        match &access_path.scan_type {
                            crate::sql::planner::ScanType::FullTableScan => None,
                            _ => {
                                let index_cost = PhysicalCost {
                                    startup: 0.0,
                                    total: access_path.cost,
                                    rows: child.cost.rows,
                                };
                                Some(PhysicalPlan {
                                    node: PhysicalNode::IndexScan {
                                        table_name: table_name.clone(),
                                        alias: alias.clone(),
                                        scan_type: access_path.scan_type,
                                    },
                                    schema: child.schema.clone(),
                                    cost: index_cost,
                                })
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let effective_child = scan_node.unwrap_or(child);
                let cost = PhysicalCost {
                    startup: effective_child.cost.startup,
                    total: effective_child.cost.total + rows as f64 * 0.01,
                    rows,
                };
                PhysicalPlan {
                    node: PhysicalNode::Filter {
                        predicate: predicate.clone(),
                        input: Box::new(effective_child),
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

                // Check for TopN optimization: Sort + Limit with small limit.
                // After the plan restructuring, the shape for non-aggregate
                // queries is Limit → Project → Sort, so we look through a
                // single Project node when the direct child is not a Sort.
                let (sort_order_by, sort_input, project_wrapper) = match &child.node {
                    PhysicalNode::Sort {
                        order_by,
                        input: si,
                    } => (Some(order_by), Some(si), None),
                    PhysicalNode::Project {
                        projections,
                        input: proj_input,
                    } => {
                        if let PhysicalNode::Sort {
                            order_by,
                            input: si,
                        } = &proj_input.node
                        {
                            (Some(order_by), Some(si), Some(projections))
                        } else {
                            (None, None, None)
                        }
                    }
                    _ => (None, None, None),
                };

                if let (Some(limit_expr), Some(order_by), Some(sort_input)) =
                    (limit, sort_order_by, sort_input)
                {
                    if let Some(limit_val) = extract_constant_usize(limit_expr) {
                        let offset_val = offset
                            .as_ref()
                            .and_then(extract_constant_usize)
                            .unwrap_or(0);

                        if offset_val == 0 {
                            if let PhysicalNode::SeqScan { table_name, alias } = &sort_input.node {
                                let scan_key = super::schema_map_key(table_name, alias.as_deref());
                                if let Some(schema) = ctx.get_schema(&scan_key) {
                                    if let Some(hnsw_params) = detect_hnsw_scan_opportunity(
                                        schema,
                                        order_by,
                                        Some(limit_val),
                                        &schema.indexes,
                                    ) {
                                        let full_scan_sort_cost = sort_input.cost.total
                                            + (sort_input.cost.rows as f64
                                                * (sort_input.cost.rows as f64).log2().max(1.0));
                                        let hnsw_scan_cost = estimate_hnsw_scan_cost(hnsw_params.k);

                                        if hnsw_scan_cost < full_scan_sort_cost {
                                            let hnsw_scan_plan = PhysicalPlan {
                                                node: PhysicalNode::HnswScan {
                                                    table_name: table_name.clone(),
                                                    alias: alias.clone(),
                                                    scan_type: crate::sql::planner::ScanType::HnswIndexScan {
                                                        index_id: hnsw_params.index_id,
                                                        index_name: hnsw_params.index_name,
                                                        query_vector: hnsw_params.query_vector,
                                                        k: hnsw_params.k,
                                                        distance_metric: hnsw_params.distance_metric,
                                                        distance_expr: Some(Box::new(
                                                            hnsw_params.distance_expr,
                                                        )),
                                                    },
                                                },
                                                schema: sort_input.schema.clone(),
                                                cost: PhysicalCost {
                                                    startup: 0.0,
                                                    total: hnsw_scan_cost,
                                                    rows: limit_val.min(sort_input.cost.rows),
                                                },
                                            };

                                            let limit_child = if let Some(projections) =
                                                project_wrapper
                                            {
                                                PhysicalPlan {
                                                    node: PhysicalNode::Project {
                                                        projections: projections.clone(),
                                                        input: Box::new(hnsw_scan_plan),
                                                    },
                                                    schema: child.schema.clone(),
                                                    cost: PhysicalCost {
                                                        startup: 0.0,
                                                        total: hnsw_scan_cost,
                                                        rows: limit_val.min(sort_input.cost.rows),
                                                    },
                                                }
                                            } else {
                                                hnsw_scan_plan
                                            };

                                            let limit_cost = PhysicalCost {
                                                startup: limit_child.cost.startup,
                                                total: limit_child.cost.total
                                                    + limit_val as f64 * 0.01,
                                                rows: limit_val.min(limit_child.cost.rows),
                                            };

                                            return PhysicalPlan {
                                                node: PhysicalNode::Limit {
                                                    limit: limit.clone(),
                                                    offset: offset.clone(),
                                                    input: Box::new(limit_child),
                                                },
                                                schema: logical.schema.clone(),
                                                cost: limit_cost,
                                            };
                                        }
                                    }
                                }
                            }
                        }

                        if limit_val + offset_val < TOPN_THRESHOLD {
                            let effective_limit = limit_val + offset_val;
                            let topn_cost = PhysicalCost {
                                startup: sort_input.cost.startup,
                                total: sort_input.cost.total + effective_limit as f64 * 0.01,
                                rows: limit_val,
                            };
                            let topn_plan = PhysicalPlan {
                                node: PhysicalNode::TopNSort {
                                    order_by: order_by.clone(),
                                    limit: effective_limit,
                                    input: sort_input.clone(),
                                },
                                schema: sort_input.schema.clone(),
                                cost: topn_cost.clone(),
                            };
                            // Re-wrap in Project if we looked through one.
                            let limit_child = if let Some(projections) = project_wrapper {
                                PhysicalPlan {
                                    node: PhysicalNode::Project {
                                        projections: projections.clone(),
                                        input: Box::new(topn_plan),
                                    },
                                    schema: child.schema.clone(),
                                    cost: topn_cost.clone(),
                                }
                            } else {
                                topn_plan
                            };
                            return PhysicalPlan {
                                node: PhysicalNode::Limit {
                                    limit: limit.clone(),
                                    offset: offset.clone(),
                                    input: Box::new(limit_child),
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

            LogicalNode::Window {
                window_functions,
                input,
                ..
            } => {
                let child = Self::plan_node(input, ctx);
                let cost = child.cost.clone();
                PhysicalPlan {
                    node: PhysicalNode::Window {
                        window_functions: window_functions.clone(),
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
                let left_rows = left_phys.cost.rows;
                let right_rows = right_phys.cost.rows;
                let left_width = left.schema.columns.len();

                let total_rows = Self::estimate_join_rows(
                    left, right, left_rows, right_rows, join_type, condition, ctx,
                );
                let total_cost =
                    left_phys.cost.total + right_phys.cost.total + total_rows as f64 * 0.01;
                let cost = PhysicalCost {
                    startup: 0.0,
                    total: total_cost,
                    rows: total_rows,
                };

                // Algorithm selection: HashJoin whenever ON has at least one
                // cross-boundary equi key (residual conjuncts are evaluated as
                // hash-join filters), NLJ otherwise.
                let right_depends_on_outer =
                    crate::sql::optimizer::join_reorder::logical_has_correlated_refs(right);
                let node = if !right_depends_on_outer
                    && join_keys::extract_equi_keys_with_residual(condition, left_width).is_some()
                {
                    let left_is_build = left_rows <= right_rows;
                    PhysicalNode::HashJoin {
                        left: Box::new(left_phys),
                        right: Box::new(right_phys),
                        join_type: *join_type,
                        condition: condition.clone(),
                        left_is_build,
                    }
                } else {
                    PhysicalNode::NestedLoopJoin {
                        left: Box::new(left_phys),
                        right: Box::new(right_phys),
                        join_type: *join_type,
                        condition: condition.clone(),
                    }
                };

                PhysicalPlan {
                    node,
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

            LogicalNode::SemiJoin {
                left,
                right,
                condition,
            } => {
                let left_phys = Self::plan_node(left, ctx);
                let right_phys = Self::plan_node(right, ctx);
                let left_rows = left_phys.cost.rows;
                let sel = Self::estimate_join_selectivity(left, right, condition, ctx);
                let rows = ((left_rows as f64) * sel).ceil() as usize;
                let rows = rows.max(1);
                let total_cost = left_phys.cost.total + right_phys.cost.total + rows as f64 * 0.01;
                PhysicalPlan {
                    node: PhysicalNode::HashSemiJoin {
                        left: Box::new(left_phys),
                        right: Box::new(right_phys),
                        anti: false,
                        condition: condition.clone(),
                    },
                    schema: logical.schema.clone(),
                    cost: PhysicalCost {
                        startup: 0.0,
                        total: total_cost,
                        rows,
                    },
                }
            }

            LogicalNode::AntiJoin {
                left,
                right,
                condition,
            } => {
                let left_phys = Self::plan_node(left, ctx);
                let right_phys = Self::plan_node(right, ctx);
                let left_rows = left_phys.cost.rows;
                let sel = Self::estimate_join_selectivity(left, right, condition, ctx);
                let rows = ((left_rows as f64) * (1.0 - sel)).ceil() as usize;
                let rows = rows.max(1);
                let total_cost = left_phys.cost.total + right_phys.cost.total + rows as f64 * 0.01;
                PhysicalPlan {
                    node: PhysicalNode::HashSemiJoin {
                        left: Box::new(left_phys),
                        right: Box::new(right_phys),
                        anti: true,
                        condition: condition.clone(),
                    },
                    schema: logical.schema.clone(),
                    cost: PhysicalCost {
                        startup: 0.0,
                        total: total_cost,
                        rows,
                    },
                }
            }

            LogicalNode::Subquery { subplan, .. } => {
                let child = Self::plan_node(subplan, ctx);
                let cost = child.cost.clone();
                PhysicalPlan {
                    node: PhysicalNode::Subquery {
                        subplan: Box::new(child),
                    },
                    schema: logical.schema.clone(),
                    cost,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
