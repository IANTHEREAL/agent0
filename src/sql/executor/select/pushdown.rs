use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GenerateSeriesOffsetLimitPushdownPlan {
    pub(super) offset: usize,
    pub(super) limit: Option<usize>,
    /// When true, the executor must clear query-level OFFSET/LIMIT/FETCH so the slice
    /// is not applied twice.
    pub(super) clear_query_offset_limit_fetch: bool,
}

pub(super) fn generate_series_offset_limit_pushdown_eligible(
    query: &Query,
    select: &sqlparser::ast::Select,
) -> bool {
    let group_by_is_empty = matches!(
        &select.group_by,
        GroupByExpr::Expressions(exprs) if exprs.is_empty()
    );
    let has_window_funcs = !extract_window_functions(&select.projection).is_empty();
    let has_agg_funcs = {
        let extra_start = select.projection.len();
        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    collect_having_agg_funcs(expr, &mut agg_funcs, extra_start);
                }
                _ => {}
            }
        }
        !agg_funcs.is_empty()
    };

    let eligible_shape = select.selection.is_none()
        && select.distinct.is_none()
        && group_by_is_empty
        && select.having.is_none()
        && query.order_by.is_empty()
        && !has_window_funcs
        && !has_agg_funcs
        && (query.offset.is_some() || query.limit.is_some() || query.fetch.is_some());
    eligible_shape && !analysis::projection_has_select_list_srf(&select.projection)
}

pub(super) fn normalize_query_offset_limit_fetch_expressions(query: &Query) -> Query {
    let value_to_usize = |v: Value| -> Option<usize> {
        match v {
            Value::Int64(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Int32(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| usize::try_from(n).ok()),
            _ => None,
        }
    };

    let mut q = query.clone();

    if let Some(offset) = q.offset.as_mut() {
        if let Ok(v) = eval_expr(&offset.value, None, None) {
            if let Some(n) = value_to_usize(v) {
                offset.value = Expr::Value(SqlValue::Number(n.to_string(), false));
            }
        }
    }

    if let Some(limit) = q.limit.as_mut() {
        if let Ok(v) = eval_expr(limit, None, None) {
            if let Some(n) = value_to_usize(v) {
                *limit = Expr::Value(SqlValue::Number(n.to_string(), false));
            }
        }
    }

    if let Some(fetch) = q.fetch.as_mut() {
        if let Some(quantity) = fetch.quantity.as_mut() {
            if let Ok(v) = eval_expr(quantity, None, None) {
                if let Some(n) = value_to_usize(v) {
                    *quantity = Expr::Value(SqlValue::Number(n.to_string(), false));
                }
            }
        }
    }

    q
}

pub(super) fn plan_generate_series_offset_limit_pushdown(
    query: &Query,
    select: &sqlparser::ast::Select,
) -> GenerateSeriesOffsetLimitPushdownPlan {
    if !generate_series_offset_limit_pushdown_eligible(query, select) {
        return GenerateSeriesOffsetLimitPushdownPlan {
            offset: 0,
            limit: None,
            clear_query_offset_limit_fetch: false,
        };
    }

    let value_to_usize = |v: Value| -> Option<usize> {
        match v {
            Value::Int64(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Int32(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| usize::try_from(n).ok()),
            _ => None,
        }
    };

    let mut offset = 0usize;
    if let Some(offset_expr) = &query.offset {
        if let Ok(v) = eval_expr(&offset_expr.value, None, None) {
            offset = value_to_usize(v).unwrap_or(0);
        }
    }

    let mut limit_n = usize::MAX;
    if let Some(limit_expr) = &query.limit {
        if let Ok(v) = eval_expr(limit_expr, None, None) {
            limit_n = value_to_usize(v).unwrap_or(usize::MAX);
        }
    }

    let mut fetch_n = usize::MAX;
    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            if let Ok(v) = eval_expr(quantity, None, None) {
                fetch_n = value_to_usize(v).unwrap_or(1);
            }
        } else {
            fetch_n = 1;
        }
    }

    let effective_limit = limit_n.min(fetch_n);

    // OFFSET pushdown is only safe when the projection is trivial, because SQL applies
    // OFFSET after projection and skipped rows must still be evaluated (errors/side effects).
    // Keep this conservative: only allow OFFSET pushdown for identifiers / wildcards /
    // literal values.
    let offset_pushdown_safe = offset == 0 || analysis::projection_is_trivial(&select.projection);

    if offset > 0 && !offset_pushdown_safe {
        let limit = if effective_limit == usize::MAX {
            None
        } else {
            Some(offset.saturating_add(effective_limit))
        };
        return GenerateSeriesOffsetLimitPushdownPlan {
            offset: 0,
            limit,
            clear_query_offset_limit_fetch: false,
        };
    }

    let limit = if effective_limit == usize::MAX {
        None
    } else {
        Some(effective_limit)
    };
    GenerateSeriesOffsetLimitPushdownPlan {
        offset,
        limit,
        clear_query_offset_limit_fetch: offset != 0 || limit.is_some(),
    }
}
