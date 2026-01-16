//! Subquery resolution for the SQL executor

use super::executor::Executor;
use super::helpers::{
    query_has_outer_reference, substitute_outer_values_in_query, value_to_sql_expr,
};
use super::ExecuteResult;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{BinaryOperator, Expr, Query, SelectItem, Value as SqlValue};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    pub(crate) fn resolve_subqueries<'a>(
        &'a self,
        txn: &'a mut Transaction,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        expr: &'a Expr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Expr>> + Send + 'a>> {
        Box::pin(async move {
            match expr {
                Expr::InSubquery {
                    expr: inner_expr,
                    subquery,
                    negated,
                } => {
                    let result = self
                        .execute_query(txn, sequence_values, search_path, subquery)
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
                        self.resolve_subqueries(txn, sequence_values, search_path, inner_expr)
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
                        self.resolve_subqueries(txn, sequence_values, search_path, left)
                            .await?,
                    );
                    let resolved_right = Box::new(
                        self.resolve_subqueries(txn, sequence_values, search_path, right)
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
                        self.resolve_subqueries(txn, sequence_values, search_path, inner)
                            .await?,
                    );
                    Ok(Expr::UnaryOp {
                        op: op.clone(),
                        expr: resolved,
                    })
                }
                Expr::Nested(inner) => {
                    let resolved = Box::new(
                        self.resolve_subqueries(txn, sequence_values, search_path, inner)
                            .await?,
                    );
                    Ok(Expr::Nested(resolved))
                }
                Expr::Subquery(subquery) => {
                    let result = self
                        .execute_query(txn, sequence_values, search_path, subquery)
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
                        .execute_query(txn, sequence_values, search_path, subquery)
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
                            self.resolve_subqueries(txn, sequence_values, search_path, op)
                                .await?,
                        ))
                    } else {
                        None
                    };
                    let mut resolved_conditions = Vec::with_capacity(conditions.len());
                    for cond in conditions {
                        resolved_conditions.push(
                            self.resolve_subqueries(txn, sequence_values, search_path, cond)
                                .await?,
                        );
                    }
                    let mut resolved_results = Vec::with_capacity(results.len());
                    for res in results {
                        resolved_results.push(
                            self.resolve_subqueries(txn, sequence_values, search_path, res)
                                .await?,
                        );
                    }
                    let resolved_else = if let Some(else_expr) = else_result {
                        Some(Box::new(
                            self.resolve_subqueries(txn, sequence_values, search_path, else_expr)
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
                                    self.resolve_subqueries(txn, sequence_values, search_path, e)
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
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        projection: &[SelectItem],
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => {
                    SelectItem::UnnamedExpr(
                        self.resolve_subqueries(txn, sequence_values, search_path, e)
                            .await?,
                    )
                }
                SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias {
                    expr: self
                        .resolve_subqueries(txn, sequence_values, search_path, expr)
                        .await?,
                    alias: alias.clone(),
                },
                other => other.clone(),
            };
            resolved.push(resolved_item);
        }
        Ok(resolved)
    }

    #[allow(dead_code)]
    pub(crate) async fn resolve_projection_subqueries_with_outer_context(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        projection: &[SelectItem],
        outer_alias: &str,
    ) -> Result<Vec<SelectItem>> {
        let mut resolved = Vec::with_capacity(projection.len());
        for item in projection {
            let resolved_item = match item {
                SelectItem::UnnamedExpr(e) => {
                    if self.expr_is_correlated_subquery(e, outer_alias) {
                        SelectItem::UnnamedExpr(e.clone())
                    } else {
                        SelectItem::UnnamedExpr(
                            self.resolve_subqueries(txn, sequence_values, search_path, e)
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
                                .resolve_subqueries(txn, sequence_values, search_path, expr)
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

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub(crate) fn eval_correlated_exists<'a>(
        &'a self,
        txn: &'a mut Transaction,
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
                .execute_query(txn, sequence_values, search_path, &substituted_query)
                .await?;
            let exists = match result {
                ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                _ => false,
            };
            let result_bool = if negated { !exists } else { exists };
            Ok(Value::Boolean(result_bool))
        })
    }

    #[allow(dead_code)]
    pub(crate) fn eval_selection_with_correlated_exists<'a>(
        &'a self,
        txn: &'a mut Transaction,
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
                            .execute_query(txn, sequence_values, search_path, subquery)
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
                            sequence_values,
                            right,
                            search_path,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    match op {
                        BinaryOperator::And => {
                            let left_bool = matches!(left_val, Value::Boolean(true));
                            let right_bool = matches!(right_val, Value::Boolean(true));
                            Ok(Value::Boolean(left_bool && right_bool))
                        }
                        BinaryOperator::Or => {
                            let left_bool = matches!(left_val, Value::Boolean(true));
                            let right_bool = matches!(right_val, Value::Boolean(true));
                            Ok(Value::Boolean(left_bool || right_bool))
                        }
                        _ => {
                            self.eval_expr_maybe_sequence(
                                txn,
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
                            sequence_values,
                            inner,
                            search_path,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )
                        .await?;
                    let inner_bool = matches!(inner_val, Value::Boolean(true));
                    Ok(Value::Boolean(!inner_bool))
                }
                Expr::Nested(inner) => {
                    self.eval_selection_with_correlated_exists(
                        txn,
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

    #[allow(dead_code)]
    pub(crate) fn eval_correlated_subquery<'a>(
        &'a self,
        txn: &'a mut Transaction,
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
                .execute_query(txn, sequence_values, search_path, &substituted_query)
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
