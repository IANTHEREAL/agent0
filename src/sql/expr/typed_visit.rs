//! Shared TypedExpr tree traversal helpers.
//!
//! Thin wrapper over [`super::traverse::visit_any`] for backward compatibility.
//! New code should use `traverse::visit_any` directly.

use crate::sql::analyzer::types::TypedExpr;
use crate::sql::expr::traverse::visit_any;

/// Return true if any node in the typed-expression tree matches `predicate`.
pub fn expr_any<F>(expr: &TypedExpr, predicate: &F) -> bool
where
    F: Fn(&TypedExpr) -> bool,
{
    visit_any(expr, |e| predicate(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{
        FunctionKind, ResolvedFunction, TypedExprKind, WindowFrame, WindowFrameBound,
        WindowFrameUnits,
    };

    #[test]
    fn expr_any_visits_window_frame_bound_expr() {
        let expr = TypedExpr::new(
            TypedExprKind::WindowCall {
                func: ResolvedFunction {
                    name: "sum".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(1)),
                    DataType::Int64,
                )],
                partition_by: vec![],
                order_by: vec![],
                window_frame: Some(WindowFrame {
                    units: WindowFrameUnits::Rows,
                    start: WindowFrameBound::Preceding(Some(Box::new(TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "n".to_string(),
                        },
                        DataType::Int64,
                    )))),
                    end: None,
                }),
            },
            DataType::Int64,
        );

        assert!(expr_any(&expr, &|node| matches!(
            node.kind,
            TypedExprKind::ColumnRef { .. }
        )));
    }
}
