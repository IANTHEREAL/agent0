//! TypedExpr constant-folding helpers.
//!
//! This pass is intentionally conservative: it only folds row-independent,
//! sync-safe subtrees and avoids folding function/subquery nodes directly.

use crate::model::{Row, Value};
use crate::sql::analyzer::types::{
    FunctionKind, ResolvedFunction, TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::expr::traverse::map_children;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_visit::expr_any;
use crate::sql::query_context::QueryContext;

/// Fold row-independent constant subtrees inside a typed expression.
///
/// Uses [`map_children`] for canonical child recursion. The `Case` variant
/// retains custom dead-branch-elimination logic.
pub fn fold_typed_expr(expr: &TypedExpr, qctx: &QueryContext) -> TypedExpr {
    let kind = match &expr.kind {
        // Case: custom dead-branch elimination (preserves existing semantics)
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
        // Everything else: canonical child recursion
        _ => map_children(expr, &mut |child| fold_typed_expr(child, qctx)),
    };

    let rebuilt = TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    };
    fold_subtree_if_safe(rebuilt, qctx)
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

pub(crate) fn is_fold_candidate(expr: &TypedExpr) -> bool {
    !expr_any(expr, &|node| match &node.kind {
        TypedExprKind::ColumnRef { .. }
        | TypedExprKind::AggregateCall { .. }
        | TypedExprKind::WindowCall { .. }
        | TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::TupleInSubquery { .. }
        | TypedExprKind::AnyAll { .. }
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Default
        | TypedExprKind::Parameter { .. } => true,
        TypedExprKind::FunctionCall {
            func,
            order_by,
            filter,
            ..
        } => !is_foldable_function_call(func, order_by, filter),
        _ => false,
    })
}

fn is_foldable_function_call(
    func: &ResolvedFunction,
    order_by: &[TypedOrderByExpr],
    filter: &Option<Box<TypedExpr>>,
) -> bool {
    if !matches!(func.kind, FunctionKind::Builtin) {
        return false;
    }
    if !order_by.is_empty() || filter.is_some() {
        return false;
    }

    !is_volatile_or_side_effecting_builtin(&func.name)
}

pub(crate) fn is_volatile_or_side_effecting_builtin(name: &str) -> bool {
    if crate::sql::advisory_locks::is_advisory_lock_function(name) {
        return true;
    }
    matches!(
        name.to_ascii_uppercase().as_str(),
        "NEXTVAL"
            | "CURRVAL"
            | "SETVAL"
            | "PG_SLEEP"
            | "RANDOM"
            | "SETSEED"
            | "GEN_RANDOM_UUID"
            | "UUID_GENERATE_V4"
            | "UUIDV7"
            | "CLOCK_TIMESTAMP"
            | "TXID_CURRENT"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{BinaryOp, FunctionKind, ResolvedFunction};
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
    fn folds_builtin_constant_function_call() {
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
        match folded.kind {
            TypedExprKind::Constant(Value::Text(v)) => assert_eq!(v, "db703"),
            other => panic!("expected folded constant, got {:?}", other),
        }
    }

    #[test]
    fn does_not_fold_volatile_function_call() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "random".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Float64,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Float64,
        );

        let folded = fold_typed_expr(&expr, &qctx);
        assert!(matches!(folded.kind, TypedExprKind::FunctionCall { .. }));
    }

    #[test]
    fn does_not_fold_user_defined_function_call() {
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "f_udf".to_string(),
                    kind: FunctionKind::UserDefined { oid: 42 },
                    return_type: DataType::Int32,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Int32,
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
