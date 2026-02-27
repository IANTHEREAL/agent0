//! Predicate analysis for query planning
//!
//! Extracts structured [`TypedPredicate`] from Analyzer (`TypedExpr`) expression
//! trees, enabling index selection and cost estimation.

use std::collections::HashMap;

use super::{CmpOp, TypedPredicate};
use crate::model::Value;

/// Extract [`TypedPredicate`] from a [`TypedExpr`] tree.
///
/// Simpler than the old AST version: column names and constant values are already
/// resolved by the Analyzer.
pub fn analyze_typed_predicates(
    expr: &crate::sql::analyzer::types::TypedExpr,
) -> Vec<TypedPredicate> {
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
    predicates: &mut Vec<TypedPredicate>,
) {
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                collect_typed_predicates(left, predicates);
                collect_typed_predicates(right, predicates);
            }
            TypedBinaryOp::Or => {} // can't use for index selection
            _ => {
                let cmp_op = match op {
                    TypedBinaryOp::Eq => Some(CmpOp::Eq),
                    TypedBinaryOp::NotEq => Some(CmpOp::Ne),
                    TypedBinaryOp::Lt => Some(CmpOp::Lt),
                    TypedBinaryOp::LtEq => Some(CmpOp::Le),
                    TypedBinaryOp::Gt => Some(CmpOp::Gt),
                    TypedBinaryOp::GtEq => Some(CmpOp::Ge),
                    _ => None,
                };
                if let Some(cmp_op) = cmp_op {
                    // Try col OP const or const OP col
                    if let Some(pred) = extract_typed_simple_predicate(left, right, cmp_op.clone())
                    {
                        predicates.push(pred);
                    } else if let Some(pred) =
                        extract_typed_simple_predicate(right, left, flip_cmp_op(cmp_op))
                    {
                        predicates.push(pred);
                    }
                }
            }
        },
        // IS [NOT] NULL cannot drive B-tree index lookups; skip.
        TypedExprKind::IsTest { .. } => {}
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
                    predicates.push(TypedPredicate::InList {
                        column: column_name.to_lowercase(),
                        values,
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
    op: CmpOp,
) -> Option<TypedPredicate> {
    use crate::sql::analyzer::types::TypedExprKind;
    if let TypedExprKind::ColumnRef { column_name, .. } = &maybe_col.kind {
        if let TypedExprKind::Constant(val) = &maybe_val.kind {
            return Some(TypedPredicate::Comparison {
                column: column_name.to_lowercase(),
                op,
                value: val.clone(),
            });
        }
    }
    None
}

pub(super) fn flip_cmp_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        other => other,
    }
}
