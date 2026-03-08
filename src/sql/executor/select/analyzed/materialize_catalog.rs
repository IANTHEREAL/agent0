//! Catalog-dependent expression materialization.
//!
//! Handles async resolution of catalog-dependent functions within typed
//! expressions: `pg_get_indexdef`, `pg_get_constraintdef`, `format_type`,
//! `pg_sleep`, user-defined functions, cron/bg_sql scalar functions, and
//! recursive traversal of composite expression nodes.

use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{FunctionKind, TypedExpr, TypedExprKind};
use crate::sql::error::SqlError;
use crate::sql::executor::core::Executor;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;

use crate::sql::sequences::{self, SequenceSession};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::Transaction;

fn advisory_lock_timeout(
    qctx_lock_timeout: Option<Duration>,
    settings_snapshot: Option<&HashMap<String, String>>,
) -> Option<Duration> {
    if qctx_lock_timeout.is_some() {
        return qctx_lock_timeout;
    }

    settings_snapshot
        .and_then(|s| s.get("lock_timeout"))
        .and_then(
            |v| match crate::sql::session::SessionSettings::parse_timeout_value(v) {
                Ok(ms) if ms > 0 => Some(Duration::from_millis(ms)),
                Ok(_) => None,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid lock_timeout in settings snapshot for advisory lock execution"
                    );
                    None
                }
            },
        )
}

fn is_pg_get_serial_sequence_function_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("PG_GET_SERIAL_SEQUENCE")
        || name.rsplit_once(".").is_some_and(|(schema, func)| {
            schema.eq_ignore_ascii_case("PG_CATALOG")
                && func.eq_ignore_ascii_case("PG_GET_SERIAL_SEQUENCE")
        })
}

fn non_pg_catalog_qualified_pg_get_serial_sequence_signature(
    name: &str,
    args: &[TypedExpr],
) -> Option<String> {
    let (schema, func) = name.rsplit_once(".")?;
    if schema.is_empty()
        || schema.eq_ignore_ascii_case("PG_CATALOG")
        || !func.eq_ignore_ascii_case("PG_GET_SERIAL_SEQUENCE")
    {
        return None;
    }
    let arg_types = args
        .iter()
        .map(pg_get_serial_sequence_arg_type_name)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{}.pg_get_serial_sequence({})",
        schema.to_lowercase(),
        arg_types
    ))
}

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

fn pg_get_serial_sequence_arg_type_name(arg: &TypedExpr) -> String {
    if matches!(
        &arg.kind,
        TypedExprKind::Constant(Value::Text(_)) | TypedExprKind::Constant(Value::Null)
    ) {
        return "unknown".to_string();
    }
    pg_type_name_from_data_type(&arg.data_type)
}

fn pg_get_serial_sequence_accepts_text_arg(arg: &TypedExpr) -> bool {
    matches!(
        arg.data_type,
        DataType::Text | DataType::Varchar(_) | DataType::Name
    ) || matches!(
        &arg.kind,
        TypedExprKind::Constant(Value::Text(_)) | TypedExprKind::Constant(Value::Null)
    )
}

impl Executor {
    pub(super) fn materialize_catalog_functions<'a>(
        &'a self,
        expr: &'a TypedExpr,
        row: &'a Row,
        schema: Option<&'a TableSchema>,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        qctx: &'a QueryContext,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        Box::pin(async move {
            match &expr.kind {
                TypedExprKind::FunctionCall {
                    func,
                    args,
                    order_by,
                    filter,
                } => {
                    // Recurse into args first (so we can eval constant args safely).
                    let mut new_args = Vec::with_capacity(args.len());
                    for a in args {
                        new_args.push(
                            self.materialize_catalog_functions(
                                a,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        );
                    }

                    let mut new_order_by = Vec::with_capacity(order_by.len());
                    for ob in order_by {
                        new_order_by.push(crate::sql::analyzer::types::TypedOrderByExpr {
                            expr: self
                                .materialize_catalog_functions(
                                    &ob.expr,
                                    row,
                                    schema,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    qctx,
                                )
                                .await?,
                            asc: ob.asc,
                            nulls_first: ob.nulls_first,
                        });
                    }
                    let new_filter = match filter {
                        Some(f) => Some(Box::new(
                            self.materialize_catalog_functions(
                                f,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        )),
                        None => None,
                    };

                    if func.name.eq_ignore_ascii_case("PG_GET_INDEXDEF") {
                        let val = self
                            .eval_pg_get_indexdef(&new_args, row, schema, txn, db_id, qctx)
                            .await?;
                        return Ok(TypedExpr::new(TypedExprKind::Constant(val), DataType::Text));
                    }

                    if func.name.eq_ignore_ascii_case("PG_SLEEP") {
                        let seconds = match new_args.first() {
                            None => 0.0_f64,
                            Some(arg) => match eval_typed_expr(arg, row, qctx)? {
                                Value::Null => 0.0_f64,
                                Value::Int32(v) => v as f64,
                                Value::Int64(v) => v as f64,
                                Value::Float64(v) => v,
                                Value::Numeric(v) => {
                                    let s = v.normalize().to_string();
                                    s.parse::<f64>().map_err(|_| {
                                        anyhow!("invalid argument for pg_sleep: {}", s)
                                    })?
                                }
                                Value::Text(s) => s
                                    .parse::<f64>()
                                    .map_err(|_| anyhow!("invalid argument for pg_sleep: {}", s))?,
                                other => {
                                    return Err(anyhow!(
                                        "pg_sleep requires a numeric argument, got: {:?}",
                                        other
                                    ));
                                }
                            },
                        };
                        if !seconds.is_finite() || seconds < 0.0 {
                            return Err(anyhow!(
                                "pg_sleep requires a non-negative finite duration, got: {}",
                                seconds
                            ));
                        }
                        tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
                        return Ok(TypedExpr::new(
                            // PostgreSQL renders `void` results as an empty field in psql.
                            // Use empty text here so unaligned output matches that behavior.
                            TypedExprKind::Constant(Value::Text(String::new())),
                            expr.data_type.clone(),
                        ));
                    }

                    if func.name.eq_ignore_ascii_case("PG_GET_CONSTRAINTDEF") {
                        // Row-level compatibility: return constraintdef if present.
                        if let Some((_, idx)) = find_text_column(schema, "constraintdef") {
                            if let Some(v) = row.values.get(idx) {
                                if !matches!(v, Value::Null) {
                                    return Ok(TypedExpr::new(
                                        TypedExprKind::Constant(v.clone()),
                                        DataType::Text,
                                    ));
                                }
                            }
                        }
                        return Ok(TypedExpr::new(
                            TypedExprKind::Constant(Value::Text(String::new())),
                            DataType::Text,
                        ));
                    }

                    if func.name.eq_ignore_ascii_case("FORMAT_TYPE") {
                        let val = self
                            .eval_format_type(&new_args, row, schema, txn, db_id, qctx)
                            .await?;
                        return Ok(TypedExpr::new(TypedExprKind::Constant(val), DataType::Text));
                    }

                    if func.name.eq_ignore_ascii_case("TO_REGTYPE") {
                        let val = self
                            .eval_to_regtype(&new_args, row, txn, db_id, search_path, qctx)
                            .await?;
                        return Ok(TypedExpr::new(
                            TypedExprKind::Constant(val),
                            expr.data_type.clone(),
                        ));
                    }

                    if let Some(signature) =
                        non_pg_catalog_qualified_pg_get_serial_sequence_signature(
                            &func.name, &new_args,
                        )
                    {
                        return Err(SqlError::FunctionNotFound(signature).into());
                    }

                    if is_pg_get_serial_sequence_function_name(&func.name) {
                        let val = self
                            .eval_pg_get_serial_sequence(
                                &new_args,
                                row,
                                txn,
                                db_id,
                                search_path,
                                qctx,
                            )
                            .await?;
                        return Ok(TypedExpr::new(
                            TypedExprKind::Constant(val),
                            expr.data_type.clone(),
                        ));
                    }

                    if crate::sql::executor::split_cron_scalar_function_name(&func.name).is_some() {
                        let mut arg_values = Vec::with_capacity(new_args.len());
                        let dummy_row = Row::new(vec![]);
                        for arg in &new_args {
                            arg_values.push(eval_typed_expr(arg, &dummy_row, qctx)?);
                        }
                        if let Some(result) = crate::sql::executor::execute_cron_scalar_function(
                            &self.store(),
                            txn,
                            db_id,
                            qctx.current_user.as_ref(),
                            qctx.database_name.as_ref(),
                            crate::extensions::context::is_superuser(),
                            &func.name,
                            &arg_values,
                            self.tenant_keyspace(),
                        )
                        .await
                        {
                            return Ok(TypedExpr::new(
                                TypedExprKind::Constant(result?),
                                expr.data_type.clone(),
                            ));
                        }
                    }

                    if crate::sql::executor::is_bg_sql_function(&func.name) {
                        let mut arg_values = Vec::with_capacity(new_args.len());
                        let dummy_row = Row::new(vec![]);
                        for arg in &new_args {
                            arg_values.push(eval_typed_expr(arg, &dummy_row, qctx)?);
                        }
                        if let Some(result) = crate::sql::executor::execute_bg_sql_function(
                            &self.store(),
                            txn,
                            db_id,
                            qctx.current_user.as_ref(),
                            &func.name,
                            &arg_values,
                            self.tenant_keyspace(),
                        )
                        .await
                        {
                            return Ok(TypedExpr::new(
                                TypedExprKind::Constant(result?),
                                expr.data_type.clone(),
                            ));
                        }
                    }

                    if crate::sql::advisory_locks::is_advisory_lock_function(&func.name) {
                        let mut arg_values = Vec::with_capacity(new_args.len());
                        for arg in &new_args {
                            arg_values.push(eval_typed_expr(arg, row, qctx)?);
                        }
                        let ks: Arc<str> = Arc::from(self.tenant_keyspace());
                        let lock_timeout = advisory_lock_timeout(
                            qctx.lock_timeout,
                            qctx.settings_snapshot.as_deref(),
                        );
                        if let Some(result) = crate::sql::executor::execute_advisory_lock_function(
                            &ks,
                            qctx.connection_id,
                            &func.name,
                            &arg_values,
                            lock_timeout,
                            qctx.xact_advisory_lock_used.clone(),
                            qctx.xact_advisory_savepoint_tracker.clone(),
                        )
                        .await
                        {
                            return Ok(TypedExpr::new(
                                TypedExprKind::Constant(result?),
                                expr.data_type.clone(),
                            ));
                        }
                    }

                    if matches!(func.kind, FunctionKind::UserDefined { .. }) {
                        let mut arg_values = Vec::with_capacity(new_args.len());
                        for arg in &new_args {
                            arg_values.push(eval_typed_expr(arg, row, qctx)?);
                        }
                        if let Some(result) = crate::sql::plpgsql::try_execute_user_function(
                            &self.store(),
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &func.name,
                            arg_values,
                            Some(self),
                        )
                        .await?
                        {
                            return Ok(TypedExpr::new(
                                TypedExprKind::Constant(result),
                                expr.data_type.clone(),
                            ));
                        }
                        return Err(anyhow!("Function '{}' does not exist", func.name));
                    }

                    Ok(TypedExpr {
                        kind: TypedExprKind::FunctionCall {
                            func: func.clone(),
                            args: new_args,
                            order_by: new_order_by,
                            filter: new_filter,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                // Subqueries should already be resolved by `pre_materialize_async_exprs`.
                TypedExprKind::ScalarSubquery(_)
                | TypedExprKind::ArraySubquery(_)
                | TypedExprKind::Exists { .. }
                | TypedExprKind::InSubquery { .. }
                | TypedExprKind::TupleInSubquery { .. }
                | TypedExprKind::AnyAll { .. } => Ok(expr.clone()),

                // Recurse through composite nodes.
                TypedExprKind::BinaryOp { left, right, op } => Ok(TypedExpr {
                    kind: TypedExprKind::BinaryOp {
                        left: Box::new(
                            self.materialize_catalog_functions(
                                left,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        op: op.clone(),
                        right: Box::new(
                            self.materialize_catalog_functions(
                                right,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::UnaryOp { op, operand } => Ok(TypedExpr {
                    kind: TypedExprKind::UnaryOp {
                        op: *op,
                        operand: Box::new(
                            self.materialize_catalog_functions(
                                operand,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::Cast {
                    expr: inner,
                    target_type,
                    cast_context,
                } => Ok(TypedExpr {
                    kind: TypedExprKind::Cast {
                        expr: Box::new(
                            self.materialize_catalog_functions(
                                inner,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        target_type: target_type.clone(),
                        cast_context: *cast_context,
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::IsTest {
                    expr: inner,
                    test,
                    negated,
                } => Ok(TypedExpr {
                    kind: TypedExprKind::IsTest {
                        expr: Box::new(
                            self.materialize_catalog_functions(
                                inner,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        test: *test,
                        negated: *negated,
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::Between {
                    expr: inner,
                    low,
                    high,
                    negated,
                } => Ok(TypedExpr {
                    kind: TypedExprKind::Between {
                        expr: Box::new(
                            self.materialize_catalog_functions(
                                inner,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        low: Box::new(
                            self.materialize_catalog_functions(
                                low,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        high: Box::new(
                            self.materialize_catalog_functions(
                                high,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        negated: *negated,
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::InList {
                    expr: inner,
                    list,
                    negated,
                } => {
                    let inner = self
                        .materialize_catalog_functions(
                            inner,
                            row,
                            schema,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            qctx,
                        )
                        .await?;
                    let mut new_list = Vec::with_capacity(list.len());
                    for item in list {
                        new_list.push(
                            self.materialize_catalog_functions(
                                item,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: TypedExprKind::InList {
                            expr: Box::new(inner),
                            list: new_list,
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }
                TypedExprKind::ScalarArrayCmp {
                    expr: inner,
                    elems,
                    op,
                    use_or,
                } => {
                    let inner = self
                        .materialize_catalog_functions(
                            inner,
                            row,
                            schema,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            qctx,
                        )
                        .await?;
                    let mut new_elems = Vec::with_capacity(elems.len());
                    for item in elems {
                        new_elems.push(
                            self.materialize_catalog_functions(
                                item,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: TypedExprKind::ScalarArrayCmp {
                            expr: Box::new(inner),
                            elems: new_elems,
                            op: op.clone(),
                            use_or: *use_or,
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
                } => Ok(TypedExpr {
                    kind: TypedExprKind::Like {
                        expr: Box::new(
                            self.materialize_catalog_functions(
                                inner,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        pattern: Box::new(
                            self.materialize_catalog_functions(
                                pattern,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        escape: match escape {
                            Some(e) => Some(Box::new(
                                self.materialize_catalog_functions(
                                    e,
                                    row,
                                    schema,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    qctx,
                                )
                                .await?,
                            )),
                            None => None,
                        },
                        negated: *negated,
                        case_insensitive: *case_insensitive,
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::SimilarTo {
                    expr: inner,
                    pattern,
                    escape,
                    negated,
                } => Ok(TypedExpr {
                    kind: TypedExprKind::SimilarTo {
                        expr: Box::new(
                            self.materialize_catalog_functions(
                                inner,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        pattern: Box::new(
                            self.materialize_catalog_functions(
                                pattern,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        escape: match escape {
                            Some(e) => Some(Box::new(
                                self.materialize_catalog_functions(
                                    e,
                                    row,
                                    schema,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    qctx,
                                )
                                .await?,
                            )),
                            None => None,
                        },
                        negated: *negated,
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::Case {
                    operand,
                    when_clauses,
                    else_result,
                } => {
                    let operand = match operand {
                        Some(e) => Some(Box::new(
                            self.materialize_catalog_functions(
                                e,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        )),
                        None => None,
                    };
                    let mut new_when = Vec::with_capacity(when_clauses.len());
                    for (w, t) in when_clauses {
                        new_when.push((
                            self.materialize_catalog_functions(
                                w,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                            self.materialize_catalog_functions(
                                t,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ));
                    }
                    let else_result = match else_result {
                        Some(e) => Some(Box::new(
                            self.materialize_catalog_functions(
                                e,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        )),
                        None => None,
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Case {
                            operand,
                            when_clauses: new_when,
                            else_result,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }
                TypedExprKind::Coalesce(args)
                | TypedExprKind::ArrayLiteral(args)
                | TypedExprKind::Row(args) => {
                    let mut new_args = Vec::with_capacity(args.len());
                    for a in args {
                        new_args.push(
                            self.materialize_catalog_functions(
                                a,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: match &expr.kind {
                            TypedExprKind::Coalesce(_) => TypedExprKind::Coalesce(new_args),
                            TypedExprKind::ArrayLiteral(_) => TypedExprKind::ArrayLiteral(new_args),
                            TypedExprKind::Row(_) => TypedExprKind::Row(new_args),
                            _ => unreachable!(),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }
                TypedExprKind::NullIf(a, b) => Ok(TypedExpr {
                    kind: TypedExprKind::NullIf(
                        Box::new(
                            self.materialize_catalog_functions(
                                a,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        Box::new(
                            self.materialize_catalog_functions(
                                b,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                    ),
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::JsonAccess {
                    expr: inner,
                    path,
                    operator,
                } => Ok(TypedExpr {
                    kind: TypedExprKind::JsonAccess {
                        expr: Box::new(
                            self.materialize_catalog_functions(
                                inner,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        path: Box::new(
                            self.materialize_catalog_functions(
                                path,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        operator: *operator,
                    },
                    data_type: expr.data_type.clone(),
                }),
                TypedExprKind::ArrayIndex { array, index } => Ok(TypedExpr {
                    kind: TypedExprKind::ArrayIndex {
                        array: Box::new(
                            self.materialize_catalog_functions(
                                array,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                        index: Box::new(
                            self.materialize_catalog_functions(
                                index,
                                row,
                                schema,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                qctx,
                            )
                            .await?,
                        ),
                    },
                    data_type: expr.data_type.clone(),
                }),

                // Leaf nodes.
                _ => Ok(expr.clone()),
            }
        })
    }

    async fn eval_pg_get_indexdef(
        &self,
        args: &[TypedExpr],
        row: &Row,
        schema: Option<&TableSchema>,
        txn: &mut Transaction,
        db_id: u64,
        qctx: &QueryContext,
    ) -> Result<Value> {
        let Some(arg0) = args.first() else {
            return Ok(Value::Text("CREATE INDEX".to_string()));
        };

        let oid_val = eval_typed_expr(arg0, row, qctx)?;

        // 3-arg form: pg_get_indexdef(index_oid, column_no, pretty)
        // Returns the column name or expression at the given 1-based ordinal.
        if args.len() >= 3 {
            let Some(col_no_arg) = args.get(1) else {
                return Err(anyhow!(
                    "pg_get_indexdef(oid, column_no, pretty) missing column_no argument"
                ));
            };
            let Some(pretty_arg) = args.get(2) else {
                return Err(anyhow!(
                    "pg_get_indexdef(oid, column_no, pretty) missing pretty argument"
                ));
            };
            let col_no_val = eval_typed_expr(col_no_arg, row, qctx)?;
            let pretty_val = eval_typed_expr(pretty_arg, row, qctx)?;

            // PostgreSQL edge case semantics for col_no:
            // - NULL col_no → return NULL
            // - col_no < 0 → return empty string ''
            // - col_no = 0 → return full index definition
            // - col_no > num_columns → return empty string ''
            // Function is strict in all 3 args: any NULL arg returns NULL.
            if matches!(oid_val, Value::Null)
                || matches!(col_no_val, Value::Null)
                || matches!(pretty_val, Value::Null)
            {
                return Ok(Value::Null);
            }

            let Some(oid) = value_to_i64(&oid_val) else {
                return Ok(Value::Null);
            };
            let col_no = value_to_i64_strict(&col_no_val, "column_no")?;
            let _pretty = value_to_bool_strict(&pretty_val, "pretty")?;

            if col_no < 0 {
                let store = self.store();
                let exists = index_oid_exists_by_oid(&store, txn, db_id, oid).await?;
                return if exists {
                    Ok(Value::Text("".to_string())) // existing index + col_no < 0 → empty string
                } else {
                    Ok(Value::Null) // unknown index OID → NULL
                };
            } else if col_no == 0 {
                // col_no=0 means return full definition (same as 1-arg form)
                let store = self.store();
                let indexdef = lookup_indexdef_by_oid(&store, txn, db_id, oid).await?;
                return Ok(indexdef.map(Value::Text).unwrap_or(Value::Null));
            } else {
                let store = self.store();
                let col_def =
                    lookup_index_column_by_oid(&store, txn, db_id, oid, col_no as usize).await?;

                match col_def {
                    Some(Some(def)) => return Ok(Value::Text(def)),
                    Some(None) => return Ok(Value::Text(String::new())), // out of range → empty string
                    None => return Ok(Value::Null),                      // unknown index OID
                }
            }
        }

        let Some(oid) = value_to_i64(&oid_val) else {
            return Ok(Value::Text("CREATE INDEX".to_string()));
        };

        // 1-arg form: prefer row-level indexdef if present and matches the OID.
        if let Some(schema) = schema {
            if let (Some(relid_idx), Some(def_idx)) = (
                schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case("indexrelid")),
                schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case("indexdef")),
            ) {
                if let Some(row_relid) = row.values.get(relid_idx) {
                    if value_to_i64(row_relid) == Some(oid) {
                        if let Some(row_def) = row.values.get(def_idx) {
                            if !matches!(row_def, Value::Null) {
                                return Ok(row_def.clone());
                            }
                        }
                    }
                }
            } else if let Some(def_idx) = schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case("indexdef"))
            {
                if let Some(row_def) = row.values.get(def_idx) {
                    if !matches!(row_def, Value::Null) {
                        return Ok(row_def.clone());
                    }
                }
            }
        }

        let store = self.store();
        let indexdef = lookup_indexdef_by_oid(&store, txn, db_id, oid).await?;
        Ok(Value::Text(
            indexdef.unwrap_or_else(|| "CREATE INDEX".to_string()),
        ))
    }

    async fn eval_format_type(
        &self,
        args: &[TypedExpr],
        row: &Row,
        schema: Option<&TableSchema>,
        txn: &mut Transaction,
        db_id: u64,
        qctx: &QueryContext,
    ) -> Result<Value> {
        let Some(arg0) = args.first() else {
            return Ok(Value::Null);
        };

        let oid_val = eval_typed_expr(arg0, row, qctx)?;
        let Some(oid) = value_to_i64(&oid_val) else {
            return Ok(Value::Text("text".to_string()));
        };

        // Read typmod from second argument (default -1 when absent)
        let typmod = args
            .get(1)
            .and_then(|a| eval_typed_expr(a, row, qctx).ok())
            .and_then(|v| value_to_i64(&v))
            .unwrap_or(-1);

        // Handle types that need typmod for canonical name (PG format_type compat)
        use crate::sql::pg_types;
        match oid {
            pg_types::OID_VARCHAR => {
                return if typmod > 0 {
                    Ok(Value::Text(format!("character varying({})", typmod - 4)))
                } else {
                    Ok(Value::Text("character varying".to_string()))
                };
            }
            pg_types::OID_BPCHAR => {
                return if typmod > 0 {
                    Ok(Value::Text(format!("character({})", typmod - 4)))
                } else {
                    Ok(Value::Text("character".to_string()))
                };
            }
            pg_types::OID_NUMERIC if typmod > 0 => {
                let precision = ((typmod - 4) >> 16) & 0xffff;
                let scale = (typmod - 4) & 0xffff;
                return Ok(Value::Text(format!("numeric({},{})", precision, scale)));
            }
            _ => {}
        }

        // Row-level compatibility: if typname column is visible, return it.
        if let Some(schema) = schema {
            if let Some(idx) = schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case("typname"))
            {
                if let Some(Value::Text(s)) = row.values.get(idx) {
                    return Ok(Value::Text(s.clone()));
                }
            }
        }

        let store = self.store();
        let typname = lookup_typname_by_oid(&store, txn, db_id, oid).await?;
        Ok(Value::Text(typname.unwrap_or_else(|| "text".to_string())))
    }

    async fn eval_to_regtype(
        &self,
        args: &[TypedExpr],
        row: &Row,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        qctx: &QueryContext,
    ) -> Result<Value> {
        let Some(arg0) = args.first() else {
            return Ok(Value::Null);
        };

        let raw = match eval_typed_expr(arg0, row, qctx)? {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s,
            _ => return Err(anyhow!("function to_regtype(text) does not exist")),
        };

        // Fast path for pg_catalog builtins handled by the scalar function
        // implementation.
        let resolved_builtin =
            crate::sql::expr::functions::pg_compat::to_regtype(vec![Value::Text(raw.clone())])?;
        if !matches!(resolved_builtin, Value::Null) {
            return Ok(resolved_builtin);
        }

        let (without_array, is_array) = strip_regtype_array_dims(&raw);
        let normalized = strip_regtype_typmod(&without_array);
        if normalized.trim().is_empty() {
            return Ok(Value::Null);
        }

        let store = self.store();
        let oid = lookup_regtype_oid_with_hstore_extension(
            &store,
            txn,
            db_id,
            &normalized,
            is_array,
            search_path,
        )
        .await?;
        Ok(oid.map(Value::Int64).unwrap_or(Value::Null))
    }

    async fn eval_pg_get_serial_sequence(
        &self,
        args: &[TypedExpr],
        row: &Row,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        qctx: &QueryContext,
    ) -> Result<Value> {
        let Some(table_arg_expr) = args.first() else {
            return Ok(Value::Null);
        };
        let Some(column_arg_expr) = args.get(1) else {
            return Ok(Value::Null);
        };

        let table_arg_type = pg_get_serial_sequence_arg_type_name(table_arg_expr);
        let column_arg_type = pg_get_serial_sequence_arg_type_name(column_arg_expr);
        if !pg_get_serial_sequence_accepts_text_arg(table_arg_expr)
            || !pg_get_serial_sequence_accepts_text_arg(column_arg_expr)
        {
            return Err(anyhow!(
                "function pg_get_serial_sequence({}, {}) does not exist",
                table_arg_type,
                column_arg_type
            ));
        }

        // Evaluate both arguments before validation.
        // Strict function semantics: any NULL argument → NULL result.
        let table_val = eval_typed_expr(table_arg_expr, row, qctx)?;
        let column_val = eval_typed_expr(column_arg_expr, row, qctx)?;

        if matches!(table_val, Value::Null) || matches!(column_val, Value::Null) {
            return Ok(Value::Null);
        }

        let table_arg = match table_val {
            Value::Text(s) => s,
            _ => {
                return Err(anyhow!(
                    "function pg_get_serial_sequence({}, {}) does not exist",
                    table_arg_type,
                    column_arg_type
                ));
            }
        };
        if table_arg.trim().is_empty() {
            return Err(SqlError::InvalidName("invalid name syntax".to_string()).into());
        }
        let column_arg = match column_val {
            Value::Text(s) => s,
            _ => {
                return Err(anyhow!(
                    "function pg_get_serial_sequence({}, {}) does not exist",
                    table_arg_type,
                    column_arg_type
                ));
            }
        };

        let store = self.store();
        let Some(sequence_full_name) = sequences::resolve_serial_sequence(
            &store,
            txn,
            db_id,
            search_path,
            &table_arg,
            &column_arg,
        )
        .await?
        else {
            return Ok(Value::Null);
        };

        let (schema, sequence_name) = crate::sql::names::parse_full_name(&sequence_full_name)?;
        Ok(Value::Text(sequences::format_serial_sequence_name(
            &schema,
            &sequence_name,
        )))
    }
}

fn find_text_column<'a>(
    schema: Option<&'a TableSchema>,
    primary: &str,
) -> Option<(&'a TableSchema, usize)> {
    let schema = schema?;
    if let Some(idx) = schema
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(primary))
    {
        return Some((schema, idx));
    }
    None
}

use crate::sql::expr::functions::pg_compat::{strip_regtype_array_dims, strip_regtype_typmod};

fn split_regtype_name_parts(raw: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                current.push(ch);
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                }
            }
            '.' if !in_quotes => {
                parts.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }

    if in_quotes {
        return None;
    }

    parts.push(current);
    Some(parts)
}

fn parse_regtype_ident(raw: &str) -> Option<sqlparser::ast::Ident> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('"') || trimmed.ends_with('"') {
        if !(trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"')) {
            return None;
        }
        let mut out = String::new();
        let mut chars = trimmed[1..trimmed.len() - 1].chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    out.push('"');
                    chars.next();
                } else {
                    return None;
                }
            } else {
                out.push(ch);
            }
        }
        let mut ident = sqlparser::ast::Ident::new(out);
        ident.quote_style = Some('"');
        return Some(ident);
    }

    if trimmed.contains('"') {
        return None;
    }

    Some(sqlparser::ast::Ident::new(trimmed.to_lowercase()))
}

fn parse_regtype_object_name(raw: &str) -> Option<sqlparser::ast::ObjectName> {
    let parts = split_regtype_name_parts(raw)?;
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let mut idents = Vec::with_capacity(parts.len());
    for p in parts {
        idents.push(parse_regtype_ident(&p)?);
    }
    Some(sqlparser::ast::ObjectName(idents))
}

fn regtype_ident_matches(ident: &sqlparser::ast::Ident, expected: &str) -> bool {
    if ident.quote_style.is_some() {
        ident.value == expected
    } else {
        ident.value.eq_ignore_ascii_case(expected)
    }
}

fn value_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(*n as i64),
        Value::Int64(n) => Some(*n),
        Value::Float64(n) => Some(*n as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn value_to_i64_strict(v: &Value, arg_name: &str) -> Result<i64> {
    match v {
        Value::Int32(n) => Ok(*n as i64),
        Value::Int64(n) => Ok(*n),
        Value::Text(s) => s.trim().parse::<i64>().map_err(|_| {
            anyhow!(
                "pg_get_indexdef(oid, column_no, pretty): {} must be integer-compatible, got {:?}",
                arg_name,
                v
            )
        }),
        _ => Err(anyhow!(
            "pg_get_indexdef(oid, column_no, pretty): {} must be integer-compatible, got {:?}",
            arg_name,
            v
        )),
    }
}

fn value_to_bool_strict(v: &Value, arg_name: &str) -> Result<bool> {
    match v {
        Value::Boolean(b) => Ok(*b),
        _ => Err(anyhow!(
            "pg_get_indexdef(oid, column_no, pretty): {} must be boolean, got {:?}",
            arg_name,
            v
        )),
    }
}

async fn lookup_indexdef_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<Option<String>> {
    use crate::sql::catalog::helpers::{format_indexdef, split_schema_and_name};
    use crate::sql::catalog_oids;

    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(&full_table_name);
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                return Ok(Some(format_indexdef(&table_schema, &table_name, idx)));
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                let pk_cols: Vec<String> = schema
                    .pk_indices
                    .iter()
                    .filter_map(|idx| schema.columns.get(*idx).map(|c| c.name.clone()))
                    .collect();
                let indexdef = format!(
                    "CREATE UNIQUE INDEX {}_pkey ON {}.{} USING btree ({})",
                    table_name,
                    table_schema,
                    table_name,
                    pk_cols.join(", ")
                );
                return Ok(Some(indexdef));
            }
        }
    }

    Ok(None)
}

/// Lookup a single column name/expression from an index by OID and 1-based ordinal.
/// Used by `pg_get_indexdef(oid, column_no, pretty_bool)`.
/// Returns:
/// - `None` when index OID is not found
/// - `Some(None)` when index exists but ordinal is out of range
/// - `Some(Some(...))` when the ordinal resolves to a column/expression
async fn lookup_index_column_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
    col_no: usize, // 1-based
) -> Result<Option<Option<String>>> {
    use crate::sql::catalog_oids;

    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                // Build ordered list: regular columns then expression columns
                let mut elements: Vec<String> = idx.columns.clone();
                elements.extend(idx.expressions.iter().cloned());
                let element = elements.get(col_no - 1).cloned();
                return Ok(Some(element));
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                let pk_cols: Vec<String> = schema
                    .pk_indices
                    .iter()
                    .filter_map(|i| schema.columns.get(*i).map(|c| c.name.clone()))
                    .collect();
                let element = pk_cols.get(col_no - 1).cloned();
                return Ok(Some(element));
            }
        }
    }

    Ok(None)
}

async fn index_oid_exists_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<bool> {
    use crate::sql::catalog_oids;

    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                return Ok(true);
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

async fn lookup_typname_by_oid(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<Option<String>> {
    if let Some(t) = crate::sql::pg_types::format_type_name_for_oid(oid) {
        return Ok(Some(t.to_string()));
    }

    let mut types = store.list_types(txn, db_id).await?;
    types.sort_by_key(|t| t.oid);
    Ok(types
        .into_iter()
        .find(|t| t.oid as i64 == oid)
        .map(|t| t.name))
}

fn regtype_search_path_schemas(search_path: &[String]) -> Vec<&str> {
    let mut schemas: Vec<&str> = search_path
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.eq_ignore_ascii_case("$user"))
        .collect();
    if schemas.is_empty() {
        schemas.push("public");
    }
    schemas
}

fn hstore_extension_oid_for_name(name: &sqlparser::ast::Ident, is_array: bool) -> Option<i64> {
    let base_oid = if regtype_ident_matches(name, "hstore") {
        Some(crate::sql::pg_types::OID_HSTORE)
    } else if regtype_ident_matches(name, "_hstore") {
        Some(crate::sql::pg_types::OID_HSTORE_ARRAY)
    } else {
        None
    };

    if is_array {
        match base_oid {
            Some(crate::sql::pg_types::OID_HSTORE) => Some(crate::sql::pg_types::OID_HSTORE_ARRAY),
            _ => None,
        }
    } else {
        base_oid
    }
}

async fn lookup_hstore_extension_oid_if_enabled(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    name: &sqlparser::ast::Ident,
    is_array: bool,
) -> Result<Option<i64>> {
    let Some(ext_oid) = hstore_extension_oid_for_name(name, is_array) else {
        return Ok(None);
    };
    let Some(installed) = store.get_extension(txn, db_id, "hstore").await? else {
        return Ok(None);
    };
    if !installed.enabled {
        return Ok(None);
    }
    Ok(Some(ext_oid))
}

async fn lookup_regtype_oid_with_hstore_extension(
    store: &Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    normalized_type_name: &str,
    is_array: bool,
    search_path: &[String],
) -> Result<Option<i64>> {
    let Some(type_name) = parse_regtype_object_name(normalized_type_name) else {
        return Ok(None);
    };
    let parts = type_name.0;
    match parts.as_slice() {
        [name] => {
            for schema in regtype_search_path_schemas(search_path) {
                let full_name = format!("{}.{}", schema, name.value);
                if let Some(def) = store.get_type(txn, db_id, &full_name).await? {
                    if is_array {
                        // db9 does not synthesize array OIDs for user-defined base types yet.
                        return Ok(None);
                    }
                    return Ok(Some(def.oid as i64));
                }

                if schema == "public" {
                    if let Some(ext_oid) =
                        lookup_hstore_extension_oid_if_enabled(store, txn, db_id, name, is_array)
                            .await?
                    {
                        return Ok(Some(ext_oid));
                    }
                }
            }
            Ok(None)
        }
        [schema, name] => {
            let full_name = format!("{}.{}", schema.value, name.value);
            if let Some(def) = store.get_type(txn, db_id, &full_name).await? {
                if is_array {
                    // db9 does not synthesize array OIDs for user-defined base types yet.
                    return Ok(None);
                }
                return Ok(Some(def.oid as i64));
            }

            if regtype_ident_matches(schema, "public") {
                return lookup_hstore_extension_oid_if_enabled(store, txn, db_id, name, is_array)
                    .await;
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        advisory_lock_timeout, find_text_column, hstore_extension_oid_for_name,
        is_pg_get_serial_sequence_function_name,
        non_pg_catalog_qualified_pg_get_serial_sequence_signature, parse_regtype_object_name,
        pg_get_serial_sequence_accepts_text_arg, pg_get_serial_sequence_arg_type_name,
        regtype_ident_matches, regtype_search_path_schemas, split_regtype_name_parts,
        strip_regtype_array_dims, strip_regtype_typmod, value_to_bool_strict, value_to_i64,
        value_to_i64_strict,
    };
    use crate::model::{ColumnDef, DataType, TableSchema, Value};
    use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn test_advisory_lock_timeout_reads_lock_timeout_guc_when_typed_missing() {
        let mut settings = HashMap::new();
        settings.insert("statement_timeout".to_string(), "30s".to_string());
        settings.insert("lock_timeout".to_string(), "100ms".to_string());
        assert_eq!(
            advisory_lock_timeout(None, Some(&settings)),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn test_advisory_lock_timeout_ignores_zero_and_invalid_values() {
        let mut settings = HashMap::new();
        settings.insert("lock_timeout".to_string(), "0".to_string());
        assert_eq!(advisory_lock_timeout(None, Some(&settings)), None);

        settings.insert("lock_timeout".to_string(), "not_a_timeout".to_string());
        assert_eq!(advisory_lock_timeout(None, Some(&settings)), None);
    }

    #[test]
    fn test_advisory_lock_timeout_prefers_typed_query_context_timeout() {
        let mut settings = HashMap::new();
        settings.insert("lock_timeout".to_string(), "10ms".to_string());
        assert_eq!(
            advisory_lock_timeout(Some(Duration::from_millis(250)), Some(&settings)),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn test_advisory_lock_timeout_handles_missing_sources() {
        assert_eq!(advisory_lock_timeout(None, None), None);
        assert_eq!(
            advisory_lock_timeout(Some(Duration::from_millis(1)), None),
            Some(Duration::from_millis(1))
        );
    }

    #[test]
    fn test_advisory_lock_timeout_returns_none_when_lock_timeout_missing() {
        let mut settings = HashMap::new();
        settings.insert("statement_timeout".to_string(), "5s".to_string());
        assert_eq!(advisory_lock_timeout(None, Some(&settings)), None);
    }

    #[test]
    fn test_advisory_lock_timeout_parses_non_ms_unit_values() {
        let mut settings = HashMap::new();
        settings.insert("lock_timeout".to_string(), "2s".to_string());
        assert_eq!(
            advisory_lock_timeout(None, Some(&settings)),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn test_advisory_lock_timeout_parses_minute_unit_values() {
        let mut settings = HashMap::new();
        settings.insert("lock_timeout".to_string(), "1min".to_string());
        assert_eq!(
            advisory_lock_timeout(None, Some(&settings)),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn test_advisory_lock_timeout_parses_plain_milliseconds_value() {
        let mut settings = HashMap::new();
        settings.insert("lock_timeout".to_string(), "2500".to_string());
        assert_eq!(
            advisory_lock_timeout(None, Some(&settings)),
            Some(Duration::from_millis(2500))
        );
    }

    #[test]
    fn test_advisory_lock_timeout_keeps_zero_typed_timeout() {
        let mut settings = HashMap::new();
        settings.insert("lock_timeout".to_string(), "10s".to_string());
        assert_eq!(
            advisory_lock_timeout(Some(Duration::ZERO), Some(&settings)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn test_value_to_i64_strict_accepts_integer_text() {
        assert_eq!(
            value_to_i64_strict(&Value::Text("42".to_string()), "column_no").unwrap(),
            42
        );
        assert_eq!(
            value_to_i64_strict(&Value::Int32(7), "column_no").unwrap(),
            7
        );
        assert_eq!(
            value_to_i64_strict(&Value::Int64(8), "column_no").unwrap(),
            8
        );
    }

    #[test]
    fn test_value_to_i64_strict_rejects_non_integer_text() {
        assert!(value_to_i64_strict(&Value::Text("not_an_int".to_string()), "column_no").is_err());
    }

    #[test]
    fn test_value_to_i64_strict_rejects_float64() {
        assert!(value_to_i64_strict(&Value::Float64(1.9), "column_no").is_err());
    }

    #[test]
    fn test_value_to_i64_strict_error_mentions_argument_name() {
        let err = value_to_i64_strict(&Value::Boolean(false), "column_no").unwrap_err();
        assert!(err.to_string().contains("column_no"));
    }

    #[test]
    fn test_value_to_i64_strict_accepts_negative_integer_text_with_spaces() {
        assert_eq!(
            value_to_i64_strict(&Value::Text("  -42 ".to_string()), "column_no").unwrap(),
            -42
        );
    }

    #[test]
    fn test_value_to_i64_strict_rejects_null_value() {
        assert!(value_to_i64_strict(&Value::Null, "column_no").is_err());
    }

    #[test]
    fn test_value_to_bool_strict_accepts_boolean() {
        assert!(value_to_bool_strict(&Value::Boolean(true), "pretty").unwrap());
    }

    #[test]
    fn test_value_to_bool_strict_rejects_int32() {
        assert!(value_to_bool_strict(&Value::Int32(1), "pretty").is_err());
    }

    #[test]
    fn test_value_to_bool_strict_error_mentions_argument_name() {
        let err = value_to_bool_strict(&Value::Text("true".to_string()), "pretty").unwrap_err();
        assert!(err.to_string().contains("pretty"));
    }

    #[test]
    fn test_value_to_bool_strict_rejects_null_value() {
        assert!(value_to_bool_strict(&Value::Null, "pretty").is_err());
    }

    #[test]
    fn test_find_text_column_case_insensitive_and_none_cases() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "OID".to_string(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "typname".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let (_, idx) = find_text_column(Some(&schema), "TypName").expect("column should match");
        assert_eq!(idx, 1);
        assert!(find_text_column(Some(&schema), "missing").is_none());
        assert!(find_text_column(None, "typname").is_none());
    }

    #[test]
    fn test_find_text_column_returns_first_match_when_duplicate_names_exist() {
        let schema = TableSchema {
            name: "dup".to_string(),
            table_id: 2,
            columns: vec![
                ColumnDef {
                    name: "typname".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "TyPnAmE".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let (_, idx) = find_text_column(Some(&schema), "typname").expect("column should match");
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_value_to_i64_accepts_numeric_and_text_inputs() {
        assert_eq!(value_to_i64(&Value::Int32(7)), Some(7));
        assert_eq!(value_to_i64(&Value::Int64(8)), Some(8));
        assert_eq!(value_to_i64(&Value::Float64(9.9)), Some(9));
        assert_eq!(value_to_i64(&Value::Float64(-9.1)), Some(-9));
        assert_eq!(value_to_i64(&Value::Text("+12".to_string())), Some(12));
        assert_eq!(value_to_i64(&Value::Text(" 10 ".to_string())), Some(10));
        assert_eq!(value_to_i64(&Value::Text(" -11 ".to_string())), Some(-11));
        assert_eq!(value_to_i64(&Value::Text("12.5".to_string())), None);
        assert_eq!(value_to_i64(&Value::Text("bad".to_string())), None);
        assert_eq!(value_to_i64(&Value::Null), None);
        assert_eq!(value_to_i64(&Value::Boolean(true)), None);
    }

    #[test]
    fn test_strip_regtype_array_dims_and_typmod() {
        assert_eq!(
            strip_regtype_array_dims("integer[]"),
            ("integer".to_string(), true)
        );
        assert_eq!(
            strip_regtype_array_dims("\"MyType\"[][]"),
            ("\"MyType\"".to_string(), true)
        );
        assert_eq!(strip_regtype_typmod("varchar(5)"), "varchar".to_string());
        assert_eq!(strip_regtype_typmod("numeric(10,2)"), "numeric".to_string());
        assert_eq!(strip_regtype_typmod("\"MyType\""), "\"MyType\"".to_string());
    }

    #[test]
    fn test_parse_regtype_object_name_preserves_quoted_case() {
        let unquoted = parse_regtype_object_name("mytype").expect("parse unquoted");
        assert_eq!(unquoted.0.len(), 1);
        assert_eq!(unquoted.0[0].value, "mytype");
        assert!(unquoted.0[0].quote_style.is_none());

        let quoted = parse_regtype_object_name("\"MyType\"").expect("parse quoted");
        assert_eq!(quoted.0.len(), 1);
        assert_eq!(quoted.0[0].value, "MyType");
        assert_eq!(quoted.0[0].quote_style, Some('"'));

        let qualified =
            parse_regtype_object_name("\"MySchema\".\"MyType\"").expect("parse qualified");
        assert_eq!(qualified.0.len(), 2);
        assert_eq!(qualified.0[0].value, "MySchema");
        assert_eq!(qualified.0[0].quote_style, Some('"'));
        assert_eq!(qualified.0[1].value, "MyType");
        assert_eq!(qualified.0[1].quote_style, Some('"'));
    }

    #[test]
    fn test_regtype_ident_matches_respects_quoted_case() {
        let unquoted = parse_regtype_object_name("HSTORE").expect("parse unquoted");
        assert!(regtype_ident_matches(&unquoted.0[0], "hstore"));
        assert!(regtype_ident_matches(&unquoted.0[0], "HSTORE"));

        let quoted = parse_regtype_object_name("\"HSTORE\"").expect("parse quoted");
        assert!(!regtype_ident_matches(&quoted.0[0], "hstore"));
        assert!(regtype_ident_matches(&quoted.0[0], "HSTORE"));
    }

    #[test]
    fn test_parse_regtype_object_name_rejects_invalid_shapes() {
        assert!(parse_regtype_object_name("").is_none());
        assert!(parse_regtype_object_name("\"unterminated").is_none());
        assert!(parse_regtype_object_name("a.b.c").is_none());
    }

    #[test]
    fn test_split_regtype_name_parts_handles_escaped_quotes() {
        let parts = split_regtype_name_parts("\"A\".\"B\"").expect("split");
        assert_eq!(parts, vec!["\"A\"".to_string(), "\"B\"".to_string()]);
        let escaped = split_regtype_name_parts("\"A\"\"B\"").expect("split escaped");
        assert_eq!(escaped, vec!["\"A\"\"B\"".to_string()]);
    }

    #[test]
    fn test_regtype_search_path_schemas_preserves_order_and_defaults_public() {
        let path = vec!["$user".to_string(), "s1".to_string(), "public".to_string()];
        let schemas = regtype_search_path_schemas(&path);
        assert_eq!(schemas, vec!["s1", "public"]);
        assert_eq!(regtype_search_path_schemas(&[]), vec!["public"]);
    }

    #[test]
    fn test_hstore_extension_oid_for_name_respects_identifier_and_array_rules() {
        let hstore = parse_regtype_object_name("hstore").expect("parse");
        assert_eq!(
            hstore_extension_oid_for_name(&hstore.0[0], false),
            Some(crate::sql::pg_types::OID_HSTORE)
        );
        assert_eq!(
            hstore_extension_oid_for_name(&hstore.0[0], true),
            Some(crate::sql::pg_types::OID_HSTORE_ARRAY)
        );

        let array_alias = parse_regtype_object_name("_hstore").expect("parse");
        assert_eq!(
            hstore_extension_oid_for_name(&array_alias.0[0], false),
            Some(crate::sql::pg_types::OID_HSTORE_ARRAY)
        );
        assert_eq!(hstore_extension_oid_for_name(&array_alias.0[0], true), None);

        let quoted_upper = parse_regtype_object_name("\"HSTORE\"").expect("parse quoted");
        assert_eq!(
            hstore_extension_oid_for_name(&quoted_upper.0[0], false),
            None
        );
    }
    #[test]
    fn test_pg_get_serial_sequence_name_match_accepts_pg_catalog_and_case_variants() {
        assert!(is_pg_get_serial_sequence_function_name(
            "pg_get_serial_sequence"
        ));
        assert!(is_pg_get_serial_sequence_function_name(
            "PG_GET_SERIAL_SEQUENCE"
        ));
        assert!(is_pg_get_serial_sequence_function_name(
            "pg_catalog.pg_get_serial_sequence"
        ));
        assert!(is_pg_get_serial_sequence_function_name(
            "PG_CATALOG.PG_GET_SERIAL_SEQUENCE"
        ));
        assert!(!is_pg_get_serial_sequence_function_name(
            "public.pg_get_serial_sequence"
        ));
    }

    #[test]
    fn test_pg_get_serial_sequence_arg_type_guard_matches_pg_signature_surface() {
        let int_arg = TypedExpr {
            kind: TypedExprKind::Constant(Value::Int32(1)),
            data_type: DataType::Int32,
        };
        let text_arg = TypedExpr {
            kind: TypedExprKind::Constant(Value::Text("t".to_string())),
            data_type: DataType::Text,
        };
        let null_arg = TypedExpr {
            kind: TypedExprKind::Constant(Value::Null),
            data_type: DataType::Text,
        };
        let varchar_arg = TypedExpr {
            kind: TypedExprKind::Parameter { index: 0 },
            data_type: DataType::Varchar(32),
        };

        let name_arg = TypedExpr {
            kind: TypedExprKind::Constant(Value::Text("t".to_string())),
            data_type: DataType::Name,
        };

        assert!(!pg_get_serial_sequence_accepts_text_arg(&int_arg));
        assert!(pg_get_serial_sequence_accepts_text_arg(&text_arg));
        assert!(pg_get_serial_sequence_accepts_text_arg(&null_arg));
        assert!(pg_get_serial_sequence_accepts_text_arg(&varchar_arg));
        assert!(pg_get_serial_sequence_accepts_text_arg(&name_arg));

        assert_eq!(pg_get_serial_sequence_arg_type_name(&int_arg), "integer");
        assert_eq!(pg_get_serial_sequence_arg_type_name(&text_arg), "unknown");
        assert_eq!(pg_get_serial_sequence_arg_type_name(&null_arg), "unknown");
    }

    #[test]
    fn test_non_pg_catalog_qualified_pg_get_serial_sequence_returns_function_signature() {
        let text_arg = TypedExpr {
            kind: TypedExprKind::Constant(Value::Text("t".to_string())),
            data_type: DataType::Text,
        };
        let null_arg = TypedExpr {
            kind: TypedExprKind::Constant(Value::Null),
            data_type: DataType::Text,
        };
        let signature = non_pg_catalog_qualified_pg_get_serial_sequence_signature(
            "PUBLIC.PG_GET_SERIAL_SEQUENCE",
            &[text_arg, null_arg],
        );
        assert_eq!(
            signature,
            Some("public.pg_get_serial_sequence(unknown, unknown)".to_string())
        );

        assert_eq!(
            non_pg_catalog_qualified_pg_get_serial_sequence_signature(
                "pg_catalog.pg_get_serial_sequence",
                &[],
            ),
            None
        );
        assert_eq!(
            non_pg_catalog_qualified_pg_get_serial_sequence_signature(
                "pg_get_serial_sequence",
                &[],
            ),
            None
        );
    }
}
