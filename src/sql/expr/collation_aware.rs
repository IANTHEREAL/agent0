//! Collation-aware comparison utilities

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::collation::{compare_with_resolved_collation, ResolvedCollation};
use crate::types::Value;
use anyhow::Result;

/// Extract collation name from a typed expression, if any (legacy — for display/debug).
pub fn extract_collation(expr: &TypedExpr) -> Option<String> {
    match &expr.kind {
        TypedExprKind::Collate { collation, .. } => Some(collation.clone()),
        TypedExprKind::Cast { expr: inner, .. } => extract_collation(inner),
        _ => None,
    }
}

/// Extract resolved collation from a typed expression, if any.
pub fn extract_resolved_collation(expr: &TypedExpr) -> Option<ResolvedCollation> {
    match &expr.kind {
        TypedExprKind::Collate { resolved, .. } => Some(resolved.clone()),
        TypedExprKind::Cast { expr: inner, .. } => extract_resolved_collation(inner),
        _ => None,
    }
}

/// Compare two text values using collation if specified.
/// Caller MUST guard for NULL before calling this function.
pub fn compare_with_collation_from_expr(
    left_val: &Value,
    right_val: &Value,
    left_expr: &TypedExpr,
    right_expr: &TypedExpr,
) -> Result<i8> {
    debug_assert!(
        !matches!(left_val, Value::Null) && !matches!(right_val, Value::Null),
        "BUG: collation comparison called with NULL operand — caller must guard"
    );

    // Try to get resolved collation from either operand (left takes precedence)
    let resolved =
        extract_resolved_collation(left_expr).or_else(|| extract_resolved_collation(right_expr));

    match (left_val, right_val) {
        (Value::Text(a), Value::Text(b)) => {
            if let Some(ref coll) = resolved {
                let cmp = compare_with_resolved_collation(a, b, coll)?;
                Ok(cmp as i8)
            } else {
                // No collation — use standard comparison
                crate::sql::expr::operators::compare_values(left_val, right_val)
            }
        }
        _ => {
            // For non-text types, use standard comparison
            crate::sql::expr::operators::compare_values(left_val, right_val)
        }
    }
}
