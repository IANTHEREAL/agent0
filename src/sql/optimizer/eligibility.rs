//! Optimizer eligibility — shared by execution and EXPLAIN routing.
//!
//! Determines whether an `AnalyzedQuery` can be routed through the CBO
//! pipeline. Execution-side routing (`mod.rs:111`) adds its own additional
//! guards (locks, SELECT INTO) — this helper covers query shape + expression
//! safety only.

use crate::sql::analyzer::types::{
    AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind,
    JoinCondition,
};
use crate::sql::expr::classify::needs_async;

/// Check whether a query is eligible for the CBO optimizer pipeline.
///
/// Phase 3 eligibility covers:
/// - SELECT body only (no SetOperation, no VALUES)
/// - FROM non-empty; may be a single join tree (`A JOIN B`) OR multiple
///   comma-separated tables (`FROM a, b`). The analyzer keeps comma FROM as
///   separate entries in `select.from`; the logical planner builds implicit
///   CROSS JOINs for them (`LogicalPlanner::build_from`).
/// - All leaf table refs are Table (no subquery, no function)
/// - No expression position contains async expressions (subqueries,
///   catalog-dependent functions)
pub fn is_optimizer_eligible(analyzed: &AnalyzedQuery) -> bool {
    let select = match &analyzed.body {
        AnalyzedQueryBody::Select(select) => select,
        // SetOperation and Values not supported in optimizer build
        // (build.rs would fail).
        _ => return false,
    };

    // Must have at least one FROM item.
    if select.from.is_empty() {
        return false;
    }

    // All leaf table refs must be simple Tables (no subquery, no function).
    // Handles both single join trees and comma-separated FROM items.
    if !select.from.iter().all(all_leaves_are_tables) {
        return false;
    }

    // Reject if any expression position contains async expressions.
    if has_any_async_expr(select, analyzed) {
        return false;
    }

    true
}

/// Recursively check that all leaves in a table ref tree are `Table` variants.
fn all_leaves_are_tables(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => true,
        AnalyzedTableRefKind::Join { left, right, .. } => {
            all_leaves_are_tables(left) && all_leaves_are_tables(right)
        }
        // Subquery and Function table refs not supported.
        _ => false,
    }
}

/// Check if any expression position in the query contains async expressions.
///
/// Walks: SELECT list, WHERE, HAVING, ORDER BY, all JOIN ON conditions.
/// The optimizer path bypasses all legacy async materialization, so any async
/// expression makes the query ineligible.
fn has_any_async_expr(select: &AnalyzedSelect, query: &AnalyzedQuery) -> bool {
    // SELECT list
    for proj in &select.projection {
        if needs_async(&proj.expr) {
            return true;
        }
    }

    // WHERE
    if let Some(ref where_expr) = select.where_clause {
        if needs_async(where_expr) {
            return true;
        }
    }

    // HAVING
    if let Some(ref having_expr) = select.having {
        if needs_async(having_expr) {
            return true;
        }
    }

    // ORDER BY
    for ob in &query.order_by {
        if needs_async(&ob.expr) {
            return true;
        }
    }

    // JOIN ON conditions (recursive)
    for table_ref in &select.from {
        if table_ref_has_async_condition(table_ref) {
            return true;
        }
    }

    false
}

/// Recursively check JOIN ON conditions for async expressions.
fn table_ref_has_async_condition(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => false,
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            // Check ON condition
            if let JoinCondition::On(ref expr) = condition {
                if needs_async(expr) {
                    return true;
                }
            }
            // Recurse into children
            table_ref_has_async_condition(left) || table_ref_has_async_condition(right)
        }
        // Subquery / Function — shouldn't reach here (rejected by
        // all_leaves_are_tables), but be safe.
        _ => false,
    }
}
