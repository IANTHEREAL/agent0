//! Subquery analysis helpers for the analyzed SELECT path.
//!
//! Provides correlation detection (`is_correlated_query`, `has_outer_ref`),
//! outer-reference substitution (`substitute_outer_refs_in_query`,
//! `substitute_outer_refs_in_expr`).

use crate::sql::analyzer::types::{
    AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind, JoinCondition,
    TypedExpr, TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::expr::traverse::{map_children, visit_any};
use crate::types::{Row, Value};

/// Check if an AnalyzedQuery references outer scope columns (correlated).
///
/// A correlated subquery has at least one `ColumnRef` with `scope_depth > 0`
/// somewhere in its WHERE, projection, or other clauses. Also detects
/// transitively-correlated nested subqueries (e.g., a scalar subquery inside
/// the WHERE clause that itself references the outer scope).
pub(super) fn is_correlated_query(query: &AnalyzedQuery) -> bool {
    query_has_outer_ref_beyond(query, 0)
}

/// Check if a TypedExpr has any outer reference (`scope_depth > 0`), including
/// transitively inside nested subquery expressions.
///
/// Uses [`visit_any`] for canonical traversal. When a subquery expression node
/// is encountered, descends into its `AnalyzedQuery` payload with an incremented
/// depth threshold to detect transitively-outer references (e.g., `scope_depth=2`
/// inside a nested subquery means the ref points beyond the current query).
pub(super) fn has_outer_ref(expr: &TypedExpr) -> bool {
    has_outer_ref_beyond(expr, 0)
}

/// Check if an expression has ColumnRefs with `scope_depth > min_depth`.
///
/// For nested expression-level subqueries, increments `min_depth` by 1 to
/// account for the additional scope boundary. A `scope_depth` of `min_depth + 1`
/// inside a nested subquery points to the current query's scope (not beyond),
/// so only `scope_depth > min_depth + 1` indicates a transitively-outer ref.
fn has_outer_ref_beyond(expr: &TypedExpr, min_depth: u32) -> bool {
    visit_any(expr, |e| match &e.kind {
        TypedExprKind::ColumnRef { scope_depth, .. } => *scope_depth > min_depth,
        TypedExprKind::ScalarSubquery(q) | TypedExprKind::ArraySubquery(q) => {
            query_has_outer_ref_beyond(q, min_depth + 1)
        }
        TypedExprKind::Exists { subquery, .. }
        | TypedExprKind::InSubquery { subquery, .. }
        | TypedExprKind::AnyAll { subquery, .. } => {
            query_has_outer_ref_beyond(subquery, min_depth + 1)
        }
        _ => false,
    })
}

/// Check if a query has any ColumnRef with `scope_depth > min_depth`,
/// including transitively inside nested subquery expressions.
fn query_has_outer_ref_beyond(query: &AnalyzedQuery, min_depth: u32) -> bool {
    let body_has = match &query.body {
        AnalyzedQueryBody::Select(select) => {
            if select
                .from
                .iter()
                .any(|tr| table_ref_has_outer_ref_beyond(tr, min_depth))
            {
                return true;
            }
            if let Some(ref w) = select.where_clause {
                if has_outer_ref_beyond(w, min_depth) {
                    return true;
                }
            }
            if select
                .projection
                .iter()
                .any(|p| has_outer_ref_beyond(&p.expr, min_depth))
            {
                return true;
            }
            if select
                .group_by
                .iter()
                .any(|e| has_outer_ref_beyond(e, min_depth))
            {
                return true;
            }
            if let Some(ref h) = select.having {
                if has_outer_ref_beyond(h, min_depth) {
                    return true;
                }
            }
            false
        }
        AnalyzedQueryBody::Values(rows) => rows
            .iter()
            .flatten()
            .any(|e| has_outer_ref_beyond(e, min_depth)),
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            query_has_outer_ref_beyond(left, min_depth)
                || query_has_outer_ref_beyond(right, min_depth)
        }
    };

    body_has
        || query
            .order_by
            .iter()
            .any(|o| has_outer_ref_beyond(&o.expr, min_depth))
}

/// Check if a table reference (or its nested joins) contains outer references
/// with `scope_depth > min_depth`.
fn table_ref_has_outer_ref_beyond(table_ref: &AnalyzedTableRef, min_depth: u32) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Subquery(query) => {
            // A derived-table subquery introduces a scope boundary, same as
            // expression-level subqueries. scope_depth == min_depth + 1 inside
            // the subquery refers to the enclosing query's scope (not beyond).
            query_has_outer_ref_beyond(query, min_depth + 1)
        }
        AnalyzedTableRefKind::Function { args, .. } => args.iter().any(|arg| match arg {
            TypedFunctionArg::Positional(expr) => has_outer_ref_beyond(expr, min_depth),
            TypedFunctionArg::Named { expr, .. } => has_outer_ref_beyond(expr, min_depth),
        }),
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            table_ref_has_outer_ref_beyond(left, min_depth)
                || table_ref_has_outer_ref_beyond(right, min_depth)
                || matches!(condition, JoinCondition::On(expr) if has_outer_ref_beyond(expr, min_depth))
        }
        _ => false,
    }
}
// ── Correlated subquery substitution ─────────────────────────

/// Substitute outer column references in an AnalyzedQuery with constant
/// values from the given row. After substitution, the query is no longer
/// correlated and can be executed as a standalone query.
///
/// Only handles scope_depth == 1 (the immediately enclosing scope).
/// Deeper nesting is left unchanged (decremented by 1).
pub(super) fn substitute_outer_refs_in_query(
    query: &AnalyzedQuery,
    outer_row: &Row,
) -> AnalyzedQuery {
    let body = match &query.body {
        AnalyzedQueryBody::Select(select) => {
            AnalyzedQueryBody::Select(substitute_outer_refs_in_select(select, outer_row))
        }
        AnalyzedQueryBody::Values(rows) => AnalyzedQueryBody::Values(
            rows.iter()
                .map(|row| {
                    row.iter()
                        .map(|e| substitute_outer_refs_in_expr(e, outer_row))
                        .collect()
                })
                .collect(),
        ),
        AnalyzedQueryBody::SetOperation {
            op,
            all,
            left,
            right,
        } => AnalyzedQueryBody::SetOperation {
            op: op.clone(),
            all: *all,
            left: Box::new(substitute_outer_refs_in_query(left, outer_row)),
            right: Box::new(substitute_outer_refs_in_query(right, outer_row)),
        },
    };
    AnalyzedQuery {
        ctes: query.ctes.clone(),
        body,
        order_by: query
            .order_by
            .iter()
            .map(|o| TypedOrderByExpr {
                expr: substitute_outer_refs_in_expr(&o.expr, outer_row),
                asc: o.asc,
                nulls_first: o.nulls_first,
            })
            .collect(),
        limit: query.limit.clone(),
        offset: query.offset.clone(),
        output_schema: query.output_schema.clone(),
    }
}

fn substitute_outer_refs_in_select(select: &AnalyzedSelect, outer_row: &Row) -> AnalyzedSelect {
    AnalyzedSelect {
        from: select
            .from
            .iter()
            .map(|t| substitute_outer_refs_in_table_ref(t, outer_row))
            .collect(),
        where_clause: select
            .where_clause
            .as_ref()
            .map(|w| substitute_outer_refs_in_expr(w, outer_row)),
        projection: select
            .projection
            .iter()
            .map(|p| crate::sql::analyzer::types::AnalyzedProjection {
                expr: substitute_outer_refs_in_expr(&p.expr, outer_row),
                output_name: p.output_name.clone(),
            })
            .collect(),
        group_by: select
            .group_by
            .iter()
            .map(|e| substitute_outer_refs_in_expr(e, outer_row))
            .collect(),
        having: select
            .having
            .as_ref()
            .map(|h| substitute_outer_refs_in_expr(h, outer_row)),
        distinct: select.distinct.clone(),
    }
}

fn substitute_outer_refs_in_table_ref(
    table_ref: &AnalyzedTableRef,
    outer_row: &Row,
) -> AnalyzedTableRef {
    let kind = match &table_ref.kind {
        AnalyzedTableRefKind::Subquery(query) => AnalyzedTableRefKind::Subquery(Box::new(
            substitute_outer_refs_in_query(query, outer_row),
        )),
        AnalyzedTableRefKind::Function {
            func,
            args,
            output_columns,
        } => {
            let args = args
                .iter()
                .map(|arg| match arg {
                    TypedFunctionArg::Positional(expr) => {
                        TypedFunctionArg::Positional(substitute_outer_refs_in_expr(expr, outer_row))
                    }
                    TypedFunctionArg::Named { name, expr } => TypedFunctionArg::Named {
                        name: name.clone(),
                        expr: substitute_outer_refs_in_expr(expr, outer_row),
                    },
                })
                .collect();
            AnalyzedTableRefKind::Function {
                func: func.clone(),
                args,
                output_columns: output_columns.clone(),
            }
        }
        AnalyzedTableRefKind::Join {
            join_type,
            left,
            right,
            condition,
            left_col_start,
        } => AnalyzedTableRefKind::Join {
            join_type: join_type.clone(),
            left: Box::new(substitute_outer_refs_in_table_ref(left, outer_row)),
            right: Box::new(substitute_outer_refs_in_table_ref(right, outer_row)),
            condition: match condition {
                JoinCondition::On(expr) => {
                    JoinCondition::On(substitute_outer_refs_in_expr(expr, outer_row))
                }
                other => other.clone(),
            },
            left_col_start: *left_col_start,
        },
        other => other.clone(),
    };
    AnalyzedTableRef {
        kind,
        alias: table_ref.alias.clone(),
    }
}

/// Substitute outer column references (scope_depth > 0) in a TypedExpr tree
/// with constant values from the outer row.
///
/// Uses [`map_children`] for canonical child recursion on non-subquery composite
/// variants. Subquery variants are handled explicitly because they must call
/// [`substitute_outer_refs_in_query`] to descend into `AnalyzedQuery` payloads.
///
/// **Fixes vs previous manual walker**: the old catch-all `_ => expr.clone()`
/// skipped recursion into SimilarTo, WindowCall, MinMax, Row, and ArrayLiteral.
/// Outer refs inside those variants are now correctly substituted.
pub(super) fn substitute_outer_refs_in_expr(expr: &TypedExpr, outer_row: &Row) -> TypedExpr {
    let kind = match &expr.kind {
        // ColumnRef: custom substitution logic
        TypedExprKind::ColumnRef {
            column_index,
            scope_depth,
            column_name,
        } => {
            if *scope_depth == 1 {
                // Direct outer reference → substitute with row value.
                let val = outer_row
                    .values
                    .get(*column_index)
                    .cloned()
                    .unwrap_or(Value::Null);
                return TypedExpr::new(TypedExprKind::Constant(val), expr.data_type.clone());
            } else if *scope_depth > 1 {
                // Deeper nesting → decrement scope_depth.
                return TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        column_index: *column_index,
                        scope_depth: scope_depth - 1,
                        column_name: column_name.clone(),
                    },
                    expr.data_type.clone(),
                );
            } else {
                return expr.clone();
            }
        }

        // 5 subquery variants: MUST keep explicit — they call substitute_outer_refs_in_query()
        // to descend into AnalyzedQuery payloads (map_children cannot do this).
        TypedExprKind::ScalarSubquery(subquery) => {
            if is_correlated_query(subquery) {
                let substituted = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExprKind::ScalarSubquery(Box::new(substituted))
            } else {
                return expr.clone();
            }
        }
        TypedExprKind::ArraySubquery(subquery) => {
            if is_correlated_query(subquery) {
                let substituted = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExprKind::ArraySubquery(Box::new(substituted))
            } else {
                return expr.clone();
            }
        }
        TypedExprKind::Exists { subquery, negated } => {
            if is_correlated_query(subquery) {
                let substituted = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExprKind::Exists {
                    subquery: Box::new(substituted),
                    negated: *negated,
                }
            } else {
                return expr.clone();
            }
        }
        TypedExprKind::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => {
            let inner_sub = substitute_outer_refs_in_expr(inner, outer_row);
            if is_correlated_query(subquery) {
                let sub = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExprKind::InSubquery {
                    expr: Box::new(inner_sub),
                    subquery: Box::new(sub),
                    negated: *negated,
                }
            } else {
                TypedExprKind::InSubquery {
                    expr: Box::new(inner_sub),
                    subquery: subquery.clone(),
                    negated: *negated,
                }
            }
        }
        TypedExprKind::AnyAll {
            expr: inner,
            op,
            subquery,
            is_all,
        } => {
            let inner_sub = substitute_outer_refs_in_expr(inner, outer_row);
            if is_correlated_query(subquery) {
                let sub = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExprKind::AnyAll {
                    expr: Box::new(inner_sub),
                    op: op.clone(),
                    subquery: Box::new(sub),
                    is_all: *is_all,
                }
            } else {
                TypedExprKind::AnyAll {
                    expr: Box::new(inner_sub),
                    op: op.clone(),
                    subquery: subquery.clone(),
                    is_all: *is_all,
                }
            }
        }

        // Everything else: canonical child recursion via map_children
        _ => map_children(expr, &mut |child| {
            substitute_outer_refs_in_expr(child, outer_row)
        }),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

#[cfg(test)]
mod tests;
