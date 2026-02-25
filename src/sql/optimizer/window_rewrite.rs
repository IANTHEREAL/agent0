//! Window function rewrite utilities shared by optimizer and executor.
//!
//! These operate on `TypedExpr` trees: detecting, extracting, and rewriting
//! `WindowCall` nodes for the Window → Project pipeline.

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::operators::WindowFunctionExpr;

/// Check if a TypedExpr tree contains a WindowCall.
pub fn contains_window(expr: &TypedExpr) -> bool {
    crate::sql::expr::traverse::visit_any(expr, |e| {
        matches!(e.kind, TypedExprKind::WindowCall { .. })
    })
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
        TypedExprKind::IsTest { expr, .. } => {
            collect_window_calls_from_expr(expr, output_name, result);
        }
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            collect_window_calls_from_expr(expr, output_name, result);
            collect_window_calls_from_expr(low, output_name, result);
            collect_window_calls_from_expr(high, output_name, result);
        }
        TypedExprKind::InList { expr, list, .. } => {
            collect_window_calls_from_expr(expr, output_name, result);
            for e in list {
                collect_window_calls_from_expr(e, output_name, result);
            }
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
            collect_window_calls_from_expr(expr, output_name, result);
            collect_window_calls_from_expr(pattern, output_name, result);
            if let Some(e) = escape {
                collect_window_calls_from_expr(e, output_name, result);
            }
        }
        TypedExprKind::ArrayLiteral(elems) | TypedExprKind::Row(elems) => {
            for e in elems {
                collect_window_calls_from_expr(e, output_name, result);
            }
        }
        TypedExprKind::ArrayIndex { array, index } => {
            collect_window_calls_from_expr(array, output_name, result);
            collect_window_calls_from_expr(index, output_name, result);
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            collect_window_calls_from_expr(expr, output_name, result);
            collect_window_calls_from_expr(path, output_name, result);
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

        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                test: *test,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                low: Box::new(rewrite_for_post_window(low, input_col_count, counter)),
                high: Box::new(rewrite_for_post_window(high, input_col_count, counter)),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                list: list
                    .iter()
                    .map(|e| rewrite_for_post_window(e, input_col_count, counter))
                    .collect(),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::Like {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                pattern: Box::new(rewrite_for_post_window(pattern, input_col_count, counter)),
                escape: escape
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_window(e, input_col_count, counter))),
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::SimilarTo {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                pattern: Box::new(rewrite_for_post_window(pattern, input_col_count, counter)),
                escape: escape
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_window(e, input_col_count, counter))),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::ArrayLiteral(elems) => TypedExpr {
            kind: TypedExprKind::ArrayLiteral(
                elems
                    .iter()
                    .map(|e| rewrite_for_post_window(e, input_col_count, counter))
                    .collect(),
            ),
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Row(elems) => TypedExpr {
            kind: TypedExprKind::Row(
                elems
                    .iter()
                    .map(|e| rewrite_for_post_window(e, input_col_count, counter))
                    .collect(),
            ),
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::ArrayIndex { array, index } => TypedExpr {
            kind: TypedExprKind::ArrayIndex {
                array: Box::new(rewrite_for_post_window(array, input_col_count, counter)),
                index: Box::new(rewrite_for_post_window(index, input_col_count, counter)),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExpr {
            kind: TypedExprKind::JsonAccess {
                expr: Box::new(rewrite_for_post_window(inner, input_col_count, counter)),
                path: Box::new(rewrite_for_post_window(path, input_col_count, counter)),
                operator: *operator,
            },
            data_type: expr.data_type.clone(),
        },

        // Non-recursive nodes — return as-is.
        _ => expr.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DataType;
    use crate::sql::analyzer::types::{FunctionKind, IsTestKind, JsonAccessOp, ResolvedFunction};

    // ── helpers ─────────────────────────────────────────────

    fn window_call() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::WindowCall {
                func: ResolvedFunction {
                    name: "row_number".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                partition_by: vec![],
                order_by: vec![],
                window_frame: None,
            },
            data_type: DataType::Int64,
        }
    }

    fn col_ref() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "x".to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn const_int() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(crate::model::Value::Int64(1)),
            data_type: DataType::Int64,
        }
    }

    fn const_text() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(crate::model::Value::Text("a".to_string())),
            data_type: DataType::Text,
        }
    }

    /// Assert that all three functions agree on window presence, extraction, and rewrite.
    fn assert_window_detected(expr: &TypedExpr, label: &str) {
        assert!(
            contains_window(expr),
            "{label}: contains_window should be true"
        );

        let mut collected = Vec::new();
        collect_window_calls_from_expr(expr, "out", &mut collected);
        assert!(
            !collected.is_empty(),
            "{label}: collect_window_calls_from_expr should find at least one window"
        );

        let mut counter = 0;
        let rewritten = rewrite_for_post_window(expr, 10, &mut counter);
        assert!(
            counter > 0,
            "{label}: rewrite_for_post_window should advance counter"
        );
        assert!(
            !contains_window(&rewritten),
            "{label}: rewritten expr should contain no window calls"
        );
    }

    fn assert_no_window(expr: &TypedExpr, label: &str) {
        assert!(
            !contains_window(expr),
            "{label}: contains_window should be false"
        );
    }

    // ── leaf / baseline tests ───────────────────────────────

    #[test]
    fn test_bare_window_call() {
        assert_window_detected(&window_call(), "bare WindowCall");
    }

    #[test]
    fn test_leaf_no_window() {
        assert_no_window(&col_ref(), "ColumnRef");
        assert_no_window(&const_int(), "Constant");
    }

    // ── IS test ─────────────────────────────────────────────

    #[test]
    fn test_is_test_with_window() {
        let expr = TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(window_call()),
                test: IsTestKind::Null,
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        assert_window_detected(&expr, "IsTest(window)");
    }

    // ── BETWEEN ─────────────────────────────────────────────

    #[test]
    fn test_between_with_window_in_expr() {
        let expr = TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(window_call()),
                low: Box::new(const_int()),
                high: Box::new(const_int()),
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        assert_window_detected(&expr, "Between(window, _, _)");
    }

    #[test]
    fn test_between_with_window_in_high() {
        let expr = TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(col_ref()),
                low: Box::new(const_int()),
                high: Box::new(window_call()),
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        assert_window_detected(&expr, "Between(_, _, window)");
    }

    // ── IN list ─────────────────────────────────────────────

    #[test]
    fn test_in_list_with_window() {
        let expr = TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(col_ref()),
                list: vec![const_int(), window_call()],
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        assert_window_detected(&expr, "InList(_, [_, window])");
    }

    // ── LIKE / SIMILAR TO ───────────────────────────────────

    #[test]
    fn test_like_with_window_in_expr() {
        let expr = TypedExpr {
            kind: TypedExprKind::Like {
                expr: Box::new(window_call()),
                pattern: Box::new(const_text()),
                escape: None,
                case_insensitive: false,
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        assert_window_detected(&expr, "Like(window, _)");
    }

    #[test]
    fn test_similar_to_with_window_in_escape() {
        let expr = TypedExpr {
            kind: TypedExprKind::SimilarTo {
                expr: Box::new(col_ref()),
                pattern: Box::new(const_text()),
                escape: Some(Box::new(window_call())),
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        assert_window_detected(&expr, "SimilarTo(_, _, escape=window)");
    }

    // ── ARRAY / ROW / JSON ──────────────────────────────────

    #[test]
    fn test_array_literal_with_window() {
        let expr = TypedExpr {
            kind: TypedExprKind::ArrayLiteral(vec![const_int(), window_call()]),
            data_type: DataType::Text, // array type placeholder
        };
        assert_window_detected(&expr, "ArrayLiteral([_, window])");
    }

    #[test]
    fn test_row_with_window() {
        let expr = TypedExpr {
            kind: TypedExprKind::Row(vec![window_call(), col_ref()]),
            data_type: DataType::Text,
        };
        assert_window_detected(&expr, "Row(window, _)");
    }

    #[test]
    fn test_array_index_with_window() {
        let expr = TypedExpr {
            kind: TypedExprKind::ArrayIndex {
                array: Box::new(col_ref()),
                index: Box::new(window_call()),
            },
            data_type: DataType::Int64,
        };
        assert_window_detected(&expr, "ArrayIndex(_, window)");
    }

    #[test]
    fn test_json_access_with_window() {
        let expr = TypedExpr {
            kind: TypedExprKind::JsonAccess {
                expr: Box::new(window_call()),
                path: Box::new(const_text()),
                operator: JsonAccessOp::Arrow,
            },
            data_type: DataType::Text,
        };
        assert_window_detected(&expr, "JsonAccess(window, _)");
    }
}
