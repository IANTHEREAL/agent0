//! PL/pgSQL execution: `execute_plpgsql_function`, `execute_statements`, expression evaluation,
//! and user function dispatch (`try_execute_user_function`).

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::Transaction;

use crate::model::{DataType, FunctionDef, Value};
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::parse_sql;
use crate::sql::quoting;
use crate::sql::raw_sql::{classify, RawSqlKind};
use crate::sql::sequences;
use crate::sql::ExecuteResult;
use crate::sql::Executor;
use crate::storage::TikvStore;

use super::ast_bind;
use super::parser::{parse_begin_block, parse_declare_block, PlpgsqlStatement};
use super::utils::{
    consume_exit_signal, format_raise_message, has_exit_signal, is_type_keyword,
    parse_literal_value, parse_plpgsql_type, replace_identifier, set_exit_signal,
    substitute_variables,
};
use super::PlpgsqlContext;
use crate::sql::sequences::SequenceSession;

/// Execute a PL/pgSQL function body with the given arguments.
pub fn execute_plpgsql_function<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut SequenceSession,
    search_path: &'a [String],
    func_def: &'a FunctionDef,
    args: Vec<Value>,
    executor: Option<&'a Executor>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        let mut ctx = PlpgsqlContext::new();
        ctx.function_name = func_def.name.to_lowercase();

        // SECURITY DEFINER: set the role override so all statements inside
        // this function execute with the owner's identity for RLS evaluation.
        if func_def.security_definer {
            ctx.security_definer_role = Some(func_def.owner.clone());
        }

        for (i, arg_type) in func_def.arg_types.iter().enumerate() {
            let parts: Vec<&str> = arg_type.split_whitespace().collect();
            let (param_name, param_type) = if parts.len() >= 2 && !is_type_keyword(parts[0]) {
                (parts[0].to_lowercase(), parts[1..].join(" "))
            } else {
                (format!("${}", i + 1), arg_type.clone())
            };

            let data_type = parse_plpgsql_type(&param_type);
            ctx.variable_types.insert(param_name.clone(), data_type);

            if i < args.len() {
                ctx.variables.insert(param_name, args[i].clone());
            } else {
                ctx.variables.insert(param_name, Value::Null);
            }
        }

        let body = &func_def.body;
        let (var_defaults, types, _rest) = parse_declare_block(body)?;
        for (name, dt) in types {
            ctx.variable_types.insert(name, dt);
        }
        for (name, default_expr) in var_defaults {
            let value = if let Some(expr_str) = default_expr {
                evaluate_expression(
                    store,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &ctx,
                    &expr_str,
                    executor,
                )
                .await?
            } else {
                Value::Null
            };
            ctx.variables.insert(name, value);
        }

        let declared_vars: HashSet<String> = ctx
            .variable_types
            .keys()
            .map(|k| k.to_lowercase())
            .collect();
        let statements = parse_begin_block(body, &declared_vars)?;
        execute_statements(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            &mut ctx,
            &statements,
            executor,
        )
        .await
    })
}

fn execute_statements<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut SequenceSession,
    search_path: &'a [String],
    ctx: &'a mut PlpgsqlContext,
    statements: &'a [PlpgsqlStatement],
    executor: Option<&'a Executor>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        for stmt in statements {
            match stmt {
                PlpgsqlStatement::Return(expr_str) => {
                    if expr_str.is_empty() {
                        return Ok(Value::Null);
                    }
                    return evaluate_expression(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctx,
                        expr_str,
                        executor,
                    )
                    .await;
                }

                PlpgsqlStatement::Assignment(var_name, expr_str) => {
                    let value = evaluate_expression(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctx,
                        expr_str,
                        executor,
                    )
                    .await?;
                    ctx.set_var(var_name, value);
                }

                PlpgsqlStatement::If(condition, then_stmts, else_stmts) => {
                    let cond_value = evaluate_expression(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctx,
                        condition,
                        executor,
                    )
                    .await?;
                    let is_true = match cond_value {
                        Value::Boolean(b) => b,
                        Value::Null => false,
                        Value::Int32(n) => n != 0,
                        Value::Int64(n) => n != 0,
                        _ => true,
                    };

                    if is_true {
                        let result = execute_statements(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctx,
                            then_stmts,
                            executor,
                        )
                        .await?;
                        if !matches!(result, Value::Null)
                            || then_stmts
                                .iter()
                                .any(|s| matches!(s, PlpgsqlStatement::Return(_)))
                        {
                            return Ok(result);
                        }
                    } else if !else_stmts.is_empty() {
                        let result = execute_statements(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctx,
                            else_stmts,
                            executor,
                        )
                        .await?;
                        if !matches!(result, Value::Null)
                            || else_stmts
                                .iter()
                                .any(|s| matches!(s, PlpgsqlStatement::Return(_)))
                        {
                            return Ok(result);
                        }
                    }
                }

                PlpgsqlStatement::RaiseNotice(msg) => {
                    ctx.notices.push(format_raise_message(ctx, msg));
                }

                PlpgsqlStatement::RaiseException(msg) => {
                    return Err(anyhow!("{}", format_raise_message(ctx, msg)));
                }

                PlpgsqlStatement::Sql(sql) => {
                    let exec = executor
                        .ok_or_else(|| anyhow!("SQL statement requires execution context"))?;

                    // Check for CREATE TYPE ... AS ENUM (needs text form for classify).
                    // NOTE: This path still uses text substitution for both classification
                    // and execution. CREATE TYPE ENUM bodies don't reference table columns,
                    // so text substitution is safe here. Plan A does not use this path.
                    let expanded_for_classify = substitute_variables(ctx, sql);
                    let raw_trimmed = expanded_for_classify.trim().trim_end_matches(';').trim();
                    let raw_upper = raw_trimmed.to_ascii_uppercase();
                    if matches!(classify(&raw_upper), Some(RawSqlKind::CreateTypeEnum)) {
                        let _ = exec
                            .execute_create_type_enum_on_txn(txn, db_id, search_path, raw_trimmed)
                            .await?;
                        continue;
                    }

                    // AST-aware binding: parse raw SQL first, then bind variables in AST.
                    // No fallback to text substitution — parse/bind failure is an error.
                    let stmts = ast_bind::bind_sql_statements(sql, ctx)?;
                    let expanded = stmts
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join("; ");
                    let mut create_index_with_params =
                        crate::sql::extract_create_index_with_params(&expanded).into_iter();
                    for stmt in stmts {
                        let with_params =
                            if matches!(&stmt, sqlparser::ast::Statement::CreateIndex { .. }) {
                                create_index_with_params.next().flatten()
                            } else {
                                None
                            };
                        exec.execute_statement_on_txn_with_create_index_with_params(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &stmt,
                            with_params.as_deref(),
                            ctx.security_definer_role.as_deref(),
                            None,
                        )
                        .await?;
                    }
                }

                PlpgsqlStatement::Perform(query) => {
                    let exec =
                        executor.ok_or_else(|| anyhow!("PERFORM requires execution context"))?;
                    let select_sql = format!("SELECT {}", query);
                    let stmts = ast_bind::bind_sql_statements(&select_sql, ctx)?;
                    for stmt in stmts {
                        let _ = exec
                            .execute_statement_on_txn(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &stmt,
                                ctx.security_definer_role.as_deref(),
                                None,
                            )
                            .await?;
                    }
                }

                PlpgsqlStatement::DmlReturningInto { sql, variables } => {
                    let exec = executor
                        .ok_or_else(|| anyhow!("DML RETURNING INTO requires execution context"))?;

                    let stmts = ast_bind::bind_sql_statements(sql, ctx)?;
                    if let Some(stmt) = stmts.into_iter().next() {
                        let result = exec
                            .execute_statement_on_txn(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &stmt,
                                ctx.security_definer_role.as_deref(),
                                None,
                            )
                            .await?;
                        if let ExecuteResult::Select { rows, .. } = result {
                            let row_count = rows.len();
                            if row_count > 1 {
                                return Err(anyhow!("query returned more than one row"));
                            }
                            if let Some(row) = rows.into_iter().next() {
                                for (i, var_name) in variables.iter().enumerate() {
                                    let val = row.values.get(i).cloned().unwrap_or(Value::Null);
                                    ctx.set_var(var_name, val);
                                }
                            } else {
                                for var_name in variables {
                                    ctx.set_var(var_name, Value::Null);
                                }
                            }
                        }
                    }
                }

                PlpgsqlStatement::SelectInto {
                    variables,
                    query,
                    strict,
                } => {
                    let stmts = ast_bind::bind_sql_statements(query, ctx)?;
                    if let Some(exec) = executor {
                        if let Some(stmt) = stmts.into_iter().next() {
                            let result = exec
                                .execute_statement_on_txn(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &stmt,
                                    ctx.security_definer_role.as_deref(),
                                    None,
                                )
                                .await?;
                            if let ExecuteResult::Select { rows, .. } = result {
                                let row_count = rows.len();
                                if *strict && row_count == 0 {
                                    return Err(SqlError::NoDataFound.into());
                                }
                                if *strict && row_count > 1 {
                                    return Err(SqlError::TooManyRows.into());
                                }
                                if let Some(row) = rows.into_iter().next() {
                                    for (i, var_name) in variables.iter().enumerate() {
                                        let val = row.values.get(i).cloned().unwrap_or(Value::Null);
                                        ctx.set_var(var_name, val);
                                    }
                                } else {
                                    for var_name in variables {
                                        ctx.set_var(var_name, Value::Null);
                                    }
                                }
                            }
                        }
                    } else {
                        return Err(anyhow!("SELECT INTO requires SQL execution context"));
                    }
                }

                PlpgsqlStatement::ForQuery {
                    variable,
                    query,
                    body,
                } => {
                    let stmts = ast_bind::bind_sql_statements(query, ctx)?;
                    if let Some(exec) = executor {
                        if let Some(stmt) = stmts.into_iter().next() {
                            let result = exec
                                .execute_statement_on_txn(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &stmt,
                                    ctx.security_definer_role.as_deref(),
                                    None,
                                )
                                .await?;
                            if let ExecuteResult::Select { columns, rows, .. } = result {
                                for row in rows {
                                    for (i, col) in columns.iter().enumerate() {
                                        let val = row.values.get(i).cloned().unwrap_or(Value::Null);
                                        let field_name = format!("{}.{}", variable, col);
                                        ctx.set_var(&field_name, val);
                                    }

                                    let body_result = execute_statements(
                                        store,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        ctx,
                                        body,
                                        executor,
                                    )
                                    .await?;

                                    if consume_exit_signal(ctx) {
                                        break;
                                    }

                                    if !matches!(body_result, Value::Null)
                                        || body
                                            .iter()
                                            .any(|s| matches!(s, PlpgsqlStatement::Return(_)))
                                    {
                                        return Ok(body_result);
                                    }
                                }
                            }
                        }
                    } else {
                        return Err(anyhow!("FOR loop requires SQL execution context"));
                    }
                }

                PlpgsqlStatement::ForRange {
                    variable,
                    start_expr,
                    end_expr,
                    step_expr,
                    reverse,
                    body,
                } => {
                    let start_val = evaluate_expression(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctx,
                        start_expr,
                        executor,
                    )
                    .await?;
                    let end_val = evaluate_expression(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctx,
                        end_expr,
                        executor,
                    )
                    .await?;

                    let start = match start_val {
                        Value::Int32(n) => n as i64,
                        Value::Int64(n) => n,
                        _ => return Err(anyhow!("FOR loop bounds must be integers")),
                    };
                    let end = match end_val {
                        Value::Int32(n) => n as i64,
                        Value::Int64(n) => n,
                        _ => return Err(anyhow!("FOR loop bounds must be integers")),
                    };
                    let step = if let Some(step_str) = step_expr {
                        let step_val = evaluate_expression(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctx,
                            step_str,
                            executor,
                        )
                        .await?;
                        match step_val {
                            Value::Int32(n) => n as i64,
                            Value::Int64(n) => n,
                            _ => return Err(anyhow!("FOR loop step must be integer")),
                        }
                    } else {
                        1i64
                    };

                    if step == 0 {
                        return Err(anyhow!("FOR loop step cannot be zero"));
                    }

                    let mut i = start;
                    loop {
                        if !*reverse {
                            if i > end {
                                break;
                            }
                        } else if i < end {
                            break;
                        }

                        ctx.set_var(variable, Value::Int64(i));
                        let body_result = execute_statements(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctx,
                            body,
                            executor,
                        )
                        .await?;

                        if consume_exit_signal(ctx) {
                            break;
                        }

                        if !matches!(body_result, Value::Null)
                            || body
                                .iter()
                                .any(|s| matches!(s, PlpgsqlStatement::Return(_)))
                        {
                            return Ok(body_result);
                        }

                        if *reverse {
                            i -= step;
                        } else {
                            i += step;
                        }
                    }
                }

                PlpgsqlStatement::Exit => {
                    set_exit_signal(ctx);
                    return Ok(Value::Null);
                }

                PlpgsqlStatement::Null => {}
            }

            if has_exit_signal(ctx) {
                return Ok(Value::Null);
            }
        }
        Ok(Value::Null)
    })
}

async fn evaluate_expression(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    ctx: &PlpgsqlContext,
    expr_str: &str,
    executor: Option<&Executor>,
) -> Result<Value> {
    // AST-aware binding: parse expression first, bind variables, reconstruct SQL.
    // No fallback to text substitution — parse/bind failure is an error.
    let sql = ast_bind::bind_expression(expr_str, ctx)?;

    if let Some(exec) = executor {
        let stmts = parse_sql(&sql)?;
        if let Some(stmt) = stmts.first() {
            let result = exec
                .execute_statement_on_txn(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    stmt,
                    ctx.security_definer_role.as_deref(),
                    None,
                )
                .await?;
            if let ExecuteResult::Select { rows, .. } = result {
                if let Some(first_row) = rows.first() {
                    return Ok(first_row.values.first().cloned().unwrap_or(Value::Null));
                }
                return Ok(Value::Null);
            }
            return Ok(Value::Null);
        }
    }

    if let Ok(stmts) = parse_sql(&sql) {
        if let Some(sqlparser::ast::Statement::Query(query)) = stmts.into_iter().next() {
            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                    select.projection.into_iter().next()
                {
                    return sequences::eval_expr_with_sequences(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &expr,
                        None,
                        None,
                    )
                    .await;
                }
            }
        }
    }

    // Last-resort fallback: strip the "SELECT " prefix and try parsing as a literal
    let expr_part = sql.strip_prefix("SELECT ").unwrap_or(&sql);
    parse_literal_value(expr_part, &DataType::Text)
}

/// Try to look up and execute a user-defined function by name. Returns `Ok(None)` if
/// the function does not exist.
pub async fn try_execute_user_function(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    func_name: &str,
    args: Vec<Value>,
    executor: Option<&'_ Executor>,
) -> Result<Option<Value>> {
    let func_obj = names::object_name_from_str(func_name)?;
    let resolved =
        names::resolve_existing_function_name(store.as_ref(), txn, db_id, &func_obj, search_path)
            .await?;

    let full_name = match resolved {
        Some(r) => r.full,
        None => {
            for schema in search_path {
                let full = format!("{}.{}", schema, func_name.to_lowercase());
                if store.get_function(txn, db_id, &full).await?.is_some() {
                    return execute_user_function_by_name(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &full,
                        args,
                        executor,
                    )
                    .await
                    .map(Some);
                }
            }
            return Ok(None);
        }
    };

    execute_user_function_by_name(
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        &full_name,
        args,
        executor,
    )
    .await
    .map(Some)
}

async fn execute_user_function_by_name(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    full_name: &str,
    args: Vec<Value>,
    executor: Option<&'_ Executor>,
) -> Result<Value> {
    let func_def = store
        .get_function(txn, db_id, full_name)
        .await?
        .ok_or_else(|| anyhow!("Function '{}' does not exist", full_name))?;

    let lang = func_def.language.to_lowercase();
    if lang != "plpgsql" && lang != "sql" {
        return Err(anyhow!(
            "Unsupported function language '{}', only plpgsql and sql are supported",
            func_def.language
        ));
    }

    if lang == "sql" {
        // Always prefer the full executor path for SQL-language functions.
        // The old fast path (execute_sql_function) extracted only the first
        // projection expression and evaluated it via eval_expr_with_sequences,
        // which silently discarded CTEs (WITH), FROM clauses, WHERE clauses,
        // and subqueries — breaking any non-trivial SQL function body.
        // The executor path handles all SQL constructs correctly.
        if let Some(exec) = executor {
            return execute_sql_function_via_executor(
                exec,
                txn,
                db_id,
                sequence_values,
                search_path,
                &func_def,
                args,
            )
            .await;
        }
        // Fallback for call sites without an executor reference (e.g.,
        // sequence expression evaluation). This path only works for trivial
        // scalar expressions like `SELECT 1 + 1`.
        return execute_sql_function(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            &func_def,
            args,
        )
        .await;
    }

    execute_plpgsql_function(
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        &func_def,
        args,
        executor,
    )
    .await
}

async fn execute_sql_function(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    func_def: &FunctionDef,
    args: Vec<Value>,
) -> Result<Value> {
    let mut param_map = HashMap::new();
    for (i, arg_type) in func_def.arg_types.iter().enumerate() {
        let parts: Vec<&str> = arg_type.split_whitespace().collect();
        let param_name = if parts.len() >= 2 && !is_type_keyword(parts[0]) {
            parts[0].to_lowercase()
        } else {
            format!("${}", i + 1)
        };
        if i < args.len() {
            param_map.insert(param_name, args[i].clone());
        }
    }

    let mut sql = func_def.body.clone();
    for (name, value) in &param_map {
        let value_str = match value {
            Value::Null => "NULL".to_string(),
            Value::Text(t) => quoting::quote_literal(t),
            Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            v => v.to_string(),
        };
        sql = replace_identifier(&sql, name, &value_str);
    }

    let stmts = parse_sql(&sql)?;
    for stmt in stmts {
        if let sqlparser::ast::Statement::Query(query) = stmt {
            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                    select.projection.into_iter().next()
                {
                    return sequences::eval_expr_with_sequences(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &expr,
                        None,
                        None,
                    )
                    .await;
                }
            }
        }
    }

    Ok(Value::Null)
}

/// Execute a scalar SQL function through the full executor pipeline so that
/// SECURITY DEFINER role context switching is applied (RLS evaluation uses
/// the function owner's identity).
async fn execute_sql_function_via_executor(
    executor: &Executor,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    func_def: &FunctionDef,
    args: Vec<Value>,
) -> Result<Value> {
    let mut param_map = HashMap::new();
    for (i, arg_type) in func_def.arg_types.iter().enumerate() {
        let parts: Vec<&str> = arg_type.split_whitespace().collect();
        let param_name = if parts.len() >= 2 && !is_type_keyword(parts[0]) {
            parts[0].to_lowercase()
        } else {
            format!("${}", i + 1)
        };
        if i < args.len() {
            param_map.insert(param_name, args[i].clone());
        }
    }

    let mut sql = func_def.body.clone();
    for (name, value) in &param_map {
        let value_str = match value {
            Value::Null => "NULL".to_string(),
            Value::Text(t) => quoting::quote_literal(t),
            Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            v => v.to_string(),
        };
        sql = replace_identifier(&sql, name, &value_str);
    }

    let stmts = parse_sql(&sql)?;
    let current_role = if func_def.security_definer {
        Some(func_def.owner.as_str())
    } else {
        None
    };

    for stmt in &stmts {
        let result = executor
            .execute_statement_on_txn(
                txn,
                db_id,
                sequence_values,
                search_path,
                stmt,
                current_role,
                None,
            )
            .await?;
        if let ExecuteResult::Select { rows, .. } = result {
            if let Some(first_row) = rows.first() {
                return Ok(first_row.values.first().cloned().unwrap_or(Value::Null));
            }
            return Ok(Value::Null);
        }
    }

    Ok(Value::Null)
}
