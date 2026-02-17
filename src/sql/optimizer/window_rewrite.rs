//! Window function rewrite utilities shared by optimizer and executor.
//!
//! These operate on `TypedExpr` trees: detecting, extracting, and rewriting
//! `WindowCall` nodes for the Window → Project pipeline.

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::operators::WindowFunctionExpr;

/// Check if a TypedExpr tree contains a WindowCall.
pub fn contains_window(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::WindowCall { .. } => true,
        TypedExprKind::BinaryOp { left, right, .. } => {
            contains_window(left) || contains_window(right)
        }
        TypedExprKind::UnaryOp { operand, .. } | TypedExprKind::Cast { expr: operand, .. } => {
            contains_window(operand)
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|e| contains_window(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| contains_window(w) || contains_window(t))
                || else_result.as_ref().is_some_and(|e| contains_window(e))
        }
        TypedExprKind::Coalesce(args) | TypedExprKind::MinMax { args, .. } => {
            args.iter().any(contains_window)
        }
        TypedExprKind::NullIf(a, b) => contains_window(a) || contains_window(b),
        TypedExprKind::FunctionCall { args, .. } => args.iter().any(contains_window),
        _ => false,
    }
}

/// Recursively collect WindowCall nodes from a TypedExpr tree.
pub fn collect_window_calls_from_expr(
    expr: &TypedExpr,
    output_name: &str,
    result: &mut Vec<WindowFunctionExpr>,
) {
    match &expr.kind {
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => {
            let func_name = func.name.to_lowercase();
            let is_lag_lead = func_name == "lag" || func_name == "lead" || func_name == "nth_value";

            let (arg_expr, offset_expr, default_value_expr) = if is_lag_lead {
                (
                    args.first().cloned(),
                    args.get(1).cloned(),
                    args.get(2).cloned(),
                )
            } else {
                (args.first().cloned(), None, None)
            };

            let idx = result.len();
            result.push(WindowFunctionExpr {
                func_name,
                arg_expr,
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                offset_expr,
                default_value_expr,
                window_frame: window_frame.clone(),
                filter_expr: None,
                output_name: if idx == 0 {
                    output_name.to_string()
                } else {
                    format!("{}_{}", output_name, idx)
                },
                output_type: expr.data_type.clone(),
            });
            // Don't recurse into WindowCall children.
        }

        // Recurse to find nested WindowCall (e.g., CAST(ROW_NUMBER() OVER (...) AS INT)).
        TypedExprKind::BinaryOp { left, right, .. } => {
            collect_window_calls_from_expr(left, output_name, result);
            collect_window_calls_from_expr(right, output_name, result);
        }
        TypedExprKind::UnaryOp { operand, .. } | TypedExprKind::Cast { expr: operand, .. } => {
            collect_window_calls_from_expr(operand, output_name, result);
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_window_calls_from_expr(op, output_name, result);
            }
            for (w, t) in when_clauses {
                collect_window_calls_from_expr(w, output_name, result);
                collect_window_calls_from_expr(t, output_name, result);
            }
            if let Some(el) = else_result {
                collect_window_calls_from_expr(el, output_name, result);
            }
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_window_calls_from_expr(arg, output_name, result);
            }
        }
        TypedExprKind::NullIf(a, b) => {
            collect_window_calls_from_expr(a, output_name, result);
            collect_window_calls_from_expr(b, output_name, result);
        }
        _ => {}
    }
}

/// Rewrite a TypedExpr for post-window evaluation.
///
/// Replaces each WindowCall with a ColumnRef pointing to the window output
/// position (input_col_count + sequential window index). The counter tracks
/// the window index across the expression tree.
pub fn rewrite_for_post_window(
    expr: &TypedExpr,
    input_col_count: usize,
    counter: &mut usize,
) -> TypedExpr {
    match &expr.kind {
        TypedExprKind::WindowCall { .. } => {
            let idx = *counter;
            *counter += 1;
            TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: input_col_count + idx,
                    column_name: format!("window_{}", idx),
                },
                data_type: expr.data_type.clone(),
            }
        }

        TypedExprKind::BinaryOp { left, right, op } => TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(rewrite_for_post_window(left, input_col_count, counter)),
                op: op.clone(),
                right: Box::new(rewrite_for_post_window(right, input_col_count, counter)),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::UnaryOp { op, operand } => TypedExpr {
            kind: TypedExprKind::UnaryOp {
                op: *op,
                operand: Box::new(rewrite_for_post_window(operand, input_col_count, counter)),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                target_type: target_type.clone(),
                cast_context: cast_context.clone(),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExpr {
            kind: TypedExprKind::Case {
                operand: operand
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_window(e, input_col_count, counter))),
                when_clauses: when_clauses
                    .iter()
                    .map(|(w, t)| {
                        (
                            rewrite_for_post_window(w, input_col_count, counter),
                            rewrite_for_post_window(t, input_col_count, counter),
                        )
                    })
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_window(e, input_col_count, counter))),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Coalesce(args) => TypedExpr {
            kind: TypedExprKind::Coalesce(
                args.iter()
                    .map(|e| rewrite_for_post_window(e, input_col_count, counter))
                    .collect(),
            ),
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::NullIf(a, b) => TypedExpr {
            kind: TypedExprKind::NullIf(
                Box::new(rewrite_for_post_window(a, input_col_count, counter)),
                Box::new(rewrite_for_post_window(b, input_col_count, counter)),
            ),
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|e| rewrite_for_post_window(e, input_col_count, counter))
                    .collect(),
                order_by: order_by.clone(),
                filter: filter
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_window(e, input_col_count, counter))),
            },
            data_type: expr.data_type.clone(),
        },

        // Non-recursive nodes — return as-is.
        _ => expr.clone(),
    }
}
