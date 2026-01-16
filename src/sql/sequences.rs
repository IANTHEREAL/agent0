use crate::storage::TikvStore;
use crate::types::{SequenceBacking, SequenceDef, SequenceState};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, MinMaxValue, ObjectName, SequenceOptions,
};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::Transaction;

use super::expr::{eval_expr, eval_expr_join, JoinContext};
use super::helpers::{normalize_ident, value_to_sql_expr};
use super::names;
use super::ExecuteResult;

pub(crate) fn expr_uses_sequence_functions(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        if let Expr::Function(func) = e {
            let name = function_name_upper(func);
            if matches!(name.as_str(), "NEXTVAL" | "CURRVAL" | "SETVAL") {
                found = true;
                return ControlFlow::Break(());
            }
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

pub(crate) fn expr_uses_current_schema(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        if let Expr::Function(func) = e {
            let name = function_name_upper(func);
            if name == "CURRENT_SCHEMA" {
                found = true;
                return ControlFlow::Break(());
            }
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

pub(crate) fn normalize_sequence_name(
    name: &ObjectName,
    search_path: &[String],
) -> Result<(String, String, String)> {
    let parts: Vec<String> = name.0.iter().map(normalize_ident).collect();
    if parts.is_empty() {
        return Err(anyhow!("Invalid sequence name"));
    }

    let (schema, seq_name) = match parts.as_slice() {
        [seq_name] => (
            names::default_schema(search_path).to_string(),
            seq_name.clone(),
        ),
        [schema, seq_name] => (schema.clone(), seq_name.clone()),
        _ => return Err(anyhow!("Invalid sequence name")),
    };

    Ok((
        schema.clone(),
        seq_name.clone(),
        format!("{}.{}", schema, seq_name),
    ))
}

pub(crate) fn implicit_sequence_name(table_name: &str, column_name: &str) -> String {
    format!("{}_{}_seq", table_name, column_name)
}

pub(crate) fn build_implicit_sequence_def(
    table_full_name: &str,
    column_name: &str,
    table_id: u64,
) -> SequenceDef {
    let (schema, table_name) = match table_full_name.split_once('.') {
        Some((schema, table)) if !schema.is_empty() && !table.is_empty() => {
            (schema.to_string(), table.to_string())
        }
        _ => ("public".to_string(), table_full_name.to_string()),
    };
    SequenceDef {
        schema,
        name: implicit_sequence_name(&table_name, column_name),
        increment: 1,
        min_value: 1,
        max_value: i64::MAX,
        is_cycled: false,
        owned_by: Some((table_full_name.to_string(), column_name.to_string())),
        owner: "postgres".to_string(),
        backing: SequenceBacking::TableId(table_id),
    }
}

fn eval_i64(expr: &Expr) -> Result<i64> {
    match eval_expr(expr, None, None)? {
        crate::types::Value::Int32(n) => Ok(n as i64),
        crate::types::Value::Int64(n) => Ok(n),
        crate::types::Value::Float64(n) => Ok(n as i64),
        crate::types::Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| anyhow!("Expected integer, got {}", s)),
        other => Err(anyhow!("Expected integer, got {}", other)),
    }
}

fn parse_sequence_name_token(token: &str) -> Result<(Option<String>, String)> {
    fn push_part(parts: &mut Vec<String>, raw: &str, quoted: bool) -> Result<()> {
        let trimmed = if quoted { raw } else { raw.trim() };
        if trimmed.is_empty() {
            return Err(anyhow!("Invalid sequence name"));
        }
        if quoted {
            parts.push(trimmed.to_string());
        } else {
            parts.push(trimmed.to_lowercase());
        }
        Ok(())
    }

    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut part_quoted = false;

    let mut chars = token.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        buf.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                    part_quoted = true;
                }
            }
            '.' if !in_quotes => {
                push_part(&mut parts, &buf, part_quoted)?;
                buf.clear();
                part_quoted = false;
            }
            _ => buf.push(ch),
        }
    }

    if in_quotes {
        return Err(anyhow!("Unterminated quoted identifier in sequence name"));
    }
    push_part(&mut parts, &buf, part_quoted)?;

    if parts.len() >= 2 {
        Ok((
            Some(parts[parts.len() - 2].clone()),
            parts[parts.len() - 1].clone(),
        ))
    } else {
        Ok((None, parts[0].clone()))
    }
}

fn parse_minmax(value: &MinMaxValue) -> Result<Option<i64>> {
    match value {
        MinMaxValue::Empty => Ok(None),
        MinMaxValue::None => Ok(None),
        MinMaxValue::Some(expr) => Ok(Some(eval_i64(expr)?)),
    }
}

pub(crate) async fn execute_create_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    search_path: &[String],
    name: &ObjectName,
    if_not_exists: bool,
    sequence_options: &[SequenceOptions],
) -> Result<ExecuteResult> {
    let (schema, seq_name, full_name) = normalize_sequence_name(name, search_path)?;
    if !store.schema_exists(txn, &schema).await? {
        return Err(anyhow!("schema '{}' does not exist", schema));
    }

    if if_not_exists && store.get_sequence(txn, &full_name).await?.is_some() {
        return Ok(ExecuteResult::Empty);
    }

    let mut start_value: i64 = 1;
    let mut increment: i64 = 1;
    let mut min_value: i64 = 1;
    let mut max_value: i64 = i64::MAX;
    let mut is_cycled: bool = false;

    for opt in sequence_options {
        match opt {
            SequenceOptions::StartWith(expr, _) => start_value = eval_i64(expr)?,
            SequenceOptions::IncrementBy(expr, _) => increment = eval_i64(expr)?,
            SequenceOptions::MinValue(v) => {
                if let Some(val) = parse_minmax(v)? {
                    min_value = val;
                }
            }
            SequenceOptions::MaxValue(v) => {
                if let Some(val) = parse_minmax(v)? {
                    max_value = val;
                }
            }
            SequenceOptions::Cycle(no_cycle) => {
                is_cycled = !*no_cycle;
            }
            SequenceOptions::Cache(_) => {}
        }
    }

    if increment == 0 {
        return Err(anyhow!("Sequence '{}' has invalid INCREMENT 0", full_name));
    }
    if min_value > max_value {
        return Err(anyhow!(
            "Sequence '{}' has invalid MINVALUE/MAXVALUE ({}/{})",
            full_name,
            min_value,
            max_value
        ));
    }
    if start_value < min_value || start_value > max_value {
        return Err(anyhow!(
            "Sequence '{}' START value {} is out of bounds ({}, {})",
            full_name,
            start_value,
            min_value,
            max_value
        ));
    }

    let def = SequenceDef {
        schema,
        name: seq_name,
        increment,
        min_value,
        max_value,
        is_cycled,
        owned_by: None,
        owner: "postgres".to_string(),
        backing: SequenceBacking::Standalone(SequenceState {
            last_value: start_value,
            is_called: false,
        }),
    };

    store.create_sequence(txn, def).await?;
    Ok(ExecuteResult::Empty)
}

pub(crate) async fn execute_drop_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ExecuteResult> {
    for name in names {
        let resolved =
            names::resolve_existing_sequence_name(store.as_ref(), txn, name, search_path).await?;
        let Some(resolved) = resolved else {
            if !if_exists {
                return Err(anyhow!("Sequence '{}' does not exist", name));
            }
            continue;
        };
        let existed = store.drop_sequence(txn, &resolved.full).await?;
        if !existed && !if_exists {
            return Err(anyhow!("Sequence '{}' does not exist", resolved.full));
        }
    }
    Ok(ExecuteResult::Empty)
}

fn function_name_upper(func: &Function) -> String {
    func.name
        .0
        .last()
        .map(|n| n.value.to_uppercase())
        .unwrap_or_default()
}

fn extract_arg_expr<'a>(args: &'a [FunctionArg], idx: usize) -> Result<&'a Expr> {
    match args.get(idx) {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) => Ok(expr),
        Some(_) => Err(anyhow!("Unsupported function argument")),
        None => Err(anyhow!("Missing function argument")),
    }
}

async fn resolve_sequence_full_name_from_value(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    search_path: &[String],
    v: crate::types::Value,
) -> Result<String> {
    let crate::types::Value::Text(s) = v else {
        return Err(anyhow!("Sequence name must be text/regclass"));
    };

    let (schema_opt, seq_name) = parse_sequence_name_token(&s)?;
    if let Some(schema) = schema_opt {
        return Ok(names::ResolvedName::new(schema, seq_name)?.full);
    }

    let candidates: Vec<&str> = if search_path.is_empty() {
        vec!["public"]
    } else {
        search_path.iter().map(|s| s.as_str()).collect()
    };
    for schema in candidates {
        let resolved = names::ResolvedName::new(schema.to_string(), seq_name.clone())?;
        if store.get_sequence(txn, &resolved.full).await?.is_some() {
            return Ok(resolved.full);
        }
    }

    Ok(names::ResolvedName::new(names::default_schema(search_path).to_string(), seq_name)?.full)
}

pub(crate) async fn eval_expr_with_sequences(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    last_sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    expr: &Expr,
    row: Option<&crate::types::Row>,
    schema: Option<&crate::types::TableSchema>,
) -> Result<crate::types::Value> {
    let rewritten = replace_sequence_functions(
        store,
        txn,
        last_sequence_values,
        search_path,
        expr,
        row,
        schema,
    )
    .await?;
    eval_expr(&rewritten, row, schema)
}

pub(crate) async fn eval_expr_join_with_sequences(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    last_sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    expr: &Expr,
    join_ctx: &JoinContext<'_>,
) -> Result<crate::types::Value> {
    let rewritten = replace_sequence_functions_join(
        store,
        txn,
        last_sequence_values,
        search_path,
        expr,
        join_ctx,
    )
    .await?;
    eval_expr_join(&rewritten, join_ctx)
}

pub(crate) fn replace_sequence_functions<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    last_sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    expr: &'a Expr,
    row: Option<&'a crate::types::Row>,
    schema: Option<&'a crate::types::TableSchema>,
) -> Pin<Box<dyn Future<Output = Result<Expr>> + Send + 'a>> {
    Box::pin(async move {
        match expr {
            Expr::Function(func) => {
                let name = function_name_upper(func);
                match name.as_str() {
                    "CURRENT_SCHEMA" => Ok(value_to_sql_expr(&crate::types::Value::Text(
                        names::default_schema(search_path).to_string(),
                    ))),
                    "NEXTVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            search_path,
                            eval_expr(arg0, row, schema)?,
                        )
                        .await?;
                        let val = store.nextval_sequence(txn, &full_name).await?;
                        last_sequence_values.insert(full_name, val);
                        Ok(value_to_sql_expr(&crate::types::Value::Int64(val)))
                    }
                    "CURRVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            search_path,
                            eval_expr(arg0, row, schema)?,
                        )
                        .await?;
                        if store.get_sequence(txn, &full_name).await?.is_none() {
                            return Err(anyhow!("Sequence '{}' does not exist", full_name));
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
                        Ok(value_to_sql_expr(&crate::types::Value::Int64(val)))
                    }
                    "SETVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let arg1 = extract_arg_expr(&func.args, 1)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            search_path,
                            eval_expr(arg0, row, schema)?,
                        )
                        .await?;
                        let val = eval_expr(arg1, row, schema)?;
                        let value_i64 = match val {
                            crate::types::Value::Int32(n) => n as i64,
                            crate::types::Value::Int64(n) => n,
                            crate::types::Value::Float64(n) => n as i64,
                            crate::types::Value::Text(s) => s
                                .trim()
                                .parse::<i64>()
                                .map_err(|_| anyhow!("setval: value must be integer, got {}", s))?,
                            other => {
                                return Err(anyhow!("setval: value must be integer, got {}", other))
                            }
                        };
                        let is_called = if func.args.len() >= 3 {
                            let arg2 = extract_arg_expr(&func.args, 2)?;
                            match eval_expr(arg2, row, schema)? {
                                crate::types::Value::Boolean(b) => b,
                                crate::types::Value::Text(s) => {
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
                            .setval_sequence(txn, &full_name, value_i64, is_called)
                            .await?;
                        Ok(value_to_sql_expr(&crate::types::Value::Int64(res)))
                    }
                    _ => {
                        let mut resolved_args = Vec::with_capacity(func.args.len());
                        for arg in &func.args {
                            let resolved_arg = match arg {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                        replace_sequence_functions(
                                            store,
                                            txn,
                                            last_sequence_values,
                                            search_path,
                                            e,
                                            row,
                                            schema,
                                        )
                                        .await?,
                                    ))
                                }
                                other => other.clone(),
                            };
                            resolved_args.push(resolved_arg);
                        }
                        let resolved_filter = if let Some(filter) = &func.filter {
                            Some(Box::new(
                                replace_sequence_functions(
                                    store,
                                    txn,
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

pub(crate) fn replace_sequence_functions_join<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    last_sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    expr: &'a Expr,
    join_ctx: &'a JoinContext<'a>,
) -> Pin<Box<dyn Future<Output = Result<Expr>> + Send + 'a>> {
    Box::pin(async move {
        match expr {
            Expr::Function(func) => {
                let name = function_name_upper(func);
                match name.as_str() {
                    "CURRENT_SCHEMA" => Ok(value_to_sql_expr(&crate::types::Value::Text(
                        names::default_schema(search_path).to_string(),
                    ))),
                    "NEXTVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            search_path,
                            eval_expr_join(arg0, join_ctx)?,
                        )
                        .await?;
                        let val = store.nextval_sequence(txn, &full_name).await?;
                        last_sequence_values.insert(full_name, val);
                        Ok(value_to_sql_expr(&crate::types::Value::Int64(val)))
                    }
                    "CURRVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            search_path,
                            eval_expr_join(arg0, join_ctx)?,
                        )
                        .await?;
                        if store.get_sequence(txn, &full_name).await?.is_none() {
                            return Err(anyhow!("Sequence '{}' does not exist", full_name));
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
                        Ok(value_to_sql_expr(&crate::types::Value::Int64(val)))
                    }
                    "SETVAL" => {
                        let arg0 = extract_arg_expr(&func.args, 0)?;
                        let arg1 = extract_arg_expr(&func.args, 1)?;
                        let full_name = resolve_sequence_full_name_from_value(
                            store,
                            txn,
                            search_path,
                            eval_expr_join(arg0, join_ctx)?,
                        )
                        .await?;
                        let val = eval_expr_join(arg1, join_ctx)?;
                        let value_i64 = match val {
                            crate::types::Value::Int32(n) => n as i64,
                            crate::types::Value::Int64(n) => n,
                            crate::types::Value::Float64(n) => n as i64,
                            crate::types::Value::Text(s) => s
                                .trim()
                                .parse::<i64>()
                                .map_err(|_| anyhow!("setval: value must be integer, got {}", s))?,
                            other => {
                                return Err(anyhow!("setval: value must be integer, got {}", other))
                            }
                        };
                        let is_called = if func.args.len() >= 3 {
                            let arg2 = extract_arg_expr(&func.args, 2)?;
                            match eval_expr_join(arg2, join_ctx)? {
                                crate::types::Value::Boolean(b) => b,
                                crate::types::Value::Text(s) => matches!(
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
                            .setval_sequence(txn, &full_name, value_i64, is_called)
                            .await?;
                        Ok(value_to_sql_expr(&crate::types::Value::Int64(res)))
                    }
                    _ => {
                        let mut resolved_args = Vec::with_capacity(func.args.len());
                        for arg in &func.args {
                            let resolved_arg = match arg {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                        replace_sequence_functions_join(
                                            store,
                                            txn,
                                            last_sequence_values,
                                            search_path,
                                            e,
                                            join_ctx,
                                        )
                                        .await?,
                                    ))
                                }
                                other => other.clone(),
                            };
                            resolved_args.push(resolved_arg);
                        }

                        let resolved_filter = if let Some(filter) = &func.filter {
                            Some(Box::new(
                                replace_sequence_functions_join(
                                    store,
                                    txn,
                                    last_sequence_values,
                                    search_path,
                                    filter,
                                    join_ctx,
                                )
                                .await?,
                            ))
                        } else {
                            None
                        };

                        Ok(Expr::Function(Function {
                            name: func.name.clone(),
                            args: resolved_args,
                            filter: resolved_filter,
                            null_treatment: func.null_treatment.clone(),
                            over: func.over.clone(),
                            distinct: func.distinct,
                            special: func.special,
                            order_by: func.order_by.clone(),
                        }))
                    }
                }
            }
            Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
                left: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        left,
                        join_ctx,
                    )
                    .await?,
                ),
                op: op.clone(),
                right: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        right,
                        join_ctx,
                    )
                    .await?,
                ),
            }),
            Expr::UnaryOp { op, expr: inner } => Ok(Expr::UnaryOp {
                op: op.clone(),
                expr: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        inner,
                        join_ctx,
                    )
                    .await?,
                ),
            }),
            Expr::Nested(inner) => Ok(Expr::Nested(Box::new(
                replace_sequence_functions_join(
                    store,
                    txn,
                    last_sequence_values,
                    search_path,
                    inner,
                    join_ctx,
                )
                .await?,
            ))),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let resolved_expr = replace_sequence_functions_join(
                    store,
                    txn,
                    last_sequence_values,
                    search_path,
                    expr,
                    join_ctx,
                )
                .await?;
                let mut resolved_list = Vec::with_capacity(list.len());
                for item in list {
                    resolved_list.push(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            item,
                            join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
                    )
                    .await?,
                ),
                negated: *negated,
                low: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        low,
                        join_ctx,
                    )
                    .await?,
                ),
                high: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        high,
                        join_ctx,
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
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            op,
                            join_ctx,
                        )
                        .await?,
                    ))
                } else {
                    None
                };
                let mut resolved_conditions = Vec::with_capacity(conditions.len());
                for cond in conditions {
                    resolved_conditions.push(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            cond,
                            join_ctx,
                        )
                        .await?,
                    );
                }
                let mut resolved_results = Vec::with_capacity(results.len());
                for res in results {
                    resolved_results.push(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            res,
                            join_ctx,
                        )
                        .await?,
                    );
                }
                let resolved_else = if let Some(else_expr) = else_result {
                    Some(Box::new(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            else_expr,
                            join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
                    )
                    .await?,
                ),
                substring_from: match substring_from {
                    Some(e) => Some(Box::new(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            e,
                            join_ctx,
                        )
                        .await?,
                    )),
                    None => None,
                },
                substring_for: match substring_for {
                    Some(e) => Some(Box::new(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            e,
                            join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
                    )
                    .await?,
                ),
                trim_where: trim_where.clone(),
                trim_what: match trim_what {
                    Some(e) => Some(Box::new(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            e,
                            join_ctx,
                        )
                        .await?,
                    )),
                    None => None,
                },
                trim_characters: trim_characters.clone(),
            }),
            Expr::Position { expr, r#in } => Ok(Expr::Position {
                expr: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
                    )
                    .await?,
                ),
                r#in: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        r#in,
                        join_ctx,
                    )
                    .await?,
                ),
            }),
            Expr::Extract { field, expr } => Ok(Expr::Extract {
                field: *field,
                expr: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
                    )
                    .await?,
                ),
            }),
            Expr::Ceil { expr, field } => Ok(Expr::Ceil {
                expr: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
                    )
                    .await?,
                ),
                field: *field,
            }),
            Expr::Floor { expr, field } => Ok(Expr::Floor {
                expr: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        expr,
                        join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        left,
                        join_ctx,
                    )
                    .await?,
                ),
                operator: *operator,
                right: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        right,
                        join_ctx,
                    )
                    .await?,
                ),
            }),
            Expr::Array(arr) => {
                let mut elems = Vec::with_capacity(arr.elem.len());
                for elem in &arr.elem {
                    elems.push(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            elem,
                            join_ctx,
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
                let resolved_obj = replace_sequence_functions_join(
                    store,
                    txn,
                    last_sequence_values,
                    search_path,
                    obj,
                    join_ctx,
                )
                .await?;
                let mut resolved_indexes = Vec::with_capacity(indexes.len());
                for idx in indexes {
                    resolved_indexes.push(
                        replace_sequence_functions_join(
                            store,
                            txn,
                            last_sequence_values,
                            search_path,
                            idx,
                            join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        left,
                        join_ctx,
                    )
                    .await?,
                ),
                compare_op: compare_op.clone(),
                right: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        right,
                        join_ctx,
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
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        left,
                        join_ctx,
                    )
                    .await?,
                ),
                compare_op: compare_op.clone(),
                right: Box::new(
                    replace_sequence_functions_join(
                        store,
                        txn,
                        last_sequence_values,
                        search_path,
                        right,
                        join_ctx,
                    )
                    .await?,
                ),
            }),
            _ => Ok(expr.clone()),
        }
    })
}
