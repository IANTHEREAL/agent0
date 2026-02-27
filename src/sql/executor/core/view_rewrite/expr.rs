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

#[cfg(test)]
mod tests {
    use super::expr_requires_view_expansion;
    use crate::sql::parse_sql;
    use sqlparser::ast::{Expr, SelectItem, SetExpr, Statement};

    fn parse_projection_expr(expr_sql: &str) -> Expr {
        let mut stmts = parse_sql(&format!("SELECT {}", expr_sql)).expect("parse sql");
        let stmt = stmts.remove(0);
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let SetExpr::Select(s) = *q.body else {
            panic!("expected select");
        };
        let item = s.projection.into_iter().next().expect("projection item");
        match item {
            SelectItem::UnnamedExpr(e) => e,
            _ => panic!("expected unnamed expr"),
        }
    }

    fn parse_query_projection_expr(query_sql: &str) -> Expr {
        let mut stmts = parse_sql(query_sql).expect("parse sql");
        let stmt = stmts.remove(0);
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let SetExpr::Select(s) = *q.body else {
            panic!("expected select");
        };
        let item = s.projection.into_iter().next().expect("projection item");
        match item {
            SelectItem::UnnamedExpr(e) => e,
            _ => panic!("expected unnamed expr"),
        }
    }

    #[test]
    fn no_subquery_shapes_do_not_require_expansion() {
        let expr = parse_projection_expr("((a + 1) * 2) BETWEEN 1 AND 10");
        assert!(!expr_requires_view_expansion(&expr));

        let expr = parse_projection_expr(
            "CASE WHEN a > 1 THEN substring(b from 1 for 2) ELSE trim(c) END",
        );
        assert!(!expr_requires_view_expansion(&expr));
    }

    #[test]
    fn direct_subquery_shapes_require_expansion() {
        let e1 = parse_projection_expr("(SELECT 1)");
        assert!(expr_requires_view_expansion(&e1));

        let e2 = parse_projection_expr("EXISTS (SELECT 1)");
        assert!(expr_requires_view_expansion(&e2));

        let e3 = parse_projection_expr("a IN (SELECT x FROM t)");
        assert!(expr_requires_view_expansion(&e3));
    }

    #[test]
    fn any_all_ops_require_expansion_when_subquery_present() {
        let e1 = parse_projection_expr("a = ANY(SELECT x FROM t)");
        assert!(expr_requires_view_expansion(&e1));

        let e2 = parse_projection_expr("a = ALL(SELECT x FROM t)");
        assert!(expr_requires_view_expansion(&e2));
    }

    #[test]
    fn nested_subquery_inside_function_is_detected() {
        let expr = parse_projection_expr(
            "coalesce(1, (SELECT max(x) FROM t), CASE WHEN 1=1 THEN 2 ELSE 3 END)",
        );
        assert!(expr_requires_view_expansion(&expr));
    }

    #[test]
    fn specialized_expression_shapes_detect_subqueries() {
        let cases = [
            "array[(SELECT 1)]",
            "(ARRAY[1,2])[ (SELECT 1) ]",
            "'{\"k\":1}'::jsonb -> (SELECT 'k')",
            "substring((SELECT 'abc') from 1 for 2)",
            "trim((SELECT 'abc'))",
            "position((SELECT 'a') in 'abc')",
            "extract(epoch from (SELECT now()))",
            "overlay((SELECT 'abc') placing 'X' from 1 for 1)",
            "(SELECT now()) AT TIME ZONE 'UTC'",
        ];

        for sql in cases {
            let expr = parse_projection_expr(sql);
            assert!(
                expr_requires_view_expansion(&expr),
                "expected subquery detection for: {}",
                sql
            );
        }
    }

    #[test]
    fn specialized_expression_shapes_without_subquery_do_not_require_expansion() {
        let expr = parse_projection_expr(
            "overlay(substring(trim('abc') from 1 for 2) placing 'X' from 1 for 1)",
        );
        assert!(!expr_requires_view_expansion(&expr));
    }

    #[test]
    fn unary_cast_and_null_predicates_follow_subquery_presence() {
        let cases = [
            ("-(SELECT 1)", true),
            ("(SELECT 1)::int", true),
            ("(SELECT 1) IS NULL", true),
            ("(SELECT 1) IS NOT NULL", true),
            ("(SELECT true) IS TRUE", true),
            ("(SELECT false) IS NOT FALSE", true),
            ("(SELECT NULL) IS UNKNOWN", true),
            ("(SELECT 1) IS NOT UNKNOWN", true),
            ("-1", false),
            ("1::int", false),
            ("1 IS NULL", false),
        ];

        for (sql, expected) in cases {
            let expr = parse_projection_expr(sql);
            assert_eq!(expr_requires_view_expansion(&expr), expected, "sql={sql}");
        }
    }

    #[test]
    fn in_list_and_case_branches_detect_nested_subqueries() {
        let e1 = parse_projection_expr("1 IN (2, 3, (SELECT 4))");
        assert!(expr_requires_view_expansion(&e1));

        let e2 = parse_projection_expr("CASE (SELECT 1) WHEN 1 THEN 2 ELSE 3 END");
        assert!(expr_requires_view_expansion(&e2));

        let e3 = parse_projection_expr("CASE WHEN 1=1 THEN 2 ELSE 3 END");
        assert!(!expr_requires_view_expansion(&e3));
    }

    #[test]
    fn function_filter_and_window_spec_branches_are_walked() {
        let expr = parse_query_projection_expr(
            "SELECT sum(x) FILTER (WHERE EXISTS (SELECT 1)) OVER (PARTITION BY (SELECT 2) ORDER BY (SELECT 3)) FROM t",
        );
        assert!(expr_requires_view_expansion(&expr));

        let expr =
            parse_query_projection_expr("SELECT sum(x) OVER w FROM t WINDOW w AS (PARTITION BY x)");
        assert!(!expr_requires_view_expansion(&expr));
    }

    #[test]
    fn array_and_json_shapes_follow_subquery_presence() {
        let with_subquery = [
            "array[(SELECT 1), 2]",
            "(ARRAY[1,2])[1][(SELECT 1)]",
            "'{\"a\":1}'::jsonb -> (SELECT 'a')",
        ];
        for sql in with_subquery {
            let expr = parse_projection_expr(sql);
            assert!(expr_requires_view_expansion(&expr), "sql={sql}");
        }

        let without_subquery = [
            "array[1, 2, 3]",
            "(ARRAY[1,2])[1]",
            "'{\"a\":1}'::jsonb -> 'a'",
        ];
        for sql in without_subquery {
            let expr = parse_projection_expr(sql);
            assert!(!expr_requires_view_expansion(&expr), "sql={sql}");
        }
    }

    #[test]
    fn binary_between_and_position_shapes_cover_positive_and_negative() {
        let expr = parse_projection_expr("((SELECT 1) + 2) BETWEEN 1 AND 10");
        assert!(expr_requires_view_expansion(&expr));

        let expr = parse_projection_expr("position('a' in (SELECT 'abc'))");
        assert!(expr_requires_view_expansion(&expr));

        let expr = parse_projection_expr("(1 + 2) BETWEEN 1 AND 10");
        assert!(!expr_requires_view_expansion(&expr));
    }
}
