//! Catalog-dependent expression materialization.
//!
//! Handles async resolution of catalog-dependent functions within typed
//! expressions: `pg_get_indexdef`, `pg_get_constraintdef`, `format_type`,
//! `pg_sleep`, user-defined functions, cron/bg_sql scalar functions, and
//! recursive traversal of composite expression nodes.

mod helpers;

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

use helpers::*;

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

fn pg_get_serial_sequence_arg_type_name(arg: &TypedExpr) -> String {
    if matches!(
        &arg.kind,
        TypedExprKind::Constant(Value::Text(_)) | TypedExprKind::Constant(Value::Null)
    ) {
        return "unknown".to_string();
    }
    arg.data_type.pg_display_name()
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

fn regclass_lookup_parts<'a>(
    parsed: &'a crate::sql::names::ParsedRegclassInput,
    current_database: &str,
    input: &str,
) -> Result<(Option<&'a str>, &'a str)> {
    if !parsed.is_current_database(current_database) {
        return Err(SqlError::Unsupported(format!(
            "cross-database references are not implemented: \"{}\"",
            input.trim()
        ))
        .into());
    }

    Ok(parsed.relation_lookup_parts())
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

                    if func.name.eq_ignore_ascii_case("TO_REGCLASS") {
                        let val = self
                            .eval_to_regclass(&new_args, row, txn, db_id, search_path, qctx)
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
                } => {
                    let materialized_inner = self
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

                    if matches!(
                        target_type,
                        DataType::UserDefined(name)
                            if name.eq_ignore_ascii_case("regclass")
                                || name.eq_ignore_ascii_case("pg_catalog.regclass")
                    ) {
                        let val = self
                            .eval_regclass_cast(
                                &materialized_inner,
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

                    if *target_type == DataType::Text
                        && matches!(
                            &materialized_inner.data_type,
                            DataType::UserDefined(name)
                                if name.eq_ignore_ascii_case("regclass")
                                    || name.eq_ignore_ascii_case("pg_catalog.regclass")
                        )
                    {
                        if let TypedExprKind::Constant(Value::Int64(oid)) = &materialized_inner.kind
                        {
                            if let Some(relname) = lookup_relname_by_regclass_oid(
                                self.store().as_ref(),
                                txn,
                                db_id,
                                *oid,
                                search_path,
                            )
                            .await?
                            {
                                return Ok(TypedExpr::new(
                                    TypedExprKind::Constant(Value::Text(relname)),
                                    DataType::Text,
                                ));
                            }
                        }
                    }

                    if *target_type == DataType::Text
                        && matches!(
                            &materialized_inner.data_type,
                            DataType::UserDefined(name)
                                if name.eq_ignore_ascii_case("regtype")
                                    || name.eq_ignore_ascii_case("pg_catalog.regtype")
                        )
                    {
                        if let TypedExprKind::Constant(value) = &materialized_inner.kind {
                            if let Some(oid) = value_to_i64(value) {
                                if let Some(typname) = lookup_regtype_text_by_oid(
                                    self.store().as_ref(),
                                    txn,
                                    db_id,
                                    oid,
                                    search_path,
                                )
                                .await?
                                {
                                    return Ok(TypedExpr::new(
                                        TypedExprKind::Constant(Value::Text(typname)),
                                        DataType::Text,
                                    ));
                                }
                            }
                        }
                    }

                    Ok(TypedExpr {
                        kind: TypedExprKind::Cast {
                            expr: Box::new(materialized_inner),
                            target_type: target_type.clone(),
                            cast_context: *cast_context,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }
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

        let Some(lookup) = crate::sql::expr::functions::pg_compat::parse_regtype_lookup(&raw)?
        else {
            return Ok(Value::Null);
        };

        let store = self.store();
        let resolved =
            lookup_regtype_oid_with_hstore_extension(&store, txn, db_id, &lookup, search_path)
                .await?;
        crate::sql::expr::functions::pg_compat::validate_resolved_regtype_typmod(
            &lookup,
            resolved.map(|resolved| resolved.oid),
            resolved.is_some_and(|resolved| resolved.is_user_defined),
        )?;
        Ok(resolved
            .map(|resolved| Value::Int64(resolved.oid))
            .unwrap_or(Value::Null))
    }

    async fn eval_to_regclass(
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
            _ => return Err(anyhow!("function to_regclass(text) does not exist")),
        };

        let parsed = crate::sql::names::parse_regclass_input(&raw).map_err(|input| {
            SqlError::Unsupported(format!(
                "cross-database references are not implemented: \"{}\"",
                input
            ))
        })?;
        let (schema_opt, name) = regclass_lookup_parts(&parsed, qctx.database_name.as_ref(), &raw)?;
        if name.is_empty() {
            return Ok(Value::Null);
        }

        let oid = crate::sql::names::resolve_existing_relation_oid(
            self.store().as_ref(),
            txn,
            db_id,
            schema_opt,
            name,
            search_path,
        )
        .await?;
        Ok(oid.map(Value::Int64).unwrap_or(Value::Null))
    }

    async fn eval_regclass_cast(
        &self,
        inner: &TypedExpr,
        row: &Row,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        qctx: &QueryContext,
    ) -> Result<Value> {
        let input = eval_typed_expr(inner, row, qctx)?;
        match input {
            Value::Null => Ok(Value::Null),
            Value::Int32(n) => Ok(Value::Int64(n as i64)),
            Value::Int64(n) => Ok(Value::Int64(n)),
            Value::Text(raw) => {
                let trimmed = raw.trim();
                if let Ok(n) = trimmed.parse::<i64>() {
                    return Ok(Value::Int64(n));
                }

                let parsed = crate::sql::names::parse_regclass_input(trimmed).map_err(|input| {
                    SqlError::Unsupported(format!(
                        "cross-database references are not implemented: \"{}\"",
                        input
                    ))
                })?;
                let (schema_opt, name) =
                    regclass_lookup_parts(&parsed, qctx.database_name.as_ref(), trimmed)?;

                if name.is_empty() {
                    return Err(SqlError::InvalidInputSyntax {
                        type_name: "regclass".into(),
                        value: raw,
                    }
                    .into());
                }

                let oid = crate::sql::names::resolve_existing_relation_oid(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    schema_opt,
                    name,
                    search_path,
                )
                .await?;

                oid.map(Value::Int64).ok_or_else(|| {
                    SqlError::InvalidInputSyntax {
                        type_name: "regclass".into(),
                        value: raw,
                    }
                    .into()
                })
            }
            other => Err(SqlError::InvalidInputSyntax {
                type_name: "regclass".into(),
                value: other.to_string(),
            }
            .into()),
        }
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

#[cfg(test)]
mod tests;
