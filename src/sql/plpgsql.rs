//! PL/pgSQL function execution
//!
//! Supports: DECLARE, BEGIN/END, RETURN, IF/THEN/ELSIF/ELSE/END IF, RAISE, assignment (:=)

use crate::storage::TikvStore;
use crate::types::{DataType, FunctionDef, Value};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::Transaction;

use super::names;
use super::parse_sql;
use super::sequences;

pub struct PlpgsqlContext {
    pub variables: HashMap<String, Value>,
    pub variable_types: HashMap<String, DataType>,
    pub notices: Vec<String>,
}

impl PlpgsqlContext {
    pub fn new() -> Self {
        Self {
            variables: HashMap::new(),
            variable_types: HashMap::new(),
            notices: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn get_var(&self, name: &str) -> Option<&Value> {
        let name_lower = name.to_lowercase();
        self.variables
            .iter()
            .find(|(k, _)| k.to_lowercase() == name_lower)
            .map(|(_, v)| v)
    }

    pub fn set_var(&mut self, name: &str, value: Value) {
        let name_lower = name.to_lowercase();
        let key = self
            .variables
            .keys()
            .find(|k| k.to_lowercase() == name_lower)
            .cloned()
            .unwrap_or_else(|| name_lower);
        self.variables.insert(key, value);
    }
}

fn parse_plpgsql_type(type_str: &str) -> DataType {
    let t = type_str.trim().to_lowercase();
    match t.as_str() {
        "integer" | "int" | "int4" => DataType::Int32,
        "bigint" | "int8" => DataType::Int64,
        "smallint" | "int2" => DataType::Int32,
        "boolean" | "bool" => DataType::Boolean,
        "text" | "varchar" | "character varying" => DataType::Text,
        "real" | "float4" | "double precision" | "float8" | "float" => DataType::Float64,
        "timestamp"
        | "timestamptz"
        | "timestamp with time zone"
        | "timestamp without time zone" => DataType::Timestamp,
        "date" => DataType::Date,
        "uuid" => DataType::Uuid,
        "json" | "jsonb" => DataType::Json,
        "bytea" => DataType::Bytes,
        _ if t.starts_with("varchar") || t.starts_with("character") => DataType::Text,
        _ if t.starts_with("numeric") || t.starts_with("decimal") => DataType::Float64,
        _ => DataType::Text,
    }
}

fn parse_declare_block(
    body: &str,
) -> Result<(
    HashMap<String, Option<String>>,
    HashMap<String, DataType>,
    &str,
)> {
    let body_upper = body.to_uppercase();
    let mut var_defaults: HashMap<String, Option<String>> = HashMap::new();
    let mut types = HashMap::new();

    let declare_pos = body_upper.find("DECLARE");
    let begin_pos = body_upper
        .find("BEGIN")
        .ok_or_else(|| anyhow!("PL/pgSQL function must have BEGIN block"))?;

    let rest = if let Some(decl_pos) = declare_pos {
        if decl_pos < begin_pos {
            let decl_section = &body[decl_pos + 7..begin_pos];
            for line in decl_section.lines() {
                let line = line.trim().trim_end_matches(';');
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
                if parts.len() >= 2 {
                    let var_name = parts[0].trim().to_lowercase();
                    let rest = parts[1].trim();

                    let (type_str, default_expr) =
                        if let Some(def_pos) = rest.to_uppercase().find("DEFAULT") {
                            (
                                rest[..def_pos].trim(),
                                Some(rest[def_pos + 7..].trim().to_string()),
                            )
                        } else if let Some(assign_pos) = rest.find(":=") {
                            (
                                rest[..assign_pos].trim(),
                                Some(rest[assign_pos + 2..].trim().to_string()),
                            )
                        } else {
                            (rest, None)
                        };

                    let data_type = parse_plpgsql_type(type_str);
                    types.insert(var_name.clone(), data_type);
                    var_defaults.insert(var_name, default_expr);
                }
            }
        }
        &body[begin_pos..]
    } else {
        &body[begin_pos..]
    };

    Ok((var_defaults, types, rest))
}

fn parse_literal_value(s: &str, _data_type: &DataType) -> Result<Value> {
    let s = s.trim();

    if s.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if s.eq_ignore_ascii_case("true") {
        return Ok(Value::Boolean(true));
    }
    if s.eq_ignore_ascii_case("false") {
        return Ok(Value::Boolean(false));
    }
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        let inner = &s[1..s.len() - 1];
        let unescaped = inner.replace("''", "'").replace("\\\"", "\"");
        return Ok(Value::Text(unescaped));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Value::Int64(i));
    }
    if let Ok(f) = s.parse::<f64>() {
        return Ok(Value::Float64(f));
    }
    Ok(Value::Text(s.to_string()))
}

#[derive(Debug)]
enum PlpgsqlStatement {
    Return(String),
    Assignment(String, String),
    If(String, Vec<PlpgsqlStatement>, Vec<PlpgsqlStatement>),
    RaiseNotice(String),
    RaiseException(String),
    Sql(String),
    Null,
}

pub fn validate_plpgsql_body(body: &str) -> Result<()> {
    let _ = parse_declare_block(body)?;
    let _ = parse_begin_block(body)?;
    Ok(())
}

fn parse_begin_block(body: &str) -> Result<Vec<PlpgsqlStatement>> {
    let body_upper = body.to_uppercase();
    let begin_pos = body_upper
        .find("BEGIN")
        .ok_or_else(|| anyhow!("Missing BEGIN"))?;
    let end_pos = find_matching_end(&body[begin_pos..])
        .ok_or_else(|| anyhow!("Missing END for BEGIN block"))?;

    let block_content = &body[begin_pos + 5..begin_pos + end_pos];
    parse_statements(block_content)
}

fn find_matching_end(s: &str) -> Option<usize> {
    let s_upper = s.to_uppercase();
    let mut depth = 0;
    let mut i = 0;
    let bytes = s_upper.as_bytes();

    while i < bytes.len() {
        if i + 5 <= bytes.len() && &s_upper[i..i + 5] == "BEGIN" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                if i + 5 == bytes.len() || !bytes[i + 5].is_ascii_alphanumeric() {
                    depth += 1;
                    i += 5;
                    continue;
                }
            }
        }
        if i + 2 <= bytes.len() && &s_upper[i..i + 2] == "IF" {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (i + 2 == bytes.len() || !bytes[i + 2].is_ascii_alphanumeric())
            {
                depth += 1;
                i += 2;
                continue;
            }
        }
        if i + 6 <= bytes.len() && &s_upper[i..i + 6] == "END IF" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                depth -= 1;
                i += 6;
                continue;
            }
        }
        if i + 3 <= bytes.len() && &s_upper[i..i + 3] == "END" {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (i + 3 == bytes.len()
                    || !bytes[i + 3].is_ascii_alphanumeric()
                    || (i + 4 <= bytes.len() && bytes[i + 3] == b';'))
            {
                let rest = s_upper[i + 3..].trim_start();
                if !rest.starts_with("IF") {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn parse_statements(content: &str) -> Result<Vec<PlpgsqlStatement>> {
    let mut statements = Vec::new();
    let content = content.trim();

    if content.is_empty() {
        return Ok(statements);
    }

    let mut remaining = content;
    while !remaining.trim().is_empty() {
        remaining = remaining.trim();

        let remaining_upper = remaining.to_uppercase();
        if remaining_upper.starts_with("IF ")
            || remaining_upper.starts_with("IF\n")
        {
            let (if_stmt, rest) = parse_if_statement(remaining)?;
            statements.push(if_stmt);
            remaining = rest;
            continue;
        }

        if remaining_upper.starts_with("ELSIF ")
            || remaining_upper.starts_with("ELSIF\n")
        {
            let synthetic_if = format!("IF{} END IF", &remaining[5..]);
            let (if_stmt, _rest) = parse_if_statement(&synthetic_if)?;
            statements.push(if_stmt);
            break;
        }

        if let Some(semi_pos) = find_statement_end(remaining) {
            let stmt_str = remaining[..semi_pos].trim();
            if !stmt_str.is_empty() {
                statements.push(parse_single_statement(stmt_str)?);
            }
            remaining = &remaining[semi_pos + 1..];
        } else {
            if !remaining.is_empty() {
                statements.push(parse_single_statement(remaining)?);
            }
            break;
        }
    }

    Ok(statements)
}

fn find_statement_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut i = 0;

    while i < bytes.len() {
        if in_string {
            if bytes[i] == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_string = false;
            }
        } else {
            match bytes[i] {
                b'\'' => in_string = true,
                b';' => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn parse_single_statement(s: &str) -> Result<PlpgsqlStatement> {
    let s = s.trim();
    let s_upper = s.to_uppercase();

    if s_upper == "NULL" {
        return Ok(PlpgsqlStatement::Null);
    }

    if s_upper.starts_with("RETURN ") || s_upper == "RETURN" {
        let expr = if s.len() > 7 { s[7..].trim() } else { "" };
        return Ok(PlpgsqlStatement::Return(expr.to_string()));
    }

    if s_upper.starts_with("CALL ") {
        return Err(anyhow!("CALL is not allowed in a function"));
    }

    if s_upper.starts_with("RAISE ") {
        let rest = s[6..].trim();
        let rest_upper = rest.to_uppercase();
        if rest_upper.starts_with("NOTICE ") {
            return Ok(PlpgsqlStatement::RaiseNotice(rest[7..].trim().to_string()));
        }
        if rest_upper.starts_with("EXCEPTION ") {
            return Ok(PlpgsqlStatement::RaiseException(
                rest[10..].trim().to_string(),
            ));
        }
        return Ok(PlpgsqlStatement::RaiseException(rest.to_string()));
    }

    if let Some(assign_pos) = s.find(":=") {
        let var_name = s[..assign_pos].trim().to_string();
        let expr = s[assign_pos + 2..].trim().to_string();
        return Ok(PlpgsqlStatement::Assignment(var_name, expr));
    }

    Ok(PlpgsqlStatement::Sql(s.to_string()))
}

fn parse_if_statement(s: &str) -> Result<(PlpgsqlStatement, &str)> {
    let s_upper = s.to_uppercase();

    let then_pos = s_upper
        .find(" THEN")
        .or_else(|| s_upper.find("\nTHEN"))
        .ok_or_else(|| anyhow!("IF without THEN"))?;

    let condition = s[2..then_pos].trim().to_string();
    let after_then = &s[then_pos + 5..];

    let (then_block, else_block, rest) = find_if_blocks(after_then)?;

    let then_stmts = parse_statements(then_block)?;
    let else_stmts = if else_block.is_empty() {
        Vec::new()
    } else {
        parse_statements(else_block)?
    };

    Ok((
        PlpgsqlStatement::If(condition, then_stmts, else_stmts),
        rest,
    ))
}

enum ElseBranchType {
    Elsif(usize),
    Else(usize),
}

fn find_if_blocks(s: &str) -> Result<(&str, &str, &str)> {
    let s_upper = s.to_uppercase();
    let mut depth = 1;
    let mut i = 0;
    let bytes = s_upper.as_bytes();
    let mut else_branch: Option<ElseBranchType> = None;
    let mut end_if_pos: Option<usize> = None;

    while i < bytes.len() {
        if i + 3 <= bytes.len()
            && &s_upper[i..i + 2] == "IF"
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && !bytes[i + 2].is_ascii_alphanumeric()
        {
            depth += 1;
            i += 2;
            continue;
        }

        if depth == 1
            && i + 5 <= bytes.len()
            && &s_upper[i..i + 5] == "ELSIF"
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            if else_branch.is_none() {
                else_branch = Some(ElseBranchType::Elsif(i));
            }
            i += 5;
            continue;
        }

        if depth == 1
            && i + 4 <= bytes.len()
            && &s_upper[i..i + 4] == "ELSE"
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
        {
            if else_branch.is_none() {
                else_branch = Some(ElseBranchType::Else(i));
            }
            i += 4;
            continue;
        }

        if i + 6 <= bytes.len() && &s_upper[i..i + 6] == "END IF" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                depth -= 1;
                if depth == 0 {
                    end_if_pos = Some(i);
                    break;
                }
            }
            i += 6;
            continue;
        }

        i += 1;
    }

    let end_if_pos = end_if_pos.ok_or_else(|| anyhow!("IF without END IF"))?;

    let (then_block, else_block) = match else_branch {
        Some(ElseBranchType::Elsif(pos)) => {
            let then_block = &s[..pos];
            let elsif_rest = &s[pos..end_if_pos];
            (then_block, elsif_rest)
        }
        Some(ElseBranchType::Else(pos)) => (&s[..pos], &s[pos + 4..end_if_pos]),
        None => (&s[..end_if_pos], ""),
    };

    let rest_start = end_if_pos + 6;
    let rest = s[rest_start..].trim_start();
    let rest = if rest.starts_with(';') {
        &rest[1..]
    } else {
        rest
    };

    Ok((then_block.trim(), else_block.trim(), rest))
}

pub fn execute_plpgsql_function<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    func_def: &'a FunctionDef,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        let mut ctx = PlpgsqlContext::new();

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
                evaluate_expression(store, txn, db_id, sequence_values, search_path, &ctx, &expr_str)
                    .await?
            } else {
                Value::Null
            };
            ctx.variables.insert(name, value);
        }

        let statements = parse_begin_block(body)?;
        execute_statements(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            &mut ctx,
            &statements,
        )
        .await
    })
}

fn is_type_keyword(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "integer"
            | "int"
            | "int4"
            | "int8"
            | "bigint"
            | "smallint"
            | "int2"
            | "boolean"
            | "bool"
            | "text"
            | "varchar"
            | "character"
            | "real"
            | "float4"
            | "float8"
            | "float"
            | "double"
            | "numeric"
            | "decimal"
            | "timestamp"
            | "timestamptz"
            | "date"
            | "uuid"
            | "json"
            | "jsonb"
            | "bytea"
            | "trigger"
            | "void"
    )
}

fn execute_statements<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    ctx: &'a mut PlpgsqlContext,
    statements: &'a [PlpgsqlStatement],
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

                PlpgsqlStatement::Sql(_sql) => {}

                PlpgsqlStatement::Null => {}
            }
        }
        Ok(Value::Null)
    })
}

fn format_raise_message(_ctx: &PlpgsqlContext, msg: &str) -> String {
    let msg = msg.trim();
    if (msg.starts_with('\'') && msg.ends_with('\''))
        || (msg.starts_with('"') && msg.ends_with('"'))
    {
        msg[1..msg.len() - 1].to_string()
    } else {
        msg.to_string()
    }
}

fn substitute_variables(ctx: &PlpgsqlContext, s: &str) -> String {
    let mut result = s.to_string();
    for (name, value) in &ctx.variables {
        let value_str = match value {
            Value::Null => "NULL".to_string(),
            Value::Text(t) => format!("'{}'", t.replace('\'', "''")),
            v => v.to_string(),
        };
        result = replace_identifier(&result, name, &value_str);
    }
    result
}

fn replace_identifier(s: &str, name: &str, replacement: &str) -> String {
    if name.is_empty() {
        return s.to_string();
    }

    let bytes = s.as_bytes();
    let name_bytes = name.as_bytes();
    let mut i = 0;
    let mut in_string = false;

    let mut result = Vec::with_capacity(bytes.len());
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if in_string {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    result.push(bytes[i]);
                    result.push(bytes[i + 1]);
                    i += 2;
                    continue;
                }
                in_string = false;
            } else {
                in_string = true;
            }
            result.push(bytes[i]);
            i += 1;
            continue;
        }

        if in_string {
            result.push(bytes[i]);
            i += 1;
            continue;
        }

        if i + name_bytes.len() <= bytes.len()
            && bytes[i..i + name_bytes.len()].eq_ignore_ascii_case(name_bytes)
        {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + name_bytes.len() == bytes.len()
                || !is_ident_char(bytes[i + name_bytes.len()]);

            if before_ok && after_ok {
                result.extend_from_slice(replacement.as_bytes());
                i += name_bytes.len();
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(result).unwrap_or_else(|_| s.to_string())
}

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

async fn evaluate_expression(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    ctx: &PlpgsqlContext,
    expr_str: &str,
) -> Result<Value> {
    let expanded = substitute_variables(ctx, expr_str);
    let sql = format!("SELECT {}", expanded);
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

    parse_literal_value(&expanded, &DataType::Text)
}

pub async fn try_execute_user_function(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    func_name: &str,
    args: Vec<Value>,
) -> Result<Option<Value>> {
    let func_obj = names::object_name_from_str(func_name)?;
    let resolved =
        names::resolve_existing_function_name(store.as_ref(), txn, db_id, &func_obj, search_path).await?;

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
                    )
                    .await
                    .map(Some);
                }
            }
            return Ok(None);
        }
    };

    execute_user_function_by_name(store, txn, db_id, sequence_values, search_path, &full_name, args)
        .await
        .map(Some)
}

async fn execute_user_function_by_name(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    full_name: &str,
    args: Vec<Value>,
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
        return execute_sql_function(store, txn, db_id, sequence_values, search_path, &func_def, args)
            .await;
    }

    execute_plpgsql_function(store, txn, db_id, sequence_values, search_path, &func_def, args).await
}

async fn execute_sql_function(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
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
            Value::Text(t) => format!("'{}'", t.replace('\'', "''")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_identifier_respects_word_boundaries() {
        assert_eq!(replace_identifier("n + 1", "n", "5"), "5 + 1");
        assert_eq!(replace_identifier("nn + n", "n", "5"), "nn + 5");
    }

    #[test]
    fn test_replace_identifier_preserves_utf8() {
        let input = "'你好' || n";
        let output = replace_identifier(input, "n", "5");
        assert_eq!(output, "'你好' || 5");
    }

    #[test]
    fn test_replace_identifier_skips_string_literals() {
        assert_eq!(
            replace_identifier("'negative' || n", "n", "5"),
            "'negative' || 5"
        );
        assert_eq!(
            replace_identifier("CASE WHEN n < 0 THEN 'negative' END", "n", "-5"),
            "CASE WHEN -5 < 0 THEN 'negative' END"
        );
        assert_eq!(
            replace_identifier("'it''s a test' || n", "n", "5"),
            "'it''s a test' || 5"
        );
    }
}
