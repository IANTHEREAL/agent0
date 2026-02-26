//! Utility functions for aggregate expression collection and GROUP BY matching.

use super::aggregate::aggregate_identity_matches;
use crate::model::{DataType, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::operators::AggregateExpr;

/// Find the index of a GROUP BY expression that matches `expr`.
pub(crate) fn find_matching_group_by(expr: &TypedExpr, group_by: &[TypedExpr]) -> Option<usize> {
    // Fast path: ColumnRef-to-ColumnRef matching by column_index + name
    for (i, gb) in group_by.iter().enumerate() {
        if let (
            TypedExprKind::ColumnRef {
                column_index: ei,
                column_name: en,
                ..
            },
            TypedExprKind::ColumnRef {
                column_index: gi,
                column_name: gn,
                ..
            },
        ) = (&expr.kind, &gb.kind)
        {
            if ei == gi && en == gn {
                return Some(i);
            }
        }
    }
    // Slow path: structural comparison via Display for expression GROUP BY keys
    let expr_display = format!("{}", expr);
    for (i, gb) in group_by.iter().enumerate() {
        if format!("{}", gb) == expr_display && expr.data_type == gb.data_type {
            return Some(i);
        }
    }
    None
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
                    args.get(1).and_then(|a| {
                        if let TypedExprKind::Constant(Value::Text(s)) = &a.kind {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
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
        TypedExprKind::IsTest { expr, .. } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(low, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(high, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::InList { expr, list, .. }
        | TypedExprKind::ScalarArrayCmp {
            expr, elems: list, ..
        } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
            for item in list {
                collect_agg_exprs_from(item, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(pattern, output_name, agg_exprs, agg_names, agg_types);
            if let Some(e) = escape {
                collect_agg_exprs_from(e, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(pattern, output_name, agg_exprs, agg_names, agg_types);
            if let Some(e) = escape {
                collect_agg_exprs_from(e, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        // Recurse into sub-expressions (e.g., CAST(COUNT(*) AS int))
        TypedExprKind::BinaryOp { left, right, .. } => {
            collect_agg_exprs_from(left, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(right, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::UnaryOp { operand, .. } => {
            collect_agg_exprs_from(operand, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::Cast { expr: inner, .. } => {
            collect_agg_exprs_from(inner, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_agg_exprs_from(op, output_name, agg_exprs, agg_names, agg_types);
            }
            for (w, t) in when_clauses {
                collect_agg_exprs_from(w, output_name, agg_exprs, agg_names, agg_types);
                collect_agg_exprs_from(t, output_name, agg_exprs, agg_names, agg_types);
            }
            if let Some(e) = else_result {
                collect_agg_exprs_from(e, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::Coalesce(args) => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::NullIf(a, b) => {
            collect_agg_exprs_from(a, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(b, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::MinMax { args, .. } => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::AnyAll { expr, .. } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::ArrayLiteral(items) | TypedExprKind::Row(items) => {
            for item in items {
                collect_agg_exprs_from(item, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        TypedExprKind::ArrayIndex { array, index } => {
            collect_agg_exprs_from(array, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(index, output_name, agg_exprs, agg_names, agg_types);
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            collect_agg_exprs_from(expr, output_name, agg_exprs, agg_names, agg_types);
            collect_agg_exprs_from(path, output_name, agg_exprs, agg_names, agg_types);
        }
        // WindowCall: recurse into children to find aggregate sub-expressions.
        // Handles cases like `ROW_NUMBER() OVER (ORDER BY COUNT(*))` and
        // `LAG(COUNT(*)) OVER (...)`.
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                collect_agg_exprs_from(arg, output_name, agg_exprs, agg_names, agg_types);
            }
            for e in partition_by {
                collect_agg_exprs_from(e, output_name, agg_exprs, agg_names, agg_types);
            }
            for ob in order_by {
                collect_agg_exprs_from(&ob.expr, output_name, agg_exprs, agg_names, agg_types);
            }
        }
        _ => {}
    }
}
