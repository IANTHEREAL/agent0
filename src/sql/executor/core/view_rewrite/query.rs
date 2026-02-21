//! SELECT-level and table-with-joins view expansion.
//!
//! Handles walking `Select` nodes (projection, FROM, WHERE, GROUP BY, HAVING)
//! and their constituent `TableWithJoins` items, recursively expanding any
//! view references found in table factors or subquery expressions.

use crate::storage::TikvStore;
use anyhow::Result;
use sqlparser::ast::{self, Select, SelectItem};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

use super::expr::expand_views_in_expr;
use super::table::expand_views_in_table_with_joins;

pub(crate) fn expand_views_in_select<'a>(
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
