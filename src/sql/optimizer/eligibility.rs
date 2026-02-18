//! Optimizer eligibility — shared by execution and EXPLAIN routing.
//!
//! Determines whether an `AnalyzedQuery` can be routed through the CBO
//! pipeline.  The caller is expected to have pre-materialized non-correlated
//! async expressions before calling this check, so the only remaining
//! ineligibility reasons are structural: aggregate ORDER BY/HAVING rewrite
//! failures and window functions in DISTINCT ON.

use std::collections::HashSet;

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, JoinCondition,
};
use crate::sql::expr::classify::has_unresolved_subquery;
use crate::sql::optimizer::logical_planner::expr_has_aggregate;

/// Check whether a query is eligible for the CBO optimizer pipeline.
///
/// After pre-materialization of async expressions by the caller, the
/// optimizer handles all query shapes: single/multi-table, joins, set ops,
/// CTEs, VALUES, tableless SELECT (empty FROM), table functions, virtual
/// catalog tables, subquery FROM, and remaining async expressions (handled
/// by post-processing).
///
/// The only remaining ineligibility reasons are:
/// - Aggregate ORDER BY / HAVING / DISTINCT ON expressions that cannot be
///   rewritten to post-aggregate column positions
/// - Window functions in DISTINCT ON (PostgreSQL constraint)
pub fn is_optimizer_eligible(analyzed: &AnalyzedQuery) -> bool {
    is_eligible_inner(analyzed, &HashSet::new())
}

/// Inner eligibility check that threads CTE names from parent to child queries.
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
            if !is_eligible_inner(left, &cte_names) || !is_eligible_inner(right, &cte_names) {
                return false;
            }
            return true;
        }
        // VALUES body is now supported by the optimizer.
        AnalyzedQueryBody::Values(_) => return true,
    };

    // Empty FROM (tableless SELECT like `SELECT 1+1`) is handled by
    // the logical planner via LogicalNode::Empty.

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

    // Reject queries with subquery-derived tables in FROM — the optimizer's
    // collect_query_table_refs / logical planner don't handle these yet.
    if has_subquery_from_leaf(select) {
        return false;
    }

    // Reject queries with unresolved subquery expressions in JOIN ON conditions.
    // Non-correlated subqueries in WHERE/projection are handled by
    // pre-materialization + post-processing, but JOIN ON subqueries (especially
    // correlated ones) can't be pre-materialized and the operator tree can't
    // evaluate them per-row.
    if has_subquery_in_join_on(select) {
        return false;
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

fn has_subquery_from_leaf(select: &AnalyzedSelect) -> bool {
    select.from.iter().any(|tr| table_ref_has_subquery(tr))
}

fn table_ref_has_subquery(tr: &AnalyzedTableRef) -> bool {
    match &tr.kind {
        AnalyzedTableRefKind::Subquery(_) => true,
        AnalyzedTableRefKind::Join { left, right, .. } => {
            table_ref_has_subquery(left) || table_ref_has_subquery(right)
        }
        AnalyzedTableRefKind::Table { .. } | AnalyzedTableRefKind::Function { .. } => false,
    }
}

fn has_subquery_in_join_on(select: &AnalyzedSelect) -> bool {
    select.from.iter().any(|tr| join_on_has_subquery(tr))
}

fn join_on_has_subquery(tr: &AnalyzedTableRef) -> bool {
    match &tr.kind {
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            if let JoinCondition::On(expr) = condition {
                if has_unresolved_subquery(expr) {
                    return true;
                }
            }
            join_on_has_subquery(left) || join_on_has_subquery(right)
        }
        _ => false,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{
        AnalyzedCte, AnalyzedProjection, AnalyzedTableRef, AnalyzedTableRefKind, FunctionKind,
        IsTestKind, JsonAccessOp, ResolvedFunction, TableRefSchema, TypedExpr, TypedExprKind,
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
