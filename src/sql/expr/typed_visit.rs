//! Shared TypedExpr tree traversal helpers.
//!
//! Keep recursive shape handling centralized so semantic predicates and
//! rewriters do not need to duplicate full enum walks.

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind, WindowFrame, WindowFrameBound};

/// Return true if any node in the typed-expression tree matches `predicate`.
pub fn expr_any<F>(expr: &TypedExpr, predicate: &F) -> bool
where
    F: Fn(&TypedExpr) -> bool,
{
    if predicate(expr) {
        return true;
    }

    match &expr.kind {
        TypedExprKind::Constant(_)
        | TypedExprKind::ColumnRef { .. }
        | TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::Default => false,
        TypedExprKind::BinaryOp { left, right, .. } => {
            expr_any(left, predicate) || expr_any(right, predicate)
        }
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => expr_any(operand, predicate),
        TypedExprKind::Between {
            expr, low, high, ..
        } => expr_any(expr, predicate) || expr_any(low, predicate) || expr_any(high, predicate),
        TypedExprKind::InList { expr, list, .. } => {
            expr_any(expr, predicate) || list.iter().any(|e| expr_any(e, predicate))
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
            expr_any(expr, predicate)
                || expr_any(pattern, predicate)
                || escape.as_ref().is_some_and(|e| expr_any(e, predicate))
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|e| expr_any(e, predicate))
                || when_clauses
                    .iter()
                    .any(|(w, t)| expr_any(w, predicate) || expr_any(t, predicate))
                || else_result.as_ref().is_some_and(|e| expr_any(e, predicate))
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().any(|e| expr_any(e, predicate)),
        TypedExprKind::NullIf(a, b) => expr_any(a, predicate) || expr_any(b, predicate),
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
            args.iter().any(|e| expr_any(e, predicate))
                || order_by.iter().any(|ob| expr_any(&ob.expr, predicate))
                || filter.as_ref().is_some_and(|f| expr_any(f, predicate))
        }
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            window_frame,
            ..
        } => {
            args.iter().any(|e| expr_any(e, predicate))
                || partition_by.iter().any(|e| expr_any(e, predicate))
                || order_by.iter().any(|ob| expr_any(&ob.expr, predicate))
                || window_frame_any(window_frame, predicate)
        }
        TypedExprKind::InSubquery { expr, .. } | TypedExprKind::AnyAll { expr, .. } => {
            expr_any(expr, predicate)
        }
        TypedExprKind::ArrayIndex { array, index } => {
            expr_any(array, predicate) || expr_any(index, predicate)
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            expr_any(expr, predicate) || expr_any(path, predicate)
        }
    }
}

fn window_frame_any<F>(frame: &Option<WindowFrame>, predicate: &F) -> bool
where
    F: Fn(&TypedExpr) -> bool,
{
    frame.as_ref().is_some_and(|f| {
        window_frame_bound_any(&f.start, predicate)
            || f.end
                .as_ref()
                .is_some_and(|b| window_frame_bound_any(b, predicate))
    })
}

fn window_frame_bound_any<F>(bound: &WindowFrameBound, predicate: &F) -> bool
where
    F: Fn(&TypedExpr) -> bool,
{
    match bound {
        WindowFrameBound::CurrentRow => false,
        WindowFrameBound::Preceding(v) | WindowFrameBound::Following(v) => {
            v.as_ref().is_some_and(|e| expr_any(e, predicate))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{ResolvedFunction, WindowFrameUnits};
    use crate::types::{DataType, Value};

    #[test]
    fn expr_any_visits_window_frame_bound_expr() {
        let expr = TypedExpr::new(
            TypedExprKind::WindowCall {
                func: ResolvedFunction {
                    name: "sum".to_string(),
                    kind: crate::sql::analyzer::types::FunctionKind::Builtin,
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
