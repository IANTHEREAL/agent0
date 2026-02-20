//! Subquery analysis helpers for the analyzed SELECT path.

use crate::sql::analyzer::types::{
    AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind, BinaryOp,
    JoinCondition, TypedExpr, TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::expr::classify::has_unresolved_subquery;
use crate::sql::expr::traverse::{map_children, visit_any};
use crate::types::{DataType, Row, Value};

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

// ── WHERE clause splitting for async subquery handling ────────

/// Split a WHERE clause (AND-conjunction) into sync and async parts.
///
/// Returns `(sync_part, async_part)` where:
/// - sync_part: conjuncts without subqueries, safe for FilterOperator
/// - async_part: conjuncts with unresolved subqueries, needs per-row async evaluation
pub(super) fn split_where_for_async(expr: &TypedExpr) -> (Option<TypedExpr>, Option<TypedExpr>) {
    let mut sync_parts = Vec::new();
    let mut async_parts = Vec::new();
    flatten_and(expr, &mut sync_parts, &mut async_parts);

    let sync_expr = combine_and(sync_parts);
    let async_expr = combine_and(async_parts);
    (sync_expr, async_expr)
}

/// Flatten top-level AND conjuncts, classifying each as sync or async.
fn flatten_and<'a>(
    expr: &'a TypedExpr,
    sync_parts: &mut Vec<&'a TypedExpr>,
    async_parts: &mut Vec<&'a TypedExpr>,
) {
    if let TypedExprKind::BinaryOp {
        left,
        right,
        op: BinaryOp::And,
    } = &expr.kind
    {
        flatten_and(left, sync_parts, async_parts);
        flatten_and(right, sync_parts, async_parts);
    } else if has_unresolved_subquery(expr) {
        async_parts.push(expr);
    } else {
        sync_parts.push(expr);
    }
}

/// Combine a list of expressions into an AND chain.
fn combine_and(parts: Vec<&TypedExpr>) -> Option<TypedExpr> {
    parts.into_iter().cloned().reduce(|a, b| {
        TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(a),
                op: BinaryOp::And,
                right: Box::new(b),
            },
            DataType::Boolean,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{
        AnalyzedDistinct, AnalyzedProjection, AnalyzedQueryBody, AnalyzedTableRef,
        AnalyzedTableRefKind, BinaryOp, FunctionKind, ResolvedFunction, TypedFunctionArg,
    };

    fn int_const(v: i32) -> TypedExpr {
        TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
    }

    fn scalar_values_query(v: i32) -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Values(vec![vec![int_const(v)]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32)],
        }
    }

    #[test]
    fn has_outer_ref_recurses_into_any_all_lhs() {
        let expr = TypedExpr::new(
            TypedExprKind::AnyAll {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "x".to_string(),
                    },
                    DataType::Int32,
                )),
                op: BinaryOp::Eq,
                subquery: Box::new(scalar_values_query(1)),
                is_all: false,
            },
            DataType::Boolean,
        );

        assert!(has_outer_ref(&expr));
    }

    #[test]
    fn is_correlated_query_detects_outer_ref_inside_any_all() {
        let where_expr = TypedExpr::new(
            TypedExprKind::AnyAll {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "outer_x".to_string(),
                    },
                    DataType::Int32,
                )),
                op: BinaryOp::Eq,
                subquery: Box::new(scalar_values_query(1)),
                is_all: false,
            },
            DataType::Boolean,
        );

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![],
                where_clause: Some(where_expr),
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        assert!(is_correlated_query(&query));
    }

    #[test]
    fn is_correlated_query_detects_outer_ref_in_table_function_args() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Function {
                        func: ResolvedFunction {
                            name: "generate_series".to_string(),
                            kind: FunctionKind::Builtin,
                            return_type: DataType::Int32,
                        },
                        args: vec![
                            TypedFunctionArg::Positional(int_const(1)),
                            TypedFunctionArg::Positional(TypedExpr::new(
                                TypedExprKind::ColumnRef {
                                    scope_depth: 1,
                                    column_index: 0,
                                    column_name: "outer_n".to_string(),
                                },
                                DataType::Int32,
                            )),
                        ],
                        output_columns: vec![("generate_series".to_string(), DataType::Int32)],
                    },
                    alias: Some("gs".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        assert!(is_correlated_query(&query));
    }

    fn correlated_values_query() -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 1,
                            column_index: 0,
                            column_name: "outer_x".to_string(),
                        },
                        DataType::Int32,
                    ),
                    output_name: "outer_x".to_string(),
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
            output_schema: vec![("outer_x".to_string(), DataType::Int32)],
        }
    }

    #[test]
    fn is_correlated_query_detects_scalar_subquery_correlation() {
        assert!(is_correlated_query(&correlated_values_query()));
    }

    #[test]
    fn is_correlated_query_does_not_treat_nested_correlation_as_outer_ref() {
        let expr = TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(correlated_values_query())),
            DataType::Int32,
        );
        let where_expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(expr),
                op: BinaryOp::Eq,
                right: Box::new(int_const(1)),
            },
            DataType::Boolean,
        );

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![],
                where_clause: Some(where_expr),
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        assert!(!is_correlated_query(&query));
    }

    #[test]
    fn is_correlated_query_detects_nested_ref_beyond_current_scope() {
        let nested = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 2,
                            column_index: 0,
                            column_name: "grand_outer".to_string(),
                        },
                        DataType::Int32,
                    ),
                    output_name: "grand_outer".to_string(),
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
            output_schema: vec![("grand_outer".to_string(), DataType::Int32)],
        };

        let expr = TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(nested)),
            DataType::Int32,
        );

        let where_expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Boolean(false)),
                    DataType::Boolean,
                )),
                op: BinaryOp::Or,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(expr),
                        op: BinaryOp::Eq,
                        right: Box::new(int_const(1)),
                    },
                    DataType::Boolean,
                )),
            },
            DataType::Boolean,
        );

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![],
                where_clause: Some(where_expr),
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        assert!(is_correlated_query(&query));
    }

    #[test]
    fn substitute_outer_refs_in_query_rewrites_table_function_args() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Function {
                        func: ResolvedFunction {
                            name: "generate_series".to_string(),
                            kind: FunctionKind::Builtin,
                            return_type: DataType::Int32,
                        },
                        args: vec![
                            TypedFunctionArg::Positional(int_const(1)),
                            TypedFunctionArg::Positional(TypedExpr::new(
                                TypedExprKind::ColumnRef {
                                    scope_depth: 1,
                                    column_index: 1,
                                    column_name: "outer_n".to_string(),
                                },
                                DataType::Int32,
                            )),
                        ],
                        output_columns: vec![("generate_series".to_string(), DataType::Int32)],
                    },
                    alias: Some("gs".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };
        let outer_row = Row::new(vec![Value::Int32(7), Value::Int32(5)]);

        let substituted = substitute_outer_refs_in_query(&query, &outer_row);
        let AnalyzedQueryBody::Select(select) = substituted.body else {
            panic!("expected select body");
        };
        let Some(AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Function { args, .. },
            ..
        }) = select.from.first()
        else {
            panic!("expected function table ref");
        };

        let TypedFunctionArg::Positional(expr) = &args[1] else {
            panic!("expected positional argument");
        };
        assert!(matches!(
            expr.kind,
            TypedExprKind::Constant(Value::Int32(5))
        ));
    }

    // ── Semantic change regression tests ──────────────────────────

    #[test]
    fn has_outer_ref_detects_outer_ref_in_like_escape() {
        // Previously missed: escape field in Like was not traversed
        let expr = TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(int_const(1)),
                pattern: Box::new(int_const(2)),
                escape: Some(Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "esc_col".to_string(),
                    },
                    DataType::Text,
                ))),
                case_insensitive: false,
                negated: false,
            },
            DataType::Boolean,
        );
        assert!(has_outer_ref(&expr));
    }

    #[test]
    fn has_outer_ref_detects_outer_ref_in_function_order_by() {
        // Previously missed: order_by exprs in FunctionCall
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "array_agg".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int32,
                },
                args: vec![int_const(1)],
                order_by: vec![TypedOrderByExpr {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 1,
                            column_index: 0,
                            column_name: "sort_col".to_string(),
                        },
                        DataType::Int32,
                    ),
                    asc: true,
                    nulls_first: false,
                }],
                filter: None,
            },
            DataType::Int32,
        );
        assert!(has_outer_ref(&expr));
    }

    #[test]
    fn has_outer_ref_detects_outer_ref_in_json_path() {
        // Previously missed: path field in JsonAccess
        let expr = TypedExpr::new(
            TypedExprKind::JsonAccess {
                expr: Box::new(int_const(1)),
                path: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "path_col".to_string(),
                    },
                    DataType::Text,
                )),
                operator: crate::sql::analyzer::types::JsonAccessOp::Arrow,
            },
            DataType::Text,
        );
        assert!(has_outer_ref(&expr));
    }

    #[test]
    fn substitute_outer_refs_recurses_into_similar_to() {
        // Previously the catch-all `_ => expr.clone()` skipped SimilarTo
        let expr = TypedExpr::new(
            TypedExprKind::SimilarTo {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "outer_col".to_string(),
                    },
                    DataType::Text,
                )),
                pattern: Box::new(int_const(1)),
                escape: None,
                negated: false,
            },
            DataType::Boolean,
        );
        let outer_row = Row::new(vec![Value::Text("hello".to_string())]);
        let result = substitute_outer_refs_in_expr(&expr, &outer_row);
        if let TypedExprKind::SimilarTo { expr: inner, .. } = &result.kind {
            assert!(matches!(
                inner.kind,
                TypedExprKind::Constant(Value::Text(_))
            ));
        } else {
            panic!("expected SimilarTo, got {:?}", result.kind);
        }
    }

    #[test]
    fn substitute_outer_refs_recurses_into_min_max() {
        // Previously the catch-all `_ => expr.clone()` skipped MinMax
        let expr = TypedExpr::new(
            TypedExprKind::MinMax {
                args: vec![
                    TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 1,
                            column_index: 0,
                            column_name: "a".to_string(),
                        },
                        DataType::Int32,
                    ),
                    int_const(5),
                ],
                is_greatest: true,
            },
            DataType::Int32,
        );
        let outer_row = Row::new(vec![Value::Int32(10)]);
        let result = substitute_outer_refs_in_expr(&expr, &outer_row);
        if let TypedExprKind::MinMax { args, .. } = &result.kind {
            assert!(matches!(
                args[0].kind,
                TypedExprKind::Constant(Value::Int32(10))
            ));
        } else {
            panic!("expected MinMax, got {:?}", result.kind);
        }
    }

    /// Regression test for derived-subquery scope boundary.
    ///
    /// A FROM subquery (derived table) with scope_depth=1 refs points to its
    /// enclosing query's scope — NOT beyond. The enclosing query itself should
    /// NOT be classified as correlated just because its FROM subquery references
    /// the enclosing scope.
    ///
    /// Example: `SELECT * FROM (SELECT t1.x FROM t2) AS sub`
    ///   - Inside the derived subquery, `t1.x` has scope_depth=1 (one scope up)
    ///   - This makes the derived subquery correlated with its parent
    ///   - But the parent query is NOT correlated with any outer scope
    #[test]
    fn derived_subquery_scope_depth_1_does_not_make_parent_correlated() {
        // Build: SELECT 1 FROM (SELECT outer_col FROM t2) AS sub
        // where outer_col has scope_depth=1 inside the derived subquery
        let derived_subquery = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 1, // references enclosing query's scope
                            column_index: 0,
                            column_name: "outer_col".to_string(),
                        },
                        DataType::Int32,
                    ),
                    output_name: "outer_col".to_string(),
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
            output_schema: vec![("outer_col".to_string(), DataType::Int32)],
        };

        let parent_query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(derived_subquery)),
                    alias: Some("sub".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        // The parent query should NOT be classified as correlated — the derived
        // subquery's scope_depth=1 ref points to the parent's own scope, not beyond.
        assert!(
            !is_correlated_query(&parent_query),
            "parent query should not be correlated: derived subquery's scope_depth=1 \
             refs point to the parent scope, not beyond"
        );
    }

    /// Derived subquery with scope_depth=2 DOES make the parent correlated
    /// (the ref points beyond the parent to a grandparent scope).
    #[test]
    fn derived_subquery_scope_depth_2_makes_parent_correlated() {
        let derived_subquery = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 2, // references grandparent scope (beyond parent)
                            column_index: 0,
                            column_name: "grandparent_col".to_string(),
                        },
                        DataType::Int32,
                    ),
                    output_name: "gp".to_string(),
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
            output_schema: vec![("gp".to_string(), DataType::Int32)],
        };

        let parent_query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: int_const(1),
                    output_name: "?column?".to_string(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(derived_subquery)),
                    alias: Some("sub".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        assert!(
            is_correlated_query(&parent_query),
            "parent query should be correlated: derived subquery's scope_depth=2 \
             ref points beyond the parent scope"
        );
    }
}
