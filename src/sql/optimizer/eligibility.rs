//! Optimizer eligibility — shared by execution and EXPLAIN routing.
//!
//! Determines whether an `AnalyzedQuery` can be routed through the CBO
//! pipeline. Execution-side routing (`mod.rs:111`) adds its own additional
//! guards (locks, SELECT INTO) — this helper covers query shape + expression
//! safety only.

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, JoinCondition, TypedExpr, TypedExprKind,
};
use crate::sql::expr::classify::needs_async;
use crate::sql::optimizer::logical_planner::expr_has_aggregate;

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

    // Reject CTEs — the optimizer doesn't handle WITH clauses.
    if !analyzed.ctes.is_empty() {
        return false;
    }

    // Reject window functions anywhere — projection, ORDER BY, HAVING.
    if select
        .projection
        .iter()
        .any(|p| expr_contains_window(&p.expr))
    {
        return false;
    }
    for ob in &analyzed.order_by {
        if expr_contains_window(&ob.expr) {
            return false;
        }
    }
    if let Some(ref having) = select.having {
        if expr_contains_window(having) {
            return false;
        }
    }

    // Reject DISTINCT ON — requires specialized ordering semantics.
    if matches!(select.distinct, AnalyzedDistinct::DistinctOn(_)) {
        return false;
    }

    // Reject aggregate queries whose ORDER BY / HAVING cannot be rewritten
    // to post-aggregate column positions.  This is a compile-time feasibility
    // check so that optimize() never needs a runtime fallback path.
    if !select.group_by.is_empty()
        || select
            .projection
            .iter()
            .any(|p| expr_has_aggregate(&p.expr))
        || select.having.is_some()
    {
        if !can_rewrite_post_aggregate(select, analyzed) {
            return false;
        }
    }

    true
}

/// Check whether all ORDER BY / HAVING expressions in an aggregate query
/// can be rewritten to reference post-aggregate column positions.
///
/// Performs a dry-run of `rewrite_post_aggregate_expr` — if any expression
/// fails to rewrite, the query is not eligible for the optimizer.
fn can_rewrite_post_aggregate(select: &AnalyzedSelect, query: &AnalyzedQuery) -> bool {
    let group_by = &select.group_by;
    let group_by_count = group_by.len();

    // Collect aggregate metadata from projections.
    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();
    for proj in &select.projection {
        super::build::collect_agg_exprs_from(
            &proj.expr,
            &proj.output_name,
            &mut agg_exprs,
            &mut agg_names,
            &mut agg_types,
        );
    }

    // Trial-rewrite HAVING.
    if let Some(ref having) = select.having {
        if super::build::rewrite_post_aggregate_expr(having, group_by, group_by_count, &agg_exprs)
            .is_err()
        {
            return false;
        }
    }

    // Trial-rewrite ORDER BY.
    for ob in &query.order_by {
        if super::build::rewrite_post_aggregate_expr(&ob.expr, group_by, group_by_count, &agg_exprs)
            .is_err()
        {
            return false;
        }
    }

    true
}

/// Recursively check that all leaves in a table ref tree are `Table` variants
/// backed by real KV-persisted schemas (not virtual catalog tables).
fn all_leaves_are_tables(table_ref: &AnalyzedTableRef) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, .. } => !is_virtual_catalog_table(name),
        AnalyzedTableRefKind::Join { left, right, .. } => {
            all_leaves_are_tables(left) && all_leaves_are_tables(right)
        }
        // Subquery and Function table refs not supported.
        _ => false,
    }
}

/// Virtual catalog tables have no KV schema, no indexes, and no statistics —
/// the optimizer cannot plan them.  Instead of fragile prefix matching, consult
/// the authoritative `CatalogRegistry` which holds every registered virtual
/// table by its bare name (e.g. `"pg_type"`, `"cron.job"`, `"columns"`).
fn is_virtual_catalog_table(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Check the name as-is (handles bare names like "pg_type" and
    // dotted names like "cron.job").
    if crate::sql::catalog::global_catalog().get(&lower).is_some() {
        return true;
    }
    // For schema-qualified names like "pg_catalog.pg_type" or
    // "information_schema.columns", strip the schema prefix and retry.
    let bare = lower
        .strip_prefix("information_schema.")
        .or_else(|| lower.strip_prefix("pg_catalog."));
    bare.map_or(false, |b| {
        crate::sql::catalog::global_catalog().get(b).is_some()
    })
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

/// Recursively check if an expression contains a window function call.
fn expr_contains_window(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::WindowCall { .. } => true,
        TypedExprKind::BinaryOp { left, right, .. } => {
            expr_contains_window(left) || expr_contains_window(right)
        }
        TypedExprKind::UnaryOp { operand, .. } => expr_contains_window(operand),
        TypedExprKind::Cast { expr, .. } => expr_contains_window(expr),
        TypedExprKind::FunctionCall { args, .. } => args.iter().any(expr_contains_window),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().map_or(false, |e| expr_contains_window(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| expr_contains_window(w) || expr_contains_window(t))
                || else_result
                    .as_ref()
                    .map_or(false, |e| expr_contains_window(e))
        }
        TypedExprKind::AggregateCall { args, filter, .. } => {
            args.iter().any(expr_contains_window)
                || filter.as_ref().map_or(false, |f| expr_contains_window(f))
        }
        _ => false,
    }
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
