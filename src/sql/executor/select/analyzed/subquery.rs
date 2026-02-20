//! Subquery analysis helpers for the analyzed SELECT path.

use crate::sql::analyzer::types::{
    AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind, BinaryOp,
    JoinCondition, TypedExpr, TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::expr::classify::has_unresolved_subquery;
use crate::types::{DataType, Row, Value};

/// Check if an AnalyzedQuery references outer scope columns (correlated).
///
/// A correlated subquery has at least one `ColumnRef` with `scope_depth > 0`
/// somewhere in its WHERE, projection, or other clauses.
pub(super) fn is_correlated_query(query: &AnalyzedQuery) -> bool {
    let body_has_outer_ref = match &query.body {
        AnalyzedQueryBody::Select(select) => {
            // Check FROM clause (JOIN ON conditions may contain outer refs).
            if select.from.iter().any(table_ref_has_outer_ref) {
                return true;
            }
            // Check WHERE.
            if let Some(ref w) = select.where_clause {
                if has_outer_ref(w) {
                    return true;
                }
            }
            // Check projection.
            if select.projection.iter().any(|p| has_outer_ref(&p.expr)) {
                return true;
            }
            // Check GROUP BY.
            if select.group_by.iter().any(has_outer_ref) {
                return true;
            }
            // Check HAVING.
            if let Some(ref h) = select.having {
                if has_outer_ref(h) {
                    return true;
                }
            }
            false
        }
        AnalyzedQueryBody::Values(rows) => rows.iter().flatten().any(has_outer_ref),
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            is_correlated_query(left) || is_correlated_query(right)
        }
    };

    body_has_outer_ref || query.order_by.iter().any(|o| has_outer_ref(&o.expr))
}

/// Check if a table reference (or its nested joins) contains outer references.
fn table_ref_has_outer_ref(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Subquery(query) => is_correlated_query(query),
        AnalyzedTableRefKind::Function { args, .. } => args.iter().any(|arg| match arg {
            TypedFunctionArg::Positional(expr) => has_outer_ref(expr),
            TypedFunctionArg::Named { expr, .. } => has_outer_ref(expr),
        }),
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            table_ref_has_outer_ref(left)
                || table_ref_has_outer_ref(right)
                || matches!(condition, JoinCondition::On(expr) if has_outer_ref(expr))
        }
        _ => false,
    }
}

/// Check if a TypedExpr has any ColumnRef with scope_depth > 0 (outer reference).
pub(super) fn has_outer_ref(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::ColumnRef { scope_depth, .. } => *scope_depth > 0,
        TypedExprKind::BinaryOp { left, right, .. } => has_outer_ref(left) || has_outer_ref(right),
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => has_outer_ref(operand),
        TypedExprKind::Between {
            expr, low, high, ..
        } => has_outer_ref(expr) || has_outer_ref(low) || has_outer_ref(high),
        TypedExprKind::InList { expr, list, .. } => {
            has_outer_ref(expr) || list.iter().any(has_outer_ref)
        }
        TypedExprKind::Like { expr, pattern, .. }
        | TypedExprKind::SimilarTo { expr, pattern, .. } => {
            has_outer_ref(expr) || has_outer_ref(pattern)
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|e| has_outer_ref(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| has_outer_ref(w) || has_outer_ref(t))
                || else_result.as_ref().is_some_and(|e| has_outer_ref(e))
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().any(has_outer_ref),
        TypedExprKind::NullIf(a, b) => has_outer_ref(a) || has_outer_ref(b),
        TypedExprKind::FunctionCall { args, filter, .. } => {
            args.iter().any(has_outer_ref) || filter.as_ref().is_some_and(|f| has_outer_ref(f))
        }
        TypedExprKind::AggregateCall { args, filter, .. } => {
            args.iter().any(has_outer_ref) || filter.as_ref().is_some_and(|f| has_outer_ref(f))
        }
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(has_outer_ref)
                || partition_by.iter().any(has_outer_ref)
                || order_by.iter().any(|o| has_outer_ref(&o.expr))
        }
        TypedExprKind::InSubquery { expr, .. } | TypedExprKind::AnyAll { expr, .. } => {
            has_outer_ref(expr)
        }
        TypedExprKind::ArrayIndex { array, index } => has_outer_ref(array) || has_outer_ref(index),
        TypedExprKind::JsonAccess { expr, .. } => has_outer_ref(expr),
        TypedExprKind::Constant(_)
        | TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::Default => false,
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
pub(super) fn substitute_outer_refs_in_expr(expr: &TypedExpr, outer_row: &Row) -> TypedExpr {
    match &expr.kind {
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
                TypedExpr::new(TypedExprKind::Constant(val), expr.data_type.clone())
            } else if *scope_depth > 1 {
                // Deeper nesting → decrement scope_depth.
                TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        column_index: *column_index,
                        scope_depth: scope_depth - 1,
                        column_name: column_name.clone(),
                    },
                    expr.data_type.clone(),
                )
            } else {
                expr.clone()
            }
        }
        TypedExprKind::ScalarSubquery(subquery) => {
            if is_correlated_query(subquery) {
                let substituted = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExpr::new(
                    TypedExprKind::ScalarSubquery(Box::new(substituted)),
                    expr.data_type.clone(),
                )
            } else {
                expr.clone()
            }
        }
        TypedExprKind::ArraySubquery(subquery) => {
            if is_correlated_query(subquery) {
                let substituted = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExpr::new(
                    TypedExprKind::ArraySubquery(Box::new(substituted)),
                    expr.data_type.clone(),
                )
            } else {
                expr.clone()
            }
        }
        TypedExprKind::Exists { subquery, negated } => {
            if is_correlated_query(subquery) {
                let substituted = substitute_outer_refs_in_query(subquery, outer_row);
                TypedExpr::new(
                    TypedExprKind::Exists {
                        subquery: Box::new(substituted),
                        negated: *negated,
                    },
                    expr.data_type.clone(),
                )
            } else {
                expr.clone()
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
                TypedExpr::new(
                    TypedExprKind::InSubquery {
                        expr: Box::new(inner_sub),
                        subquery: Box::new(sub),
                        negated: *negated,
                    },
                    expr.data_type.clone(),
                )
            } else {
                TypedExpr::new(
                    TypedExprKind::InSubquery {
                        expr: Box::new(inner_sub),
                        subquery: subquery.clone(),
                        negated: *negated,
                    },
                    expr.data_type.clone(),
                )
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
                TypedExpr::new(
                    TypedExprKind::AnyAll {
                        expr: Box::new(inner_sub),
                        op: op.clone(),
                        subquery: Box::new(sub),
                        is_all: *is_all,
                    },
                    expr.data_type.clone(),
                )
            } else {
                TypedExpr::new(
                    TypedExprKind::AnyAll {
                        expr: Box::new(inner_sub),
                        op: op.clone(),
                        subquery: subquery.clone(),
                        is_all: *is_all,
                    },
                    expr.data_type.clone(),
                )
            }
        }
        // Recurse into composite nodes.
        TypedExprKind::BinaryOp { left, right, op } => TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(substitute_outer_refs_in_expr(left, outer_row)),
                op: op.clone(),
                right: Box::new(substitute_outer_refs_in_expr(right, outer_row)),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::UnaryOp { operand, op } => TypedExpr::new(
            TypedExprKind::UnaryOp {
                operand: Box::new(substitute_outer_refs_in_expr(operand, outer_row)),
                op: op.clone(),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(substitute_outer_refs_in_expr(inner, outer_row)),
                target_type: target_type.clone(),
                cast_context: cast_context.clone(),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|a| substitute_outer_refs_in_expr(a, outer_row))
                    .collect(),
                order_by: order_by.clone(),
                filter: filter
                    .as_ref()
                    .map(|f| Box::new(substitute_outer_refs_in_expr(f, outer_row))),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExpr::new(
            TypedExprKind::Case {
                operand: operand
                    .as_ref()
                    .map(|e| Box::new(substitute_outer_refs_in_expr(e, outer_row))),
                when_clauses: when_clauses
                    .iter()
                    .map(|(w, t)| {
                        (
                            substitute_outer_refs_in_expr(w, outer_row),
                            substitute_outer_refs_in_expr(t, outer_row),
                        )
                    })
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|e| Box::new(substitute_outer_refs_in_expr(e, outer_row))),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(substitute_outer_refs_in_expr(inner, outer_row)),
                test: test.clone(),
                negated: *negated,
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(substitute_outer_refs_in_expr(inner, outer_row)),
                pattern: Box::new(substitute_outer_refs_in_expr(pattern, outer_row)),
                escape: escape
                    .as_ref()
                    .map(|e| Box::new(substitute_outer_refs_in_expr(e, outer_row))),
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::Coalesce(args) => TypedExpr::new(
            TypedExprKind::Coalesce(
                args.iter()
                    .map(|a| substitute_outer_refs_in_expr(a, outer_row))
                    .collect(),
            ),
            expr.data_type.clone(),
        ),
        TypedExprKind::NullIf(a, b) => TypedExpr::new(
            TypedExprKind::NullIf(
                Box::new(substitute_outer_refs_in_expr(a, outer_row)),
                Box::new(substitute_outer_refs_in_expr(b, outer_row)),
            ),
            expr.data_type.clone(),
        ),
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExpr::new(
            TypedExprKind::Between {
                expr: Box::new(substitute_outer_refs_in_expr(inner, outer_row)),
                low: Box::new(substitute_outer_refs_in_expr(low, outer_row)),
                high: Box::new(substitute_outer_refs_in_expr(high, outer_row)),
                negated: *negated,
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(substitute_outer_refs_in_expr(inner, outer_row)),
                list: list
                    .iter()
                    .map(|l| substitute_outer_refs_in_expr(l, outer_row))
                    .collect(),
                negated: *negated,
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            order_by,
            filter,
        } => TypedExpr::new(
            TypedExprKind::AggregateCall {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|a| substitute_outer_refs_in_expr(a, outer_row))
                    .collect(),
                distinct: *distinct,
                order_by: order_by.clone(),
                filter: filter
                    .as_ref()
                    .map(|f| Box::new(substitute_outer_refs_in_expr(f, outer_row))),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExpr::new(
            TypedExprKind::JsonAccess {
                expr: Box::new(substitute_outer_refs_in_expr(inner, outer_row)),
                path: Box::new(substitute_outer_refs_in_expr(path, outer_row)),
                operator: operator.clone(),
            },
            expr.data_type.clone(),
        ),
        TypedExprKind::ArrayIndex { array, index } => TypedExpr::new(
            TypedExprKind::ArrayIndex {
                array: Box::new(substitute_outer_refs_in_expr(array, outer_row)),
                index: Box::new(substitute_outer_refs_in_expr(index, outer_row)),
            },
            expr.data_type.clone(),
        ),
        // Leaf nodes that don't contain column refs.
        TypedExprKind::Constant(_) => expr.clone(),
        // Anything else: clone as-is (SimilarTo, WindowCall, MinMax, Row, ArrayLiteral).
        _ => expr.clone(),
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
}
