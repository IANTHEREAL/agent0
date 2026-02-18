//! Cost-Based Optimizer (CBO) — Phase 3: LogicalPlan pipeline + joins.
//!
//! Translates `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator`.
//! Uses table statistics (when available via ANALYZE) for selectivity estimation
//! and cardinality estimates. Falls back to legacy heuristics when no stats exist.

pub mod build;
pub mod eligibility;
pub mod join_keys;
pub mod logical_plan;
pub mod logical_planner;
pub mod physical_plan;
pub mod physical_planner;
pub mod rewrite;
pub mod selectivity;
pub mod statistics;
pub mod window_rewrite;

pub use build::BuildContext;
pub use logical_planner::LogicalPlanner;
pub use physical_planner::{PhysicalPlanner, PlanningContext};

/// Build a scope-safe key for schema/stats maps from a table name and optional alias.
///
/// When a query has subqueries, table refs from different scopes are collected
/// into a single flat map. Using only the alias as key would cause collisions
/// when the same alias appears in different scopes for different tables
/// (e.g., `FROM users AS t JOIN (SELECT * FROM orders AS t) AS sub`).
///
/// This function produces a key that is unique per (table_name, alias) pair:
/// - `FROM users` → `"users"`
/// - `FROM users AS t` → `"users\0t"`
/// - Self-join `FROM users AS a JOIN users AS b` → `"users\0a"`, `"users\0b"`
/// - Cross-scope `FROM users AS t ... (SELECT * FROM orders AS t)` → `"users\0t"`, `"orders\0t"`
pub fn schema_map_key(table_name: &str, alias: Option<&str>) -> String {
    match alias {
        Some(a) => format!("{}\0{}", table_name, a),
        None => table_name.to_string(),
    }
}

use crate::sql::analyzer::types::{
    AnalyzedQuery, AnalyzedQueryBody, AnalyzedTableRef, AnalyzedTableRefKind, TableRefSchema,
};
use physical_plan::PhysicalPlan;

// ── Query table-ref collection ────────────────────────────────────────
//
// Recursively walks an `AnalyzedQuery` body (handling Select, SetOperation,
// Values) and collects every leaf table reference.  Used by both execution
// (`execute_via_optimizer`) and EXPLAIN (`statement.rs`) to pre-load table
// schemas and statistics before calling `optimize()`.

/// Collect all leaf table references from an `AnalyzedQuery`, recursively
/// descending into `SetOperation` branches.
///
/// Returns `(table_name, table_ref_schema, optional_alias)` for each leaf.
pub fn collect_query_table_refs(
    query: &AnalyzedQuery,
) -> Vec<(&str, &TableRefSchema, Option<&str>)> {
    let mut refs = Vec::new();
    collect_body_refs(&query.body, &mut refs);
    refs
}

fn collect_body_refs<'a>(
    body: &'a AnalyzedQueryBody,
    refs: &mut Vec<(&'a str, &'a TableRefSchema, Option<&'a str>)>,
) {
    match body {
        AnalyzedQueryBody::Select(select) => {
            for table_ref in &select.from {
                collect_join_tree_refs(table_ref, refs);
            }
        }
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            collect_body_refs(&left.body, refs);
            collect_body_refs(&right.body, refs);
        }
        AnalyzedQueryBody::Values(_) => {}
    }
}

fn collect_join_tree_refs<'a>(
    table_ref: &'a AnalyzedTableRef,
    refs: &mut Vec<(&'a str, &'a TableRefSchema, Option<&'a str>)>,
) {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, schema } => {
            refs.push((name.as_str(), schema, table_ref.alias.as_deref()));
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            collect_join_tree_refs(left, refs);
            collect_join_tree_refs(right, refs);
        }
        AnalyzedTableRefKind::Subquery(subquery) => {
            // Recurse into subquery body to collect inner table refs
            // so their schemas get pre-loaded for the optimizer.
            collect_body_refs(&subquery.body, refs);
        }
        AnalyzedTableRefKind::Function { .. } => {
            // Table functions are pre-loaded separately by preload_table_functions.
        }
    }
}

/// Single optimizer entrypoint: AnalyzedQuery → PhysicalPlan.
///
/// Returns `Err` if the logical planner cannot build a valid plan (e.g.
/// aggregate rewrite failure for unsupported HAVING / ORDER BY patterns).
///
/// Execution (`execute_via_optimizer`) and EXPLAIN (`statement.rs`) both call
/// this function, ensuring they produce identical plans — no drift.
pub fn optimize(
    analyzed: &AnalyzedQuery,
    planning_ctx: &PlanningContext,
) -> anyhow::Result<PhysicalPlan> {
    // Step 1: AnalyzedQuery → LogicalPlan
    let logical = LogicalPlanner::build(analyzed)?;
    // Step 2: Apply rewrite rules (predicate pushdown, etc.)
    let optimized = rewrite::apply_rewrites(logical);
    // Step 3: LogicalPlan → PhysicalPlan (cost-based)
    Ok(PhysicalPlanner::plan(&optimized, planning_ctx))
}
