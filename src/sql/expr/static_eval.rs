//! Static evaluation helpers for TypedExpr.
//!
//! This module separates row-independent expression evaluation from row-driven
//! execution. It does not perform name resolution; callers must provide
//! analyzer-produced `TypedExpr`.

use crate::model::{Row, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_visit::expr_any;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};

/// Return true when an expression requires row values.
pub fn is_row_dependent(expr: &TypedExpr) -> bool {
    expr_any(expr, &|node| {
        matches!(&node.kind, TypedExprKind::ColumnRef { .. })
    })
}

/// Return true when an expression needs async executor-side materialization.
pub fn needs_async_materialization(expr: &TypedExpr) -> bool {
    // Keep aggregate/window guarded explicitly (not a valid static-eval target),
    // and defer async/materialization classification to the shared classifier used
    // by analyzed SELECT and operator paths.
    expr_any(expr, &|node| {
        matches!(
            &node.kind,
            TypedExprKind::AggregateCall { .. } | TypedExprKind::WindowCall { .. }
        )
    }) || crate::sql::expr::classify::needs_pre_materialization(expr)
        || crate::sql::expr::classify::needs_async(expr)
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
    use crate::model::DataType;
    use crate::sql::analyzer::types::{FunctionKind, ResolvedFunction};
    use std::sync::Arc;

    fn test_qctx() -> QueryContext {
        QueryContext::new(
            7,
            Arc::from("db703"),
            Arc::from("postgres"),
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

    #[test]
    fn static_eval_rejects_lastval_for_async_materialization() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "lastval".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Int64,
        );
        let err = eval_static_typed_expr(&expr, &qctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires async materialization"));
    }

    #[test]
    fn static_eval_rejects_pg_get_serial_sequence_for_async_materialization() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "Pg_Catalog.PG_GET_SERIAL_SEQUENCE".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Text("public.t".to_string())),
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

        let err = eval_static_typed_expr(&expr, &qctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires async materialization"));
    }
}
