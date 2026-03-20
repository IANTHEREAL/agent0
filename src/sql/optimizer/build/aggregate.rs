//! Aggregate operator builders: HashAggregate construction and post-aggregate projection rewriting.

use anyhow::{anyhow, Result};

use super::utils::{
    collect_agg_exprs_from, find_matching_group_by, normalize_string_agg_delimiter,
};
use crate::model::DataType;
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind, TypedOrderByExpr};
use crate::sql::operators::{AggregateExpr, BoxedOperator, HashAggregateOperator, ProjectOperator};

/// Check whether an `AggregateExpr` matches the identity of an `AggregateCall`.
///
/// Compares all 6 identity fields: `func_name`, `distinct`, `arg`, `delimiter`,
/// `filter`, and `order_by`.  This is the single source of truth for aggregate
/// dedup (collection) and slot lookup (rewrite) to prevent drift.
pub(crate) fn aggregate_identity_matches(
    ae: &AggregateExpr,
    func: &crate::sql::analyzer::types::ResolvedFunction,
    args: &[TypedExpr],
    distinct: bool,
    filter: &Option<Box<TypedExpr>>,
    order_by: &[crate::sql::analyzer::types::TypedOrderByExpr],
) -> bool {
    // 1. func_name
    if ae.func_name != func.name {
        return false;
    }
    // 2. distinct
    if ae.distinct != distinct {
        return false;
    }
    // 3. arg (first argument)
    let arg_matches = match (&ae.arg, args.first()) {
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
        (Some(stored), Some(current)) => stored == current,
    };
    if !arg_matches {
        return false;
    }
    // 4. delimiter (string_agg second argument)
    let call_delimiter = if func.name.eq_ignore_ascii_case("string_agg") {
        args.get(1).and_then(normalize_string_agg_delimiter)
    } else {
        None
    };
    if ae.delimiter != call_delimiter {
        return false;
    }
    // 5. filter
    let filter_matches = match (&ae.filter, filter) {
        (None, None) => true,
        (Some(stored), Some(current)) => stored == current.as_ref(),
        _ => false,
    };
    if !filter_matches {
        return false;
    }
    // 6. order_by
    if ae.order_by.len() != order_by.len() {
        return false;
    }
    for (stored, current) in ae.order_by.iter().zip(order_by.iter()) {
        if stored.expr != current.expr
            || stored.asc != current.asc
            || stored.nulls_first != current.nulls_first
        {
            return false;
        }
    }
    true
}

/// Build a HashAggregateOperator from group-by and projection lists.
///
/// Extracts aggregate function calls from projections and separates them from
/// group-by column references.  When projections contain expressions wrapping
/// aggregates (e.g. `COUNT(*) + 1`), a post-aggregate `ProjectOperator` is
/// added to evaluate those expressions against the aggregate output.
pub(super) fn build_hash_aggregate(
    child: BoxedOperator,
    group_by: &[TypedExpr],
    projections: &[crate::sql::analyzer::types::AnalyzedProjection],
) -> Result<BoxedOperator> {
    // Build group-by names and types.
    let group_by_count = group_by.len();
    let group_by_names: Vec<String> = group_by
        .iter()
        .enumerate()
        .map(|(i, gb)| match &gb.kind {
            TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
            _ => format!("group_by_{}", i),
        })
        .collect();
    let group_by_types: Vec<DataType> = group_by.iter().map(|gb| gb.data_type.clone()).collect();

    // Extract unique aggregate expressions from projections.
    // Track each aggregate's position in the operator output (after group-by columns).
    let mut aggregate_exprs = Vec::new();
    let mut aggregate_names = Vec::new();
    let mut aggregate_types = Vec::new();

    for proj in projections {
        collect_agg_exprs_from(
            &proj.expr,
            &proj.output_name,
            &mut aggregate_exprs,
            &mut aggregate_names,
            &mut aggregate_types,
        );
    }

    let agg_op: BoxedOperator = Box::new(HashAggregateOperator::new(
        child,
        group_by.to_vec(),
        aggregate_exprs.clone(),
        group_by_names.clone(),
        group_by_types.clone(),
        aggregate_names,
        aggregate_types,
    ));

    // Check if any projection wraps an aggregate in an expression (e.g. COUNT(*) + 1).
    // If so, we need a post-aggregate Project to evaluate those expressions.
    let needs_post_projection = projections.iter().any(|p| {
        !matches!(p.expr.kind, TypedExprKind::AggregateCall { .. }) && contains_aggregate(&p.expr)
    });

    // Also check for group-by-only projections mixed with aggregates — these need
    // rewriting too since the aggregate operator output schema differs from the
    // original table schema.
    let has_group_by_refs =
        group_by_count > 0 && projections.iter().any(|p| !contains_aggregate(&p.expr));

    // After dedup, the aggregate output width (group_by + unique aggregates) may be
    // narrower than the projection list when the same aggregate appears more than
    // once (e.g. SELECT SUM(a), SUM(a)).  A post-projection is needed to duplicate
    // the column so the output schema matches the analyzed projection count.
    let has_duplicate_agg_refs = projections.len() != group_by_count + aggregate_exprs.len();

    if !needs_post_projection && !has_group_by_refs && !has_duplicate_agg_refs {
        return Ok(agg_op);
    }

    // Build rewritten projection expressions.  In the aggregate operator output:
    //   columns 0..group_by_count → group-by key values
    //   columns group_by_count..  → aggregate result values
    let rewritten: Vec<TypedExpr> = projections
        .iter()
        .map(|p| rewrite_post_aggregate_expr(&p.expr, group_by, group_by_count, &aggregate_exprs))
        .collect::<Result<Vec<_>>>()?;
    let output_names: Vec<String> = projections.iter().map(|p| p.output_name.clone()).collect();
    let output_types: Vec<DataType> = projections
        .iter()
        .map(|p| p.expr.data_type.clone())
        .collect();

    Ok(Box::new(ProjectOperator::new(
        agg_op,
        rewritten,
        output_names,
        output_types,
    )))
}

/// Check if a TypedExpr contains any AggregateCall.
fn contains_aggregate(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::AggregateCall { .. } => true,
        TypedExprKind::IsTest { expr, .. } => contains_aggregate(expr),
        TypedExprKind::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        TypedExprKind::InList { expr, list, .. }
        | TypedExprKind::ScalarArrayCmp {
            expr, elems: list, ..
        } => contains_aggregate(expr) || list.iter().any(contains_aggregate),
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            contains_aggregate(expr)
                || contains_aggregate(pattern)
                || escape.as_ref().is_some_and(|e| contains_aggregate(e))
        }
        TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            contains_aggregate(expr)
                || contains_aggregate(pattern)
                || escape.as_ref().is_some_and(|e| contains_aggregate(e))
        }
        TypedExprKind::BinaryOp { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        TypedExprKind::UnaryOp { operand, .. } => contains_aggregate(operand),
        TypedExprKind::Cast { expr: inner, .. } => contains_aggregate(inner),
        TypedExprKind::FunctionCall { args, .. } => args.iter().any(contains_aggregate),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|o| contains_aggregate(o))
                || when_clauses
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_ref().is_some_and(|e| contains_aggregate(e))
        }
        TypedExprKind::AnyAll { expr, .. } => contains_aggregate(expr),
        TypedExprKind::Coalesce(args) => args.iter().any(contains_aggregate),
        TypedExprKind::NullIf(a, b) => contains_aggregate(a) || contains_aggregate(b),
        TypedExprKind::MinMax { args, .. } => args.iter().any(contains_aggregate),
        TypedExprKind::ArrayLiteral(items) | TypedExprKind::Row(items) => {
            items.iter().any(contains_aggregate)
        }
        TypedExprKind::ArrayIndex { array, index } => {
            contains_aggregate(array) || contains_aggregate(index)
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            contains_aggregate(expr) || contains_aggregate(path)
        }
        _ => false,
    }
}

/// Rewrite a projection expression for post-aggregate evaluation.
///
/// - `AggregateCall` -> `ColumnRef` at `group_by_count + agg_index`
/// - `ColumnRef` matching a GROUP BY expression -> `ColumnRef` at `group_by_index`
/// - Everything else -> recurse into children
///
/// Returns `Err` if an aggregate call cannot be matched to an extracted slot,
/// rather than silently falling back to slot 0 (which would produce wrong results).
pub(crate) fn rewrite_post_aggregate_expr(
    expr: &TypedExpr,
    group_by: &[TypedExpr],
    group_by_count: usize,
    aggregate_exprs: &[AggregateExpr],
) -> Result<TypedExpr> {
    // Check if this expression matches a GROUP BY key.
    if let Some(gb_idx) = find_matching_group_by(expr, group_by) {
        return Ok(TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: gb_idx,
                column_name: match &group_by[gb_idx].kind {
                    TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
                    _ => format!("group_by_{}", gb_idx),
                },
            },
            data_type: expr.data_type.clone(),
        });
    }

    match &expr.kind {
        // Replace aggregate call with a ColumnRef pointing to its output position.
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            filter,
            order_by,
        } => {
            // Find this aggregate in the extracted list using full 6-field identity match.
            let agg_idx = aggregate_exprs
                .iter()
                .position(|ae| {
                    aggregate_identity_matches(ae, func, args, *distinct, filter, order_by)
                })
                .ok_or_else(|| {
                    anyhow!(
                        "aggregate rewrite: no matching slot for {}({})",
                        func.name,
                        args.first()
                            .map(|a| format!("{}", a))
                            .unwrap_or_else(|| "*".to_string())
                    )
                })?;

            Ok(TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: group_by_count + agg_idx,
                    column_name: func.name.clone(),
                },
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => Ok(TypedExpr {
            kind: TypedExprKind::IsTest {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                test: *test,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => Ok(TypedExpr {
            kind: TypedExprKind::Between {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                low: Box::new(rewrite_post_aggregate_expr(
                    low,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                high: Box::new(rewrite_post_aggregate_expr(
                    high,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => Ok(TypedExpr {
            kind: TypedExprKind::InList {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                list: list
                    .iter()
                    .map(|e| {
                        rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                    })
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::ScalarArrayCmp {
            expr: inner,
            elems,
            op,
            use_or,
        } => Ok(TypedExpr {
            kind: TypedExprKind::ScalarArrayCmp {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                elems: elems
                    .iter()
                    .map(|e| {
                        rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                    })
                    .collect::<Result<Vec<_>>>()?,
                op: op.clone(),
                use_or: *use_or,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Ok(TypedExpr {
            kind: TypedExprKind::Like {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                pattern: Box::new(rewrite_post_aggregate_expr(
                    pattern,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                escape: escape
                    .as_ref()
                    .map(|e| {
                        rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                            .map(Box::new)
                    })
                    .transpose()?,
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => Ok(TypedExpr {
            kind: TypedExprKind::SimilarTo {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                pattern: Box::new(rewrite_post_aggregate_expr(
                    pattern,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                escape: escape
                    .as_ref()
                    .map(|e| {
                        rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                            .map(Box::new)
                    })
                    .transpose()?,
                negated: *negated,
            },
            data_type: expr.data_type.clone(),
        }),
        // Recurse into wrapping expressions.
        TypedExprKind::BinaryOp { left, op, right } => Ok(TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(rewrite_post_aggregate_expr(
                    left,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                op: op.clone(),
                right: Box::new(rewrite_post_aggregate_expr(
                    right,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::UnaryOp { op, operand } => Ok(TypedExpr {
            kind: TypedExprKind::UnaryOp {
                op: *op,
                operand: Box::new(rewrite_post_aggregate_expr(
                    operand,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => Ok(TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                target_type: target_type.clone(),
                cast_context: *cast_context,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            let rewritten_args: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::FunctionCall {
                    func: func.clone(),
                    args: rewritten_args,
                    order_by: order_by.clone(),
                    filter: filter.clone(),
                },
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            let rewritten_operand = operand
                .as_ref()
                .map(|o| {
                    rewrite_post_aggregate_expr(o, group_by, group_by_count, aggregate_exprs)
                        .map(Box::new)
                })
                .transpose()?;
            let rewritten_whens: Vec<(TypedExpr, TypedExpr)> = when_clauses
                .iter()
                .map(|(w, t)| {
                    Ok((
                        rewrite_post_aggregate_expr(w, group_by, group_by_count, aggregate_exprs)?,
                        rewrite_post_aggregate_expr(t, group_by, group_by_count, aggregate_exprs)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let rewritten_else = else_result
                .as_ref()
                .map(|e| {
                    rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                        .map(Box::new)
                })
                .transpose()?;
            Ok(TypedExpr {
                kind: TypedExprKind::Case {
                    operand: rewritten_operand,
                    when_clauses: rewritten_whens,
                    else_result: rewritten_else,
                },
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::Coalesce(args) => {
            let rewritten: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::Coalesce(rewritten),
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::NullIf(a, b) => Ok(TypedExpr {
            kind: TypedExprKind::NullIf(
                Box::new(rewrite_post_aggregate_expr(
                    a,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                Box::new(rewrite_post_aggregate_expr(
                    b,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            ),
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::MinMax { args, is_greatest } => {
            let rewritten: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::MinMax {
                    args: rewritten,
                    is_greatest: *is_greatest,
                },
                data_type: expr.data_type.clone(),
            })
        }
        TypedExprKind::AnyAll {
            expr: inner,
            op,
            subquery,
            is_all,
        } => Ok(TypedExpr {
            kind: TypedExprKind::AnyAll {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                op: op.clone(),
                subquery: subquery.clone(),
                is_all: *is_all,
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::ArrayLiteral(items) => Ok(TypedExpr {
            kind: TypedExprKind::ArrayLiteral(
                items
                    .iter()
                    .map(|e| {
                        rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::Row(items) => Ok(TypedExpr {
            kind: TypedExprKind::Row(
                items
                    .iter()
                    .map(|e| {
                        rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs)
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::ArrayIndex { array, index } => Ok(TypedExpr {
            kind: TypedExprKind::ArrayIndex {
                array: Box::new(rewrite_post_aggregate_expr(
                    array,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                index: Box::new(rewrite_post_aggregate_expr(
                    index,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
            },
            data_type: expr.data_type.clone(),
        }),
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => Ok(TypedExpr {
            kind: TypedExprKind::JsonAccess {
                expr: Box::new(rewrite_post_aggregate_expr(
                    inner,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                path: Box::new(rewrite_post_aggregate_expr(
                    path,
                    group_by,
                    group_by_count,
                    aggregate_exprs,
                )?),
                operator: *operator,
            },
            data_type: expr.data_type.clone(),
        }),
        // WindowCall: preserve the wrapper but recurse into children (args,
        // partition_by, order_by) to rewrite any aggregate/group-by references.
        // This handles mixed expressions like `LAG(COUNT(*)) OVER (ORDER BY dept)`.
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => {
            let rewritten_args: Vec<TypedExpr> = args
                .iter()
                .map(|a| rewrite_post_aggregate_expr(a, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            let rewritten_partition: Vec<TypedExpr> = partition_by
                .iter()
                .map(|e| rewrite_post_aggregate_expr(e, group_by, group_by_count, aggregate_exprs))
                .collect::<Result<Vec<_>>>()?;
            let rewritten_order: Vec<TypedOrderByExpr> = order_by
                .iter()
                .map(|ob| {
                    Ok(TypedOrderByExpr {
                        expr: rewrite_post_aggregate_expr(
                            &ob.expr,
                            group_by,
                            group_by_count,
                            aggregate_exprs,
                        )?,
                        asc: ob.asc,
                        nulls_first: ob.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(TypedExpr {
                kind: TypedExprKind::WindowCall {
                    func: func.clone(),
                    args: rewritten_args,
                    partition_by: rewritten_partition,
                    order_by: rewritten_order,
                    window_frame: window_frame.clone(),
                },
                data_type: expr.data_type.clone(),
            })
        }
        // Leaf nodes (constants, etc.) pass through unchanged.
        _ => Ok(expr.clone()),
    }
}
