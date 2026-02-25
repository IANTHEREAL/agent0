//! Canonical TypedExpr tree traversal primitives.
//!
//! Two core primitives:
//! - [`for_each_child`]: yields references to each immediate TypedExpr child (read-only)
//! - [`map_children`]: transforms each immediate TypedExpr child, rebuilding the node
//!
//! Combinators built on top:
//! - [`visit_any`]: stack-safe iterative predicate test over all descendants
//!
//! Async support:
//! - [`AsyncExprTransform`]: trait for async expression transforms
//! - [`map_children_async`]: async version of map_children
//!
//! ## Traversal Order Contract
//!
//! All traversal functions follow **left-to-right, depth-first** order.
//! For `BinaryOp { left, right, .. }`: `left` is visited/transformed before `right`.
//! This matters because `FnMut` callbacks may accumulate state.
//!
//! ## Subquery Boundary Contract
//!
//! These are **expression-level** utilities. `AnalyzedQuery` payloads inside subquery
//! variants (`ScalarSubquery`, `Exists`, `InSubquery`, `AnyAll`, `ArraySubquery`) are
//! **not** traversed — they are opaque boundaries. Callers needing subquery descent
//! must handle those variants explicitly.

use crate::sql::analyzer::types::{
    TypedExpr, TypedExprKind, TypedOrderByExpr, WindowFrame, WindowFrameBound,
};
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;

// ── Primitive 1: for_each_child ─────────────────────────────

/// Yields references to each immediate `TypedExpr` child of this node.
///
/// Children are yielded in left-to-right order.
/// Subquery payloads (`AnalyzedQuery`) are NOT yielded — see module-level docs.
pub fn for_each_child<'a>(expr: &'a TypedExpr, f: &mut impl FnMut(&'a TypedExpr)) {
    match &expr.kind {
        // Leaves: nothing to yield
        TypedExprKind::Constant(_)
        | TypedExprKind::ColumnRef { .. }
        | TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::Default
        | TypedExprKind::Parameter { .. } => {}

        TypedExprKind::BinaryOp { left, right, .. } => {
            f(left);
            f(right);
        }
        TypedExprKind::UnaryOp { operand, .. }
        | TypedExprKind::Cast { expr: operand, .. }
        | TypedExprKind::IsTest { expr: operand, .. } => f(operand),

        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            f(expr);
            f(low);
            f(high);
        }
        TypedExprKind::InList { expr, list, .. } => {
            f(expr);
            list.iter().for_each(f);
        }
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        }
        | TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            f(expr);
            f(pattern);
            if let Some(e) = escape {
                f(e);
            }
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            if let Some(op) = operand {
                f(op);
            }
            for (w, t) in when_clauses {
                f(w);
                f(t);
            }
            if let Some(e) = else_result {
                f(e);
            }
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => args.iter().for_each(f),

        TypedExprKind::NullIf(a, b) => {
            f(a);
            f(b);
        }
        TypedExprKind::FunctionCall {
            args,
            order_by,
            filter,
            ..
        }
        | TypedExprKind::AggregateCall {
            args,
            order_by,
            filter,
            ..
        } => {
            args.iter().for_each(&mut *f);
            order_by.iter().for_each(|ob| f(&ob.expr));
            if let Some(fl) = filter {
                f(fl);
            }
        }
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            window_frame,
            ..
        } => {
            args.iter().for_each(&mut *f);
            partition_by.iter().for_each(&mut *f);
            order_by.iter().for_each(|ob| f(&ob.expr));
            if let Some(frame) = window_frame {
                for_each_frame_bound_child(&frame.start, f);
                if let Some(end) = &frame.end {
                    for_each_frame_bound_child(end, f);
                }
            }
        }
        TypedExprKind::InSubquery { expr, .. } | TypedExprKind::AnyAll { expr, .. } => f(expr),

        TypedExprKind::ArrayIndex { array, index } => {
            f(array);
            f(index);
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            f(expr);
            f(path);
        }
        TypedExprKind::Collate { expr, .. } => f(expr),
    }
}

fn for_each_frame_bound_child<'a>(bound: &'a WindowFrameBound, f: &mut impl FnMut(&'a TypedExpr)) {
    match bound {
        WindowFrameBound::CurrentRow => {}
        WindowFrameBound::Preceding(v) | WindowFrameBound::Following(v) => {
            if let Some(e) = v {
                f(e);
            }
        }
    }
}

macro_rules! sync_map_one {
    ($f:expr, $child:expr) => {
        $f($child)
    };
}

macro_rules! sync_map_vec {
    ($f:expr, $items:expr) => {
        $items.iter().map(&mut *$f).collect()
    };
}

macro_rules! sync_map_pairs {
    ($f:expr, $pairs:expr) => {
        $pairs.iter().map(|(w, t)| ($f(w), $f(t))).collect()
    };
}

macro_rules! sync_map_opt_box {
    ($f:expr, $opt:expr) => {
        $opt.as_ref().map(|e| Box::new($f(e)))
    };
}

macro_rules! sync_map_order_by {
    ($f:expr, $order_by:expr) => {
        map_order_by($order_by, $f)
    };
}

macro_rules! sync_map_window_frame {
    ($f:expr, $window_frame:expr) => {
        map_window_frame($window_frame, $f)
    };
}

macro_rules! async_map_one {
    ($t:expr, $child:expr) => {
        $t.transform_expr($child).await?
    };
}

macro_rules! async_map_vec {
    ($t:expr, $items:expr) => {{
        let mut mapped = Vec::with_capacity($items.len());
        for item in $items.iter() {
            mapped.push($t.transform_expr(item).await?);
        }
        mapped
    }};
}

macro_rules! async_map_pairs {
    ($t:expr, $pairs:expr) => {{
        let mut mapped = Vec::with_capacity($pairs.len());
        for (when_expr, then_expr) in $pairs.iter() {
            mapped.push((
                $t.transform_expr(when_expr).await?,
                $t.transform_expr(then_expr).await?,
            ));
        }
        mapped
    }};
}

macro_rules! async_map_opt_box {
    ($t:expr, $opt:expr) => {{
        match $opt {
            Some(e) => Some(Box::new($t.transform_expr(e).await?)),
            None => None,
        }
    }};
}

macro_rules! async_map_order_by {
    ($t:expr, $order_by:expr) => {
        map_order_by_async($order_by, $t).await?
    };
}

macro_rules! async_map_window_frame {
    ($t:expr, $window_frame:expr) => {
        map_window_frame_async($window_frame, $t).await?
    };
}

macro_rules! map_children_match {
    (
        $expr:expr,
        $mapper:expr,
        $map_one:ident,
        $map_vec:ident,
        $map_pairs:ident,
        $map_opt_box:ident,
        $map_order_by:ident,
        $map_window_frame:ident
    ) => {
        match &$expr.kind {
            // Leaves: clone as-is
            TypedExprKind::Constant(v) => TypedExprKind::Constant(v.clone()),
            TypedExprKind::ColumnRef {
                scope_depth,
                column_index,
                column_name,
            } => TypedExprKind::ColumnRef {
                scope_depth: *scope_depth,
                column_index: *column_index,
                column_name: column_name.clone(),
            },
            TypedExprKind::ScalarSubquery(q) => TypedExprKind::ScalarSubquery(q.clone()),
            TypedExprKind::ArraySubquery(q) => TypedExprKind::ArraySubquery(q.clone()),
            TypedExprKind::Exists { subquery, negated } => TypedExprKind::Exists {
                subquery: subquery.clone(),
                negated: *negated,
            },
            TypedExprKind::Default => TypedExprKind::Default,
            TypedExprKind::Parameter { index } => TypedExprKind::Parameter { index: *index },

            // Composite nodes: transform children
            TypedExprKind::BinaryOp { left, op, right } => TypedExprKind::BinaryOp {
                left: Box::new($map_one!($mapper, left)),
                op: op.clone(),
                right: Box::new($map_one!($mapper, right)),
            },
            TypedExprKind::UnaryOp { op, operand } => TypedExprKind::UnaryOp {
                op: *op,
                operand: Box::new($map_one!($mapper, operand)),
            },
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context,
            } => TypedExprKind::Cast {
                expr: Box::new($map_one!($mapper, inner)),
                target_type: target_type.clone(),
                cast_context: *cast_context,
            },
            TypedExprKind::IsTest {
                expr: inner,
                test,
                negated,
            } => TypedExprKind::IsTest {
                expr: Box::new($map_one!($mapper, inner)),
                test: *test,
                negated: *negated,
            },
            TypedExprKind::Between {
                expr: inner,
                low,
                high,
                negated,
            } => TypedExprKind::Between {
                expr: Box::new($map_one!($mapper, inner)),
                low: Box::new($map_one!($mapper, low)),
                high: Box::new($map_one!($mapper, high)),
                negated: *negated,
            },
            TypedExprKind::InList {
                expr: inner,
                list,
                negated,
            } => TypedExprKind::InList {
                expr: Box::new($map_one!($mapper, inner)),
                list: $map_vec!($mapper, list),
                negated: *negated,
            },
            TypedExprKind::Like {
                expr: inner,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => TypedExprKind::Like {
                expr: Box::new($map_one!($mapper, inner)),
                pattern: Box::new($map_one!($mapper, pattern)),
                escape: $map_opt_box!($mapper, escape),
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            TypedExprKind::SimilarTo {
                expr: inner,
                pattern,
                escape,
                negated,
            } => TypedExprKind::SimilarTo {
                expr: Box::new($map_one!($mapper, inner)),
                pattern: Box::new($map_one!($mapper, pattern)),
                escape: $map_opt_box!($mapper, escape),
                negated: *negated,
            },
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => TypedExprKind::Case {
                operand: $map_opt_box!($mapper, operand),
                when_clauses: $map_pairs!($mapper, when_clauses),
                else_result: $map_opt_box!($mapper, else_result),
            },
            TypedExprKind::Coalesce(args) => TypedExprKind::Coalesce($map_vec!($mapper, args)),
            TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
                Box::new($map_one!($mapper, a)),
                Box::new($map_one!($mapper, b)),
            ),
            TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
                args: $map_vec!($mapper, args),
                is_greatest: *is_greatest,
            },
            TypedExprKind::FunctionCall {
                func,
                args,
                order_by,
                filter,
            } => TypedExprKind::FunctionCall {
                func: func.clone(),
                args: $map_vec!($mapper, args),
                order_by: $map_order_by!($mapper, order_by),
                filter: $map_opt_box!($mapper, filter),
            },
            TypedExprKind::AggregateCall {
                func,
                args,
                distinct,
                order_by,
                filter,
            } => TypedExprKind::AggregateCall {
                func: func.clone(),
                args: $map_vec!($mapper, args),
                distinct: *distinct,
                order_by: $map_order_by!($mapper, order_by),
                filter: $map_opt_box!($mapper, filter),
            },
            TypedExprKind::WindowCall {
                func,
                args,
                partition_by,
                order_by,
                window_frame,
            } => TypedExprKind::WindowCall {
                func: func.clone(),
                args: $map_vec!($mapper, args),
                partition_by: $map_vec!($mapper, partition_by),
                order_by: $map_order_by!($mapper, order_by),
                window_frame: $map_window_frame!($mapper, window_frame),
            },
            TypedExprKind::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => TypedExprKind::InSubquery {
                expr: Box::new($map_one!($mapper, inner)),
                subquery: subquery.clone(),
                negated: *negated,
            },
            TypedExprKind::AnyAll {
                expr: inner,
                op,
                subquery,
                is_all,
            } => TypedExprKind::AnyAll {
                expr: Box::new($map_one!($mapper, inner)),
                op: op.clone(),
                subquery: subquery.clone(),
                is_all: *is_all,
            },
            TypedExprKind::ArrayLiteral(args) => {
                TypedExprKind::ArrayLiteral($map_vec!($mapper, args))
            }
            TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
                array: Box::new($map_one!($mapper, array)),
                index: Box::new($map_one!($mapper, index)),
            },
            TypedExprKind::JsonAccess {
                expr: inner,
                path,
                operator,
            } => TypedExprKind::JsonAccess {
                expr: Box::new($map_one!($mapper, inner)),
                path: Box::new($map_one!($mapper, path)),
                operator: *operator,
            },
            TypedExprKind::Row(args) => TypedExprKind::Row($map_vec!($mapper, args)),
            TypedExprKind::Collate {
                expr,
                collation,
                resolved,
            } => TypedExprKind::Collate {
                expr: Box::new($map_one!($mapper, expr)),
                collation: collation.clone(),
                resolved: resolved.clone(),
            },
        }
    };
}

// ── Primitive 2: map_children ───────────────────────────────

/// Transform each immediate `TypedExpr` child via `f`, rebuilding the `TypedExprKind`.
///
/// Non-expression fields (op, negated, func, etc.) are cloned/copied.
/// Returns the new `TypedExprKind`; caller wraps with `data_type`.
/// Children are transformed in left-to-right order.
pub fn map_children(
    expr: &TypedExpr,
    f: &mut impl FnMut(&TypedExpr) -> TypedExpr,
) -> TypedExprKind {
    map_children_match!(
        expr,
        f,
        sync_map_one,
        sync_map_vec,
        sync_map_pairs,
        sync_map_opt_box,
        sync_map_order_by,
        sync_map_window_frame
    )
}

fn map_order_by(
    order_by: &[TypedOrderByExpr],
    f: &mut impl FnMut(&TypedExpr) -> TypedExpr,
) -> Vec<TypedOrderByExpr> {
    order_by
        .iter()
        .map(|ob| TypedOrderByExpr {
            expr: f(&ob.expr),
            asc: ob.asc,
            nulls_first: ob.nulls_first,
        })
        .collect()
}

fn map_window_frame(
    frame: &Option<WindowFrame>,
    f: &mut impl FnMut(&TypedExpr) -> TypedExpr,
) -> Option<WindowFrame> {
    frame.as_ref().map(|wf| WindowFrame {
        units: wf.units,
        start: map_frame_bound(&wf.start, f),
        end: wf.end.as_ref().map(|b| map_frame_bound(b, f)),
    })
}

fn map_frame_bound(
    bound: &WindowFrameBound,
    f: &mut impl FnMut(&TypedExpr) -> TypedExpr,
) -> WindowFrameBound {
    match bound {
        WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
        WindowFrameBound::Preceding(v) => {
            WindowFrameBound::Preceding(v.as_ref().map(|e| Box::new(f(e))))
        }
        WindowFrameBound::Following(v) => {
            WindowFrameBound::Following(v.as_ref().map(|e| Box::new(f(e))))
        }
    }
}

// ── Combinator: visit_any ───────────────────────────────────

/// Stack-safe iterative visit. Returns `true` if `predicate` matches any node.
///
/// Traversal is left-to-right depth-first: children are pushed in reverse order
/// onto the LIFO stack so the leftmost child is popped (visited) first.
pub fn visit_any(expr: &TypedExpr, mut predicate: impl FnMut(&TypedExpr) -> bool) -> bool {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        if predicate(node) {
            return true;
        }
        // Collect children, then push in reverse for left-to-right pop order.
        let before = stack.len();
        for_each_child(node, &mut |child| stack.push(child));
        stack[before..].reverse();
    }
    false
}

// ── Async support ───────────────────────────────────────────

/// Trait for async expression transforms.
///
/// Uses boxed futures because Rust requires boxing for recursive async.
/// Implementors handle their domain-specific variants and delegate the
/// generic child-recursion `_ =>` arm to [`map_children_async`].
pub(crate) trait AsyncExprTransform: Send {
    fn transform_expr<'a>(
        &'a mut self,
        expr: &'a TypedExpr,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>>;
}

/// Async version of [`map_children`]. Children are processed **sequentially**
/// (left-to-right) to support `&mut self` state (e.g., `&mut Transaction`).
///
/// Returns the new `TypedExprKind`; caller wraps with `data_type`.
pub(crate) fn map_children_async<'a, T: AsyncExprTransform>(
    expr: &'a TypedExpr,
    t: &'a mut T,
) -> Pin<Box<dyn Future<Output = Result<TypedExprKind>> + Send + 'a>> {
    Box::pin(async move {
        Ok(map_children_match!(
            expr,
            t,
            async_map_one,
            async_map_vec,
            async_map_pairs,
            async_map_opt_box,
            async_map_order_by,
            async_map_window_frame
        ))
    })
}

async fn map_order_by_async<T: AsyncExprTransform>(
    order_by: &[TypedOrderByExpr],
    t: &mut T,
) -> Result<Vec<TypedOrderByExpr>> {
    let mut result = Vec::with_capacity(order_by.len());
    for ob in order_by {
        result.push(TypedOrderByExpr {
            expr: t.transform_expr(&ob.expr).await?,
            asc: ob.asc,
            nulls_first: ob.nulls_first,
        });
    }
    Ok(result)
}

async fn map_window_frame_async<T: AsyncExprTransform>(
    frame: &Option<WindowFrame>,
    t: &mut T,
) -> Result<Option<WindowFrame>> {
    match frame {
        Some(wf) => Ok(Some(WindowFrame {
            units: wf.units,
            start: map_frame_bound_async(&wf.start, t).await?,
            end: match &wf.end {
                Some(b) => Some(map_frame_bound_async(b, t).await?),
                None => None,
            },
        })),
        None => Ok(None),
    }
}

async fn map_frame_bound_async<T: AsyncExprTransform>(
    bound: &WindowFrameBound,
    t: &mut T,
) -> Result<WindowFrameBound> {
    match bound {
        WindowFrameBound::CurrentRow => Ok(WindowFrameBound::CurrentRow),
        WindowFrameBound::Preceding(v) => match v {
            Some(e) => Ok(WindowFrameBound::Preceding(Some(Box::new(
                t.transform_expr(e).await?,
            )))),
            None => Ok(WindowFrameBound::Preceding(None)),
        },
        WindowFrameBound::Following(v) => match v {
            Some(e) => Ok(WindowFrameBound::Following(Some(Box::new(
                t.transform_expr(e).await?,
            )))),
            None => Ok(WindowFrameBound::Following(None)),
        },
    }
}

#[cfg(test)]
mod tests;
