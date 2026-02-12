//! Subquery resolution for the SQL executor

use super::super::names::{self, normalize_ident};
use super::super::value_coercion::value_to_sql_expr;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Ident, Query, SelectItem, SetExpr, Value as SqlValue,
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

/// Qualify bare identifiers in a subquery that match outer-schema columns and
/// are NOT shadowed by the subquery's own inner-scope columns.
///
/// Rewrites `Expr::Identifier("col")` → `Expr::CompoundIdentifier(["outer_alias", "col"])`
/// so that the existing qualified-reference substitution handles them.
///
/// Only touches expressions at the subquery's immediate level; nested
/// subqueries are left untouched (they need their own inner-column sets).
pub(crate) fn qualify_bare_outer_refs_in_query(
    query: &Query,
    outer_alias: &str,
    outer_schema: &TableSchema,
    inner_columns: &HashSet<String>,
) -> Query {
    use core::ops::ControlFlow;
    use sqlparser::ast::{VisitMut, VisitorMut};

    struct QualifyVisitor<'a> {
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        inner_columns: &'a HashSet<String>,
        query_depth: usize,
    }

    impl<'a> VisitorMut for QualifyVisitor<'a> {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.query_depth += 1;
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.query_depth -= 1;
            ControlFlow::Continue(())
        }

        fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
            // Only qualify at the immediate query level (depth 1: we entered
            // the query node).  Deeper levels are nested subqueries.
            if self.query_depth != 1 {
                return ControlFlow::Continue(());
            }
            if let Expr::Identifier(ref ident) = expr {
                let name = normalize_ident(ident);
                // Skip if this column exists in inner scope.
                if self.inner_columns.contains(&name.to_lowercase()) {
                    return ControlFlow::Continue(());
                }
                // Qualify if it matches an outer column.
                if self
                    .outer_schema
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&name))
                {
                    *expr =
                        Expr::CompoundIdentifier(vec![Ident::new(self.outer_alias), ident.clone()]);
                }
            }
            ControlFlow::Continue(())
        }
    }

    let mut out = query.clone();
    let mut visitor = QualifyVisitor {
        outer_alias,
        outer_schema,
        inner_columns,
        query_depth: 0,
    };
    let _ = out.visit(&mut visitor);
    out
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
            // Qualify bare outer references before substitution.
            let qualified_query = if let Some(inner_columns) =
                collect_inner_column_names(&self.store(), txn, db_id, search_path, subquery, ctes)
                    .await?
            {
                qualify_bare_outer_refs_in_query(
                    subquery,
                    outer_alias,
                    outer_schema,
                    &inner_columns,
                )
            } else {
                subquery.clone()
            };

            let substituted_query = substitute_outer_values_in_query(
                &qualified_query,
                outer_alias,
                outer_schema,
                outer_row,
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
    fn table_factor_shadows_outer_alias(
        factor: &sqlparser::ast::TableFactor,
        outer_alias: &str,
    ) -> bool {
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
                    table_with_joins_shadows_outer_alias(table_with_joins, outer_alias)
                }
            }
            sqlparser::ast::TableFactor::Pivot { alias, .. }
            | sqlparser::ast::TableFactor::Unpivot { alias, .. } => alias
                .as_ref()
                .map(|a| normalize_ident(&a.name))
                .is_some_and(|a| a.eq_ignore_ascii_case(outer_alias)),
        }
    }

    fn table_with_joins_shadows_outer_alias(
        table_with_joins: &sqlparser::ast::TableWithJoins,
        outer_alias: &str,
    ) -> bool {
        if table_factor_shadows_outer_alias(&table_with_joins.relation, outer_alias) {
            return true;
        }
        for join in &table_with_joins.joins {
            if table_factor_shadows_outer_alias(&join.relation, outer_alias) {
                return true;
            }
        }
        false
    }

    fn from_shadows_outer_alias(
        from: &[sqlparser::ast::TableWithJoins],
        outer_alias: &str,
    ) -> bool {
        from.iter()
            .any(|twj| table_with_joins_shadows_outer_alias(twj, outer_alias))
    }

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
        if from_shadows_outer_alias(&select.from, outer_alias) {
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
        if from_shadows_outer_alias(&select.from, outer_alias) {
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

/// Substitute outer table column references with literal values from the current row
pub fn substitute_outer_values(
    expr: &Expr,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
) -> Expr {
    use core::ops::ControlFlow;
    use sqlparser::ast::{VisitMut, VisitorMut};

    #[derive(Clone)]
    struct SubstituteVisitor<'a> {
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
        query_depth: usize,
    }

    impl<'a> VisitorMut for SubstituteVisitor<'a> {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.query_depth = self.query_depth.saturating_add(1);
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.query_depth = self.query_depth.saturating_sub(1);
            ControlFlow::Continue(())
        }

        fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
            // Never mutate inside nested query scopes here; subqueries are substituted via
            // `substitute_outer_values_in_query`, which handles alias shadowing per query block.
            if self.query_depth > 0 {
                return ControlFlow::Continue(());
            }

            match expr {
                Expr::CompoundIdentifier(parts) => {
                    if parts.len() >= 2 {
                        // Support schema-qualified (schema.table.col) and even db.schema.table.col by
                        // treating the second-to-last identifier as the table/alias.
                        let table_part = normalize_ident(&parts[parts.len() - 2]);
                        if table_part.eq_ignore_ascii_case(self.outer_alias) {
                            // INTENTIONAL: sqlparser guarantees non-empty ObjectName from parsed SQL
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
                }
                Expr::Subquery(q) => {
                    let substituted = substitute_outer_values_in_query(
                        q,
                        self.outer_alias,
                        self.outer_schema,
                        self.outer_row,
                    );
                    *q = Box::new(substituted);
                }
                Expr::InSubquery { subquery, .. } => {
                    let substituted = substitute_outer_values_in_query(
                        subquery,
                        self.outer_alias,
                        self.outer_schema,
                        self.outer_row,
                    );
                    *subquery = Box::new(substituted);
                }
                Expr::Exists { subquery, .. } => {
                    let substituted = substitute_outer_values_in_query(
                        subquery,
                        self.outer_alias,
                        self.outer_schema,
                        self.outer_row,
                    );
                    *subquery = Box::new(substituted);
                }
                _ => {}
            }

            ControlFlow::Continue(())
        }
    }

    let mut out = expr.clone();
    let mut visitor = SubstituteVisitor {
        outer_alias,
        outer_schema,
        outer_row,
        query_depth: 0,
    };
    let _ = out.visit(&mut visitor);
    out
}

/// Substitute outer values in a query
pub fn substitute_outer_values_in_query(
    query: &Query,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
) -> Query {
    fn table_factor_shadows_outer_alias(
        factor: &sqlparser::ast::TableFactor,
        outer_alias: &str,
    ) -> bool {
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
                    table_with_joins_shadows_outer_alias(table_with_joins, outer_alias)
                }
            }
            sqlparser::ast::TableFactor::Pivot { alias, .. }
            | sqlparser::ast::TableFactor::Unpivot { alias, .. } => alias
                .as_ref()
                .map(|a| normalize_ident(&a.name))
                .is_some_and(|a| a.eq_ignore_ascii_case(outer_alias)),
        }
    }

    fn table_with_joins_shadows_outer_alias(
        table_with_joins: &sqlparser::ast::TableWithJoins,
        outer_alias: &str,
    ) -> bool {
        if table_factor_shadows_outer_alias(&table_with_joins.relation, outer_alias) {
            return true;
        }
        for join in &table_with_joins.joins {
            if table_factor_shadows_outer_alias(&join.relation, outer_alias) {
                return true;
            }
        }
        false
    }

    fn from_shadows_outer_alias(
        from: &[sqlparser::ast::TableWithJoins],
        outer_alias: &str,
    ) -> bool {
        from.iter()
            .any(|twj| table_with_joins_shadows_outer_alias(twj, outer_alias))
    }

    fn substitute_window_frame_bound(
        bound: &sqlparser::ast::WindowFrameBound,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::WindowFrameBound {
        match bound {
            sqlparser::ast::WindowFrameBound::CurrentRow => bound.clone(),
            sqlparser::ast::WindowFrameBound::Preceding(Some(expr)) => {
                sqlparser::ast::WindowFrameBound::Preceding(Some(Box::new(
                    substitute_outer_values(expr, outer_alias, outer_schema, outer_row),
                )))
            }
            sqlparser::ast::WindowFrameBound::Following(Some(expr)) => {
                sqlparser::ast::WindowFrameBound::Following(Some(Box::new(
                    substitute_outer_values(expr, outer_alias, outer_schema, outer_row),
                )))
            }
            sqlparser::ast::WindowFrameBound::Preceding(None)
            | sqlparser::ast::WindowFrameBound::Following(None) => bound.clone(),
        }
    }

    fn substitute_window_spec(
        spec: &sqlparser::ast::WindowSpec,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::WindowSpec {
        let new_partition_by = spec
            .partition_by
            .iter()
            .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
            .collect();
        let new_order_by = spec
            .order_by
            .iter()
            .map(|o| sqlparser::ast::OrderByExpr {
                expr: substitute_outer_values(&o.expr, outer_alias, outer_schema, outer_row),
                asc: o.asc,
                nulls_first: o.nulls_first,
            })
            .collect();

        let new_frame = spec.window_frame.as_ref().map(|f| {
            let new_start =
                substitute_window_frame_bound(&f.start_bound, outer_alias, outer_schema, outer_row);
            let new_end = f
                .end_bound
                .as_ref()
                .map(|b| substitute_window_frame_bound(b, outer_alias, outer_schema, outer_row));
            sqlparser::ast::WindowFrame {
                units: f.units,
                start_bound: new_start,
                end_bound: new_end,
            }
        });

        sqlparser::ast::WindowSpec {
            partition_by: new_partition_by,
            order_by: new_order_by,
            window_frame: new_frame,
        }
    }

    fn substitute_join_operator(
        op: &sqlparser::ast::JoinOperator,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::JoinOperator {
        match op {
            sqlparser::ast::JoinOperator::Inner(c) => sqlparser::ast::JoinOperator::Inner(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::LeftOuter(c) => sqlparser::ast::JoinOperator::LeftOuter(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::RightOuter(c) => {
                sqlparser::ast::JoinOperator::RightOuter(substitute_join_constraint(
                    c,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ))
            }
            sqlparser::ast::JoinOperator::FullOuter(c) => sqlparser::ast::JoinOperator::FullOuter(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::LeftSemi(c) => sqlparser::ast::JoinOperator::LeftSemi(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::RightSemi(c) => sqlparser::ast::JoinOperator::RightSemi(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::LeftAnti(c) => sqlparser::ast::JoinOperator::LeftAnti(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::RightAnti(c) => sqlparser::ast::JoinOperator::RightAnti(
                substitute_join_constraint(c, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinOperator::CrossJoin => sqlparser::ast::JoinOperator::CrossJoin,
            sqlparser::ast::JoinOperator::CrossApply => sqlparser::ast::JoinOperator::CrossApply,
            sqlparser::ast::JoinOperator::OuterApply => sqlparser::ast::JoinOperator::OuterApply,
        }
    }

    fn substitute_join_constraint(
        c: &sqlparser::ast::JoinConstraint,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::JoinConstraint {
        match c {
            sqlparser::ast::JoinConstraint::On(e) => sqlparser::ast::JoinConstraint::On(
                substitute_outer_values(e, outer_alias, outer_schema, outer_row),
            ),
            sqlparser::ast::JoinConstraint::Using(cols) => {
                sqlparser::ast::JoinConstraint::Using(cols.clone())
            }
            sqlparser::ast::JoinConstraint::Natural => sqlparser::ast::JoinConstraint::Natural,
            sqlparser::ast::JoinConstraint::None => sqlparser::ast::JoinConstraint::None,
        }
    }

    fn substitute_table_factor(
        factor: &sqlparser::ast::TableFactor,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::TableFactor {
        match factor {
            sqlparser::ast::TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                version,
                partitions,
            } => {
                let new_args = args.as_ref().map(|args| {
                    args.iter()
                        .map(|arg| match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => FunctionArg::Unnamed(
                                FunctionArgExpr::Expr(substitute_outer_values(
                                    e,
                                    outer_alias,
                                    outer_schema,
                                    outer_row,
                                )),
                            ),
                            other => other.clone(),
                        })
                        .collect()
                });

                let new_hints = with_hints
                    .iter()
                    .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
                    .collect();

                let new_version =
                    version.as_ref().map(|v| match v {
                        sqlparser::ast::TableVersion::ForSystemTimeAsOf(e) => {
                            sqlparser::ast::TableVersion::ForSystemTimeAsOf(
                                substitute_outer_values(e, outer_alias, outer_schema, outer_row),
                            )
                        }
                    });

                sqlparser::ast::TableFactor::Table {
                    name: name.clone(),
                    alias: alias.clone(),
                    args: new_args,
                    with_hints: new_hints,
                    version: new_version,
                    partitions: partitions.clone(),
                }
            }
            sqlparser::ast::TableFactor::Derived {
                lateral,
                subquery,
                alias,
            } => sqlparser::ast::TableFactor::Derived {
                lateral: *lateral,
                subquery: Box::new(substitute_outer_values_in_query(
                    subquery,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
                alias: alias.clone(),
            },
            sqlparser::ast::TableFactor::TableFunction { expr, alias } => {
                sqlparser::ast::TableFactor::TableFunction {
                    expr: substitute_outer_values(expr, outer_alias, outer_schema, outer_row),
                    alias: alias.clone(),
                }
            }
            sqlparser::ast::TableFactor::Function {
                lateral,
                name,
                args,
                alias,
            } => sqlparser::ast::TableFactor::Function {
                lateral: *lateral,
                name: name.clone(),
                args: args
                    .iter()
                    .map(|arg| match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(substitute_outer_values(
                                e,
                                outer_alias,
                                outer_schema,
                                outer_row,
                            )))
                        }
                        other => other.clone(),
                    })
                    .collect(),
                alias: alias.clone(),
            },
            sqlparser::ast::TableFactor::UNNEST {
                alias,
                array_exprs,
                with_offset,
                with_offset_alias,
            } => sqlparser::ast::TableFactor::UNNEST {
                alias: alias.clone(),
                array_exprs: array_exprs
                    .iter()
                    .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
                    .collect(),
                with_offset: *with_offset,
                with_offset_alias: with_offset_alias.clone(),
            },
            sqlparser::ast::TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => sqlparser::ast::TableFactor::NestedJoin {
                table_with_joins: Box::new(substitute_table_with_joins(
                    table_with_joins,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
                alias: alias.clone(),
            },
            sqlparser::ast::TableFactor::Pivot {
                table,
                aggregate_function,
                value_column,
                pivot_values,
                alias,
            } => sqlparser::ast::TableFactor::Pivot {
                table: Box::new(substitute_table_factor(
                    table,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
                aggregate_function: substitute_outer_values(
                    aggregate_function,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ),
                value_column: value_column.clone(),
                pivot_values: pivot_values.clone(),
                alias: alias.clone(),
            },
            sqlparser::ast::TableFactor::Unpivot {
                table,
                value,
                name,
                columns,
                alias,
            } => sqlparser::ast::TableFactor::Unpivot {
                table: Box::new(substitute_table_factor(
                    table,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
                value: value.clone(),
                name: name.clone(),
                columns: columns.clone(),
                alias: alias.clone(),
            },
        }
    }

    fn substitute_table_with_joins(
        twj: &sqlparser::ast::TableWithJoins,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::TableWithJoins {
        sqlparser::ast::TableWithJoins {
            relation: substitute_table_factor(&twj.relation, outer_alias, outer_schema, outer_row),
            joins: twj
                .joins
                .iter()
                .map(|j| sqlparser::ast::Join {
                    relation: substitute_table_factor(
                        &j.relation,
                        outer_alias,
                        outer_schema,
                        outer_row,
                    ),
                    join_operator: substitute_join_operator(
                        &j.join_operator,
                        outer_alias,
                        outer_schema,
                        outer_row,
                    ),
                })
                .collect(),
        }
    }

    fn substitute_select(
        select: &sqlparser::ast::Select,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> sqlparser::ast::Select {
        // FROM-scope alias shadowing: if the SELECT block defines the same alias as the
        // outer alias, treat it as non-outer-ref and avoid substitution in this scope.
        if from_shadows_outer_alias(&select.from, outer_alias) {
            return select.clone();
        }

        let new_distinct = select.distinct.as_ref().map(|d| match d {
            sqlparser::ast::Distinct::Distinct => sqlparser::ast::Distinct::Distinct,
            sqlparser::ast::Distinct::On(exprs) => sqlparser::ast::Distinct::On(
                exprs
                    .iter()
                    .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
                    .collect(),
            ),
        });

        let new_top = select.top.as_ref().map(|t| sqlparser::ast::Top {
            with_ties: t.with_ties,
            percent: t.percent,
            quantity: t
                .quantity
                .as_ref()
                .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row)),
        });

        let new_projection = select
            .projection
            .iter()
            .map(|item| match item {
                sqlparser::ast::SelectItem::UnnamedExpr(e) => {
                    sqlparser::ast::SelectItem::UnnamedExpr(substitute_outer_values(
                        e,
                        outer_alias,
                        outer_schema,
                        outer_row,
                    ))
                }
                sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                    sqlparser::ast::SelectItem::ExprWithAlias {
                        expr: substitute_outer_values(expr, outer_alias, outer_schema, outer_row),
                        alias: alias.clone(),
                    }
                }
                other => other.clone(),
            })
            .collect();

        let new_from = select
            .from
            .iter()
            .map(|twj| substitute_table_with_joins(twj, outer_alias, outer_schema, outer_row))
            .collect();

        let new_lateral_views = select
            .lateral_views
            .iter()
            .map(|lv| sqlparser::ast::LateralView {
                lateral_view: substitute_outer_values(
                    &lv.lateral_view,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ),
                lateral_view_name: lv.lateral_view_name.clone(),
                lateral_col_alias: lv.lateral_col_alias.clone(),
                outer: lv.outer,
            })
            .collect();

        let new_selection = select
            .selection
            .as_ref()
            .map(|sel| substitute_outer_values(sel, outer_alias, outer_schema, outer_row));

        let new_group_by = match &select.group_by {
            sqlparser::ast::GroupByExpr::All => sqlparser::ast::GroupByExpr::All,
            sqlparser::ast::GroupByExpr::Expressions(exprs) => {
                sqlparser::ast::GroupByExpr::Expressions(
                    exprs
                        .iter()
                        .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
                        .collect(),
                )
            }
        };

        let new_cluster_by = select
            .cluster_by
            .iter()
            .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
            .collect();
        let new_distribute_by = select
            .distribute_by
            .iter()
            .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
            .collect();
        let new_sort_by = select
            .sort_by
            .iter()
            .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
            .collect();

        let new_having = select
            .having
            .as_ref()
            .map(|h| substitute_outer_values(h, outer_alias, outer_schema, outer_row));

        let new_named_window = select
            .named_window
            .iter()
            .map(|def| {
                sqlparser::ast::NamedWindowDefinition(
                    def.0.clone(),
                    substitute_window_spec(&def.1, outer_alias, outer_schema, outer_row),
                )
            })
            .collect();

        let new_qualify = select
            .qualify
            .as_ref()
            .map(|q| substitute_outer_values(q, outer_alias, outer_schema, outer_row));

        sqlparser::ast::Select {
            distinct: new_distinct,
            top: new_top,
            projection: new_projection,
            into: select.into.clone(),
            from: new_from,
            lateral_views: new_lateral_views,
            selection: new_selection,
            group_by: new_group_by,
            cluster_by: new_cluster_by,
            distribute_by: new_distribute_by,
            sort_by: new_sort_by,
            having: new_having,
            named_window: new_named_window,
            qualify: new_qualify,
        }
    }

    fn substitute_set_expr(
        body: &SetExpr,
        outer_alias: &str,
        outer_schema: &TableSchema,
        outer_row: &Row,
    ) -> SetExpr {
        match body {
            SetExpr::Select(select) => SetExpr::Select(Box::new(substitute_select(
                select,
                outer_alias,
                outer_schema,
                outer_row,
            ))),
            SetExpr::Query(q) => SetExpr::Query(Box::new(substitute_outer_values_in_query(
                q,
                outer_alias,
                outer_schema,
                outer_row,
            ))),
            SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } => SetExpr::SetOperation {
                op: *op,
                set_quantifier: *set_quantifier,
                left: Box::new(substitute_set_expr(
                    left,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
                right: Box::new(substitute_set_expr(
                    right,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
            },
            SetExpr::Values(values) => SetExpr::Values(sqlparser::ast::Values {
                explicit_row: values.explicit_row,
                rows: values
                    .rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|e| {
                                substitute_outer_values(e, outer_alias, outer_schema, outer_row)
                            })
                            .collect()
                    })
                    .collect(),
            }),
            _ => body.clone(),
        }
    }

    let new_with = query.with.as_ref().map(|w| sqlparser::ast::With {
        recursive: w.recursive,
        cte_tables: w
            .cte_tables
            .iter()
            .map(|cte| sqlparser::ast::Cte {
                alias: cte.alias.clone(),
                query: Box::new(substitute_outer_values_in_query(
                    &cte.query,
                    outer_alias,
                    outer_schema,
                    outer_row,
                )),
                from: cte.from.clone(),
            })
            .collect(),
    });

    // Preserve existing FROM-scope alias shadowing behavior for SELECT query blocks:
    // if this query block defines `outer_alias` in its FROM, do not substitute within it.
    if let SetExpr::Select(select) = &*query.body {
        if from_shadows_outer_alias(&select.from, outer_alias) {
            return Query {
                with: new_with,
                body: query.body.clone(),
                order_by: query.order_by.clone(),
                limit: query.limit.clone(),
                offset: query.offset.clone(),
                fetch: query.fetch.clone(),
                locks: query.locks.clone(),
                limit_by: query.limit_by.clone(),
                for_clause: query.for_clause.clone(),
            };
        }
    }

    let new_body = Box::new(substitute_set_expr(
        &query.body,
        outer_alias,
        outer_schema,
        outer_row,
    ));

    let new_order_by = query
        .order_by
        .iter()
        .map(|o| sqlparser::ast::OrderByExpr {
            expr: substitute_outer_values(&o.expr, outer_alias, outer_schema, outer_row),
            asc: o.asc,
            nulls_first: o.nulls_first,
        })
        .collect();

    let new_limit = query
        .limit
        .as_ref()
        .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row));

    let new_limit_by = query
        .limit_by
        .iter()
        .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row))
        .collect();

    let new_offset = query.offset.as_ref().map(|o| sqlparser::ast::Offset {
        value: substitute_outer_values(&o.value, outer_alias, outer_schema, outer_row),
        rows: o.rows,
    });

    let new_fetch = query.fetch.as_ref().map(|f| sqlparser::ast::Fetch {
        with_ties: f.with_ties,
        percent: f.percent,
        quantity: f
            .quantity
            .as_ref()
            .map(|e| substitute_outer_values(e, outer_alias, outer_schema, outer_row)),
    });

    Query {
        with: new_with,
        body: new_body,
        order_by: new_order_by,
        limit: new_limit,
        offset: new_offset,
        fetch: new_fetch,
        locks: query.locks.clone(),
        limit_by: new_limit_by,
        for_clause: query.for_clause.clone(),
    }
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
        let out = substitute_outer_values(&expr, "o", &outer_schema, &outer_row);
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
            substitute_outer_values_in_query(subquery, "qs_o", &outer_schema, &outer_row);
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
    fn test_qualify_bare_outer_refs_in_query() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "SELECT a + c FROM t_inner WHERE d > a";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["a", "b"]);
        let inner_columns: HashSet<String> = ["c", "d"].iter().map(|s| s.to_string()).collect();

        let qualified =
            qualify_bare_outer_refs_in_query(query, "t_outer", &outer_schema, &inner_columns);

        // After qualification, `a` should become `t_outer.a` (a CompoundIdentifier).
        // `c` and `d` are in inner_columns so should remain bare.
        let qualified_sql = qualified.to_string();
        assert!(
            qualified_sql.contains("t_outer.a"),
            "expected t_outer.a in: {qualified_sql}"
        );
        assert!(
            !qualified_sql.contains("t_outer.c"),
            "c should not be qualified: {qualified_sql}"
        );
        assert!(
            !qualified_sql.contains("t_outer.d"),
            "d should not be qualified: {qualified_sql}"
        );
    }

    #[test]
    fn test_qualify_bare_outer_refs_inner_shadow_prevents_qualification() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "SELECT a FROM t_both";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["a"]);
        // Inner table also has column "a" — it should shadow the outer.
        let inner_columns: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();

        let qualified =
            qualify_bare_outer_refs_in_query(query, "t_outer", &outer_schema, &inner_columns);
        let qualified_sql = qualified.to_string();

        // `a` exists in inner scope, so it must NOT be qualified.
        assert!(
            !qualified_sql.contains("t_outer.a"),
            "inner-scope `a` should not be qualified: {qualified_sql}"
        );
    }

    /// Regression test for #678: substitute_outer_values (expr-entry path)
    /// must still substitute inside CTEs even when the query body shadows
    /// the outer alias.
    #[test]
    fn test_substitute_outer_values_subquery_cte_not_blocked_by_body_shadowing() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let sql = "WITH helper AS (SELECT t_outer.id AS oid FROM other_table) \
                    SELECT helper.oid FROM helper, some_table AS t_outer \
                    WHERE t_outer.id = helper.oid";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let expr = Expr::Subquery(query.clone());
        let outer_schema = make_schema(&["id"]);
        let outer_row = Row::new(vec![Value::Int32(77)]);

        let substituted_expr =
            substitute_outer_values(&expr, "t_outer", &outer_schema, &outer_row);
        let Expr::Subquery(substituted_query) = substituted_expr else {
            panic!("expected subquery");
        };

        // CTE should have `t_outer.id` replaced with 77.
        let cte_query = &substituted_query.with.as_ref().unwrap().cte_tables[0].query;
        let cte_sql = cte_query.to_string();
        assert!(
            cte_sql.contains("77"),
            "CTE should have outer ref substituted via expr path: {cte_sql}"
        );

        // Body's WHERE `t_outer.id` should NOT be substituted (shadowed by FROM).
        let SetExpr::Select(select) = &*substituted_query.body else {
            panic!("expected SELECT body");
        };
        let where_sql = select.selection.as_ref().unwrap().to_string();
        assert!(
            where_sql.contains("t_outer.id"),
            "body WHERE should keep t_outer.id (shadowed): {where_sql}"
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
            substitute_outer_values_in_query(query, "t_outer", &outer_schema, &outer_row);
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
        let sql =
            "SELECT t_outer.id FROM other_table WHERE t_outer.id = 5 UNION SELECT 2";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected query");
        };

        let outer_schema = make_schema(&["id"]);
        let outer_row = Row::new(vec![Value::Int32(77)]);

        let substituted =
            substitute_outer_values_in_query(query, "t_outer", &outer_schema, &outer_row);
        let out_sql = substituted.to_string();

        // Left arm does NOT shadow t_outer, so substitution should happen.
        assert!(
            out_sql.contains("77"),
            "non-shadowed t_outer.id should be substituted: {out_sql}"
        );
    }
}
