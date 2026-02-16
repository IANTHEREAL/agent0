//! Static evaluation helpers for TypedExpr.
//!
//! This module separates row-independent expression evaluation from row-driven
//! execution. It does not perform name resolution; callers must provide
//! analyzer-produced `TypedExpr`.

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, Value};
use anyhow::{anyhow, Result};

/// Return true when an expression requires row values.
pub fn is_row_dependent(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::ColumnRef { .. } => true,
        TypedExprKind::BinaryOp { left, right, .. } => {
            is_row_dependent(left) || is_row_dependent(right)
        }
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => is_row_dependent(operand),
        TypedExprKind::Between {
            expr, low, high, ..
        } => is_row_dependent(expr) || is_row_dependent(low) || is_row_dependent(high),
        TypedExprKind::InList { expr, list, .. } => {
            is_row_dependent(expr) || list.iter().any(is_row_dependent)
        }
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        }
        | TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            is_row_dependent(expr)
                || is_row_dependent(pattern)
                || escape.as_ref().is_some_and(|e| is_row_dependent(e))
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|e| is_row_dependent(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| is_row_dependent(w) || is_row_dependent(t))
                || else_result.as_ref().is_some_and(|e| is_row_dependent(e))
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().any(is_row_dependent),
        TypedExprKind::NullIf(a, b) => is_row_dependent(a) || is_row_dependent(b),
        TypedExprKind::FunctionCall {
            args,
            order_by,
            filter,
            ..
        }
        | TypedExprKind::AggregateCall {
            args,
            order_by,
            filter,
            ..
        } => {
            args.iter().any(is_row_dependent)
                || order_by.iter().any(|o| is_row_dependent(&o.expr))
                || filter.as_ref().is_some_and(|f| is_row_dependent(f))
        }
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(is_row_dependent)
                || partition_by.iter().any(is_row_dependent)
                || order_by.iter().any(|o| is_row_dependent(&o.expr))
        }
        TypedExprKind::InSubquery { expr, .. } | TypedExprKind::AnyAll { expr, .. } => {
            is_row_dependent(expr)
        }
        TypedExprKind::ArrayIndex { array, index } => {
            is_row_dependent(array) || is_row_dependent(index)
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            is_row_dependent(expr) || is_row_dependent(path)
        }
        TypedExprKind::Constant(_)
        | TypedExprKind::Default
        | TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. } => false,
    }
}

/// Return true when an expression needs async executor-side materialization.
pub fn needs_async_materialization(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::AnyAll { .. }
        | TypedExprKind::AggregateCall { .. }
        | TypedExprKind::WindowCall { .. } => true,
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            let name = func.name.to_uppercase();
            matches!(name.as_str(), "NEXTVAL" | "CURRVAL" | "SETVAL")
                || args.iter().any(needs_async_materialization)
                || order_by
                    .iter()
                    .any(|o| needs_async_materialization(&o.expr))
                || filter
                    .as_ref()
                    .is_some_and(|f| needs_async_materialization(f))
        }
        TypedExprKind::BinaryOp { left, right, .. } => {
            needs_async_materialization(left) || needs_async_materialization(right)
        }
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => needs_async_materialization(operand),
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            needs_async_materialization(expr)
                || needs_async_materialization(low)
                || needs_async_materialization(high)
        }
        TypedExprKind::InList { expr, list, .. } => {
            needs_async_materialization(expr) || list.iter().any(needs_async_materialization)
        }
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        }
        | TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            needs_async_materialization(expr)
                || needs_async_materialization(pattern)
                || escape
                    .as_ref()
                    .is_some_and(|e| needs_async_materialization(e))
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand
                .as_ref()
                .is_some_and(|e| needs_async_materialization(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| needs_async_materialization(w) || needs_async_materialization(t))
                || else_result
                    .as_ref()
                    .is_some_and(|e| needs_async_materialization(e))
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().any(needs_async_materialization),
        TypedExprKind::NullIf(a, b) => {
            needs_async_materialization(a) || needs_async_materialization(b)
        }
        TypedExprKind::ArrayIndex { array, index } => {
            needs_async_materialization(array) || needs_async_materialization(index)
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            needs_async_materialization(expr) || needs_async_materialization(path)
        }
        TypedExprKind::Constant(_) | TypedExprKind::ColumnRef { .. } | TypedExprKind::Default => {
            false
        }
    }
}

/// Evaluate a row-independent typed expression.
///
/// Returns an error if the expression needs row data or async materialization.
pub fn eval_static_typed_expr(expr: &TypedExpr, qctx: &QueryContext) -> Result<Value> {
    if is_row_dependent(expr) {
        return Err(anyhow!(
            "expression requires row context and cannot be statically evaluated"
        ));
    }
    if needs_async_materialization(expr) {
        return Err(anyhow!(
            "expression requires async materialization and cannot be statically evaluated"
        ));
    }
    eval_typed_expr(expr, &Row::new(vec![]), qctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{FunctionKind, ResolvedFunction};
    use crate::types::DataType;
    use std::sync::Arc;

    fn test_qctx() -> QueryContext {
        QueryContext::new(
            7,
            Arc::from("db703"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        )
    }

    #[test]
    fn static_eval_constant_works() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(TypedExprKind::Constant(Value::Int32(42)), DataType::Int32);
        let got = eval_static_typed_expr(&expr, &qctx).unwrap();
        assert_eq!(got, Value::Int32(42));
    }

    #[test]
    fn static_eval_rejects_row_dependent_expr() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "x".to_string(),
            },
            DataType::Int32,
        );
        let err = eval_static_typed_expr(&expr, &qctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires row context"));
    }

    #[test]
    fn static_eval_current_database_uses_explicit_qctx() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "current_database".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );
        let got = eval_static_typed_expr(&expr, &qctx).unwrap();
        assert_eq!(got, Value::Text("db703".to_string()));
    }
}
