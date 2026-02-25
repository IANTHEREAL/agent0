//! `replace_sequence_functions` -- recursive AST rewriting for NEXTVAL/CURRVAL/SETVAL/
//! CURRENT_SCHEMA/PG_GET_INDEXDEF dispatch and user function evaluation.

use crate::model::{Row, TableSchema, Value};
use crate::sql::names;
use crate::sql::names::function_name_upper;
use crate::sql::plpgsql;
use crate::sql::value_coercion::value_to_sql_expr;
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::Transaction;

use super::{
    eval_seq_expr, extract_arg_expr, index_helpers::lookup_indexdef_by_oid,
    resolve_sequence_full_name_from_value, value_to_i64,
};

pub(crate) fn replace_sequence_functions<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    last_sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    expr: &'a Expr,
    row: Option<&'a Row>,
    schema: Option<&'a TableSchema>,
) -> Pin<Box<dyn Future<Output = Result<Expr>> + Send + 'a>> {
    Box::pin(async move {
        match expr {
            Expr::Function(func) => {
                let name = function_name_upper(func);
                match name.as_str() {
                    "CURRENT_SCHEMA" => Ok(value_to_sql_expr(&crate::model::Value::Text(
                        names::default_schema(search_path).to_string(),
                    ))),
                    "NEXTVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            db_id,
                            search_path,
                            eval_seq_expr(arg0, row, schema)?,
                        )
                        .await?;
                        let val = store.nextval_sequence(txn, db_id, &full_name).await?;
                        last_sequence_values.insert(full_name, val);
                        Ok(value_to_sql_expr(&crate::model::Value::Int64(val)))
                    }
                    "CURRVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            db_id,
                            search_path,
                            eval_seq_expr(arg0, row, schema)?,
                        )
                        .await?;
                        if store.get_sequence(txn, db_id, &full_name).await?.is_none() {
                            return Err(crate::sql::error::SqlError::RelationNotFound(
                                full_name.clone(),
                            )
                            .into());
                        }
                        let val =
                            last_sequence_values
                                .get(&full_name)
                                .copied()
                                .ok_or_else(|| {
                                    anyhow!(
                            "currval of sequence \"{}\" is not yet defined in this session",
                            full_name
                        )
                                })?;
                        Ok(value_to_sql_expr(&crate::model::Value::Int64(val)))
                    }
                    "SETVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let arg1 = extract_arg_expr(&func.args, 1)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            db_id,
                            search_path,
                            eval_seq_expr(arg0, row, schema)?,
                        )
                        .await?;
                        let val = eval_seq_expr(arg1, row, schema)?;
                        let value_i64 = match val {
                            crate::model::Value::Int32(n) => n as i64,
                            crate::model::Value::Int64(n) => n,
                            crate::model::Value::Float64(n) => n as i64,
                            crate::model::Value::Text(s) => s
                                .trim()
                                .parse::<i64>()
                                .map_err(|_| anyhow!("setval: value must be integer, got {}", s))?,
                            other => {
                                return Err(anyhow!("setval: value must be integer, got {}", other))
                            }
                        };
                        let is_called = if func.args.len() >= 3 {
                            let arg2 = extract_arg_expr(&func.args, 2)?;
                            match eval_seq_expr(arg2, row, schema)? {
                                crate::model::Value::Boolean(b) => b,
                                crate::model::Value::Text(s) => {
                                    matches!(
                                        s.to_lowercase().as_str(),
                                        "true" | "t" | "1" | "yes" | "y"
                                    )
                                }
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
                        Ok(value_to_sql_expr(&crate::model::Value::Int64(res)))
                    }
                    "PG_GET_INDEXDEF" => {
                        let arg0 = match extract_arg_expr(&func.args, 0) {
                            Ok(expr) => expr,
                            Err(_) => {
                                return Ok(value_to_sql_expr(&Value::Text(
                                    "CREATE INDEX".to_string(),
                                )));
                            }
                        };
                        let oid_val = eval_seq_expr(arg0, row, schema)?;
                        let Some(oid) = value_to_i64(&oid_val) else {
                            return Ok(value_to_sql_expr(&Value::Text("CREATE INDEX".to_string())));
                        };

                        if let (Some(row), Some(schema)) = (row, schema) {
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
                                                return Ok(value_to_sql_expr(row_def));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        let indexdef = lookup_indexdef_by_oid(store, txn, db_id, oid).await?;
                        Ok(value_to_sql_expr(&Value::Text(
                            indexdef.unwrap_or_else(|| "CREATE INDEX".to_string()),
                        )))
                    }
                    _ => {
                        let mut resolved_args = Vec::with_capacity(func.args.len());
                        let mut arg_values = Vec::new();
                        for arg in &func.args {
                            let resolved_arg = match arg {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    let resolved = replace_sequence_functions(
                                        store,
                                        txn,
                                        db_id,
                                        last_sequence_values,
                                        search_path,
                                        e,
                                        row,
                                        schema,
                                    )
                                    .await?;
                                    if let Ok(val) = eval_seq_expr(&resolved, row, schema) {
                                        arg_values.push(val);
                                    }
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(resolved))
                                }
                                other => other.clone(),
                            };
                            resolved_args.push(resolved_arg);
                        }

                        let func_name_str = func
                            .name
                            .0
                            .iter()
                            .map(|i| i.value.as_str())
                            .collect::<Vec<_>>()
                            .join(".");
                        if let Ok(Some(result)) = plpgsql::try_execute_user_function(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            &func_name_str,
                            arg_values,
                            None,
                        )
                        .await
                        {
                            return Ok(value_to_sql_expr(&result));
                        }

                        let resolved_filter = if let Some(filter) = &func.filter {
                            Some(Box::new(
                                replace_sequence_functions(
                                    store,
                                    txn,
                                    db_id,
                                    last_sequence_values,
                                    search_path,
                                    filter,
                                    row,
                                    schema,
                                )
                                .await?,
                            ))
                        } else {
                            None
                        };

                        let mut resolved_order_by = func.order_by.clone();
                        for ob in &mut resolved_order_by {
                            ob.expr = replace_sequence_functions(
                                store,
                                txn,
                                db_id,
                                last_sequence_values,
                                search_path,
                                &ob.expr,
                                row,
                                schema,
                            )
                            .await?;
                        }

                        Ok(Expr::Function(Function {
                            name: func.name.clone(),
                            args: resolved_args,
                            filter: resolved_filter,
                            null_treatment: func.null_treatment.clone(),
                            over: func.over.clone(),
                            distinct: func.distinct,
                            special: func.special,
                            order_by: resolved_order_by,
                        }))
                    }
                }
            }
            Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
                left: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        left,
                        row,
                        schema,
                    )
                    .await?,
                ),
                op: op.clone(),
                right: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        right,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::UnaryOp { op, expr } => Ok(Expr::UnaryOp {
                op: op.clone(),
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::Nested(inner) => Ok(Expr::Nested(Box::new(
                replace_sequence_functions(
                    store,
                    txn,
                    db_id,
                    last_sequence_values,
                    search_path,
                    inner,
                    row,
                    schema,
                )
                .await?,
            ))),
            Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(
                replace_sequence_functions(
                    store,
                    txn,
                    db_id,
                    last_sequence_values,
                    search_path,
                    inner,
                    row,
                    schema,
                )
                .await?,
            ))),
            Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(
                replace_sequence_functions(
                    store,
                    txn,
                    db_id,
                    last_sequence_values,
                    search_path,
                    inner,
                    row,
                    schema,
                )
                .await?,
            ))),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let resolved_expr = replace_sequence_functions(
                    store,
                    txn,
                    db_id,
                    last_sequence_values,
                    search_path,
                    expr,
                    row,
                    schema,
                )
                .await?;
                let mut resolved_list = Vec::with_capacity(list.len());
                for item in list {
                    resolved_list.push(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            item,
                            row,
                            schema,
                        )
                        .await?,
                    );
                }
                Ok(Expr::InList {
                    expr: Box::new(resolved_expr),
                    list: resolved_list,
                    negated: *negated,
                })
            }
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => Ok(Expr::Between {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                negated: *negated,
                low: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        low,
                        row,
                        schema,
                    )
                    .await?,
                ),
                high: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        high,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                let resolved_operand = if let Some(op) = operand {
                    Some(Box::new(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            op,
                            row,
                            schema,
                        )
                        .await?,
                    ))
                } else {
                    None
                };
                let mut resolved_conditions = Vec::with_capacity(conditions.len());
                for cond in conditions {
                    resolved_conditions.push(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            cond,
                            row,
                            schema,
                        )
                        .await?,
                    );
                }
                let mut resolved_results = Vec::with_capacity(results.len());
                for res in results {
                    resolved_results.push(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            res,
                            row,
                            schema,
                        )
                        .await?,
                    );
                }
                let resolved_else = if let Some(else_expr) = else_result {
                    Some(Box::new(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            else_expr,
                            row,
                            schema,
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
            Expr::Cast {
                expr,
                data_type,
                format,
            } => Ok(Expr::Cast {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                data_type: data_type.clone(),
                format: format.clone(),
            }),
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                special,
            } => Ok(Expr::Substring {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                substring_from: match substring_from {
                    Some(e) => Some(Box::new(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            e,
                            row,
                            schema,
                        )
                        .await?,
                    )),
                    None => None,
                },
                substring_for: match substring_for {
                    Some(e) => Some(Box::new(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            e,
                            row,
                            schema,
                        )
                        .await?,
                    )),
                    None => None,
                },
                special: *special,
            }),
            Expr::Trim {
                expr,
                trim_where,
                trim_what,
                trim_characters,
            } => Ok(Expr::Trim {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                trim_where: trim_where.clone(),
                trim_what: match trim_what {
                    Some(e) => Some(Box::new(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            e,
                            row,
                            schema,
                        )
                        .await?,
                    )),
                    None => None,
                },
                trim_characters: trim_characters.clone(),
            }),
            Expr::Position { expr, r#in } => Ok(Expr::Position {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                r#in: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        r#in,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::Extract { field, expr } => Ok(Expr::Extract {
                field: *field,
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::Ceil { expr, field } => Ok(Expr::Ceil {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                field: *field,
            }),
            Expr::Floor { expr, field } => Ok(Expr::Floor {
                expr: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        expr,
                        row,
                        schema,
                    )
                    .await?,
                ),
                field: *field,
            }),
            Expr::JsonAccess {
                left,
                operator,
                right,
            } => Ok(Expr::JsonAccess {
                left: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        left,
                        row,
                        schema,
                    )
                    .await?,
                ),
                operator: *operator,
                right: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        right,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::Array(arr) => {
                let mut elems = Vec::with_capacity(arr.elem.len());
                for elem in &arr.elem {
                    elems.push(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            elem,
                            row,
                            schema,
                        )
                        .await?,
                    );
                }
                Ok(Expr::Array(sqlparser::ast::Array {
                    elem: elems,
                    named: arr.named,
                }))
            }
            Expr::ArrayIndex { obj, indexes } => {
                let resolved_obj = replace_sequence_functions(
                    store,
                    txn,
                    db_id,
                    last_sequence_values,
                    search_path,
                    obj,
                    row,
                    schema,
                )
                .await?;
                let mut resolved_indexes = Vec::with_capacity(indexes.len());
                for idx in indexes {
                    resolved_indexes.push(
                        replace_sequence_functions(
                            store,
                            txn,
                            db_id,
                            last_sequence_values,
                            search_path,
                            idx,
                            row,
                            schema,
                        )
                        .await?,
                    );
                }
                Ok(Expr::ArrayIndex {
                    obj: Box::new(resolved_obj),
                    indexes: resolved_indexes,
                })
            }
            Expr::AnyOp {
                left,
                compare_op,
                right,
            } => Ok(Expr::AnyOp {
                left: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        left,
                        row,
                        schema,
                    )
                    .await?,
                ),
                compare_op: compare_op.clone(),
                right: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        right,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            Expr::AllOp {
                left,
                compare_op,
                right,
            } => Ok(Expr::AllOp {
                left: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        left,
                        row,
                        schema,
                    )
                    .await?,
                ),
                compare_op: compare_op.clone(),
                right: Box::new(
                    replace_sequence_functions(
                        store,
                        txn,
                        db_id,
                        last_sequence_values,
                        search_path,
                        right,
                        row,
                        schema,
                    )
                    .await?,
                ),
            }),
            _ => Ok(expr.clone()),
        }
    })
}
