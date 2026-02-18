//! Operator bridge: translates [`PhysicalPlan`] into executable [`BoxedOperator`] trees.
//!
//! This module is the final step in the CBO pipeline:
//! `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`
//!
//! The [`BuildContext`] carries pre-resolved table schemas so that operator
//! construction is a pure, synchronous tree walk (no async catalog lookups).

use std::collections::HashMap;

use anyhow::{anyhow, Result};

use super::physical_plan::{PhysicalNode, PhysicalPlan};
use crate::sql::analyzer::types::{
    JoinCondition, JoinType, SetOpKind, TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::operators::AggregateExpr;
use crate::sql::operators::{
    BoxedOperator, DistinctOnOperator, DistinctOperator, FilterOperator, HashAggregateOperator,
    HashJoinConfig, HashJoinOperator, HashJoinType, JoinType as OpJoinType, LimitOperator,
    NestedLoopJoinOperator, ProjectOperator, SetOperationOperator, SetOperationType, SortOperator,
    TableScanOperator,
};
use crate::sql::operators::{InListScanOperator, IndexScanOperator, RangeIndexScanOperator};
use crate::sql::planner::ScanType;
use crate::types::{DataType, Row, TableSchema, Value};

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
}

impl BuildContext {
    pub fn new() -> Self {
        Self {
            table_schemas: HashMap::new(),
            preloaded_rows: HashMap::new(),
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
                let key = super::schema_map_key(table_name, alias.as_deref());
                let schema = ctx
                    .table_schemas
                    .get(&key)
                    .ok_or_else(|| anyhow!("Table schema not found: {}", key))?;
                let mut schema = schema.clone();
                if let Some(a) = alias {
                    schema.from_alias = Some(a.clone());
                }
                // Use preloaded rows for virtual catalog tables, CTEs, etc.
                if let Some(rows) = ctx.preloaded_rows.get(&key) {
                    Ok(Box::new(TableScanOperator::new_with_rows(
                        schema,
                        rows.clone(),
                    )))
                } else {
                    Ok(Box::new(TableScanOperator::new(schema)))
                }
            }

            PhysicalNode::IndexScan {
                table_name,
                alias,
                scan_type,
            } => {
                let key = super::schema_map_key(table_name, alias.as_deref());
                let schema = ctx
                    .table_schemas
                    .get(&key)
                    .ok_or_else(|| anyhow!("Table schema not found: {}", key))?;
                let mut schema = schema.clone();
                if let Some(a) = alias {
                    schema.from_alias = Some(a.clone());
                }
                match scan_type {
                    ScanType::IndexScan {
                        index_id,
                        index_name,
                        values,
                        ..
                    } => Ok(Box::new(IndexScanOperator::new_with_scan_limit(
                        schema,
                        *index_id,
                        index_name.clone(),
                        values.clone(),
                        None,
                    ))),
                    ScanType::IndexRangeScan {
                        index_id,
                        index_name,
                        prefix_values,
                        ..
                    } => Ok(Box::new(RangeIndexScanOperator::new(
                        schema,
                        *index_id,
                        index_name.clone(),
                        prefix_values.clone(),
                        None,
                        true,
                        None,
                        true,
                    ))),
                    ScanType::IndexBoundedRangeScan {
                        index_id,
                        index_name,
                        prefix_values,
                        range_start,
                        start_inclusive,
                        range_end,
                        end_inclusive,
                        ..
                    } => Ok(Box::new(RangeIndexScanOperator::new(
                        schema,
                        *index_id,
                        index_name.clone(),
                        prefix_values.clone(),
                        range_start.clone(),
                        *start_inclusive,
                        range_end.clone(),
                        *end_inclusive,
                    ))),
                    ScanType::InListScan {
                        index_id,
                        index_name,
                        column_values,
                        ..
                    } => Ok(Box::new(InListScanOperator::new(
                        schema,
                        *index_id,
                        index_name.clone(),
                        column_values.clone(),
                    ))),
                    // FullTableScan and GinIndexScan should not appear in PhysicalNode::IndexScan —
                    // the physical planner only emits btree variants here.
                    other => Err(anyhow!(
                        "Unexpected ScanType {:?} in PhysicalNode::IndexScan for table '{}'",
                        other,
                        table_name
                    )),
                }
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
                build_hash_aggregate(child, group_by, projections)
            }

            PhysicalNode::StreamAggregate {
                group_by,
                projections,
                input,
            } => {
                // Phase 1: stream aggregate falls back to hash aggregate.
                let child = input.build_operators(ctx)?;
                build_hash_aggregate(child, group_by, projections)
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
                let child = input.build_operators(ctx)?;
                let limit_val = limit.as_ref().map(eval_const_usize).transpose()?;
                let offset_val = offset
                    .as_ref()
                    .map(eval_const_usize)
                    .transpose()?
                    .unwrap_or(0);
                Ok(Box::new(LimitOperator::new(child, limit_val, offset_val)))
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
                let op_join_type = convert_join_type(join_type);
                let cond = extract_on_condition(condition);
                Ok(Box::new(NestedLoopJoinOperator::new(
                    left_op,
                    right_op,
                    op_join_type,
                    cond,
                )))
            }

            PhysicalNode::HashJoin {
                left,
                right,
                join_type,
                condition,
                left_is_build,
            } => {
                let left_op = left.build_operators(ctx)?;
                let right_op = right.build_operators(ctx)?;
                let (left_key_indices, right_key_indices, filter) =
                    extract_hash_join_keys(condition, left.schema.columns.len())?;
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

            // ── Correlated ──────────────────────────────────────
            PhysicalNode::Subquery { subplan, .. } => {
                // Recursively build the subquery plan.
                subplan.build_operators(ctx)
            }
        }
    }
}

// ── Helper functions ────────────────────────────────────────────

/// Evaluate a TypedExpr that should be a non-negative constant integer (LIMIT/OFFSET).
fn eval_const_usize(expr: &TypedExpr) -> Result<usize> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Int32(n)) => {
            if *n < 0 {
                Err(anyhow!("LIMIT/OFFSET must not be negative"))
            } else {
                Ok(*n as usize)
            }
        }
        TypedExprKind::Constant(Value::Int64(n)) => {
            if *n < 0 {
                Err(anyhow!("LIMIT/OFFSET must not be negative"))
            } else {
                Ok(*n as usize)
            }
        }
        _ => Err(anyhow!(
            "Expected constant integer for LIMIT/OFFSET, got: {:?}",
            expr.kind
        )),
    }
}

/// Convert analyzer JoinType to operator JoinType.
fn convert_join_type(jt: &JoinType) -> OpJoinType {
    match jt {
        JoinType::Inner => OpJoinType::Inner,
        JoinType::Left => OpJoinType::Left,
        JoinType::Right => OpJoinType::Right,
        JoinType::Full => OpJoinType::Full,
        JoinType::Cross => OpJoinType::Cross,
    }
}

/// Extract the ON condition expression from a JoinCondition for NLJ.
fn extract_on_condition(condition: &JoinCondition) -> Option<TypedExpr> {
    match condition {
        JoinCondition::On(expr) => Some(expr.clone()),
        JoinCondition::Using(cols) => {
            // Build equality conjunction: col1_left = col1_right AND col2_left = col2_right ...
            let mut result: Option<TypedExpr> = None;
            for col in cols {
                let eq = TypedExpr {
                    kind: TypedExprKind::BinaryOp {
                        left: Box::new(TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.left_index,
                                column_name: col.name.clone(),
                            },
                            data_type: col.data_type.clone(),
                        }),
                        op: crate::sql::analyzer::types::BinaryOp::Eq,
                        right: Box::new(TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.right_index,
                                column_name: col.name.clone(),
                            },
                            data_type: col.data_type.clone(),
                        }),
                    },
                    data_type: DataType::Boolean,
                };
                result = Some(match result {
                    None => eq,
                    Some(prev) => TypedExpr {
                        kind: TypedExprKind::BinaryOp {
                            left: Box::new(prev),
                            op: crate::sql::analyzer::types::BinaryOp::And,
                            right: Box::new(eq),
                        },
                        data_type: DataType::Boolean,
                    },
                });
            }
            result
        }
        JoinCondition::None => None,
    }
}

/// Extract equi-join key indices from a JoinCondition for hash join.
///
/// Delegates to `join_keys::try_extract_equi_keys` for validated, rebased indices.
/// Returns (left_key_indices, right_key_indices, optional_residual_filter).
fn extract_hash_join_keys(
    condition: &JoinCondition,
    left_width: usize,
) -> Result<(Vec<usize>, Vec<usize>, Option<TypedExpr>)> {
    match super::join_keys::try_extract_equi_keys(condition, left_width) {
        Some((left_keys, right_keys)) => Ok((left_keys, right_keys, None)),
        None => match condition {
            JoinCondition::None => Ok((vec![], vec![], None)),
            _ => Err(anyhow!(
                "HashJoin requires equi-join keys but got non-equi condition"
            )),
        },
    }
}

/// Check whether an `AggregateExpr` matches the identity of an `AggregateCall`.
///
/// Compares all 6 identity fields: `func_name`, `distinct`, `arg`, `delimiter`,
/// `filter`, and `order_by`.  This is the single source of truth for aggregate
/// dedup (collection) and slot lookup (rewrite) to prevent drift.
pub(crate) fn aggregate_identity_matches(
    ae: &AggregateExpr,
    func: &crate::sql::analyzer::types::ResolvedFunction,
    args: &[TypedExpr],
    distinct: bool,
    filter: &Option<Box<TypedExpr>>,
    order_by: &[crate::sql::analyzer::types::TypedOrderByExpr],
) -> bool {
    // 1. func_name
    if ae.func_name != func.name {
        return false;
    }
    // 2. distinct
    if ae.distinct != distinct {
        return false;
    }
    // 3. arg (first argument)
    let arg_matches = match (&ae.arg, args.first()) {
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
        (Some(stored), Some(current)) => format!("{}", stored) == format!("{}", current),
    };
    if !arg_matches {
        return false;
    }
    // 4. delimiter (string_agg second argument)
    let call_delimiter = if func.name.eq_ignore_ascii_case("string_agg") {
        args.get(1).and_then(|a| {
            if let TypedExprKind::Constant(Value::Text(s)) = &a.kind {
                Some(s.clone())
            } else {
                None
            }
        })
    } else {
        None
    };
    if ae.delimiter != call_delimiter {
        return false;
    }
    // 5. filter
    let filter_matches = match (&ae.filter, filter) {
        (None, None) => true,
        (Some(stored), Some(current)) => format!("{}", stored) == format!("{}", current),
        _ => false,
    };
    if !filter_matches {
        return false;
    }
    // 6. order_by
    if ae.order_by.len() != order_by.len() {
        return false;
    }
    for (stored, current) in ae.order_by.iter().zip(order_by.iter()) {
        if format!("{}", stored.expr) != format!("{}", current.expr)
            || stored.asc != current.asc
            || stored.nulls_first != current.nulls_first
        {
            return false;
        }
    }
    true
}

/// Build a HashAggregateOperator from group-by and projection lists.
///
/// Extracts aggregate function calls from projections and separates them from
/// group-by column references.  When projections contain expressions wrapping
/// aggregates (e.g. `COUNT(*) + 1`), a post-aggregate `ProjectOperator` is
/// added to evaluate those expressions against the aggregate output.
fn build_hash_aggregate(
    child: BoxedOperator,
    group_by: &[TypedExpr],
    projections: &[crate::sql::analyzer::types::AnalyzedProjection],
) -> Result<BoxedOperator> {
    // Build group-by names and types.
    let group_by_count = group_by.len();
    let group_by_names: Vec<String> = group_by
        .iter()
        .enumerate()
        .map(|(i, gb)| match &gb.kind {
            TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
            _ => format!("group_by_{}", i),
        })
        .collect();
    let group_by_types: Vec<DataType> = group_by.iter().map(|gb| gb.data_type.clone()).collect();

    // Extract unique aggregate expressions from projections.
    // Track each aggregate's position in the operator output (after group-by columns).
    let mut aggregate_exprs = Vec::new();
    let mut aggregate_names = Vec::new();
    let mut aggregate_types = Vec::new();

    for proj in projections {
        collect_agg_exprs_from(
            &proj.expr,
            &proj.output_name,
            &mut aggregate_exprs,
            &mut aggregate_names,
            &mut aggregate_types,
        );
    }

    let agg_op: BoxedOperator = Box::new(HashAggregateOperator::new(
        child,
        group_by.to_vec(),
        aggregate_exprs.clone(),
        group_by_names.clone(),
        group_by_types.clone(),
        aggregate_names,
        aggregate_types,
    ));

    // Check if any projection wraps an aggregate in an expression (e.g. COUNT(*) + 1).
    // If so, we need a post-aggregate Project to evaluate those expressions.
    let needs_post_projection = projections.iter().any(|p| {
        !matches!(p.expr.kind, TypedExprKind::AggregateCall { .. }) && contains_aggregate(&p.expr)
    });

    // Also check for group-by-only projections mixed with aggregates — these need
    // rewriting too since the aggregate operator output schema differs from the
    // original table schema.
    let has_group_by_refs =
        group_by_count > 0 && projections.iter().any(|p| !contains_aggregate(&p.expr));

    // After dedup, the aggregate output width (group_by + unique aggregates) may be
    // narrower than the projection list when the same aggregate appears more than
    // once (e.g. SELECT SUM(a), SUM(a)).  A post-projection is needed to duplicate
    // the column so the output schema matches the analyzed projection count.
    let has_duplicate_agg_refs = projections.len() != group_by_count + aggregate_exprs.len();

    if !needs_post_projection && !has_group_by_refs && !has_duplicate_agg_refs {
        return Ok(agg_op);
    }

    // Build rewritten projection expressions.  In the aggregate operator output:
    //   columns 0..group_by_count → group-by key values
    //   columns group_by_count..  → aggregate result values
    let rewritten: Vec<TypedExpr> = projections
        .iter()
        .map(|p| rewrite_post_aggregate_expr(&p.expr, group_by, group_by_count, &aggregate_exprs))
        .collect::<Result<Vec<_>>>()?;
    let output_names: Vec<String> = projections.iter().map(|p| p.output_name.clone()).collect();
    let output_types: Vec<DataType> = projections
        .iter()
        .map(|p| p.expr.data_type.clone())
        .collect();

    Ok(Box::new(ProjectOperator::new(
        agg_op,
        rewritten,
        output_names,
        output_types,
    )))
}

/// Check if a TypedExpr contains any AggregateCall.
fn contains_aggregate(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::AggregateCall { .. } => true,
        TypedExprKind::BinaryOp { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        TypedExprKind::UnaryOp { operand, .. } => contains_aggregate(operand),
        TypedExprKind::Cast { expr: inner, .. } => contains_aggregate(inner),
        TypedExprKind::FunctionCall { args, .. } => args.iter().any(contains_aggregate),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().map_or(false, |o| contains_aggregate(o))
                || when_clauses
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result
                    .as_ref()
                    .map_or(false, |e| contains_aggregate(e))
        }
        TypedExprKind::Coalesce(args) => args.iter().any(contains_aggregate),
        TypedExprKind::NullIf(a, b) => contains_aggregate(a) || contains_aggregate(b),
        TypedExprKind::MinMax { args, .. } => args.iter().any(contains_aggregate),
        _ => false,
    }
}

/// Rewrite a projection expression for post-aggregate evaluation.
///
/// - `AggregateCall` → `ColumnRef` at `group_by_count + agg_index`
/// - `ColumnRef` matching a GROUP BY expression → `ColumnRef` at `group_by_index`
/// - Everything else → recurse into children
///
/// Returns `Err` if an aggregate call cannot be matched to an extracted slot,
/// rather than silently falling back to slot 0 (which would produce wrong results).
pub(crate) fn rewrite_post_aggregate_expr(
    expr: &TypedExpr,
    group_by: &[TypedExpr],
    group_by_count: usize,
    aggregate_exprs: &[AggregateExpr],
) -> Result<TypedExpr> {
    // Check if this expression matches a GROUP BY key.
    if let Some(gb_idx) = find_matching_group_by(expr, group_by) {
        return Ok(TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: gb_idx,
                column_name: match &group_by[gb_idx].kind {
                    TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
                    _ => format!("group_by_{}", gb_idx),
                },
            },
            data_type: expr.data_type.clone(),
        });
    }

    match &expr.kind {
        // Replace aggregate call with a ColumnRef pointing to its output position.
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            filter,
            order_by,
        } => {
            // Find this aggregate in the extracted list using full 6-field identity match.
            let agg_idx = aggregate_exprs
                .iter()
                .position(|ae| {
                    aggregate_identity_matches(ae, func, args, *distinct, filter, order_by)
                })
                .ok_or_else(|| {
                    anyhow!(
                        "aggregate rewrite: no matching slot for {}({})",
                        func.name,
                        args.first()
                            .map(|a| format!("{}", a))
                            .unwrap_or_else(|| "*".to_string())
                    )
                })?;

            Ok(TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: group_by_count + agg_idx,
                    column_name: func.name.clone(),
                },
                data_type: expr.data_type.clone(),
            })
        }
        // Recurse into wrapping expressions.
        TypedExprKind::BinaryOp { left, op, right } => Ok(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(rewrite_post_aggregate_expr(
                    left,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                op: op.clone(),
                right: Box::new(rewrite_post_aggregate_expr(
                    right,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::UnaryOp { op, operand } => Ok(TypedExpr {
            kind: TypedExprKind::UnaryOp {
                op: op.clone(),
                operand: Box::new(rewrite_post_aggregate_expr(
                    operand,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => Ok(TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                target_type: target_type.clone(),
                cast_context: *cast_context,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            let rewritten_args: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::FunctionCall {
                    func: func.clone(),
                    args: rewritten_args,
                    order_by: order_by.clone(),
                    filter: filter.clone(),
                },
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            let rewritten_operand = operand
                .as_ref()
                .map(|o| {
                    rewrite_post_aggregate_expr(o, group_by, group_by_count, aggregate_exprs)
                        .map(Box::new)
                })
                .transpose()?;
            let rewritten_whens: Vec<(TypedExpr, TypedExpr)> = when_clauses
                .iter()
                .map(|(w, t)| {
                    Ok((
                        rewrite_post_aggregate_expr(w, group_by, group_by_count, aggregate_exprs)?,
                        rewrite_post_aggregate_expr(t, group_by, group_by_count, aggregate_exprs)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let rewritten_else = else_result
                .as_ref()
                .map(|e| {
                    rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                        .map(Box::new)
                })
                .transpose()?;
            Ok(TypedExpr {
                kind: TypedExprKind::Case {
                    operand: rewritten_operand,
                    when_clauses: rewritten_whens,
                    else_result: rewritten_else,
                },
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::Coalesce(args) => {
            let rewritten: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::Coalesce(rewritten),
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::NullIf(a, b) => Ok(TypedExpr {
            kind: TypedExprKind::NullIf(
                Box::new(rewrite_post_aggregate_expr(
                    a,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                Box::new(rewrite_post_aggregate_expr(
                    b,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            ),
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::MinMax { args, is_greatest } => {
            let rewritten: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::MinMax {
                    args: rewritten,
                    is_greatest: *is_greatest,
                },
                data_type: expr.data_type.clone(),
            })
        }
        // WindowCall: preserve the wrapper but recurse into children (args,
        // partition_by, order_by) to rewrite any aggregate/group-by references.
        // This handles mixed expressions like `LAG(COUNT(*)) OVER (ORDER BY dept)`.
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => {
            let rewritten_args: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            let rewritten_partition: Vec<TypedExpr> = partition_by
                .iter()
                .map(|e| rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            let rewritten_order: Vec<TypedOrderByExpr> = order_by
                .iter()
                .map(|ob| {
                    Ok(TypedOrderByExpr {
                        expr: rewrite_post_aggregate_expr(
                            &ob.expr,
                            group_by,
                            group_by_count,
                            aggregate_exprs,
                        )?,
                        asc: ob.asc,
                        nulls_first: ob.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::WindowCall {
                    func: func.clone(),
                    args: rewritten_args,
                    partition_by: rewritten_partition,
                    order_by: rewritten_order,
                    window_frame: window_frame.clone(),
                },
                data_type: expr.data_type.clone(),
            })
        }
        // Leaf nodes (constants, etc.) pass through unchanged.
        _ => Ok(expr.clone()),
    }
}

/// Find the index of a GROUP BY expression that matches `expr`.
pub(crate) fn find_matching_group_by(expr: &TypedExpr, group_by: &[TypedExpr]) -> Option<usize> {
    // Fast path: ColumnRef-to-ColumnRef matching by column_index + name
    for (i, gb) in group_by.iter().enumerate() {
        if let (
            TypedExprKind::ColumnRef {
                column_index: ei,
                column_name: en,
                ..
            },
            TypedExprKind::ColumnRef {
                column_index: gi,
                column_name: gn,
                ..
            },
        ) = (&expr.kind, &gb.kind)
        {
            if ei == gi && en == gn {
                return Some(i);
            }
        }
    }
    // Slow path: structural comparison via Display for expression GROUP BY keys
    let expr_display = format!("{}", expr);
    for (i, gb) in group_by.iter().enumerate() {
        if format!("{}", gb) == expr_display && expr.data_type == gb.data_type {
            return Some(i);
        }
    }
    None
}

/// Recursively collect AggregateCall nodes from a TypedExpr.
pub(crate) fn collect_agg_exprs_from(
    expr: &TypedExpr,
    output_name: &str,
    agg_exprs: &mut Vec<AggregateExpr>,
    agg_names: &mut Vec<String>,
    agg_types: &mut Vec<DataType>,
) {
    match &expr.kind {
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            filter,
            order_by,
        } => {
            // Dedup: only add if no existing entry matches on all 6 identity fields.
            let already_exists = agg_exprs
                .iter()
                .any(|ae| aggregate_identity_matches(ae, func, args, *distinct, filter, order_by));
            if !already_exists {
                let arg = args.first().cloned();
                let delimiter = if func.name.eq_ignore_ascii_case("string_agg") {
                    args.get(1).and_then(|a| {
                        if let TypedExprKind::Constant(Value::Text(s)) = &a.kind {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };
                agg_exprs.push(AggregateExpr {
                    func_name: func.name.clone(),
                    arg,
                    distinct: *distinct,
                    delimiter,
                    filter: filter.as_deref().cloned(),
                    order_by: order_by.clone(),
                });
                agg_names.push(output_name.to_string());
                agg_types.push(expr.data_type.clone());
            }
        }
        // Recurse into sub-expressions (e.g., CAST(COUNT(*) AS int))
        TypedExprKind::BinaryOp { left, right, .. } => {
            collect_agg_exprs_from(left, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(right, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::UnaryOp { operand, .. } => {
            collect_agg_exprs_from(operand, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::Cast { expr: inner, .. } => {
            collect_agg_exprs_from(inner, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_agg_exprs_from(op, output_name, agg_exprs, agg_names, agg_types);
            }
            for (w, t) in when_clauses {
                collect_agg_exprs_from(w, output_name, agg_exprs, agg_names, agg_types);
                collect_agg_exprs_from(t, output_name, agg_exprs, agg_names, agg_types);
            }
            if let Some(e) = else_result {
                collect_agg_exprs_from(e, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::Coalesce(args) => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::NullIf(a, b) => {
            collect_agg_exprs_from(a, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(b, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::MinMax { args, .. } => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        // WindowCall: recurse into children to find aggregate sub-expressions.
        // Handles cases like `ROW_NUMBER() OVER (ORDER BY COUNT(*))` and
        // `LAG(COUNT(*)) OVER (...)`.
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
            for e in partition_by {
                collect_agg_exprs_from(e, output_name, agg_exprs, agg_names, agg_types);
            }
            for ob in order_by {
                collect_agg_exprs_from(&ob.expr, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{
        AnalyzedProjection, BinaryOp as TypedBinaryOp, SetOpKind, TypedExpr, TypedExprKind,
        TypedOrderByExpr,
    };
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::optimizer::physical_plan::{PhysicalCost, PhysicalNode, PhysicalPlan};
    use crate::types::{ColumnDef, DataType, Value};

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
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
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
                },
                ColumnDef {
                    name: "val".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
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

    use crate::sql::analyzer::types::{FunctionKind, ResolvedFunction};

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
                },
                ColumnDef {
                    name: "val".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
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
            }],
            vec![0],
        );
        let child: BoxedOperator = Box::new(TableScanOperator::new(schema));

        let op = build_hash_aggregate(child, &group_by, &projections).unwrap();
        // Must be wrapped in a Project to duplicate the single aggregate slot into 2 columns.
        assert_eq!(op.name(), "Project");
        assert_eq!(op.schema().columns.len(), 2);
    }
}
