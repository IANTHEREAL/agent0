//! Optimizer eligibility — shared by execution and EXPLAIN routing.
//!
//! Determines whether an `AnalyzedQuery` can be routed through the CBO
//! pipeline. Execution-side routing (`mod.rs:111`) adds its own additional
//! guards (locks, SELECT INTO) — this helper covers query shape + expression
//! safety only.

use std::collections::HashSet;

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, JoinCondition,
};
use crate::sql::expr::classify::needs_async;
use crate::sql::optimizer::logical_planner::expr_has_aggregate;

/// Check whether a query is eligible for the CBO optimizer pipeline.
///
/// Eligibility covers:
/// - SELECT body (with real tables, no subquery/function leaves, no async exprs)
/// - SetOperation body (UNION/INTERSECT/EXCEPT) when all branches are
///   individually eligible (recursive check)
/// - CTEs (WITH clauses): CTE names are threaded through to prevent
///   false rejection by the virtual catalog table check
/// - FROM non-empty for SELECT branches; may be a single join tree
///   (`A JOIN B`) OR multiple comma-separated tables (`FROM a, b`)
/// - No expression position contains async expressions (subqueries,
///   catalog-dependent functions)
pub fn is_optimizer_eligible(analyzed: &AnalyzedQuery) -> bool {
    is_eligible_inner(analyzed, &HashSet::new())
}

/// Inner eligibility check that threads CTE names from parent to child queries.
///
/// `inherited_cte_names` carries CTE names defined at outer query levels,
/// ensuring that CTE table references in SetOperation branches or nested
/// scopes are not falsely rejected as virtual catalog tables.
fn is_eligible_inner(analyzed: &AnalyzedQuery, inherited_cte_names: &HashSet<String>) -> bool {
    // Merge this query's CTE names with inherited ones (case-insensitive).
    let mut cte_names = inherited_cte_names.clone();
    for cte in &analyzed.ctes {
        cte_names.insert(cte.name.to_lowercase());
    }

    let select = match &analyzed.body {
        AnalyzedQueryBody::Select(select) => select,
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            // Recursively check each branch independently, threading CTE names.
            // Branches containing VALUES, TableFunction, or subquery leaves
            // will be rejected by the recursive check (they hit hard errors
            // at build.rs Values/TableFunction handlers).
            if !is_eligible_inner(left, &cte_names) || !is_eligible_inner(right, &cte_names) {
                return false;
            }
            // Check top-level ORDER BY for async expressions.
            for ob in &analyzed.order_by {
                if needs_async(&ob.expr) {
                    return false;
                }
            }
            return true;
        }
        // Values not supported in optimizer build (build.rs would fail).
        AnalyzedQueryBody::Values(_) => return false,
    };

    // Must have at least one FROM item.
    if select.from.is_empty() {
        return false;
    }

    // All leaf table refs must be simple Tables (no subquery, no function).
    // Handles both single join trees and comma-separated FROM items.
    // CTE names are passed through to avoid false rejection when a CTE
    // shadows a virtual catalog table name (e.g. WITH pg_type AS (...)).
    if !select
        .from
        .iter()
        .all(|tr| all_leaves_are_tables(tr, &cte_names))
    {
        return false;
    }

    // Reject if any expression position contains async expressions.
    if has_any_async_expr(select, analyzed) {
        return false;
    }

    // Reject window functions in DISTINCT ON — PostgreSQL does not allow
    // window functions outside SELECT list and ORDER BY.
    if let AnalyzedDistinct::DistinctOn(on_exprs) = &select.distinct {
        if on_exprs
            .iter()
            .any(|e| super::window_rewrite::contains_window(e))
        {
            return false;
        }
    }

    // Reject aggregate queries whose ORDER BY / HAVING / DISTINCT ON cannot be rewritten
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

/// Check whether all ORDER BY / HAVING / DISTINCT ON expressions in an
/// aggregate query can be rewritten to reference post-aggregate column
/// positions.
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

    // Trial-rewrite DISTINCT ON expressions.
    if let AnalyzedDistinct::DistinctOn(on_exprs) = &select.distinct {
        for expr in on_exprs {
            if super::build::rewrite_post_aggregate_expr(expr, group_by, group_by_count, &agg_exprs)
                .is_err()
            {
                return false;
            }
        }
    }

    true
}

/// Recursively check that all leaves in a table ref tree are `Table` variants
/// backed by real KV-persisted schemas or CTE definitions (not virtual catalog
/// tables). CTE names are checked first to prevent false rejection when a CTE
/// shadows a catalog table name (e.g. `WITH pg_type AS (...)`).
fn all_leaves_are_tables(table_ref: &AnalyzedTableRef, cte_names: &HashSet<String>) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, .. } => {
            // CTE names are valid — resolved from ExecutionContext::cte_tables at runtime.
            cte_names.contains(&name.to_lowercase()) || !is_virtual_catalog_table(name)
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            all_leaves_are_tables(left, cte_names) && all_leaves_are_tables(right, cte_names)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{
        AnalyzedCte, AnalyzedProjection, FunctionKind, IsTestKind, JsonAccessOp, ResolvedFunction,
        TableRefSchema, TypedExpr, TypedExprKind,
    };
    use crate::types::DataType;

    // ── helpers ─────────────────────────────────────────────

    fn window_call() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::WindowCall {
                func: ResolvedFunction {
                    name: "row_number".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                partition_by: vec![],
                order_by: vec![],
                window_frame: None,
            },
            data_type: DataType::Int64,
        }
    }

    fn col_ref(idx: usize, name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: idx,
                column_name: name.to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn const_int() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(crate::types::Value::Int64(1)),
            data_type: DataType::Int64,
        }
    }

    fn const_text() -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(crate::types::Value::Text("a".to_string())),
            data_type: DataType::Text,
        }
    }

    fn simple_table_ref() -> AnalyzedTableRef {
        AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Table {
                name: "t".to_string(),
                schema: TableRefSchema {
                    table_id: 1,
                    columns: vec![("id".to_string(), DataType::Int64, false)],
                },
            },
            alias: None,
        }
    }

    /// Build a minimal eligible query with the given DISTINCT mode.
    fn query_with_distinct(distinct: AnalyzedDistinct) -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: col_ref(0, "id"),
                    output_name: "id".to_string(),
                }],
                from: vec![simple_table_ref()],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        }
    }

    // ── baseline ────────────────────────────────────────────

    #[test]
    fn test_simple_select_eligible() {
        let q = query_with_distinct(AnalyzedDistinct::All);
        assert!(is_optimizer_eligible(&q));
    }

    // ── DISTINCT ON + window in every wrapper variant ───────

    #[test]
    fn test_distinct_on_bare_window_rejected() {
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![window_call()]));
        assert!(!is_optimizer_eligible(&q), "bare window in DISTINCT ON");
    }

    #[test]
    fn test_distinct_on_window_in_is_test() {
        let expr = TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(window_call()),
                test: IsTestKind::Null,
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in IsTest");
    }

    #[test]
    fn test_distinct_on_window_in_between() {
        let expr = TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(col_ref(0, "id")),
                low: Box::new(window_call()),
                high: Box::new(const_int()),
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in Between");
    }

    #[test]
    fn test_distinct_on_window_in_in_list() {
        let expr = TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(col_ref(0, "id")),
                list: vec![const_int(), window_call()],
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in InList");
    }

    #[test]
    fn test_distinct_on_window_in_like() {
        let expr = TypedExpr {
            kind: TypedExprKind::Like {
                expr: Box::new(window_call()),
                pattern: Box::new(const_text()),
                escape: None,
                case_insensitive: false,
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in Like");
    }

    #[test]
    fn test_distinct_on_window_in_similar_to() {
        let expr = TypedExpr {
            kind: TypedExprKind::SimilarTo {
                expr: Box::new(col_ref(0, "id")),
                pattern: Box::new(const_text()),
                escape: Some(Box::new(window_call())),
                negated: false,
            },
            data_type: DataType::Boolean,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in SimilarTo escape");
    }

    #[test]
    fn test_distinct_on_window_in_array_literal() {
        let expr = TypedExpr {
            kind: TypedExprKind::ArrayLiteral(vec![const_int(), window_call()]),
            data_type: DataType::Text,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in ArrayLiteral");
    }

    #[test]
    fn test_distinct_on_window_in_row() {
        let expr = TypedExpr {
            kind: TypedExprKind::Row(vec![window_call(), col_ref(0, "id")]),
            data_type: DataType::Text,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in Row");
    }

    #[test]
    fn test_distinct_on_window_in_array_index() {
        let expr = TypedExpr {
            kind: TypedExprKind::ArrayIndex {
                array: Box::new(col_ref(0, "arr")),
                index: Box::new(window_call()),
            },
            data_type: DataType::Int64,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in ArrayIndex");
    }

    #[test]
    fn test_distinct_on_window_in_json_access() {
        let expr = TypedExpr {
            kind: TypedExprKind::JsonAccess {
                expr: Box::new(window_call()),
                path: Box::new(const_text()),
                operator: JsonAccessOp::Arrow,
            },
            data_type: DataType::Text,
        };
        let q = query_with_distinct(AnalyzedDistinct::DistinctOn(vec![expr]));
        assert!(!is_optimizer_eligible(&q), "window in JsonAccess");
    }

    // ── CTE shadow: eligibility still passes (CTE names threaded) ──

    #[test]
    fn test_cte_shadowing_real_table_still_eligible() {
        // WITH pg_type AS (SELECT 1 AS id FROM t) SELECT * FROM pg_type
        // The CTE name "pg_type" shadows a virtual catalog table but should
        // remain eligible because CTE names are threaded through.
        let inner_query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: const_int(),
                    output_name: "id".to_string(),
                }],
                from: vec![simple_table_ref()],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };

        let q = AnalyzedQuery {
            ctes: vec![AnalyzedCte {
                name: "pg_type".to_string(),
                query: inner_query,
                columns: vec![("id".to_string(), DataType::Int64)],
                materialized: None,
            }],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: col_ref(0, "id"),
                    output_name: "id".to_string(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "pg_type".to_string(),
                        schema: TableRefSchema {
                            table_id: 99,
                            columns: vec![("id".to_string(), DataType::Int64, false)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };
        assert!(
            is_optimizer_eligible(&q),
            "CTE shadowing pg_type should still be eligible"
        );
    }
}
