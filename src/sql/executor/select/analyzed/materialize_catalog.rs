//! Catalog-dependent expression materialization.
//!
//! Handles async resolution of catalog-dependent functions within typed
//! expressions: `pg_get_indexdef`, `pg_get_constraintdef`, `format_type`,
//! `pg_sleep`, user-defined functions, cron/bg_sql scalar functions, and
//! recursive traversal of composite expression nodes.

use crate::sql::analyzer::types::{FunctionKind, TypedExpr, TypedExprKind};
use crate::sql::executor::core::Executor;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{DataType, Row, TableSchema, Value};

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::Transaction;

impl Executor {
    pub(super) fn materialize_catalog_functions<'a>(
        &'a self,
        expr: &'a TypedExpr,
        row: &'a Row,
        schema: Option<&'a TableSchema>,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
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
                        cast_context: cast_context.clone(),
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
        let Some(oid) = value_to_i64(&oid_val) else {
            return Ok(Value::Text("CREATE INDEX".to_string()));
        };

        // Prefer row-level indexdef if present and matches the OID.
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
                if let Some(v) = row.values.get(idx) {
                    if let Value::Text(s) = v {
                        return Ok(Value::Text(s.clone()));
                    }
                }
            }
        }

        let store = self.store();
        let typname = lookup_typname_by_oid(&store, txn, db_id, oid).await?;
        Ok(Value::Text(typname.unwrap_or_else(|| "text".to_string())))
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

fn value_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(*n as i64),
        Value::Int64(n) => Some(*n),
        Value::Float64(n) => Some(*n as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
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
