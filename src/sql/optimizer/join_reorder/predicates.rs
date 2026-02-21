//! Predicate classification, column lifting, and join-group flattening.
//!
//! Splits conjuncts into base-local filters, two-relation join edges,
//! and remaining (0-rel or 3+-rel) predicates.

use std::collections::{HashMap, HashSet};

use crate::sql::analyzer::types::{
    reindex_typed_expr, BinaryOp, JoinCondition, JoinType, TypedExpr, TypedExprKind,
};
use crate::sql::expr::traverse::map_children;

use super::super::logical_plan::{LogicalNode, LogicalPlan};
use super::super::rewrite::collect_column_indices;
use super::logical_has_correlated_refs;

// ── Flattening ──────────────────────────────────────────────────

/// A base relation in the flattened join group.
#[derive(Debug, Clone)]
pub(super) struct BaseRelation {
    /// Unique ID in this join group (index into the rels vector).
    pub id: usize,
    /// The logical plan subtree for this relation.
    pub plan: LogicalPlan,
    /// Column offset in the root join schema.
    pub col_offset: usize,
    /// Number of output columns.
    pub width: usize,
}

/// Flatten an inner/cross join tree into base relations and raw predicates.
///
/// `root_offset` tracks the cumulative column offset from the root of the
/// join group, so ON conditions can be lifted to root-level indices.
pub(super) fn flatten_recursive(
    plan: LogicalPlan,
    rels: &mut Vec<BaseRelation>,
    preds: &mut Vec<TypedExpr>,
    root_offset: usize,
) {
    match plan.node {
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } if matches!(join_type, JoinType::Inner | JoinType::Cross)
            && !matches!(condition, JoinCondition::Using(..))
            && !logical_has_correlated_refs(&right) =>
        {
            let left_width = left.schema.columns.len();
            flatten_recursive(*left, rels, preds, root_offset);
            flatten_recursive(*right, rels, preds, root_offset + left_width);
            if let JoinCondition::On(expr) = condition {
                let lifted = if root_offset > 0 {
                    lift_typed_expr(&expr, root_offset)
                } else {
                    expr
                };
                preds.push(lifted);
            }
        }
        _ => {
            // Opaque base relation
            let width = plan.schema.columns.len();
            let id = rels.len();
            rels.push(BaseRelation {
                id,
                plan,
                col_offset: root_offset,
                width,
            });
        }
    }
}

/// Lift column indices by adding `offset` to `ColumnRef.column_index` at scope_depth 0.
///
/// Inverse of `reindex_typed_expr` — used to convert local ON conditions
/// to root-level join-group indices.
pub(super) fn lift_typed_expr(expr: &TypedExpr, offset: usize) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } if *scope_depth == 0 => TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: column_index + offset,
            column_name: column_name.clone(),
        },
        _ => map_children(expr, &mut |child| lift_typed_expr(child, offset)),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

// ── Predicate classification ────────────────────────────────────

/// A join edge between two base relations.
#[derive(Debug, Clone)]
pub(super) struct JoinEdge {
    /// Bitmask of involved relations (exactly 2 bits set).
    pub rels: u64,
    /// The predicate conjunct.
    pub predicate: TypedExpr,
    /// Whether this is a pure equi-join predicate (bare Var = Var across rels).
    pub is_equi: bool,
}

/// Result of classifying all predicates.
pub(super) struct Classification {
    /// Two-relation edges.
    pub edges: Vec<JoinEdge>,
    /// Single-relation filters, keyed by rel_id.
    pub base_local_filters: HashMap<usize, Vec<TypedExpr>>,
    /// Predicates referencing 0 or 3+ relations.
    pub remaining: Vec<TypedExpr>,
}

/// Determine which base relation a column index belongs to.
fn find_rel_for_column(col_idx: usize, rels: &[BaseRelation]) -> Option<usize> {
    for rel in rels {
        if col_idx >= rel.col_offset && col_idx < rel.col_offset + rel.width {
            return Some(rel.id);
        }
    }
    None
}

/// Check if a predicate is a pure equi-join predicate:
/// bare `ColumnRef = ColumnRef` where the two columns reference different relations.
pub(super) fn is_pure_equi_join_pred(expr: &TypedExpr, rels: &[BaseRelation]) -> bool {
    if let TypedExprKind::BinaryOp { left, op, right } = &expr.kind {
        if *op != BinaryOp::Eq {
            return false;
        }
        // Both sides must be bare ColumnRef at scope_depth 0
        let left_col = match &left.kind {
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index,
                ..
            } => Some(*column_index),
            _ => None,
        };
        let right_col = match &right.kind {
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index,
                ..
            } => Some(*column_index),
            _ => None,
        };
        if let (Some(l_idx), Some(r_idx)) = (left_col, right_col) {
            let l_rel = find_rel_for_column(l_idx, rels);
            let r_rel = find_rel_for_column(r_idx, rels);
            if let (Some(lr), Some(rr)) = (l_rel, r_rel) {
                return lr != rr;
            }
        }
    }
    false
}

/// Classify all predicates into edges, base-local filters, and remaining.
pub(super) fn classify_predicates(
    conjuncts: &[TypedExpr],
    rels: &[BaseRelation],
) -> Classification {
    let mut edges = Vec::new();
    let mut base_local: HashMap<usize, Vec<TypedExpr>> = HashMap::new();
    let mut remaining = Vec::new();

    for conj in conjuncts {
        let indices = collect_column_indices(conj);

        // Find which relations are referenced
        let mut rel_set: HashSet<usize> = HashSet::new();
        for &idx in &indices {
            if let Some(rel_id) = find_rel_for_column(idx, rels) {
                rel_set.insert(rel_id);
            }
        }

        match rel_set.len() {
            1 => {
                let rel_id = *rel_set.iter().next().unwrap();
                // Localize: subtract the base col_offset
                let local_pred = reindex_typed_expr(conj, rels[rel_id].col_offset);
                base_local.entry(rel_id).or_default().push(local_pred);
            }
            2 => {
                let mut iter = rel_set.iter();
                let r1 = *iter.next().unwrap();
                let r2 = *iter.next().unwrap();
                let rel_mask = (1u64 << r1) | (1u64 << r2);
                let is_equi = is_pure_equi_join_pred(conj, rels);
                edges.push(JoinEdge {
                    rels: rel_mask,
                    predicate: conj.clone(),
                    is_equi,
                });
            }
            _ => {
                // 0 or 3+ relations
                remaining.push(conj.clone());
            }
        }
    }

    Classification {
        edges,
        base_local_filters: base_local,
        remaining,
    }
}
