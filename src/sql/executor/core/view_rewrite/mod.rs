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

mod expr;
mod query;
mod table;

// Re-export the public API surface.
// Re-export the public API surface (some used only in tests or sibling modules).
#[allow(unused_imports)]
pub(crate) use expr::{expand_views_in_expr, expr_requires_view_expansion};
#[allow(unused_imports)]
pub(crate) use query::expand_views_in_select;
#[allow(unused_imports)]
pub(crate) use table::{
    expand_views_in_table_factor, expand_views_in_table_with_joins, normalize_object_name,
    parse_view_query, resolve_view,
};

use crate::sql::names;
use crate::storage::TikvStore;
use anyhow::Result;
use sqlparser::ast::{Query, SetExpr, Values};
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
