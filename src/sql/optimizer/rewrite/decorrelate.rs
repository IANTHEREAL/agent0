//! Subquery decorrelation: EXISTS / NOT EXISTS → SemiJoin / AntiJoin.
//!
//! Transforms correlated `EXISTS (SELECT ... WHERE outer.col = inner.col)`
//! subqueries into SemiJoin / AntiJoin logical nodes that can execute as a
//! single hash join instead of per-row re-execution.
//!
//! **v1 scope:** Pure equi-correlation only. Simple subquery shape (no GROUP BY,
//! HAVING, DISTINCT, LIMIT, ORDER BY, CTEs, windows). Non-eligible patterns
//! remain on existing per-row evaluation path unchanged.

use super::super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use super::{conjuncts_to_predicate, split_conjunction, LogicalRewriteRule};
use crate::model::DataType;
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRefKind,
    BinaryOp, JoinCondition, JoinType, TypedExpr, TypedExprKind,
};

// ── Rewrite rule ────────────────────────────────────────────────

pub(super) struct SubqueryDecorrelation;

impl LogicalRewriteRule for SubqueryDecorrelation {
    fn rewrite(&self, plan: LogicalPlan) -> LogicalPlan {
        decorrelate_recursive(plan)
    }
}

// ── Bottom-up recursive rewrite ─────────────────────────────────

fn decorrelate_recursive(plan: LogicalPlan) -> LogicalPlan {
    // 1. Recurse into all children first (bottom-up).
    let plan = decorrelate_children(plan);

    // 2. If node is Filter, try to decorrelate EXISTS conjuncts.
    match plan.node {
        LogicalNode::Filter { predicate, input } => {
            try_decorrelate_filter(predicate, *input, plan.schema)
        }
        _ => plan,
    }
}

fn decorrelate_children(plan: LogicalPlan) -> LogicalPlan {
    plan.map_children(decorrelate_recursive)
}

/// Try to decorrelate EXISTS/NOT EXISTS conjuncts in a Filter predicate.
fn try_decorrelate_filter(
    predicate: TypedExpr,
    mut current_plan: LogicalPlan,
    filter_schema: PlanSchema,
) -> LogicalPlan {
    let conjuncts = split_conjunction(predicate);
    let mut remaining = Vec::new();

    for conj in conjuncts {
        match &conj.kind {
            TypedExprKind::Exists { subquery, negated } => {
                match try_decorrelate_exists(subquery, &current_plan, *negated) {
                    Ok(new_plan) => {
                        current_plan = new_plan;
                    }
                    Err(_) => {
                        remaining.push(conj);
                    }
                }
            }
            // v1 scope: only EXISTS / NOT EXISTS. All other subquery forms kept as-is.
            _ => {
                remaining.push(conj);
            }
        }
    }

    if remaining.is_empty() {
        current_plan
    } else {
        let pred = conjuncts_to_predicate(remaining);
        LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(current_plan),
            },
            schema: filter_schema,
        }
    }
}

// ── Decorrelation info ──────────────────────────────────────────

struct DecorrelationInfo {
    /// (outer_col_idx, inner_col_idx) pairs for the equi-join condition.
    equi_pairs: Vec<(usize, usize)>,
    /// Non-correlated WHERE conjuncts (scope_depth=0 only).
    inner_only_predicates: Vec<TypedExpr>,
}

// ── Eligibility gate ────────────────────────────────────────────

/// Check if a subquery is eligible for decorrelation.
/// Returns `None` (skip) if any gate rejects.
fn is_decorrelatable(subquery: &AnalyzedQuery, outer_width: usize) -> Option<DecorrelationInfo> {
    // Must be a plain SELECT body
    let select = match &subquery.body {
        AnalyzedQueryBody::Select(s) => s,
        _ => return None,
    };

    // No GROUP BY
    if !select.group_by.is_empty() {
        return None;
    }

    // No HAVING
    if select.having.is_some() {
        return None;
    }

    // No DISTINCT
    if !matches!(select.distinct, AnalyzedDistinct::All) {
        return None;
    }

    // No ORDER BY, LIMIT, OFFSET at query level
    if !subquery.order_by.is_empty() || subquery.limit.is_some() || subquery.offset.is_some() {
        return None;
    }

    // No CTEs
    if !subquery.ctes.is_empty() {
        return None;
    }

    // Non-empty FROM
    if select.from.is_empty() {
        return None;
    }

    // No OUTER JOINs in FROM
    if from_has_outer_join(&select.from) {
        return None;
    }

    // No outer refs in JOIN ON conditions in FROM
    if from_join_on_has_outer_refs(&select.from) {
        return None;
    }

    // Check that ALL outer refs (scope_depth >= 1) appear exclusively in WHERE
    if projection_has_outer_refs(&select.projection) {
        return None;
    }

    // No volatile or catalog-dependent functions anywhere in the subquery tree.
    // Decorrelation changes evaluation from per-row to one-time build-side
    // materialization, which changes evaluation count for volatile functions.
    // Must check entire tree: WHERE, JOIN ON, table-function args, projection,
    // and nested subqueries.
    if subquery_tree_has_volatile_or_catalog(select) {
        return None;
    }

    // Must have WHERE clause with correlated predicates
    let where_clause = match &select.where_clause {
        Some(w) => w,
        None => return None,
    };

    // Extract correlation equi-pairs from WHERE
    let conjuncts = split_conjunction(where_clause.clone());
    extract_correlation_equi_pairs(&conjuncts, outer_width)
}

/// Check if FROM clause contains any OUTER JOIN (Left/Right/Full).
fn from_has_outer_join(from: &[crate::sql::analyzer::types::AnalyzedTableRef]) -> bool {
    for table_ref in from {
        if table_ref_has_outer_join(&table_ref.kind) {
            return true;
        }
    }
    false
}

fn table_ref_has_outer_join(kind: &AnalyzedTableRefKind) -> bool {
    match kind {
        AnalyzedTableRefKind::Join {
            left,
            right,
            join_type,
            ..
        } => {
            matches!(join_type, JoinType::Left | JoinType::Right | JoinType::Full)
                || table_ref_has_outer_join(&left.kind)
                || table_ref_has_outer_join(&right.kind)
        }
        AnalyzedTableRefKind::Subquery(_) => false,
        AnalyzedTableRefKind::Table { .. } | AnalyzedTableRefKind::Function { .. } => false,
    }
}

/// Check if any JOIN ON condition in FROM has outer refs (scope_depth >= 1).
fn from_join_on_has_outer_refs(from: &[crate::sql::analyzer::types::AnalyzedTableRef]) -> bool {
    for table_ref in from {
        if table_ref_join_on_has_outer_refs(&table_ref.kind) {
            return true;
        }
    }
    false
}

fn table_ref_join_on_has_outer_refs(kind: &AnalyzedTableRefKind) -> bool {
    match kind {
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            if let JoinCondition::On(expr) = condition {
                if expr_has_outer_refs_beyond(expr, 0) {
                    return true;
                }
            }
            table_ref_join_on_has_outer_refs(&left.kind)
                || table_ref_join_on_has_outer_refs(&right.kind)
        }
        _ => false,
    }
}

/// Check if any projection item has outer refs (depth-aware, crosses subquery boundaries).
fn projection_has_outer_refs(
    projection: &[crate::sql::analyzer::types::AnalyzedProjection],
) -> bool {
    projection
        .iter()
        .any(|p| expr_has_outer_refs_beyond(&p.expr, 0))
}

/// Check the full subquery tree for volatile or catalog-dependent functions.
///
/// Decorrelation changes evaluation from per-row to one-time build-side
/// materialization. Volatile functions (e.g. `random()`) and catalog-dependent
/// functions can produce different results when evaluation count changes.
///
/// **Crosses all subquery boundaries** — walks into expression-level subquery
/// payloads (EXISTS, IN, scalar subquery, etc.) and derived-table subqueries
/// in FROM, covering all query body types (Select, SetOperation, Values).
fn subquery_tree_has_volatile_or_catalog(select: &AnalyzedSelect) -> bool {
    // Check WHERE
    if let Some(ref w) = select.where_clause {
        if expr_has_volatile_or_catalog_deep(w) {
            return true;
        }
    }
    // Check projection
    for p in &select.projection {
        if expr_has_volatile_or_catalog_deep(&p.expr) {
            return true;
        }
    }
    // Check FROM tree (JOIN ON conditions, table-function args, derived subqueries)
    for table_ref in &select.from {
        if table_ref_has_volatile_or_catalog(&table_ref.kind) {
            return true;
        }
    }
    // GROUP BY / HAVING (already rejected by eligibility gates, but be safe)
    for g in &select.group_by {
        if expr_has_volatile_or_catalog_deep(g) {
            return true;
        }
    }
    if let Some(ref h) = select.having {
        if expr_has_volatile_or_catalog_deep(h) {
            return true;
        }
    }
    false
}

/// Expression-level volatile/catalog check that **crosses subquery boundaries**.
///
/// Unlike `is_volatile`/`has_catalog_dependent_function` from classify.rs (which
/// use `visit_any` and stop at subquery payloads), this function descends into
/// nested AnalyzedQuery payloads to detect volatile/catalog functions anywhere
/// in the expression tree.
fn expr_has_volatile_or_catalog_deep(expr: &TypedExpr) -> bool {
    use crate::sql::analyzer::types::FunctionKind;
    use crate::sql::expr::typed_fold::is_volatile_or_side_effecting_builtin;

    crate::sql::expr::traverse::visit_any(expr, |e| match &e.kind {
        // Volatile function check (same logic as classify::is_volatile)
        TypedExprKind::FunctionCall { func, .. } => match func.kind {
            FunctionKind::Builtin => {
                is_volatile_or_side_effecting_builtin(&func.name)
                    || is_catalog_dependent_function_check(&func.kind, &func.name)
            }
            FunctionKind::UserDefined { .. } => true, // conservative: UDFs are volatile
        },
        TypedExprKind::AggregateCall { func, .. } | TypedExprKind::WindowCall { func, .. } => {
            is_catalog_dependent_function_check(&func.kind, &func.name)
        }
        // Cross subquery boundaries — descend into AnalyzedQuery payloads
        TypedExprKind::ScalarSubquery(q) | TypedExprKind::ArraySubquery(q) => {
            query_has_volatile_or_catalog(q)
        }
        TypedExprKind::Exists { subquery, .. }
        | TypedExprKind::InSubquery { subquery, .. }
        | TypedExprKind::AnyAll { subquery, .. } => query_has_volatile_or_catalog(subquery),
        _ => false,
    })
}

/// Thin wrapper around classify.rs's private `is_catalog_dependent_function`.
///
/// Duplicates the check because the original is module-private (fn, not pub).
/// The pub wrappers in classify.rs (`has_catalog_dependent_function`) use
/// `visit_any` which doesn't cross subquery boundaries.
fn is_catalog_dependent_function_check(
    func_kind: &crate::sql::analyzer::types::FunctionKind,
    name: &str,
) -> bool {
    use crate::sql::analyzer::types::FunctionKind;

    if matches!(func_kind, FunctionKind::UserDefined { .. }) {
        return true;
    }
    if name.eq_ignore_ascii_case("PG_SLEEP") {
        return true;
    }
    if name.eq_ignore_ascii_case("PG_GET_INDEXDEF")
        || name.eq_ignore_ascii_case("PG_GET_CONSTRAINTDEF")
        || name.eq_ignore_ascii_case("FORMAT_TYPE")
    {
        return true;
    }
    if crate::sql::executor::split_cron_scalar_function_name(name).is_some() {
        return true;
    }
    if crate::sql::executor::is_bg_sql_function(name) {
        return true;
    }
    crate::sql::advisory_locks::is_advisory_lock_function(name)
}

/// Query-level volatile/catalog check — walks all clauses and all body types.
fn query_has_volatile_or_catalog(query: &AnalyzedQuery) -> bool {
    // CTEs
    if query
        .ctes
        .iter()
        .any(|cte| query_has_volatile_or_catalog(&cte.query))
    {
        return true;
    }
    // ORDER BY
    if query
        .order_by
        .iter()
        .any(|o| expr_has_volatile_or_catalog_deep(&o.expr))
    {
        return true;
    }
    // LIMIT / OFFSET
    if let Some(ref l) = query.limit {
        if expr_has_volatile_or_catalog_deep(l) {
            return true;
        }
    }
    if let Some(ref o) = query.offset {
        if expr_has_volatile_or_catalog_deep(o) {
            return true;
        }
    }

    match &query.body {
        AnalyzedQueryBody::Select(select) => subquery_tree_has_volatile_or_catalog(select),
        AnalyzedQueryBody::Values(rows) => {
            rows.iter().flatten().any(expr_has_volatile_or_catalog_deep)
        }
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            query_has_volatile_or_catalog(left) || query_has_volatile_or_catalog(right)
        }
    }
}

/// Check a FROM table ref tree for volatile or catalog-dependent functions.
fn table_ref_has_volatile_or_catalog(kind: &AnalyzedTableRefKind) -> bool {
    match kind {
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            if let JoinCondition::On(expr) = condition {
                if expr_has_volatile_or_catalog_deep(expr) {
                    return true;
                }
            }
            table_ref_has_volatile_or_catalog(&left.kind)
                || table_ref_has_volatile_or_catalog(&right.kind)
        }
        AnalyzedTableRefKind::Function { args, .. } => args.iter().any(|arg| {
            let expr = match arg {
                crate::sql::analyzer::types::TypedFunctionArg::Positional(e) => e,
                crate::sql::analyzer::types::TypedFunctionArg::Named { expr, .. } => expr,
            };
            expr_has_volatile_or_catalog_deep(expr)
        }),
        AnalyzedTableRefKind::Subquery(query) => query_has_volatile_or_catalog(query),
        AnalyzedTableRefKind::Table { .. } => false,
    }
}

/// Check if an expression contains any ColumnRef with scope_depth >= 1.
///
/// **Crosses subquery boundaries** with depth-aware accounting: inside a nested
/// subquery, scope_depth must exceed min_depth+1 to count as an outer ref
/// (scope_depth == min_depth+1 points to the subquery's own enclosing scope,
/// not beyond). This prevents nested subqueries from silently hiding transitive
/// outer refs.
fn expr_has_outer_refs(expr: &TypedExpr) -> bool {
    expr_has_outer_refs_beyond(expr, 0)
}

/// Depth-aware outer-ref detection that crosses subquery boundaries.
///
/// `min_depth` is the scope boundary of the "current" context. A ColumnRef
/// with `scope_depth > min_depth` reaches beyond the current scope. When
/// descending into subquery payloads we increment `min_depth` by 1.
fn expr_has_outer_refs_beyond(expr: &TypedExpr, min_depth: u32) -> bool {
    // Use visit_any for expression-level traversal (does NOT cross subquery
    // boundaries), but intercept subquery nodes explicitly.
    crate::sql::expr::traverse::visit_any(expr, |e| match &e.kind {
        TypedExprKind::ColumnRef { scope_depth, .. } => *scope_depth > min_depth,
        // Subquery payloads: cross boundary with incremented depth
        TypedExprKind::ScalarSubquery(q) | TypedExprKind::ArraySubquery(q) => {
            query_has_outer_refs_beyond(q, min_depth + 1)
        }
        TypedExprKind::Exists { subquery, .. }
        | TypedExprKind::InSubquery { subquery, .. }
        | TypedExprKind::AnyAll { subquery, .. } => {
            query_has_outer_refs_beyond(subquery, min_depth + 1)
        }
        _ => false,
    })
}

/// Check if an AnalyzedQuery has any ColumnRef with scope_depth > min_depth,
/// recursing into all clauses, CTEs, and nested subqueries.
fn query_has_outer_refs_beyond(query: &AnalyzedQuery, min_depth: u32) -> bool {
    // CTEs
    if query
        .ctes
        .iter()
        .any(|cte| query_has_outer_refs_beyond(&cte.query, min_depth))
    {
        return true;
    }
    // ORDER BY
    if query
        .order_by
        .iter()
        .any(|o| expr_has_outer_refs_beyond(&o.expr, min_depth))
    {
        return true;
    }
    // LIMIT / OFFSET
    if let Some(ref l) = query.limit {
        if expr_has_outer_refs_beyond(l, min_depth) {
            return true;
        }
    }
    if let Some(ref o) = query.offset {
        if expr_has_outer_refs_beyond(o, min_depth) {
            return true;
        }
    }

    match &query.body {
        AnalyzedQueryBody::Select(select) => {
            // FROM (including JOIN ON conditions and subquery table refs)
            if select
                .from
                .iter()
                .any(|tr| table_ref_has_outer_refs_beyond(tr, min_depth))
            {
                return true;
            }
            // WHERE
            if let Some(ref w) = select.where_clause {
                if expr_has_outer_refs_beyond(w, min_depth) {
                    return true;
                }
            }
            // Projection
            if select
                .projection
                .iter()
                .any(|p| expr_has_outer_refs_beyond(&p.expr, min_depth))
            {
                return true;
            }
            // GROUP BY
            if select
                .group_by
                .iter()
                .any(|e| expr_has_outer_refs_beyond(e, min_depth))
            {
                return true;
            }
            // HAVING
            if let Some(ref h) = select.having {
                if expr_has_outer_refs_beyond(h, min_depth) {
                    return true;
                }
            }
            false
        }
        AnalyzedQueryBody::Values(rows) => rows
            .iter()
            .flatten()
            .any(|e| expr_has_outer_refs_beyond(e, min_depth)),
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            query_has_outer_refs_beyond(left, min_depth)
                || query_has_outer_refs_beyond(right, min_depth)
        }
    }
}

/// Check table refs (including nested joins and subquery table refs) for outer refs.
fn table_ref_has_outer_refs_beyond(
    table_ref: &crate::sql::analyzer::types::AnalyzedTableRef,
    min_depth: u32,
) -> bool {
    match &table_ref.kind {
        AnalyzedTableRefKind::Subquery(query) => {
            // Derived-table subquery introduces a scope boundary
            query_has_outer_refs_beyond(query, min_depth + 1)
        }
        AnalyzedTableRefKind::Function { args, .. } => args.iter().any(|arg| match arg {
            crate::sql::analyzer::types::TypedFunctionArg::Positional(expr) => {
                expr_has_outer_refs_beyond(expr, min_depth)
            }
            crate::sql::analyzer::types::TypedFunctionArg::Named { expr, .. } => {
                expr_has_outer_refs_beyond(expr, min_depth)
            }
        }),
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            table_ref_has_outer_refs_beyond(left, min_depth)
                || table_ref_has_outer_refs_beyond(right, min_depth)
                || matches!(condition, JoinCondition::On(expr) if expr_has_outer_refs_beyond(expr, min_depth))
        }
        AnalyzedTableRefKind::Table { .. } => false,
    }
}

/// Extract equi-join pairs from WHERE conjuncts.
///
/// For each conjunct, check if it's `ColumnRef{scope_depth=1} = ColumnRef{scope_depth=0}`
/// (or commuted). Returns `Some(DecorrelationInfo)` if all correlating predicates are
/// pure equi, or `None` if any is non-equi or has nested correlation (scope_depth > 1).
fn extract_correlation_equi_pairs(
    conjuncts: &[TypedExpr],
    _outer_width: usize,
) -> Option<DecorrelationInfo> {
    let mut equi_pairs = Vec::new();
    let mut inner_only = Vec::new();
    let mut has_any_correlation = false;

    for conj in conjuncts {
        if !expr_has_outer_refs(conj) {
            // Non-correlated predicate — will become inner filter
            inner_only.push(conj.clone());
            continue;
        }

        // Must be equi-join between outer (scope_depth=1) and inner (scope_depth=0)
        match &conj.kind {
            TypedExprKind::BinaryOp {
                left,
                op: BinaryOp::Eq,
                right,
            } => {
                match try_extract_equi_pair(left, right) {
                    Some(pair) => {
                        equi_pairs.push(pair);
                        has_any_correlation = true;
                    }
                    None => {
                        // Non-equi or complex correlation — reject entirely
                        return None;
                    }
                }
            }
            _ => {
                // Non-equi correlation (e.g. outer.x > inner.y) — reject
                return None;
            }
        }
    }

    // Must have at least one equi correlation
    if !has_any_correlation {
        return None;
    }

    // Check for nested correlation (scope_depth > 1)
    for conj in conjuncts {
        if expr_has_deeply_correlated_refs(conj) {
            return None;
        }
    }

    Some(DecorrelationInfo {
        equi_pairs,
        inner_only_predicates: inner_only,
    })
}

/// Try to extract an equi-pair from `left = right`.
/// Returns `Some((outer_col_idx, inner_col_idx))` if one side is scope_depth=1
/// and the other is scope_depth=0.
fn try_extract_equi_pair(left: &TypedExpr, right: &TypedExpr) -> Option<(usize, usize)> {
    let (l_depth, l_idx) = extract_col_ref(left)?;
    let (r_depth, r_idx) = extract_col_ref(right)?;

    if l_depth == 1 && r_depth == 0 {
        Some((l_idx, r_idx))
    } else if l_depth == 0 && r_depth == 1 {
        Some((r_idx, l_idx))
    } else {
        None
    }
}

fn extract_col_ref(expr: &TypedExpr) -> Option<(u32, usize)> {
    match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            ..
        } => Some((*scope_depth, *column_index)),
        _ => None,
    }
}

/// Check if an expression has any ColumnRef with scope_depth > 1 (nested correlation).
///
/// Uses depth-aware traversal that crosses subquery boundaries.
fn expr_has_deeply_correlated_refs(expr: &TypedExpr) -> bool {
    expr_has_outer_refs_beyond(expr, 1)
}

// ── The decorrelation transform ─────────────────────────────────

fn try_decorrelate_exists(
    subquery: &AnalyzedQuery,
    input: &LogicalPlan,
    negated: bool,
) -> Result<LogicalPlan, ()> {
    let outer_width = input.schema.columns.len();

    let info = is_decorrelatable(subquery, outer_width).ok_or(())?;

    // Build inner plan from the subquery's FROM clause
    let select = match &subquery.body {
        AnalyzedQueryBody::Select(s) => s,
        _ => return Err(()),
    };

    let inner_plan =
        crate::sql::optimizer::logical_planner::nodes::build_from(&select.from).map_err(|_| ())?;

    // Apply inner-only predicates as Filter on inner plan
    let inner_plan = if info.inner_only_predicates.is_empty() {
        inner_plan
    } else {
        let pred = conjuncts_to_predicate(info.inner_only_predicates);
        inner_plan.filter(pred)
    };

    // Build equi-join condition from equi_pairs using real schema metadata
    let condition = build_equi_condition(
        &info.equi_pairs,
        outer_width,
        &input.schema,
        &inner_plan.schema,
    );

    // Return semi_join or anti_join based on negated
    if negated {
        Ok(input.clone().anti_join(inner_plan, condition))
    } else {
        Ok(input.clone().semi_join(inner_plan, condition))
    }
}

/// Build a JoinCondition::On from equi_pairs.
///
/// Each pair (outer_col_idx, inner_col_idx) becomes:
/// `ColumnRef{scope=0, idx=outer_col_idx} = ColumnRef{scope=0, idx=outer_width + inner_col_idx}`
///
/// Types and names are derived from the left (outer) and right (inner) schemas.
fn build_equi_condition(
    equi_pairs: &[(usize, usize)],
    outer_width: usize,
    outer_schema: &PlanSchema,
    inner_schema: &PlanSchema,
) -> JoinCondition {
    let mut exprs: Vec<TypedExpr> = Vec::with_capacity(equi_pairs.len());

    for &(outer_idx, inner_idx) in equi_pairs {
        // Derive real type and name from schemas
        let (left_name, left_type) = outer_schema
            .columns
            .get(outer_idx)
            .map(|(n, t)| (n.clone(), t.clone()))
            .unwrap_or_else(|| (format!("col_{}", outer_idx), DataType::Text));
        let (right_name, right_type) = inner_schema
            .columns
            .get(inner_idx)
            .map(|(n, t)| (n.clone(), t.clone()))
            .unwrap_or_else(|| (format!("col_{}", inner_idx), DataType::Text));

        let left_ref = TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: outer_idx,
                column_name: left_name,
            },
            data_type: left_type,
        };
        let right_ref = TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: outer_width + inner_idx,
                column_name: right_name,
            },
            data_type: right_type,
        };
        exprs.push(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left_ref),
                op: BinaryOp::Eq,
                right: Box::new(right_ref),
            },
            data_type: DataType::Boolean,
        });
    }

    JoinCondition::On(conjuncts_to_predicate(exprs))
}
