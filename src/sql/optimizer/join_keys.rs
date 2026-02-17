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
        } => {
            if let (
                TypedExprKind::ColumnRef {
                    column_index: a, ..
                },
                TypedExprKind::ColumnRef {
                    column_index: b, ..
                },
            ) = (&left.kind, &right.kind)
            {
                if *a < left_width && *b >= left_width {
                    left_keys.push(*a);
                    right_keys.push(*b - left_width);
                    true
                } else if *b < left_width && *a >= left_width {
                    left_keys.push(*b);
                    right_keys.push(*a - left_width);
                    true
                } else {
                    false // same-side equality — not a cross-boundary equi-key
                }
            } else {
                false // not a simple column=column
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{ResolvedUsingColumn, TypedExpr, TypedExprKind};
    use crate::types::DataType;

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
}
