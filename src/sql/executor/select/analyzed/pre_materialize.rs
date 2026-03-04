//! Pre-materialization transform for async expressions in the analyzed SELECT path.
//!
//! Contains [`PreMaterializeTransform`], which implements [`AsyncExprTransform`]
//! to walk TypedExpr trees and replace non-correlated subqueries + sequence
//! function calls with their constant results.
//!
//! Also contains the `pre_materialize_async_exprs` method on [`Executor`] that
//! kicks off the transform.

use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExpr, TypedExprKind};
use crate::sql::error::SqlError;
use crate::sql::executor::core::Executor;
use crate::sql::expr::classify::needs_pre_materialization;
use crate::sql::expr::traverse::{map_children_async, AsyncExprTransform};
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::sequences::resolve_sequence_full_name_from_value;
use crate::sql::ExecuteResult;

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

use crate::sql::sequences::SequenceSession;

use super::postprocess::build_any_all_rhs_constant_expr;
use super::subquery::is_correlated_query;

fn pg_type_name_from_data_type(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "boolean".to_string(),
        DataType::Int32 => "integer".to_string(),
        DataType::Int64 => "bigint".to_string(),
        DataType::Float64 => "double precision".to_string(),
        DataType::Numeric { .. } => "numeric".to_string(),
        _ => data_type.to_string().to_lowercase(),
    }
}

fn pg_lastval_arg_type_name(arg: &TypedExpr) -> String {
    if matches!(
        &arg.kind,
        TypedExprKind::Constant(Value::Text(_)) | TypedExprKind::Constant(Value::Null)
    ) {
        return "unknown".to_string();
    }

    // PostgreSQL reports scientific-notation numeric literals (parsed in db9 as
    // float constants) as NUMERIC in function-signature errors.
    if matches!(&arg.kind, TypedExprKind::Constant(Value::Float64(_))) {
        return "numeric".to_string();
    }

    pg_type_name_from_data_type(&arg.data_type)
}

impl Executor {
    fn pre_materialize_async_exprs_impl<'a>(
        &'a self,
        expr: &'a TypedExpr,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        skip_root_check_once: bool,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        Box::pin(async move {
            let mut transform = PreMaterializeTransform {
                executor: self,
                txn,
                db_id,
                sequence_values,
                search_path,
                ctes,
                skip_root_check_once,
            };
            transform.transform_expr(expr).await
        })
    }

    /// Pre-materialize all uncorrelated subqueries in a TypedExpr tree.
    ///
    /// Walks the tree and replaces:
    /// - `InSubquery { expr, subquery }` -> `InList { expr, list: [constants...] }`
    /// - `EXISTS { subquery }` -> `Constant(Bool(...))`
    /// - `ScalarSubquery(q)` -> `Constant(value)`
    /// - `AnyAll { expr, op, subquery }` -> expanded to `expr op ANY (ARRAY[...])`
    ///
    /// Only processes uncorrelated subqueries (no outer column references).
    /// Correlated subqueries are left as-is for per-row materialization.
    ///
    /// Uses [`AsyncExprTransform`] via [`PreMaterializeTransform`] for canonical
    /// child recursion. Custom arms handle 5 subquery variants + sequence functions;
    /// all other variants delegate to [`map_children_async`].
    pub(in crate::sql::executor::select::analyzed) fn pre_materialize_async_exprs<'a>(
        &'a self,
        expr: &'a TypedExpr,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        self.pre_materialize_async_exprs_impl(
            expr,
            txn,
            db_id,
            sequence_values,
            search_path,
            ctes,
            false,
        )
    }

    /// Pre-materialize expression when caller already checked
    /// [`needs_pre_materialization`] at the root.
    ///
    /// This keeps per-node short-circuiting during recursion, but avoids one
    /// duplicate full-tree classification at the entry point.
    pub(in crate::sql::executor::select::analyzed) fn pre_materialize_async_exprs_prechecked<'a>(
        &'a self,
        expr: &'a TypedExpr,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        self.pre_materialize_async_exprs_impl(
            expr,
            txn,
            db_id,
            sequence_values,
            search_path,
            ctes,
            true,
        )
    }
}

// ── PreMaterializeTransform ─────────────────────────────────
//
// Bundles mutable state for pre-materialization into an AsyncExprTransform.
// Custom arms handle 5 subquery variants + sequence FunctionCall;
// all other composite variants delegate to map_children_async.

pub(super) struct PreMaterializeTransform<'a> {
    executor: &'a Executor,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut SequenceSession,
    search_path: &'a [String],
    ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    skip_root_check_once: bool,
}

fn should_return_expr_unchanged(skip_root_check_once: &mut bool, expr: &TypedExpr) -> bool {
    if *skip_root_check_once {
        *skip_root_check_once = false;
        false
    } else {
        !needs_pre_materialization(expr)
    }
}

impl AsyncExprTransform for PreMaterializeTransform<'_> {
    fn transform_expr<'a>(
        &'a mut self,
        expr: &'a TypedExpr,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        Box::pin(async move {
            if should_return_expr_unchanged(&mut self.skip_root_check_once, expr) {
                return Ok(expr.clone());
            }
            match &expr.kind {
                TypedExprKind::InSubquery {
                    expr: inner_expr,
                    subquery,
                    negated,
                } => {
                    if is_correlated_query(subquery) {
                        return Ok(expr.clone());
                    }
                    let rewritten_expr = self.transform_expr(inner_expr).await?;
                    let result = self
                        .executor
                        .execute_subquery(
                            &mut *self.txn,
                            self.db_id,
                            &mut *self.sequence_values,
                            self.search_path,
                            subquery,
                            self.ctes,
                        )
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from IN subquery")),
                    };
                    let list: Vec<TypedExpr> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .map(|v| TypedExpr {
                            data_type: rewritten_expr.data_type.clone(),
                            kind: TypedExprKind::Constant(v),
                        })
                        .collect();
                    Ok(TypedExpr {
                        kind: TypedExprKind::InList {
                            expr: Box::new(rewritten_expr),
                            list,
                            negated: *negated,
                        },
                        data_type: DataType::Boolean,
                    })
                }

                TypedExprKind::TupleInSubquery {
                    exprs,
                    subquery,
                    negated,
                } => {
                    if is_correlated_query(subquery) {
                        return Ok(expr.clone());
                    }
                    // Rewrite each tuple element expression.
                    let mut rewritten_exprs = Vec::with_capacity(exprs.len());
                    for e in exprs {
                        rewritten_exprs.push(self.transform_expr(e).await?);
                    }
                    let result = self
                        .executor
                        .execute_subquery(
                            &mut *self.txn,
                            self.db_id,
                            &mut *self.sequence_values,
                            self.search_path,
                            subquery,
                            self.ctes,
                        )
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from tuple IN subquery")),
                    };
                    if rows.is_empty() {
                        // Empty set: IN → false, NOT IN → true
                        return Ok(TypedExpr {
                            kind: TypedExprKind::Constant(Value::Boolean(*negated)),
                            data_type: DataType::Boolean,
                        });
                    }
                    // Build: (e0 = r0[0] AND e1 = r0[1]) OR (e0 = r1[0] AND e1 = r1[1]) OR ...
                    // Uses regular = (not IS NOT DISTINCT FROM) to preserve PostgreSQL's
                    // three-valued NULL logic for IN/NOT IN.
                    let row_comparisons: Vec<TypedExpr> = rows
                        .into_iter()
                        .map(|r| {
                            let col_eqs: Vec<TypedExpr> = rewritten_exprs
                                .iter()
                                .enumerate()
                                .map(|(i, lhs)| {
                                    let rhs_val = r.values.get(i).cloned().unwrap_or(Value::Null);
                                    TypedExpr {
                                        kind: TypedExprKind::BinaryOp {
                                            left: Box::new(lhs.clone()),
                                            op: TypedBinaryOp::Eq,
                                            right: Box::new(TypedExpr::new(
                                                TypedExprKind::Constant(rhs_val),
                                                lhs.data_type.clone(),
                                            )),
                                        },
                                        data_type: DataType::Boolean,
                                    }
                                })
                                .collect();
                            // AND all column equalities together
                            col_eqs
                                .into_iter()
                                .reduce(|a, b| TypedExpr {
                                    kind: TypedExprKind::BinaryOp {
                                        left: Box::new(a),
                                        op: TypedBinaryOp::And,
                                        right: Box::new(b),
                                    },
                                    data_type: DataType::Boolean,
                                })
                                .unwrap()
                        })
                        .collect();
                    // OR all row comparisons together
                    let combined = row_comparisons
                        .into_iter()
                        .reduce(|a, b| TypedExpr {
                            kind: TypedExprKind::BinaryOp {
                                left: Box::new(a),
                                op: TypedBinaryOp::Or,
                                right: Box::new(b),
                            },
                            data_type: DataType::Boolean,
                        })
                        .unwrap();
                    if *negated {
                        Ok(TypedExpr {
                            kind: TypedExprKind::UnaryOp {
                                op: crate::sql::analyzer::types::UnaryOp::Not,
                                operand: Box::new(combined),
                            },
                            data_type: DataType::Boolean,
                        })
                    } else {
                        Ok(combined)
                    }
                }

                TypedExprKind::Exists { subquery, negated } => {
                    if is_correlated_query(subquery) {
                        return Ok(expr.clone());
                    }
                    let result = self
                        .executor
                        .execute_subquery(
                            &mut *self.txn,
                            self.db_id,
                            &mut *self.sequence_values,
                            self.search_path,
                            subquery,
                            self.ctes,
                        )
                        .await?;
                    let has_rows = match result {
                        ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                        _ => false,
                    };
                    let val = if *negated { !has_rows } else { has_rows };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Constant(Value::Boolean(val)),
                        data_type: DataType::Boolean,
                    })
                }

                TypedExprKind::ScalarSubquery(subquery) => {
                    if is_correlated_query(subquery) {
                        return Ok(expr.clone());
                    }
                    let result = self
                        .executor
                        .execute_subquery(
                            &mut *self.txn,
                            self.db_id,
                            &mut *self.sequence_values,
                            self.search_path,
                            subquery,
                            self.ctes,
                        )
                        .await?;
                    let value = match result {
                        ExecuteResult::Select { rows, .. } => {
                            if rows.is_empty() {
                                Value::Null
                            } else if rows.len() == 1 {
                                rows.into_iter()
                                    .next()
                                    .and_then(|r| r.values.into_iter().next())
                                    .unwrap_or(Value::Null)
                            } else {
                                return Err(anyhow!("Scalar subquery returned more than one row"));
                            }
                        }
                        _ => Value::Null,
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Constant(value),
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::AnyAll {
                    expr: inner_expr,
                    op,
                    subquery,
                    is_all,
                } => {
                    if is_correlated_query(subquery) {
                        return Ok(expr.clone());
                    }
                    let rewritten_expr = self.transform_expr(inner_expr).await?;
                    let result = self
                        .executor
                        .execute_subquery(
                            &mut *self.txn,
                            self.db_id,
                            &mut *self.sequence_values,
                            self.search_path,
                            subquery,
                            self.ctes,
                        )
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from ANY/ALL subquery")),
                    };
                    let rhs_declared_type = subquery
                        .output_schema
                        .first()
                        .map(|(_, dt, _)| dt.clone())
                        .ok_or_else(|| anyhow!("ANY/ALL subquery has empty output schema"))?;
                    let values: Vec<TypedExpr> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .map(|v| {
                            build_any_all_rhs_constant_expr(
                                &rewritten_expr.data_type,
                                &rhs_declared_type,
                                v,
                            )
                        })
                        .collect();
                    if values.is_empty() {
                        return Ok(TypedExpr {
                            kind: TypedExprKind::Constant(Value::Boolean(*is_all)),
                            data_type: DataType::Boolean,
                        });
                    }
                    let comparisons: Vec<TypedExpr> = values
                        .into_iter()
                        .map(|v| TypedExpr {
                            kind: TypedExprKind::BinaryOp {
                                left: Box::new(rewritten_expr.clone()),
                                op: op.clone(),
                                right: Box::new(v),
                            },
                            data_type: DataType::Boolean,
                        })
                        .collect();
                    let chain_op = if *is_all {
                        TypedBinaryOp::And
                    } else {
                        TypedBinaryOp::Or
                    };
                    let combined = comparisons
                        .into_iter()
                        .reduce(|a, b| TypedExpr {
                            kind: TypedExprKind::BinaryOp {
                                left: Box::new(a),
                                op: chain_op.clone(),
                                right: Box::new(b),
                            },
                            data_type: DataType::Boolean,
                        })
                        .unwrap();
                    Ok(combined)
                }

                TypedExprKind::ArraySubquery(subquery) => {
                    if is_correlated_query(subquery) {
                        return Ok(expr.clone());
                    }
                    let result = self
                        .executor
                        .execute_subquery(
                            &mut *self.txn,
                            self.db_id,
                            &mut *self.sequence_values,
                            self.search_path,
                            subquery,
                            self.ctes,
                        )
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from ARRAY(subquery)")),
                    };
                    let values: Vec<Value> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .collect();
                    Ok(TypedExpr {
                        kind: TypedExprKind::Constant(Value::Array(values)),
                        data_type: expr.data_type.clone(),
                    })
                }

                // Sequence functions: execute and replace with Constant.
                TypedExprKind::FunctionCall { func, args, .. } => {
                    let name_upper = func.name.to_ascii_uppercase();
                    let store = self.executor.store();
                    let qctx = crate::sql::query_context::QueryContext::from_task_locals();

                    match name_upper.as_str() {
                        "NEXTVAL" => {
                            let arg_val = eval_typed_expr(
                                args.first()
                                    .ok_or_else(|| anyhow!("nextval requires 1 argument"))?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let full_name = resolve_sequence_full_name_from_value(
                                &store,
                                &mut *self.txn,
                                self.db_id,
                                self.search_path,
                                arg_val,
                            )
                            .await?;
                            let val = store
                                .nextval_sequence(&mut *self.txn, self.db_id, &full_name)
                                .await?;
                            self.sequence_values.record_nextval(full_name, val);
                            Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            })
                        }
                        "CURRVAL" => {
                            let arg_val = eval_typed_expr(
                                args.first()
                                    .ok_or_else(|| anyhow!("currval requires 1 argument"))?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let full_name = resolve_sequence_full_name_from_value(
                                &store,
                                &mut *self.txn,
                                self.db_id,
                                self.search_path,
                                arg_val,
                            )
                            .await?;
                            if store
                                .get_sequence(&mut *self.txn, self.db_id, &full_name)
                                .await?
                                .is_none()
                            {
                                return Err(SqlError::RelationNotFound(full_name.clone()).into());
                            }
                            let val = self.sequence_values.currval(&full_name)?;
                            Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            })
                        }
                        "SETVAL" => {
                            let arg0 = eval_typed_expr(
                                args.first().ok_or_else(|| {
                                    anyhow!("setval requires at least 2 arguments")
                                })?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let arg1 = eval_typed_expr(
                                args.get(1).ok_or_else(|| {
                                    anyhow!("setval requires at least 2 arguments")
                                })?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let full_name = resolve_sequence_full_name_from_value(
                                &store,
                                &mut *self.txn,
                                self.db_id,
                                self.search_path,
                                arg0,
                            )
                            .await?;
                            let value_i64 = match arg1 {
                                Value::Int32(n) => n as i64,
                                Value::Int64(n) => n,
                                Value::Float64(n) => n as i64,
                                Value::Text(s) => s.trim().parse::<i64>().map_err(|_| {
                                    anyhow!("setval: value must be integer, got {}", s)
                                })?,
                                other => {
                                    return Err(anyhow!(
                                        "setval: value must be integer, got {}",
                                        other
                                    ))
                                }
                            };
                            let is_called = if let Some(arg2) = args.get(2) {
                                match eval_typed_expr(arg2, &Row::new(vec![]), &qctx)? {
                                    Value::Boolean(b) => b,
                                    Value::Text(s) => matches!(
                                        s.to_lowercase().as_str(),
                                        "true" | "t" | "1" | "yes" | "y"
                                    ),
                                    other => {
                                        return Err(anyhow!(
                                            "setval: is_called must be boolean, got {}",
                                            other
                                        ))
                                    }
                                }
                            } else {
                                true
                            };
                            let res = store
                                .setval_sequence(
                                    &mut *self.txn,
                                    self.db_id,
                                    &full_name,
                                    value_i64,
                                    is_called,
                                )
                                .await?;
                            self.sequence_values
                                .record_setval(full_name, res, is_called);
                            Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(res)),
                                data_type: DataType::Int64,
                            })
                        }
                        "LASTVAL" => {
                            if !args.is_empty() {
                                let arg_types = args
                                    .iter()
                                    .map(pg_lastval_arg_type_name)
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                return Err(crate::sql::error::SqlError::FunctionNotFound(
                                    format!("lastval({})", arg_types),
                                )
                                .into());
                            }
                            let val = self.sequence_values.lastval()?;
                            Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            })
                        }
                        // Non-sequence function: canonical async child recursion.
                        _ => {
                            let kind = map_children_async(expr, self).await?;
                            Ok(TypedExpr {
                                kind,
                                data_type: expr.data_type.clone(),
                            })
                        }
                    }
                }

                // All other composite variants: canonical async child recursion.
                _ => {
                    let kind = map_children_async(expr, self).await?;
                    Ok(TypedExpr {
                        kind,
                        data_type: expr.data_type.clone(),
                    })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{AnalyzedQuery, AnalyzedQueryBody};

    fn const_bool(v: bool) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(v)),
            DataType::Boolean,
        )
    }

    fn scalar_subquery_expr() -> TypedExpr {
        let subquery = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Values(vec![vec![TypedExpr::new(
                TypedExprKind::Constant(Value::Int32(1)),
                DataType::Int32,
            )]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32, None)],
        };
        TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(subquery)),
            DataType::Int32,
        )
    }

    #[test]
    fn prechecked_root_guard_is_one_shot_for_non_async_expr() {
        let expr = const_bool(true);
        let mut skip_root_check_once = true;

        assert!(!should_return_expr_unchanged(
            &mut skip_root_check_once,
            &expr
        ));
        assert!(!skip_root_check_once);
        assert!(should_return_expr_unchanged(
            &mut skip_root_check_once,
            &expr
        ));
    }

    #[test]
    fn non_async_expr_short_circuits_when_not_prechecked() {
        let expr = const_bool(false);
        let mut skip_root_check_once = false;
        assert!(should_return_expr_unchanged(
            &mut skip_root_check_once,
            &expr
        ));
    }

    #[test]
    fn async_expr_never_short_circuits_after_root_skip_consumed() {
        let expr = scalar_subquery_expr();
        let mut skip_root_check_once = true;

        assert!(!should_return_expr_unchanged(
            &mut skip_root_check_once,
            &expr
        ));
        assert!(!skip_root_check_once);
        assert!(!should_return_expr_unchanged(
            &mut skip_root_check_once,
            &expr
        ));
    }

    #[test]
    fn async_expr_does_not_short_circuit_without_prechecked_flag() {
        let expr = scalar_subquery_expr();
        let mut skip_root_check_once = false;
        assert!(!should_return_expr_unchanged(
            &mut skip_root_check_once,
            &expr
        ));
    }
}
