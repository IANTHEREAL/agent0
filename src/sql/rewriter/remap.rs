//! Column reference remapping and bounds checking.
//!
//! Recursively remaps `ColumnRef { scope_depth: 0 }` through a column map,
//! respecting subquery expression boundaries.

use crate::sql::analyzer::types::{
    AnalyzedDistinct, TypedExpr, TypedExprKind, TypedOrderByExpr, WindowFrame, WindowFrameBound,
};
use crate::sql::expr::typed_visit::expr_any;

// -- Bounds checking --

/// Check that all ColumnRef { scope_depth: 0 } in the given expressions have
/// column_index within the bounds of the column map and base names.
pub(super) fn remap_is_safe(
    exprs: &[&TypedExpr],
    column_map: &[usize],
    base_names: &[String],
) -> bool {
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

pub(super) fn distinct_remap_is_safe(
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

// -- Column remapping --

/// Recursively remap ColumnRef { scope_depth: 0 } through the column map.
/// Does NOT descend into subquery expression boundaries (ScalarSubquery, Exists,
/// InSubquery.subquery, AnyAll.subquery, ArraySubquery) -- those have independent
/// scopes where scope_depth: 0 means something different.
pub(super) fn remap_column_refs(
    expr: TypedExpr,
    column_map: &[usize],
    base_names: &[String],
) -> TypedExpr {
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

        // Leaves -- return as-is
        TypedExprKind::Constant(_)
        | TypedExprKind::ColumnRef { .. } // scope_depth > 0
        | TypedExprKind::Default
        | TypedExprKind::Parameter { .. } => kind,

        // Subquery boundaries -- do NOT descend
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. } => kind,

        // InSubquery/AnyAll -- remap expr only, NOT subquery
        TypedExprKind::InSubquery {
            expr: inner_expr,
            subquery,
            negated,
        } => TypedExprKind::InSubquery {
            expr: Box::new(remap_column_refs(*inner_expr, column_map, base_names)),
            subquery,
            negated,
        },
        TypedExprKind::TupleInSubquery {
            exprs,
            subquery,
            negated,
        } => TypedExprKind::TupleInSubquery {
            exprs: exprs
                .into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
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
        TypedExprKind::IsDistinctFrom {
            left,
            right,
            negated,
        } => TypedExprKind::IsDistinctFrom {
            left: Box::new(remap_column_refs(*left, column_map, base_names)),
            right: Box::new(remap_column_refs(*right, column_map, base_names)),
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
        TypedExprKind::ScalarArrayCmp {
            expr: inner,
            elems,
            op,
            use_or,
        } => TypedExprKind::ScalarArrayCmp {
            expr: Box::new(remap_column_refs(*inner, column_map, base_names)),
            elems: elems
                .into_iter()
                .map(|e| remap_column_refs(e, column_map, base_names))
                .collect(),
            op,
            use_or,
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

        // Collate
        TypedExprKind::Collate {
            expr,
            collation,
            resolved,
        } => TypedExprKind::Collate {
            expr: Box::new(remap_column_refs(*expr, column_map, base_names)),
            collation,
            resolved,
        },
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
