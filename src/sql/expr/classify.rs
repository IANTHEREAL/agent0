//! Expression classification helpers shared across executor and optimizer.
//!
//! Canonical home for `needs_async` (async materialization detection) and its
//! helper predicates. Both execution routing (`executor/select/analyzed/mod.rs`)
//! and optimizer eligibility (`optimizer/eligibility.rs`) import from here —
//! no duplication, no dependency inversion.
//!
//! Also provides `is_volatile` and `has_correlated_ref` for predicate pushdown.

use crate::sql::analyzer::types::{FunctionKind, TypedExpr, TypedExprKind};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{BinaryOp, ResolvedFunction};

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
}
