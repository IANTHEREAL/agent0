//! Equi-join key extraction from JoinCondition.
//!
//! Shared by physical_planner.rs (algorithm selection) and build.rs (operator construction).
//! Returns local indices: left keys are 0-based in left child's schema,
//! right keys are 0-based in right child's schema.

use crate::sql::analyzer::types::{BinaryOp, JoinCondition, TypedExpr, TypedExprKind};

/// Try to extract equi-join key pairs from a JoinCondition.
///
/// Returns `Some((left_local_indices, right_local_indices))` when:
/// - USING: uses the analyzer-computed local indices directly.
/// - ON: every AND-conjunct is `col[a] = col[b]` where exactly one of {a,b}
///   is < left_width (left side) and the other >= left_width (right side).
///   Right indices are rebased to 0-based (subtracts left_width).
///   Handles commuted equality (`right_col = left_col`).
///
/// Returns `None` for:
/// - Cross join (JoinCondition::None)
/// - Non-equi conditions (>, <, etc.)
/// - Same-side equalities (both indices on left or both on right)
/// - Mixed conjunctions with any non-equi term (all-or-nothing — see L1 limitation)
pub(super) fn try_extract_equi_keys(
    condition: &JoinCondition,
    left_width: usize,
) -> Option<(Vec<usize>, Vec<usize>)> {
    match condition {
        JoinCondition::Using(cols) => {
            if cols.is_empty() {
                return None;
            }
            let left_keys = cols.iter().map(|c| c.left_index).collect();
            let right_keys = cols.iter().map(|c| c.right_index).collect();
            Some((left_keys, right_keys))
        }
        JoinCondition::On(expr) => {
            let mut left_keys = Vec::new();
            let mut right_keys = Vec::new();
            if extract_validated(expr, left_width, &mut left_keys, &mut right_keys)
                && !left_keys.is_empty()
            {
                Some((left_keys, right_keys))
            } else {
                None
            }
        }
        JoinCondition::None => None,
    }
}

/// Extract hash-join keys and residual predicate from a JoinCondition.
///
/// Unlike [`try_extract_equi_keys`], this supports mixed ON predicates:
/// `equi_conjunct AND residual_conjunct ...`.
///
/// Returns `Some((left_keys, right_keys, residual))` when at least one
/// cross-boundary equi key is present. `residual` contains the remaining ON
/// conjuncts (if any) to be applied as a post-key join filter.
pub(super) fn extract_equi_keys_with_residual(
    condition: &JoinCondition,
    left_width: usize,
) -> Option<(Vec<usize>, Vec<usize>, Option<TypedExpr>)> {
    match condition {
        JoinCondition::Using(cols) => {
            if cols.is_empty() {
                return None;
            }
            let left_keys = cols.iter().map(|c| c.left_index).collect();
            let right_keys = cols.iter().map(|c| c.right_index).collect();
            Some((left_keys, right_keys, None))
        }
        JoinCondition::On(expr) => {
            let mut conjuncts = Vec::new();
            collect_and_conjuncts(expr, &mut conjuncts);

            let mut left_keys = Vec::new();
            let mut right_keys = Vec::new();
            let mut residual_terms = Vec::new();

            for term in conjuncts {
                if let Some((lk, rk)) = extract_single_equi_key(&term, left_width) {
                    left_keys.push(lk);
                    right_keys.push(rk);
                } else {
                    residual_terms.push(term);
                }
            }

            if left_keys.is_empty() {
                None
            } else {
                Some((left_keys, right_keys, combine_and_terms(residual_terms)))
            }
        }
        JoinCondition::None => None,
    }
}

fn collect_and_conjuncts(expr: &TypedExpr, out: &mut Vec<TypedExpr>) {
    match &expr.kind {
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => {
            collect_and_conjuncts(left, out);
            collect_and_conjuncts(right, out);
        }
        _ => out.push(expr.clone()),
    }
}

fn combine_and_terms(mut terms: Vec<TypedExpr>) -> Option<TypedExpr> {
    if terms.is_empty() {
        return None;
    }
    let first = terms.remove(0);
    Some(terms.into_iter().fold(first, |acc, next| TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(acc),
            op: BinaryOp::And,
            right: Box::new(next),
        },
        data_type: crate::model::DataType::Boolean,
    }))
}

fn extract_single_equi_key(term: &TypedExpr, left_width: usize) -> Option<(usize, usize)> {
    match &term.kind {
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::Eq,
            right,
        } => extract_cross_boundary_eq(left, right, left_width),
        _ => None,
    }
}

/// Recursive AND-tree walker. Returns false on first non-equi or same-side conjunct.
fn extract_validated(
    expr: &TypedExpr,
    left_width: usize,
    left_keys: &mut Vec<usize>,
    right_keys: &mut Vec<usize>,
) -> bool {
    match &expr.kind {
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => {
            extract_validated(left, left_width, left_keys, right_keys)
                && extract_validated(right, left_width, left_keys, right_keys)
        }
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::Eq,
            right,
        } => match extract_cross_boundary_eq(left, right, left_width) {
            Some((lk, rk)) => {
                left_keys.push(lk);
                right_keys.push(rk);
                true
            }
            None => false,
        },
        _ => false,
    }
}

fn extract_cross_boundary_eq(
    left: &TypedExpr,
    right: &TypedExpr,
    left_width: usize,
) -> Option<(usize, usize)> {
    let (left_sd, left_idx) = peel_column_ref(left)?;
    let (right_sd, right_idx) = peel_column_ref(right)?;
    if left_sd != 0 || right_sd != 0 {
        return None;
    }

    if left_idx < left_width && right_idx >= left_width {
        Some((left_idx, right_idx - left_width))
    } else if right_idx < left_width && left_idx >= left_width {
        Some((right_idx, left_idx - left_width))
    } else {
        None
    }
}

fn peel_column_ref(expr: &TypedExpr) -> Option<(u32, usize)> {
    match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            ..
        } => Some((*scope_depth, *column_index)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DataType;
    use crate::sql::analyzer::types::{ResolvedUsingColumn, TypedExpr, TypedExprKind};

    fn col_ref(idx: usize) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: idx,
                column_name: format!("col{}", idx),
            },
            data_type: DataType::Int32,
        }
    }

    fn eq_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Eq,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn and_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::And,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    #[test]
    fn test_basic() {
        // ON col[0] = col[2], left_width=2
        let cond = JoinCondition::On(eq_expr(col_ref(0), col_ref(2)));
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, Some((vec![0], vec![0])));
    }

    #[test]
    fn test_commuted() {
        // ON col[3] = col[1], left_width=2 → right=col[3] on left side of Eq, left=col[1]
        let cond = JoinCondition::On(eq_expr(col_ref(3), col_ref(1)));
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, Some((vec![1], vec![1])));
    }

    #[test]
    fn test_multi_key() {
        // ON col[0]=col[2] AND col[1]=col[3], left_width=2
        let left_eq = eq_expr(col_ref(0), col_ref(2));
        let right_eq = eq_expr(col_ref(1), col_ref(3));
        let cond = JoinCondition::On(and_expr(left_eq, right_eq));
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, Some((vec![0, 1], vec![0, 1])));
    }

    #[test]
    fn test_same_side_rejected() {
        // ON col[0] = col[1], left_width=3 → both on left side
        let cond = JoinCondition::On(eq_expr(col_ref(0), col_ref(1)));
        let result = try_extract_equi_keys(&cond, 3);
        assert_eq!(result, None);
    }

    #[test]
    fn test_non_eq_rejected() {
        // ON col[0] > col[2], left_width=2
        let cond = JoinCondition::On(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0)),
                op: BinaryOp::Gt,
                right: Box::new(col_ref(2)),
            },
            data_type: DataType::Boolean,
        });
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, None);
    }

    #[test]
    fn test_using() {
        let cond = JoinCondition::Using(vec![ResolvedUsingColumn {
            name: "id".to_string(),
            left_index: 0,
            right_index: 1,
            data_type: DataType::Int32,
            left_type: DataType::Int32,
            right_type: DataType::Int32,
        }]);
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, Some((vec![0], vec![1])));
    }

    #[test]
    fn test_cross_join() {
        let cond = JoinCondition::None;
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, None);
    }

    #[test]
    fn test_correlated_ref_rejected() {
        // ON correlated_col[0] = col[2], left_width=2
        // scope_depth=1 on left → must reject
        let correlated = TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 1,
                column_index: 0,
                column_name: "outer_col".to_string(),
            },
            data_type: DataType::Int32,
        };
        let local = col_ref(2);
        let cond = JoinCondition::On(eq_expr(correlated, local));
        let result = try_extract_equi_keys(&cond, 2);
        assert_eq!(result, None, "correlated ref must be rejected");
    }

    #[test]
    fn test_extract_with_residual_mixed_on() {
        // ON col[0] = col[2] AND col[1] > col[3], left_width=2
        let mixed = and_expr(
            eq_expr(col_ref(0), col_ref(2)),
            TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(col_ref(1)),
                    op: BinaryOp::Gt,
                    right: Box::new(col_ref(3)),
                },
                data_type: DataType::Boolean,
            },
        );
        let cond = JoinCondition::On(mixed);
        let (left_keys, right_keys, residual) =
            extract_equi_keys_with_residual(&cond, 2).expect("must extract hash keys");
        assert_eq!(left_keys, vec![0]);
        assert_eq!(right_keys, vec![0]);
        assert!(
            residual.is_some(),
            "non-equi conjunct must stay as residual"
        );
    }

    #[test]
    fn test_extract_with_residual_pure_equi_has_no_residual() {
        let cond = JoinCondition::On(eq_expr(col_ref(0), col_ref(2)));
        let (left_keys, right_keys, residual) =
            extract_equi_keys_with_residual(&cond, 2).expect("must extract pure equi");
        assert_eq!(left_keys, vec![0]);
        assert_eq!(right_keys, vec![0]);
        assert!(residual.is_none());
    }

    #[test]
    fn test_extract_with_residual_non_equi_rejected() {
        let cond = JoinCondition::On(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0)),
                op: BinaryOp::Gt,
                right: Box::new(col_ref(2)),
            },
            data_type: DataType::Boolean,
        });
        let result = extract_equi_keys_with_residual(&cond, 2);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_with_residual_same_side_eq_kept_as_residual() {
        // ON col[0] = col[2] AND col[0] = col[1], left_width=2.
        // The same-side conjunct must not become a hash key.
        let cond = JoinCondition::On(and_expr(
            eq_expr(col_ref(0), col_ref(2)),
            eq_expr(col_ref(0), col_ref(1)),
        ));
        let (left_keys, right_keys, residual) =
            extract_equi_keys_with_residual(&cond, 2).expect("must keep cross-boundary key");
        assert_eq!(left_keys, vec![0]);
        assert_eq!(right_keys, vec![0]);
        assert!(
            residual.is_some(),
            "same-side equality must remain residual"
        );
    }

    #[test]
    fn test_extract_with_residual_rejects_cast_wrapped_column_refs() {
        let casted_left = TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(col_ref(0)),
                target_type: DataType::Text,
                cast_context: crate::sql::types::CastContext::Implicit,
            },
            data_type: DataType::Text,
        };
        let casted_right = TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(col_ref(2)),
                target_type: DataType::Text,
                cast_context: crate::sql::types::CastContext::Implicit,
            },
            data_type: DataType::Text,
        };
        let cond = JoinCondition::On(eq_expr(casted_left, casted_right));
        let result = extract_equi_keys_with_residual(&cond, 2);
        assert!(
            result.is_none(),
            "cast-wrapped equality must not be extracted as hash key"
        );
    }
}
