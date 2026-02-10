use super::*;

fn select_item_expr(item: &SelectItem) -> Option<&Expr> {
    match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => None,
    }
}

/// Recursively check if an expression contains an aggregate function call
/// that is NOT a window function (i.e., no OVER clause).
fn expr_has_non_window_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            if is_aggregate_func(f) && f.over.is_none() {
                return true;
            }
            f.args.iter().any(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => expr_has_non_window_aggregate(e),
                _ => false,
            })
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_non_window_aggregate(left) || expr_has_non_window_aggregate(right)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::TryCast { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNotFalse(inner) => expr_has_non_window_aggregate(inner),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_ref()
                .map_or(false, |o| expr_has_non_window_aggregate(o))
                || conditions.iter().any(|c| expr_has_non_window_aggregate(c))
                || results.iter().any(|r| expr_has_non_window_aggregate(r))
                || else_result
                    .as_ref()
                    .map_or(false, |e| expr_has_non_window_aggregate(e))
        }
        Expr::InList { expr: e, list, .. } => {
            expr_has_non_window_aggregate(e)
                || list.iter().any(|i| expr_has_non_window_aggregate(i))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_non_window_aggregate(expr)
                || expr_has_non_window_aggregate(low)
                || expr_has_non_window_aggregate(high)
        }
        Expr::ArrayAgg(_) => true,
        _ => false,
    }
}

pub(super) fn projection_has_non_window_aggregate(projection: &[SelectItem]) -> bool {
    projection
        .iter()
        .filter_map(select_item_expr)
        .any(expr_has_non_window_aggregate)
}

fn expr_is_select_list_srf(expr: &Expr) -> bool {
    let Expr::Function(f) = expr else {
        return false;
    };
    let Some(name) = f.name.0.last() else {
        return false;
    };
    name.value.eq_ignore_ascii_case("UNNEST")
        || name.value.eq_ignore_ascii_case("REGEXP_SPLIT_TO_TABLE")
        || name.value.eq_ignore_ascii_case("REGEXP_MATCHES")
        || name.value.eq_ignore_ascii_case("JSONB_OBJECT_KEYS")
        || name.value.eq_ignore_ascii_case("JSONB_ARRAY_ELEMENTS")
        || name.value.eq_ignore_ascii_case("JSONB_ARRAY_ELEMENTS_TEXT")
        || name.value.eq_ignore_ascii_case("JSONB_EACH")
        || name.value.eq_ignore_ascii_case("JSONB_EACH_TEXT")
}

pub(super) fn projection_has_select_list_srf(projection: &[SelectItem]) -> bool {
    projection
        .iter()
        .filter_map(select_item_expr)
        .any(expr_is_select_list_srf)
}

fn expr_has_window_function(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            if f.over.is_some() {
                return true;
            }
            f.args.iter().any(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => expr_has_window_function(e),
                _ => false,
            })
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_window_function(left) || expr_has_window_function(right)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::TryCast { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNotFalse(inner) => expr_has_window_function(inner),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_ref()
                .map_or(false, |o| expr_has_window_function(o))
                || conditions.iter().any(|c| expr_has_window_function(c))
                || results.iter().any(|r| expr_has_window_function(r))
                || else_result
                    .as_ref()
                    .map_or(false, |e| expr_has_window_function(e))
        }
        Expr::InList { expr: e, list, .. } => {
            expr_has_window_function(e) || list.iter().any(|i| expr_has_window_function(i))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_window_function(expr)
                || expr_has_window_function(low)
                || expr_has_window_function(high)
        }
        // Ignore nested query scopes; window routing decisions should be based on the current
        // SELECT list only.
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => false,
        _ => false,
    }
}

pub(super) fn projection_has_window_function(projection: &[SelectItem]) -> bool {
    projection
        .iter()
        .filter_map(select_item_expr)
        .any(expr_has_window_function)
}

fn expr_is_trivial_projection(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => true,
        Expr::Nested(inner) => expr_is_trivial_projection(inner),
        _ => false,
    }
}

pub(super) fn projection_is_trivial(projection: &[SelectItem]) -> bool {
    projection.iter().all(|item| match item {
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => true,
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_is_trivial_projection(expr)
        }
    })
}
