//! Canonical TypedExpr tree traversal primitives.
//!
//! Two core primitives:
//! - [`for_each_child`]: yields references to each immediate TypedExpr child (read-only)
//! - [`map_children`]: transforms each immediate TypedExpr child, rebuilding the node
//!
//! Combinators built on top:
//! - [`visit_any`]: stack-safe iterative predicate test over all descendants
//! - [`transform_bottom_up`]: recursive bottom-up sync transform
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
            list.iter().for_each(|e| f(e));
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
        | TypedExprKind::Row(args) => args.iter().for_each(|e| f(e)),

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
            args.iter().for_each(|e| f(e));
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
            args.iter().for_each(|e| f(e));
            partition_by.iter().for_each(|e| f(e));
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
    match &expr.kind {
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
            left: Box::new(f(left)),
            op: op.clone(),
            right: Box::new(f(right)),
        },
        TypedExprKind::UnaryOp { op, operand } => TypedExprKind::UnaryOp {
            op: *op,
            operand: Box::new(f(operand)),
        },
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExprKind::Cast {
            expr: Box::new(f(inner)),
            target_type: target_type.clone(),
            cast_context: *cast_context,
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExprKind::IsTest {
            expr: Box::new(f(inner)),
            test: *test,
            negated: *negated,
        },
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExprKind::Between {
            expr: Box::new(f(inner)),
            low: Box::new(f(low)),
            high: Box::new(f(high)),
            negated: *negated,
        },
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExprKind::InList {
            expr: Box::new(f(inner)),
            list: list.iter().map(|e| f(e)).collect(),
            negated: *negated,
        },
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExprKind::Like {
            expr: Box::new(f(inner)),
            pattern: Box::new(f(pattern)),
            escape: escape.as_ref().map(|e| Box::new(f(e))),
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExprKind::SimilarTo {
            expr: Box::new(f(inner)),
            pattern: Box::new(f(pattern)),
            escape: escape.as_ref().map(|e| Box::new(f(e))),
            negated: *negated,
        },
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExprKind::Case {
            operand: operand.as_ref().map(|e| Box::new(f(e))),
            when_clauses: when_clauses.iter().map(|(w, t)| (f(w), f(t))).collect(),
            else_result: else_result.as_ref().map(|e| Box::new(f(e))),
        },
        TypedExprKind::Coalesce(args) => {
            TypedExprKind::Coalesce(args.iter().map(|e| f(e)).collect())
        }
        TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(Box::new(f(a)), Box::new(f(b))),
        TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
            args: args.iter().map(|e| f(e)).collect(),
            is_greatest: *is_greatest,
        },
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExprKind::FunctionCall {
            func: func.clone(),
            args: args.iter().map(|e| f(e)).collect(),
            order_by: map_order_by(order_by, f),
            filter: filter.as_ref().map(|fl| Box::new(f(fl))),
        },
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            order_by,
            filter,
        } => TypedExprKind::AggregateCall {
            func: func.clone(),
            args: args.iter().map(|e| f(e)).collect(),
            distinct: *distinct,
            order_by: map_order_by(order_by, f),
            filter: filter.as_ref().map(|fl| Box::new(f(fl))),
        },
        TypedExprKind::WindowCall {
            func,
            args,
            partition_by,
            order_by,
            window_frame,
        } => TypedExprKind::WindowCall {
            func: func.clone(),
            args: args.iter().map(|e| f(e)).collect(),
            partition_by: partition_by.iter().map(|e| f(e)).collect(),
            order_by: map_order_by(order_by, f),
            window_frame: map_window_frame(window_frame, f),
        },
        TypedExprKind::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => TypedExprKind::InSubquery {
            expr: Box::new(f(inner)),
            subquery: subquery.clone(),
            negated: *negated,
        },
        TypedExprKind::AnyAll {
            expr: inner,
            op,
            subquery,
            is_all,
        } => TypedExprKind::AnyAll {
            expr: Box::new(f(inner)),
            op: op.clone(),
            subquery: subquery.clone(),
            is_all: *is_all,
        },
        TypedExprKind::ArrayLiteral(args) => {
            TypedExprKind::ArrayLiteral(args.iter().map(|e| f(e)).collect())
        }
        TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
            array: Box::new(f(array)),
            index: Box::new(f(index)),
        },
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExprKind::JsonAccess {
            expr: Box::new(f(inner)),
            path: Box::new(f(path)),
            operator: *operator,
        },
        TypedExprKind::Row(args) => TypedExprKind::Row(args.iter().map(|e| f(e)).collect()),
    }
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

// ── Combinator: transform_bottom_up ─────────────────────────

/// Bottom-up sync transform: recurse children first, then apply `f`.
///
/// NOT stack-safe — acceptable since expression trees are shallow in practice.
pub fn transform_bottom_up(
    expr: &TypedExpr,
    f: &mut impl FnMut(TypedExpr) -> TypedExpr,
) -> TypedExpr {
    let kind = map_children(expr, &mut |child| transform_bottom_up(child, f));
    f(TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    })
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
        Ok(match &expr.kind {
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

            // Composite: await each child sequentially
            TypedExprKind::BinaryOp { left, op, right } => TypedExprKind::BinaryOp {
                left: Box::new(t.transform_expr(left).await?),
                op: op.clone(),
                right: Box::new(t.transform_expr(right).await?),
            },
            TypedExprKind::UnaryOp { op, operand } => TypedExprKind::UnaryOp {
                op: *op,
                operand: Box::new(t.transform_expr(operand).await?),
            },
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context,
            } => TypedExprKind::Cast {
                expr: Box::new(t.transform_expr(inner).await?),
                target_type: target_type.clone(),
                cast_context: *cast_context,
            },
            TypedExprKind::IsTest {
                expr: inner,
                test,
                negated,
            } => TypedExprKind::IsTest {
                expr: Box::new(t.transform_expr(inner).await?),
                test: *test,
                negated: *negated,
            },
            TypedExprKind::Between {
                expr: inner,
                low,
                high,
                negated,
            } => TypedExprKind::Between {
                expr: Box::new(t.transform_expr(inner).await?),
                low: Box::new(t.transform_expr(low).await?),
                high: Box::new(t.transform_expr(high).await?),
                negated: *negated,
            },
            TypedExprKind::InList {
                expr: inner,
                list,
                negated,
            } => {
                let e = t.transform_expr(inner).await?;
                let mut new_list = Vec::with_capacity(list.len());
                for item in list {
                    new_list.push(t.transform_expr(item).await?);
                }
                TypedExprKind::InList {
                    expr: Box::new(e),
                    list: new_list,
                    negated: *negated,
                }
            }
            TypedExprKind::Like {
                expr: inner,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => TypedExprKind::Like {
                expr: Box::new(t.transform_expr(inner).await?),
                pattern: Box::new(t.transform_expr(pattern).await?),
                escape: match escape {
                    Some(e) => Some(Box::new(t.transform_expr(e).await?)),
                    None => None,
                },
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            TypedExprKind::SimilarTo {
                expr: inner,
                pattern,
                escape,
                negated,
            } => TypedExprKind::SimilarTo {
                expr: Box::new(t.transform_expr(inner).await?),
                pattern: Box::new(t.transform_expr(pattern).await?),
                escape: match escape {
                    Some(e) => Some(Box::new(t.transform_expr(e).await?)),
                    None => None,
                },
                negated: *negated,
            },
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => {
                let new_operand = match operand {
                    Some(e) => Some(Box::new(t.transform_expr(e).await?)),
                    None => None,
                };
                let mut new_whens = Vec::with_capacity(when_clauses.len());
                for (w, then) in when_clauses {
                    new_whens.push((t.transform_expr(w).await?, t.transform_expr(then).await?));
                }
                let new_else = match else_result {
                    Some(e) => Some(Box::new(t.transform_expr(e).await?)),
                    None => None,
                };
                TypedExprKind::Case {
                    operand: new_operand,
                    when_clauses: new_whens,
                    else_result: new_else,
                }
            }
            TypedExprKind::Coalesce(args) => {
                let mut new = Vec::with_capacity(args.len());
                for a in args {
                    new.push(t.transform_expr(a).await?);
                }
                TypedExprKind::Coalesce(new)
            }
            TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
                Box::new(t.transform_expr(a).await?),
                Box::new(t.transform_expr(b).await?),
            ),
            TypedExprKind::MinMax { args, is_greatest } => {
                let mut new = Vec::with_capacity(args.len());
                for a in args {
                    new.push(t.transform_expr(a).await?);
                }
                TypedExprKind::MinMax {
                    args: new,
                    is_greatest: *is_greatest,
                }
            }
            TypedExprKind::FunctionCall {
                func,
                args,
                order_by,
                filter,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(t.transform_expr(a).await?);
                }
                TypedExprKind::FunctionCall {
                    func: func.clone(),
                    args: new_args,
                    order_by: map_order_by_async(order_by, t).await?,
                    filter: match filter {
                        Some(fl) => Some(Box::new(t.transform_expr(fl).await?)),
                        None => None,
                    },
                }
            }
            TypedExprKind::AggregateCall {
                func,
                args,
                distinct,
                order_by,
                filter,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(t.transform_expr(a).await?);
                }
                TypedExprKind::AggregateCall {
                    func: func.clone(),
                    args: new_args,
                    distinct: *distinct,
                    order_by: map_order_by_async(order_by, t).await?,
                    filter: match filter {
                        Some(fl) => Some(Box::new(t.transform_expr(fl).await?)),
                        None => None,
                    },
                }
            }
            TypedExprKind::WindowCall {
                func,
                args,
                partition_by,
                order_by,
                window_frame,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(t.transform_expr(a).await?);
                }
                let mut new_pb = Vec::with_capacity(partition_by.len());
                for p in partition_by {
                    new_pb.push(t.transform_expr(p).await?);
                }
                TypedExprKind::WindowCall {
                    func: func.clone(),
                    args: new_args,
                    partition_by: new_pb,
                    order_by: map_order_by_async(order_by, t).await?,
                    window_frame: map_window_frame_async(window_frame, t).await?,
                }
            }
            TypedExprKind::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => TypedExprKind::InSubquery {
                expr: Box::new(t.transform_expr(inner).await?),
                subquery: subquery.clone(),
                negated: *negated,
            },
            TypedExprKind::AnyAll {
                expr: inner,
                op,
                subquery,
                is_all,
            } => TypedExprKind::AnyAll {
                expr: Box::new(t.transform_expr(inner).await?),
                op: op.clone(),
                subquery: subquery.clone(),
                is_all: *is_all,
            },
            TypedExprKind::ArrayLiteral(args) => {
                let mut new = Vec::with_capacity(args.len());
                for a in args {
                    new.push(t.transform_expr(a).await?);
                }
                TypedExprKind::ArrayLiteral(new)
            }
            TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
                array: Box::new(t.transform_expr(array).await?),
                index: Box::new(t.transform_expr(index).await?),
            },
            TypedExprKind::JsonAccess {
                expr: inner,
                path,
                operator,
            } => TypedExprKind::JsonAccess {
                expr: Box::new(t.transform_expr(inner).await?),
                path: Box::new(t.transform_expr(path).await?),
                operator: *operator,
            },
            TypedExprKind::Row(args) => {
                let mut new = Vec::with_capacity(args.len());
                for a in args {
                    new.push(t.transform_expr(a).await?);
                }
                TypedExprKind::Row(new)
            }
        })
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
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{BinaryOp, FunctionKind, ResolvedFunction, WindowFrameUnits};
    use crate::types::{DataType, Value};

    fn int_const(v: i32) -> TypedExpr {
        TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
    }

    fn col_ref(idx: usize, name: &str) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: idx,
                column_name: name.to_string(),
            },
            DataType::Int32,
        )
    }

    fn binary_add(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Add,
                right: Box::new(right),
            },
            DataType::Int32,
        )
    }

    #[test]
    fn map_children_round_trip_identity() {
        // map_children with clone should produce equivalent kind
        let expr = binary_add(int_const(1), col_ref(0, "x"));
        let kind = map_children(&expr, &mut |child| child.clone());
        let rebuilt = TypedExpr {
            kind,
            data_type: expr.data_type.clone(),
        };
        assert!(matches!(rebuilt.kind, TypedExprKind::BinaryOp { .. }));
        if let TypedExprKind::BinaryOp { left, right, .. } = &rebuilt.kind {
            assert!(matches!(
                left.kind,
                TypedExprKind::Constant(Value::Int32(1))
            ));
            assert!(matches!(
                right.kind,
                TypedExprKind::ColumnRef {
                    column_index: 0,
                    ..
                }
            ));
        }
    }

    #[test]
    fn visit_any_finds_nested_node() {
        let expr = binary_add(int_const(1), binary_add(int_const(2), col_ref(0, "x")));
        assert!(visit_any(&expr, |e| matches!(
            e.kind,
            TypedExprKind::ColumnRef { .. }
        )));
        assert!(!visit_any(&expr, |e| matches!(
            e.kind,
            TypedExprKind::Default
        )));
    }

    #[test]
    fn visit_any_left_to_right_order() {
        // Verify that visit_any visits nodes in left-to-right DFS order
        let expr = binary_add(int_const(1), int_const(2));
        let mut visited = Vec::new();
        visit_any(&expr, |e| {
            if let TypedExprKind::Constant(Value::Int32(v)) = &e.kind {
                visited.push(*v);
            }
            false // never short-circuit
        });
        assert_eq!(visited, vec![1, 2]);
    }

    #[test]
    fn visit_any_deep_tree_no_stack_overflow() {
        let mut expr = int_const(0);
        for i in 1..2048 {
            expr = binary_add(expr, int_const(i));
        }
        // Should not stack overflow
        assert!(!visit_any(&expr, |_| false));
    }

    #[test]
    fn for_each_child_visits_window_frame_bound() {
        let expr = TypedExpr::new(
            TypedExprKind::WindowCall {
                func: ResolvedFunction {
                    name: "sum".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![int_const(1)],
                partition_by: vec![],
                order_by: vec![],
                window_frame: Some(WindowFrame {
                    units: WindowFrameUnits::Rows,
                    start: WindowFrameBound::Preceding(Some(Box::new(col_ref(0, "n")))),
                    end: None,
                }),
            },
            DataType::Int64,
        );

        let mut found_col_ref = false;
        for_each_child(&expr, &mut |child| {
            if matches!(child.kind, TypedExprKind::ColumnRef { .. }) {
                found_col_ref = true;
            }
        });
        assert!(found_col_ref);
    }

    #[test]
    fn for_each_child_subquery_opaque() {
        use crate::sql::analyzer::types::AnalyzedQueryBody;
        use crate::sql::analyzer::AnalyzedQuery;

        let subquery = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Values(vec![vec![int_const(42)]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32)],
        };
        let expr = TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(subquery)),
            DataType::Int32,
        );

        let mut count = 0;
        for_each_child(&expr, &mut |_| count += 1);
        assert_eq!(count, 0, "ScalarSubquery should yield no children");
    }

    #[test]
    fn transform_bottom_up_replaces_constants() {
        let expr = binary_add(int_const(1), int_const(2));
        let result = transform_bottom_up(&expr, &mut |e| {
            if let TypedExprKind::Constant(Value::Int32(v)) = &e.kind {
                TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(v * 10)),
                    e.data_type.clone(),
                )
            } else {
                e
            }
        });
        if let TypedExprKind::BinaryOp { left, right, .. } = &result.kind {
            assert!(matches!(
                left.kind,
                TypedExprKind::Constant(Value::Int32(10))
            ));
            assert!(matches!(
                right.kind,
                TypedExprKind::Constant(Value::Int32(20))
            ));
        } else {
            panic!("expected BinaryOp");
        }
    }

    #[test]
    fn for_each_child_like_escape() {
        let expr = TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(col_ref(0, "x")),
                pattern: Box::new(int_const(1)),
                escape: Some(Box::new(int_const(2))),
                case_insensitive: false,
                negated: false,
            },
            DataType::Boolean,
        );
        let mut count = 0;
        for_each_child(&expr, &mut |_| count += 1);
        assert_eq!(count, 3, "Like with escape should yield 3 children");
    }

    #[test]
    fn for_each_child_function_order_by_and_filter() {
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "array_agg".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int32,
                },
                args: vec![col_ref(0, "x")],
                order_by: vec![TypedOrderByExpr {
                    expr: col_ref(1, "y"),
                    asc: true,
                    nulls_first: false,
                }],
                filter: Some(Box::new(col_ref(2, "z"))),
            },
            DataType::Int32,
        );
        let mut children = Vec::new();
        for_each_child(&expr, &mut |child| {
            if let TypedExprKind::ColumnRef { column_name, .. } = &child.kind {
                children.push(column_name.clone());
            }
        });
        assert_eq!(children, vec!["x", "y", "z"]);
    }
}
