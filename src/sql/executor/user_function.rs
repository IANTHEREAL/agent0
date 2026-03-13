use crate::model::{FunctionDef, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::plpgsql;
use crate::sql::quoting;
use crate::sql::{parse_sql, ExecuteResult};
use anyhow::{anyhow, Result};
use sqlparser::ast::{FunctionArg, FunctionArgExpr, ObjectName, TableAlias};
use std::collections::HashMap;
use tikv_client::Transaction;

use super::core::Executor;
use crate::sql::expr::bridge::eval_const_ast_expr;
use crate::sql::sequences::SequenceSession;

impl Executor {
    #[allow(clippy::type_complexity)]
    pub(crate) fn try_execute_user_table_function<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        name: &'a ObjectName,
        args: &'a [FunctionArg],
        _alias: Option<&'a TableAlias>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<(TableSchema, Vec<Row>)>>> + Send + 'a>,
    > {
        Box::pin(async move {
            let resolved = names::resolve_existing_function_name(
                self.store().as_ref(),
                txn,
                db_id,
                name,
                search_path,
            )
            .await?;

            let full_name = match resolved {
                Some(r) => r.full,
                None => {
                    let (_schema_opt, obj_name) = names::split_object_name(name)?;
                    let mut found = None;
                    for schema in search_path {
                        let full = format!("{}.{}", schema, obj_name.to_lowercase());
                        if self
                            .store()
                            .get_function(txn, db_id, &full)
                            .await?
                            .is_some()
                        {
                            found = Some(full);
                            break;
                        }
                    }
                    match found {
                        Some(f) => f,
                        None => return Ok(None),
                    }
                }
            };

            let func_def = self
                .store()
                .get_function(txn, db_id, &full_name)
                .await?
                .ok_or_else(|| anyhow!("Function '{}' does not exist", full_name))?;

            let ret_lower = func_def.return_type.to_lowercase();
            if !ret_lower.starts_with("setof ") && !is_returns_table(&ret_lower) {
                return Ok(None);
            }

            let call_args = eval_function_args(args)?;

            let expected_arity = func_def.arg_types.len();
            let actual_arity = call_args.len();
            if actual_arity != expected_arity {
                return Err(anyhow!(
                    "function {}() requires {} argument{}, but {} {} provided",
                    full_name.rsplit('.').next().unwrap_or(&full_name),
                    expected_arity,
                    if expected_arity == 1 { "" } else { "s" },
                    actual_arity,
                    if actual_arity == 1 { "was" } else { "were" },
                ));
            }

            let result = execute_sql_table_function(
                self,
                txn,
                db_id,
                sequence_values,
                search_path,
                &func_def,
                call_args,
            )
            .await?;

            Ok(Some(result))
        })
    }
}

fn eval_function_args(args: &[FunctionArg]) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        let expr = match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
            | FunctionArg::Named {
                arg: FunctionArgExpr::Expr(e),
                ..
            } => e,
            _ => {
                return Err(
                    SqlError::Unsupported("Unsupported function argument type".into()).into(),
                )
            }
        };
        let val = eval_const_ast_expr(expr)?;
        values.push(val);
    }
    Ok(values)
}

async fn execute_sql_table_function(
    executor: &Executor,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut SequenceSession,
    search_path: &[String],
    func_def: &FunctionDef,
    args: Vec<Value>,
) -> Result<(TableSchema, Vec<Row>)> {
    let lang = func_def.language.to_lowercase();
    if lang != "sql" {
        return Err(anyhow!(
            "table functions only support LANGUAGE SQL, got '{}'",
            func_def.language
        ));
    }

    let sql = substitute_params(func_def, &args);

    let stmts = parse_sql(&sql)?;

    let last_select = stmts
        .iter()
        .rfind(|s| matches!(s, sqlparser::ast::Statement::Query(_)));

    let stmt = last_select.ok_or_else(|| {
        anyhow!(
            "table function '{}' body contains no SELECT statement",
            func_def.name
        )
    })?;

    // SECURITY DEFINER: execute using the function owner's role identity so that
    // RLS policies are evaluated against the definer, not the caller.
    let current_role_override = if func_def.security_definer {
        Some(func_def.owner.as_str())
    } else {
        None
    };

    let result = executor
        .execute_statement_on_txn(
            txn,
            db_id,
            sequence_values,
            search_path,
            stmt,
            current_role_override,
            None,
        )
        .await?;

    match result {
        ExecuteResult::Select {
            columns,
            column_types,
            rows,
            ..
        } => {
            let ret_lower = func_def.return_type.to_lowercase();
            let schema = if let Some(declared_cols) = parse_returns_table_columns(&ret_lower) {
                build_returns_table_schema(&declared_cols)
            } else {
                let target_table = ret_lower
                    .strip_prefix("setof ")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                build_output_schema(&target_table, &columns, &column_types)
            };
            Ok((schema, rows))
        }
        _ => Err(anyhow!(
            "table function '{}' body did not produce a SELECT result",
            func_def.name
        )),
    }
}

fn substitute_params(func_def: &FunctionDef, args: &[Value]) -> String {
    let mut param_map = HashMap::new();
    for (i, arg_type) in func_def.arg_types.iter().enumerate() {
        let parts: Vec<&str> = arg_type.split_whitespace().collect();
        let param_name = if parts.len() >= 2 && !looks_like_type(parts[0]) {
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
        let value_str = value_to_sql_literal(value);
        sql = plpgsql::replace_identifier(&sql, name, &value_str);
    }
    sql
}

fn value_to_sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int32(i) => i.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Text(t) => quoting::quote_literal(t),
        Value::Vector(_) => format!("'{}'", value),
        Value::Uuid(_) => format!("'{}'::uuid", value),
        Value::Timestamp(ts) => format!("'{}'::timestamp", ts),
        Value::Date(_) => format!("'{}'::date", value),
        Value::Time(_) => format!("'{}'::time", value),
        Value::Interval(iv) => format!("'{}'::interval", iv),
        Value::Json(s) => format!("{}::json", quoting::quote_literal(s)),
        Value::Jsonb(s) => format!("{}::jsonb", quoting::quote_literal(s)),
        Value::Bytes(b) => format!("'\\x{}'::bytea", hex::encode(b)),
        Value::Array(elems) => {
            let inner: Vec<String> = elems.iter().map(value_to_sql_literal).collect();
            format!("ARRAY[{}]", inner.join(", "))
        }
        Value::Numeric(d) => format!("{}::numeric", d),
        Value::Tsvector(s) => format!("{}::tsvector", quoting::quote_literal(s)),
        Value::Tsquery(s) => format!("{}::tsquery", quoting::quote_literal(s)),
    }
}

fn looks_like_type(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "integer"
            | "int"
            | "int4"
            | "int8"
            | "bigint"
            | "smallint"
            | "boolean"
            | "bool"
            | "text"
            | "varchar"
            | "real"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "numeric"
            | "decimal"
            | "timestamp"
            | "timestamptz"
            | "date"
            | "time"
            | "interval"
            | "uuid"
            | "json"
            | "jsonb"
            | "bytea"
            | "vector"
            | "tsvector"
            | "tsquery"
            | "setof"
    )
}

fn is_returns_table(ret_lower: &str) -> bool {
    ret_lower.starts_with("table(") || ret_lower.starts_with("table (")
}

fn parse_returns_table_columns(ret_lower: &str) -> Option<Vec<(String, crate::model::DataType)>> {
    use crate::model::DataType;

    let inner = ret_lower
        .strip_prefix("table")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))?;

    let mut cols = Vec::new();
    for part in inner.split(',') {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() < 2 {
            return None;
        }
        let col_name = tokens[0].to_string();
        let type_str = tokens[1..].join(" ").to_uppercase();
        let dt = match type_str.as_str() {
            "BOOL" | "BOOLEAN" => DataType::Boolean,
            "INT" | "INTEGER" | "INT4" | "SMALLINT" | "INT2" => DataType::Int32,
            "BIGINT" | "INT8" => DataType::Int64,
            "REAL" | "FLOAT4" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" | "FLOAT" => {
                DataType::Float64
            }
            "TEXT" | "VARCHAR" | "CHARACTER VARYING" | "CHAR" | "CHARACTER" => DataType::Text,
            "NUMERIC" | "DECIMAL" => DataType::Numeric {
                precision: None,
                scale: None,
            },
            "DATE" => DataType::Date,
            "TIME" => DataType::Time,
            "TIMESTAMP" | "TIMESTAMP WITHOUT TIME ZONE" => DataType::Timestamp,
            "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => DataType::TimestampTz,
            "INTERVAL" => DataType::Interval,
            "UUID" => DataType::Uuid,
            "BYTEA" => DataType::Bytes,
            "JSON" => DataType::Json,
            "JSONB" => DataType::Jsonb,
            "TSVECTOR" => DataType::Tsvector,
            "TSQUERY" => DataType::Tsquery,
            s if s.starts_with("VECTOR") => {
                let dim = s
                    .strip_prefix("VECTOR")
                    .and_then(|r| r.trim().strip_prefix('('))
                    .and_then(|r| r.strip_suffix(')'))
                    .and_then(|r| r.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                DataType::Vector(dim)
            }
            _ => DataType::Text,
        };
        cols.push((col_name, dt));
    }
    if cols.is_empty() {
        None
    } else {
        Some(cols)
    }
}

fn build_returns_table_schema(declared_cols: &[(String, crate::model::DataType)]) -> TableSchema {
    use crate::model::ColumnDef;

    let cols: Vec<ColumnDef> = declared_cols
        .iter()
        .map(|(name, dt)| ColumnDef {
            name: name.clone(),
            data_type: dt.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        })
        .collect();

    TableSchema {
        table_id: 0,
        name: String::new(),
        columns: cols,
        indexes: vec![],
        ..Default::default()
    }
}

fn build_output_schema(
    table_name: &str,
    columns: &[String],
    column_types: &Option<Vec<crate::model::DataType>>,
) -> TableSchema {
    use crate::model::{ColumnDef, DataType};

    let cols: Vec<ColumnDef> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let dt = column_types
                .as_ref()
                .and_then(|cts| cts.get(i))
                .cloned()
                // INTENTIONAL: no RETURNS clause — default to Text (PG-compatible)
                .unwrap_or(DataType::Text);
            ColumnDef {
                name: name.clone(),
                data_type: dt,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            }
        })
        .collect();

    TableSchema {
        table_id: 0,
        name: table_name.to_string(),
        columns: cols,
        indexes: vec![],
        ..Default::default()
    }
}
