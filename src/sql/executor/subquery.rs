//! Subquery resolution for the SQL executor

use super::super::names::{self, normalize_ident};
use super::super::value_coercion::value_to_sql_expr;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Ident, Query, SelectItem, SetExpr, Value as SqlValue, With,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tikv_client::Transaction;

pub(crate) fn expr_contains_subquery(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }
        match e {
            Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
                found = true;
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        }
    });
    found
}

/// Check if a query contains bare (unqualified) identifier references matching
/// any column in the outer schema.  This is a conservative over-detection: it may
/// flag uncorrelated subqueries as correlated (performance, not correctness).
fn query_has_bare_outer_reference(query: &Query, outer_schema: &TableSchema) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::{Visit, Visitor};

    struct BareRefVisitor<'a> {
        outer_schema: &'a TableSchema,
        found: bool,
        query_depth: usize,
    }

    impl<'a> Visitor for BareRefVisitor<'a> {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.query_depth += 1;
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.query_depth -= 1;
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            if self.found {
                return ControlFlow::Break(());
            }
            // Only check at the query's immediate level (depth 1 because the
            // visit enters the Query node first).
            if self.query_depth != 1 {
                return ControlFlow::Continue(());
            }
            if let Expr::Identifier(ident) = expr {
                let name = normalize_ident(ident);
                if self
                    .outer_schema
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&name))
                {
                    self.found = true;
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        }
    }

    let mut visitor = BareRefVisitor {
        outer_schema,
        found: false,
        query_depth: 0,
    };
    let _ = query.visit(&mut visitor);
    visitor.found
}

/// Extract table names (not aliases) from a query's top-level FROM clause
/// for schema lookup.
fn collect_from_table_names(query: &Query) -> Vec<(Option<String>, String)> {
    let select = match &*query.body {
        SetExpr::Select(s) => s,
        _ => return Vec::new(),
    };

    fn collect_factor(
        factor: &sqlparser::ast::TableFactor,
        out: &mut Vec<(Option<String>, String)>,
    ) {
        match factor {
            sqlparser::ast::TableFactor::Table { name, .. } => {
                let parts: Vec<String> = name.0.iter().map(normalize_ident).collect();
                match parts.as_slice() {
                    [single] => out.push((None, single.clone())),
                    [schema, obj] => out.push((Some(schema.clone()), obj.clone())),
                    _ => {} // skip unsupported multi-part names
                }
            }
            sqlparser::ast::TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                collect_factor(&table_with_joins.relation, out);
                for join in &table_with_joins.joins {
                    collect_factor(&join.relation, out);
                }
            }
            // Derived tables, functions, UNNEST, etc. — we cannot resolve their
            // column sets without executing them; skip.
            _ => {}
        }
    }

    let mut names = Vec::new();
    for twj in &select.from {
        collect_factor(&twj.relation, &mut names);
        for join in &twj.joins {
            collect_factor(&join.relation, &mut names);
        }
    }
    names
}

/// Return true if the FROM clause contains any entry whose columns cannot be
/// determined by schema lookup alone (derived tables, table functions, etc.).
fn has_unresolvable_from(from: &[sqlparser::ast::TableWithJoins]) -> bool {
    fn is_plain_table(factor: &sqlparser::ast::TableFactor) -> bool {
        match factor {
            sqlparser::ast::TableFactor::Table { args: None, .. } => true,
            sqlparser::ast::TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                is_plain_table(&table_with_joins.relation)
                    && table_with_joins
                        .joins
                        .iter()
                        .all(|j| is_plain_table(&j.relation))
            }
            _ => false,
        }
    }

    for twj in from {
        if !is_plain_table(&twj.relation) {
            return true;
        }
        for join in &twj.joins {
            if !is_plain_table(&join.relation) {
                return true;
            }
        }
    }
    false
}

/// Check whether a `TableFactor` exposes a name (alias or bare table name)
/// that matches `outer_alias`.  Shared by both detection and substitution.
fn table_factor_shadows_alias(factor: &sqlparser::ast::TableFactor, outer_alias: &str) -> bool {
    fn exposed_name_for_object(name: &sqlparser::ast::ObjectName) -> Option<String> {
        name.0.last().map(normalize_ident)
    }

    match factor {
        sqlparser::ast::TableFactor::Table { name, alias, .. } => {
            // INTENTIONAL: sqlparser guarantees non-empty ObjectName from parsed SQL
            let exposed = alias
                .as_ref()
                .map(|a| normalize_ident(&a.name))
                .or_else(|| exposed_name_for_object(name))
                .unwrap_or_default();
            exposed.eq_ignore_ascii_case(outer_alias)
        }
        sqlparser::ast::TableFactor::Derived { alias, .. } => alias
            .as_ref()
            .map(|a| normalize_ident(&a.name))
            .is_some_and(|a| a.eq_ignore_ascii_case(outer_alias)),
        sqlparser::ast::TableFactor::Function { name, alias, .. } => {
            // INTENTIONAL: sqlparser guarantees non-empty ObjectName from parsed SQL
            let exposed = alias
                .as_ref()
                .map(|a| normalize_ident(&a.name))
                .or_else(|| exposed_name_for_object(name))
                .unwrap_or_default();
            exposed.eq_ignore_ascii_case(outer_alias)
        }
        sqlparser::ast::TableFactor::UNNEST { alias, .. } => alias
            .as_ref()
            .map(|a| normalize_ident(&a.name))
            .is_some_and(|a| a.eq_ignore_ascii_case(outer_alias)),
        sqlparser::ast::TableFactor::TableFunction { alias, .. } => alias
            .as_ref()
            .map(|a| normalize_ident(&a.name))
            .is_some_and(|a| a.eq_ignore_ascii_case(outer_alias)),
        sqlparser::ast::TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } => {
            if let Some(a) = alias.as_ref() {
                normalize_ident(&a.name).eq_ignore_ascii_case(outer_alias)
            } else {
                table_factor_shadows_alias(&table_with_joins.relation, outer_alias)
                    || table_with_joins
                        .joins
                        .iter()
                        .any(|j| table_factor_shadows_alias(&j.relation, outer_alias))
            }
        }
        sqlparser::ast::TableFactor::Pivot { alias, .. }
        | sqlparser::ast::TableFactor::Unpivot { alias, .. } => alias
            .as_ref()
            .map(|a| normalize_ident(&a.name))
            .is_some_and(|a| a.eq_ignore_ascii_case(outer_alias)),
    }
}

/// Check whether any table in a FROM clause shadows `outer_alias`.
fn from_clause_shadows_alias(from: &[sqlparser::ast::TableWithJoins], outer_alias: &str) -> bool {
    from.iter().any(|twj| {
        table_factor_shadows_alias(&twj.relation, outer_alias)
            || twj
                .joins
                .iter()
                .any(|j| table_factor_shadows_alias(&j.relation, outer_alias))
    })
}

/// Check whether a `Query`'s top-level SELECT FROM clause shadows `outer_alias`.
fn query_from_shadows_alias(query: &Query, outer_alias: &str) -> bool {
    if let SetExpr::Select(select) = &*query.body {
        from_clause_shadows_alias(&select.from, outer_alias)
    } else {
        false
    }
}

/// Resolve FROM-clause table names to their column name sets.
/// Returns `None` if any FROM entry cannot be resolved (derived tables, views,
/// unrecognised names), signalling that bare-identifier qualification should be
/// skipped for this subquery (conservative fallback).
pub(crate) async fn collect_inner_column_names(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
    ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
) -> Result<Option<HashSet<String>>> {
    let select = match &*query.body {
        SetExpr::Select(s) => s,
        _ => return Ok(None),
    };

    if has_unresolvable_from(&select.from) {
        return Ok(None);
    }

    let table_names = collect_from_table_names(query);
    let mut columns = HashSet::new();

    for (schema_opt, table_name) in &table_names {
        // Check CTEs first.
        let cte_key = table_name.to_lowercase();
        if let Some((cte_schema, _)) = ctes.get(&cte_key) {
            for col in &cte_schema.columns {
                columns.insert(col.name.to_lowercase());
            }
            continue;
        }

        // Resolve via store with search_path.
        let obj_name = match schema_opt {
            Some(schema) => {
                sqlparser::ast::ObjectName(vec![Ident::new(schema), Ident::new(table_name)])
            }
            None => sqlparser::ast::ObjectName(vec![Ident::new(table_name)]),
        };

        if let Some(resolved) =
            names::resolve_existing_table_name(store.as_ref(), txn, db_id, &obj_name, search_path)
                .await?
        {
            if let Some(table_schema) = store.get_schema(txn, db_id, &resolved.full).await? {
                for col in &table_schema.columns {
                    columns.insert(col.name.to_lowercase());
                }
                continue;
            }
        }

        // Unresolvable (could be a view, or doesn't exist).
        return Ok(None);
    }

    Ok(Some(columns))
}

impl Executor {
    pub(crate) fn resolve_subqueries<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        expr: &'a Expr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        from_aliases: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Expr>> + Send + 'a>> {
        Box::pin(async move {
            if !expr_contains_subquery(expr) {
                return Ok(expr.clone());
            }
            match expr {
                Expr::InSubquery {
                    expr: inner_expr,
                    subquery,
                    negated,
                } => {
                    // If the subquery references any FROM-clause alias, it's correlated.
                    // Leave it unresolved for per-row evaluation.
                    if from_aliases
                        .iter()
                        .any(|alias| query_has_outer_reference(subquery, alias))
                    {
                        return Ok(expr.clone());
                    }
                    let result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            ctes,
                        )
                        .await?;
                    let values = match result {
                        ExecuteResult::Select { rows, .. } => rows
                            .iter()
                            .filter_map(|row| row.values.first().cloned())
                            .map(|v| value_to_sql_expr(&v))
                            .collect::<Vec<_>>(),
                        _ => return Err(anyhow!("Subquery must return a SELECT result")),
                    };
                    let resolved_inner = Box::new(
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner_expr,
                            ctes,
                            from_aliases,
                        )
                        .await?,
                    );
                    Ok(Expr::InList {
                        expr: resolved_inner,
                        list: values,
                        negated: *negated,
                    })
                }
                Expr::BinaryOp { left, op, right } => {
                    let resolved_left = Box::new(
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            left,
                            ctes,
                            from_aliases,
                        )
                        .await?,
                    );
                    let resolved_right = Box::new(
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            right,
                            ctes,
                            from_aliases,
                        )
                        .await?,
                    );
                    Ok(Expr::BinaryOp {
                        left: resolved_left,
                        op: op.clone(),
                        right: resolved_right,
                    })
                }
                Expr::UnaryOp { op, expr: inner } => {
                    let resolved = Box::new(
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            from_aliases,
                        )
                        .await?,
                    );
                    Ok(Expr::UnaryOp {
                        op: op.clone(),
                        expr: resolved,
                    })
                }
                Expr::Nested(inner) => {
                    let resolved = Box::new(
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            from_aliases,
                        )
                        .await?,
                    );
                    Ok(Expr::Nested(resolved))
                }
                Expr::Cast {
                    expr: inner,
                    data_type,
                    format,
                } => {
                    let resolved = Box::new(
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            from_aliases,
                        )
                        .await?,
                    );
                    Ok(Expr::Cast {
                        expr: resolved,
                        data_type: data_type.clone(),
                        format: format.clone(),
                    })
                }
                Expr::Subquery(subquery) => {
                    if from_aliases
                        .iter()
                        .any(|alias| query_has_outer_reference(subquery, alias))
                    {
                        return Ok(expr.clone());
                    }
                    let result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            ctes,
                        )
                        .await?;
                    match result {
                        ExecuteResult::Select { rows, .. } => {
                            if rows.is_empty() {
                                Ok(Expr::Value(SqlValue::Null))
                            } else if rows.len() == 1 {
                                let value = rows[0].values.first().cloned().unwrap_or(Value::Null);
                                Ok(value_to_sql_expr(&value))
                            } else {
                                Err(anyhow!("Scalar subquery returned more than one row"))
                            }
                        }
                        _ => Err(anyhow!("Subquery must return a SELECT result")),
                    }
                }
                Expr::Exists { subquery, negated } => {
                    if from_aliases
                        .iter()
                        .any(|alias| query_has_outer_reference(subquery, alias))
                    {
                        return Ok(expr.clone());
                    }
                    let result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            ctes,
                        )
                        .await?;
                    let exists = match result {
                        ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                        _ => false,
                    };
                    let result_bool = if *negated { !exists } else { exists };
                    Ok(Expr::Value(SqlValue::Boolean(result_bool)))
                }
                Expr::Case {
                    operand,
                    conditions,
                    results,
                    else_result,
                } => {
                    let resolved_operand = if let Some(op) = operand {
                        Some(Box::new(
                            self.resolve_subqueries(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                op,
                                ctes,
                                from_aliases,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    let mut resolved_conditions = Vec::with_capacity(conditions.len());
                    for cond in conditions {
                        resolved_conditions.push(
                            self.resolve_subqueries(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                cond,
                                ctes,
                                from_aliases,
                            )
                            .await?,
                        );
                    }
                    let mut resolved_results = Vec::with_capacity(results.len());
                    for res in results {
                        resolved_results.push(
                            self.resolve_subqueries(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                res,
                                ctes,
                                from_aliases,
                            )
                            .await?,
                        );
                    }
                    let resolved_else = if let Some(else_expr) = else_result {
                        Some(Box::new(
                            self.resolve_subqueries(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                else_expr,
                                ctes,
                                from_aliases,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    Ok(Expr::Case {
                        operand: resolved_operand,
                        conditions: resolved_conditions,
                        results: resolved_results,
                        else_result: resolved_else,
                    })
                }
                Expr::Function(func) => {
                    let mut resolved_args = Vec::new();
                    for arg in &func.args {
                        let resolved_arg = match arg {
                            sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(e),
                            ) => sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(
                                    self.resolve_subqueries(
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        e,
                                        ctes,
                                        from_aliases,
                                    )
                                    .await?,
                                ),
                            ),
                            other => other.clone(),
                        };
                        resolved_args.push(resolved_arg);
                    }
                    Ok(Expr::Function(sqlparser::ast::Function {
                        name: func.name.clone(),
                        args: resolved_args,
                        filter: func.filter.clone(),
                        null_treatment: func.null_treatment.clone(),
                        over: func.over.clone(),
                        distinct: func.distinct,
                        special: func.special,
                        order_by: func.order_by.clone(),
                    }))
                }
                _ => Ok(expr.clone()),
            }
        })
    }

    pub(crate) async fn resolve_projection_subqueries(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        projection: &[SelectItem],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => SelectItem::UnnamedExpr(
                    self.resolve_subqueries(txn, db_id, sequence_values, search_path, e, ctes, &[])
                        .await?,
                ),
                SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias {
                    expr: self
                        .resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            ctes,
                            &[],
                        )
                        .await?,
                    alias: alias.clone(),
                },
                other => other.clone(),
            };
            resolved.push(resolved_item);
        }
        Ok(resolved)
    }

    pub(crate) async fn resolve_projection_subqueries_with_outer_context(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        projection: &[SelectItem],
        outer_alias: &str,
        outer_schema: &TableSchema,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => {
                    if self.expr_is_correlated_subquery(e, outer_alias, outer_schema) {
                        SelectItem::UnnamedExpr(e.clone())
                    } else {
                        SelectItem::UnnamedExpr(
                            self.resolve_subqueries(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                e,
                                ctes,
                                &[],
                            )
                            .await?,
                        )
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    if self.expr_is_correlated_subquery(expr, outer_alias, outer_schema) {
                        SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: alias.clone(),
                        }
                    } else {
                        SelectItem::ExprWithAlias {
                            expr: self
                                .resolve_subqueries(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    ctes,
                                    &[],
                                )
                                .await?,
                            alias: alias.clone(),
                        }
                    }
                }
                other => other.clone(),
            };
            resolved.push(resolved_item);
        }
        Ok(resolved)
    }

    pub(crate) fn expr_is_correlated_subquery(
        &self,
        expr: &Expr,
        outer_alias: &str,
        outer_schema: &TableSchema,
    ) -> bool {
        match expr {
            Expr::Subquery(q) => {
                query_has_outer_reference(q, outer_alias)
                    || query_has_bare_outer_reference(q, outer_schema)
            }
            _ => false,
        }
    }

    pub(crate) fn eval_correlated_subquery<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        subquery: &'a Query,
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let inner_columns =
                collect_inner_column_names(&self.store(), txn, db_id, search_path, subquery, ctes)
                    .await?;

            let substituted_query = substitute_outer_values_in_query(
                subquery,
                outer_alias,
                outer_schema,
                outer_row,
                inner_columns.as_ref(),
            );
            let result = self
                .execute_query(txn, db_id, sequence_values, search_path, &substituted_query)
                .await?;
            match result {
                ExecuteResult::Select { rows, .. } => {
                    if rows.is_empty() {
                        Ok(Value::Null)
                    } else if rows.len() == 1 {
                        Ok(rows[0].values.first().cloned().unwrap_or(Value::Null))
                    } else {
                        Err(anyhow!("Scalar subquery returned more than one row"))
                    }
                }
                _ => Err(anyhow!("Subquery must return a SELECT result")),
            }
        })
    }
}

/// Check if an expression contains references to an outer table alias
/// Used to detect correlated subqueries
pub fn expr_has_outer_reference(expr: &Expr, outer_alias: &str) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::{Visit, Visitor};

    struct OuterRefVisitor<'a> {
        outer_alias: &'a str,
        found: bool,
        query_depth: usize,
    }

    impl<'a> Visitor for OuterRefVisitor<'a> {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.query_depth = self.query_depth.saturating_add(1);
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.query_depth = self.query_depth.saturating_sub(1);
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            if self.found {
                return ControlFlow::Break(());
            }

            // Handle subqueries explicitly to preserve FROM-scope alias shadowing behavior.
            match expr {
                Expr::Subquery(q)
                | Expr::InSubquery { subquery: q, .. }
                | Expr::Exists { subquery: q, .. } => {
                    if query_has_outer_reference(q, self.outer_alias) {
                        self.found = true;
                        return ControlFlow::Break(());
                    }
                }
                _ => {}
            }

            // Only substitute/detect *qualified* outer references (e.g. `outer_alias.col`).
            // Bare identifiers are not scope-aware and can incorrectly bind to inner columns.
            if self.query_depth == 0 {
                if let Expr::CompoundIdentifier(parts) = expr {
                    if parts.len() >= 2 {
                        // Support schema-qualified (schema.table.col) and even db.schema.table.col
                        // by treating the second-to-last identifier as the table/alias.
                        let table_part = normalize_ident(&parts[parts.len() - 2]);
                        if table_part.eq_ignore_ascii_case(self.outer_alias) {
                            self.found = true;
                            return ControlFlow::Break(());
                        }
                    }
                }
            }

            ControlFlow::Continue(())
        }
    }

    let mut visitor = OuterRefVisitor {
        outer_alias,
        found: false,
        query_depth: 0,
    };
    let _ = expr.visit(&mut visitor);
    visitor.found
}

/// Check if a query contains references to an outer table alias
pub fn query_has_outer_reference(query: &Query, outer_alias: &str) -> bool {
    fn window_frame_bound_has_outer_reference(
        bound: &sqlparser::ast::WindowFrameBound,
        outer_alias: &str,
    ) -> bool {
        match bound {
            sqlparser::ast::WindowFrameBound::CurrentRow => false,
            sqlparser::ast::WindowFrameBound::Preceding(Some(expr))
            | sqlparser::ast::WindowFrameBound::Following(Some(expr)) => {
                expr_has_outer_reference(expr, outer_alias)
            }
            sqlparser::ast::WindowFrameBound::Preceding(None)
            | sqlparser::ast::WindowFrameBound::Following(None) => false,
        }
    }

    fn window_spec_has_outer_reference(
        spec: &sqlparser::ast::WindowSpec,
        outer_alias: &str,
    ) -> bool {
        for e in &spec.partition_by {
            if expr_has_outer_reference(e, outer_alias) {
                return true;
            }
        }
        for o in &spec.order_by {
            if expr_has_outer_reference(&o.expr, outer_alias) {
                return true;
            }
        }
        if let Some(frame) = &spec.window_frame {
            if window_frame_bound_has_outer_reference(&frame.start_bound, outer_alias) {
                return true;
            }
            if let Some(end) = &frame.end_bound {
                if window_frame_bound_has_outer_reference(end, outer_alias) {
                    return true;
                }
            }
        }
        false
    }

    fn join_operator_has_outer_reference(
        op: &sqlparser::ast::JoinOperator,
        outer_alias: &str,
    ) -> bool {
        use sqlparser::ast::JoinConstraint;
        match op {
            sqlparser::ast::JoinOperator::Inner(c)
            | sqlparser::ast::JoinOperator::LeftOuter(c)
            | sqlparser::ast::JoinOperator::RightOuter(c)
            | sqlparser::ast::JoinOperator::FullOuter(c)
            | sqlparser::ast::JoinOperator::LeftSemi(c)
            | sqlparser::ast::JoinOperator::RightSemi(c)
            | sqlparser::ast::JoinOperator::LeftAnti(c)
            | sqlparser::ast::JoinOperator::RightAnti(c) => match c {
                JoinConstraint::On(e) => expr_has_outer_reference(e, outer_alias),
                _ => false,
            },
            sqlparser::ast::JoinOperator::CrossJoin
            | sqlparser::ast::JoinOperator::CrossApply
            | sqlparser::ast::JoinOperator::OuterApply => false,
        }
    }

    fn table_factor_has_outer_reference(
        factor: &sqlparser::ast::TableFactor,
        outer_alias: &str,
    ) -> bool {
        match factor {
            sqlparser::ast::TableFactor::Table {
                args,
                with_hints,
                version,
                ..
            } => {
                if let Some(args) = args {
                    for arg in args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            if expr_has_outer_reference(e, outer_alias) {
                                return true;
                            }
                        }
                    }
                }
                for hint in with_hints {
                    if expr_has_outer_reference(hint, outer_alias) {
                        return true;
                    }
                }
                if let Some(v) = version {
                    match v {
                        sqlparser::ast::TableVersion::ForSystemTimeAsOf(e) => {
                            if expr_has_outer_reference(e, outer_alias) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
            sqlparser::ast::TableFactor::Derived { subquery, .. } => {
                query_has_outer_reference(subquery, outer_alias)
            }
            sqlparser::ast::TableFactor::TableFunction { expr, .. } => {
                expr_has_outer_reference(expr, outer_alias)
            }
            sqlparser::ast::TableFactor::Function { args, .. } => args.iter().any(|arg| {
                matches!(arg, FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) if expr_has_outer_reference(e, outer_alias))
            }),
            sqlparser::ast::TableFactor::UNNEST { array_exprs, .. } => array_exprs
                .iter()
                .any(|e| expr_has_outer_reference(e, outer_alias)),
            sqlparser::ast::TableFactor::NestedJoin {
                table_with_joins,
                ..
            } => table_with_joins_has_outer_reference(table_with_joins, outer_alias),
            sqlparser::ast::TableFactor::Pivot {
                table,
                aggregate_function,
                ..
            } => {
                table_factor_has_outer_reference(table, outer_alias)
                    || expr_has_outer_reference(aggregate_function, outer_alias)
            }
            sqlparser::ast::TableFactor::Unpivot { table, .. } => {
                table_factor_has_outer_reference(table, outer_alias)
            }
        }
    }

    fn table_with_joins_has_outer_reference(
        table_with_joins: &sqlparser::ast::TableWithJoins,
        outer_alias: &str,
    ) -> bool {
        if table_factor_has_outer_reference(&table_with_joins.relation, outer_alias) {
            return true;
        }
        for join in &table_with_joins.joins {
            if table_factor_has_outer_reference(&join.relation, outer_alias) {
                return true;
            }
            if join_operator_has_outer_reference(&join.join_operator, outer_alias) {
                return true;
            }
        }
        false
    }

    fn select_has_outer_reference(select: &sqlparser::ast::Select, outer_alias: &str) -> bool {
        if from_clause_shadows_alias(&select.from, outer_alias) {
            return false;
        }

        if let Some(distinct) = &select.distinct {
            if let sqlparser::ast::Distinct::On(exprs) = distinct {
                for e in exprs {
                    if expr_has_outer_reference(e, outer_alias) {
                        return true;
                    }
                }
            }
        }

        if let Some(top) = &select.top {
            if let Some(qty) = &top.quantity {
                if expr_has_outer_reference(qty, outer_alias) {
                    return true;
                }
            }
        }

        for item in &select.projection {
            match item {
                sqlparser::ast::SelectItem::UnnamedExpr(e)
                | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } => {
                    if expr_has_outer_reference(e, outer_alias) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        for twj in &select.from {
            if table_with_joins_has_outer_reference(twj, outer_alias) {
                return true;
            }
        }

        for lv in &select.lateral_views {
            if expr_has_outer_reference(&lv.lateral_view, outer_alias) {
                return true;
            }
        }

        if let Some(selection) = &select.selection {
            if expr_has_outer_reference(selection, outer_alias) {
                return true;
            }
        }

        match &select.group_by {
            sqlparser::ast::GroupByExpr::All => {}
            sqlparser::ast::GroupByExpr::Expressions(exprs) => {
                for e in exprs {
                    if expr_has_outer_reference(e, outer_alias) {
                        return true;
                    }
                }
            }
        }

        for e in &select.cluster_by {
            if expr_has_outer_reference(e, outer_alias) {
                return true;
            }
        }
        for e in &select.distribute_by {
            if expr_has_outer_reference(e, outer_alias) {
                return true;
            }
        }
        for e in &select.sort_by {
            if expr_has_outer_reference(e, outer_alias) {
                return true;
            }
        }

        if let Some(having) = &select.having {
            if expr_has_outer_reference(having, outer_alias) {
                return true;
            }
        }

        for def in &select.named_window {
            if window_spec_has_outer_reference(&def.1, outer_alias) {
                return true;
            }
        }

        if let Some(qualify) = &select.qualify {
            if expr_has_outer_reference(qualify, outer_alias) {
                return true;
            }
        }

        false
    }

    fn set_expr_has_outer_reference(body: &SetExpr, outer_alias: &str) -> bool {
        match body {
            SetExpr::Select(select) => select_has_outer_reference(select, outer_alias),
            SetExpr::Query(q) => query_has_outer_reference(q, outer_alias),
            SetExpr::SetOperation { left, right, .. } => {
                set_expr_has_outer_reference(left, outer_alias)
                    || set_expr_has_outer_reference(right, outer_alias)
            }
            SetExpr::Values(values) => values
                .rows
                .iter()
                .flatten()
                .any(|e| expr_has_outer_reference(e, outer_alias)),
            _ => false,
        }
    }

    // CTEs can contain correlated references.
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            if query_has_outer_reference(&cte.query, outer_alias) {
                return true;
            }
        }
    }

    // Preserve existing FROM-scope alias shadowing behavior for SELECT query blocks.
    if let SetExpr::Select(select) = &*query.body {
        if from_clause_shadows_alias(&select.from, outer_alias) {
            return false;
        }
    }

    if set_expr_has_outer_reference(&query.body, outer_alias) {
        return true;
    }

    for o in &query.order_by {
        if expr_has_outer_reference(&o.expr, outer_alias) {
            return true;
        }
    }
    if let Some(limit) = &query.limit {
        if expr_has_outer_reference(limit, outer_alias) {
            return true;
        }
    }
    for e in &query.limit_by {
        if expr_has_outer_reference(e, outer_alias) {
            return true;
        }
    }
    if let Some(offset) = &query.offset {
        if expr_has_outer_reference(&offset.value, outer_alias) {
            return true;
        }
    }
    if let Some(fetch) = &query.fetch {
        if let Some(qty) = &fetch.quantity {
            if expr_has_outer_reference(qty, outer_alias) {
                return true;
            }
        }
    }

    false
}

/// VisitorMut that substitutes outer-scope column references with literal values.
///
/// Handles both qualified references (`outer_alias.col` → literal) and, when
/// `inner_columns` is `Some`, bare references (`col` → literal) at the
/// immediate query level (depth 1) for columns not shadowed by inner scope.
///
/// Tracks alias shadowing: if a query's FROM clause defines the same name as
/// `outer_alias`, all substitution within that query scope is skipped.
///
/// Per-query CTE scoping: a query's WITH clause is logically outside that
/// query block's FROM scope, so CTEs are visited *before* FROM-shadow tracking
/// is applied.  This ensures consistent behavior regardless of entry point
/// (`substitute_outer_values` vs `substitute_outer_values_in_query`).
struct SubstituteVisitor<'a> {
    outer_alias: &'a str,
    outer_schema: &'a TableSchema,
    outer_row: &'a Row,
    inner_columns: Option<&'a HashSet<String>>,
    /// Number of nested query scopes whose FROM shadows `outer_alias`.
    shadow_count: usize,
    /// Current query nesting depth (incremented on entering any Query node).
    query_depth: usize,
    /// Stack of detached WITH clauses, processed before FROM-shadow tracking.
    stashed_withs: Vec<Option<With>>,
    /// Stack of detached SetOperation bodies, visited with per-arm shadow tracking.
    stashed_bodies: Vec<Option<Box<SetExpr>>>,
}

impl<'a> SubstituteVisitor<'a> {
    /// Visit a SetExpr tree with per-arm FROM-shadow tracking.
    ///
    /// For SetOperation (UNION/INTERSECT/EXCEPT), each arm may independently
    /// shadow the outer alias in its FROM clause. We visit each arm separately
    /// with its own shadow_count adjustment.
    fn visit_set_expr_with_shadows(&mut self, set_expr: &mut SetExpr) {
        use sqlparser::ast::VisitMut;

        match set_expr {
            SetExpr::Select(select) => {
                let shadows = from_clause_shadows_alias(&select.from, self.outer_alias);
                if shadows {
                    self.shadow_count += 1;
                }
                let _ = set_expr.visit(self);
                if shadows {
                    self.shadow_count -= 1;
                }
            }
            SetExpr::SetOperation { left, right, .. } => {
                self.visit_set_expr_with_shadows(left);
                self.visit_set_expr_with_shadows(right);
            }
            SetExpr::Query(q) => {
                // Nested Query — the pre/post_visit_query hooks handle it.
                let _ = q.visit(self);
            }
            other => {
                let _ = other.visit(self);
            }
        }
    }
}

impl<'a> sqlparser::ast::VisitorMut for SubstituteVisitor<'a> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> core::ops::ControlFlow<Self::Break> {
        use sqlparser::ast::VisitMut;

        self.query_depth += 1;

        // Detach CTEs so the default traversal skips them.  We visit them now,
        // *before* applying this query block's FROM-shadow, because WITH is
        // logically in the outer scope.
        let mut detached_with = query.with.take();
        if let Some(ref mut w) = detached_with {
            for cte in &mut w.cte_tables {
                let _ = cte.query.visit(self);
            }
        }
        self.stashed_withs.push(detached_with);

        // For SetOperation bodies (UNION/INTERSECT/EXCEPT), each arm may
        // independently shadow the outer alias, so we visit the body manually
        // with per-arm shadow tracking.  Detach the body so the default
        // traversal skips it; reattach in post_visit_query.
        let is_set_op = matches!(&*query.body, SetExpr::SetOperation { .. });
        if is_set_op {
            let mut detached_body = std::mem::replace(
                &mut query.body,
                Box::new(SetExpr::Values(sqlparser::ast::Values {
                    explicit_row: false,
                    rows: vec![],
                })),
            );
            self.visit_set_expr_with_shadows(&mut detached_body);
            self.stashed_bodies.push(Some(detached_body));
        } else {
            self.stashed_bodies.push(None);
            // Apply FROM-shadow tracking for simple SELECT bodies.
            if query_from_shadows_alias(query, self.outer_alias) {
                self.shadow_count += 1;
            }
        }
        core::ops::ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &mut Query) -> core::ops::ControlFlow<Self::Break> {
        // Reattach SetOperation body if it was detached, otherwise undo shadow.
        if let Some(detached_body) = self.stashed_bodies.pop().flatten() {
            query.body = detached_body;
        } else if query_from_shadows_alias(query, self.outer_alias) {
            self.shadow_count -= 1;
        }
        self.query_depth -= 1;

        // Reattach the processed CTEs.
        query.with = self.stashed_withs.pop().flatten();
        core::ops::ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> core::ops::ControlFlow<Self::Break> {
        if self.shadow_count > 0 {
            return core::ops::ControlFlow::Continue(());
        }

        match expr {
            // Qualified outer reference: `outer_alias.col` or `schema.outer_alias.col`
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
                let table_part = normalize_ident(&parts[parts.len() - 2]);
                if table_part.eq_ignore_ascii_case(self.outer_alias) {
                    let col_name = parts.last().map(normalize_ident).unwrap_or_default();
                    let qualified = format!("{}.{}", table_part, col_name);
                    let col_idx = self
                        .outer_schema
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(&qualified))
                        .or_else(|| {
                            self.outer_schema
                                .columns
                                .iter()
                                .position(|c| c.name.eq_ignore_ascii_case(&col_name))
                        });
                    if let Some(col_idx) = col_idx {
                        if let Some(value) = self.outer_row.values.get(col_idx) {
                            *expr = value_to_sql_expr(value);
                        }
                    }
                }
            }
            // Bare outer reference: unqualified `col` at the immediate query level
            Expr::Identifier(ref ident) if self.query_depth == 1 => {
                if let Some(inner_columns) = self.inner_columns {
                    let name = normalize_ident(ident);
                    if !inner_columns.contains(&name.to_lowercase()) {
                        if let Some(col_idx) = self
                            .outer_schema
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(&name))
                        {
                            if let Some(value) = self.outer_row.values.get(col_idx) {
                                *expr = value_to_sql_expr(value);
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        core::ops::ControlFlow::Continue(())
    }
}

/// Substitute outer table column references with literal values from the current row.
///
/// When `inner_columns` is `Some`, bare `Identifier` references at the immediate
/// query level are also substituted if they match an outer-schema column and are
/// not present in the inner-column set.
pub fn substitute_outer_values(
    expr: &Expr,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
    inner_columns: Option<&HashSet<String>>,
) -> Expr {
    use sqlparser::ast::VisitMut;

    let mut out = expr.clone();
    let mut visitor = SubstituteVisitor {
        outer_alias,
        outer_schema,
        outer_row,
        inner_columns,
        shadow_count: 0,
        query_depth: 0,
        stashed_withs: Vec::new(),
        stashed_bodies: Vec::new(),
    };
    let _ = out.visit(&mut visitor);
    out
}

/// Substitute outer values in a query.
///
/// When `inner_columns` is `Some`, bare `Identifier` references at the
/// immediate query level (depth 1) are also substituted.
///
/// CTE scoping is handled by the visitor itself: each query's WITH clause
/// is visited before FROM-shadow tracking is applied for that query block.
pub fn substitute_outer_values_in_query(
    query: &Query,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
    inner_columns: Option<&HashSet<String>>,
) -> Query {
    use sqlparser::ast::VisitMut;

    let mut out = query.clone();
    let mut visitor = SubstituteVisitor {
        outer_alias,
        outer_schema,
        outer_row,
        inner_columns,
        shadow_count: 0,
        query_depth: 0,
        stashed_withs: Vec::new(),
        stashed_bodies: Vec::new(),
    };
    let _ = out.visit(&mut visitor);
    out
}

/// Substitute outer column references in a subquery using join context
/// This handles correlated subqueries in JOIN queries where multiple tables may be referenced
#[cfg(test)]
pub fn substitute_join_context_values(
    expr: &Expr,
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_row: &crate::types::Row,
) -> Expr {
    match expr {
        // Only substitute *qualified* outer references (e.g. `outer_alias.col`). Substituting bare
        // identifiers is not scope-aware and can incorrectly rewrite inner-scope columns that share
        // names with outer columns in correlated subqueries.
        Expr::Identifier(_) => expr.clone(),
        Expr::CompoundIdentifier(parts) => {
            let (table_part, col_name) = if parts.len() == 2 {
                (normalize_ident(&parts[0]), normalize_ident(&parts[1]))
            } else if parts.len() == 3 {
                (normalize_ident(&parts[1]), normalize_ident(&parts[2]))
            } else {
                return expr.clone();
            };

            let key = format!("{}.{}", table_part, col_name);
            if let Some(&offset) = column_offsets.get(&key) {
                if let Some(value) = combined_row.values.get(offset) {
                    return value_to_sql_expr(value);
                }
            }
            let key_lower = key.to_lowercase();
            for (k, &offset) in column_offsets {
                if k.to_lowercase() == key_lower {
                    if let Some(value) = combined_row.values.get(offset) {
                        return value_to_sql_expr(value);
                    }
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_join_context_values(
                left,
                column_offsets,
                combined_row,
            )),
            op: op.clone(),
            right: Box::new(substitute_join_context_values(
                right,
                column_offsets,
                combined_row,
            )),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(substitute_join_context_values(
            inner,
            column_offsets,
            combined_row,
        ))),
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Expr::Cast {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::TryCast {
            expr: inner,
            data_type,
            format,
        } => Expr::TryCast {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::SafeCast {
            expr: inner,
            data_type,
            format,
        } => Expr::SafeCast {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::Function(f) => {
            let mut new_args = Vec::new();
            for arg in &f.args {
                let new_arg =
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                substitute_join_context_values(e, column_offsets, combined_row),
                            ))
                        }
                        other => other.clone(),
                    };
                new_args.push(new_arg);
            }
            Expr::Function(sqlparser::ast::Function {
                name: f.name.clone(),
                args: new_args,
                filter: f.filter.clone(),
                null_treatment: f.null_treatment.clone(),
                over: f.over.clone(),
                distinct: f.distinct,
                special: f.special,
                order_by: f.order_by.clone(),
            })
        }
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_join_context_values(
            inner,
            column_offsets,
            combined_row,
        ))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(substitute_join_context_values(
            inner,
            column_offsets,
            combined_row,
        ))),
        Expr::Subquery(q) => Expr::Subquery(Box::new(substitute_join_context_values_in_query(
            q,
            column_offsets,
            combined_row,
        ))),
        Expr::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            subquery: Box::new(substitute_join_context_values_in_query(
                subquery,
                column_offsets,
                combined_row,
            )),
            negated: *negated,
        },
        Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(substitute_join_context_values_in_query(
                subquery,
                column_offsets,
                combined_row,
            )),
            negated: *negated,
        },
        _ => expr.clone(),
    }
}

/// Substitute outer values in a query using join context
#[cfg(test)]
pub fn substitute_join_context_values_in_query(
    query: &Query,
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_row: &crate::types::Row,
) -> Query {
    let new_body = match &*query.body {
        SetExpr::Select(select) => {
            let new_selection = select
                .selection
                .as_ref()
                .map(|sel| substitute_join_context_values(sel, column_offsets, combined_row));

            let new_projection: Vec<sqlparser::ast::SelectItem> = select
                .projection
                .iter()
                .map(|item| match item {
                    sqlparser::ast::SelectItem::UnnamedExpr(e) => {
                        sqlparser::ast::SelectItem::UnnamedExpr(substitute_join_context_values(
                            e,
                            column_offsets,
                            combined_row,
                        ))
                    }
                    sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                        sqlparser::ast::SelectItem::ExprWithAlias {
                            expr: substitute_join_context_values(
                                expr,
                                column_offsets,
                                combined_row,
                            ),
                            alias: alias.clone(),
                        }
                    }
                    other => other.clone(),
                })
                .collect();

            let new_having = select
                .having
                .as_ref()
                .map(|h| substitute_join_context_values(h, column_offsets, combined_row));

            Box::new(SetExpr::Select(Box::new(sqlparser::ast::Select {
                distinct: select.distinct.clone(),
                top: select.top.clone(),
                projection: new_projection,
                into: select.into.clone(),
                from: select.from.clone(),
                lateral_views: select.lateral_views.clone(),
                selection: new_selection,
                group_by: select.group_by.clone(),
                cluster_by: select.cluster_by.clone(),
                distribute_by: select.distribute_by.clone(),
                sort_by: select.sort_by.clone(),
                having: new_having,
                named_window: select.named_window.clone(),
                qualify: select.qualify.clone(),
            })))
        }
        _ => query.body.clone(),
    };

    Query {
        with: query.with.clone(),
        body: new_body,
        order_by: query.order_by.clone(),
        limit: query.limit.clone(),
        offset: query.offset.clone(),
        fetch: query.fetch.clone(),
        locks: query.locks.clone(),
        limit_by: query.limit_by.clone(),
        for_clause: query.for_clause.clone(),
    }
}

#[cfg(test)]
mod subquery_tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, Row, Value};
    use sqlparser::ast::{Expr, Ident, Value as SqlValue};
    use std::collections::HashMap;

    #[test]
    fn test_substitute_join_context_values_only_substitutes_qualified() {
        let mut column_offsets: HashMap<String, usize> = HashMap::new();
        column_offsets.insert("id".to_string(), 0);
        column_offsets.insert("o.id".to_string(), 0);

        let combined_row = Row {
            values: vec![Value::Int32(7)],
        };

        let expr = Expr::Identifier(Ident::new("id"));
        let out = substitute_join_context_values(&expr, &column_offsets, &combined_row);
        assert!(matches!(out, Expr::Identifier(_)));

        let expr = Expr::CompoundIdentifier(vec![Ident::new("o"), Ident::new("id")]);
        let out = substitute_join_context_values(&expr, &column_offsets, &combined_row);
        assert!(matches!(
            out,
            Expr::Value(SqlValue::Number(ref n, _)) if n == "7"
        ));
    }

    #[test]
    fn test_substitute_join_context_values_recurses_into_cast() {
        let mut column_offsets: HashMap<String, usize> = HashMap::new();
        column_offsets.insert("pg_attribute.attrelid".to_string(), 0);

        let combined_row = Row {
            values: vec![Value::Int32(42)],
        };

        let expr = Expr::Cast {
            expr: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("pg_catalog"),
                Ident::new("pg_attribute"),
                Ident::new("attrelid"),
            ])),
            data_type: sqlparser::ast::DataType::Regclass,
            format: None,
        };

        let out = substitute_join_context_values(&expr, &column_offsets, &combined_row);
        match out {
            Expr::Cast { expr: inner, .. } => assert!(matches!(
                *inner,
                Expr::Value(SqlValue::Number(ref n, _)) if n == "42"
            )),
            other => panic!("expected cast expression, got {other:?}"),
        }
    }

    #[test]
    fn test_expr_has_outer_reference_schema_qualified_compound_identifier() {
        let expr = Expr::CompoundIdentifier(vec![
            Ident::new("pg_catalog"),
            Ident::new("pg_attribute"),
            Ident::new("attrelid"),
        ]);
        assert!(expr_has_outer_reference(&expr, "pg_attribute"));
        assert!(!expr_has_outer_reference(&expr, "pg_attrdef"));
    }

    #[test]
    fn test_substitute_outer_values_schema_qualified_compound_identifier() {
        let outer_schema = TableSchema {
            name: "o".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let outer_row = Row::new(vec![Value::Int32(9)]);

        let expr = Expr::CompoundIdentifier(vec![
            Ident::new("pg_catalog"),
            Ident::new("o"),
            Ident::new("id"),
        ]);
        let out = substitute_outer_values(&expr, "o", &outer_schema, &outer_row, None);
        assert!(matches!(
            out,
            Expr::Value(SqlValue::Number(ref n, _)) if n == "9"
        ));
    }

    #[test]
    fn test_correlated_subquery_outer_ref_in_join_on_is_detected_and_substituted() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = r#"
SELECT qs_o.id,
       (SELECT qs_i.v
        FROM qs_i
        JOIN qs_j ON qs_j.id = qs_o.id
        WHERE qs_i.id = qs_j.id
        LIMIT 1) AS vv
FROM qs_o JOIN qs_i ON qs_o.id = qs_i.id
ORDER BY qs_o.id;
"#;

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).expect("parse SQL");
        let sqlparser::ast::Statement::Query(outer_query) = &statements[0] else {
            panic!("expected query statement");
        };
        let sqlparser::ast::SetExpr::Select(outer_select) = &*outer_query.body else {
            panic!("expected SELECT");
        };

        let subquery = outer_select
            .projection
            .iter()
            .find_map(|item| match item {
                sqlparser::ast::SelectItem::UnnamedExpr(e)
                | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } => match e {
                    Expr::Subquery(q) => Some(q.as_ref()),
                    _ => None,
                },
                _ => None,
            })
            .expect("find scalar subquery in projection");

        assert!(query_has_outer_reference(subquery, "qs_o"));

        let sqlparser::ast::SetExpr::Select(sub_select) = &*subquery.body else {
            panic!("expected subquery SELECT");
        };
        let join = &sub_select.from[0].joins[0];
        let on_expr = match &join.join_operator {
            sqlparser::ast::JoinOperator::Inner(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::RightOuter(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::FullOuter(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::LeftSemi(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::RightSemi(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::LeftAnti(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::RightAnti(sqlparser::ast::JoinConstraint::On(e)) => e,
            other => panic!("expected JOIN ... ON, got {other:?}"),
        };
        assert!(expr_has_outer_reference(on_expr, "qs_o"));

        let outer_schema = TableSchema {
            name: "qs_o".to_string(),
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            ..Default::default()
        };
        let outer_row = Row::new(vec![Value::Int32(1)]);

        let substituted =
            substitute_outer_values_in_query(subquery, "qs_o", &outer_schema, &outer_row, None);
        assert!(!query_has_outer_reference(&substituted, "qs_o"));

        let sqlparser::ast::SetExpr::Select(substituted_select) = &*substituted.body else {
            panic!("expected substituted SELECT");
        };
        let substituted_join = &substituted_select.from[0].joins[0];
        let substituted_on_expr = match &substituted_join.join_operator {
            sqlparser::ast::JoinOperator::Inner(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::RightOuter(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::FullOuter(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::LeftSemi(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::RightSemi(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::LeftAnti(sqlparser::ast::JoinConstraint::On(e))
            | sqlparser::ast::JoinOperator::RightAnti(sqlparser::ast::JoinConstraint::On(e)) => e,
            other => panic!("expected substituted JOIN ... ON, got {other:?}"),
        };
        assert!(!expr_has_outer_reference(substituted_on_expr, "qs_o"));
    }

    fn make_col(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        }
    }

    fn make_schema(cols: &[&str]) -> TableSchema {
        TableSchema {
            columns: cols.iter().map(|c| make_col(c)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_query_has_bare_outer_reference_detects_matching_column() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "SELECT a + c FROM t_inner";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["a", "b"]);
        assert!(query_has_bare_outer_reference(query, &outer_schema));
    }

    #[test]
    fn test_query_has_bare_outer_reference_ignores_non_matching() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "SELECT c + d FROM t_inner";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["a"]);
        assert!(!query_has_bare_outer_reference(query, &outer_schema));
    }

    #[test]
    fn test_bare_outer_ref_substituted_via_inner_columns() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "SELECT a + c FROM t_inner WHERE d > a";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["a", "b"]);
        let outer_row = Row::new(vec![Value::Int32(42), Value::Int32(99)]);
        let inner_columns: HashSet<String> = ["c", "d"].iter().map(|s| s.to_string()).collect();

        let substituted = substitute_outer_values_in_query(
            query,
            "t_outer",
            &outer_schema,
            &outer_row,
            Some(&inner_columns),
        );

        // After substitution, bare `a` (an outer column not in inner_columns)
        // should be replaced with the literal 42.
        // `c` and `d` are in inner_columns so should remain bare.
        let substituted_sql = substituted.to_string();
        assert!(
            substituted_sql.contains("42"),
            "expected literal 42 for outer column `a`: {substituted_sql}"
        );
        assert!(
            !substituted_sql.contains("t_outer"),
            "should not contain t_outer qualifier: {substituted_sql}"
        );
    }

    #[test]
    fn test_bare_outer_ref_inner_shadow_prevents_substitution() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "SELECT a FROM t_both";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["a"]);
        let outer_row = Row::new(vec![Value::Int32(42)]);
        // Inner table also has column "a" — it should shadow the outer.
        let inner_columns: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();

        let substituted = substitute_outer_values_in_query(
            query,
            "t_outer",
            &outer_schema,
            &outer_row,
            Some(&inner_columns),
        );
        let substituted_sql = substituted.to_string();

        // `a` exists in inner scope, so it must NOT be substituted.
        assert!(
            !substituted_sql.contains("42"),
            "inner-scope `a` should not be substituted: {substituted_sql}"
        );
    }

    #[test]
    fn test_cte_substituted_even_when_body_shadows_alias() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        // The CTE references `t_outer.id` (outer scope).  The body's FROM
        // shadows `t_outer`, so the body should NOT be substituted — but the
        // CTE must still be substituted because it is defined in the outer scope.
        let sql = "\
            WITH helper AS (SELECT t_outer.id AS oid FROM other_table) \
            SELECT helper.oid FROM helper, some_table AS t_outer \
            WHERE t_outer.id = helper.oid";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["id"]);
        let outer_row = Row::new(vec![Value::Int32(77)]);

        let substituted =
            substitute_outer_values_in_query(query, "t_outer", &outer_schema, &outer_row, None);
        let sql_out = substituted.to_string();

        // CTE should have `t_outer.id` replaced with 77.
        let cte_query = &substituted.with.as_ref().unwrap().cte_tables[0].query;
        let cte_sql = cte_query.to_string();
        assert!(
            cte_sql.contains("77"),
            "CTE should have outer ref substituted: {cte_sql}"
        );

        // Body's WHERE `t_outer.id` should NOT be substituted (shadowed by FROM).
        let SetExpr::Select(select) = &*substituted.body else {
            panic!("expected SELECT body");
        };
        let where_sql = select.selection.as_ref().unwrap().to_string();
        assert!(
            where_sql.contains("t_outer.id"),
            "body WHERE should keep t_outer.id (shadowed): {sql_out}"
        );
    }

    /// Same scenario as `test_cte_substituted_even_when_body_shadows_alias`,
    /// but entered through the expr-entry path (`substitute_outer_values`).
    /// Before the visitor handled CTE scoping, this path would incorrectly
    /// suppress substitution inside the CTE.
    #[test]
    fn test_cte_substitution_via_expr_entry_path() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "\
            WITH helper AS (SELECT t_outer.id AS oid FROM other_table) \
            SELECT helper.oid FROM helper, some_table AS t_outer \
            WHERE t_outer.id = helper.oid";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };
        // Wrap the parsed query in an Expr::Subquery to exercise the
        // expr-entry path (substitute_outer_values) instead of
        // substitute_outer_values_in_query.
        let expr = Expr::Subquery(query.clone());

        let outer_schema = make_schema(&["id"]);
        let outer_row = Row::new(vec![Value::Int32(77)]);

        let substituted =
            substitute_outer_values(&expr, "t_outer", &outer_schema, &outer_row, None);

        let Expr::Subquery(ref sub_query) = substituted else {
            panic!("expected Subquery expr");
        };

        // CTE should have `t_outer.id` replaced with 77.
        let cte_query = &sub_query.with.as_ref().unwrap().cte_tables[0].query;
        let cte_sql = cte_query.to_string();
        assert!(
            cte_sql.contains("77"),
            "CTE should have outer ref substituted via expr path: {cte_sql}"
        );

        // Body's WHERE `t_outer.id` should NOT be substituted (shadowed).
        let SetExpr::Select(select) = &*sub_query.body else {
            panic!("expected SELECT body");
        };
        let where_sql = select.selection.as_ref().unwrap().to_string();
        assert!(
            where_sql.contains("t_outer.id"),
            "body WHERE should keep t_outer.id (shadowed) via expr path: {where_sql}"
        );
    }

    #[test]
    fn test_setop_arm_shadowing_prevents_substitution() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        // UNION query: left arm shadows `t_outer` in its FROM.
        let sql = "SELECT 1 FROM some_table AS t_outer WHERE t_outer.id = 5 UNION SELECT 2";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["id"]);
        let outer_row = Row::new(vec![Value::Int32(77)]);

        let substituted =
            substitute_outer_values_in_query(query, "t_outer", &outer_schema, &outer_row, None);
        let out_sql = substituted.to_string();

        // Left arm WHERE keeps `t_outer.id` (inner alias), no literal 77 inserted.
        assert!(
            out_sql.contains("t_outer.id"),
            "shadowed t_outer.id should not be substituted: {out_sql}"
        );
        assert!(
            !out_sql.contains("77"),
            "literal 77 should not appear (shadowed): {out_sql}"
        );
    }

    #[test]
    fn test_setop_arm_without_shadow_allows_substitution() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        // UNION query: left arm does NOT shadow `t_outer`.
        let sql = "SELECT t_outer.id FROM other_table WHERE t_outer.id = 5 UNION SELECT 2";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["id"]);
        let outer_row = Row::new(vec![Value::Int32(77)]);

        let substituted =
            substitute_outer_values_in_query(query, "t_outer", &outer_schema, &outer_row, None);
        let out_sql = substituted.to_string();

        // Left arm does NOT shadow t_outer, so substitution should happen.
        assert!(
            out_sql.contains("77"),
            "non-shadowed t_outer.id should be substituted: {out_sql}"
        );
    }
}
