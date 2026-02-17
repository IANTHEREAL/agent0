//! Expression classification helpers shared across executor and optimizer.
//!
//! Canonical home for `needs_async` (async materialization detection) and its
//! helper predicates. Both execution routing (`executor/select/analyzed/mod.rs`)
//! and optimizer eligibility (`optimizer/eligibility.rs`) import from here —
//! no duplication, no dependency inversion.
//!
//! Also provides `is_volatile` and `has_correlated_ref` for predicate pushdown.

use crate::sql::analyzer::types::{FunctionKind, TypedExpr, TypedExprKind};
use crate::sql::expr::typed_fold::is_volatile_or_side_effecting_builtin;
use crate::sql::expr::typed_visit::expr_any;

/// Check if a TypedExpr needs async (per-row) materialization.
///
/// Returns `true` if the expression contains subquery nodes or catalog-dependent
/// functions that can't be evaluated by the pure typed evaluator.
pub(crate) fn needs_async(expr: &TypedExpr) -> bool {
    has_unresolved_subquery(expr) || has_catalog_dependent_function(expr)
}

/// Check if a TypedExpr tree contains any non-materialized subquery node.
/// Used to detect expressions that can't be evaluated synchronously by FilterOperator.
pub(crate) fn has_unresolved_subquery(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::AnyAll { .. } => true,
        TypedExprKind::BinaryOp { left, right, .. } => {
            has_unresolved_subquery(left) || has_unresolved_subquery(right)
        }
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => has_unresolved_subquery(operand),
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            has_unresolved_subquery(expr)
                || has_unresolved_subquery(low)
                || has_unresolved_subquery(high)
        }
        TypedExprKind::InList { expr, list, .. } => {
            has_unresolved_subquery(expr) || list.iter().any(has_unresolved_subquery)
        }
        TypedExprKind::Like { expr, pattern, .. }
        | TypedExprKind::SimilarTo { expr, pattern, .. } => {
            has_unresolved_subquery(expr) || has_unresolved_subquery(pattern)
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|e| has_unresolved_subquery(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| has_unresolved_subquery(w) || has_unresolved_subquery(t))
                || else_result
                    .as_ref()
                    .is_some_and(|e| has_unresolved_subquery(e))
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().any(has_unresolved_subquery),
        TypedExprKind::NullIf(a, b) => has_unresolved_subquery(a) || has_unresolved_subquery(b),
        TypedExprKind::FunctionCall { args, filter, .. } => {
            args.iter().any(has_unresolved_subquery)
                || filter.as_ref().is_some_and(|f| has_unresolved_subquery(f))
        }
        TypedExprKind::AggregateCall { args, filter, .. } => {
            args.iter().any(has_unresolved_subquery)
                || filter.as_ref().is_some_and(|f| has_unresolved_subquery(f))
        }
        TypedExprKind::ArrayIndex { array, index } => {
            has_unresolved_subquery(array) || has_unresolved_subquery(index)
        }
        TypedExprKind::JsonAccess { expr, .. } => has_unresolved_subquery(expr),
        TypedExprKind::Constant(_) | TypedExprKind::ColumnRef { .. } => false,
        _ => false,
    }
}

/// Return true if `expr` contains a catalog-dependent function that must be
/// resolved at executor level (not via the pure typed evaluator).
pub(crate) fn has_catalog_dependent_function(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            let name = func.name.as_str();
            if name.eq_ignore_ascii_case("PG_GET_INDEXDEF")
                || name.eq_ignore_ascii_case("PG_GET_CONSTRAINTDEF")
                || name.eq_ignore_ascii_case("FORMAT_TYPE")
            {
                return true;
            }
            args.iter().any(has_catalog_dependent_function)
                || filter
                    .as_ref()
                    .is_some_and(|f| has_catalog_dependent_function(f))
                || order_by
                    .iter()
                    .any(|o| has_catalog_dependent_function(&o.expr))
        }
        TypedExprKind::AggregateCall {
            func,
            args,
            order_by,
            filter,
            ..
        } => {
            let name = func.name.as_str();
            if name.eq_ignore_ascii_case("PG_GET_INDEXDEF")
                || name.eq_ignore_ascii_case("PG_GET_CONSTRAINTDEF")
                || name.eq_ignore_ascii_case("FORMAT_TYPE")
            {
                return true;
            }
            args.iter().any(has_catalog_dependent_function)
                || filter
                    .as_ref()
                    .is_some_and(|f| has_catalog_dependent_function(f))
                || order_by
                    .iter()
                    .any(|o| has_catalog_dependent_function(&o.expr))
        }
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            ..
        } => {
            let name = func.name.as_str();
            if name.eq_ignore_ascii_case("PG_GET_INDEXDEF")
                || name.eq_ignore_ascii_case("PG_GET_CONSTRAINTDEF")
                || name.eq_ignore_ascii_case("FORMAT_TYPE")
            {
                return true;
            }
            args.iter().any(has_catalog_dependent_function)
                || partition_by.iter().any(has_catalog_dependent_function)
                || order_by
                    .iter()
                    .any(|o| has_catalog_dependent_function(&o.expr))
        }
        TypedExprKind::BinaryOp { left, right, .. } => {
            has_catalog_dependent_function(left) || has_catalog_dependent_function(right)
        }
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => has_catalog_dependent_function(operand),
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            has_catalog_dependent_function(expr)
                || has_catalog_dependent_function(low)
                || has_catalog_dependent_function(high)
        }
        TypedExprKind::InList { expr, list, .. } => {
            has_catalog_dependent_function(expr) || list.iter().any(has_catalog_dependent_function)
        }
        TypedExprKind::Like { expr, pattern, .. }
        | TypedExprKind::SimilarTo { expr, pattern, .. } => {
            has_catalog_dependent_function(expr) || has_catalog_dependent_function(pattern)
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand
                .as_ref()
                .is_some_and(|e| has_catalog_dependent_function(e))
                || when_clauses.iter().any(|(w, t)| {
                    has_catalog_dependent_function(w) || has_catalog_dependent_function(t)
                })
                || else_result
                    .as_ref()
                    .is_some_and(|e| has_catalog_dependent_function(e))
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().any(has_catalog_dependent_function),
        TypedExprKind::NullIf(a, b) => {
            has_catalog_dependent_function(a) || has_catalog_dependent_function(b)
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            has_catalog_dependent_function(expr) || has_catalog_dependent_function(path)
        }
        TypedExprKind::ArrayIndex { array, index } => {
            has_catalog_dependent_function(array) || has_catalog_dependent_function(index)
        }
        _ => false,
    }
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
    expr_any(expr, &|e| match &e.kind {
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
    expr_any(
        expr,
        &|e| matches!(&e.kind, TypedExprKind::ColumnRef { scope_depth, .. } if *scope_depth > 0),
    )
}
