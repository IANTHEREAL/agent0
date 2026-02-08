//! Subquery resolution for the SQL executor

use super::super::expr::{coerce_text_literal_to_bool, eval_binary_op_public};
use super::super::names::normalize_ident;
use super::super::value_coercion::value_to_sql_expr;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, Query, SelectItem, SetExpr,
    Value as SqlValue,
};
use std::collections::HashMap;
use tikv_client::Transaction;

fn expr_contains_subquery(expr: &Expr) -> bool {
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

impl Executor {
    pub(crate) fn resolve_subqueries<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        expr: &'a Expr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
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
                    self.resolve_subqueries(txn, db_id, sequence_values, search_path, e, ctes)
                        .await?,
                ),
                SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias {
                    expr: self
                        .resolve_subqueries(txn, db_id, sequence_values, search_path, expr, ctes)
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
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => {
                    if self.expr_is_correlated_subquery(e, outer_alias) {
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
                            )
                            .await?,
                        )
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    if self.expr_is_correlated_subquery(expr, outer_alias) {
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

    pub(crate) fn expr_is_correlated_subquery(&self, expr: &Expr, outer_alias: &str) -> bool {
        match expr {
            Expr::Subquery(q) => query_has_outer_reference(q, outer_alias),
            _ => false,
        }
    }

    pub(crate) fn expr_has_correlated_exists(&self, expr: &Expr, outer_alias: &str) -> bool {
        match expr {
            Expr::Exists { subquery, .. } => query_has_outer_reference(subquery, outer_alias),
            Expr::BinaryOp { left, right, .. } => {
                self.expr_has_correlated_exists(left, outer_alias)
                    || self.expr_has_correlated_exists(right, outer_alias)
            }
            Expr::UnaryOp { expr: inner, .. } => {
                self.expr_has_correlated_exists(inner, outer_alias)
            }
            Expr::Nested(inner) => self.expr_has_correlated_exists(inner, outer_alias),
            _ => false,
        }
    }

    pub(crate) fn eval_correlated_exists<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        subquery: &'a Query,
        negated: bool,
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let substituted_query =
                substitute_outer_values_in_query(subquery, outer_alias, outer_schema, outer_row);
            let result = self
                .execute_query(txn, db_id, sequence_values, search_path, &substituted_query)
                .await?;
            let exists = match result {
                ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                _ => false,
            };
            let result_bool = if negated { !exists } else { exists };
            Ok(Value::Boolean(result_bool))
        })
    }

    pub(crate) fn eval_selection_with_correlated_exists<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        expr: &'a Expr,
        search_path: &'a [String],
        outer_alias: &'a str,
        outer_schema: &'a TableSchema,
        outer_row: &'a Row,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            match expr {
                Expr::Exists { subquery, negated } => {
                    if query_has_outer_reference(subquery, outer_alias) {
                        self.eval_correlated_exists(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            *negated,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await
                    } else {
                        let result = self
                            .execute_query(txn, db_id, sequence_values, search_path, subquery)
                            .await?;
                        let exists = match result {
                            ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                            _ => false,
                        };
                        let result_bool = if *negated { !exists } else { exists };
                        Ok(Value::Boolean(result_bool))
                    }
                }
                Expr::BinaryOp { left, op, right } => {
                    let left_val = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            db_id,
                            sequence_values,
                            left,
                            search_path,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    let right_val = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            db_id,
                            sequence_values,
                            right,
                            search_path,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    match op {
                        BinaryOperator::And | BinaryOperator::Or => {
                            let left_val = coerce_text_literal_to_bool(left, left_val)?;
                            let right_val = coerce_text_literal_to_bool(right, right_val)?;
                            eval_binary_op_public(left_val, op, right_val)
                        }
                        _ => {
                            self.eval_expr_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                expr,
                                Some(outer_row),
                                Some(outer_schema),
                            )
                            .await
                        }
                    }
                }
                Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Not,
                    expr: inner,
                } => {
                    let inner_val = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            db_id,
                            sequence_values,
                            inner,
                            search_path,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    let inner_val = coerce_text_literal_to_bool(inner, inner_val)?;
                    match inner_val {
                        Value::Boolean(b) => Ok(Value::Boolean(!b)),
                        Value::Null => Ok(Value::Null),
                        other => Err(anyhow!("NOT requires boolean, got {:?}", other)),
                    }
                }
                Expr::Nested(inner) => {
                    self.eval_selection_with_correlated_exists(
                        txn,
                        db_id,
                        sequence_values,
                        inner,
                        search_path,
                        outer_alias,
                        outer_schema,
                        outer_row,
                    )
                    .await
                }
                _ => {
                    self.eval_expr_maybe_sequence(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        expr,
                        Some(outer_row),
                        Some(outer_schema),
                    )
                    .await
                }
            }
        })
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let substituted_query =
                substitute_outer_values_in_query(subquery, outer_alias, outer_schema, outer_row);
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
    match expr {
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                // Support schema-qualified (schema.table.col) and even db.schema.table.col by
                // treating the second-to-last identifier as the table/alias.
                let table_part = normalize_ident(&parts[parts.len() - 2]);
                table_part.eq_ignore_ascii_case(outer_alias)
            } else {
                false
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_outer_reference(left, outer_alias)
                || expr_has_outer_reference(right, outer_alias)
        }
        Expr::UnaryOp { expr: inner, .. } => expr_has_outer_reference(inner, outer_alias),
        Expr::Nested(inner) => expr_has_outer_reference(inner, outer_alias),
        Expr::Function(f) => {
            for arg in &f.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                    if expr_has_outer_reference(e, outer_alias) {
                        return true;
                    }
                }
            }
            false
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                if expr_has_outer_reference(op, outer_alias) {
                    return true;
                }
            }
            for cond in conditions {
                if expr_has_outer_reference(cond, outer_alias) {
                    return true;
                }
            }
            for res in results {
                if expr_has_outer_reference(res, outer_alias) {
                    return true;
                }
            }
            if let Some(else_expr) = else_result {
                if expr_has_outer_reference(else_expr, outer_alias) {
                    return true;
                }
            }
            false
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            if expr_has_outer_reference(inner, outer_alias) {
                return true;
            }
            for item in list {
                if expr_has_outer_reference(item, outer_alias) {
                    return true;
                }
            }
            false
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_outer_reference(expr, outer_alias)
                || expr_has_outer_reference(low, outer_alias)
                || expr_has_outer_reference(high, outer_alias)
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expr_has_outer_reference(inner, outer_alias)
        }
        Expr::Subquery(q)
        | Expr::InSubquery { subquery: q, .. }
        | Expr::Exists { subquery: q, .. } => query_has_outer_reference(q, outer_alias),
        _ => false,
    }
}

/// Check if a query contains references to an outer table alias
pub fn query_has_outer_reference(query: &Query, outer_alias: &str) -> bool {
    match &*query.body {
        SetExpr::Select(select) => {
            // Check selection (WHERE clause)
            if let Some(selection) = &select.selection {
                if expr_has_outer_reference(selection, outer_alias) {
                    return true;
                }
            }
            // Check projection
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
            // Check HAVING
            if let Some(having) = &select.having {
                if expr_has_outer_reference(having, outer_alias) {
                    return true;
                }
            }
            false
        }
        SetExpr::SetOperation { left, right, .. } => {
            let left_query = Query {
                with: None,
                body: left.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
            let right_query = Query {
                with: None,
                body: right.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
            query_has_outer_reference(&left_query, outer_alias)
                || query_has_outer_reference(&right_query, outer_alias)
        }
        _ => false,
    }
}

/// Substitute outer table column references with literal values from the current row
pub fn substitute_outer_values(
    expr: &Expr,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
) -> Expr {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                // Support schema-qualified (schema.table.col) and even db.schema.table.col by
                // treating the second-to-last identifier as the table/alias.
                let table_part = normalize_ident(&parts[parts.len() - 2]);
                if table_part.eq_ignore_ascii_case(outer_alias) {
                    let col_name = parts
                        .last()
                        .map(normalize_ident)
                        .unwrap_or_else(|| "".to_string());
                    // Find column index in outer schema
                    if let Some(col_idx) = outer_schema
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(&col_name))
                    {
                        if let Some(value) = outer_row.values.get(col_idx) {
                            return value_to_sql_expr(value);
                        }
                    }
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_outer_values(
                left,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            op: op.clone(),
            right: Box::new(substitute_outer_values(
                right,
                outer_alias,
                outer_schema,
                outer_row,
            )),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            )),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(substitute_outer_values(
            inner,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::Function(f) => {
            let mut new_args = Vec::new();
            for arg in &f.args {
                let new_arg = match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(substitute_outer_values(
                            e,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )))
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
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            let new_operand = operand.as_ref().map(|op| {
                Box::new(substitute_outer_values(
                    op,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ))
            });
            let new_conditions: Vec<Expr> = conditions
                .iter()
                .map(|c| substitute_outer_values(c, outer_alias, outer_schema, outer_row))
                .collect();
            let new_results: Vec<Expr> = results
                .iter()
                .map(|r| substitute_outer_values(r, outer_alias, outer_schema, outer_row))
                .collect();
            let new_else = else_result.as_ref().map(|e| {
                Box::new(substitute_outer_values(
                    e,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ))
            });
            Expr::Case {
                operand: new_operand,
                conditions: new_conditions,
                results: new_results,
                else_result: new_else,
            }
        }
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => {
            let new_inner = Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            ));
            let new_list: Vec<Expr> = list
                .iter()
                .map(|item| substitute_outer_values(item, outer_alias, outer_schema, outer_row))
                .collect();
            Expr::InList {
                expr: new_inner,
                list: new_list,
                negated: *negated,
            }
        }
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            negated: *negated,
            low: Box::new(substitute_outer_values(
                low,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            high: Box::new(substitute_outer_values(
                high,
                outer_alias,
                outer_schema,
                outer_row,
            )),
        },
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_outer_values(
            inner,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(substitute_outer_values(
            inner,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::Subquery(q) => Expr::Subquery(Box::new(substitute_outer_values_in_query(
            q,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            subquery: Box::new(substitute_outer_values_in_query(
                subquery,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            negated: *negated,
        },
        Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(substitute_outer_values_in_query(
                subquery,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            negated: *negated,
        },
        _ => expr.clone(),
    }
}

/// Substitute outer values in a query
pub fn substitute_outer_values_in_query(
    query: &Query,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
) -> Query {
    let new_body = match &*query.body {
        SetExpr::Select(select) => {
            let new_selection = select
                .selection
                .as_ref()
                .map(|sel| substitute_outer_values(sel, outer_alias, outer_schema, outer_row));

            let new_projection: Vec<sqlparser::ast::SelectItem> = select
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
                            expr: substitute_outer_values(
                                expr,
                                outer_alias,
                                outer_schema,
                                outer_row,
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
                .map(|h| substitute_outer_values(h, outer_alias, outer_schema, outer_row));

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
}
