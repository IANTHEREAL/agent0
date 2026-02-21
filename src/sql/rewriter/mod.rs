//! Post-analysis query rewriter -- subquery flattening for views.
//!
//! View expansion (`expand_views_in_query`) replaces `FROM my_view` with
//! `FROM (SELECT ... FROM base_table WHERE ...) AS my_view` at the AST level.
//! After analysis, this produces `AnalyzedTableRefKind::Subquery`, which the
//! optimizer rejects. This module flattens simple view subqueries back to
//! direct table references, enabling both the optimizer and legacy planner to
//! use index-aware scan strategies.
//!
//! Architecture: `Parser -> View Expansion -> Analyzer -> [Rewriter] -> Optimizer/Executor`

mod flatten;
mod remap;

#[cfg(test)]
mod tests;

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, TypedExpr, TypedExprKind,
    TypedOrderByExpr,
};
use crate::sql::expr::typed_visit::expr_any;

/// Public entry point. Flattens a single simple view subquery in FROM position
/// back to a direct table reference. Returns the query unchanged if it does not
/// meet the conservative flattenable criteria.
pub fn rewrite_query(query: AnalyzedQuery) -> AnalyzedQuery {
    // Criterion 1: no CTEs
    if !query.ctes.is_empty() {
        return query;
    }

    // Criterion 2: body is Select with exactly 1 FROM source
    let AnalyzedQueryBody::Select(ref select) = query.body else {
        return query;
    };
    if select.from.len() != 1 {
        return query;
    }

    // Criterion 3: that FROM source is a Subquery
    let crate::sql::analyzer::types::AnalyzedTableRefKind::Subquery(ref inner_query) =
        select.from[0].kind
    else {
        return query;
    };

    if !can_flatten(inner_query) {
        return query;
    }

    // Criterion 9: no subquery expressions in outer clauses.
    // Subquery bodies (ScalarSubquery, Exists, InSubquery, AnyAll, ArraySubquery)
    // may contain correlated refs (scope_depth > 0) that reference the outer row
    // by column_index. Our remap does not descend into subquery bodies, so after
    // a non-identity column remap those correlated refs would read the wrong
    // outer column -- producing silent wrong results.
    if outer_has_subquery_exprs(select, &query.order_by) {
        return query;
    }

    flatten::flatten_subquery(query)
}

/// Return true if any outer expression (projection, WHERE, GROUP BY, HAVING,
/// DISTINCT ON, ORDER BY) contains a subquery expression node.
fn outer_has_subquery_exprs(select: &AnalyzedSelect, order_by: &[TypedOrderByExpr]) -> bool {
    let is_subquery_node = |e: &TypedExpr| {
        matches!(
            e.kind,
            TypedExprKind::ScalarSubquery(_)
                | TypedExprKind::ArraySubquery(_)
                | TypedExprKind::Exists { .. }
                | TypedExprKind::InSubquery { .. }
                | TypedExprKind::AnyAll { .. }
        )
    };

    for proj in &select.projection {
        if expr_any(&proj.expr, &is_subquery_node) {
            return true;
        }
    }
    if let Some(ref w) = select.where_clause {
        if expr_any(w, &is_subquery_node) {
            return true;
        }
    }
    for expr in &select.group_by {
        if expr_any(expr, &is_subquery_node) {
            return true;
        }
    }
    if let Some(ref h) = select.having {
        if expr_any(h, &is_subquery_node) {
            return true;
        }
    }
    if let AnalyzedDistinct::DistinctOn(ref exprs) = select.distinct {
        for expr in exprs {
            if expr_any(expr, &is_subquery_node) {
                return true;
            }
        }
    }
    for ob in order_by {
        if expr_any(&ob.expr, &is_subquery_node) {
            return true;
        }
    }
    false
}

/// Check whether the inner subquery meets all conservative flattenable criteria.
fn can_flatten(inner: &AnalyzedQuery) -> bool {
    use crate::sql::analyzer::types::AnalyzedTableRefKind;

    // Criterion 4: no CTEs, no LIMIT, no OFFSET, no ORDER BY
    if !inner.ctes.is_empty() {
        return false;
    }
    if inner.limit.is_some() || inner.offset.is_some() {
        return false;
    }
    if !inner.order_by.is_empty() {
        return false;
    }

    // Criterion 5: inner body is Select with no GROUP BY, no HAVING, DISTINCT = All
    let AnalyzedQueryBody::Select(ref inner_select) = inner.body else {
        return false;
    };
    if !inner_select.group_by.is_empty() {
        return false;
    }
    if inner_select.having.is_some() {
        return false;
    }
    if !matches!(inner_select.distinct, AnalyzedDistinct::All) {
        return false;
    }

    // Criterion 6: inner FROM has exactly 1 source, which is Table
    if inner_select.from.len() != 1 {
        return false;
    }
    if !matches!(
        inner_select.from[0].kind,
        AnalyzedTableRefKind::Table { .. }
    ) {
        return false;
    }

    // Criterion 7: all inner projection items are plain ColumnRef { scope_depth: 0 }
    for proj in &inner_select.projection {
        match &proj.expr.kind {
            TypedExprKind::ColumnRef { scope_depth: 0, .. } => {}
            _ => return false,
        }
    }

    // Criterion 8: inner WHERE has no correlated refs (scope_depth > 0)
    if let Some(ref where_expr) = inner_select.where_clause {
        let has_correlated = expr_any(
            where_expr,
            &|e| matches!(e.kind, TypedExprKind::ColumnRef { scope_depth, .. } if scope_depth > 0),
        );
        if has_correlated {
            return false;
        }
    }

    true
}
