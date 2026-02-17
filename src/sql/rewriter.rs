//! Post-analysis query rewriter — subquery flattening for views.
//!
//! View expansion (`expand_views_in_query`) replaces `FROM my_view` with
//! `FROM (SELECT ... FROM base_table WHERE ...) AS my_view` at the AST level.
//! After analysis, this produces `AnalyzedTableRefKind::Subquery`, which the
//! optimizer rejects. This module flattens simple view subqueries back to
//! direct table references, enabling both the optimizer and legacy planner to
//! use index-aware scan strategies.
//!
//! Architecture: `Parser → View Expansion → Analyzer → [Rewriter] → Optimizer/Executor`

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, TypedExpr, TypedExprKind, TypedOrderByExpr, WindowFrame,
    WindowFrameBound,
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
    let AnalyzedTableRefKind::Subquery(ref inner_query) = select.from[0].kind else {
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
    // outer column — producing silent wrong results.
    if outer_has_subquery_exprs(select, &query.order_by) {
        return query;
    }

    flatten_subquery(query)
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

/// Flatten the subquery: replace Subquery(inner) → Table(base), remap column
/// indices, merge WHERE clauses.
fn flatten_subquery(query: AnalyzedQuery) -> AnalyzedQuery {
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

    // Replace FROM: Subquery → inner's Table, preserving outer alias.
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

// ── WHERE merge ──────────────────────────────────────────────────────────

/// Merge inner and outer WHERE clauses.
///
/// **Inner-only**: used directly — NULL is already falsy in WHERE context, and
/// wrapping with IS TRUE would suppress planner predicate extraction.
///
/// **Both present**: inner is wrapped with IS TRUE, then ANDed with outer.
/// Reason: the evaluator (`typed_eval.rs:378`) evaluates the RHS when LHS is
/// NULL. Without IS TRUE, a NULL inner predicate would cause `outer_where` to
/// be evaluated on rows the original subquery would have discarded — which can
/// surface errors (e.g. division by zero) that the pre-flatten path avoided.
/// IS TRUE converts NULL→FALSE, which short-circuits AND correctly.
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
            // No wrapping — NULL is already falsy in WHERE context, and IS TRUE
            // would prevent the planner from extracting predicates like `a = 1`
            // for index selection (planner only recognizes bare comparisons).
            Some(inner)
        }
        (Some(inner), Some(outer)) => {
            // Wrap inner with IS TRUE: NULL→FALSE short-circuits AND, preventing
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

// ── Bounds checking ──────────────────────────────────────────────────────

/// Check that all ColumnRef { scope_depth: 0 } in the given expressions have
/// column_index within the bounds of the column map and base names.
fn remap_is_safe(exprs: &[&TypedExpr], column_map: &[usize], base_names: &[String]) -> bool {
    let map_len = column_map.len();
    let names_len = base_names.len();
    for expr in exprs {
        if expr_any(expr, &|e| {
            if let TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index,
                ..
            } = &e.kind
            {
                *column_index >= map_len || *column_index >= names_len
            } else {
                false
            }
        }) {
            return false;
        }
    }
    true
}

fn distinct_remap_is_safe(
    distinct: &AnalyzedDistinct,
    column_map: &[usize],
    base_names: &[String],
) -> bool {
    match distinct {
        AnalyzedDistinct::DistinctOn(exprs) => {
            remap_is_safe(&exprs.iter().collect::<Vec<_>>(), column_map, base_names)
        }
        _ => true,
    }
}

// ── Column remapping ─────────────────────────────────────────────────────

/// Recursively remap ColumnRef { scope_depth: 0 } through the column map.
/// Does NOT descend into subquery expression boundaries (ScalarSubquery, Exists,
/// InSubquery.subquery, AnyAll.subquery, ArraySubquery) — those have independent
/// scopes where scope_depth: 0 means something different.
fn remap_column_refs(expr: TypedExpr, column_map: &[usize], base_names: &[String]) -> TypedExpr {
    let TypedExpr { kind, data_type } = expr;

    let new_kind = match kind {
        // Remap target
        TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index,
            ..
        } => TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: column_map[column_index],
            column_name: base_names[column_index].clone(),
        },

        // Leaves — return as-is
        TypedExprKind::Constant(_)
        | TypedExprKind::ColumnRef { .. } // scope_depth > 0
        | TypedExprKind::Default => kind,

        // Subquery boundaries — do NOT descend
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. } => kind,

        // InSubquery/AnyAll — remap expr only, NOT subquery
        TypedExprKind::InSubquery {
            expr: inner_expr,
            subquery,
            negated,
        } => TypedExprKind::InSubquery {
            expr: Box::new(remap_column_refs(*inner_expr, column_map, base_names)),
            subquery,
            negated,
        },
        TypedExprKind::AnyAll {
            expr: inner_expr,
            op,
            subquery,
            is_all,
        } => TypedExprKind::AnyAll {
            expr: Box::new(remap_column_refs(*inner_expr, column_map, base_names)),
            op,
            subquery,
            is_all,
        },

        // Binary/unary operators
        TypedExprKind::BinaryOp { left, op, right } => TypedExprKind::BinaryOp {
            left: Box::new(remap_column_refs(*left, column_map, base_names)),
            op,
            right: Box::new(remap_column_refs(*right, column_map, base_names)),
        },
        TypedExprKind::UnaryOp { op, operand } => TypedExprKind::UnaryOp {
            op,
            operand: Box::new(remap_column_refs(*operand, column_map, base_names)),
        },
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExprKind::Cast {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            target_type,
            cast_context,
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExprKind::IsTest {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            test,
            negated,
        },

        // Range/pattern expressions
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExprKind::Between {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            low: Box::new(remap_column_refs(*low, column_map, base_names)),
            high: Box::new(remap_column_refs(*high, column_map, base_names)),
            negated,
        },
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExprKind::InList {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            list: list
                .into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
            negated,
        },
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExprKind::Like {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            pattern: Box::new(remap_column_refs(*pattern, column_map, base_names)),
            escape: escape.map(|e| Box::new(remap_column_refs(*e, column_map, base_names))),
            case_insensitive,
            negated,
        },
        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExprKind::SimilarTo {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            pattern: Box::new(remap_column_refs(*pattern, column_map, base_names)),
            escape: escape.map(|e| Box::new(remap_column_refs(*e, column_map, base_names))),
            negated,
        },

        // Conditional
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExprKind::Case {
            operand: operand.map(|e| Box::new(remap_column_refs(*e, column_map, base_names))),
            when_clauses: when_clauses
                .into_iter()
                .map(|(w, t)| {
                    (
                        remap_column_refs(w, column_map, base_names),
                        remap_column_refs(t, column_map, base_names),
                    )
                })
                .collect(),
            else_result: else_result
                .map(|e| Box::new(remap_column_refs(*e, column_map, base_names))),
        },
        TypedExprKind::Coalesce(args) => TypedExprKind::Coalesce(
            args.into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
        ),
        TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
            Box::new(remap_column_refs(*a, column_map, base_names)),
            Box::new(remap_column_refs(*b, column_map, base_names)),
        ),
        TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
            args: args
                .into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
            is_greatest,
        },

        // Functions
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExprKind::FunctionCall {
            func,
            args: remap_vec(args, column_map, base_names),
            order_by: remap_order_by(order_by, column_map, base_names),
            filter: filter.map(|f| Box::new(remap_column_refs(*f, column_map, base_names))),
        },
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            order_by,
            filter,
        } => TypedExprKind::AggregateCall {
            func,
            args: remap_vec(args, column_map, base_names),
            distinct,
            order_by: remap_order_by(order_by, column_map, base_names),
            filter: filter.map(|f| Box::new(remap_column_refs(*f, column_map, base_names))),
        },
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => TypedExprKind::WindowCall {
            func,
            args: remap_vec(args, column_map, base_names),
            partition_by: remap_vec(partition_by, column_map, base_names),
            order_by: remap_order_by(order_by, column_map, base_names),
            window_frame: remap_window_frame(window_frame, column_map, base_names),
        },

        // Array/JSON
        TypedExprKind::ArrayLiteral(args) => TypedExprKind::ArrayLiteral(
            args.into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
        ),
        TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
            array: Box::new(remap_column_refs(*array, column_map, base_names)),
            index: Box::new(remap_column_refs(*index, column_map, base_names)),
        },
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExprKind::JsonAccess {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            path: Box::new(remap_column_refs(*path, column_map, base_names)),
            operator,
        },

        // Row
        TypedExprKind::Row(args) => TypedExprKind::Row(
            args.into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
        ),
    };

    TypedExpr::new(new_kind, data_type)
}

fn remap_vec(exprs: Vec<TypedExpr>, column_map: &[usize], base_names: &[String]) -> Vec<TypedExpr> {
    exprs
        .into_iter()
        .map(|e| remap_column_refs(e, column_map, base_names))
        .collect()
}

fn remap_order_by(
    order_by: Vec<TypedOrderByExpr>,
    column_map: &[usize],
    base_names: &[String],
) -> Vec<TypedOrderByExpr> {
    order_by
        .into_iter()
        .map(|ob| TypedOrderByExpr {
            expr: remap_column_refs(ob.expr, column_map, base_names),
            asc: ob.asc,
            nulls_first: ob.nulls_first,
        })
        .collect()
}

fn remap_window_frame(
    frame: Option<WindowFrame>,
    column_map: &[usize],
    base_names: &[String],
) -> Option<WindowFrame> {
    frame.map(|f| WindowFrame {
        units: f.units,
        start: remap_frame_bound(f.start, column_map, base_names),
        end: f.end.map(|b| remap_frame_bound(b, column_map, base_names)),
    })
}

fn remap_frame_bound(
    bound: WindowFrameBound,
    column_map: &[usize],
    base_names: &[String],
) -> WindowFrameBound {
    match bound {
        WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
        WindowFrameBound::Preceding(v) => WindowFrameBound::Preceding(
            v.map(|e| Box::new(remap_column_refs(*e, column_map, base_names))),
        ),
        WindowFrameBound::Following(v) => WindowFrameBound::Following(
            v.map(|e| Box::new(remap_column_refs(*e, column_map, base_names))),
        ),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{
        AnalyzedProjection, AnalyzedTableRef, AnalyzedTableRefKind, BinaryOp, TableRefSchema,
    };
    use crate::types::{DataType, Value};

    // ── Test helpers ─────────────────────────────────────────────────

    fn col_ref(index: usize, name: &str, dt: DataType) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            dt,
        )
    }

    fn projection(index: usize, name: &str, dt: DataType) -> AnalyzedProjection {
        AnalyzedProjection {
            expr: col_ref(index, name, dt.clone()),
            output_name: name.to_string(),
        }
    }

    fn table_ref(name: &str) -> AnalyzedTableRef {
        AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Table {
                name: name.to_string(),
                schema: TableRefSchema {
                    table_id: 1,
                    columns: vec![],
                },
            },
            alias: None,
        }
    }

    fn simple_inner_query(
        table: &str,
        projections: Vec<AnalyzedProjection>,
        where_clause: Option<TypedExpr>,
    ) -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: projections,
                from: vec![table_ref(table)],
                where_clause,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        }
    }

    fn wrap_as_outer(
        inner: AnalyzedQuery,
        alias: &str,
        outer_projection: Vec<AnalyzedProjection>,
        outer_where: Option<TypedExpr>,
    ) -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: outer_projection,
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some(alias.to_string()),
                }],
                where_clause: outer_where,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("id".to_string(), DataType::Int64),
                ("name".to_string(), DataType::Text),
            ],
        }
    }

    fn is_table_from(query: &AnalyzedQuery, expected_table: &str) -> bool {
        if let AnalyzedQueryBody::Select(ref s) = query.body {
            if let Some(ref from) = s.from.first() {
                if let AnalyzedTableRefKind::Table { ref name, .. } = from.kind {
                    return name == expected_table;
                }
            }
        }
        false
    }

    fn is_subquery_from(query: &AnalyzedQuery) -> bool {
        if let AnalyzedQueryBody::Select(ref s) = query.body {
            if let Some(ref from) = s.from.first() {
                return matches!(from.kind, AnalyzedTableRefKind::Subquery(_));
            }
        }
        false
    }

    // ── Positive test: basic flattening ──────────────────────────────

    #[test]
    fn test_flatten_simple_view() {
        // Inner: SELECT id(col0), name(col1) FROM users WHERE active(col2) = true
        let inner_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(2, "active", DataType::Boolean)),
                op: BinaryOp::Eq,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Boolean(true)),
                    DataType::Boolean,
                )),
            },
            DataType::Boolean,
        );
        let inner = simple_inner_query(
            "users",
            vec![
                projection(0, "id", DataType::Int64),
                projection(1, "name", DataType::Text),
            ],
            Some(inner_where),
        );

        // Outer: SELECT * FROM (inner) AS v WHERE id > 5
        let outer_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "id", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(5)),
                    DataType::Int64,
                )),
            },
            DataType::Boolean,
        );
        let query = wrap_as_outer(
            inner,
            "v",
            vec![
                projection(0, "id", DataType::Int64),
                projection(1, "name", DataType::Text),
            ],
            Some(outer_where),
        );

        let result = rewrite_query(query);

        // Should be flattened: FROM users, not subquery
        assert!(is_table_from(&result, "users"));

        // WHERE should be merged: (inner IS TRUE) AND outer
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let where_clause = s.where_clause.as_ref().expect("should have WHERE");
        assert!(matches!(
            where_clause.kind,
            TypedExprKind::BinaryOp {
                op: BinaryOp::And,
                ..
            }
        ));
    }

    // ── Negative tests: non-flattenable cases ────────────────────────

    #[test]
    fn test_no_flatten_group_by() {
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![col_ref(0, "id", DataType::Int64)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(inner, "v", vec![projection(0, "id", DataType::Int64)], None);
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_limit() {
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: Some(TypedExpr::new(
                TypedExprKind::Constant(Value::Int64(10)),
                DataType::Int64,
            )),
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(inner, "v", vec![projection(0, "id", DataType::Int64)], None);
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_distinct() {
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::Distinct,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(inner, "v", vec![projection(0, "id", DataType::Int64)], None);
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_computed_projection() {
        // Inner projection has a BinaryOp, not a plain ColumnRef
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(col_ref(0, "a", DataType::Int64)),
                            op: BinaryOp::Add,
                            right: Box::new(TypedExpr::new(
                                TypedExprKind::Constant(Value::Int64(1)),
                                DataType::Int64,
                            )),
                        },
                        DataType::Int64,
                    ),
                    output_name: "a_plus_one".to_string(),
                }],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "a_plus_one", DataType::Int64)],
            None,
        );
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_multi_from() {
        // Outer has 2 FROM sources
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![
                    AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                        alias: Some("v".to_string()),
                    },
                    table_ref("other"),
                ],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let result = rewrite_query(query);
        // Not flattened because outer has 2 FROM sources
        if let AnalyzedQueryBody::Select(ref s) = result.body {
            assert_eq!(s.from.len(), 2);
        } else {
            panic!("expected Select");
        }
    }

    // ── Column remap tests ───────────────────────────────────────────

    #[test]
    fn test_column_remap_reorder() {
        // Inner: SELECT col2 AS a, col0 AS b FROM users
        // mapping = [2, 0] — outer col0 → base col2, outer col1 → base col0
        let inner = simple_inner_query(
            "users",
            vec![
                projection(2, "c", DataType::Text),
                projection(0, "a", DataType::Int64),
            ],
            None,
        );

        // Outer: SELECT col0, col1 FROM (inner) AS v
        let query = wrap_as_outer(
            inner,
            "v",
            vec![
                projection(0, "a", DataType::Text),
                projection(1, "b", DataType::Int64),
            ],
            None,
        );

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        // Check that projection column indices are remapped
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let TypedExprKind::ColumnRef { column_index, .. } = &s.projection[0].expr.kind {
            assert_eq!(*column_index, 2, "outer col0 should map to base col2");
        } else {
            panic!("expected ColumnRef");
        }
        if let TypedExprKind::ColumnRef { column_index, .. } = &s.projection[1].expr.kind {
            assert_eq!(*column_index, 0, "outer col1 should map to base col0");
        } else {
            panic!("expected ColumnRef");
        }
    }

    #[test]
    fn test_where_merge_both_is_true_guard() {
        // Inner has WHERE, outer has WHERE → merged as (inner IS TRUE) AND outer.
        // IS TRUE is required: the evaluator evaluates RHS when LHS is NULL
        // (typed_eval.rs:378), so without IS TRUE a NULL inner predicate would
        // cause outer evaluation on rows the original subquery discarded.
        let inner_where = col_ref(2, "active", DataType::Boolean);
        let inner = simple_inner_query(
            "users",
            vec![projection(0, "id", DataType::Int64)],
            Some(inner_where),
        );

        let outer_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "id", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(5)),
                    DataType::Int64,
                )),
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            Some(outer_where),
        );

        let result = rewrite_query(query);
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let w = s.where_clause.as_ref().expect("should have WHERE");

        // Top-level: AND
        let TypedExprKind::BinaryOp {
            ref left,
            op: BinaryOp::And,
            ..
        } = w.kind
        else {
            panic!("expected AND at top level, got: {:?}", w.kind);
        };

        // LHS: IS TRUE wrapping inner WHERE — prevents NULL from leaking to RHS
        assert!(
            matches!(
                left.kind,
                TypedExprKind::IsTest {
                    test: crate::sql::analyzer::types::IsTestKind::True,
                    negated: false,
                    ..
                }
            ),
            "LHS should be IS TRUE wrapping inner WHERE"
        );
    }

    #[test]
    fn test_where_merge_inner_only() {
        // Only inner WHERE → becomes outer WHERE directly (NOT wrapped in IS TRUE,
        // because IS TRUE suppresses planner predicate extraction for index selection).
        let inner_where = col_ref(2, "active", DataType::Boolean);
        let inner = simple_inner_query(
            "users",
            vec![projection(0, "id", DataType::Int64)],
            Some(inner_where),
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            None, // no outer WHERE
        );

        let result = rewrite_query(query);
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let w = s.where_clause.as_ref().expect("should have WHERE");
        // Inner WHERE should be used directly — a bare ColumnRef, not IS TRUE wrapped.
        assert!(
            matches!(
                w.kind,
                TypedExprKind::ColumnRef {
                    column_index: 2,
                    ..
                }
            ),
            "inner-only WHERE should be used directly without IS TRUE wrapping"
        );
    }

    #[test]
    fn test_order_by_remap() {
        // Inner: SELECT col1 AS a FROM users → mapping = [1]
        let inner = simple_inner_query("users", vec![projection(1, "b", DataType::Int64)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "a", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "a", DataType::Int64),
                asc: true,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            output_schema: vec![("a".to_string(), DataType::Int64)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        if let TypedExprKind::ColumnRef { column_index, .. } = &result.order_by[0].expr.kind {
            assert_eq!(*column_index, 1, "ORDER BY col0 should remap to base col1");
        } else {
            panic!("expected ColumnRef in ORDER BY");
        }
    }

    #[test]
    fn test_outer_group_by_remap() {
        // Inner: SELECT col2 AS x FROM users → mapping = [2]
        let inner = simple_inner_query("users", vec![projection(2, "c", DataType::Text)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "x", DataType::Text)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![col_ref(0, "x", DataType::Text)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Text)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let TypedExprKind::ColumnRef { column_index, .. } = &s.group_by[0].kind {
            assert_eq!(*column_index, 2, "GROUP BY col0 should remap to base col2");
        } else {
            panic!("expected ColumnRef in GROUP BY");
        }
    }

    #[test]
    fn test_outer_having_remap() {
        // Inner: SELECT col1 AS x FROM users → mapping = [1]
        let inner = simple_inner_query("users", vec![projection(1, "b", DataType::Int64)], None);

        let having = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "x", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(10)),
                    DataType::Int64,
                )),
            },
            DataType::Boolean,
        );

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "x", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![col_ref(0, "x", DataType::Int64)],
                having: Some(having),
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Int64)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let h = s.having.as_ref().expect("should have HAVING");
        if let TypedExprKind::BinaryOp { ref left, .. } = h.kind {
            if let TypedExprKind::ColumnRef { column_index, .. } = &left.kind {
                assert_eq!(*column_index, 1, "HAVING col0 should remap to base col1");
            } else {
                panic!("expected ColumnRef in HAVING");
            }
        } else {
            panic!("expected BinaryOp in HAVING");
        }
    }

    #[test]
    fn test_outer_distinct_on_remap() {
        // Inner: SELECT col3 AS x FROM users → mapping = [3]
        let inner = simple_inner_query("users", vec![projection(3, "d", DataType::Text)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "x", DataType::Text)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::DistinctOn(vec![col_ref(0, "x", DataType::Text)]),
            }),
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "x", DataType::Text),
                asc: true,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Text)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let AnalyzedDistinct::DistinctOn(ref exprs) = s.distinct {
            if let TypedExprKind::ColumnRef { column_index, .. } = &exprs[0].kind {
                assert_eq!(
                    *column_index, 3,
                    "DISTINCT ON col0 should remap to base col3"
                );
            } else {
                panic!("expected ColumnRef in DISTINCT ON");
            }
        } else {
            panic!("expected DistinctOn");
        }
    }

    #[test]
    fn test_no_flatten_outer_subquery_expr() {
        // Outer WHERE has a ScalarSubquery — flattening must be skipped because
        // subquery bodies may contain correlated refs (scope_depth > 0) whose
        // column_index references the outer row layout. Column remap does not
        // descend into subquery bodies, so after non-identity remap those
        // correlated refs would read the wrong outer column.
        let inner = simple_inner_query(
            "users",
            vec![
                projection(2, "c", DataType::Int64), // non-identity mapping = [2]
            ],
            None,
        );

        let scalar_subquery_expr = TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(simple_inner_query(
                "other",
                vec![projection(0, "x", DataType::Int64)],
                None,
            ))),
            DataType::Int64,
        );

        let outer_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "a", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(scalar_subquery_expr),
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "a", DataType::Int64)],
            Some(outer_where),
        );

        let result = rewrite_query(query);
        // Must NOT flatten — outer contains subquery expression
        assert!(
            is_subquery_from(&result),
            "should bail out when outer has subquery expressions"
        );
    }

    #[test]
    fn test_no_flatten_outer_exists_expr() {
        // Outer WHERE has an Exists subquery — must not flatten.
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let exists_expr = TypedExpr::new(
            TypedExprKind::Exists {
                subquery: Box::new(simple_inner_query(
                    "other",
                    vec![projection(0, "x", DataType::Int64)],
                    None,
                )),
                negated: false,
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            Some(exists_expr),
        );

        let result = rewrite_query(query);
        assert!(
            is_subquery_from(&result),
            "should bail out when outer has EXISTS expression"
        );
    }

    #[test]
    fn test_no_flatten_outer_in_subquery_expr() {
        // Outer WHERE has IN (subquery) — must not flatten.
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let in_subquery_expr = TypedExpr::new(
            TypedExprKind::InSubquery {
                expr: Box::new(col_ref(0, "id", DataType::Int64)),
                subquery: Box::new(simple_inner_query(
                    "other",
                    vec![projection(0, "x", DataType::Int64)],
                    None,
                )),
                negated: false,
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            Some(in_subquery_expr),
        );

        let result = rewrite_query(query);
        assert!(
            is_subquery_from(&result),
            "should bail out when outer has IN (subquery) expression"
        );
    }

    #[test]
    fn test_column_name_remapped_for_alias() {
        // Inner: SELECT a AS x FROM users → base column name is "a", alias is "x"
        // After flatten, outer ColumnRef should get column_name "a" (base), not "x" (alias)
        let inner = simple_inner_query(
            "users",
            vec![AnalyzedProjection {
                expr: col_ref(0, "a", DataType::Int64),
                output_name: "x".to_string(), // alias
            }],
            None,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![AnalyzedProjection {
                expr: col_ref(0, "x", DataType::Int64), // outer sees alias "x"
                output_name: "x".to_string(),
            }],
            None,
        );

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let TypedExprKind::ColumnRef {
            column_name,
            column_index,
            ..
        } = &s.projection[0].expr.kind
        {
            assert_eq!(column_name, "a", "should use base column name, not alias");
            assert_eq!(*column_index, 0);
        } else {
            panic!("expected ColumnRef in projection");
        }
    }

    // ── Defensive bounds check ───────────────────────────────────────

    #[test]
    fn test_out_of_bounds_column_ref_no_panic() {
        // Inner projects 1 column but outer refs col1 (out of bounds)
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let query = wrap_as_outer(
            inner,
            "v",
            vec![
                projection(0, "id", DataType::Int64),
                projection(1, "oops", DataType::Text), // col1 is out of bounds (only 1 inner proj)
            ],
            None,
        );

        let result = rewrite_query(query);
        // Should NOT flatten — defensive guard kicks in
        assert!(
            is_subquery_from(&result),
            "should bail out on out-of-bounds ref"
        );
    }
}
