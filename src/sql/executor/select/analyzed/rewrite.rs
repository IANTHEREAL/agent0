//! Aggregate and window function analysis and expression rewriting
//! for the analyzed SELECT path.

use crate::sql::analyzer::types::{
    AnalyzedSelect, ResolvedFunction, TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::operators::{AggregateExpr, WindowFunctionExpr};
use crate::model::{DataType, Value};
use std::collections::HashMap;

// Re-export window utilities from the canonical location in the optimizer.
pub(super) use crate::sql::optimizer::window_rewrite::{
    collect_window_calls_from_expr, contains_window, rewrite_for_post_window,
};

pub(super) fn has_windows(select: &AnalyzedSelect) -> bool {
    select.projection.iter().any(|p| contains_window(&p.expr))
}

/// Extract WindowCall nodes from projection expressions and convert to
/// WindowFunctionExpr for the WindowOperator.
///
/// The expressions may already be rewritten for post-aggregate positions,
/// so WindowCall children (partition_by, order_by) reference the correct
/// input columns for the window operator.
pub(super) fn extract_window_functions(
    projection: &[TypedExpr],
    select: &AnalyzedSelect,
) -> Vec<WindowFunctionExpr> {
    let mut result = Vec::new();
    for (i, expr) in projection.iter().enumerate() {
        let output_name = &select.projection[i].output_name;
        collect_window_calls_from_expr(expr, output_name, &mut result);
    }
    result
}

// ── Aggregate helper functions ──────────────────────────────

/// Check if a SELECT has aggregates (GROUP BY, HAVING, or AggregateCall in projection).
pub(super) fn has_aggregates(select: &AnalyzedSelect) -> bool {
    !select.group_by.is_empty()
        || select.having.is_some()
        || select
            .projection
            .iter()
            .any(|p| contains_aggregate(&p.expr))
}

/// Check if a TypedExpr tree contains an AggregateCall.
pub(super) fn contains_aggregate(expr: &TypedExpr) -> bool {
    crate::sql::expr::traverse::visit_any(expr, |e| {
        matches!(e.kind, TypedExprKind::AggregateCall { .. })
    })
}

/// Aggregate analysis info for building the post-aggregate pipeline.
///
/// Maps input column positions and aggregate calls to their positions
/// in the HashAggregate output: `[group_by_0..g, agg_0..a]`.
pub(super) struct AggregateAnalysis {
    pub(super) group_by_exprs: Vec<TypedExpr>,
    pub(super) group_by_names: Vec<String>,
    pub(super) group_by_types: Vec<DataType>,
    /// Maps input column_index → GROUP BY output position.
    pub(super) column_to_group_by: HashMap<usize, usize>,
    /// Maps GROUP BY expression display → GROUP BY output position.
    pub(super) expr_to_group_by: HashMap<String, usize>,
    pub(super) aggregate_exprs: Vec<AggregateExpr>,
    pub(super) aggregate_names: Vec<String>,
    pub(super) aggregate_types: Vec<DataType>,
    /// Maps aggregate display key → aggregate index.
    pub(super) agg_key_to_index: HashMap<String, usize>,
    /// Number of GROUP BY columns (aggregate positions start at this offset).
    pub(super) group_by_count: usize,
}

/// Build aggregate analysis from an AnalyzedSelect + ORDER BY.
///
/// Collects GROUP BY column mappings and all unique AggregateCall nodes
/// from projection, HAVING, and ORDER BY.
pub(super) fn build_aggregate_analysis(
    select: &AnalyzedSelect,
    order_by: &[TypedOrderByExpr],
) -> AggregateAnalysis {
    let mut analysis = AggregateAnalysis {
        group_by_exprs: select.group_by.clone(),
        group_by_names: Vec::new(),
        group_by_types: Vec::new(),
        column_to_group_by: HashMap::new(),
        expr_to_group_by: HashMap::new(),
        aggregate_exprs: Vec::new(),
        aggregate_names: Vec::new(),
        aggregate_types: Vec::new(),
        agg_key_to_index: HashMap::new(),
        group_by_count: select.group_by.len(),
    };

    // Build GROUP BY column → output position mappings.
    for (i, gb_expr) in select.group_by.iter().enumerate() {
        let name = match &gb_expr.kind {
            TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
            _ => format!("group_by_{}", i),
        };
        analysis.group_by_names.push(name);
        analysis.group_by_types.push(gb_expr.data_type.clone());

        if let TypedExprKind::ColumnRef { column_index, .. } = &gb_expr.kind {
            analysis.column_to_group_by.insert(*column_index, i);
        }
        analysis.expr_to_group_by.insert(format!("{}", gb_expr), i);
    }

    // Collect unique AggregateCall nodes from projection, HAVING, and ORDER BY.
    for proj in &select.projection {
        collect_aggregates_from_expr(&proj.expr, &mut analysis);
    }
    if let Some(ref having) = select.having {
        collect_aggregates_from_expr(having, &mut analysis);
    }
    for ob in order_by {
        collect_aggregates_from_expr(&ob.expr, &mut analysis);
    }

    analysis
}

/// Recursively collect AggregateCall nodes from a TypedExpr, deduplicating
/// by display key. Does NOT recurse into AggregateCall children (they are
/// input-level expressions evaluated by the aggregate operator itself).
///
/// Uses [`crate::sql::expr::traverse::for_each_child`] for canonical child
/// enumeration, but handles AggregateCall explicitly to stop recursion there.
pub(super) fn collect_aggregates_from_expr(expr: &TypedExpr, analysis: &mut AggregateAnalysis) {
    if let TypedExprKind::AggregateCall {
        func,
        args,
        distinct,
        order_by,
        filter,
    } = &expr.kind
    {
        let key = agg_display_key(func, args, *distinct, filter);
        if !analysis.agg_key_to_index.contains_key(&key) {
            let idx = analysis.aggregate_exprs.len();
            let agg_expr = AggregateExpr {
                func_name: func.name.to_uppercase(),
                arg: args.first().cloned(),
                distinct: *distinct,
                delimiter: if func.name.eq_ignore_ascii_case("STRING_AGG") {
                    args.get(1).and_then(|e| match &e.kind {
                        TypedExprKind::Constant(Value::Text(s)) => Some(s.clone()),
                        _ => None,
                    })
                } else {
                    None
                },
                filter: filter.as_ref().map(|f| *f.clone()),
                order_by: order_by.clone(),
            };
            analysis
                .aggregate_names
                .push(format!("{}_{}", func.name.to_lowercase(), idx));
            analysis.aggregate_types.push(expr.data_type.clone());
            analysis.aggregate_exprs.push(agg_expr);
            analysis.agg_key_to_index.insert(key, idx);
        }
        // Don't recurse — aggregate args are evaluated against pre-aggregate rows.
        return;
    }

    // Recurse into children to find nested aggregates (e.g. COUNT(*) + 1).
    crate::sql::expr::traverse::for_each_child(expr, &mut |child| {
        collect_aggregates_from_expr(child, analysis);
    });
}

/// Build a display key for an AggregateCall for deduplication.
///
/// Includes the FILTER clause in the key so that `COUNT(*) FILTER (WHERE x)`
/// and `COUNT(*)` are correctly treated as distinct aggregates.
pub(super) fn agg_display_key(
    func: &ResolvedFunction,
    args: &[TypedExpr],
    distinct: bool,
    filter: &Option<Box<TypedExpr>>,
) -> String {
    let args_str: Vec<String> = args.iter().map(|a| format!("{}", a)).collect();
    let base = format!(
        "{}({}{})",
        func.name.to_uppercase(),
        if distinct { "DISTINCT " } else { "" },
        if args_str.is_empty() {
            "*".to_string()
        } else {
            args_str.join(", ")
        }
    );
    match filter {
        Some(f) => format!("{} FILTER (WHERE {})", base, f),
        None => base,
    }
}

/// Rewrite a TypedExpr for post-aggregate evaluation.
///
/// - `ColumnRef` matching a GROUP BY column → remapped to GROUP BY output position
/// - `AggregateCall` → remapped to `ColumnRef` at `group_by_count + agg_index`
/// - Complex expressions → recursively rewrite children
pub(super) fn rewrite_for_post_aggregate(
    expr: &TypedExpr,
    analysis: &AggregateAnalysis,
) -> TypedExpr {
    // Check if the whole expression matches a GROUP BY key (before recursing).
    // This handles complex GROUP BY expressions like CAST(DATE_PART(...)).
    let expr_key = format!("{}", expr);
    if let Some(&gb_idx) = analysis.expr_to_group_by.get(&expr_key) {
        return TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: gb_idx,
                column_name: analysis.group_by_names[gb_idx].clone(),
            },
            data_type: expr.data_type.clone(),
        };
    }

    match &expr.kind {
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            filter,
            ..
        } => {
            let key = agg_display_key(func, args, *distinct, filter);
            if let Some(&idx) = analysis.agg_key_to_index.get(&key) {
                TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: analysis.group_by_count + idx,
                        column_name: analysis.aggregate_names[idx].clone(),
                    },
                    data_type: expr.data_type.clone(),
                }
            } else {
                expr.clone()
            }
        }

        TypedExprKind::ColumnRef {
            column_index,
            column_name,
            ..
        } => {
            if let Some(&gb_idx) = analysis.column_to_group_by.get(column_index) {
                TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: gb_idx,
                        column_name: column_name.clone(),
                    },
                    data_type: expr.data_type.clone(),
                }
            } else {
                // Column not in GROUP BY — shouldn't happen for valid aggregate queries,
                // but return as-is for robustness.
                expr.clone()
            }
        }

        TypedExprKind::Constant(_) => expr.clone(),

        TypedExprKind::BinaryOp { left, right, op } => TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(rewrite_for_post_aggregate(left, analysis)),
                op: op.clone(),
                right: Box::new(rewrite_for_post_aggregate(right, analysis)),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::UnaryOp { op, operand } => TypedExpr {
            kind: TypedExprKind::UnaryOp {
                op: *op,
                operand: Box::new(rewrite_for_post_aggregate(operand, analysis)),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                target_type: target_type.clone(),
                cast_context: cast_context.clone(),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                test: *test,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                low: Box::new(rewrite_for_post_aggregate(low, analysis)),
                high: Box::new(rewrite_for_post_aggregate(high, analysis)),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                list: list
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::ScalarArrayCmp {
            expr: inner,
            elems,
            op,
            use_or,
        } => TypedExpr {
            kind: TypedExprKind::ScalarArrayCmp {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                elems: elems
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
                op: op.clone(),
                use_or: *use_or,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::Like {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                pattern: Box::new(rewrite_for_post_aggregate(pattern, analysis)),
                escape: escape
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_aggregate(e, analysis))),
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExpr {
            kind: TypedExprKind::SimilarTo {
                expr: Box::new(rewrite_for_post_aggregate(inner, analysis)),
                pattern: Box::new(rewrite_for_post_aggregate(pattern, analysis)),
                escape: escape
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_aggregate(e, analysis))),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExpr {
            kind: TypedExprKind::Case {
                operand: operand
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_aggregate(e, analysis))),
                when_clauses: when_clauses
                    .iter()
                    .map(|(w, t)| {
                        (
                            rewrite_for_post_aggregate(w, analysis),
                            rewrite_for_post_aggregate(t, analysis),
                        )
                    })
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_aggregate(e, analysis))),
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::Coalesce(args) => TypedExpr {
            kind: TypedExprKind::Coalesce(
                args.iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
            ),
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::NullIf(a, b) => TypedExpr {
            kind: TypedExprKind::NullIf(
                Box::new(rewrite_for_post_aggregate(a, analysis)),
                Box::new(rewrite_for_post_aggregate(b, analysis)),
            ),
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::MinMax { args, is_greatest } => TypedExpr {
            kind: TypedExprKind::MinMax {
                args: args
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
                is_greatest: *is_greatest,
            },
            data_type: expr.data_type.clone(),
        },

        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
                order_by: order_by.clone(),
                filter: filter
                    .as_ref()
                    .map(|e| Box::new(rewrite_for_post_aggregate(e, analysis))),
            },
            data_type: expr.data_type.clone(),
        },

        // WindowCall: recurse into children but keep the WindowCall wrapper.
        // The window rewrite pass handles replacing WindowCall → ColumnRef.
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => TypedExpr {
            kind: TypedExprKind::WindowCall {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
                partition_by: partition_by
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect(),
                order_by: order_by
                    .iter()
                    .map(|o| TypedOrderByExpr {
                        expr: rewrite_for_post_aggregate(&o.expr, analysis),
                        asc: o.asc,
                        nulls_first: o.nulls_first,
                    })
                    .collect(),
                window_frame: window_frame.clone(),
            },
            data_type: expr.data_type.clone(),
        },

        // Opaque/leaf nodes — return as-is.
        _ => expr.clone(),
    }
}

// ── Subquery pre-materialization ────────────────────────────
