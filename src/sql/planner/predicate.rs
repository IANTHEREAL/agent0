//! Predicate analysis for query planning
//!
//! Extracts structured [`PredicateInfo`] from Analyzer (`TypedExpr`) expression
//! trees, enabling index selection and cost estimation.

use std::collections::HashMap;

use super::{PredicateInfo, PredicateOp};
use crate::model::Value;

/// Extract [`PredicateInfo`] from a [`TypedExpr`] tree.
///
/// Simpler than the old AST version: column names and constant values are already
/// resolved by the Analyzer.
pub fn analyze_typed_predicates(
    expr: &crate::sql::analyzer::types::TypedExpr,
) -> Vec<PredicateInfo> {
    let mut predicates = Vec::new();
    collect_typed_predicates(expr, &mut predicates);
    predicates
}

/// Collect a conjunction of `col = const` predicates into a map keyed by
/// lower-cased column name.
///
/// Returns `None` if the expression is not an AND tree of equality predicates,
/// or if the same column appears with conflicting constant values.
pub(crate) fn collect_typed_eq_predicates(
    expr: &crate::sql::analyzer::types::TypedExpr,
    out: &mut HashMap<String, Value>,
) -> Option<()> {
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                collect_typed_eq_predicates(left, out)?;
                collect_typed_eq_predicates(right, out)?;
                Some(())
            }
            TypedBinaryOp::Eq => {
                let (col, val) = if let TypedExprKind::ColumnRef { column_name, .. } = &left.kind {
                    if let TypedExprKind::Constant(v) = &right.kind {
                        (column_name.to_lowercase(), v.clone())
                    } else {
                        return None;
                    }
                } else if let TypedExprKind::ColumnRef { column_name, .. } = &right.kind {
                    if let TypedExprKind::Constant(v) = &left.kind {
                        (column_name.to_lowercase(), v.clone())
                    } else {
                        return None;
                    }
                } else {
                    return None;
                };

                if let Some(existing) = out.get(&col) {
                    if existing != &val {
                        return None;
                    }
                    return Some(());
                }

                out.insert(col, val);
                Some(())
            }
            _ => None,
        },
        _ => None,
    }
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
                        value: Some(Value::Null),
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
                        value: None,
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
                value: Some(val.clone()),
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
