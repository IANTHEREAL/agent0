//! TypedExpr constant-folding helpers.
//!
//! This pass is intentionally conservative: it only folds row-independent,
//! sync-safe subtrees and avoids folding function/subquery nodes directly.

use crate::sql::analyzer::types::{
    TypedExpr, TypedExprKind, TypedOrderByExpr, WindowFrame, WindowFrameBound,
};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_visit::expr_any;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, Value};

/// Fold row-independent constant subtrees inside a typed expression.
pub fn fold_typed_expr(expr: &TypedExpr, qctx: &QueryContext) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::Constant(v) => TypedExprKind::Constant(v.clone()),
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } => TypedExprKind::ColumnRef {
            scope_depth: *scope_depth,
            column_index: *column_index,
            column_name: column_name.clone(),
        },
        TypedExprKind::BinaryOp { left, op, right } => TypedExprKind::BinaryOp {
            left: Box::new(fold_typed_expr(left, qctx)),
            op: op.clone(),
            right: Box::new(fold_typed_expr(right, qctx)),
        },
        TypedExprKind::UnaryOp { op, operand } => TypedExprKind::UnaryOp {
            op: *op,
            operand: Box::new(fold_typed_expr(operand, qctx)),
        },
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExprKind::Cast {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            target_type: target_type.clone(),
            cast_context: *cast_context,
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExprKind::IsTest {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            test: *test,
            negated: *negated,
        },
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExprKind::Between {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            low: Box::new(fold_typed_expr(low, qctx)),
            high: Box::new(fold_typed_expr(high, qctx)),
            negated: *negated,
        },
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExprKind::InList {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            list: list.iter().map(|e| fold_typed_expr(e, qctx)).collect(),
            negated: *negated,
        },
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExprKind::Like {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            pattern: Box::new(fold_typed_expr(pattern, qctx)),
            escape: escape.as_ref().map(|e| Box::new(fold_typed_expr(e, qctx))),
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExprKind::SimilarTo {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            pattern: Box::new(fold_typed_expr(pattern, qctx)),
            escape: escape.as_ref().map(|e| Box::new(fold_typed_expr(e, qctx))),
            negated: *negated,
        },
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            let folded_operand = operand.as_ref().map(|e| Box::new(fold_typed_expr(e, qctx)));
            let is_searched_case = folded_operand.is_none();

            let mut folded_when = Vec::with_capacity(when_clauses.len());
            let mut has_unconditional_true_branch = false;
            for (w, t) in when_clauses {
                if has_unconditional_true_branch {
                    break;
                }

                let fw = fold_typed_expr(w, qctx);
                let ft = fold_typed_expr(t, qctx);

                if is_searched_case
                    && matches!(fw.kind, TypedExprKind::Constant(Value::Boolean(true)))
                {
                    has_unconditional_true_branch = true;
                }

                folded_when.push((fw, ft));
            }

            let folded_else = if has_unconditional_true_branch {
                None
            } else {
                else_result
                    .as_ref()
                    .map(|e| Box::new(fold_typed_expr(e, qctx)))
            };

            TypedExprKind::Case {
                operand: folded_operand,
                when_clauses: folded_when,
                else_result: folded_else,
            }
        }
        TypedExprKind::Coalesce(args) => {
            TypedExprKind::Coalesce(args.iter().map(|e| fold_typed_expr(e, qctx)).collect())
        }
        TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
            Box::new(fold_typed_expr(a, qctx)),
            Box::new(fold_typed_expr(b, qctx)),
        ),
        TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
            args: args.iter().map(|e| fold_typed_expr(e, qctx)).collect(),
            is_greatest: *is_greatest,
        },
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExprKind::FunctionCall {
            func: func.clone(),
            args: args.iter().map(|e| fold_typed_expr(e, qctx)).collect(),
            order_by: fold_order_by(order_by, qctx),
            filter: filter.as_ref().map(|f| Box::new(fold_typed_expr(f, qctx))),
        },
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            order_by,
            filter,
        } => TypedExprKind::AggregateCall {
            func: func.clone(),
            args: args.iter().map(|e| fold_typed_expr(e, qctx)).collect(),
            distinct: *distinct,
            order_by: fold_order_by(order_by, qctx),
            filter: filter.as_ref().map(|f| Box::new(fold_typed_expr(f, qctx))),
        },
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => TypedExprKind::WindowCall {
            func: func.clone(),
            args: args.iter().map(|e| fold_typed_expr(e, qctx)).collect(),
            partition_by: partition_by
                .iter()
                .map(|e| fold_typed_expr(e, qctx))
                .collect(),
            order_by: fold_order_by(order_by, qctx),
            window_frame: fold_window_frame(window_frame, qctx),
        },
        TypedExprKind::ScalarSubquery(q) => TypedExprKind::ScalarSubquery(q.clone()),
        TypedExprKind::Exists { subquery, negated } => TypedExprKind::Exists {
            subquery: subquery.clone(),
            negated: *negated,
        },
        TypedExprKind::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => TypedExprKind::InSubquery {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            subquery: subquery.clone(),
            negated: *negated,
        },
        TypedExprKind::AnyAll {
            expr: inner,
            op,
            subquery,
            is_all,
        } => TypedExprKind::AnyAll {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            op: op.clone(),
            subquery: subquery.clone(),
            is_all: *is_all,
        },
        TypedExprKind::ArraySubquery(q) => TypedExprKind::ArraySubquery(q.clone()),
        TypedExprKind::ArrayLiteral(args) => {
            TypedExprKind::ArrayLiteral(args.iter().map(|e| fold_typed_expr(e, qctx)).collect())
        }
        TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
            array: Box::new(fold_typed_expr(array, qctx)),
            index: Box::new(fold_typed_expr(index, qctx)),
        },
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExprKind::JsonAccess {
            expr: Box::new(fold_typed_expr(inner, qctx)),
            path: Box::new(fold_typed_expr(path, qctx)),
            operator: *operator,
        },
        TypedExprKind::Row(args) => {
            TypedExprKind::Row(args.iter().map(|e| fold_typed_expr(e, qctx)).collect())
        }
        TypedExprKind::Default => TypedExprKind::Default,
    };

    let rebuilt = TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    };
    fold_subtree_if_safe(rebuilt, qctx)
}

fn fold_order_by(order_by: &[TypedOrderByExpr], qctx: &QueryContext) -> Vec<TypedOrderByExpr> {
    order_by
        .iter()
        .map(|ob| TypedOrderByExpr {
            expr: fold_typed_expr(&ob.expr, qctx),
            asc: ob.asc,
            nulls_first: ob.nulls_first,
        })
        .collect()
}

fn fold_window_frame(frame: &Option<WindowFrame>, qctx: &QueryContext) -> Option<WindowFrame> {
    frame.as_ref().map(|f| WindowFrame {
        units: f.units,
        start: fold_window_frame_bound(&f.start, qctx),
        end: f.end.as_ref().map(|b| fold_window_frame_bound(b, qctx)),
    })
}

fn fold_window_frame_bound(bound: &WindowFrameBound, qctx: &QueryContext) -> WindowFrameBound {
    match bound {
        WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
        WindowFrameBound::Preceding(v) => {
            WindowFrameBound::Preceding(v.as_ref().map(|e| Box::new(fold_typed_expr(e, qctx))))
        }
        WindowFrameBound::Following(v) => {
            WindowFrameBound::Following(v.as_ref().map(|e| Box::new(fold_typed_expr(e, qctx))))
        }
    }
}

fn fold_subtree_if_safe(expr: TypedExpr, qctx: &QueryContext) -> TypedExpr {
    if !is_fold_candidate(&expr) {
        return expr;
    }

    match eval_typed_expr(&expr, &Row::new(vec![]), qctx) {
        Ok(value) => TypedExpr::new(TypedExprKind::Constant(value), expr.data_type.clone()),
        Err(_) => expr,
    }
}

fn is_fold_candidate(expr: &TypedExpr) -> bool {
    !expr_any(expr, &|node| {
        matches!(
            &node.kind,
            TypedExprKind::ColumnRef { .. }
                | TypedExprKind::FunctionCall { .. }
                | TypedExprKind::AggregateCall { .. }
                | TypedExprKind::WindowCall { .. }
                | TypedExprKind::ScalarSubquery(_)
                | TypedExprKind::Exists { .. }
                | TypedExprKind::InSubquery { .. }
                | TypedExprKind::AnyAll { .. }
                | TypedExprKind::ArraySubquery(_)
                | TypedExprKind::Default
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{BinaryOp, FunctionKind, ResolvedFunction};
    use crate::types::{DataType, Value};
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
    fn fold_arithmetic_constants() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(1)),
                    DataType::Int32,
                )),
                op: BinaryOp::Add,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(2)),
                    DataType::Int32,
                )),
            },
            DataType::Int32,
        );

        let folded = fold_typed_expr(&expr, &qctx);
        match folded.kind {
            TypedExprKind::Constant(Value::Int32(v)) => assert_eq!(v, 3),
            other => panic!("expected folded constant, got {:?}", other),
        }
    }

    #[test]
    fn does_not_fold_column_ref_expr() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "x".to_string(),
                    },
                    DataType::Int32,
                )),
                op: BinaryOp::Add,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(1)),
                    DataType::Int32,
                )),
            },
            DataType::Int32,
        );

        let folded = fold_typed_expr(&expr, &qctx);
        assert!(matches!(folded.kind, TypedExprKind::BinaryOp { .. }));
    }

    #[test]
    fn does_not_fold_function_call_node() {
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

        let folded = fold_typed_expr(&expr, &qctx);
        assert!(matches!(folded.kind, TypedExprKind::FunctionCall { .. }));
    }

    #[test]
    fn folds_case_without_evaluating_dead_branch() {
        let qctx = test_qctx();
        let bad_div = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(1)),
                    DataType::Int32,
                )),
                op: BinaryOp::Div,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(0)),
                    DataType::Int32,
                )),
            },
            DataType::Int32,
        );

        let expr = TypedExpr::new(
            TypedExprKind::Case {
                operand: None,
                when_clauses: vec![(
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Boolean(true)),
                        DataType::Boolean,
                    ),
                    TypedExpr::new(TypedExprKind::Constant(Value::Int32(7)), DataType::Int32),
                )],
                else_result: Some(Box::new(bad_div)),
            },
            DataType::Int32,
        );

        let folded = fold_typed_expr(&expr, &qctx);
        match folded.kind {
            TypedExprKind::Constant(Value::Int32(v)) => assert_eq!(v, 7),
            other => panic!("expected folded constant, got {:?}", other),
        }
    }

    #[test]
    fn folds_case_with_unreachable_async_branch() {
        let qctx = test_qctx();
        let dead_async = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "nextval".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("public.s".to_string())),
                    DataType::Text,
                )],
                order_by: vec![],
                filter: None,
            },
            DataType::Int64,
        );

        let expr = TypedExpr::new(
            TypedExprKind::Case {
                operand: None,
                when_clauses: vec![
                    (
                        TypedExpr::new(
                            TypedExprKind::Constant(Value::Boolean(true)),
                            DataType::Boolean,
                        ),
                        TypedExpr::new(
                            TypedExprKind::Constant(Value::Text("ok".to_string())),
                            DataType::Text,
                        ),
                    ),
                    (
                        TypedExpr::new(
                            TypedExprKind::Constant(Value::Boolean(true)),
                            DataType::Boolean,
                        ),
                        dead_async,
                    ),
                ],
                else_result: Some(Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("fallback".to_string())),
                    DataType::Text,
                ))),
            },
            DataType::Text,
        );

        let folded = fold_typed_expr(&expr, &qctx);
        match folded.kind {
            TypedExprKind::Constant(Value::Text(v)) => assert_eq!(v, "ok"),
            other => panic!("expected folded constant, got {:?}", other),
        }
    }
}
