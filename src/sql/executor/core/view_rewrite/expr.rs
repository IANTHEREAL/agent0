//! Expression-level view expansion.
//!
//! Walks expression trees looking for subquery-bearing nodes (`Subquery`,
//! `Exists`, `InSubquery`, `AnyOp`, `AllOp`) and recursively expands any
//! view references found inside them. Also provides a fast-path predicate
//! (`expr_requires_view_expansion`) that avoids the async walk when no
//! subquery nodes are reachable from a given expression.

use crate::storage::TikvStore;
use anyhow::Result;
use sqlparser::ast::{self, Expr};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

use super::expand_views_in_query_mut;

pub(crate) fn expand_views_in_expr<'a>(
    store: &'a TikvStore,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    expr: &'a mut Expr,
    visible_ctes: &'a HashSet<String>,
    stack: &'a mut Vec<String>,
    stack_set: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        // Fast path: this subtree cannot contain subqueries, so no view
        // expansion work is needed.
        if !expr_requires_view_expansion(expr) {
            return Ok(());
        }

        match expr {
            // ── Subquery-bearing nodes ────────────────────
            Expr::Subquery(q) | Expr::ArraySubquery(q) => {
                Box::pin(expand_views_in_query_mut(
                    store,
                    txn,
                    db_id,
                    search_path,
                    q.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::Exists { subquery, .. } => {
                Box::pin(expand_views_in_query_mut(
                    store,
                    txn,
                    db_id,
                    search_path,
                    subquery.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::InSubquery { expr, subquery, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_query_mut(
                    store,
                    txn,
                    db_id,
                    search_path,
                    subquery.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    left.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    right.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }

            // ── Common expression shapes ───────────────────
            Expr::Nested(e)
            | Expr::Cast { expr: e, .. }
            | Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::IsTrue(e)
            | Expr::IsNotTrue(e)
            | Expr::IsFalse(e)
            | Expr::IsNotFalse(e)
            | Expr::IsUnknown(e)
            | Expr::IsNotUnknown(e)
            | Expr::UnaryOp { expr: e, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    e.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::BinaryOp { left, right, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    left.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    right.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    low.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    high.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::InList { expr, list, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                for e in list {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(op) = operand.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        op.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                for c in conditions {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        c,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                for r in results {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        r,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                if let Some(el) = else_result.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        el.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::Function(func) => {
                for arg in &mut func.args {
                    match arg {
                        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => {
                            Box::pin(expand_views_in_expr(
                                store,
                                txn,
                                db_id,
                                search_path,
                                e,
                                visible_ctes,
                                stack,
                                stack_set,
                            ))
                            .await?;
                        }
                        ast::FunctionArg::Named {
                            arg: ast::FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            Box::pin(expand_views_in_expr(
                                store,
                                txn,
                                db_id,
                                search_path,
                                e,
                                visible_ctes,
                                stack,
                                stack_set,
                            ))
                            .await?;
                        }
                        _ => {}
                    }
                }
                if let Some(filter) = func.filter.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        filter.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                for ob in &mut func.order_by {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        &mut ob.expr,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                if let Some(over) = func.over.as_mut() {
                    match over {
                        ast::WindowType::WindowSpec(spec) => {
                            for e in &mut spec.partition_by {
                                Box::pin(expand_views_in_expr(
                                    store,
                                    txn,
                                    db_id,
                                    search_path,
                                    e,
                                    visible_ctes,
                                    stack,
                                    stack_set,
                                ))
                                .await?;
                            }
                            for ob in &mut spec.order_by {
                                Box::pin(expand_views_in_expr(
                                    store,
                                    txn,
                                    db_id,
                                    search_path,
                                    &mut ob.expr,
                                    visible_ctes,
                                    stack,
                                    stack_set,
                                ))
                                .await?;
                            }
                        }
                        ast::WindowType::NamedWindow(_) => {}
                    }
                }
            }
            Expr::Array(arr) => {
                for e in &mut arr.elem {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::ArrayIndex { obj, indexes } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    obj.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                for e in indexes {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::JsonAccess { left, right, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    left.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    right.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                if let Some(e) = substring_from.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                if let Some(e) = substring_for.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::Trim {
                expr, trim_what, ..
            } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                if let Some(e) = trim_what.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::Position { expr, r#in } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    r#in.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::Extract { expr, .. } | Expr::Ceil { expr, .. } | Expr::Floor { expr, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    expr.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    overlay_what.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    overlay_from.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
                if let Some(e) = overlay_for.as_mut() {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        e.as_mut(),
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
            }
            Expr::AtTimeZone { timestamp, .. } => {
                Box::pin(expand_views_in_expr(
                    store,
                    txn,
                    db_id,
                    search_path,
                    timestamp.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }

            // Everything else: no subqueries possible or irrelevant for view expansion.
            _ => {}
        }

        Ok(())
    })
}

pub(crate) fn expr_requires_view_expansion(expr: &Expr) -> bool {
    let mut stack = vec![expr];

    while let Some(node) = stack.pop() {
        match node {
            Expr::Subquery(_) | Expr::ArraySubquery(_) | Expr::Exists { .. } => return true,
            Expr::InSubquery { .. } | Expr::AnyOp { .. } | Expr::AllOp { .. } => return true,

            Expr::Nested(e)
            | Expr::Cast { expr: e, .. }
            | Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::IsTrue(e)
            | Expr::IsNotTrue(e)
            | Expr::IsFalse(e)
            | Expr::IsNotFalse(e)
            | Expr::IsUnknown(e)
            | Expr::IsNotUnknown(e)
            | Expr::UnaryOp { expr: e, .. } => stack.push(e.as_ref()),

            Expr::BinaryOp { left, right, .. } => {
                stack.push(left.as_ref());
                stack.push(right.as_ref());
            }

            Expr::Between {
                expr, low, high, ..
            } => {
                stack.push(expr.as_ref());
                stack.push(low.as_ref());
                stack.push(high.as_ref());
            }

            Expr::InList { expr, list, .. } => {
                stack.push(expr.as_ref());
                stack.extend(list.iter());
            }

            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(op) = operand.as_ref() {
                    stack.push(op.as_ref());
                }
                stack.extend(conditions.iter());
                stack.extend(results.iter());
                if let Some(el) = else_result.as_ref() {
                    stack.push(el.as_ref());
                }
            }

            Expr::Function(func) => {
                for arg in &func.args {
                    match arg {
                        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => stack.push(e),
                        ast::FunctionArg::Named {
                            arg: ast::FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            stack.push(e);
                        }
                        _ => {}
                    }
                }
                if let Some(filter) = func.filter.as_ref() {
                    stack.push(filter.as_ref());
                }
                stack.extend(func.order_by.iter().map(|ob| &ob.expr));
                if let Some(over) = func.over.as_ref() {
                    match over {
                        ast::WindowType::WindowSpec(spec) => {
                            stack.extend(spec.partition_by.iter());
                            stack.extend(spec.order_by.iter().map(|ob| &ob.expr));
                        }
                        ast::WindowType::NamedWindow(_) => {}
                    }
                }
            }

            Expr::Array(arr) => {
                stack.extend(arr.elem.iter());
            }

            Expr::ArrayIndex { obj, indexes } => {
                stack.push(obj.as_ref());
                stack.extend(indexes.iter());
            }

            Expr::JsonAccess { left, right, .. } => {
                stack.push(left.as_ref());
                stack.push(right.as_ref());
            }

            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                stack.push(expr.as_ref());
                if let Some(e) = substring_from.as_ref() {
                    stack.push(e.as_ref());
                }
                if let Some(e) = substring_for.as_ref() {
                    stack.push(e.as_ref());
                }
            }

            Expr::Trim {
                expr, trim_what, ..
            } => {
                stack.push(expr.as_ref());
                if let Some(e) = trim_what.as_ref() {
                    stack.push(e.as_ref());
                }
            }

            Expr::Position { expr, r#in } => {
                stack.push(expr.as_ref());
                stack.push(r#in.as_ref());
            }

            Expr::Extract { expr, .. } | Expr::Ceil { expr, .. } | Expr::Floor { expr, .. } => {
                stack.push(expr.as_ref());
            }

            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                stack.push(expr.as_ref());
                stack.push(overlay_what.as_ref());
                stack.push(overlay_from.as_ref());
                if let Some(e) = overlay_for.as_ref() {
                    stack.push(e.as_ref());
                }
            }

            Expr::AtTimeZone { timestamp, .. } => {
                stack.push(timestamp.as_ref());
            }

            _ => {}
        }
    }

    false
}
