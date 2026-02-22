//! Operator bridge: translates [`PhysicalPlan`] into executable [`BoxedOperator`] trees.
//!
//! This module is the final step in the CBO pipeline:
//! `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`
//!
//! The [`BuildContext`] carries pre-resolved table schemas so that operator
//! construction is a pure, synchronous tree walk (no async catalog lookups).

mod aggregate;
mod join;
mod scan;
mod utils;

#[cfg(test)]
mod tests;

// Re-export pub(crate) items so that sibling modules (logical_planner, eligibility)
// can continue to reference them as `super::build::*`.
pub(crate) use aggregate::{aggregate_identity_matches, rewrite_post_aggregate_expr};
#[allow(unused_imports)]
pub(crate) use utils::{collect_agg_exprs_from, find_matching_group_by};

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};

use super::physical_plan::{PhysicalNode, PhysicalPlan};
use crate::sql::analyzer::types::{JoinType, SetOpKind, TypedExpr};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_eval::eval_const_usize;
use crate::sql::operators::{
    BoxedOperator, DistinctOnOperator, DistinctOperator, FilterOperator, HashJoinConfig,
    HashJoinOperator, HashJoinType, HashSemiJoinOperator, LimitOperator, NestedLoopJoinOperator,
    ProjectOperator, SetOperationOperator, SetOperationType, SortOperator, TableScanOperator,
};
use crate::types::{DataType, Row, TableSchema};

/// Context needed to translate a [`PhysicalPlan`] into operator trees.
///
/// The caller pre-resolves all table schemas from the catalog/store and
/// passes them here.  This keeps operator construction synchronous.
///
/// For virtual catalog tables, table functions, and CTEs, the caller also
/// pre-loads row data into `preloaded_rows`.
#[derive(Debug)]
pub struct BuildContext {
    /// Pre-resolved table schemas, keyed by table name (as stored in the plan).
    pub table_schemas: HashMap<String, TableSchema>,
    /// Pre-loaded row data for tables that don't live in KV storage
    /// (virtual catalog tables, table functions, CTEs with materialized data).
    pub preloaded_rows: HashMap<String, Vec<Row>>,
    pub correlated_table_functions: HashSet<String>,
}

impl BuildContext {
    pub fn new() -> Self {
        Self {
            table_schemas: HashMap::new(),
            preloaded_rows: HashMap::new(),
            correlated_table_functions: HashSet::new(),
        }
    }

    #[allow(dead_code)]
    pub fn with_schema(mut self, name: String, schema: TableSchema) -> Self {
        self.table_schemas.insert(name, schema);
        self
    }

    #[allow(dead_code)]
    pub fn with_preloaded_rows(mut self, name: String, rows: Vec<Row>) -> Self {
        self.preloaded_rows.insert(name, rows);
        self
    }
}

impl PhysicalPlan {
    /// Translate this physical plan tree into an executable operator tree.
    ///
    /// Recursively walks the plan, constructing the appropriate `BoxedOperator`
    /// for each `PhysicalNode`.  All table schemas must be pre-resolved in `ctx`.
    pub fn build_operators(&self, ctx: &BuildContext) -> Result<BoxedOperator> {
        match &self.node {
            // ── Scan operators ──────────────────────────────────
            PhysicalNode::SeqScan { table_name, alias } => {
                scan::build_seq_scan_operator(ctx, table_name, alias.as_deref(), None)
            }

            PhysicalNode::IndexScan {
                table_name,
                alias,
                scan_type,
            } => {
                scan::build_index_scan_operator(ctx, table_name, alias.as_deref(), scan_type, None)
            }

            PhysicalNode::Empty => {
                // No-input operator for SELECT without FROM.
                // Use a single-row empty schema scan so projection can evaluate constants.
                let schema = TableSchema::new("__empty".to_string(), 0, vec![], vec![]);
                Ok(Box::new(TableScanOperator::new_with_rows(
                    schema,
                    vec![crate::types::Row::new(vec![])],
                )))
            }

            PhysicalNode::Values { rows } => {
                // Evaluate constant expressions at build time to produce literal rows.
                let qctx = crate::sql::query_context::QueryContext::from_task_locals();
                let dummy_row = Row::new(vec![]);
                let mut result_rows = Vec::with_capacity(rows.len());
                for value_row in rows {
                    let mut values = Vec::with_capacity(value_row.len());
                    for expr in value_row {
                        values.push(crate::sql::expr::typed_eval::eval_typed_expr(
                            expr, &dummy_row, &qctx,
                        )?);
                    }
                    result_rows.push(Row::new(values));
                }
                // Build schema from the plan's output schema.
                let schema = TableSchema::new(
                    "__values".to_string(),
                    0,
                    self.schema
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(_i, (name, dt))| crate::types::ColumnDef {
                            name: name.clone(),
                            data_type: dt.clone(),
                            nullable: true,
                            primary_key: false,
                            unique: false,
                            is_serial: false,
                            default_expr: None,
                            collation: None,
                        })
                        .collect(),
                    vec![],
                );
                Ok(Box::new(TableScanOperator::new_with_rows(
                    schema,
                    result_rows,
                )))
            }

            PhysicalNode::TableFunction {
                function_name,
                alias,
                ..
            } => {
                // Table functions are pre-executed during context preparation.
                // Look up by alias first (the key used during pre-loading), then by function name.
                let key = alias.as_deref().unwrap_or(function_name.as_str());
                let schema = ctx
                    .table_schemas
                    .get(key)
                    .or_else(|| ctx.table_schemas.get(function_name.as_str()))
                    .ok_or_else(|| anyhow!("Table function schema not found: {}", function_name))?;
                if ctx.correlated_table_functions.contains(key)
                    || ctx
                        .correlated_table_functions
                        .contains(function_name.as_str())
                {
                    return Err(SqlError::Unsupported(format!(
                        "table function {}() has correlated arguments referencing an outer query; \
                         LATERAL table functions are not yet supported",
                        function_name
                    ))
                    .into());
                }
                let rows = ctx
                    .preloaded_rows
                    .get(key)
                    .or_else(|| ctx.preloaded_rows.get(function_name.as_str()))
                    .cloned()
                    .unwrap_or_default();
                let mut schema = schema.clone();
                if let Some(a) = alias {
                    schema.from_alias = Some(a.clone());
                }
                Ok(Box::new(TableScanOperator::new_with_rows(schema, rows)))
            }

            // ── Unary operators ─────────────────────────────────
            PhysicalNode::Filter { predicate, input } => {
                let child = input.build_operators(ctx)?;
                Ok(Box::new(FilterOperator::new(child, predicate.clone())))
            }

            PhysicalNode::Project { projections, input } => {
                let child = input.build_operators(ctx)?;
                let expressions: Vec<TypedExpr> =
                    projections.iter().map(|p| p.expr.clone()).collect();
                let output_names: Vec<String> =
                    projections.iter().map(|p| p.output_name.clone()).collect();
                let output_types: Vec<DataType> = projections
                    .iter()
                    .map(|p| p.expr.data_type.clone())
                    .collect();
                Ok(Box::new(ProjectOperator::new(
                    child,
                    expressions,
                    output_names,
                    output_types,
                )))
            }

            PhysicalNode::HashAggregate {
                group_by,
                projections,
                input,
            } => {
                let child = input.build_operators(ctx)?;
                aggregate::build_hash_aggregate(child, group_by, projections)
            }

            PhysicalNode::StreamAggregate {
                group_by,
                projections,
                input,
            } => {
                // Phase 1: stream aggregate falls back to hash aggregate.
                let child = input.build_operators(ctx)?;
                aggregate::build_hash_aggregate(child, group_by, projections)
            }

            PhysicalNode::Sort { order_by, input } => {
                let child = input.build_operators(ctx)?;
                Ok(Box::new(SortOperator::new(child, order_by.clone())))
            }

            PhysicalNode::TopNSort {
                order_by,
                limit,
                input,
            } => {
                // TopN = Sort + Limit (no dedicated TopN operator yet).
                let child = input.build_operators(ctx)?;
                let sorted = Box::new(SortOperator::new(child, order_by.clone()));
                Ok(Box::new(LimitOperator::new(sorted, Some(*limit), 0)))
            }

            PhysicalNode::Limit {
                limit,
                offset,
                input,
            } => {
                // LIMIT/OFFSET semantics are runtime-evaluable (parameters allowed).
                // Constant extraction is optimization-only.
                let limit_const = limit
                    .as_ref()
                    .and_then(|expr| eval_const_usize(expr, false).ok());
                let offset_const = offset
                    .as_ref()
                    .and_then(|expr| eval_const_usize(expr, false).ok());
                // Root fix for LIMIT pushdown on the analyzed/optimizer path:
                // push scan limit only for LIMIT ... OFFSET 0 directly over a KV scan.
                // This keeps semantics intact while preventing full index scans for
                // simple top-N probes (e.g. tests/95_limit_pushdown.sql).
                let child = if offset.is_none() || matches!(offset_const, Some(0)) {
                    if let Some(scan_limit) = limit_const {
                        scan::build_limit_child_with_scan_pushdown(input, ctx, scan_limit)?
                            .unwrap_or(input.build_operators(ctx)?)
                    } else {
                        input.build_operators(ctx)?
                    }
                } else {
                    input.build_operators(ctx)?
                };
                Ok(Box::new(LimitOperator::new_with_exprs(
                    child,
                    limit.clone(),
                    offset.clone(),
                )))
            }

            PhysicalNode::Distinct { input } => {
                let child = input.build_operators(ctx)?;
                Ok(Box::new(DistinctOperator::new(child)))
            }

            PhysicalNode::DistinctOn { on_exprs, input } => {
                let child = input.build_operators(ctx)?;
                Ok(Box::new(DistinctOnOperator::new(child, on_exprs.clone())))
            }

            PhysicalNode::Window {
                window_functions,
                input,
            } => {
                let child = input.build_operators(ctx)?;
                Ok(Box::new(crate::sql::operators::WindowOperator::new(
                    child,
                    window_functions.clone(),
                )))
            }

            // ── Binary operators ────────────────────────────────
            PhysicalNode::NestedLoopJoin {
                left,
                right,
                join_type,
                condition,
            } => {
                let left_op = left.build_operators(ctx)?;
                let right_op = right.build_operators(ctx)?;
                let op_join_type = join::convert_join_type(join_type);
                let cond = join::extract_on_condition(condition);
                let right_depends_on_outer = join::plan_has_correlated_refs(right);
                Ok(Box::new(
                    NestedLoopJoinOperator::new(left_op, right_op, op_join_type, cond)
                        .with_outer_dependency(right_depends_on_outer),
                ))
            }

            PhysicalNode::HashJoin {
                left,
                right,
                join_type,
                condition,
                left_is_build,
            } => {
                if join::plan_has_correlated_refs(right) {
                    return Err(anyhow!(
                        "HashJoin does not support correlated right input; planner should choose NestedLoopJoin"
                    ));
                }
                let left_op = left.build_operators(ctx)?;
                let right_op = right.build_operators(ctx)?;
                let (left_key_indices, right_key_indices, filter) =
                    join::extract_hash_join_keys(condition, left.schema.columns.len())?;
                let hj_type = match join_type {
                    JoinType::Inner => HashJoinType::Inner,
                    JoinType::Left => HashJoinType::Left,
                    JoinType::Right => HashJoinType::Right,
                    JoinType::Full => HashJoinType::Full,
                    JoinType::Cross => HashJoinType::Inner,
                };
                Ok(Box::new(HashJoinOperator::new(
                    left_op,
                    right_op,
                    hj_type,
                    left_key_indices,
                    right_key_indices,
                    *left_is_build,
                    filter,
                    HashJoinConfig::default(),
                )))
            }

            PhysicalNode::SetOperation {
                op,
                all,
                left,
                right,
            } => {
                let left_op = left.build_operators(ctx)?;
                let right_op = right.build_operators(ctx)?;
                let op_type = match (op, all) {
                    (SetOpKind::Union, true) => SetOperationType::UnionAll,
                    (SetOpKind::Union, false) => SetOperationType::Union,
                    (SetOpKind::Intersect, true) => SetOperationType::IntersectAll,
                    (SetOpKind::Intersect, false) => SetOperationType::Intersect,
                    (SetOpKind::Except, true) => SetOperationType::ExceptAll,
                    (SetOpKind::Except, false) => SetOperationType::Except,
                };
                Ok(Box::new(SetOperationOperator::new(
                    left_op, right_op, op_type,
                )))
            }

            PhysicalNode::HashSemiJoin {
                left,
                right,
                anti,
                condition,
            } => {
                let left_op = left.build_operators(ctx)?;
                let right_op = right.build_operators(ctx)?;
                let (left_key_indices, right_key_indices, _no_residual) =
                    join::extract_hash_join_keys(condition, left.schema.columns.len())?;
                Ok(Box::new(HashSemiJoinOperator::new(
                    left_op,
                    right_op,
                    *anti,
                    left_key_indices,
                    right_key_indices,
                )))
            }

            // ── Correlated ──────────────────────────────────────
            PhysicalNode::Subquery { subplan, .. } => {
                // Recursively build the subquery plan.
                subplan.build_operators(ctx)
            }
        }
    }
}
