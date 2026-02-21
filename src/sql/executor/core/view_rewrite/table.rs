//! View-name resolution and table-factor expansion.
//!
//! Contains helpers for normalizing SQL object names, resolving view
//! definitions from the catalog, parsing view SQL text back into an AST
//! `Query`, and rewriting `TableFactor::Table` nodes that reference views
//! into `TableFactor::Derived` subqueries.

use crate::sql::names;
use crate::storage::TikvStore;
use crate::types::ViewDef;
use anyhow::{anyhow, Result};
use sqlparser::ast::{self, Query, TableFactor, TableWithJoins};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

use super::expand_views_in_query_mut;
use super::expr::expand_views_in_expr;
use super::MAX_VIEW_EXPANSION_DEPTH;

pub(crate) fn normalize_object_name(name: &ast::ObjectName) -> String {
    name.0
        .iter()
        .map(names::normalize_ident)
        .collect::<Vec<_>>()
        .join(".")
}

pub(crate) async fn resolve_view(
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

pub(crate) fn parse_view_query(view_full_name: &str, sql: &str) -> Result<Query> {
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

pub(crate) fn expand_views_in_table_factor<'a>(
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

pub(crate) fn expand_views_in_table_with_joins<'a>(
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
                ast::JoinOperator::Inner(ast::JoinConstraint::On(e))
                | ast::JoinOperator::LeftOuter(ast::JoinConstraint::On(e))
                | ast::JoinOperator::RightOuter(ast::JoinConstraint::On(e))
                | ast::JoinOperator::FullOuter(ast::JoinConstraint::On(e)) => {
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
