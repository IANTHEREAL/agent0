//! Post-processing helpers for the analyzed SELECT path.
//!
//! Contains functions that run *after* the operator tree produces rows:
//! - Row-level lock application (FOR UPDATE/SHARE)
//! - Deferred ORDER BY sorting on projected rows
//! - Async-expression helpers (extract dependency, build deferred expr)
//! - Passthrough projection helpers (source column collection, schema building)
//! - Nested query collection for CTE materialization
//! - AnyAll comparison type coercion helpers

use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind,
    JoinCondition, TypedExpr, TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::executor::core::Executor;
use crate::sql::expr::traverse::for_each_child;
use crate::sql::expr::typed_eval::eval_const_limit_bound;
use crate::sql::optimizer::BuildContext;
use crate::sql::types::coercion::comparison_target_type;
use crate::sql::types::CastContext;

use anyhow::{anyhow, Result};
use tikv_client::Transaction;

fn has_skip_locked_clause(locks: &[sqlparser::ast::LockClause]) -> bool {
    locks.iter().any(|l| {
        l.nonblock
            .as_ref()
            .is_some_and(|nb| *nb == sqlparser::ast::NonBlock::SkipLocked)
    })
}

impl Executor {
    /// Apply row-level locks (FOR UPDATE/SHARE) to rows.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::sql::executor::select::analyzed) async fn apply_row_locks(
        &self,
        rows: Vec<Row>,
        locks: &[sqlparser::ast::LockClause],
        analyzed: &AnalyzedQuery,
        build_ctx: &BuildContext,
        txn: &mut Transaction,
        db_id: u64,
        deferred_limit: &Option<(Option<TypedExpr>, Option<TypedExpr>)>,
        lock_timeout: Option<std::time::Duration>,
    ) -> Result<(Vec<Row>, bool)> {
        if locks.is_empty() {
            return Ok((rows, false));
        }
        let deferred_bounds_present = deferred_limit.is_some();

        // Extract lock properties.
        let has_skip_locked = has_skip_locked_clause(locks);
        let has_nowait = locks.iter().any(|l| {
            l.nonblock
                .as_ref()
                .is_some_and(|nb| *nb == sqlparser::ast::NonBlock::Nowait)
        });

        // Find the table name for locking. Use the first base table in FROM.
        let table_name = match &analyzed.body {
            AnalyzedQueryBody::Select(select) => {
                select.from.first().and_then(|tr| match &tr.kind {
                    AnalyzedTableRefKind::Table { name, .. } => Some(name.clone()),
                    _ => None,
                })
            }
            _ => None,
        };

        let Some(table_name) = table_name else {
            return Ok((rows, false));
        };

        // Check that the table has a primary key (required for locking).
        if let Some(schema) = build_ctx.table_schemas.get(&table_name) {
            if schema.pk_indices.is_empty() {
                return Err(anyhow!("FOR UPDATE/SHARE requires primary key"));
            }
        }

        if has_skip_locked {
            let limit = deferred_limit
                .as_ref()
                .and_then(|(l, _)| l.as_ref())
                .map(eval_const_limit_bound)
                .transpose()?
                .flatten();
            let offset = deferred_limit
                .as_ref()
                .and_then(|(_, o)| o.as_ref())
                .map(eval_const_limit_bound)
                .transpose()?
                .flatten()
                .unwrap_or(0);
            let max_locks = limit.map(|l| offset + l);
            let locked_indices = self
                .store()
                .lock_rows_skip_locked(txn, db_id, &table_name, &rows, max_locks)
                .await?;
            let locked_rows: Vec<Row> = locked_indices.iter().map(|&i| rows[i].clone()).collect();
            // Apply offset + limit to the locked subset.
            let start = offset.min(locked_rows.len());
            let end = limit.map_or(locked_rows.len(), |l| (start + l).min(locked_rows.len()));
            Ok((locked_rows[start..end].to_vec(), deferred_bounds_present))
        } else {
            // Apply deferred LIMIT/OFFSET before locking to avoid locking
            // more rows than needed. Without this, `FOR UPDATE LIMIT 1` would
            // lock ALL scanned rows, causing SKIP LOCKED in other sessions to
            // find no unlockable rows.
            let mut rows_to_lock = rows;
            if let Some((ref limit_expr, ref offset_expr)) = deferred_limit {
                let limit = limit_expr
                    .as_ref()
                    .map(eval_const_limit_bound)
                    .transpose()?
                    .flatten();
                let offset = offset_expr
                    .as_ref()
                    .map(eval_const_limit_bound)
                    .transpose()?
                    .flatten()
                    .unwrap_or(0);
                if limit.is_some() || offset > 0 {
                    let start = offset.min(rows_to_lock.len());
                    let end =
                        limit.map_or(rows_to_lock.len(), |l| (start + l).min(rows_to_lock.len()));
                    rows_to_lock = rows_to_lock[start..end].to_vec();
                }
            }
            if has_nowait {
                self.store()
                    .lock_rows_nowait(txn, db_id, &table_name, &rows_to_lock)
                    .await?;
            } else {
                self.store()
                    .lock_rows(txn, db_id, &table_name, &rows_to_lock, lock_timeout)
                    .await?;
            }
            Ok((rows_to_lock, deferred_bounds_present))
        }
    }
}

// ── Free functions ──────────────────────────────────────────────────

/// Extract the primary dependency (first ColumnRef argument) from an async
/// expression. Typically the async expression is a catalog-dependent function
/// call like `pg_get_indexdef(col_ref)` or `format_type(col_ref, const)`.
pub(super) fn extract_async_dependency(expr: &TypedExpr) -> Option<TypedExpr> {
    match &expr.kind {
        TypedExprKind::FunctionCall { args, .. } => {
            // Return the first ColumnRef argument.
            for arg in args {
                if matches!(arg.kind, TypedExprKind::ColumnRef { .. }) {
                    return Some(arg.clone());
                }
            }
            // If no ColumnRef found, recurse into first arg.
            args.first().and_then(extract_async_dependency)
        }
        _ => None,
    }
}

/// Build a deferred async expression where ColumnRef arguments are remapped
/// to reference the output column at `output_col_idx`. This allows the
/// deferred expression to be evaluated against the output row (where the
/// dependency value sits at position `output_col_idx`).
pub(super) fn build_deferred_async_expr(expr: &TypedExpr, output_col_idx: usize) -> TypedExpr {
    match &expr.kind {
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            let new_args: Vec<TypedExpr> = args
                .iter()
                .map(|arg| {
                    if matches!(arg.kind, TypedExprKind::ColumnRef { .. }) {
                        // Remap ColumnRef to output column index.
                        TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: output_col_idx,
                                column_name: match &arg.kind {
                                    TypedExprKind::ColumnRef { column_name, .. } => {
                                        column_name.clone()
                                    }
                                    _ => unreachable!(),
                                },
                            },
                            data_type: arg.data_type.clone(),
                        }
                    } else {
                        arg.clone()
                    }
                })
                .collect();
            TypedExpr {
                kind: TypedExprKind::FunctionCall {
                    func: func.clone(),
                    args: new_args,
                    order_by: order_by.clone(),
                    filter: filter.clone(),
                },
                data_type: expr.data_type.clone(),
            }
        }
        // For non-FunctionCall async expressions, return as-is (fallback).
        _ => expr.clone(),
    }
}

/// Collect all source columns from a SELECT's FROM clause table references.
///
/// Walks the FROM tree (handling joins, tables, functions) and collects
/// `(column_name, data_type)` pairs in the same order the analyzer assigns
/// column indices.
pub(super) fn collect_source_columns(
    select: &AnalyzedSelect,
) -> Vec<(
    String,
    DataType,
    Option<crate::sql::collation::ResolvedCollation>,
)> {
    let mut cols = Vec::new();
    for tr in &select.from {
        collect_table_ref_columns(tr, &mut cols);
    }
    cols
}

fn collect_table_ref_columns(
    tr: &AnalyzedTableRef,
    out: &mut Vec<(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )>,
) {
    match &tr.kind {
        AnalyzedTableRefKind::Table { schema, .. } => {
            for (name, dt, _nullable) in &schema.columns {
                out.push((name.clone(), dt.clone(), None));
            }
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            collect_table_ref_columns(left, out);
            collect_table_ref_columns(right, out);
        }
        AnalyzedTableRefKind::Function { output_columns, .. } => {
            for (name, dt) in output_columns {
                out.push((name.clone(), dt.clone(), None));
            }
        }
        AnalyzedTableRefKind::Subquery(subquery) => {
            for (name, dt, coll) in &subquery.output_schema {
                out.push((name.clone(), dt.clone(), coll.clone()));
            }
        }
    }
}

/// Create a passthrough projection that emits all source columns as ColumnRef.
pub(super) fn create_passthrough_projection(
    columns: &[(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )],
) -> Vec<crate::sql::analyzer::types::AnalyzedProjection> {
    columns
        .iter()
        .enumerate()
        .map(
            |(i, (name, dt, _coll))| crate::sql::analyzer::types::AnalyzedProjection {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: i,
                        column_name: name.clone(),
                    },
                    data_type: dt.clone(),
                },
                output_name: name.clone(),
            },
        )
        .collect()
}

/// Build a TableSchema from column name/type/collation triples.
pub(crate) fn build_schema_from_columns(
    name: &str,
    columns: &[(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )],
) -> TableSchema {
    TableSchema::new(
        name.to_string(),
        0,
        columns
            .iter()
            .map(|(col_name, dt, _coll)| crate::model::ColumnDef {
                name: col_name.clone(),
                data_type: dt.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            })
            .collect(),
        vec![],
    )
}

/// Build a TableSchema from the analyzed query's output schema.
pub(crate) fn build_output_schema(analyzed: &AnalyzedQuery) -> TableSchema {
    build_schema_from_columns("__output", &analyzed.output_schema)
}

/// Sort projected rows by deferred ORDER BY expressions.
///
/// When ORDER BY references output aliases (Analyzer clones projection expressions),
/// the ORDER BY expressions match output columns by name. We find the output column
/// index for each ORDER BY key, then sort using those column values.
pub(super) fn sort_projected_rows(
    mut rows: Vec<Row>,
    deferred_ob: &[TypedOrderByExpr],
    output_schema: &[(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )],
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<Row>> {
    // Map each ORDER BY expression to an output column index + collation.
    // Strategy: for ColumnRef ORDER BY, use column_name to find the output column.
    // For complex expressions (ScalarSubquery cloned from alias), find by data_type match.
    use crate::sql::collation::ResolvedCollation;
    use crate::sql::expr::collation_aware::extract_resolved_collation;

    let mut ob_col_indices: Vec<(usize, bool, bool, Option<ResolvedCollation>)> =
        Vec::with_capacity(deferred_ob.len());
    for ob in deferred_ob {
        let idx = match &ob.expr.kind {
            TypedExprKind::ColumnRef { column_name, .. } => {
                // Match by column name against output schema.
                output_schema
                    .iter()
                    .position(|(name, _, _)| name.eq_ignore_ascii_case(column_name))
            }
            _ => {
                // For complex expressions (ScalarSubquery, etc.): the Analyzer cloned
                // this from some projection[i].expr where output_schema[i] has the alias.
                // Find by matching data_type + being the only expression of that type.
                let target_type = &ob.expr.data_type;
                let matches: Vec<usize> = output_schema
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, dt, _))| dt == target_type)
                    .map(|(i, _)| i)
                    .collect();
                if matches.len() == 1 {
                    Some(matches[0])
                } else {
                    // Fallback: first output column.
                    Some(0)
                }
            }
        };
        let collation = extract_resolved_collation(&ob.expr);
        ob_col_indices.push((idx.unwrap_or(0), ob.asc, ob.nulls_first, collation));
    }

    // Sort.
    use crate::sql::expr::operators::sort_by_fallible;
    sort_by_fallible(&mut rows, |a, b| {
        for (col_idx, asc, nulls_first, ref collation) in &ob_col_indices {
            let va = a.values.get(*col_idx).unwrap_or(&Value::Null);
            let vb = b.values.get(*col_idx).unwrap_or(&Value::Null);
            let ord = crate::sql::expr::compare_order_by_values_collated(
                va,
                vb,
                *asc,
                *nulls_first,
                collation.as_ref(),
            )?;
            if ord != std::cmp::Ordering::Equal {
                return Ok(ord);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    })?;

    // Apply deferred LIMIT/OFFSET.
    let start = offset.min(rows.len());
    let end = limit.map_or(rows.len(), |l| (start + l).min(rows.len()));
    Ok(rows[start..end].to_vec())
}

/// Collect immediate nested analyzed queries (excluding descendants).
///
/// This provides one-level query children only. The caller performs DFS by
/// recursively materializing each child query, guaranteeing each subtree is
/// visited exactly once.
pub(super) fn collect_immediate_nested_analyzed_queries(
    query: &AnalyzedQuery,
) -> Vec<&AnalyzedQuery> {
    let mut out = Vec::new();
    collect_immediate_from_query_body(&query.body, &mut out);
    for ob in &query.order_by {
        collect_immediate_from_typed_expr(&ob.expr, &mut out);
    }
    if let Some(limit) = &query.limit {
        collect_immediate_from_typed_expr(limit, &mut out);
    }
    if let Some(offset) = &query.offset {
        collect_immediate_from_typed_expr(offset, &mut out);
    }
    out
}

fn collect_immediate_from_query_body<'a>(
    body: &'a AnalyzedQueryBody,
    out: &mut Vec<&'a AnalyzedQuery>,
) {
    match body {
        AnalyzedQueryBody::Select(select) => {
            for table_ref in &select.from {
                collect_immediate_from_table_ref(table_ref, out);
            }
            if let Some(where_clause) = &select.where_clause {
                collect_immediate_from_typed_expr(where_clause, out);
            }
            for proj in &select.projection {
                collect_immediate_from_typed_expr(&proj.expr, out);
            }
            for group_expr in &select.group_by {
                collect_immediate_from_typed_expr(group_expr, out);
            }
            if let Some(having) = &select.having {
                collect_immediate_from_typed_expr(having, out);
            }
            if let AnalyzedDistinct::DistinctOn(exprs) = &select.distinct {
                for expr in exprs {
                    collect_immediate_from_typed_expr(expr, out);
                }
            }
        }
        AnalyzedQueryBody::Values(rows) => {
            for row in rows {
                for expr in row {
                    collect_immediate_from_typed_expr(expr, out);
                }
            }
        }
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            out.push(left);
            out.push(right);
        }
    }
}

fn collect_immediate_from_table_ref<'a>(
    table_ref: &'a AnalyzedTableRef,
    out: &mut Vec<&'a AnalyzedQuery>,
) {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => {}
        AnalyzedTableRefKind::Subquery(subquery) => {
            out.push(subquery);
        }
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            collect_immediate_from_table_ref(left, out);
            collect_immediate_from_table_ref(right, out);
            if let JoinCondition::On(expr) = condition {
                collect_immediate_from_typed_expr(expr, out);
            }
        }
        AnalyzedTableRefKind::Function { args, .. } => {
            for arg in args {
                match arg {
                    TypedFunctionArg::Positional(expr) => {
                        collect_immediate_from_typed_expr(expr, out)
                    }
                    TypedFunctionArg::Named { expr, .. } => {
                        collect_immediate_from_typed_expr(expr, out)
                    }
                }
            }
        }
    }
}

fn collect_immediate_from_typed_expr<'a>(expr: &'a TypedExpr, out: &mut Vec<&'a AnalyzedQuery>) {
    match &expr.kind {
        TypedExprKind::ScalarSubquery(subquery)
        | TypedExprKind::ArraySubquery(subquery)
        | TypedExprKind::Exists { subquery, .. } => {
            out.push(subquery);
        }
        TypedExprKind::InSubquery {
            expr: lhs,
            subquery,
            ..
        }
        | TypedExprKind::AnyAll {
            expr: lhs,
            subquery,
            ..
        } => {
            collect_immediate_from_typed_expr(lhs, out);
            out.push(subquery);
        }
        TypedExprKind::TupleInSubquery {
            exprs, subquery, ..
        } => {
            for e in exprs {
                collect_immediate_from_typed_expr(e, out);
            }
            out.push(subquery);
        }
        _ => {
            for_each_child(expr, &mut |child| {
                collect_immediate_from_typed_expr(child, out)
            });
        }
    }
}

/// Build a typed RHS constant for `AnyAll` comparisons.
///
/// Runtime comparison rejects cross-type values, so we must preserve the
/// original Value type and insert explicit implicit-cast nodes when analyzer
/// comparison coercion requires a target type.
pub(super) fn build_any_all_rhs_constant_expr(
    lhs_type: &DataType,
    rhs_declared_type: &DataType,
    value: Value,
) -> TypedExpr {
    let rhs_type = value
        .data_type()
        .unwrap_or_else(|| rhs_declared_type.clone());
    let rhs = TypedExpr::new(TypedExprKind::Constant(value), rhs_type);
    coerce_any_all_rhs_for_comparison(lhs_type, rhs_declared_type, rhs)
}

fn coerce_any_all_rhs_for_comparison(
    lhs_type: &DataType,
    rhs_declared_type: &DataType,
    rhs: TypedExpr,
) -> TypedExpr {
    let Some(target_type) = comparison_target_type(lhs_type, rhs_declared_type) else {
        return rhs;
    };

    if rhs.data_type == target_type {
        rhs
    } else if rhs.is_null_constant() {
        TypedExpr::null(target_type)
    } else {
        TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(rhs),
                target_type: target_type.clone(),
                cast_context: CastContext::Implicit,
            },
            target_type,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::has_skip_locked_clause;
    use sqlparser::ast::{LockClause, LockType, NonBlock};

    #[test]
    fn has_skip_locked_clause_matches_share_and_update() {
        let share_skip = LockClause {
            lock_type: LockType::Share,
            of: None,
            nonblock: Some(NonBlock::SkipLocked),
        };
        let update_skip = LockClause {
            lock_type: LockType::Update,
            of: None,
            nonblock: Some(NonBlock::SkipLocked),
        };
        assert!(has_skip_locked_clause(&[share_skip]));
        assert!(has_skip_locked_clause(&[update_skip]));
    }

    #[test]
    fn has_skip_locked_clause_false_when_absent() {
        let share_plain = LockClause {
            lock_type: LockType::Share,
            of: None,
            nonblock: None,
        };
        let update_nowait = LockClause {
            lock_type: LockType::Update,
            of: None,
            nonblock: Some(NonBlock::Nowait),
        };
        assert!(!has_skip_locked_clause(&[share_plain]));
        assert!(!has_skip_locked_clause(&[update_nowait]));
    }
}
