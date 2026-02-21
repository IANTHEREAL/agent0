//! Predicate analysis for query planning
//!
//! Extracts structured [`PredicateInfo`] from both AST (`Expr`) and Analyzer
//! (`TypedExpr`) expression trees, enabling index selection and cost estimation.

use sqlparser::ast::{BinaryOperator, Expr};

use super::{PredicateInfo, PredicateOp};
use crate::sql::expr::bridge::eval_const_ast_expr;
use crate::sql::names::normalize_ident;
use crate::types::Value;

pub fn analyze_predicates(expr: &Expr) -> Vec<PredicateInfo> {
    let mut predicates = Vec::new();
    collect_predicates(expr, &mut predicates);
    predicates
}

/// Extract [`PredicateInfo`] from a [`TypedExpr`] tree.
///
/// Simpler than the AST version: column names and constant values are already
/// resolved by the Analyzer.
pub fn analyze_typed_predicates(
    expr: &crate::sql::analyzer::types::TypedExpr,
) -> Vec<PredicateInfo> {
    let mut predicates = Vec::new();
    collect_typed_predicates(expr, &mut predicates);
    predicates
}

pub(super) fn collect_typed_predicates(
    expr: &crate::sql::analyzer::types::TypedExpr,
    predicates: &mut Vec<PredicateInfo>,
) {
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, IsTestKind, TypedExprKind};

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                collect_typed_predicates(left, predicates);
                collect_typed_predicates(right, predicates);
            }
            TypedBinaryOp::Or => {} // can't use for index selection
            _ => {
                let pred_op = match op {
                    TypedBinaryOp::Eq => Some(PredicateOp::Eq),
                    TypedBinaryOp::NotEq => Some(PredicateOp::Ne),
                    TypedBinaryOp::Lt => Some(PredicateOp::Lt),
                    TypedBinaryOp::LtEq => Some(PredicateOp::Le),
                    TypedBinaryOp::Gt => Some(PredicateOp::Gt),
                    TypedBinaryOp::GtEq => Some(PredicateOp::Ge),
                    _ => None,
                };
                if let Some(pred_op) = pred_op {
                    // Try col OP const or const OP col
                    if let Some(pred) = extract_typed_simple_predicate(left, right, pred_op.clone())
                    {
                        predicates.push(pred);
                    } else if let Some(pred) =
                        extract_typed_simple_predicate(right, left, flip_pred_op(pred_op))
                    {
                        predicates.push(pred);
                    }
                }
            }
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => {
            if let TypedExprKind::ColumnRef { column_name, .. } = &inner.kind {
                let op = match (test, negated) {
                    (IsTestKind::Null, false) => Some(PredicateOp::IsNull),
                    (IsTestKind::Null, true) => Some(PredicateOp::IsNotNull),
                    _ => None,
                };
                if let Some(op) = op {
                    predicates.push(PredicateInfo {
                        column: column_name.to_lowercase(),
                        op,
                        value: Value::Null,
                        in_values: vec![],
                    });
                }
            }
        }
        TypedExprKind::InList {
            expr: inner,
            list,
            negated: false,
        } => {
            if let TypedExprKind::ColumnRef { column_name, .. } = &inner.kind {
                let values: Vec<Value> = list
                    .iter()
                    .filter_map(|e| {
                        if let TypedExprKind::Constant(v) = &e.kind {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if values.len() == list.len() && !values.is_empty() {
                    predicates.push(PredicateInfo {
                        column: column_name.to_lowercase(),
                        op: PredicateOp::In,
                        value: values[0].clone(),
                        in_values: values,
                    });
                }
            }
        }
        _ => {}
    }
}

pub(super) fn extract_typed_simple_predicate(
    maybe_col: &crate::sql::analyzer::types::TypedExpr,
    maybe_val: &crate::sql::analyzer::types::TypedExpr,
    op: PredicateOp,
) -> Option<PredicateInfo> {
    use crate::sql::analyzer::types::TypedExprKind;
    if let TypedExprKind::ColumnRef { column_name, .. } = &maybe_col.kind {
        if let TypedExprKind::Constant(val) = &maybe_val.kind {
            return Some(PredicateInfo {
                column: column_name.to_lowercase(),
                op,
                value: val.clone(),
                in_values: vec![],
            });
        }
    }
    None
}

pub(super) fn flip_pred_op(op: PredicateOp) -> PredicateOp {
    match op {
        PredicateOp::Lt => PredicateOp::Gt,
        PredicateOp::Le => PredicateOp::Ge,
        PredicateOp::Gt => PredicateOp::Lt,
        PredicateOp::Ge => PredicateOp::Le,
        other => other,
    }
}

pub(super) fn collect_predicates(expr: &Expr, predicates: &mut Vec<PredicateInfo>) {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                collect_predicates(left, predicates);
                collect_predicates(right, predicates);
            }
            BinaryOperator::Or => {}
            BinaryOperator::Eq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Eq) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::NotEq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Ne) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::Lt => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Lt) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::LtEq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Le) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::Gt => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Gt) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::GtEq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Ge) {
                    predicates.push(pred);
                }
            }
            _ => {}
        },
        Expr::IsNull(inner) => {
            if let Expr::Identifier(ident) = &**inner {
                predicates.push(PredicateInfo {
                    column: normalize_ident(ident),
                    op: PredicateOp::IsNull,
                    value: Value::Null,
                    in_values: Vec::new(),
                });
            }
        }
        Expr::IsNotNull(inner) => {
            if let Expr::Identifier(ident) = &**inner {
                predicates.push(PredicateInfo {
                    column: normalize_ident(ident),
                    op: PredicateOp::IsNotNull,
                    value: Value::Null,
                    in_values: Vec::new(),
                });
            }
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            if *negated {
                return;
            }
            if let Expr::Identifier(ident) = &**expr {
                let column = normalize_ident(ident);
                let Ok(low_value) = eval_const_ast_expr(low) else {
                    return;
                };
                let Ok(high_value) = eval_const_ast_expr(high) else {
                    return;
                };
                predicates.push(PredicateInfo {
                    column: column.clone(),
                    op: PredicateOp::Ge,
                    value: low_value,
                    in_values: Vec::new(),
                });
                predicates.push(PredicateInfo {
                    column,
                    op: PredicateOp::Le,
                    value: high_value,
                    in_values: Vec::new(),
                });
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            if *negated {
                return;
            }
            if let Expr::Identifier(ident) = &**expr {
                let mut values = Vec::with_capacity(list.len());
                for item in list {
                    let Ok(v) = eval_const_ast_expr(item) else {
                        return;
                    };
                    values.push(v);
                }

                if !values.is_empty() {
                    predicates.push(PredicateInfo {
                        column: normalize_ident(ident),
                        op: PredicateOp::In,
                        value: values[0].clone(),
                        in_values: values,
                    });
                }
            }
        }
        Expr::Nested(e) => collect_predicates(e, predicates),
        _ => {}
    }
}

pub(super) fn extract_simple_predicate(
    left: &Expr,
    right: &Expr,
    op: PredicateOp,
) -> Option<PredicateInfo> {
    if let Expr::Identifier(ident) = left {
        if let Ok(val) = eval_const_ast_expr(right) {
            return Some(PredicateInfo {
                column: normalize_ident(ident),
                op,
                value: val,
                in_values: Vec::new(),
            });
        }
    }
    if let Expr::Identifier(ident) = right {
        if let Ok(val) = eval_const_ast_expr(left) {
            let reversed_op = match op {
                PredicateOp::Lt => PredicateOp::Gt,
                PredicateOp::Le => PredicateOp::Ge,
                PredicateOp::Gt => PredicateOp::Lt,
                PredicateOp::Ge => PredicateOp::Le,
                other => other,
            };
            return Some(PredicateInfo {
                column: normalize_ident(ident),
                op: reversed_op,
                value: val,
                in_values: Vec::new(),
            });
        }
    }
    None
}
