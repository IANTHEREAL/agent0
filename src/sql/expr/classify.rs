//! Expression classification helpers shared across executor and optimizer.
//!
//! Canonical home for `needs_async` (async materialization detection) and its
//! helper predicates. Both execution routing (`executor/select/analyzed/mod.rs`)
//! and optimizer eligibility (`optimizer/eligibility.rs`) import from here —
//! no duplication, no dependency inversion.
//!
//! Also provides `is_volatile`, `has_correlated_ref`, and `has_any_column_ref`
//! for predicate pushdown and outer-row dependency detection.

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedTableRef, AnalyzedTableRefKind,
    FunctionKind, JoinCondition, TypedExpr, TypedExprKind, TypedFunctionArg,
};
use crate::sql::expr::traverse::visit_any;
use crate::sql::expr::typed_fold::is_volatile_or_side_effecting_builtin;

fn is_async_materialized_builtin(name: &str) -> bool {
    // These builtins require async execution and must be materialized by the executor.
    name.eq_ignore_ascii_case("PG_SLEEP")
}

fn is_pre_materialized_sequence_function(name: &str) -> bool {
    name.eq_ignore_ascii_case("NEXTVAL")
        || name.eq_ignore_ascii_case("CURRVAL")
        || name.eq_ignore_ascii_case("SETVAL")
        || name.eq_ignore_ascii_case("LASTVAL")
}

fn is_pg_get_serial_sequence_function(name: &str) -> bool {
    name.eq_ignore_ascii_case("PG_GET_SERIAL_SEQUENCE")
        || name.rsplit_once(".").is_some_and(|(schema, func)| {
            schema.eq_ignore_ascii_case("PG_CATALOG")
                && func.eq_ignore_ascii_case("PG_GET_SERIAL_SEQUENCE")
        })
}

fn is_catalog_dependent_function(func_kind: &FunctionKind, name: &str) -> bool {
    if matches!(func_kind, FunctionKind::UserDefined { .. }) {
        return true;
    }
    if is_async_materialized_builtin(name) {
        return true;
    }
    if name.eq_ignore_ascii_case("PG_GET_INDEXDEF")
        || name.eq_ignore_ascii_case("PG_GET_CONSTRAINTDEF")
        || name.eq_ignore_ascii_case("FORMAT_TYPE")
        || name.eq_ignore_ascii_case("TO_REGTYPE")
        || is_pg_get_serial_sequence_function(name)
    {
        return true;
    }
    if crate::sql::executor::split_cron_scalar_function_name(name).is_some() {
        return true;
    }
    if crate::sql::executor::is_bg_sql_function(name) {
        return true;
    }
    crate::sql::advisory_locks::is_advisory_lock_function(name)
}

/// Check if a TypedExpr needs pre-materialization before operator execution.
///
/// This is narrower than [`needs_async`]:
/// - includes unresolved subquery nodes
/// - includes sequence functions that must execute once at executor level
/// - excludes catalog-dependent/runtime-async builtins handled later
///
/// Uses [`visit_any`] (stack-safe) so deep ORM-generated predicates
/// cannot overflow the Rust stack during classification.
pub(crate) fn needs_pre_materialization(expr: &TypedExpr) -> bool {
    visit_any(expr, |node| match &node.kind {
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::TupleInSubquery { .. }
        | TypedExprKind::AnyAll { .. } => true,
        TypedExprKind::FunctionCall { func, .. } => {
            is_pre_materialized_sequence_function(&func.name)
        }
        _ => false,
    })
}

/// Check if a TypedExpr needs async (per-row) materialization.
///
/// Returns `true` if the expression contains subquery nodes or catalog-dependent
/// functions that can't be evaluated by the pure typed evaluator.
pub(crate) fn needs_async(expr: &TypedExpr) -> bool {
    has_unresolved_subquery(expr)
        || has_catalog_dependent_function(expr)
        || has_correlated_ref(expr)
}

/// Check if a TypedExpr tree contains any non-materialized subquery node.
/// Used to detect expressions that can't be evaluated synchronously by FilterOperator.
pub(crate) fn has_unresolved_subquery(expr: &TypedExpr) -> bool {
    visit_any(expr, |node| {
        matches!(
            &node.kind,
            TypedExprKind::ScalarSubquery(_)
                | TypedExprKind::ArraySubquery(_)
                | TypedExprKind::Exists { .. }
                | TypedExprKind::InSubquery { .. }
                | TypedExprKind::TupleInSubquery { .. }
                | TypedExprKind::AnyAll { .. }
        )
    })
}

/// Return true if `expr` contains a catalog-dependent function that must be
/// resolved at executor level (not via the pure typed evaluator).
pub(crate) fn has_catalog_dependent_function(expr: &TypedExpr) -> bool {
    visit_any(expr, |node| match &node.kind {
        TypedExprKind::FunctionCall { func, .. }
        | TypedExprKind::AggregateCall { func, .. }
        | TypedExprKind::WindowCall { func, .. } => {
            is_catalog_dependent_function(&func.kind, func.name.as_str())
        }
        _ => false,
    })
}

/// Check if a TypedExpr contains a volatile or side-effecting function.
///
/// Delegates to [`is_volatile_or_side_effecting_builtin`] for builtins (single
/// source of truth shared with constant folding). User-defined functions are
/// conservatively treated as volatile since we have no volatility metadata.
///
/// Note: `NOW`/`STATEMENT_TIMESTAMP`/`CURRENT_TIMESTAMP` are statement-stable
/// and intentionally NOT in the volatile list — they can be pushed down.
pub(crate) fn is_volatile(expr: &TypedExpr) -> bool {
    visit_any(expr, |e| match &e.kind {
        TypedExprKind::FunctionCall { func, .. } => match func.kind {
            FunctionKind::Builtin => is_volatile_or_side_effecting_builtin(&func.name),
            FunctionKind::UserDefined { .. } => true,
        },
        _ => false,
    })
}

/// Check if a TypedExpr contains a correlated reference (scope_depth > 0).
///
/// Correlated predicates reference outer queries and must never be pushed
/// below joins — they depend on per-row context from the outer scope.
pub(crate) fn has_correlated_ref(expr: &TypedExpr) -> bool {
    visit_any(
        expr,
        |e| matches!(&e.kind, TypedExprKind::ColumnRef { scope_depth, .. } if *scope_depth > 0),
    )
}

/// Check if a TypedExpr contains any column reference, regardless of scope.
///
/// Table functions in `FROM` require per-row execution whenever any argument
/// references a column from a previously-bound relation, even if that
/// reference is encoded as `scope_depth = 0` in the current query scope.
/// Descends into expression-level subquery payloads as well.
pub(crate) fn has_any_column_ref(expr: &TypedExpr) -> bool {
    expr_has_any_column_ref(expr)
}

fn expr_has_any_column_ref(expr: &TypedExpr) -> bool {
    visit_any(expr, |node| match &node.kind {
        TypedExprKind::ColumnRef { .. } => true,
        TypedExprKind::ScalarSubquery(subquery) | TypedExprKind::ArraySubquery(subquery) => {
            query_has_any_column_ref(subquery)
        }
        TypedExprKind::Exists { subquery, .. }
        | TypedExprKind::InSubquery { subquery, .. }
        | TypedExprKind::TupleInSubquery { subquery, .. }
        | TypedExprKind::AnyAll { subquery, .. } => query_has_any_column_ref(subquery),
        _ => false,
    })
}

fn query_has_any_column_ref(query: &AnalyzedQuery) -> bool {
    let body_has_column_ref = match &query.body {
        AnalyzedQueryBody::Select(select) => {
            let distinct_has_column_ref = match &select.distinct {
                AnalyzedDistinct::DistinctOn(on_exprs) => {
                    on_exprs.iter().any(expr_has_any_column_ref)
                }
                AnalyzedDistinct::All | AnalyzedDistinct::Distinct => false,
            };
            select.from.iter().any(table_ref_has_any_column_ref)
                || select
                    .where_clause
                    .as_ref()
                    .is_some_and(expr_has_any_column_ref)
                || select
                    .projection
                    .iter()
                    .any(|projection| expr_has_any_column_ref(&projection.expr))
                || select.group_by.iter().any(expr_has_any_column_ref)
                || select.having.as_ref().is_some_and(expr_has_any_column_ref)
                || distinct_has_column_ref
        }
        AnalyzedQueryBody::Values(rows) => rows.iter().flatten().any(expr_has_any_column_ref),
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            query_has_any_column_ref(left) || query_has_any_column_ref(right)
        }
    };

    body_has_column_ref
        || query
            .order_by
            .iter()
            .any(|order_by| expr_has_any_column_ref(&order_by.expr))
        || query.limit.as_ref().is_some_and(expr_has_any_column_ref)
        || query.offset.as_ref().is_some_and(expr_has_any_column_ref)
}

fn table_ref_has_any_column_ref(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => false,
        AnalyzedTableRefKind::Subquery(subquery) => query_has_any_column_ref(subquery),
        AnalyzedTableRefKind::Function { args, .. } => args.iter().any(|arg| match arg {
            TypedFunctionArg::Positional(expr) => expr_has_any_column_ref(expr),
            TypedFunctionArg::Named { expr, .. } => expr_has_any_column_ref(expr),
        }),
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            table_ref_has_any_column_ref(left)
                || table_ref_has_any_column_ref(right)
                || matches!(condition, JoinCondition::On(expr) if expr_has_any_column_ref(expr))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{
        AnalyzedProjection, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, BinaryOp,
        ResolvedFunction,
    };

    fn bool_const(v: bool) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(v)),
            DataType::Boolean,
        )
    }

    #[test]
    fn needs_pre_materialization_is_false_for_deep_non_async_predicate() {
        let mut expr = bool_const(false);
        for _ in 0..2048 {
            expr = TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(expr),
                    op: BinaryOp::Or,
                    right: Box::new(bool_const(true)),
                },
                DataType::Boolean,
            );
        }

        assert!(!needs_pre_materialization(&expr));
    }

    #[test]
    fn needs_async_is_false_for_deep_non_async_predicate() {
        let mut expr = bool_const(false);
        for _ in 0..2048 {
            expr = TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(expr),
                    op: BinaryOp::Or,
                    right: Box::new(bool_const(true)),
                },
                DataType::Boolean,
            );
        }

        assert!(!needs_async(&expr));
    }

    #[test]
    fn needs_pre_materialization_detects_sequence_function() {
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "nextval".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("s".to_string())),
                    DataType::Text,
                )],
                order_by: vec![],
                filter: None,
            },
            DataType::Int64,
        );

        assert!(needs_pre_materialization(&expr));
    }

    #[test]
    fn needs_async_detects_catalog_dependent_to_regtype() {
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "to_regtype".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("integer".to_string())),
                    DataType::Text,
                )],
                order_by: vec![],
                filter: None,
            },
            DataType::Int64,
        );
        assert!(needs_async(&expr));
    }

    #[test]
    fn has_any_column_ref_detects_scope_zero_reference() {
        let expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "t_col".to_string(),
            },
            DataType::Int32,
        );

        assert!(has_any_column_ref(&expr));
        assert!(!has_correlated_ref(&expr));
    }

    #[test]
    fn has_any_column_ref_descends_into_scalar_subquery_payload() {
        let subquery = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ArrayLiteral(vec![TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 1,
                                column_index: 0,
                                column_name: "t.id".to_string(),
                            },
                            DataType::Int32,
                        )]),
                        DataType::Array(Box::new(DataType::Int32)),
                    ),
                    output_name: "array_col".to_string(),
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
            output_schema: vec![(
                "array_col".to_string(),
                DataType::Array(Box::new(DataType::Int32)),
                None,
            )],
        };
        let expr = TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(subquery)),
            DataType::Array(Box::new(DataType::Int32)),
        );

        assert!(has_any_column_ref(&expr));
    }
    #[test]
    fn needs_async_detects_pg_get_serial_sequence_with_pg_catalog_qualifier() {
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "Pg_Catalog.PG_GET_SERIAL_SEQUENCE".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("t".to_string())),
                        DataType::Text,
                    ),
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("id".to_string())),
                        DataType::Text,
                    ),
                ],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );
        assert!(needs_async(&expr));
    }
}
