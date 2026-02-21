//! Subquery flattening — column mapping, FROM replacement, WHERE merge.
//!
//! Replaces `Subquery(inner) -> Table(base)`, remaps column indices, and merges
//! WHERE clauses when flattening a simple view subquery.

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, TypedExpr, TypedExprKind, TypedOrderByExpr,
};

use super::remap::{distinct_remap_is_safe, remap_column_refs, remap_is_safe};

/// Flatten the subquery: replace Subquery(inner) -> Table(base), remap column
/// indices, merge WHERE clauses.
pub(super) fn flatten_subquery(query: AnalyzedQuery) -> AnalyzedQuery {
    let AnalyzedQuery {
        ctes,
        body,
        order_by,
        limit,
        offset,
        output_schema,
    } = query;

    let AnalyzedQueryBody::Select(outer_select) = body else {
        unreachable!("checked in rewrite_query");
    };

    let AnalyzedSelect {
        projection: outer_projection,
        from: outer_from,
        where_clause: outer_where,
        group_by: outer_group_by,
        having: outer_having,
        distinct: outer_distinct,
    } = outer_select;

    // Destructure the single FROM source.
    let mut from_vec = outer_from;
    let outer_table_ref = from_vec.remove(0);
    let outer_alias = outer_table_ref.alias;
    let AnalyzedTableRefKind::Subquery(inner_query_box) = outer_table_ref.kind else {
        unreachable!("checked in rewrite_query");
    };
    let inner_query = *inner_query_box;

    let AnalyzedQueryBody::Select(inner_select) = inner_query.body else {
        unreachable!("checked in can_flatten");
    };

    // Build column map: mapping[outer_col_i] = inner_base_col_index
    // Also collect base column names for planner index selection.
    let mut column_map: Vec<usize> = Vec::with_capacity(inner_select.projection.len());
    let mut base_names: Vec<String> = Vec::with_capacity(inner_select.projection.len());

    for proj in &inner_select.projection {
        if let TypedExprKind::ColumnRef {
            column_index,
            column_name,
            ..
        } = &proj.expr.kind
        {
            column_map.push(*column_index);
            base_names.push(column_name.clone());
        } else {
            unreachable!("checked in can_flatten: all projections are ColumnRef");
        }
    }

    // Defensive guard: verify all outer refs will be in bounds after remap.
    if !remap_is_safe(
        &outer_projection.iter().map(|p| &p.expr).collect::<Vec<_>>(),
        &column_map,
        &base_names,
    ) || !remap_is_safe(
        &outer_where.iter().collect::<Vec<_>>(),
        &column_map,
        &base_names,
    ) || !remap_is_safe(
        &outer_group_by.iter().collect::<Vec<_>>(),
        &column_map,
        &base_names,
    ) || !remap_is_safe(
        &outer_having.iter().collect::<Vec<_>>(),
        &column_map,
        &base_names,
    ) || !remap_is_safe(
        &order_by.iter().map(|ob| &ob.expr).collect::<Vec<_>>(),
        &column_map,
        &base_names,
    ) || !distinct_remap_is_safe(&outer_distinct, &column_map, &base_names)
    {
        // Out-of-bounds column reference — bail out, return original query.
        let restored_body = AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: outer_projection,
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Subquery(Box::new(AnalyzedQuery {
                    ctes: inner_query.ctes,
                    body: AnalyzedQueryBody::Select(inner_select),
                    order_by: inner_query.order_by,
                    limit: inner_query.limit,
                    offset: inner_query.offset,
                    output_schema: inner_query.output_schema,
                })),
                alias: outer_alias,
            }],
            where_clause: outer_where,
            group_by: outer_group_by,
            having: outer_having,
            distinct: outer_distinct,
        });
        return AnalyzedQuery {
            ctes,
            body: restored_body,
            order_by,
            limit,
            offset,
            output_schema,
        };
    }

    // Remap all outer clause expressions.
    let remapped_projection = outer_projection
        .into_iter()
        .map(|mut p| {
            p.expr = remap_column_refs(p.expr, &column_map, &base_names);
            p
        })
        .collect();

    let remapped_outer_where = outer_where.map(|w| remap_column_refs(w, &column_map, &base_names));

    let remapped_group_by = outer_group_by
        .into_iter()
        .map(|e| remap_column_refs(e, &column_map, &base_names))
        .collect();

    let remapped_having = outer_having.map(|h| remap_column_refs(h, &column_map, &base_names));

    let remapped_order_by = order_by
        .into_iter()
        .map(|ob| TypedOrderByExpr {
            expr: remap_column_refs(ob.expr, &column_map, &base_names),
            asc: ob.asc,
            nulls_first: ob.nulls_first,
        })
        .collect();

    let remapped_distinct = match outer_distinct {
        AnalyzedDistinct::DistinctOn(exprs) => AnalyzedDistinct::DistinctOn(
            exprs
                .into_iter()
                .map(|e| remap_column_refs(e, &column_map, &base_names))
                .collect(),
        ),
        other => other,
    };

    // Merge WHERE: (inner_where IS TRUE) AND remapped_outer_where
    let merged_where = merge_where(inner_select.where_clause, remapped_outer_where);

    // Replace FROM: Subquery -> inner's Table, preserving outer alias.
    let inner_from = inner_select.from.into_iter().next().unwrap();
    let new_from = AnalyzedTableRef {
        kind: inner_from.kind,
        alias: outer_alias.or(inner_from.alias),
    };

    let new_body = AnalyzedQueryBody::Select(AnalyzedSelect {
        projection: remapped_projection,
        from: vec![new_from],
        where_clause: merged_where,
        group_by: remapped_group_by,
        having: remapped_having,
        distinct: remapped_distinct,
    });

    AnalyzedQuery {
        ctes,
        body: new_body,
        order_by: remapped_order_by,
        limit,
        offset,
        output_schema,
    }
}

// -- WHERE merge --

/// Merge inner and outer WHERE clauses.
///
/// **Inner-only**: used directly -- NULL is already falsy in WHERE context, and
/// wrapping with IS TRUE would suppress planner predicate extraction.
///
/// **Both present**: inner is wrapped with IS TRUE, then ANDed with outer.
/// Reason: the evaluator (`typed_eval.rs:378`) evaluates the RHS when LHS is
/// NULL. Without IS TRUE, a NULL inner predicate would cause `outer_where` to
/// be evaluated on rows the original subquery would have discarded -- which can
/// surface errors (e.g. division by zero) that the pre-flatten path avoided.
/// IS TRUE converts NULL->FALSE, which short-circuits AND correctly.
///
/// The planner can still extract outer predicates (the user's filter, most
/// likely to match an index) because `collect_typed_predicates` recurses into
/// both sides of AND. Only the inner predicates are opaque (wrapped in IS TRUE).
fn merge_where(
    inner_where: Option<TypedExpr>,
    outer_where: Option<TypedExpr>,
) -> Option<TypedExpr> {
    match (inner_where, outer_where) {
        (None, None) => None,
        (None, Some(outer)) => Some(outer),
        (Some(inner), None) => {
            // No wrapping -- NULL is already falsy in WHERE context, and IS TRUE
            // would prevent the planner from extracting predicates like `a = 1`
            // for index selection (planner only recognizes bare comparisons).
            Some(inner)
        }
        (Some(inner), Some(outer)) => {
            // Wrap inner with IS TRUE: NULL->FALSE short-circuits AND, preventing
            // outer_where evaluation on rows the original subquery would have
            // discarded. Outer predicates remain extractable by the planner.
            Some(TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(wrap_is_true(inner)),
                    op: crate::sql::analyzer::types::BinaryOp::And,
                    right: Box::new(outer),
                },
                crate::types::DataType::Boolean,
            ))
        }
    }
}

fn wrap_is_true(expr: TypedExpr) -> TypedExpr {
    use crate::sql::analyzer::types::IsTestKind;
    TypedExpr::new(
        TypedExprKind::IsTest {
            expr: Box::new(expr),
            test: IsTestKind::True,
            negated: false,
        },
        crate::types::DataType::Boolean,
    )
}
