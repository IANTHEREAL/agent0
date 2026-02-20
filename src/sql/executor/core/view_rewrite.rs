//! View expansion rewrite.
//!
//! tipg stores views as SQL text in the catalog. Historically, `get_table_data`
//! expanded views at runtime (executor layer) by re-parsing and executing the
//! view query when a FROM item referenced a view name.
//!
//! This module moves that work into a pre-analysis rewrite step:
//! `FROM my_view` becomes `FROM (<my_view_query>) AS my_view`.
//!
//! The Analyzer and planner then see a normal subquery tree, and the executor
//! no longer needs any view-special cases when loading table data.

use crate::sql::names;
use crate::storage::TikvStore;
use crate::types::ViewDef;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    self, Expr, JoinConstraint, Query, Select, SelectItem, SetExpr, TableFactor, TableWithJoins,
    Values,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

const MAX_VIEW_EXPANSION_DEPTH: usize = 64;

/// Expand all view references inside a query AST.
///
/// Returns a rewritten `Query` where each `FROM view_name` is replaced by a
/// derived table containing the view definition:
///
/// ```sql
/// FROM my_view
/// -- becomes
/// FROM ( <view query> ) AS my_view
/// ```
pub(crate) async fn expand_views_in_query(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
) -> Result<Query> {
    let mut rewritten = crate::sql::stack_safety::with_grown_stack(|| query.clone());
    let mut stack: Vec<String> = Vec::new();
    let mut stack_set: HashSet<String> = HashSet::new();
    let visible_ctes: HashSet<String> = HashSet::new();

    expand_views_in_query_mut(
        store,
        txn,
        db_id,
        search_path,
        &mut rewritten,
        &visible_ctes,
        &mut stack,
        &mut stack_set,
    )
    .await?;

    Ok(rewritten)
}

fn normalize_object_name(name: &ast::ObjectName) -> String {
    name.0
        .iter()
        .map(names::normalize_ident)
        .collect::<Vec<_>>()
        .join(".")
}

async fn resolve_view(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &str,
) -> Result<Option<(String, ViewDef)>> {
    if name.contains('.') {
        // Schema-qualified: try directly, no search_path fallback.
        if let Some(view_def) = store.get_view(txn, db_id, name).await? {
            return Ok(Some((name.to_string(), view_def)));
        }
        return Ok(None);
    }

    // Unqualified: try bare name first, then search_path (or public if empty).
    if let Some(view_def) = store.get_view(txn, db_id, name).await? {
        return Ok(Some((name.to_string(), view_def)));
    }

    let schemas: Vec<&str> = if search_path.is_empty() {
        vec!["public"]
    } else {
        search_path.iter().map(|s| s.as_str()).collect()
    };

    for schema in schemas {
        let full = format!("{}.{}", schema, name);
        if let Some(view_def) = store.get_view(txn, db_id, &full).await? {
            return Ok(Some((full, view_def)));
        }
    }

    Ok(None)
}

fn parse_view_query(view_full_name: &str, sql: &str) -> Result<Query> {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| anyhow!("failed to parse view '{}' query: {}", view_full_name, e))?;
    let Some(stmt) = stmts.into_iter().next() else {
        return Err(anyhow!("view '{}' has empty query text", view_full_name));
    };
    match stmt {
        ast::Statement::Query(q) => Ok(*q),
        _ => Err(anyhow!(
            "view '{}' does not contain a SELECT query",
            view_full_name
        )),
    }
}

fn expand_views_in_query_mut<'a>(
    store: &'a TikvStore,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    query: &'a mut Query,
    outer_visible_ctes: &'a HashSet<String>,
    stack: &'a mut Vec<String>,
    stack_set: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        // CTE scoping: a CTE is visible to subsequent CTEs (and to the main body).
        let mut ctes_so_far: HashSet<String> = HashSet::new();
        if let Some(with) = query.with.as_mut() {
            for cte in &mut with.cte_tables {
                let cte_name = names::normalize_ident(&cte.alias.name);
                let mut visible = outer_visible_ctes.clone();
                visible.extend(ctes_so_far.iter().cloned());
                if with.recursive {
                    visible.insert(cte_name.clone());
                }

                Box::pin(expand_views_in_query_mut(
                    store,
                    txn,
                    db_id,
                    search_path,
                    cte.query.as_mut(),
                    &visible,
                    stack,
                    stack_set,
                ))
                .await?;

                ctes_so_far.insert(cte_name);
            }
        }

        let mut visible_ctes = outer_visible_ctes.clone();
        visible_ctes.extend(ctes_so_far.iter().cloned());

        // Body (SELECT / set operations / VALUES).
        Box::pin(expand_views_in_set_expr(
            store,
            txn,
            db_id,
            search_path,
            query.body.as_mut(),
            &visible_ctes,
            stack,
            stack_set,
        ))
        .await?;

        // ORDER BY / LIMIT / OFFSET expressions may contain subqueries.
        for ob in &mut query.order_by {
            Box::pin(expand_views_in_expr(
                store,
                txn,
                db_id,
                search_path,
                &mut ob.expr,
                &visible_ctes,
                stack,
                stack_set,
            ))
            .await?;
        }
        if let Some(limit) = query.limit.as_mut() {
            Box::pin(expand_views_in_expr(
                store,
                txn,
                db_id,
                search_path,
                limit,
                &visible_ctes,
                stack,
                stack_set,
            ))
            .await?;
        }
        if let Some(offset) = query.offset.as_mut() {
            Box::pin(expand_views_in_expr(
                store,
                txn,
                db_id,
                search_path,
                &mut offset.value,
                &visible_ctes,
                stack,
                stack_set,
            ))
            .await?;
        }

        Ok(())
    })
}

fn expand_views_in_set_expr<'a>(
    store: &'a TikvStore,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    set_expr: &'a mut SetExpr,
    visible_ctes: &'a HashSet<String>,
    stack: &'a mut Vec<String>,
    stack_set: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        match set_expr {
            SetExpr::Select(select) => {
                Box::pin(expand_views_in_select(
                    store,
                    txn,
                    db_id,
                    search_path,
                    select.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }
            SetExpr::Query(q) => {
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
            SetExpr::SetOperation { left, right, .. } => {
                Box::pin(expand_views_in_set_expr(
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
                Box::pin(expand_views_in_set_expr(
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
            SetExpr::Values(Values { rows, .. }) => {
                for row in rows {
                    for expr in row {
                        Box::pin(expand_views_in_expr(
                            store,
                            txn,
                            db_id,
                            search_path,
                            expr,
                            visible_ctes,
                            stack,
                            stack_set,
                        ))
                        .await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })
}

fn expand_views_in_select<'a>(
    store: &'a TikvStore,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    select: &'a mut Select,
    visible_ctes: &'a HashSet<String>,
    stack: &'a mut Vec<String>,
    stack_set: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        // Projection expressions (may contain subqueries).
        for item in &mut select.projection {
            match item {
                SelectItem::UnnamedExpr(e) => {
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
                SelectItem::ExprWithAlias { expr, .. } => {
                    Box::pin(expand_views_in_expr(
                        store,
                        txn,
                        db_id,
                        search_path,
                        expr,
                        visible_ctes,
                        stack,
                        stack_set,
                    ))
                    .await?;
                }
                _ => {}
            }
        }

        // FROM / JOIN table factors.
        for twj in &mut select.from {
            Box::pin(expand_views_in_table_with_joins(
                store,
                txn,
                db_id,
                search_path,
                twj,
                visible_ctes,
                stack,
                stack_set,
            ))
            .await?;
        }

        // WHERE / HAVING / GROUP BY can contain subqueries.
        if let Some(selection) = select.selection.as_mut() {
            Box::pin(expand_views_in_expr(
                store,
                txn,
                db_id,
                search_path,
                selection,
                visible_ctes,
                stack,
                stack_set,
            ))
            .await?;
        }

        match &mut select.group_by {
            ast::GroupByExpr::All => {}
            ast::GroupByExpr::Expressions(exprs) => {
                for e in exprs {
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
        }

        if let Some(having) = select.having.as_mut() {
            Box::pin(expand_views_in_expr(
                store,
                txn,
                db_id,
                search_path,
                having,
                visible_ctes,
                stack,
                stack_set,
            ))
            .await?;
        }

        Ok(())
    })
}

fn expand_views_in_table_with_joins<'a>(
    store: &'a TikvStore,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    twj: &'a mut TableWithJoins,
    visible_ctes: &'a HashSet<String>,
    stack: &'a mut Vec<String>,
    stack_set: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        Box::pin(expand_views_in_table_factor(
            store,
            txn,
            db_id,
            search_path,
            &mut twj.relation,
            visible_ctes,
            stack,
            stack_set,
        ))
        .await?;

        for join in &mut twj.joins {
            Box::pin(expand_views_in_table_factor(
                store,
                txn,
                db_id,
                search_path,
                &mut join.relation,
                visible_ctes,
                stack,
                stack_set,
            ))
            .await?;

            // JOIN ON expressions may contain subqueries.
            match &mut join.join_operator {
                ast::JoinOperator::Inner(JoinConstraint::On(e))
                | ast::JoinOperator::LeftOuter(JoinConstraint::On(e))
                | ast::JoinOperator::RightOuter(JoinConstraint::On(e))
                | ast::JoinOperator::FullOuter(JoinConstraint::On(e)) => {
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

        Ok(())
    })
}

fn expand_views_in_table_factor<'a>(
    store: &'a TikvStore,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    factor: &'a mut TableFactor,
    visible_ctes: &'a HashSet<String>,
    stack: &'a mut Vec<String>,
    stack_set: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                // Table functions / table-valued functions in FROM.
                if args.is_some() {
                    return Ok(());
                }

                let raw_name = normalize_object_name(name);

                // CTEs shadow relations: never expand a CTE reference as a view.
                let is_unqualified = name.0.len() == 1;
                if is_unqualified && visible_ctes.contains(&raw_name) {
                    return Ok(());
                }

                let Some((resolved_full, view_def)) =
                    resolve_view(store, txn, db_id, search_path, &raw_name).await?
                else {
                    return Ok(());
                };

                if stack_set.contains(&resolved_full) {
                    return Err(anyhow!("recursive view detected: {}", resolved_full));
                }
                if stack.len() >= MAX_VIEW_EXPANSION_DEPTH {
                    return Err(anyhow!(
                        "view expansion depth exceeded ({}): {}",
                        MAX_VIEW_EXPANSION_DEPTH,
                        resolved_full
                    ));
                }

                stack.push(resolved_full.clone());
                stack_set.insert(resolved_full.clone());

                let mut view_query = parse_view_query(&resolved_full, &view_def.query)?;

                let expand_result = Box::pin(expand_views_in_query_mut(
                    store,
                    txn,
                    db_id,
                    search_path,
                    &mut view_query,
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await;

                stack.pop();
                stack_set.remove(&resolved_full);

                expand_result?;

                let derived_alias = alias.clone().unwrap_or_else(|| ast::TableAlias {
                    name: name
                        .0
                        .last()
                        .cloned()
                        .unwrap_or_else(|| ast::Ident::new("view")),
                    columns: vec![],
                });

                *factor = TableFactor::Derived {
                    lateral: false,
                    subquery: Box::new(view_query),
                    alias: Some(derived_alias),
                };
            }

            TableFactor::Derived { subquery, .. } => {
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

            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                Box::pin(expand_views_in_table_with_joins(
                    store,
                    txn,
                    db_id,
                    search_path,
                    table_with_joins.as_mut(),
                    visible_ctes,
                    stack,
                    stack_set,
                ))
                .await?;
            }

            _ => {}
        }

        Ok(())
    })
}

fn expand_views_in_expr<'a>(
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
                        ast::FunctionArg::Named { arg, .. } => {
                            if let ast::FunctionArgExpr::Expr(e) = arg {
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

fn expr_requires_view_expansion(expr: &Expr) -> bool {
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
                        ast::FunctionArg::Named { arg, .. } => {
                            if let ast::FunctionArgExpr::Expr(e) = arg {
                                stack.push(e);
                            }
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
