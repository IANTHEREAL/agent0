//! Utility functions for aggregate expression collection and GROUP BY matching.

use super::aggregate::aggregate_identity_matches;
use crate::model::{DataType, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::operators::AggregateExpr;

pub(crate) fn normalize_string_agg_delimiter(expr: &TypedExpr) -> Option<String> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Text(s)) => Some(s.clone()),
        TypedExprKind::Constant(Value::Null) => Some(String::new()),
        TypedExprKind::Cast { expr: inner, .. } => normalize_string_agg_delimiter(inner),
        _ => None,
    }
}

/// Find the index of a GROUP BY expression that matches `expr`.
pub(crate) fn find_matching_group_by(expr: &TypedExpr, group_by: &[TypedExpr]) -> Option<usize> {
    group_by.iter().position(|gb| gb == expr)
}

/// Recursively collect AggregateCall nodes from a TypedExpr.
pub(crate) fn collect_agg_exprs_from(
    expr: &TypedExpr,
    output_name: &str,
    agg_exprs: &mut Vec<AggregateExpr>,
    agg_names: &mut Vec<String>,
    agg_types: &mut Vec<DataType>,
) {
    match &expr.kind {
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            filter,
            order_by,
        } => {
            // Dedup: only add if no existing entry matches on all 6 identity fields.
            let already_exists = agg_exprs
                .iter()
                .any(|ae| aggregate_identity_matches(ae, func, args, *distinct, filter, order_by));
            if !already_exists {
                let arg = args.first().cloned();
                let delimiter = if func.name.eq_ignore_ascii_case("string_agg") {
                    args.get(1).and_then(normalize_string_agg_delimiter)
                } else {
                    None
                };
                agg_exprs.push(AggregateExpr {
                    func_name: func.name.clone(),
                    arg,
                    distinct: *distinct,
                    delimiter,
                    filter: filter.as_deref().cloned(),
                    order_by: order_by.clone(),
                });
                agg_names.push(output_name.to_string());
                agg_types.push(expr.data_type.clone());
            }
        }
        // All other variants: recursively collect from children using for_each_child
        // (exhaustive over all TypedExprKind variants). Covers Collate, IsDistinctFrom,
        // WindowCall, leaf nodes, and any future variants.
        _ => {
            crate::sql::expr::traverse::for_each_child(expr, &mut |child| {
                collect_agg_exprs_from(child, output_name, agg_exprs, agg_names, agg_types);
            });
        }
    }
}
