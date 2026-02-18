//! Analyzed SELECT execution path — the primary SELECT executor.
//!
//! All SELECT queries go through the Analyzer, producing `AnalyzedQuery` with
//! fully typed expressions. Async operations (subqueries, sequences) are
//! pre-materialized before building the operator tree.
//!
//! Handles FOR UPDATE/SHARE row locking and SELECT INTO natively.

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind,
    BinaryOp as TypedBinaryOp, JoinCondition, TypedExpr, TypedExprKind, TypedFunctionArg,
    TypedOrderByExpr,
};
use crate::sql::analyzer::{AnalyzedQuery, Analyzer};
use crate::sql::executor::core::catalog_prefetch::build_catalog_snapshot;
use crate::sql::executor::core::view_rewrite::expand_views_in_query;
use crate::sql::executor::core::Executor;
use crate::sql::expr::typed_eval::{eval_const_usize, eval_typed_expr};
use crate::sql::sequences::resolve_sequence_full_name_from_value;
use crate::sql::ExecuteResult;
use crate::types::{DataType, Row, TableSchema, Value};

use crate::sql::error::SqlError;
use crate::sql::optimizer::{BuildContext, PlanningContext};
use anyhow::{anyhow, Result};
use sqlparser::ast::{FunctionArg, FunctionArgExpr, ObjectName, Query, SetExpr};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

mod expr_runtime;
mod materialize;
mod subquery;

use expr_runtime::*;
use subquery::*;

impl Executor {
    /// Execute a query through the Analyzer path.
    ///
    /// This is the single execution path for all SELECT queries. The Analyzer
    /// produces fully typed expressions that are compiled into an operator tree.
    pub(crate) async fn try_execute_analyzed(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        // Pre-analysis rewrite: expand views into derived subqueries.
        //
        // Ensures the Analyzer and planner see a single query tree, and the
        // executor never needs runtime view expansion in table loading.
        let expanded_query =
            expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query).await?;

        // Build CatalogSnapshot (async: fetches table schemas from store).
        let catalog = build_catalog_snapshot(
            self.store().as_ref(),
            txn,
            db_id,
            search_path,
            self.tenant_keyspace(),
            &expanded_query,
            ctes,
        )
        .await?;

        // ── SELECT privilege check ──────────────────────────────────
        // Check that the current role has SELECT privilege on every base
        // table referenced in this query.  Virtual catalog tables
        // (information_schema, pg_catalog) are exempt.
        if current_role.is_some() {
            for table_name in catalog.base_table_full_names() {
                self.require_table_privilege(
                    txn,
                    current_role,
                    crate::auth::Privilege::Select,
                    table_name,
                )
                .await?;
            }
        }

        // Run the Analyzer (sync: name resolution + type checking).
        let mut analyzer = Analyzer::new(&catalog);
        let analyzed = analyzer
            .analyze_query(&expanded_query)
            .map_err(SqlError::from)?;

        // Post-analysis rewrite: flatten simple view subqueries back to
        // direct table references so the optimizer and planner can use
        // index-aware scan strategies.
        let analyzed = crate::sql::rewriter::rewrite_query(analyzed);

        // ── Pre-materialize async expressions ──────────────────────
        // Resolve non-correlated subqueries → constants BEFORE the optimizer
        // eligibility check. After this, most async expressions become literals
        // and the query becomes optimizer-eligible.
        let mut analyzed = analyzed;
        self.pre_materialize_query_body(
            &mut analyzed,
            txn,
            db_id,
            sequence_values,
            search_path,
            ctes,
        )
        .await?;

        // ── Single execution path: CBO optimizer pipeline ──────────
        let result = self
            .execute_via_optimizer(
                txn,
                db_id,
                sequence_values,
                search_path,
                &analyzed,
                ctes,
                &expanded_query.locks,
            )
            .await?;

        // SELECT INTO post-processing: create table from result.
        if let SetExpr::Select(select) = &*expanded_query.body {
            if let Some(ref into) = select.into {
                return self
                    .create_table_from_result(txn, db_id, search_path, &into.name, result)
                    .await;
            }
        }

        Ok(result)
    }

    /// Execute a subquery (no locks) through the optimizer pipeline.
    ///
    /// Used for recursive execution: subqueries in FROM, correlated subqueries,
    /// set-operation branches, INSERT...SELECT, etc.
    pub(crate) async fn execute_subquery(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        self.execute_via_optimizer(
            txn,
            db_id,
            sequence_values,
            search_path,
            analyzed,
            ctes,
            &[],
        )
        .await
    }

    // ── Subquery pre-materialization ────────────────────────────

    /// Pre-materialize all uncorrelated subqueries in a TypedExpr tree.
    ///
    /// Walks the tree and replaces:
    /// - `InSubquery { expr, subquery }` → `InList { expr, list: [constants...] }`
    /// - `EXISTS { subquery }` → `Constant(Bool(...))`
    /// - `ScalarSubquery(q)` → `Constant(value)`
    /// - `AnyAll { expr, op, subquery }` → expanded to `expr op ANY (ARRAY[...])`
    ///
    /// Only processes uncorrelated subqueries (no outer column references).
    /// Correlated subqueries are left as-is for per-row materialization.
    fn pre_materialize_async_exprs<'a>(
        &'a self,
        expr: &'a TypedExpr,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        Box::pin(async move {
            match &expr.kind {
                TypedExprKind::InSubquery {
                    expr: inner_expr,
                    subquery,
                    negated,
                } => {
                    if is_correlated_query(subquery) {
                        // Leave correlated IN-subquery as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Recurse into the LHS expression first.
                    let rewritten_expr = self
                        .pre_materialize_async_exprs(
                            inner_expr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;

                    // Execute the subquery to get result rows.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from IN subquery")),
                    };

                    // Extract first column of each row as constant values.
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

                TypedExprKind::Exists { subquery, negated } => {
                    if is_correlated_query(subquery) {
                        // Leave correlated EXISTS as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Execute the subquery — we only need to know if it returns any rows.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
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
                        // Leave correlated subqueries as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Execute the subquery — expect 0 or 1 rows, first column.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
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
                        // Leave correlated ANY/ALL as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Recurse into the LHS expression.
                    let rewritten_expr = self
                        .pre_materialize_async_exprs(
                            inner_expr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;

                    // Execute subquery → get values → build array.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from ANY/ALL subquery")),
                    };

                    let values: Vec<TypedExpr> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .map(|v| TypedExpr {
                            data_type: rewritten_expr.data_type.clone(),
                            kind: TypedExprKind::Constant(v),
                        })
                        .collect();

                    // ANY: true if `expr op value` for ANY value in the list.
                    // ALL: true if `expr op value` for ALL values in the list.
                    // For empty list: ANY → false, ALL → true.
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

                    // ANY → OR chain, ALL → AND chain.
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
                        .unwrap(); // values is non-empty, so this is safe.

                    Ok(combined)
                }

                TypedExprKind::ArraySubquery(subquery) => {
                    if is_correlated_query(subquery) {
                        // Leave correlated ArraySubquery as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Execute subquery → collect first column values into array.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
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

                // Recurse into children for non-subquery nodes.
                TypedExprKind::BinaryOp { left, right, op } => {
                    let l = self
                        .pre_materialize_async_exprs(
                            left,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let r = self
                        .pre_materialize_async_exprs(
                            right,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: op.clone(),
                            right: Box::new(r),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::UnaryOp { op, operand } => {
                    let inner = self
                        .pre_materialize_async_exprs(
                            operand,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::UnaryOp {
                            op: *op,
                            operand: Box::new(inner),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Cast {
                    expr: inner,
                    target_type,
                    cast_context,
                } => {
                    let rewritten = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::Cast {
                            expr: Box::new(rewritten),
                            target_type: target_type.clone(),
                            cast_context: cast_context.clone(),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::IsTest {
                    expr: inner,
                    test,
                    negated,
                } => {
                    let rewritten = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::IsTest {
                            expr: Box::new(rewritten),
                            test: *test,
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Between {
                    expr: inner,
                    low,
                    high,
                    negated,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let l = self
                        .pre_materialize_async_exprs(
                            low,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let h = self
                        .pre_materialize_async_exprs(
                            high,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::Between {
                            expr: Box::new(e),
                            low: Box::new(l),
                            high: Box::new(h),
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::InList {
                    expr: inner,
                    list,
                    negated,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let mut new_list = Vec::with_capacity(list.len());
                    for item in list {
                        new_list.push(
                            self.pre_materialize_async_exprs(
                                item,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: TypedExprKind::InList {
                            expr: Box::new(e),
                            list: new_list,
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Case {
                    operand,
                    when_clauses,
                    else_result,
                } => {
                    let new_operand = if let Some(ref op) = operand {
                        Some(Box::new(
                            self.pre_materialize_async_exprs(
                                op,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    let mut new_whens = Vec::with_capacity(when_clauses.len());
                    for (w, t) in when_clauses {
                        let nw = self
                            .pre_materialize_async_exprs(
                                w,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                        let nt = self
                            .pre_materialize_async_exprs(
                                t,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                        new_whens.push((nw, nt));
                    }
                    let new_else = if let Some(ref e) = else_result {
                        Some(Box::new(
                            self.pre_materialize_async_exprs(
                                e,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Case {
                            operand: new_operand,
                            when_clauses: new_whens,
                            else_result: new_else,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Coalesce(args) => {
                    let mut new_args = Vec::with_capacity(args.len());
                    for a in args {
                        new_args.push(
                            self.pre_materialize_async_exprs(
                                a,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: TypedExprKind::Coalesce(new_args),
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::NullIf(a, b) => {
                    let na = self
                        .pre_materialize_async_exprs(
                            a,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let nb = self
                        .pre_materialize_async_exprs(
                            b,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::NullIf(Box::new(na), Box::new(nb)),
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::FunctionCall {
                    func,
                    args,
                    order_by,
                    filter,
                } => {
                    let name_upper = func.name.to_ascii_uppercase();
                    let store = self.store();
                    let qctx = crate::sql::query_context::QueryContext::from_task_locals();

                    // Sequence functions: execute and replace with Constant.
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
                                txn,
                                db_id,
                                search_path,
                                arg_val,
                            )
                            .await?;
                            let val = store.nextval_sequence(txn, db_id, &full_name).await?;
                            sequence_values.insert(full_name, val);
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            });
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
                                txn,
                                db_id,
                                search_path,
                                arg_val,
                            )
                            .await?;
                            if store.get_sequence(txn, db_id, &full_name).await?.is_none() {
                                return Err(anyhow!("Sequence '{}' does not exist", full_name));
                            }
                            let val = sequence_values.get(&full_name).copied().ok_or_else(|| {
                            anyhow!("currval of sequence \"{}\" is not yet defined in this session", full_name)
                        })?;
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            });
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
                                txn,
                                db_id,
                                search_path,
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
                                .setval_sequence(txn, db_id, &full_name, value_i64, is_called)
                                .await?;
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(res)),
                                data_type: DataType::Int64,
                            });
                        }
                        "LASTVAL" => {
                            // LASTVAL returns the value most recently obtained by nextval.
                            let val =
                                sequence_values.values().last().copied().ok_or_else(|| {
                                    anyhow!("lastval is not yet defined in this session")
                                })?;
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            });
                        }
                        _ => {}
                    }

                    // Non-sequence function: recurse into args/filter.
                    let mut new_args = Vec::with_capacity(args.len());
                    for a in args {
                        new_args.push(
                            self.pre_materialize_async_exprs(
                                a,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        );
                    }
                    let new_filter = if let Some(ref f) = filter {
                        Some(Box::new(
                            self.pre_materialize_async_exprs(
                                f,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::FunctionCall {
                            func: func.clone(),
                            args: new_args,
                            order_by: order_by.clone(),
                            filter: new_filter,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Like {
                    expr: inner,
                    pattern,
                    escape,
                    negated,
                    case_insensitive,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let p = self
                        .pre_materialize_async_exprs(
                            pattern,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::Like {
                            expr: Box::new(e),
                            pattern: Box::new(p),
                            escape: escape.clone(),
                            negated: *negated,
                            case_insensitive: *case_insensitive,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::SimilarTo {
                    expr: inner,
                    pattern,
                    escape,
                    negated,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let p = self
                        .pre_materialize_async_exprs(
                            pattern,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::SimilarTo {
                            expr: Box::new(e),
                            pattern: Box::new(p),
                            escape: escape.clone(),
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                // Leaf nodes (Constant, ColumnRef, etc.) — return as-is.
                _ => Ok(expr.clone()),
            }
        }) // end Box::pin
    }

    /// Execute a query through the CBO optimizer pipeline.
    ///
    /// This is the **single execution path** for ALL SELECT queries:
    /// `AnalyzedQuery → pre-materialize → optimize → build → execute → post-process`
    ///
    /// Handles all query shapes: single-table, multi-table joins, set operations,
    /// CTEs, VALUES, tableless SELECT, table functions, virtual catalog tables,
    /// subqueries, correlated subqueries, catalog-dependent functions,
    /// FOR UPDATE/SHARE row locking, and DISTINCT/DISTINCT ON.
    async fn execute_via_optimizer(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        locks: &[sqlparser::ast::LockClause],
    ) -> Result<ExecuteResult> {
        use crate::sql::expr::classify::needs_async;
        use crate::sql::optimizer::{BuildContext, PlanningContext};

        tracing::debug!(target: "optimizer", "routing query through CBO pipeline");

        let rt = ExprRuntime::new(self, db_id, search_path, ctes);

        // ── Step 1: Pre-materialize non-correlated async expressions ──
        // Resolves IN subquery → InList, EXISTS → bool, ScalarSubquery → constant,
        // ANY/ALL → expanded comparisons, ArraySubquery → array literal.
        // Correlated subqueries (scope_depth > 0) are left as-is.
        let mut analyzed = analyzed.clone();
        self.pre_materialize_query_body(
            &mut analyzed,
            txn,
            db_id,
            sequence_values,
            search_path,
            ctes,
        )
        .await?;

        // ── Step 2: Determine post-processing needs ──
        let has_async_where = match &analyzed.body {
            AnalyzedQueryBody::Select(s) => s.where_clause.as_ref().is_some_and(|w| needs_async(w)),
            _ => false,
        };
        let mut has_async_projection = match &analyzed.body {
            AnalyzedQueryBody::Select(s) => s.projection.iter().any(|p| needs_async(&p.expr)),
            _ => false,
        };
        let has_async_order_by = analyzed.order_by.iter().any(|o| needs_async(&o.expr));
        let has_locks = !locks.is_empty();

        // ── Step 2a: GROUP BY + async projection special handling ──
        // Passthrough mode (replacing all projection with source columns) is
        // incompatible with GROUP BY because the Aggregate operator changes the
        // row layout. Instead, replace each async projection expression with its
        // primary dependency (the ColumnRef argument), and defer the async
        // function evaluation to post-processing.
        let has_group_by = match &analyzed.body {
            AnalyzedQueryBody::Select(s) => !s.group_by.is_empty(),
            _ => false,
        };
        // (output_col_index, deferred_async_expr_with_output_col_ref)
        let mut deferred_async_cols: Vec<(usize, TypedExpr)> = Vec::new();
        if has_group_by && has_async_projection {
            if let AnalyzedQueryBody::Select(ref mut select) = analyzed.body {
                for (i, proj) in select.projection.iter_mut().enumerate() {
                    if needs_async(&proj.expr) {
                        // Build a deferred expression that references output col `i`
                        // (the dependency value will be at this position after the
                        // optimizer evaluates the replacement expression).
                        let deferred = build_deferred_async_expr(&proj.expr, i);
                        deferred_async_cols.push((i, deferred));

                        // Replace the async expression with its primary dependency
                        // (the first ColumnRef argument). This lets the GROUP BY
                        // Aggregate operator include the dependency as a group key
                        // reference, producing the correct value in the output.
                        if let Some(dep) = extract_async_dependency(&proj.expr) {
                            proj.expr = dep;
                        } else {
                            // Fallback: NULL constant (value will be replaced in
                            // post-processing, but aggregate rewrite must still work).
                            proj.expr = TypedExpr {
                                kind: TypedExprKind::Constant(crate::types::Value::Null),
                                data_type: proj.expr.data_type.clone(),
                            };
                        }
                    }
                }
            }
            // Passthrough is no longer needed for async projection — we handled it.
            has_async_projection = false;
        }

        let needs_passthrough = has_async_projection || has_locks || has_async_where;

        // Save the final output schema before any modifications.
        let final_output_schema = analyzed.output_schema.clone();

        // ── Step 3: Prepare passthrough mode (strip async parts) ──
        // When projection has async expressions or locks need raw rows,
        // replace projection with passthrough (all source columns) and
        // handle the real projection in post-processing.
        let mut original_proj_exprs: Option<Vec<TypedExpr>> = None;
        let mut base_schema: Option<TableSchema> = None;
        let mut async_where_pred: Option<TypedExpr> = None;
        let mut deferred_order_by: Option<Vec<TypedOrderByExpr>> = None;
        let mut deferred_limit: Option<(Option<TypedExpr>, Option<TypedExpr>)> = None;

        if needs_passthrough {
            if let AnalyzedQueryBody::Select(ref mut select) = analyzed.body {
                // Save original projection.
                original_proj_exprs =
                    Some(select.projection.iter().map(|p| p.expr.clone()).collect());

                // Build base schema from source columns.
                let source_cols = collect_source_columns(select);
                base_schema = Some(build_schema_from_columns("__base", &source_cols));

                // Replace projection with passthrough.
                select.projection = create_passthrough_projection(&source_cols);
                analyzed.output_schema = source_cols;
            }

            // Defer LIMIT/OFFSET for locking (scan all, lock, then paginate).
            let limit = analyzed.limit.take();
            let offset = analyzed.offset.take();
            if limit.is_some() || offset.is_some() {
                deferred_limit = Some((limit, offset));
            }
        }

        if has_async_where {
            if let AnalyzedQueryBody::Select(ref mut select) = analyzed.body {
                if let Some(w) = select.where_clause.take() {
                    let (sync_part, async_part) = split_where_for_async(&w);
                    select.where_clause = sync_part;
                    async_where_pred = async_part;
                }
            }
        }

        // JOIN ON predicates (including correlated subqueries / catalog-dependent
        // functions) are evaluated inside join operators so outer join
        // null-extension semantics remain join-local and deterministic.

        if has_async_order_by {
            deferred_order_by = Some(analyzed.order_by.clone());
            analyzed.order_by.clear();
            // Also defer LIMIT/OFFSET (ORDER BY must happen before LIMIT).
            if deferred_limit.is_none() {
                let limit = analyzed.limit.take();
                let offset = analyzed.offset.take();
                if limit.is_some() || offset.is_some() {
                    deferred_limit = Some((limit, offset));
                }
            }
        }

        // ── Step 4: Pre-load table schemas, stats, virtual table data ──
        let mut planning_ctx = PlanningContext::empty();
        let mut build_ctx = BuildContext::new();
        self.prepare_optimizer_contexts(
            txn,
            db_id,
            sequence_values,
            search_path,
            &analyzed,
            ctes,
            &mut planning_ctx,
            &mut build_ctx,
        )
        .await?;

        // ── Step 5: AnalyzedQuery → PhysicalPlan ──
        let physical = crate::sql::optimizer::optimize(&analyzed, &planning_ctx)?;

        // ── Step 6: PhysicalPlan → BoxedOperator ──
        let mut operator = physical.build_operators(&build_ctx)?;

        // ── Step 7: Execute operator tree ──
        let mut rows = rt
            .run_operator_tree(&mut operator, txn, sequence_values)
            .await?;

        // ── Step 8: Post-processing ──

        // 8a: Async WHERE filter (correlated subqueries, catalog functions).
        if let Some(ref async_pred) = async_where_pred {
            let schema = base_schema
                .as_ref()
                .cloned()
                .unwrap_or_else(|| build_output_schema(&analyzed));
            rows = rt
                .filter_async(rows, Some(async_pred), &schema, txn, sequence_values)
                .await?;
        }

        // 8b: FOR UPDATE/SHARE locking (before projection, raw rows have PK).
        if has_locks {
            rows = self
                .apply_row_locks(
                    rows,
                    locks,
                    &analyzed,
                    &build_ctx,
                    txn,
                    db_id,
                    &deferred_limit,
                )
                .await?;
        }

        // 8c: Apply original projection (async expressions + all expressions when passthrough).
        if let Some(ref proj_exprs) = original_proj_exprs {
            let schema = base_schema
                .as_ref()
                .cloned()
                .unwrap_or_else(|| build_output_schema(&analyzed));
            rows = rt
                .project_rows(rows, proj_exprs, &schema, txn, sequence_values)
                .await?;
        }

        // 8c2: Deferred async projection for GROUP BY queries.
        // Each deferred entry has (output_col_idx, async_expr_with_output_col_ref).
        // The output row already contains the dependency value at col_idx;
        // we evaluate the async function with that value and replace the column.
        if !deferred_async_cols.is_empty() {
            let schema = build_output_schema(&analyzed);
            let qctx = crate::sql::query_context::QueryContext::from_task_locals();
            for row in &mut rows {
                for (col_idx, async_expr) in &deferred_async_cols {
                    let materialized = self
                        .materialize_expr_for_row(
                            async_expr,
                            row,
                            Some(&schema),
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                            &qctx,
                        )
                        .await?;
                    // materialize_expr_for_row returns a TypedExpr with the async
                    // function resolved to a Constant. Evaluate to get the value.
                    let val =
                        crate::sql::expr::typed_eval::eval_typed_expr(&materialized, row, &qctx)?;
                    if *col_idx < row.values.len() {
                        row.values[*col_idx] = val;
                    }
                }
            }
        }

        // 8d: Deferred ORDER BY + LIMIT/OFFSET.
        if let Some(ref deferred_ob) = deferred_order_by {
            let limit = deferred_limit
                .as_ref()
                .and_then(|(l, _)| l.as_ref())
                .map(|expr| eval_const_usize(expr, true))
                .transpose()?;
            let offset = deferred_limit
                .as_ref()
                .and_then(|(_, o)| o.as_ref())
                .map(|expr| eval_const_usize(expr, true))
                .transpose()?
                .unwrap_or(0);
            rows = sort_projected_rows(rows, deferred_ob, &final_output_schema, limit, offset)?;
        } else if let Some((ref limit_expr, ref offset_expr)) = deferred_limit {
            // LIMIT/OFFSET deferred for locking but no deferred ORDER BY.
            let limit = limit_expr
                .as_ref()
                .map(|expr| eval_const_usize(expr, true))
                .transpose()?;
            let offset = offset_expr
                .as_ref()
                .map(|expr| eval_const_usize(expr, true))
                .transpose()?
                .unwrap_or(0);
            if limit.is_some() || offset > 0 {
                let start = offset.min(rows.len());
                let end = limit.map_or(rows.len(), |l| (start + l).min(rows.len()));
                rows = rows[start..end].to_vec();
            }
        }

        // ── Step 9: Build result ──
        let columns: Vec<String> = final_output_schema
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let column_types: Vec<DataType> = final_output_schema
            .iter()
            .map(|(_, dt)| dt.clone())
            .collect();

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    /// Pre-materialize all non-correlated async expressions in the query body.
    ///
    /// Walks all TypedExpr positions (projection, WHERE, HAVING, ORDER BY,
    /// GROUP BY, DISTINCT ON, JOIN ON) and replaces non-correlated subqueries
    /// with constants.
    fn pre_materialize_query_body<'a>(
        &'a self,
        analyzed: &'a mut AnalyzedQuery,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &mut analyzed.body {
                AnalyzedQueryBody::Select(ref mut select) => {
                    // Projection.
                    for proj in &mut select.projection {
                        proj.expr = self
                            .pre_materialize_async_exprs(
                                &proj.expr,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                    }
                    // WHERE.
                    if let Some(ref w) = select.where_clause {
                        let new_w = self
                            .pre_materialize_async_exprs(
                                w,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                        select.where_clause = Some(new_w);
                    }
                    // HAVING.
                    if let Some(ref h) = select.having {
                        let new_h = self
                            .pre_materialize_async_exprs(
                                h,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                        select.having = Some(new_h);
                    }
                    // GROUP BY.
                    let group_exprs: Vec<TypedExpr> = select.group_by.clone();
                    for (i, expr) in group_exprs.iter().enumerate() {
                        select.group_by[i] = self
                            .pre_materialize_async_exprs(
                                expr,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                    }
                    // DISTINCT ON.
                    if let AnalyzedDistinct::DistinctOn(ref on_exprs) = select.distinct {
                        let cloned: Vec<TypedExpr> = on_exprs.clone();
                        let mut new_on = Vec::with_capacity(cloned.len());
                        for expr in &cloned {
                            new_on.push(
                                self.pre_materialize_async_exprs(
                                    expr,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?,
                            );
                        }
                        select.distinct = AnalyzedDistinct::DistinctOn(new_on);
                    }
                    // JOIN ON conditions (recursive through table ref tree).
                    for tr in &mut select.from {
                        self.pre_materialize_table_ref_on(
                            tr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    }
                }
                AnalyzedQueryBody::SetOperation {
                    ref mut left,
                    ref mut right,
                    ..
                } => {
                    self.pre_materialize_query_body(
                        left,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                    self.pre_materialize_query_body(
                        right,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                }
                AnalyzedQueryBody::Values(ref mut rows) => {
                    for row in rows {
                        for expr in row {
                            *expr = self
                                .pre_materialize_async_exprs(
                                    expr,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                        }
                    }
                }
            }

            // ORDER BY.
            let order_by_clone: Vec<TypedOrderByExpr> = analyzed.order_by.clone();
            for (i, ob) in order_by_clone.iter().enumerate() {
                analyzed.order_by[i].expr = self
                    .pre_materialize_async_exprs(
                        &ob.expr,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
            }

            Ok(())
        })
    }

    /// Pre-materialize JOIN ON conditions in a table ref tree.
    fn pre_materialize_table_ref_on<'a>(
        &'a self,
        table_ref: &'a mut AnalyzedTableRef,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &mut table_ref.kind {
                AnalyzedTableRefKind::Table { .. } | AnalyzedTableRefKind::Function { .. } => {
                    Ok(())
                }
                AnalyzedTableRefKind::Subquery(_) => Ok(()),
                AnalyzedTableRefKind::Join {
                    left,
                    right,
                    condition,
                    ..
                } => {
                    self.pre_materialize_table_ref_on(
                        left,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                    self.pre_materialize_table_ref_on(
                        right,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                    let new_cond = match condition {
                        JoinCondition::On(ref expr) => {
                            let new_expr = self
                                .pre_materialize_async_exprs(
                                    expr,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                            Some(JoinCondition::On(new_expr))
                        }
                        _ => None,
                    };
                    if let Some(c) = new_cond {
                        *condition = c;
                    }
                    Ok(())
                }
            }
        })
    }

    /// Pre-load table schemas, statistics, virtual table data, and table function
    /// results into the PlanningContext and BuildContext.
    #[allow(clippy::too_many_arguments)]
    async fn prepare_optimizer_contexts(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        planning_ctx: &mut PlanningContext,
        build_ctx: &mut BuildContext,
    ) -> Result<()> {
        // Walk the query body to collect all table references.
        let table_refs = crate::sql::optimizer::collect_query_table_refs(analyzed);

        let mut stats_attempted = std::collections::HashSet::new();
        for (name, _schema, alias) in &table_refs {
            // Use scope-safe composite key: "table_name\0alias" to prevent
            // collisions when the same alias appears in different scopes
            // (e.g., outer FROM users AS t vs inner subquery FROM orders AS t).
            let ctx_key = crate::sql::optimizer::schema_map_key(name, *alias);
            let alias_display = alias.unwrap_or(name);
            let cte_key = name.to_lowercase();

            // CTE: use CTE schema + rows.
            if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
                build_ctx
                    .table_schemas
                    .insert(ctx_key.clone(), cte_schema.clone());
                build_ctx.preloaded_rows.insert(ctx_key, cte_rows.clone());
                continue;
            }

            // Try KV table (regular user table).
            if let Some(table_schema) = self.store().get_schema(txn, db_id, name).await? {
                // Load statistics for cost-based optimization.
                let tid = table_schema.table_id;
                let stats = if stats_attempted.insert(tid) {
                    self.get_or_load_stats(txn, db_id, tid).await?
                } else {
                    self.stats_cache().get_full_stats(db_id, tid)
                };
                if let Some(stats) = stats {
                    planning_ctx.table_stats.insert(ctx_key.clone(), stats);
                }
                planning_ctx
                    .table_schemas
                    .insert(ctx_key.clone(), table_schema.clone());

                let mut s = table_schema;
                let short = s.name.rsplit('.').next().unwrap_or(&s.name);
                if !short.eq_ignore_ascii_case(alias_display) {
                    s.from_alias = Some(alias_display.to_string());
                }
                build_ctx.table_schemas.insert(ctx_key, s);
                continue;
            }

            // Virtual catalog table (information_schema, pg_catalog, etc.).
            // Pre-load data — these tables don't live in KV storage.
            let (mut virt_schema, virt_rows) = self
                .get_table_data(txn, db_id, sequence_values, search_path, name, ctes)
                .await?;
            let short = virt_schema
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&virt_schema.name);
            if !short.eq_ignore_ascii_case(alias_display) {
                virt_schema.from_alias = Some(alias_display.to_string());
            }
            build_ctx.table_schemas.insert(ctx_key.clone(), virt_schema);
            build_ctx.preloaded_rows.insert(ctx_key, virt_rows);
        }

        // Also walk table functions in FROM and pre-execute them.
        self.preload_table_functions(
            txn,
            db_id,
            sequence_values,
            search_path,
            analyzed,
            ctes,
            build_ctx,
        )
        .await?;

        Ok(())
    }

    /// Pre-execute table functions referenced in FROM and store results in BuildContext.
    #[allow(clippy::too_many_arguments)]
    fn preload_table_functions<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        analyzed: &'a AnalyzedQuery,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        build_ctx: &'a mut BuildContext,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let body = &analyzed.body;
            match body {
                AnalyzedQueryBody::Select(select) => {
                    for tr in &select.from {
                        self.preload_table_function_refs(
                            tr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                            build_ctx,
                        )
                        .await?;
                    }
                }
                AnalyzedQueryBody::SetOperation { left, right, .. } => {
                    self.preload_table_functions(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        left,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                    self.preload_table_functions(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        right,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                }
                AnalyzedQueryBody::Values(_) => {}
            }
            Ok(())
        }) // end Box::pin
    }

    /// Pre-execute table function references in a table ref tree.
    #[allow(clippy::too_many_arguments)]
    fn preload_table_function_refs<'a>(
        &'a self,
        table_ref: &'a AnalyzedTableRef,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        build_ctx: &'a mut BuildContext,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &table_ref.kind {
                AnalyzedTableRefKind::Function {
                    func,
                    args,
                    output_columns,
                } => {
                    let key = table_ref.alias.as_deref().unwrap_or(&func.name).to_string();

                    // Build schema from analyzer-resolved output columns.
                    let schema = TableSchema {
                        name: key.clone(),
                        table_id: 0,
                        columns: output_columns
                            .iter()
                            .map(|(name, dt)| crate::types::ColumnDef {
                                name: name.clone(),
                                data_type: dt.clone(),
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                            })
                            .collect(),
                        version: 1,
                        pk_constraint_name: None,
                        pk_indices: vec![],
                        indexes: vec![],
                        check_constraints: vec![],
                        foreign_keys: vec![],
                        owner: String::new(),
                        from_alias: table_ref.alias.clone(),
                    };

                    // Evaluate typed args to Values, then bridge to FunctionArg.
                    let qc = crate::sql::query_context::QueryContext::from_task_locals();
                    let dummy_row = Row::new(vec![]);
                    let mut bridge_args: Vec<FunctionArg> = Vec::with_capacity(args.len());
                    for tfa in args {
                        let (name_opt, typed_expr) = match tfa {
                            TypedFunctionArg::Positional(e) => (None, e),
                            TypedFunctionArg::Named { name, expr } => (Some(name.clone()), expr),
                        };
                        let val = eval_typed_expr(typed_expr, &dummy_row, &qc)?;
                        let sql_expr = crate::sql::value_coercion::value_to_sql_expr(&val);
                        let fa = match name_opt {
                            None => FunctionArg::Unnamed(FunctionArgExpr::Expr(sql_expr)),
                            Some(n) => FunctionArg::Named {
                                name: sqlparser::ast::Ident::new(n),
                                arg: FunctionArgExpr::Expr(sql_expr),
                            },
                        };
                        bridge_args.push(fa);
                    }

                    let func_upper = func.name.to_uppercase();

                    let rows = if func_upper == "GENERATE_SERIES" {
                        let (_, rows) = self
                            .execute_generate_series(&bridge_args, &key, None, 0, None)
                            .await?;
                        rows
                    } else if func_upper == "_PGTIKV_SYS_RECORD_MIGRATION" {
                        let (_, rows) = self.execute_record_migration(txn, &bridge_args).await?;
                        rows
                    } else {
                        // Try extension table function, user table function, or scalar-in-FROM.
                        let obj_name =
                            ObjectName(vec![sqlparser::ast::Ident::new(func.name.clone())]);
                        if let Some(result) = self
                            .try_execute_extension_table_function(
                                txn,
                                db_id,
                                search_path,
                                &obj_name,
                                &bridge_args,
                                None,
                            )
                            .await?
                        {
                            match result {
                            crate::sql::executor::extensions::ExtensionTableFunctionResult::Batch(
                                _,
                                rows,
                            ) => rows,
                            crate::sql::executor::extensions::ExtensionTableFunctionResult::Streaming(
                                _,
                                mut op,
                            ) => {
                                let rt = ExprRuntime::new(self, db_id, search_path, ctes);
                                rt.run_operator_tree(&mut op, txn, sequence_values).await?
                            }
                        }
                        } else if let Some((_, rows)) = self
                            .try_execute_user_table_function(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &obj_name,
                                &bridge_args,
                                None,
                            )
                            .await?
                        {
                            rows
                        } else {
                            // Scalar-in-FROM: evaluate as function call.
                            let typed_args: Vec<TypedExpr> = args
                                .iter()
                                .map(|a| match a {
                                    TypedFunctionArg::Positional(e) => e.clone(),
                                    TypedFunctionArg::Named { expr, .. } => expr.clone(),
                                })
                                .collect();
                            let mut scalar_arg_values = Vec::with_capacity(typed_args.len());
                            for arg in &typed_args {
                                scalar_arg_values.push(eval_typed_expr(arg, &dummy_row, &qc)?);
                            }
                            if let Some(result) =
                                crate::sql::executor::execute_cron_scalar_function(
                                    &self.store(),
                                    txn,
                                    db_id,
                                    qc.current_user.as_ref(),
                                    qc.database_name.as_ref(),
                                    crate::extensions::context::is_superuser(),
                                    &func.name,
                                    &scalar_arg_values,
                                    self.tenant_keyspace(),
                                )
                                .await
                            {
                                vec![Row::new(vec![result?])]
                            } else {
                                let typed_expr = TypedExpr {
                                    kind: TypedExprKind::FunctionCall {
                                        func: func.clone(),
                                        args: typed_args,
                                        order_by: vec![],
                                        filter: None,
                                    },
                                    data_type: func.return_type.clone(),
                                };
                                let val = eval_typed_expr(&typed_expr, &dummy_row, &qc)?;
                                vec![Row::new(vec![val])]
                            }
                        }
                    };

                    build_ctx.table_schemas.insert(key.clone(), schema);
                    build_ctx.preloaded_rows.insert(key, rows);
                }
                AnalyzedTableRefKind::Join { left, right, .. } => {
                    self.preload_table_function_refs(
                        left,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                    self.preload_table_function_refs(
                        right,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                }
                _ => {}
            }
            Ok(())
        }) // end Box::pin
    }

    /// Apply row-level locks (FOR UPDATE/SHARE) to rows.
    #[allow(clippy::too_many_arguments)]
    async fn apply_row_locks(
        &self,
        rows: Vec<Row>,
        locks: &[sqlparser::ast::LockClause],
        analyzed: &AnalyzedQuery,
        build_ctx: &BuildContext,
        txn: &mut Transaction,
        db_id: u64,
        deferred_limit: &Option<(Option<TypedExpr>, Option<TypedExpr>)>,
    ) -> Result<Vec<Row>> {
        if locks.is_empty() {
            return Ok(rows);
        }

        // Extract lock properties.
        let has_skip_locked = locks
            .iter()
            .any(|l| l.lock_type == sqlparser::ast::LockType::Update)
            && locks.iter().any(|l| {
                l.nonblock
                    .as_ref()
                    .is_some_and(|nb| *nb == sqlparser::ast::NonBlock::SkipLocked)
            });
        let has_nowait = locks.iter().any(|l| {
            l.nonblock
                .as_ref()
                .is_some_and(|nb| *nb == sqlparser::ast::NonBlock::Nowait)
        });

        // Find the table name for locking. Use the first base table in FROM.
        let table_name = match &analyzed.body {
            AnalyzedQueryBody::Select(select) => {
                select.from.first().and_then(|tr| match &tr.kind {
                    AnalyzedTableRefKind::Table { name, .. } => Some(name.clone()),
                    _ => None,
                })
            }
            _ => None,
        };

        let Some(table_name) = table_name else {
            return Ok(rows);
        };

        // Check that the table has a primary key (required for locking).
        if let Some(schema) = build_ctx.table_schemas.get(&table_name) {
            if schema.pk_indices.is_empty() {
                return Err(anyhow!("FOR UPDATE/SHARE requires primary key"));
            }
        }

        if has_skip_locked {
            let limit = deferred_limit
                .as_ref()
                .and_then(|(l, _)| l.as_ref())
                .map(|expr| eval_const_usize(expr, true))
                .transpose()?;
            let offset = deferred_limit
                .as_ref()
                .and_then(|(_, o)| o.as_ref())
                .map(|expr| eval_const_usize(expr, true))
                .transpose()?
                .unwrap_or(0);
            let max_locks = limit.map(|l| offset + l);
            let locked_indices = self
                .store()
                .lock_rows_skip_locked(txn, db_id, &table_name, &rows, max_locks)
                .await?;
            let locked_rows: Vec<Row> = locked_indices.iter().map(|&i| rows[i].clone()).collect();
            // Apply offset + limit to the locked subset.
            let start = offset.min(locked_rows.len());
            let end = limit.map_or(locked_rows.len(), |l| (start + l).min(locked_rows.len()));
            Ok(locked_rows[start..end].to_vec())
        } else {
            // Apply deferred LIMIT/OFFSET before locking to avoid locking
            // more rows than needed. Without this, `FOR UPDATE LIMIT 1` would
            // lock ALL scanned rows, causing SKIP LOCKED in other sessions to
            // find no unlockable rows.
            let mut rows_to_lock = rows;
            if let Some((ref limit_expr, ref offset_expr)) = deferred_limit {
                let limit = limit_expr
                    .as_ref()
                    .map(|expr| eval_const_usize(expr, true))
                    .transpose()?;
                let offset = offset_expr
                    .as_ref()
                    .map(|expr| eval_const_usize(expr, true))
                    .transpose()?
                    .unwrap_or(0);
                if limit.is_some() || offset > 0 {
                    let start = offset.min(rows_to_lock.len());
                    let end =
                        limit.map_or(rows_to_lock.len(), |l| (start + l).min(rows_to_lock.len()));
                    rows_to_lock = rows_to_lock[start..end].to_vec();
                }
            }
            if has_nowait {
                self.store()
                    .lock_rows_nowait(txn, db_id, &table_name, &rows_to_lock)
                    .await?;
            } else {
                self.store()
                    .lock_rows(txn, db_id, &table_name, &rows_to_lock)
                    .await?;
            }
            Ok(rows_to_lock)
        }
    }
}

/// Extract the primary dependency (first ColumnRef argument) from an async
/// expression. Typically the async expression is a catalog-dependent function
/// call like `pg_get_indexdef(col_ref)` or `format_type(col_ref, const)`.
fn extract_async_dependency(expr: &TypedExpr) -> Option<TypedExpr> {
    match &expr.kind {
        TypedExprKind::FunctionCall { args, .. } => {
            // Return the first ColumnRef argument.
            for arg in args {
                if matches!(arg.kind, TypedExprKind::ColumnRef { .. }) {
                    return Some(arg.clone());
                }
            }
            // If no ColumnRef found, recurse into first arg.
            args.first().and_then(extract_async_dependency)
        }
        _ => None,
    }
}

/// Build a deferred async expression where ColumnRef arguments are remapped
/// to reference the output column at `output_col_idx`. This allows the
/// deferred expression to be evaluated against the output row (where the
/// dependency value sits at position `output_col_idx`).
fn build_deferred_async_expr(expr: &TypedExpr, output_col_idx: usize) -> TypedExpr {
    match &expr.kind {
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            let new_args: Vec<TypedExpr> = args
                .iter()
                .map(|arg| {
                    if matches!(arg.kind, TypedExprKind::ColumnRef { .. }) {
                        // Remap ColumnRef to output column index.
                        TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: output_col_idx,
                                column_name: match &arg.kind {
                                    TypedExprKind::ColumnRef { column_name, .. } => {
                                        column_name.clone()
                                    }
                                    _ => unreachable!(),
                                },
                            },
                            data_type: arg.data_type.clone(),
                        }
                    } else {
                        arg.clone()
                    }
                })
                .collect();
            TypedExpr {
                kind: TypedExprKind::FunctionCall {
                    func: func.clone(),
                    args: new_args,
                    order_by: order_by.clone(),
                    filter: filter.clone(),
                },
                data_type: expr.data_type.clone(),
            }
        }
        // For non-FunctionCall async expressions, return as-is (fallback).
        _ => expr.clone(),
    }
}

/// Collect all source columns from a SELECT's FROM clause table references.
///
/// Walks the FROM tree (handling joins, tables, functions) and collects
/// `(column_name, data_type)` pairs in the same order the analyzer assigns
/// column indices.
fn collect_source_columns(select: &AnalyzedSelect) -> Vec<(String, DataType)> {
    let mut cols = Vec::new();
    for tr in &select.from {
        collect_table_ref_columns(tr, &mut cols);
    }
    cols
}

fn collect_table_ref_columns(tr: &AnalyzedTableRef, out: &mut Vec<(String, DataType)>) {
    match &tr.kind {
        AnalyzedTableRefKind::Table { schema, .. } => {
            for (name, dt, _nullable) in &schema.columns {
                out.push((name.clone(), dt.clone()));
            }
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            collect_table_ref_columns(left, out);
            collect_table_ref_columns(right, out);
        }
        AnalyzedTableRefKind::Function { output_columns, .. } => {
            for (name, dt) in output_columns {
                out.push((name.clone(), dt.clone()));
            }
        }
        AnalyzedTableRefKind::Subquery(subquery) => {
            for (name, dt) in &subquery.output_schema {
                out.push((name.clone(), dt.clone()));
            }
        }
    }
}

/// Create a passthrough projection that emits all source columns as ColumnRef.
fn create_passthrough_projection(
    columns: &[(String, DataType)],
) -> Vec<crate::sql::analyzer::types::AnalyzedProjection> {
    columns
        .iter()
        .enumerate()
        .map(
            |(i, (name, dt))| crate::sql::analyzer::types::AnalyzedProjection {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: i,
                        column_name: name.clone(),
                    },
                    data_type: dt.clone(),
                },
                output_name: name.clone(),
            },
        )
        .collect()
}

/// Build a TableSchema from column name/type pairs.
fn build_schema_from_columns(name: &str, columns: &[(String, DataType)]) -> TableSchema {
    TableSchema::new(
        name.to_string(),
        0,
        columns
            .iter()
            .map(|(col_name, dt)| crate::types::ColumnDef {
                name: col_name.clone(),
                data_type: dt.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            })
            .collect(),
        vec![],
    )
}

/// Build a TableSchema from the analyzed query's output schema.
fn build_output_schema(analyzed: &AnalyzedQuery) -> TableSchema {
    build_schema_from_columns("__output", &analyzed.output_schema)
}

/// Sort projected rows by deferred ORDER BY expressions.
///
/// When ORDER BY references output aliases (Analyzer clones projection expressions),
/// the ORDER BY expressions match output columns by name. We find the output column
/// index for each ORDER BY key, then sort using those column values.
fn sort_projected_rows(
    mut rows: Vec<Row>,
    deferred_ob: &[TypedOrderByExpr],
    output_schema: &[(String, DataType)],
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<Row>> {
    // Map each ORDER BY expression to an output column index.
    // Strategy: for ColumnRef ORDER BY, use column_name to find the output column.
    // For complex expressions (ScalarSubquery cloned from alias), find by data_type match.
    let mut ob_col_indices: Vec<(usize, bool, bool)> = Vec::with_capacity(deferred_ob.len());
    for ob in deferred_ob {
        let idx = match &ob.expr.kind {
            TypedExprKind::ColumnRef { column_name, .. } => {
                // Match by column name against output schema.
                output_schema
                    .iter()
                    .position(|(name, _)| name.eq_ignore_ascii_case(column_name))
            }
            _ => {
                // For complex expressions (ScalarSubquery, etc.): the Analyzer cloned
                // this from some projection[i].expr where output_schema[i] has the alias.
                // Find by matching data_type + being the only expression of that type.
                let target_type = &ob.expr.data_type;
                let matches: Vec<usize> = output_schema
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, dt))| dt == target_type)
                    .map(|(i, _)| i)
                    .collect();
                if matches.len() == 1 {
                    Some(matches[0])
                } else {
                    // Fallback: first output column.
                    Some(0)
                }
            }
        };
        ob_col_indices.push((idx.unwrap_or(0), ob.asc, ob.nulls_first));
    }

    // Sort.
    use crate::sql::expr::operators::sort_by_fallible;
    sort_by_fallible(&mut rows, |a, b| {
        for &(col_idx, asc, nulls_first) in &ob_col_indices {
            let va = a.values.get(col_idx).unwrap_or(&Value::Null);
            let vb = b.values.get(col_idx).unwrap_or(&Value::Null);
            let ord = crate::sql::expr::compare_order_by_values(va, vb, asc, nulls_first)?;
            if ord != std::cmp::Ordering::Equal {
                return Ok(ord);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    })?;

    // Apply deferred LIMIT/OFFSET.
    let start = offset.min(rows.len());
    let end = limit.map_or(rows.len(), |l| (start + l).min(rows.len()));
    Ok(rows[start..end].to_vec())
}
