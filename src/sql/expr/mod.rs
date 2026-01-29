//! Expression evaluation logic

mod context;
mod evaluator;
pub mod functions;
mod numeric;
mod operators;

pub use context::{JoinEvalContext, SingleTableContext};
#[allow(unused_imports)]
pub use context::EvalContext;
#[allow(unused_imports)]
pub use evaluator::eval_expr_impl as eval_expr_unified;

pub(crate) fn parse_bool_pg(s: &str) -> Option<bool> {
    operators::parse_bool_pg(s)
}

use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Context, Result};
use rust_decimal::Decimal;
use sqlparser::ast::{BinaryOperator, Expr, JsonOperator, Value as SqlValue};
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use super::timezone::parse_timezone_offset_seconds;

// Thread-local storage for connection_id used by pg_backend_pid() as a fallback.
thread_local! {
    static CONNECTION_ID_FALLBACK: Cell<i32> = const { Cell::new(0) };
}

// Task-local execution context for correct behavior across async suspension points.
tokio::task_local! {
    static CONNECTION_ID: i32;
    static CURRENT_DATABASE_NAME: Arc<str>;
}

const VERSION_STRING: &str = concat!(
    "PostgreSQL 16.0 (pg-tikv ",
    env!("CARGO_PKG_VERSION"),
    " on TiKV)"
);

/// Set the connection_id for the current thread (call before query execution)
pub fn set_connection_id(id: i32) {
    CONNECTION_ID_FALLBACK.with(|c| c.set(id));
}

fn get_connection_id() -> i32 {
    CONNECTION_ID
        .try_with(|c| *c)
        .unwrap_or_else(|_| CONNECTION_ID_FALLBACK.with(|c| c.get()))
}

fn current_database_name() -> Option<Arc<str>> {
    CURRENT_DATABASE_NAME.try_with(|name| name.clone()).ok()
}

pub(crate) async fn with_query_context<R, Fut>(
    connection_id: i32,
    database_name: Arc<str>,
    fut: Fut,
) -> R
where
    Fut: Future<Output = R>,
{
    // In debug builds, nested task-local scopes can create very large async state machines.
    // Boxing the inner future keeps scope wrappers small and avoids stack overflows.
    #[cfg(debug_assertions)]
    {
        let fut = Box::pin(fut);
        CONNECTION_ID
            .scope(connection_id, CURRENT_DATABASE_NAME.scope(database_name, fut))
            .await
    }

    #[cfg(not(debug_assertions))]
    {
        CONNECTION_ID
            .scope(connection_id, CURRENT_DATABASE_NAME.scope(database_name, fut))
            .await
    }
}

pub struct JoinContext<'a> {
    #[allow(dead_code)]
    pub tables: HashMap<String, (&'a TableSchema, &'a Row)>,
    pub column_offsets: HashMap<String, usize>,
    pub combined_row: &'a Row,
    #[allow(dead_code)]
    pub combined_schema: &'a TableSchema,
}

fn sql_datatype_is_timestamptz(dt: &sqlparser::ast::DataType) -> Option<bool> {
    match dt {
        sqlparser::ast::DataType::Timestamp(_, tz) => match tz {
            sqlparser::ast::TimezoneInfo::WithTimeZone | sqlparser::ast::TimezoneInfo::Tz => {
                Some(true)
            }
            _ => Some(false),
        },
        sqlparser::ast::DataType::Custom(name, _) => name
            .0
            .last()
            .map(|ident| ident.value.eq_ignore_ascii_case("TIMESTAMPTZ")),
        _ => None,
    }
}

fn expr_is_timestamptz(expr: &Expr, schema: Option<&TableSchema>) -> bool {
    match expr {
        Expr::Identifier(ident) => schema
            .and_then(|s| s.column_index(&ident.value).map(|idx| &s.columns[idx].data_type))
            .is_some_and(|dt| matches!(dt, DataType::TimestampTz)),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .and_then(|ident| {
                schema.and_then(|s| s.column_index(&ident.value).map(|idx| &s.columns[idx].data_type))
            })
            .is_some_and(|dt| matches!(dt, DataType::TimestampTz)),
        Expr::Function(func) => func.name.0.last().is_some_and(|ident| {
            ident.value.eq_ignore_ascii_case("NOW")
                || ident.value.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        }),
        Expr::Cast { data_type, .. } | Expr::TypedString { data_type, .. } => {
            sql_datatype_is_timestamptz(data_type).unwrap_or(false)
        }
        Expr::AtTimeZone { timestamp, .. } => !expr_is_timestamptz(timestamp, schema),
        Expr::Nested(inner) => expr_is_timestamptz(inner, schema),
        _ => false,
    }
}

fn expr_is_timestamptz_join_with_schema(
    expr: &Expr,
    column_offsets: &HashMap<String, usize>,
    combined_schema: &TableSchema,
) -> bool {
    fn column_type_from_expr<'a>(
        expr: &Expr,
        column_offsets: &'a HashMap<String, usize>,
        combined_schema: &'a TableSchema,
    ) -> Option<&'a DataType> {
        match expr {
            Expr::Identifier(ident) => column_offsets
                .get(&ident.value)
                .and_then(|&offset| combined_schema.columns.get(offset))
                .map(|col| &col.data_type),
            Expr::CompoundIdentifier(parts) => {
                if parts.len() != 2 {
                    return None;
                }
                let key = format!("{}.{}", parts[0].value, parts[1].value);
                if let Some(&offset) = column_offsets.get(&key) {
                    return combined_schema.columns.get(offset).map(|col| &col.data_type);
                }
                for (k, &offset) in column_offsets {
                    if k.eq_ignore_ascii_case(&key) {
                        return combined_schema.columns.get(offset).map(|col| &col.data_type);
                    }
                }
                None
            }
            _ => None,
        }
    }

    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            column_type_from_expr(expr, column_offsets, combined_schema)
                .is_some_and(|dt| matches!(dt, DataType::TimestampTz))
        }
        Expr::Function(func) => func.name.0.last().is_some_and(|ident| {
            ident.value.eq_ignore_ascii_case("NOW")
                || ident.value.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        }),
        Expr::Cast { data_type, .. } | Expr::TypedString { data_type, .. } => {
            sql_datatype_is_timestamptz(data_type).unwrap_or(false)
        }
        Expr::AtTimeZone { timestamp, .. } => {
            !expr_is_timestamptz_join_with_schema(timestamp, column_offsets, combined_schema)
        }
        Expr::Nested(inner) => {
            expr_is_timestamptz_join_with_schema(inner, column_offsets, combined_schema)
        }
        _ => false,
    }
}

pub fn eval_expr_join(expr: &Expr, ctx: &JoinContext) -> Result<Value> {
    stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
        let eval_ctx = JoinEvalContext::from_join_context(ctx);
        evaluator::eval_expr_impl(&eval_ctx, expr)
    })
}

#[allow(dead_code)]
pub fn eval_with_context(expr: &Expr, ctx: &SingleTableContext) -> Result<Value> {
    eval_expr(expr, ctx.row(), ctx.get_schema())
}

#[allow(dead_code)]
pub fn eval_with_join_context(expr: &Expr, ctx: &JoinEvalContext) -> Result<Value> {
    let join_ctx = JoinContext {
        tables: HashMap::new(),
        column_offsets: ctx.column_offsets.clone(),
        combined_row: ctx.combined_row,
        combined_schema: ctx.combined_schema,
    };
    eval_expr_join(expr, &join_ctx)
}

pub fn eval_expr(expr: &Expr, row: Option<&Row>, schema: Option<&TableSchema>) -> Result<Value> {
    stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
        let ctx = SingleTableContext::new(row, schema);
        evaluator::eval_expr_impl(&ctx, expr)
    })
}

#[inline(never)]
fn parse_interval_from_expr(s: &str, interval: &sqlparser::ast::Interval) -> Result<Value> {
    if let Some(field) = &interval.leading_field {
        let num: i64 = s.trim().parse().unwrap_or(0);
        return interval_from_field(num, field);
    }
    parse_interval_string(s)
}

fn interval_from_number(num: i64, interval: &sqlparser::ast::Interval) -> Result<Value> {
    use crate::types::IntervalValue;
    if let Some(field) = &interval.leading_field {
        return interval_from_field(num, field);
    }
    Ok(Value::Interval(IntervalValue::from_millis(num * 1000)))
}

fn interval_from_field(num: i64, field: &sqlparser::ast::DateTimeField) -> Result<Value> {
    use crate::types::IntervalValue;
    let iv = match field {
        sqlparser::ast::DateTimeField::Year => IntervalValue::from_months((num * 12) as i32),
        sqlparser::ast::DateTimeField::Month => IntervalValue::from_months(num as i32),
        sqlparser::ast::DateTimeField::Week => {
            IntervalValue::from_millis(num * 7 * 24 * 60 * 60 * 1000)
        }
        sqlparser::ast::DateTimeField::Day => IntervalValue::from_millis(num * 24 * 60 * 60 * 1000),
        sqlparser::ast::DateTimeField::Hour => IntervalValue::from_millis(num * 60 * 60 * 1000),
        sqlparser::ast::DateTimeField::Minute => IntervalValue::from_millis(num * 60 * 1000),
        sqlparser::ast::DateTimeField::Second => IntervalValue::from_millis(num * 1000),
        _ => return Err(anyhow!("Unsupported interval field")),
    };
    Ok(Value::Interval(iv))
}

fn eval_function<C: EvalContext>(ctx: &C, func: &sqlparser::ast::Function) -> Result<Value> {
    let func_name = func.name.0.last().map(|i| i.value.as_str()).unwrap_or("");
    let func_name_upper = func_name.to_uppercase();
    let row = ctx.row();
    let schema = ctx.schema();

    // COALESCE is evaluated left-to-right and must short-circuit.
    // Do not eagerly evaluate all args, otherwise errors in later args would be surfaced incorrectly.
    if func_name_upper == "COALESCE" {
        for arg in &func.args {
            if let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) =
                arg
            {
                let val = evaluator::eval_expr_impl(ctx, e)?;
                if !matches!(val, Value::Null) {
                    return Ok(val);
                }
            }
        }
        return Ok(Value::Null);
    }

    let mut args = Vec::with_capacity(func.args.len());
    for arg in &func.args {
        if let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) = arg
        {
            args.push(evaluator::eval_expr_impl(ctx, e)?);
        }
    }

    if let Some(registry_fn) = functions::get_registry().get(func_name_upper.as_str()) {
        return registry_fn(args);
    }

    match func_name_upper.as_str() {
        "NULLIF" => {
            if args.len() >= 2 && compare_values(&args[0], &args[1]).unwrap_or(1) == 0 {
                Ok(Value::Null)
            } else {
                Ok(args.into_iter().next().unwrap_or(Value::Null))
            }
        }
        "GREATEST" => {
            let mut max = Value::Null;
            for val in args {
                if matches!(max, Value::Null) {
                    max = val;
                } else if compare_values(&val, &max).unwrap_or(0) > 0 {
                    max = val;
                }
            }
            Ok(max)
        }
        "LEAST" => {
            let mut min = Value::Null;
            for val in args {
                if matches!(min, Value::Null) {
                    min = val;
                } else if compare_values(&val, &min).unwrap_or(0) < 0 {
                    min = val;
                }
            }
            Ok(min)
        }
        // String functions UPPER, LOWER, LENGTH, CHAR_LENGTH, CHARACTER_LENGTH, OCTET_LENGTH, BIT_LENGTH
        // are handled by the registry (functions/string.rs)
        "GET_BIT" => eval_get_bit_from_args(args),
        "SET_BIT" => eval_set_bit_from_args(args),
        "INT8SEND" => eval_int8send_from_args(args),
        "INT4SEND" => eval_int4send_from_args(args),
        "UUID_SEND" => eval_uuid_send_from_args(args),
        // CONCAT, CONCAT_WS, LEFT, RIGHT are handled by the registry (functions/string.rs)
        "SUBSTR" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            let start = match iter.next() {
                Some(Value::Int32(n)) => n,
                Some(Value::Int64(n)) => n as i32,
                Some(Value::Null) | None => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let len = match iter.next() {
                Some(Value::Int32(n)) => Some(n),
                Some(Value::Int64(n)) => Some(n as i32),
                Some(Value::Null) => return Ok(Value::Null),
                None => None,
                _ => return Ok(Value::Null),
            };

            let chars: Vec<char> = s.chars().collect();
            let start_idx = (start.saturating_sub(1).max(0)) as usize;
            let result: String = match len {
                Some(n) => chars
                    .iter()
                    .skip(start_idx)
                    .take(n.max(0) as usize)
                    .collect(),
                None => chars.iter().skip(start_idx).collect(),
            };
            Ok(Value::Text(result))
        }
        // LPAD, RPAD, REPLACE, REVERSE, TRIM, BTRIM, LTRIM, RTRIM, REPEAT, SPLIT_PART, STRPOS,
        // ASCII, CHR, MD5, ENCODE, DECODE are handled by the registry (functions/string.rs, functions/encoding.rs)
        "FORMAT" => {
            let mut iter = args.into_iter();
            let fmt = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("format() requires text format string")),
            };

            let format_args: Vec<Value> = iter.collect();
            let mut next_arg_idx = 0usize;

            let mut result = String::new();
            let mut chars = fmt.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '%' {
                    result.push(c);
                    continue;
                }

                if matches!(chars.peek(), Some('%')) {
                    chars.next();
                    result.push('%');
                    continue;
                }

                // Parse argument position if written as n$; otherwise treat leading digits as width.
                let mut digits = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }

                let mut arg_pos: Option<usize> = None;
                let mut width: Option<usize> = None;
                if !digits.is_empty() {
                    if matches!(chars.peek(), Some('$')) {
                        chars.next();
                        let pos = digits.parse::<usize>().unwrap_or(1);
                        arg_pos = Some(pos.saturating_sub(1));
                    } else {
                        width = Some(digits.parse::<usize>().unwrap_or(0));
                    }
                }

                let mut left_align = false;
                while matches!(chars.peek(), Some('-')) {
                    left_align = true;
                    chars.next();
                }

                if width.is_none() {
                    let mut width_digits = String::new();
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            width_digits.push(d);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if !width_digits.is_empty() {
                        width = Some(width_digits.parse::<usize>().unwrap_or(0));
                    }
                }

                let Some(format_char) = chars.next() else {
                    return Err(anyhow!("unterminated format() pattern"));
                };

                let arg_idx = match arg_pos {
                    Some(pos) => pos,
                    None => {
                        let pos = next_arg_idx;
                        next_arg_idx += 1;
                        pos
                    }
                };
                let arg_val = format_args
                    .get(arg_idx)
                    .ok_or_else(|| anyhow!("too few arguments for format()"))?;

                let mut rendered = match format_char {
                    's' => match arg_val {
                        Value::Null => String::new(),
                        Value::Text(s) => s.clone(),
                        v => v.to_string(),
                    },
                    'I' => {
                        if matches!(arg_val, Value::Null) {
                            return Err(anyhow!(
                                "null values cannot be formatted as an SQL identifier"
                            ));
                        }
                        let s = match arg_val {
                            Value::Text(s) => s.clone(),
                            v => v.to_string(),
                        };
                        quote_ident_impl(&s)
                    }
                    'L' => {
                        if matches!(arg_val, Value::Null) {
                            "NULL".to_string()
                        } else {
                            let s = match arg_val {
                                Value::Text(s) => s.clone(),
                                v => v.to_string(),
                            };
                            quote_literal_impl(&s)
                        }
                    }
                    other => return Err(anyhow!(format_unrecognized_specifier_error(other))),
                };

                if let Some(w) = width {
                    let len = rendered.chars().count();
                    if len < w {
                        let padding = " ".repeat(w - len);
                        if left_align {
                            rendered.push_str(&padding);
                        } else {
                            rendered = format!("{}{}", padding, rendered);
                        }
                    }
                }

                result.push_str(&rendered);
            }

            Ok(Value::Text(result))
        }
        // TRANSLATE, INITCAP are handled by the registry (functions/string.rs)
        // ABS, CEIL, CEILING, FLOOR, ROUND, TRUNC, TRUNCATE, SQRT, CBRT, POWER, POW, EXP, LN, LOG, LOG10,
        // SIGN, MOD, DEGREES, RADIANS, SIN, COS, TAN, PI, RANDOM are handled by the registry (functions/math.rs)
        "NOW" | "CURRENT_TIMESTAMP" => {
            if args.len() > 1 {
                return Err(anyhow!("{} expects 0 or 1 argument", func_name));
            }
            let precision = match args.first() {
                None => 6_u32,
                Some(Value::Int32(p)) => (*p).clamp(0, 6) as u32,
                Some(Value::Int64(p)) => (*p).clamp(0, 6) as u32,
                _ => 6_u32,
            };
            let ts = super::statement_time::statement_timestamp_millis_or_now();
            let ts = crate::types::timestamp::truncate_timestamp_millis(ts, precision);
            Ok(Value::Timestamp(ts))
        }
        "CURRENT_DATE" => {
            use chrono::Utc;
            let today = Utc::now().date_naive();
            let days = crate::types::date::naive_date_to_days(today)?;
            Ok(Value::Date(days))
        }
        "DATE_TRUNC" => eval_date_trunc_from_args(args),
        "DATE" => {
            let days = match args.into_iter().next() {
                Some(Value::Date(days)) => days,
                Some(Value::Timestamp(ts)) => {
                    crate::types::date::timestamp_millis_to_date_days(ts)?
                }
                Some(Value::Text(s)) => {
                    let ts = match parse_timestamp_string(&s)
                        .map_err(|e| anyhow!("Invalid date format: {}", e))?
                    {
                        Value::Timestamp(ts) => ts,
                        _ => return Ok(Value::Null),
                    };
                    crate::types::date::timestamp_millis_to_date_days(ts)?
                }
                _ => return Ok(Value::Null),
            };
            Ok(Value::Date(days))
        }
        "TO_CHAR" => eval_to_char_from_args(args),
        "AGE" => {
            use chrono::{Datelike, TimeZone, Utc};
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut iter = args.into_iter();
            let ts1 = match iter.next() {
                Some(Value::Timestamp(t)) => t,
                Some(Value::Date(days)) => crate::types::date::date_days_to_timestamp_millis(days)?,
                _ => return Ok(Value::Null),
            };
            let ts2 = match iter.next() {
                Some(Value::Timestamp(t)) => t,
                Some(Value::Date(days)) => crate::types::date::date_days_to_timestamp_millis(days)?,
                _ => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| anyhow!("Failed to get current time: {}", e))?
                    .as_millis() as i64,
            };

            let dt1 = Utc
                .timestamp_millis_opt(ts1)
                .single()
                .ok_or_else(|| anyhow!("Invalid timestamp"))?;
            let dt2 = Utc
                .timestamp_millis_opt(ts2)
                .single()
                .ok_or_else(|| anyhow!("Invalid timestamp"))?;

            let mut years = dt1.year() - dt2.year();
            let mut months = dt1.month() as i32 - dt2.month() as i32;
            let mut days = dt1.day() as i32 - dt2.day() as i32;

            if days < 0 {
                months -= 1;
                let prev_month = if dt1.month() == 1 {
                    12
                } else {
                    dt1.month() - 1
                };
                let prev_year = if dt1.month() == 1 {
                    dt1.year() - 1
                } else {
                    dt1.year()
                };
                days += operators::days_in_month(prev_year, prev_month) as i32;
            }
            if months < 0 {
                years -= 1;
                months += 12;
            }

            let total_months = years * 12 + months;

            Ok(Value::Interval(crate::types::IntervalValue::new(
                total_months,
                days as i64 * 24 * 60 * 60 * 1000,
            )))
        }
        "GENERATE_SERIES" => Err(anyhow!(
            "GENERATE_SERIES is a set-returning function, not supported in this context"
        )),
        // GEN_RANDOM_UUID, UUID_GENERATE_V4, UUIDV7 are handled by the registry (functions/uuid.rs)
        "NEXTVAL" | "CURRVAL" | "SETVAL" => Err(anyhow!(
            "{} is a sequence function and must be evaluated during execution",
            func_name
        )),
        "SET_CONFIG" => Ok(Value::Text(String::new())),
        // PG_IS_IN_RECOVERY, PG_ENCODING_TO_CHAR, HAS_SCHEMA_PRIVILEGE, HAS_TABLE_PRIVILEGE,
        // HAS_DATABASE_PRIVILEGE are handled by the registry (functions/pg_compat.rs)
        "PG_BACKEND_PID" => Ok(Value::Int32(get_connection_id())),
        "VERSION" => Ok(Value::Text(VERSION_STRING.to_string())),
        "CURRENT_DATABASE" => Ok(Value::Text(
            current_database_name()
                .map(|name| name.as_ref().to_string())
                .unwrap_or_else(|| "postgres".to_string()),
        )),
        "CURRENT_SCHEMA" => Ok(Value::Text("public".to_string())),
        "CURRENT_USER" | "SESSION_USER" | "USER" => Ok(Value::Text("postgres".to_string())),
        "PG_GET_USERBYID" => Ok(Value::Text("postgres".to_string())),
        "PG_GET_INDEXDEF" => {
            if let Some(schema) = ctx.schema() {
                if let Some(col) = schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case("indexdef"))
                {
                    if let Ok(val) = ctx.resolve_column(&col.name) {
                        if !matches!(val, Value::Null) {
                            return Ok(val);
                        }
                    }
                }
            }
            Ok(Value::Text("CREATE INDEX".to_string()))
        }
        "PG_GET_CONSTRAINTDEF" => {
            if let Some(schema) = ctx.schema() {
                if let Some(col) = schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case("constraintdef"))
                {
                    if let Ok(val) = ctx.resolve_column(&col.name) {
                        if !matches!(val, Value::Null) {
                            return Ok(val);
                        }
                    }
                }
            }
            Ok(Value::Text(String::new()))
        }
        "PG_GET_EXPR" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s)),
            Some(Value::Null) | None => Ok(Value::Null),
            Some(v) => Ok(Value::Text(v.to_string())),
        },
        "FORMAT_TYPE" => {
            let mut iter = args.into_iter();
            let oid = match iter.next().unwrap_or(Value::Null) {
                Value::Int32(n) => n as i64,
                Value::Int64(n) => n,
                Value::Text(s) => s.trim().parse::<i64>().unwrap_or(0),
                Value::Null => return Ok(Value::Null),
                _ => 0,
            };
            let type_name = match oid {
                16 => "bool",
                20 => "int8",
                23 => "int4",
                701 => "float8",
                25 => "text",
                17 => "bytea",
                1114 => "timestamp",
                1184 => "timestamptz",
                2950 => "uuid",
                114 => "json",
                3802 => "jsonb",
                16385 => "vector",
                _ => "text",
            };
            Ok(Value::Text(type_name.to_string()))
        }
        // OBJ_DESCRIPTION, COL_DESCRIPTION, SHOBJ_DESCRIPTION, PG_GET_SERIAL_SEQUENCE are handled by the registry
        "PG_CATALOG.SET_CONFIG" => Ok(Value::Text(String::new())),
        // UNNEST, ARRAY_LENGTH, ARRAY_UPPER, ARRAY_LOWER, CARDINALITY, ARRAY_POSITION, ARRAY_CAT,
        // ARRAY_APPEND, ARRAY_PREPEND, ARRAY_REMOVE, ARRAY_TO_STRING, STRING_TO_ARRAY are handled by the registry (functions/array.rs)
        // JSONB_ARRAY_LENGTH, JSON_ARRAY_LENGTH, JSONB_TYPEOF, JSON_TYPEOF, JSONB_BUILD_OBJECT, JSON_BUILD_OBJECT,
        // JSONB_BUILD_ARRAY, JSON_BUILD_ARRAY, JSONB_EXISTS, JSONB_EXISTS_ANY, JSONB_EXISTS_ALL, JSONB_OBJECT_KEYS,
        // JSON_OBJECT_KEYS, JSONB_EXTRACT_PATH, JSON_EXTRACT_PATH, JSONB_EXTRACT_PATH_TEXT, JSON_EXTRACT_PATH_TEXT,
        // JSONB_PRETTY, TO_JSON are handled by the registry (functions/json.rs)
        "TO_JSONB" => {
            fn eval_row_object<C: EvalContext>(
                ctx: &C,
                expr: &Expr,
            ) -> Result<Option<serde_json::Value>> {
                fn row_values<C: EvalContext>(
                    ctx: &C,
                    expr: &Expr,
                ) -> Result<Option<Vec<Value>>> {
                    match expr {
                        Expr::Nested(inner) => row_values(ctx, inner),
                        Expr::Tuple(exprs) => {
                            let mut vals = Vec::with_capacity(exprs.len());
                            for e in exprs {
                                vals.push(evaluator::eval_expr_impl(ctx, e)?);
                            }
                            Ok(Some(vals))
                        }
                        Expr::Function(func) => {
                            let name = func.name.0.last().map(|i| i.value.as_str()).unwrap_or("");
                            if !name.eq_ignore_ascii_case("ROW") {
                                return Ok(None);
                            }
                            let mut vals = Vec::with_capacity(func.args.len());
                            for arg in &func.args {
                                match arg {
                                    sqlparser::ast::FunctionArg::Unnamed(
                                        sqlparser::ast::FunctionArgExpr::Expr(e),
                                    ) => vals.push(evaluator::eval_expr_impl(ctx, e)?),
                                    _ => return Ok(None),
                                }
                            }
                            Ok(Some(vals))
                        }
                        _ => Ok(None),
                    }
                }

                let Some(values) = row_values(ctx, expr)? else {
                    return Ok(None);
                };
                let mut obj = serde_json::Map::new();
                for (idx, v) in values.into_iter().enumerate() {
                    obj.insert(format!("f{}", idx + 1), value_to_json(&v));
                }
                Ok(Some(serde_json::Value::Object(obj)))
            }

            if func.args.len() == 1 {
                if let sqlparser::ast::FunctionArg::Unnamed(
                    sqlparser::ast::FunctionArgExpr::Expr(expr),
                ) = &func.args[0]
                {
                    if let Some(obj) = eval_row_object(ctx, expr)? {
                        return Ok(Value::Jsonb(obj.to_string()));
                    }
                }
            }

            let val = args.into_iter().next().unwrap_or(Value::Null);
            let json_val = value_to_json(&val);
            Ok(Value::Jsonb(json_val.to_string()))
        }
        "L2_DISTANCE" => {
            if args.len() != 2 {
                return Err(anyhow!("l2_distance requires exactly 2 arguments"));
            }
            let vec1 = extract_vector(&args[0])?;
            let vec2 = extract_vector(&args[1])?;
            let dist = l2_distance(&vec1, &vec2)?;
            Ok(Value::Float64(dist))
        }
        "COSINE_DISTANCE" => {
            if args.len() != 2 {
                return Err(anyhow!("cosine_distance requires exactly 2 arguments"));
            }
            let vec1 = extract_vector(&args[0])?;
            let vec2 = extract_vector(&args[1])?;
            let dist = cosine_distance(&vec1, &vec2)?;
            Ok(Value::Float64(dist))
        }
        "INNER_PRODUCT" => {
            if args.len() != 2 {
                return Err(anyhow!("inner_product requires exactly 2 arguments"));
            }
            let vec1 = extract_vector(&args[0])?;
            let vec2 = extract_vector(&args[1])?;
            let prod = inner_product(&vec1, &vec2)?;
            Ok(Value::Float64(prod))
        }
        "VECTOR_DIMS" => {
            if args.is_empty() {
                return Err(anyhow!("vector_dims requires 1 argument"));
            }
            let vec = extract_vector(&args[0])?;
            Ok(Value::Int32(vec.len() as i32))
        }
        "VECTOR_NORM" => {
            if args.is_empty() {
                return Err(anyhow!("vector_norm requires 1 argument"));
            }
            let vec = extract_vector(&args[0])?;
            Ok(Value::Float64(vector_norm(&vec)))
        }

        // REGEXP_REPLACE, REGEXP_MATCHES, REGEXP_SPLIT_TO_ARRAY are handled by the registry (functions/regex.rs)
        // PG_TYPEOF, QUOTE_IDENT, QUOTE_LITERAL, QUOTE_NULLABLE, CLOCK_TIMESTAMP, STATEMENT_TIMESTAMP,
        // TRANSACTION_TIMESTAMP, TXID_CURRENT, PG_COLUMN_SIZE, PG_TABLE_IS_VISIBLE are handled by the registry (functions/pg_compat.rs)
        // JSONB_SET, JSON_SET, JSONB_ARRAY_ELEMENTS, JSON_ARRAY_ELEMENTS, JSONB_ARRAY_ELEMENTS_TEXT,
        // JSON_ARRAY_ELEMENTS_TEXT, JSONB_EACH, JSON_EACH, JSONB_EACH_TEXT, JSON_EACH_TEXT are handled by the registry (functions/json.rs)
        "ROW_TO_JSON" => {
            fn eval_row_object<C: EvalContext>(
                ctx: &C,
                expr: &Expr,
            ) -> Result<Option<serde_json::Value>> {
                fn row_values<C: EvalContext>(
                    ctx: &C,
                    expr: &Expr,
                ) -> Result<Option<Vec<Value>>> {
                    match expr {
                        Expr::Nested(inner) => row_values(ctx, inner),
                        Expr::Tuple(exprs) => {
                            let mut vals = Vec::with_capacity(exprs.len());
                            for e in exprs {
                                vals.push(evaluator::eval_expr_impl(ctx, e)?);
                            }
                            Ok(Some(vals))
                        }
                        Expr::Function(func) => {
                            let name = func.name.0.last().map(|i| i.value.as_str()).unwrap_or("");
                            if !name.eq_ignore_ascii_case("ROW") {
                                return Ok(None);
                            }
                            let mut vals = Vec::with_capacity(func.args.len());
                            for arg in &func.args {
                                match arg {
                                    sqlparser::ast::FunctionArg::Unnamed(
                                        sqlparser::ast::FunctionArgExpr::Expr(e),
                                    ) => vals.push(evaluator::eval_expr_impl(ctx, e)?),
                                    _ => return Ok(None),
                                }
                            }
                            Ok(Some(vals))
                        }
                        _ => Ok(None),
                    }
                }

                let Some(values) = row_values(ctx, expr)? else {
                    return Ok(None);
                };
                let mut obj = serde_json::Map::new();
                for (idx, v) in values.into_iter().enumerate() {
                    obj.insert(format!("f{}", idx + 1), value_to_json(&v));
                }
                Ok(Some(serde_json::Value::Object(obj)))
            }

            if func.args.len() == 1 {
                if let sqlparser::ast::FunctionArg::Unnamed(
                    sqlparser::ast::FunctionArgExpr::Expr(expr),
                ) = &func.args[0]
                {
                    if let Some(obj) = eval_row_object(ctx, expr)? {
                        return Ok(Value::Json(obj.to_string()));
                    }
                }
            }

            let val = args.into_iter().next().unwrap_or(Value::Null);
            let json_val = value_to_json(&val);
            Ok(Value::Json(json_val.to_string()))
        }

        _ => Err(anyhow!("Unsupported function: {}", func_name)),
    }
}

fn like_match(s: &str, pattern: &str, escape_char: Option<char>, case_insensitive: bool) -> bool {
    if case_insensitive {
        return like_match_impl(
            &s.to_lowercase(),
            &pattern.to_lowercase(),
            escape_char,
        );
    }
    like_match_impl(s, pattern, escape_char)
}

fn like_match_impl(s: &str, pattern: &str, escape_char: Option<char>) -> bool {
    #[inline]
    fn next_char_at(s: &str, idx: usize) -> Option<(char, usize)> {
        let ch = s[idx..].chars().next()?;
        Some((ch, idx + ch.len_utf8()))
    }

    let mut s_idx = 0usize;
    let mut p_idx = 0usize;

    // Backtracking positions for the most recent '%'.
    let mut backtrack_p: Option<usize> = None;
    let mut backtrack_s: usize = 0;

    while s_idx < s.len() {
        if p_idx < pattern.len() {
            let (pc, p_next) = next_char_at(pattern, p_idx).expect("p_idx < len");

            if escape_char.is_some_and(|esc| pc == esc) {
                // Escape: treat the next pattern character as a literal.
                if let Some((lit, p_after)) = next_char_at(pattern, p_next) {
                    if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                        if sc == lit {
                            s_idx = s_next;
                            p_idx = p_after;
                            continue;
                        }
                    }
                } else if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                    // Trailing escape char: match it literally.
                    let esc = escape_char.expect("checked is_some");
                    if sc == esc {
                        s_idx = s_next;
                        p_idx = p_next;
                        continue;
                    }
                }
            } else if pc == '%' {
                // Collapse consecutive '%' and record the backtracking point.
                let mut p_after = p_next;
                while p_after < pattern.len() {
                    let (next_pc, next_next) =
                        next_char_at(pattern, p_after).expect("p_after < len");
                    if next_pc != '%' {
                        break;
                    }
                    p_after = next_next;
                }
                backtrack_p = Some(p_after);
                backtrack_s = s_idx;
                p_idx = p_after;
                continue;
            } else if pc == '_' {
                // Match any single character.
                if let Some((_sc, s_next)) = next_char_at(s, s_idx) {
                    s_idx = s_next;
                    p_idx = p_next;
                    continue;
                }
            } else if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                if sc == pc {
                    s_idx = s_next;
                    p_idx = p_next;
                    continue;
                }
            }
        }

        // Mismatch: if we have a previous '%', backtrack and let it consume one more character.
        if let Some(p_after_percent) = backtrack_p {
            if backtrack_s < s.len() {
                let (_sc, s_next) = next_char_at(s, backtrack_s).expect("backtrack_s < len");
                backtrack_s = s_next;
                s_idx = backtrack_s;
                p_idx = p_after_percent;
                continue;
            }
        }

        return false;
    }

    // String is consumed; the remaining pattern must be empty or all '%'.
    while p_idx < pattern.len() {
        let (pc, p_next) = next_char_at(pattern, p_idx).expect("p_idx < len");
        if escape_char.is_some_and(|esc| pc == esc) {
            return false;
        }
        if pc != '%' {
            return false;
        }
        p_idx = p_next;
    }

    true
}

fn similar_to_match(s: &str, pattern: &str, escape_char: Option<char>) -> Result<bool> {
    let escape = escape_char.unwrap_or('\\');
    let mut regex_pattern = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == escape {
            // Treat escaped character as a literal.
            if let Some(next) = chars.next() {
                regex_pattern.push_str(&regex::escape(&next.to_string()));
            } else {
                regex_pattern.push_str(&regex::escape(&escape.to_string()));
            }
            continue;
        }

        match ch {
            '%' => regex_pattern.push_str(".*"),
            '_' => regex_pattern.push('.'),
            other => regex_pattern.push(other),
        }
    }

    let re = regex::Regex::new(&format!("^{}$", regex_pattern))
        .map_err(|e| anyhow!("Invalid SIMILAR TO pattern: {}", e))?;
    Ok(re.is_match(s))
}

fn round_half_away_from_zero(n: f64) -> f64 {
    if n >= 0.0 {
        (n + 0.5).floor()
    } else {
        (n - 0.5).ceil()
    }
}

fn cast_to_bytea(v: Value) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Bytes(_) => Ok(v),
        Value::Text(s) => {
            if let Some(rest) = s.strip_prefix("\\x") {
                let bytes = hex::decode(rest)
                    .map_err(|e| anyhow!("invalid input syntax for type bytea: {}", e))?;
                Ok(Value::Bytes(bytes))
            } else {
                Ok(Value::Bytes(s.into_bytes()))
            }
        }
        other => Ok(Value::Bytes(other.to_string().into_bytes())),
    }
}

fn cast_value(val: Value, data_type: &sqlparser::ast::DataType) -> Result<Value> {
    use sqlparser::ast::DataType as SqlType;
    match (val, data_type) {
        (Value::Null, _) => Ok(Value::Null),
        (v, SqlType::Numeric(info) | SqlType::Decimal(info)) => {
            let (precision, scale) = match info {
                sqlparser::ast::ExactNumberInfo::None => (None, None),
                // Postgres: NUMERIC(p) implies scale=0
                sqlparser::ast::ExactNumberInfo::Precision(p) => (Some(*p as u32), Some(0)),
                sqlparser::ast::ExactNumberInfo::PrecisionAndScale(p, s) => {
                    (Some(*p as u32), Some(*s as u32))
                }
            };
            if let Some(p) = precision {
                if p > 28 {
                    return Err(anyhow!(
                        "NUMERIC precision {} exceeds supported maximum 28",
                        p
                    ));
                }
            }
            if let Some(s) = scale {
                if s > 28 {
                    return Err(anyhow!("NUMERIC scale {} exceeds supported maximum 28", s));
                }
            }
            if let (Some(p), Some(s)) = (precision, scale) {
                if s > p {
                    return Err(anyhow!(
                        "NUMERIC scale {} must be between 0 and precision {}",
                        s,
                        p
                    ));
                }
            }

            let mut d = match v {
                Value::Numeric(d) => d,
                Value::Int32(i) => Decimal::from(i),
                Value::Int64(i) => Decimal::from(i),
                Value::Float64(f) => Decimal::try_from(f)
                    .map_err(|_| anyhow!("invalid input syntax for type numeric: \"{}\"", f))?,
                Value::Text(s) => Decimal::from_str(s.trim())
                    .map_err(|_| anyhow!("invalid input syntax for type numeric: \"{}\"", s))?,
                other => {
                    return Err(anyhow!(
                        "cannot cast {} to numeric",
                        other.data_type().unwrap_or(crate::types::DataType::Text)
                    ))
                }
            };
            if let Some(s) = scale {
                d.rescale(s);
            }
            Ok(Value::Numeric(d))
        }
        (v, SqlType::Text) => Ok(Value::Text(v.to_string())),
        (v, SqlType::Varchar(len_opt)) => {
            let s = v.to_string();
            if let Some(sqlparser::ast::CharacterLength::IntegerLength { length, .. }) = len_opt {
                let max_len = *length as usize;
                if s.chars().count() > max_len {
                    return Ok(Value::Text(s.chars().take(max_len).collect()));
                }
            }
            Ok(Value::Text(s))
        }
        (v, SqlType::String(len_opt)) => {
            let s = v.to_string();
            if let Some(n) = len_opt {
                let max_len = *n as usize;
                if s.chars().count() > max_len {
                    return Ok(Value::Text(s.chars().take(max_len).collect()));
                }
            }
            Ok(Value::Text(s))
        }
        (Value::Text(s), SqlType::Int(_) | SqlType::Integer(_)) => {
            Ok(Value::Int32(s.trim().parse().unwrap_or(0)))
        }
        (Value::Text(s), SqlType::BigInt(_) | SqlType::Int8(_)) => {
            Ok(Value::Int64(s.trim().parse().unwrap_or(0)))
        }
        (Value::Text(s), SqlType::Float(_) | SqlType::Double | SqlType::Real) => {
            Ok(Value::Float64(s.trim().parse().unwrap_or(0.0)))
        }
        (Value::Text(s), SqlType::Boolean) => Ok(Value::Boolean(matches!(
            s.to_lowercase().as_str(),
            "true" | "t" | "yes" | "y" | "1"
        ))),
        (Value::Int32(n), SqlType::Boolean) => Ok(Value::Boolean(n != 0)),
        (Value::Int64(n), SqlType::Boolean) => Ok(Value::Boolean(n != 0)),
        (Value::Float64(n), SqlType::Boolean) => Ok(Value::Boolean(n != 0.0)),
        (Value::Numeric(d), SqlType::Boolean) => Ok(Value::Boolean(!d.is_zero())),
        (Value::Numeric(d), SqlType::Int(_) | SqlType::Integer(_)) => {
            use rust_decimal::prelude::ToPrimitive;
            use rust_decimal::RoundingStrategy;
            d.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
                .to_i32()
                .map(Value::Int32)
                .ok_or_else(|| anyhow!("numeric value out of range for integer"))
        }
        (Value::Numeric(d), SqlType::BigInt(_) | SqlType::Int8(_)) => {
            use rust_decimal::prelude::ToPrimitive;
            use rust_decimal::RoundingStrategy;
            d.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
                .to_i64()
                .map(Value::Int64)
                .ok_or_else(|| anyhow!("numeric value out of range for bigint"))
        }
        (Value::Numeric(d), SqlType::Float(_) | SqlType::Double | SqlType::Real) => {
            use rust_decimal::prelude::ToPrimitive;
            d.to_f64()
                .map(Value::Float64)
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))
        }
        (Value::Int32(n), SqlType::BigInt(_) | SqlType::Int8(_)) => Ok(Value::Int64(n as i64)),
        (Value::Int32(n), SqlType::Float(_) | SqlType::Double | SqlType::Real) => {
            Ok(Value::Float64(n as f64))
        }
        (Value::Int64(n), SqlType::Int(_) | SqlType::Integer(_)) => Ok(Value::Int32(n as i32)),
        (Value::Int64(n), SqlType::Float(_) | SqlType::Double | SqlType::Real) => {
            Ok(Value::Float64(n as f64))
        }
        (Value::Float64(n), SqlType::Int(_) | SqlType::Integer(_)) => {
            Ok(Value::Int32(round_half_away_from_zero(n) as i32))
        }
        (Value::Float64(n), SqlType::BigInt(_) | SqlType::Int8(_)) => {
            Ok(Value::Int64(round_half_away_from_zero(n) as i64))
        }
        (Value::Boolean(b), SqlType::Int(_) | SqlType::Integer(_)) => {
            Ok(Value::Int32(if b { 1 } else { 0 }))
        }
        (Value::Text(s), SqlType::Interval) => parse_interval_string(&s),
        (Value::Text(s), SqlType::Timestamp(_, _)) => parse_timestamp_string(&s),
        (Value::Timestamp(ts), SqlType::Timestamp(_, _)) => Ok(Value::Timestamp(ts)),
        (Value::Text(s), SqlType::Date) => crate::types::date::parse_date_days(&s).map(Value::Date),
        (Value::Timestamp(ts), SqlType::Date) => {
            crate::types::date::timestamp_millis_to_date_days(ts).map(Value::Date)
        }
        (Value::Date(days), SqlType::Date) => Ok(Value::Date(days)),
        (Value::Date(days), SqlType::Timestamp(_, _)) => {
            crate::types::date::date_days_to_timestamp_millis(days).map(Value::Timestamp)
        }
        (Value::Text(s), SqlType::Time(_, _)) => {
            use crate::sql::helpers::parse_time_string;
            parse_time_string(&s)
                .map(Value::Time)
                .ok_or_else(|| anyhow!("Invalid time format: {}", s))
        }
        (Value::Time(micros), SqlType::Time(_, _)) => Ok(Value::Time(micros)),
        (Value::Text(s), SqlType::Uuid) => {
            let uuid =
                uuid::Uuid::parse_str(s.trim()).map_err(|e| anyhow!("Invalid UUID: {}", e))?;
            Ok(Value::Uuid(*uuid.as_bytes()))
        }
        (Value::Uuid(bytes), SqlType::Uuid) => Ok(Value::Uuid(bytes)),
        (v, SqlType::Bytea) => cast_to_bytea(v),
        (v, SqlType::Custom(name, _)) => {
            if let Some(ident) = name.0.last() {
                let type_name = ident.value.to_uppercase();
                match type_name.as_str() {
                    "JSON" => {
                        let s = match &v {
                            Value::Text(s) => s.clone(),
                            Value::Json(s) => s.clone(),
                            Value::Jsonb(s) => s.clone(),
                            other => other.to_string(),
                        };
                        serde_json::from_str::<serde_json::Value>(&s)
                            .map_err(|e| anyhow!("invalid input syntax for type json: {}", e))?;
                        Ok(Value::Json(s))
                    }
                    "BYTEA" => cast_to_bytea(v),
                    "JSONB" => {
                        let s = match &v {
                            Value::Text(s) => s.clone(),
                            Value::Json(s) => s.clone(),
                            Value::Jsonb(s) => return Ok(Value::Jsonb(s.clone())),
                            other => other.to_string(),
                        };
                        let parsed: serde_json::Value = serde_json::from_str(&s)
                            .map_err(|e| anyhow!("invalid input syntax for type jsonb: {}", e))?;
                        Ok(Value::Jsonb(parsed.to_string()))
                    }
                    "VECTOR" => match &v {
                        Value::Text(s) => parse_vector_literal(s).map(Value::Vector),
                        Value::Vector(_) => Ok(v),
                        _ => Err(anyhow!("Cannot cast {} to vector", v)),
                    },
                    "REGTYPE" => {
                        let s = match &v {
                            Value::Text(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let s = s.replace('"', "");
                        let type_name = s.rsplit('.').next().unwrap_or_else(|| s.as_str()).trim();
                        Ok(Value::Text(type_name.to_string()))
                    }
                    _ => Ok(v),
                }
            } else {
                Ok(v)
            }
        }
        (v, SqlType::Regclass) => Ok(v),
        (v, _) => Ok(v),
    }
}

fn parse_interval_string(s: &str) -> Result<Value> {
    use crate::types::IntervalValue;
    let s = s.trim().to_lowercase();
    let mut total_months: i32 = 0;
    let mut total_ms: i64 = 0;

    let parts: Vec<&str> = s.split_whitespace().collect();
    let mut i = 0;
    while i < parts.len() {
        if let Ok(num) = parts[i].parse::<i64>() {
            if i + 1 < parts.len() {
                let unit = parts[i + 1].trim_end_matches('s');
                match unit {
                    "day" => total_ms += num * 24 * 60 * 60 * 1000,
                    "hour" => total_ms += num * 60 * 60 * 1000,
                    "minute" | "min" => total_ms += num * 60 * 1000,
                    "second" | "sec" => total_ms += num * 1000,
                    "millisecond" | "ms" => total_ms += num,
                    "week" => total_ms += num * 7 * 24 * 60 * 60 * 1000,
                    "month" | "mon" => total_months += num as i32,
                    "year" => total_months += (num * 12) as i32,
                    _ => return Err(anyhow!("Unknown interval unit: {}", parts[i + 1])),
                };
                i += 2;
            } else {
                return Err(anyhow!("Interval number without unit"));
            }
        } else {
            i += 1;
        }
    }

    Ok(Value::Interval(IntervalValue::new(total_months, total_ms)))
}

pub(super) fn parse_timestamp_string(s: &str) -> Result<Value> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s.trim()) {
        return Ok(Value::Timestamp(dt.timestamp_millis()));
    }

    use chrono::{NaiveDateTime, TimeZone, Utc};
    let formats = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d",
    ];
    for fmt in &formats {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s.trim(), fmt) {
            return Ok(Value::Timestamp(
                Utc.from_utc_datetime(&dt).timestamp_millis(),
            ));
        }
    }
    if let Ok(dt) = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
        let datetime = dt.and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("Failed to create datetime from date"))?;
        return Ok(Value::Timestamp(
            Utc.from_utc_datetime(&datetime).timestamp_millis(),
        ));
    }
    Err(anyhow!("Cannot parse timestamp: {}", s))
}

fn pg_to_chrono_format(fmt: &str) -> String {
    let upper = fmt.to_ascii_uppercase();
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        let rest_upper = &upper[i..];
        if rest_upper.starts_with("HH24") {
            out.push_str("%H");
            i += 4;
            continue;
        }
        if rest_upper.starts_with("YYYY") {
            out.push_str("%Y");
            i += 4;
            continue;
        }
        if rest_upper.starts_with("YY") {
            out.push_str("%y");
            i += 2;
            continue;
        }
        if rest_upper.starts_with("MM") {
            out.push_str("%m");
            i += 2;
            continue;
        }
        if rest_upper.starts_with("DD") {
            out.push_str("%d");
            i += 2;
            continue;
        }
        if rest_upper.starts_with("MI") {
            out.push_str("%M");
            i += 2;
            continue;
        }
        if rest_upper.starts_with("SS") {
            out.push_str("%S");
            i += 2;
            continue;
        }

        // Safety: loop condition guarantees fmt[i..] is non-empty
        let Some(ch) = fmt[i..].chars().next() else {
            break;
        };
        if ch == '%' {
            // chrono uses '%' for specifiers; escape literal '%' from to_char formats.
            out.push_str("%%");
        } else {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    out
}

fn eval_get_bit_from_args(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("get_bit requires exactly 2 arguments"));
    }
    let mut iter = args.into_iter();
    let bytes = match iter.next().unwrap_or(Value::Null) {
        Value::Bytes(b) => b,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow!("get_bit requires bytea as first argument")),
    };
    let bit_index = match iter.next().unwrap_or(Value::Null) {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow!("get_bit requires integer bit index")),
    };
    let bit = super::bytea::get_bit(&bytes, bit_index)?;
    Ok(Value::Int32(bit))
}

fn eval_set_bit_from_args(args: Vec<Value>) -> Result<Value> {
    if args.len() != 3 {
        return Err(anyhow!("set_bit requires exactly 3 arguments"));
    }
    let mut iter = args.into_iter();
    let bytes = match iter.next().unwrap_or(Value::Null) {
        Value::Bytes(b) => b,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow!("set_bit requires bytea as first argument")),
    };
    let bit_index = match iter.next().unwrap_or(Value::Null) {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow!("set_bit requires integer bit index")),
    };
    let new_value = match iter.next().unwrap_or(Value::Null) {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow!("set_bit requires integer new value")),
    };
    let bytes = super::bytea::set_bit(bytes, bit_index, new_value)?;
    Ok(Value::Bytes(bytes))
}

fn eval_int8send_from_args(args: Vec<Value>) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("int8send requires exactly 1 argument"));
    }
    match args.into_iter().next().unwrap_or(Value::Null) {
        Value::Int64(n) => Ok(Value::Bytes(super::bytea::int8send(n))),
        Value::Int32(n) => Ok(Value::Bytes(super::bytea::int8send(i64::from(n)))),
        Value::Null => Ok(Value::Null),
        _ => Err(anyhow!("int8send requires bigint argument")),
    }
}

fn eval_int4send_from_args(args: Vec<Value>) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("int4send requires exactly 1 argument"));
    }
    match args.into_iter().next().unwrap_or(Value::Null) {
        Value::Int32(n) => Ok(Value::Bytes(super::bytea::int4send(n))),
        Value::Int64(n) => Ok(Value::Bytes(super::bytea::int4send(
            i32::try_from(n).map_err(|_| anyhow!("int4send requires int4 argument"))?,
        ))),
        Value::Null => Ok(Value::Null),
        _ => Err(anyhow!("int4send requires int4 argument")),
    }
}

fn eval_uuid_send_from_args(args: Vec<Value>) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("uuid_send requires exactly 1 argument"));
    }
    match args.into_iter().next().unwrap_or(Value::Null) {
        Value::Uuid(bytes) => Ok(Value::Bytes(super::bytea::uuid_send(bytes))),
        Value::Text(s) => {
            let uuid =
                uuid::Uuid::parse_str(s.trim()).map_err(|e| anyhow!("Invalid UUID: {}", e))?;
            Ok(Value::Bytes(super::bytea::uuid_send(*uuid.as_bytes())))
        }
        Value::Null => Ok(Value::Null),
        _ => Err(anyhow!("uuid_send requires uuid argument")),
    }
}

fn eval_to_char_from_args(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let val = iter.next().unwrap_or(Value::Null);
    let fmt = iter.next().unwrap_or(Value::Null);

    let fmt = match fmt {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let chrono_fmt = pg_to_chrono_format(&fmt);

    match val {
        Value::Null => Ok(Value::Null),
        Value::Timestamp(ts) => {
            use chrono::{TimeZone, Utc};
            let dt = Utc
                .timestamp_millis_opt(ts)
                .single()
                .ok_or_else(|| anyhow!("Invalid timestamp"))?;
            Ok(Value::Text(dt.format(&chrono_fmt).to_string()))
        }
        Value::Date(days) => {
            let date = crate::types::date::date_days_to_naive_date(days)?;
            let dt = date
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| anyhow!("Invalid date"))?;
            Ok(Value::Text(dt.format(&chrono_fmt).to_string()))
        }
        other => Ok(Value::Text(other.to_string())),
    }
}

fn eval_date_trunc_from_args(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let field = match iter.next() {
        Some(Value::Text(s)) => s.to_lowercase(),
        _ => return Ok(Value::Null),
    };
    let ts = match iter.next() {
        Some(Value::Timestamp(t)) => t,
        Some(Value::Date(days)) => crate::types::date::date_days_to_timestamp_millis(days)?,
        Some(Value::Text(s)) => match parse_timestamp_string(&s)? {
            Value::Timestamp(ts) => ts,
            _ => return Ok(Value::Null),
        },
        _ => return Ok(Value::Null),
    };

    if field == "second" {
        return Ok(Value::Timestamp(ts.div_euclid(1000) * 1000));
    }

    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let dt = Utc
        .timestamp_millis_opt(ts)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    let truncated = match field.as_str() {
        "year" => chrono::NaiveDate::from_ymd_opt(dt.year(), 1, 1)
            .ok_or_else(|| anyhow!("Failed to create date for year truncation"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("Failed to create datetime for year truncation"))?
            .and_utc(),
        "month" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
            .ok_or_else(|| anyhow!("Failed to create date for month truncation"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("Failed to create datetime for month truncation"))?
            .and_utc(),
        "day" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())
            .ok_or_else(|| anyhow!("Failed to create date for day truncation"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("Failed to create datetime for day truncation"))?
            .and_utc(),
        "hour" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())
            .ok_or_else(|| anyhow!("Failed to create date for hour truncation"))?
            .and_hms_opt(dt.hour(), 0, 0)
            .ok_or_else(|| anyhow!("Failed to create datetime for hour truncation"))?
            .and_utc(),
        "minute" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())
            .ok_or_else(|| anyhow!("Failed to create date for minute truncation"))?
            .and_hms_opt(dt.hour(), dt.minute(), 0)
            .ok_or_else(|| anyhow!("Failed to create datetime for minute truncation"))?
            .and_utc(),
        _ => return Err(anyhow!("Unsupported DATE_TRUNC field: {}", field)),
    };
    Ok(Value::Timestamp(truncated.timestamp_millis()))
}

pub fn eval_value_public(v: &SqlValue) -> Result<Value> {
    eval_value(v)
}

pub fn eval_binary_op_public(left: Value, op: &BinaryOperator, right: Value) -> Result<Value> {
    eval_binary_op(left, op, right)
}

pub fn cast_value_public(val: Value, data_type: &sqlparser::ast::DataType) -> Result<Value> {
    cast_value(val, data_type)
}

fn eval_value(v: &SqlValue) -> Result<Value> {
    match v {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Boolean(b) => Ok(Value::Boolean(*b)),
        SqlValue::Number(n, _) => {
            if n.contains(['e', 'E']) {
                Ok(Value::Float64(n.parse()?))
            } else if n.contains('.') {
                if let Ok(d) = Decimal::from_str(n) {
                    Ok(Value::Numeric(d))
                } else {
                    Ok(Value::Float64(n.parse()?))
                }
            } else {
                if let Ok(i) = n.parse::<i32>() {
                    Ok(Value::Int32(i))
                } else {
                    Ok(Value::Int64(n.parse()?))
                }
            }
        }
        SqlValue::HexStringLiteral(s) => {
            Ok(Value::Bytes(hex::decode(s).map_err(|e| {
                anyhow!("Invalid hex string literal: {}", e)
            })?))
        }
        SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => {
            // Try parsing as vector if starts with [
            if s.starts_with('[') && s.ends_with(']') {
                if let Ok(vec) = parse_vector_literal(s) {
                    return Ok(Value::Vector(vec));
                }
            }
            Ok(Value::Text(s.clone()))
        }
        SqlValue::DollarQuotedString(s) => {
            let body = &s.value;
            if body.starts_with('[') && body.ends_with(']') {
                if let Ok(vec) = parse_vector_literal(body) {
                    return Ok(Value::Vector(vec));
                }
            }
            Ok(Value::Text(body.clone()))
        }
        _ => Err(anyhow!("Unsupported value literal: {:?}", v)),
    }
}

fn format_unrecognized_specifier_error(spec: char) -> String {
    format!(
        "unrecognized format() type specifier \"{}\"\nHINT:  For a single \"%\" use \"%%\".",
        spec
    )
}

fn quote_literal_impl(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn quote_ident_impl(ident: &str) -> String {
    let needs_quote = ident.is_empty() || !is_simple_unquoted_ident(ident) || is_sql_keyword(ident);
    if needs_quote {
        format!("\"{}\"", ident.replace('"', "\"\""))
    } else {
        ident.to_string()
    }
}

fn is_simple_unquoted_ident(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '$') {
            return false;
        }
    }
    true
}

fn is_sql_keyword(ident: &str) -> bool {
    let upper = ident.to_ascii_uppercase();
    sqlparser::keywords::ALL_KEYWORDS
        .binary_search(&upper.as_str())
        .is_ok()
}

fn eval_binary_op(left: Value, op: &BinaryOperator, right: Value) -> Result<Value> {
    operators::eval_binary_op(left, op, right)
}

pub fn compare_values(left: &Value, right: &Value) -> Result<i8> {
    operators::compare_values(left, right)
}

pub fn compare_order_by_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
    operators::compare_order_by_values(left, right, asc, nulls_first)
}

pub(super) fn eval_json_access(left: Value, operator: &JsonOperator, right: Value) -> Result<Value> {
    // `@>`/`<@` are overloaded by PostgreSQL for both SQL arrays and JSONB.
    if let Value::Array(left_arr) = &left {
        match operator {
            JsonOperator::AtArrow => {
                let Value::Array(right_arr) = &right else {
                    return Err(anyhow!("@> on arrays requires array operand on right"));
                };
                for r in right_arr {
                    if !left_arr.iter().any(|l| compare_values(l, r).unwrap_or(1) == 0) {
                        return Ok(Value::Boolean(false));
                    }
                }
                return Ok(Value::Boolean(true));
            }
            JsonOperator::ArrowAt => {
                let Value::Array(right_arr) = &right else {
                    return Err(anyhow!("<@ on arrays requires array operand on right"));
                };
                for l in left_arr {
                    if !right_arr.iter().any(|r| compare_values(l, r).unwrap_or(1) == 0) {
                        return Ok(Value::Boolean(false));
                    }
                }
                return Ok(Value::Boolean(true));
            }
            _ => return Err(anyhow!("Unsupported operator for arrays: {:?}", operator)),
        }
    }

    let json_str = match left {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
        Value::Null => return Ok(Value::Null),
        Value::Vector(v) => format!(
            "[{}]",
            v.iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => return Err(anyhow!("JSON operators require json/jsonb operand")),
    };

    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    match operator {
        JsonOperator::AtArrow => {
            let right_str = match right {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("@> requires json/jsonb operand on right")),
            };
            let right_json: serde_json::Value = serde_json::from_str(&right_str)
                .map_err(|e| anyhow!("Invalid JSON on right side of @>: {}", e))?;
            Ok(Value::Boolean(super::jsonb::contains(
                &json_val,
                &right_json,
            )))
        }
        JsonOperator::ArrowAt => {
            let right_str = match right {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("<@ requires json/jsonb operand on right")),
            };
            let right_json: serde_json::Value = serde_json::from_str(&right_str)
                .map_err(|e| anyhow!("Invalid JSON on right side of <@: {}", e))?;
            Ok(Value::Boolean(super::jsonb::contains(
                &right_json,
                &json_val,
            )))
        }
        JsonOperator::HashArrow | JsonOperator::HashLongArrow | JsonOperator::HashMinus => {
            let path = json_path_from_value(&right)?;
            match operator {
                JsonOperator::HashArrow => match json_get_path(&json_val, &path) {
                    Some(val) => Ok(Value::Jsonb(val.to_string())),
                    None => Ok(Value::Null),
                },
                JsonOperator::HashLongArrow => match json_get_path(&json_val, &path) {
                    None => Ok(Value::Null),
                    Some(serde_json::Value::Null) => Ok(Value::Null),
                    Some(serde_json::Value::String(s)) => Ok(Value::Text(s.clone())),
                    Some(other) => Ok(Value::Text(other.to_string())),
                },
                JsonOperator::HashMinus => {
                    json_delete_path(&mut json_val, &path);
                    Ok(Value::Jsonb(json_val.to_string()))
                }
                _ => Err(anyhow!("Unsupported JSON operator: {:?}", operator)),
            }
        }
        _ => {
            let accessed = match right {
                Value::Text(key) => json_val.get(&key),
                Value::Int32(idx) => {
                    if let Some(arr) = json_val.as_array() {
                        let idx = if idx < 0 {
                            (arr.len() as i32 + idx) as usize
                        } else {
                            idx as usize
                        };
                        arr.get(idx)
                    } else {
                        None
                    }
                }
                Value::Int64(idx) => {
                    if let Some(arr) = json_val.as_array() {
                        let idx = if idx < 0 {
                            (arr.len() as i64 + idx) as usize
                        } else {
                            idx as usize
                        };
                        arr.get(idx)
                    } else {
                        None
                    }
                }
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("JSON key must be text or integer")),
            };

            match accessed {
                None => Ok(Value::Null),
                Some(val) => match operator {
                    JsonOperator::Arrow => {
                        // -> operator returns JSONB type
                        // Client drivers will parse the JSONB value and extract the actual value
                        Ok(Value::Jsonb(val.to_string()))
                    }
                    JsonOperator::LongArrow => match val {
                        serde_json::Value::Null => Ok(Value::Null),
                        serde_json::Value::Bool(b) => Ok(Value::Text(b.to_string())),
                        serde_json::Value::Number(n) => Ok(Value::Text(n.to_string())),
                        serde_json::Value::String(s) => Ok(Value::Text(s.clone())),
                        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                            Ok(Value::Text(val.to_string()))
                        }
                    },
                    _ => Err(anyhow!("Unsupported JSON operator: {:?}", operator)),
                },
            }
        }
    }
}

fn parse_pg_text_array_literal(s: &str) -> Result<Vec<String>> {
    let trimmed = s.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(inner
            .split(',')
            .map(|p| p.trim().trim_matches('"').to_string())
            .collect());
    }
    Ok(vec![trimmed.to_string()])
}

fn json_path_from_value(path: &Value) -> Result<Vec<String>> {
    match path {
        Value::Array(arr) => Ok(arr
            .iter()
            .map(|v| match v {
                Value::Text(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()),
        Value::Text(s) => parse_pg_text_array_literal(s),
        Value::Null => Ok(Vec::new()),
        other => Err(anyhow!(
            "JSON path must be text[] or array literal, got {:?}",
            other
        )),
    }
}

fn json_get_path<'a>(
    mut current: &'a serde_json::Value,
    path: &[String],
) -> Option<&'a serde_json::Value> {
    for key in path {
        match current {
            serde_json::Value::Object(obj) => {
                current = obj.get(key)?;
            }
            serde_json::Value::Array(arr) => {
                let idx: i64 = key.parse().ok()?;
                let idx = if idx < 0 {
                    (arr.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn json_delete_path(current: &mut serde_json::Value, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.len() == 1 {
        let key = &path[0];
        match current {
            serde_json::Value::Object(obj) => obj.remove(key).is_some(),
            serde_json::Value::Array(arr) => {
                if let Ok(idx) = key.parse::<i64>() {
                    let idx = if idx < 0 { arr.len() as i64 + idx } else { idx };
                    if idx >= 0 && (idx as usize) < arr.len() {
                        arr.remove(idx as usize);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    } else {
        let key = &path[0];
        match current {
            serde_json::Value::Object(obj) => match obj.get_mut(key) {
                Some(child) => json_delete_path(child, &path[1..]),
                None => false,
            },
            serde_json::Value::Array(arr) => {
                let Ok(idx) = key.parse::<i64>() else {
                    return false;
                };
                let idx = if idx < 0 { arr.len() as i64 + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    json_delete_path(&mut arr[idx as usize], &path[1..])
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}

fn value_to_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Int32(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
        Value::Int64(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
        Value::Float64(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Json(s) | Value::Jsonb(s) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone()))
        }
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(value_to_json).collect()),
        Value::Timestamp(ts) => serde_json::Value::Number(serde_json::Number::from(*ts)),
        Value::Uuid(bytes) => serde_json::Value::String(uuid::Uuid::from_bytes(*bytes).to_string()),
        Value::Bytes(b) => serde_json::Value::String(format!("\\x{}", hex::encode(b))),
        Value::Interval(iv) => serde_json::Value::String(iv.to_string()),
        Value::Vector(vec) => serde_json::Value::Array(
            vec.iter()
                .filter_map(|f| serde_json::Number::from_f64(*f))
                .map(serde_json::Value::Number)
                .collect(),
        ),
        Value::Time(micros) => serde_json::Value::Number(serde_json::Number::from(*micros)),
        Value::Date(days) => serde_json::Value::String(
            crate::types::date::format_date_days(*days).unwrap_or_else(|_| days.to_string()),
        ),
        Value::Numeric(d) => {
            let s = d.to_string();
            if let Ok(i) = s.parse::<i64>() {
                return serde_json::Value::Number(serde_json::Number::from(i));
            }
            if let Ok(u) = s.parse::<u64>() {
                return serde_json::Value::Number(serde_json::Number::from(u));
            }
            if let Ok(f) = s.parse::<f64>() {
                if let Some(n) = serde_json::Number::from_f64(f) {
                    return serde_json::Value::Number(n);
                }
            }
            serde_json::Value::String(s)
        }
    }
}

fn parse_vector_literal(s: &str) -> Result<Vec<f64>> {
    let s = s.trim();
    if !s.starts_with('[') || !s.ends_with(']') {
        return Err(anyhow!("Vector literal must be enclosed in brackets"));
    }

    let s = &s[1..s.len() - 1]; // Remove brackets
    if s.is_empty() {
        return Ok(Vec::new());
    }

    let elements: Result<Vec<f64>> = s
        .split(',')
        .map(|elem| {
            elem.trim()
                .parse::<f64>()
                .with_context(|| format!("Invalid vector element: {}", elem))
        })
        .collect();

    elements
}

/// Extract a vector from a Value (handles both Vector and Array types)
fn extract_vector(val: &Value) -> Result<Vec<f64>> {
    match val {
        Value::Vector(vec) => Ok(vec.clone()),
        Value::Array(arr) => {
            // Convert array of numbers to vector
            arr.iter()
                .map(|v| match v {
                    Value::Float64(f) => Ok(*f),
                    Value::Int32(i) => Ok(*i as f64),
                    Value::Int64(i) => Ok(*i as f64),
                    _ => Err(anyhow!("Vector elements must be numeric")),
                })
                .collect()
        }
        Value::Text(s) => {
            // Parse text representation like "[1.0, 2.0, 3.0]"
            let s = s.trim();
            if !s.starts_with('[') || !s.ends_with(']') {
                return Err(anyhow!("Invalid vector format: expected [...]"));
            }
            let inner = &s[1..s.len() - 1];
            if inner.is_empty() {
                return Ok(Vec::new());
            }
            inner
                .split(',')
                .map(|elem| {
                    elem.trim()
                        .parse::<f64>()
                        .map_err(|_| anyhow!("Invalid vector element: {}", elem))
                })
                .collect()
        }
        _ => Err(anyhow!("Expected vector, array, or text type")),
    }
}

/// Calculate L2 (Euclidean) distance between two vectors
fn l2_distance(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    let sum: f64 = vec1
        .iter()
        .zip(vec2.iter())
        .map(|(a, b)| {
            let diff = a - b;
            diff * diff
        })
        .sum();

    Ok(sum.sqrt())
}

/// Calculate cosine distance between two vectors (1 - cosine similarity)
fn cosine_distance(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    let mut dot_product = 0.0;
    let mut norm1 = 0.0;
    let mut norm2 = 0.0;

    for (a, b) in vec1.iter().zip(vec2.iter()) {
        dot_product += a * b;
        norm1 += a * a;
        norm2 += b * b;
    }

    if norm1 == 0.0 || norm2 == 0.0 {
        return Ok(1.0); // Maximum distance if either vector is zero
    }

    let cosine_similarity = dot_product / (norm1.sqrt() * norm2.sqrt());
    // Clamp to [-1, 1] to handle floating point errors
    let cosine_similarity = cosine_similarity.max(-1.0).min(1.0);

    // Return distance: 1 - similarity
    Ok(1.0 - cosine_similarity)
}

/// Calculate inner product (negative for ORDER BY compatibility)
fn inner_product(vec1: &[f64], vec2: &[f64]) -> Result<f64> {
    if vec1.len() != vec2.len() {
        return Err(anyhow!(
            "Vectors must have same dimensions ({} vs {})",
            vec1.len(),
            vec2.len()
        ));
    }

    let dot: f64 = vec1.iter().zip(vec2.iter()).map(|(a, b)| a * b).sum();

    // Return negative inner product (for ORDER BY compatibility like pgvector)
    Ok(-dot)
}

/// Calculate Euclidean norm (L2 norm) of a vector
fn vector_norm(vec: &[f64]) -> f64 {
    vec.iter().map(|x| x * x).sum::<f64>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::statement_time;
    use crate::types::ColumnDef;
    use rust_decimal::Decimal;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashMap;
    use std::str::FromStr;

    fn parse_expr(sql: &str) -> Expr {
        let full_sql = format!("SELECT {}", sql);
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, &full_sql).unwrap();
        if let sqlparser::ast::Statement::Query(q) = &ast[0] {
            if let sqlparser::ast::SetExpr::Select(s) = &*q.body {
                if let sqlparser::ast::SelectItem::UnnamedExpr(e) = &s.projection[0] {
                    return e.clone();
                }
            }
        }
        panic!("Failed to parse expression");
    }

    #[test]
    fn test_version_includes_pg_tikv() {
        let v = eval_expr(&parse_expr("version()"), None, None).unwrap();
        let Value::Text(s) = v else {
            panic!("version() must return text");
        };
        assert!(s.starts_with("PostgreSQL "));
        assert!(s.contains("pg-tikv "));
    }

    #[test]
    fn test_eval_expr_join_function_resolves_qualified_column() {
        let expr = parse_expr("LOWER(b.name)");

        let combined_schema = TableSchema {
            name: "join".to_string(),
            columns: vec![
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            ..Default::default()
        };

        let combined_row = Row::new(vec![
            Value::Text("Alice".to_string()),
            Value::Text("Bob".to_string()),
        ]);

        let mut column_offsets = HashMap::new();
        column_offsets.insert("a.name".to_string(), 0);
        column_offsets.insert("b.name".to_string(), 1);
        column_offsets.insert("name".to_string(), 0);

        let ctx = JoinContext {
            tables: HashMap::new(),
            column_offsets,
            combined_row: &combined_row,
            combined_schema: &combined_schema,
        };

        let val = eval_expr_join(&expr, &ctx).unwrap();
        assert_eq!(val, Value::Text("bob".to_string()));
    }

    #[test]
    fn test_eval_literal_values() {
        assert_eq!(
            eval_expr(&parse_expr("42"), None, None).unwrap(),
            Value::Int32(42)
        );
        assert_eq!(
            eval_expr(&parse_expr("3.14"), None, None).unwrap(),
            Value::Numeric(Decimal::from_str("3.14").unwrap())
        );
        assert_eq!(
            eval_expr(&parse_expr("'hello'"), None, None).unwrap(),
            Value::Text("hello".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("$$hello$$"), None, None).unwrap(),
            Value::Text("hello".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("$tag$hello$tag$"), None, None).unwrap(),
            Value::Text("hello".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("$$ $1 $$"), None, None).unwrap(),
            Value::Text(" $1 ".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("NULL"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("true"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("false"), None, None).unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_at_time_zone_timestamp_to_timestamptz() {
        let expr = parse_expr("TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC'");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, parse_timestamp_string("2024-01-15 10:00:00").unwrap());
    }

    #[test]
    fn test_at_time_zone_timestamp_to_timestamptz_with_offset() {
        let expr = parse_expr("TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'Asia/Shanghai'");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, parse_timestamp_string("2024-01-15 02:00:00").unwrap());
    }

    #[test]
    fn test_at_time_zone_chain_conversion() {
        let expr = parse_expr(
            "TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'",
        );
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, parse_timestamp_string("2024-01-15 05:00:00").unwrap());
    }

    #[test]
    fn test_at_time_zone_timestamptz_to_timestamp() {
        let expr =
            parse_expr("TIMESTAMPTZ '2024-01-15T10:00:00Z' AT TIME ZONE 'America/New_York'");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, parse_timestamp_string("2024-01-15 05:00:00").unwrap());
    }

    #[test]
    fn test_eval_arithmetic() {
        assert_eq!(
            eval_expr(&parse_expr("1 + 2"), None, None).unwrap(),
            Value::Int32(3)
        );
        assert_eq!(
            eval_expr(&parse_expr("10 - 4"), None, None).unwrap(),
            Value::Int32(6)
        );
        assert_eq!(
            eval_expr(&parse_expr("3 * 5"), None, None).unwrap(),
            Value::Int32(15)
        );
        assert_eq!(
            eval_expr(&parse_expr("20 / 4"), None, None).unwrap(),
            Value::Int32(5)
        );
        assert_eq!(
            eval_expr(&parse_expr("17 % 5"), None, None).unwrap(),
            Value::Int32(2)
        );
    }

    #[test]
    fn test_eval_comparison() {
        assert_eq!(
            eval_expr(&parse_expr("5 > 3"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 < 3"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 = 5"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 <> 3"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 >= 5"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 <= 6"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_eval_logical() {
        assert_eq!(
            eval_expr(&parse_expr("true AND true"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("true AND false"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("true OR false"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("false OR false"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("NOT true"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("NOT false"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_eval_logical_null_semantics_and_short_circuit() {
        assert_eq!(
            eval_expr(&parse_expr("CAST(NULL AS BOOLEAN) AND TRUE"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("FALSE AND CAST(NULL AS BOOLEAN)"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("CAST(NULL AS BOOLEAN) OR TRUE"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("NOT CAST(NULL AS BOOLEAN)"), None, None).unwrap(),
            Value::Null
        );

        // Short-circuit: RHS must not be evaluated when LHS determines result
        assert_eq!(
            eval_expr(&parse_expr("FALSE AND (1 / 0 = 0)"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("TRUE OR (1 / 0 = 0)"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

	    #[test]
	    fn test_eval_logical_short_circuit_does_not_hide_type_errors() {
	        // Short-circuit must not mask RHS type errors.
	        assert!(eval_expr(&parse_expr("TRUE OR 42"), None, None).is_err());
	        assert!(eval_expr(&parse_expr("FALSE AND 1 / 0"), None, None).is_err());
	        assert!(eval_expr(&parse_expr("TRUE OR (FALSE AND 42)"), None, None).is_err());
	        assert!(eval_expr(&parse_expr("FALSE AND (TRUE OR 42)"), None, None).is_err());
	        assert!(
	            eval_expr(&parse_expr("TRUE OR (1 LIKE 'a%')"), None, None)
	                .unwrap_err()
	                .to_string()
	                .contains("LIKE requires text operands")
	        );
	        assert!(
	            eval_expr(&parse_expr("FALSE AND (1 ILIKE 'a%')"), None, None)
	                .unwrap_err()
	                .to_string()
	                .contains("ILIKE requires text operands")
	        );
	        assert!(
	            eval_expr(&parse_expr("TRUE OR (1 && 2)"), None, None)
	                .unwrap_err()
	                .to_string()
	                .contains("&& operator requires array operands")
	        );

	        // Explicit NULL is allowed as a boolean operand.
	        assert_eq!(
	            eval_expr(&parse_expr("FALSE AND NULL"), None, None).unwrap(),
	            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("TRUE OR NULL"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_eval_nested() {
        assert_eq!(
            eval_expr(&parse_expr("(1 + 2) * 3"), None, None).unwrap(),
            Value::Int32(9)
        );
        assert_eq!(
            eval_expr(&parse_expr("10 / (2 + 3)"), None, None).unwrap(),
            Value::Int32(2)
        );
    }

    #[test]
    fn test_eval_unary_minus() {
        assert_eq!(
            eval_expr(&parse_expr("-5"), None, None).unwrap(),
            Value::Int32(-5)
        );
        assert_eq!(
            eval_expr(&parse_expr("-3.14"), None, None).unwrap(),
            Value::Numeric(Decimal::from_str("-3.14").unwrap())
        );
        assert_eq!(
            eval_expr(&parse_expr("-'10'"), None, None).unwrap(),
            Value::Int32(-10)
        );
        assert_eq!(
            eval_expr(&parse_expr("-'1.5'"), None, None).unwrap(),
            Value::Float64(-1.5)
        );
        assert!(eval_expr(&parse_expr("-'nope'"), None, None).is_err());
    }

    #[test]
    fn test_eval_is_null() {
        assert_eq!(
            eval_expr(&parse_expr("NULL IS NULL"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 IS NULL"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("NULL IS NOT NULL"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 IS NOT NULL"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_eval_in_list() {
        assert_eq!(
            eval_expr(&parse_expr("5 IN (1, 3, 5, 7)"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("4 IN (1, 3, 5, 7)"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("4 NOT IN (1, 3, 5, 7)"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("'a' IN ('a', 'b', 'c')"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_eval_between() {
        assert_eq!(
            eval_expr(&parse_expr("5 BETWEEN 1 AND 10"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("15 BETWEEN 1 AND 10"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("5 NOT BETWEEN 10 AND 20"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("1 BETWEEN 1 AND 1"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_compare_values() {
        assert_eq!(
            compare_values(&Value::Int32(5), &Value::Int32(3)).unwrap(),
            1
        );
        assert_eq!(
            compare_values(&Value::Int32(3), &Value::Int32(5)).unwrap(),
            -1
        );
        assert_eq!(
            compare_values(&Value::Int32(5), &Value::Int32(5)).unwrap(),
            0
        );
        assert_eq!(
            compare_values(&Value::Text("b".to_string()), &Value::Text("a".to_string())).unwrap(),
            1
        );
        assert_eq!(
            compare_values(&Value::Bytes(vec![0x00]), &Value::Bytes(vec![0x01])).unwrap(),
            -1
        );
        assert_eq!(
            compare_values(&Value::Bytes(vec![0x01, 0x00]), &Value::Bytes(vec![0x01])).unwrap(),
            1
        );
        assert_eq!(
            compare_values(
                &Value::Bytes(vec![0xde, 0xad]),
                &Value::Bytes(vec![0xde, 0xad])
            )
            .unwrap(),
            0
        );
        assert_eq!(compare_values(&Value::Null, &Value::Int32(5)).unwrap(), -1);
        assert_eq!(compare_values(&Value::Int32(5), &Value::Null).unwrap(), 1);

        assert_eq!(
            compare_values(&Value::Boolean(true), &Value::Text("true".to_string())).unwrap(),
            0
        );
        assert_eq!(
            compare_values(&Value::Text("false".to_string()), &Value::Boolean(false)).unwrap(),
            0
        );
        assert!(compare_values(&Value::Boolean(true), &Value::Text("nope".to_string())).is_err());

        // PostgreSQL-like float NaN semantics:
        // - NaN compares equal to NaN
        // - NaN compares greater than all non-NaN values
        assert_eq!(
            compare_values(&Value::Float64(f64::NAN), &Value::Float64(1.0)).unwrap(),
            1
        );
        assert_eq!(
            compare_values(&Value::Float64(1.0), &Value::Float64(f64::NAN)).unwrap(),
            -1
        );
        assert_eq!(
            compare_values(&Value::Float64(f64::NAN), &Value::Float64(f64::NAN)).unwrap(),
            0
        );
        assert_eq!(
            compare_values(&Value::Float64(f64::NAN), &Value::Int32(1)).unwrap(),
            1
        );
        assert_eq!(
            compare_values(&Value::Int32(1), &Value::Float64(f64::NAN)).unwrap(),
            -1
        );
    }

    #[test]
    fn test_float_nan_comparisons() {
        assert_eq!(
            eval_expr(&parse_expr("CAST('NaN' AS DOUBLE PRECISION) = 1"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(
                &parse_expr(
                    "CAST('NaN' AS DOUBLE PRECISION) = CAST('NaN' AS DOUBLE PRECISION)"
                ),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("CAST('NaN' AS DOUBLE PRECISION) > 1"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_compare_order_by_values_nulls() {
        use std::cmp::Ordering;

        // ASC defaults to NULLS LAST.
        assert_eq!(
            compare_order_by_values(&Value::Null, &Value::Date(0), true, false),
            Ordering::Greater
        );
        assert_eq!(
            compare_order_by_values(&Value::Date(0), &Value::Null, true, false),
            Ordering::Less
        );

        // DESC defaults to NULLS FIRST.
        assert_eq!(
            compare_order_by_values(&Value::Null, &Value::Date(0), false, true),
            Ordering::Less
        );
        assert_eq!(
            compare_order_by_values(&Value::Date(0), &Value::Null, false, true),
            Ordering::Greater
        );
    }

    #[test]
    fn test_compare_order_by_values_nan() {
        use std::cmp::Ordering;

        assert_eq!(
            compare_order_by_values(&Value::Float64(f64::NAN), &Value::Float64(1.0), true, false),
            Ordering::Greater
        );
        assert_eq!(
            compare_order_by_values(&Value::Float64(1.0), &Value::Float64(f64::NAN), true, false),
            Ordering::Less
        );

        // DESC should put NaN first (since NaN is treated as greatest).
        assert_eq!(
            compare_order_by_values(&Value::Float64(f64::NAN), &Value::Float64(1.0), false, false),
            Ordering::Less
        );
    }

    #[test]
    fn test_jsonb_exists_function() {
        assert_eq!(
            eval_expr(
                &parse_expr("JSONB_EXISTS('{\"a\": 1, \"b\": 2}'::jsonb, 'a')"),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(
                &parse_expr("JSONB_EXISTS('{\"a\": 1}'::jsonb, 'c')"),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(
                &parse_expr("JSONB_EXISTS('[\"a\", \"b\"]'::jsonb, 'b')"),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_to_char_format_tokens() {
        assert_eq!(
            eval_expr(
                &parse_expr("TO_CHAR(TIMESTAMP '2024-01-15 14:30:45', 'YYYY-MM')"),
                None,
                None
            )
            .unwrap(),
            Value::Text("2024-01".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("TO_CHAR(DATE '2024-01-15', 'YYYY-MM')"),
                None,
                None
            )
            .unwrap(),
            Value::Text("2024-01".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("TO_CHAR(TIMESTAMP '2024-01-15 14:30:45', 'YYYY-MM-DD HH24:MI:SS')"),
                None,
                None
            )
            .unwrap(),
            Value::Text("2024-01-15 14:30:45".to_string())
        );
    }

    #[test]
    fn test_null_comparison_three_valued_logic() {
        assert_eq!(
            eval_expr(&parse_expr("NULL = 5"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("5 = NULL"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("NULL >= 0"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("NULL < 10"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("NULL <> 5"), None, None).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_division_by_zero() {
        assert!(eval_expr(&parse_expr("5 / 0"), None, None).is_err());
        assert!(eval_expr(&parse_expr("5 % 0"), None, None).is_err());
    }

    #[test]
    fn test_function_args_do_not_drop_errors() {
        assert!(eval_expr(&parse_expr("COALESCE(5 / 0, 1)"), None, None).is_err());
        assert!(eval_expr(&parse_expr("COALESCE(NULL, 5 / 0, 1)"), None, None).is_err());
        assert_eq!(
            eval_expr(&parse_expr("COALESCE(1, 5 / 0)"), None, None).unwrap(),
            Value::Int32(1)
        );
        assert!(eval_expr(&parse_expr("NULLIF(5 / 0, 1)"), None, None).is_err());
        assert!(eval_expr(&parse_expr("GREATEST(1, 5 / 0)"), None, None).is_err());
    }

    #[test]
    fn test_function_column_references_use_row_context() {
        use crate::types::ColumnDef;

        let schema = TableSchema::new(
            "public.atm_users".to_string(),
            1,
            vec![ColumnDef {
                name: "nickname".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![],
        );
        let row = Row::new(vec![Value::Null]);
        assert_eq!(
            eval_expr(
                &parse_expr("COALESCE(nickname, 'NULL')"),
                Some(&row),
                Some(&schema),
            )
            .unwrap(),
            Value::Text("NULL".to_string())
        );

        let row_with_value = Row::new(vec![Value::Text("hi".to_string())]);
        assert_eq!(
            eval_expr(
                &parse_expr("COALESCE(nickname, 'NULL')"),
                Some(&row_with_value),
                Some(&schema)
            )
            .unwrap(),
            Value::Text("hi".to_string())
        );
    }

    #[test]
    fn test_mixed_type_arithmetic() {
        let result = eval_expr(&parse_expr("1 + 2.5"), None, None).unwrap();
        assert_eq!(result, Value::Numeric(Decimal::from_str("3.5").unwrap()));
    }

    #[test]
    fn test_string_concat() {
        assert_eq!(
            eval_expr(&parse_expr("'Hello' || ' ' || 'World'"), None, None).unwrap(),
            Value::Text("Hello World".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("'Count: ' || 42"), None, None).unwrap(),
            Value::Text("Count: 42".to_string())
        );
    }

    #[test]
    fn test_case_when() {
        assert_eq!(
            eval_expr(
                &parse_expr("CASE WHEN 1 = 1 THEN 'yes' ELSE 'no' END"),
                None,
                None
            )
            .unwrap(),
            Value::Text("yes".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("CASE WHEN 1 = 2 THEN 'yes' ELSE 'no' END"),
                None,
                None
            )
            .unwrap(),
            Value::Text("no".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("CASE WHEN 'true' THEN 'yes' ELSE 'no' END"),
                None,
                None
            )
            .unwrap(),
            Value::Text("yes".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("CASE WHEN 'false' THEN 'yes' ELSE 'no' END"),
                None,
                None
            )
            .unwrap(),
            Value::Text("no".to_string())
        );
        assert!(
            eval_expr(
                &parse_expr("CASE WHEN 'nope' THEN 'yes' ELSE 'no' END"),
                None,
                None
            )
            .is_err()
        );
        assert_eq!(
            eval_expr(
                &parse_expr("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END"),
                None,
                None
            )
            .unwrap(),
            Value::Text("two".to_string())
        );
    }

    #[test]
    fn test_string_functions() {
        assert_eq!(
            eval_expr(&parse_expr("UPPER('hello')"), None, None).unwrap(),
            Value::Text("HELLO".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("LOWER('HELLO')"), None, None).unwrap(),
            Value::Text("hello".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("LENGTH('hello')"), None, None).unwrap(),
            Value::Int32(5)
        );
        assert_eq!(
            eval_expr(&parse_expr("CONCAT('a', 'b', 'c')"), None, None).unwrap(),
            Value::Text("abc".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("LEFT('hello', 2)"), None, None).unwrap(),
            Value::Text("he".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("RIGHT('hello', 2)"), None, None).unwrap(),
            Value::Text("lo".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("REPLACE('hello world', 'world', 'there')"),
                None,
                None
            )
            .unwrap(),
            Value::Text("hello there".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("REVERSE('hello')"), None, None).unwrap(),
            Value::Text("olleh".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("REPEAT('ab', 3)"), None, None).unwrap(),
            Value::Text("ababab".to_string())
        );
    }

    #[test]
    fn test_math_functions() {
        assert_eq!(
            eval_expr(&parse_expr("ABS(-5)"), None, None).unwrap(),
            Value::Int32(5)
        );
        assert_eq!(
            eval_expr(&parse_expr("CEIL(4.3)"), None, None).unwrap(),
            Value::Float64(5.0)
        );
        assert_eq!(
            eval_expr(&parse_expr("FLOOR(4.7)"), None, None).unwrap(),
            Value::Float64(4.0)
        );
        let round_result = eval_expr(&parse_expr("ROUND(4.567, 2)"), None, None).unwrap();
        assert!(matches!(round_result, Value::Float64(f) if (f - 4.57).abs() < 0.001));
        assert_eq!(
            eval_expr(&parse_expr("SQRT(16)"), None, None).unwrap(),
            Value::Float64(4.0)
        );
        assert_eq!(
            eval_expr(&parse_expr("POWER(2, 10)"), None, None).unwrap(),
            Value::Float64(1024.0)
        );
        assert_eq!(
            eval_expr(&parse_expr("MOD(17, 5)"), None, None).unwrap(),
            Value::Int32(2)
        );
        assert_eq!(
            eval_expr(&parse_expr("SIGN(-5)"), None, None).unwrap(),
            Value::Int32(-1)
        );
    }

    #[test]
    fn test_coalesce_nullif() {
        assert_eq!(
            eval_expr(&parse_expr("COALESCE(NULL, NULL, 'default')"), None, None).unwrap(),
            Value::Text("default".to_string())
        );
        assert_eq!(
            eval_expr(
                &parse_expr("COALESCE('first', NULL, 'default')"),
                None,
                None
            )
            .unwrap(),
            Value::Text("first".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("NULLIF(5, 5)"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("NULLIF(5, 3)"), None, None).unwrap(),
            Value::Int32(5)
        );
    }

    #[test]
    fn test_greatest_least() {
        assert_eq!(
            eval_expr(&parse_expr("GREATEST(1, 5, 3)"), None, None).unwrap(),
            Value::Int32(5)
        );
        assert_eq!(
            eval_expr(&parse_expr("LEAST(1, 5, 3)"), None, None).unwrap(),
            Value::Int32(1)
        );
    }

    #[test]
    fn test_like_pattern() {
        assert_eq!(
            eval_expr(&parse_expr("'hello' LIKE 'h%'"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("'hello' LIKE '%llo'"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("'hello' LIKE 'h_llo'"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("'hello' LIKE 'world'"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("'hello' NOT LIKE 'world'"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("'hello' LIKE '%.%'"), None, None).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(&parse_expr("'a.b' LIKE '%.%'"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_like_null_semantics() {
        assert_eq!(
            eval_expr(&parse_expr("CAST(NULL AS TEXT) LIKE 'a%'"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("'a' LIKE CAST(NULL AS TEXT)"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("CAST(NULL AS TEXT) ILIKE 'a%'"), None, None).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_expr(&parse_expr("'a' ILIKE CAST(NULL AS TEXT)"), None, None).unwrap(),
            Value::Null
        );
        assert!(eval_expr(&parse_expr("1 LIKE NULL"), None, None).is_err());
        assert!(eval_expr(&parse_expr("NULL LIKE 1"), None, None).is_err());
    }

    #[test]
    fn test_ilike_pattern() {
        assert_eq!(
            eval_expr(&parse_expr("'Hello' ILIKE 'h%'"), None, None).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(&parse_expr("'HELLO' ILIKE '%llo'"), None, None).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_cast() {
        assert_eq!(
            eval_expr(&parse_expr("CAST(123 AS TEXT)"), None, None).unwrap(),
            Value::Text("123".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("CAST('456' AS INTEGER)"), None, None).unwrap(),
            Value::Int32(456)
        );
        assert_eq!(
            eval_expr(&parse_expr("CAST(3.14 AS INTEGER)"), None, None).unwrap(),
            Value::Int32(3)
        );
        assert_eq!(
            eval_expr(&parse_expr("'123'::int8"), None, None).unwrap(),
            Value::Int64(123)
        );
        assert_eq!(
            eval_expr(&parse_expr("'456'::bigint"), None, None).unwrap(),
            Value::Int64(456)
        );
        assert_eq!(
            eval_expr(&parse_expr("123::text"), None, None).unwrap(),
            Value::Text("123".to_string())
        );
    }

    #[test]
    fn test_trim() {
        assert_eq!(
            eval_expr(&parse_expr("TRIM('  hello  ')"), None, None).unwrap(),
            Value::Text("hello".to_string())
        );
    }

    #[test]
    fn test_position() {
        assert_eq!(
            eval_expr(&parse_expr("POSITION('lo' IN 'hello')"), None, None).unwrap(),
            Value::Int32(4)
        );
        assert_eq!(
            eval_expr(&parse_expr("POSITION('xyz' IN 'hello')"), None, None).unwrap(),
            Value::Int32(0)
        );
    }

    #[test]
    fn test_substring() {
        assert_eq!(
            eval_expr(&parse_expr("SUBSTRING('hello' FROM 2 FOR 3)"), None, None).unwrap(),
            Value::Text("ell".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("SUBSTRING('hello' FROM 2)"), None, None).unwrap(),
            Value::Text("ello".to_string())
        );
    }

    #[test]
    fn test_interval_parsing() {
        use crate::types::IntervalValue;
        let result = parse_interval_string("1 day").unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
        );

        let result = parse_interval_string("2 hours").unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(2 * 60 * 60 * 1000))
        );

        let result = parse_interval_string("30 minutes").unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(30 * 60 * 1000))
        );

        let result = parse_interval_string("1 week").unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(7 * 24 * 60 * 60 * 1000))
        );

        let result = parse_interval_string("1 month").unwrap();
        assert_eq!(result, Value::Interval(IntervalValue::from_months(1)));
    }

    #[test]
    fn test_interval_expression() {
        use crate::types::IntervalValue;
        let result = eval_expr(&parse_expr("INTERVAL '1 day'"), None, None).unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
        );

        let result = eval_expr(&parse_expr("INTERVAL '2' DAY"), None, None).unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(2 * 24 * 60 * 60 * 1000))
        );

        let result = eval_expr(&parse_expr("INTERVAL '3' HOUR"), None, None).unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(3 * 60 * 60 * 1000))
        );

        let result = eval_expr(&parse_expr("INTERVAL '1' MONTH"), None, None).unwrap();
        assert_eq!(result, Value::Interval(IntervalValue::from_months(1)));
    }

    #[test]
    fn test_timestamp_interval_arithmetic() {
        use crate::types::IntervalValue;
        let ts = Value::Timestamp(1000 * 60 * 60 * 24);
        let iv = Value::Interval(IntervalValue::from_millis(1000 * 60 * 60));

        let result = operators::add_values(ts.clone(), iv.clone()).unwrap();
        assert_eq!(result, Value::Timestamp(1000 * 60 * 60 * 25));

        let result = operators::sub_values(ts.clone(), iv.clone()).unwrap();
        assert_eq!(result, Value::Timestamp(1000 * 60 * 60 * 23));
    }

    #[test]
    fn test_timestamp_cast() {
        let result = parse_timestamp_string("2024-01-01 00:00:00").unwrap();
        assert!(matches!(result, Value::Timestamp(_)));

        let result = parse_timestamp_string("2024-01-01").unwrap();
        assert!(matches!(result, Value::Timestamp(_)));

        let result = parse_timestamp_string("2026-01-22T04:36:12.931807").unwrap();
        assert!(matches!(result, Value::Timestamp(_)));

        let result = parse_timestamp_string("2024-01-15T10:30:00").unwrap();
        assert!(matches!(result, Value::Timestamp(_)));
    }

    #[test]
    fn test_now_plus_interval() {
        let result = eval_expr(&parse_expr("NOW() + INTERVAL '1 DAY'"), None, None).unwrap();
        assert!(matches!(result, Value::Timestamp(_)));
    }

    #[test]
    fn test_string_concat_to_interval() {
        use crate::types::IntervalValue;
        let result = eval_expr(&parse_expr("('1' || ' day')::interval"), None, None).unwrap();
        assert_eq!(
            result,
            Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
        );
    }

    #[test]
    fn test_complex_datetime_expression() {
        let result = eval_expr(
            &parse_expr("now()::timestamp + ('1' || ' day')::interval"),
            None,
            None,
        )
        .unwrap();
        assert!(matches!(result, Value::Timestamp(_)));
    }

    #[test]
    fn test_int8_cast_from_int() {
        assert_eq!(
            eval_expr(&parse_expr("42::int8"), None, None).unwrap(),
            Value::Int64(42)
        );
    }

    #[test]
    fn test_int8_cast_from_text() {
        assert_eq!(
            eval_expr(&parse_expr("'999'::int8"), None, None).unwrap(),
            Value::Int64(999)
        );
    }

    #[test]
    fn test_gen_random_uuid() {
        let result = eval_expr(&parse_expr("gen_random_uuid()"), None, None).unwrap();
        assert!(matches!(result, Value::Uuid(_)));
    }

    #[test]
    fn test_uuid_cast_from_text() {
        let result = eval_expr(
            &parse_expr("'550e8400-e29b-41d4-a716-446655440000'::uuid"),
            None,
            None,
        )
        .unwrap();
        if let Value::Uuid(bytes) = result {
            let uuid = uuid::Uuid::from_bytes(bytes);
            assert_eq!(uuid.to_string(), "550e8400-e29b-41d4-a716-446655440000");
        } else {
            panic!("Expected UUID value");
        }
    }

    #[test]
    fn test_bytea_send_functions() {
        assert_eq!(
            eval_expr(&parse_expr("int8send(72623859790382856::bigint)"), None, None).unwrap(),
            Value::Bytes(vec![1, 2, 3, 4, 5, 6, 7, 8])
        );
        assert_eq!(
            eval_expr(&parse_expr("int4send(16909060)"), None, None).unwrap(),
            Value::Bytes(vec![1, 2, 3, 4])
        );

        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            eval_expr(
                &parse_expr("uuid_send('550e8400-e29b-41d4-a716-446655440000'::uuid)"),
                None,
                None
            )
            .unwrap(),
            Value::Bytes(uuid.as_bytes().to_vec())
        );
    }

    #[test]
    fn test_set_bit_get_bit_bytea() {
        assert_eq!(
            eval_expr(&parse_expr(r"set_bit('\x00'::bytea, 0, 1)"), None, None).unwrap(),
            Value::Bytes(vec![0x80])
        );
        assert_eq!(
            eval_expr(&parse_expr(r"set_bit('\x00'::bytea, 7, 1)"), None, None).unwrap(),
            Value::Bytes(vec![0x01])
        );
        assert_eq!(
            eval_expr(&parse_expr(r"get_bit('\x80'::bytea, 0)"), None, None).unwrap(),
            Value::Int32(1)
        );
        assert_eq!(
            eval_expr(&parse_expr(r"get_bit('\x80'::bytea, 7)"), None, None).unwrap(),
            Value::Int32(0)
        );
    }

    #[test]
    fn test_uuidv7_expression_components() {
        let expr = r#"encode(
            set_bit(
                set_bit(
                    overlay(
                        uuid_send('550e8400-e29b-41d4-a716-446655440000'::uuid)
                        placing substring(int8send(1705312800000::bigint) from 3)
                        from 1 for 6
                    ),
                    52, 1
                ),
                53, 1
            ),
            'hex'
        )::uuid"#;

        let result = eval_expr(&parse_expr(expr), None, None).unwrap();
        let Value::Uuid(bytes) = result else {
            panic!("expected UUID result");
        };

        let base_uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let mut expected = base_uuid.as_bytes().to_vec();

        let ts: i64 = 1705312800000;
        let ts_bytes = ts.to_be_bytes();
        expected[..6].copy_from_slice(&ts_bytes[2..]);
        // uuidv7 sets version bits (52/53) to 1.
        expected[6] |= 0x0c;

        assert_eq!(bytes.as_slice(), expected.as_slice());
    }

    #[test]
    fn test_encode_decode_escape() {
        assert_eq!(
            eval_expr(
                &parse_expr(r"encode('\x48656c6c6f'::bytea, 'escape')"),
                None,
                None
            )
            .unwrap(),
            Value::Text("Hello".to_string())
        );

        assert_eq!(
            eval_expr(&parse_expr("decode('Hello', 'escape')"), None, None).unwrap(),
            Value::Bytes(b"Hello".to_vec())
        );

        assert_eq!(
            eval_expr(&parse_expr(r"decode('\000', 'escape')"), None, None).unwrap(),
            Value::Bytes(vec![0])
        );
    }

    #[test]
    fn test_decode_escape_invalid_sequence_errors() {
        assert!(eval_expr(&parse_expr(r"decode('\8', 'escape')"), None, None).is_err());
        assert!(eval_expr(&parse_expr(r"decode('\999', 'escape')"), None, None).is_err());
    }

    #[test]
    fn test_json_arrow_object_key() {
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"name": "Alice", "age": 30}' -> 'name'"#),
                None,
                None
            )
            .unwrap(),
            Value::Jsonb("\"Alice\"".to_string())
        );
    }

    #[test]
    fn test_json_long_arrow_object_key() {
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"name": "Alice", "age": 30}' ->> 'name'"#),
                None,
                None
            )
            .unwrap(),
            Value::Text("Alice".to_string())
        );
    }

    #[test]
    fn test_json_arrow_array_index() {
        assert_eq!(
            eval_expr(&parse_expr(r#"'[1, 2, 3]' -> 0"#), None, None).unwrap(),
            Value::Jsonb("1".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr(r#"'["a", "b", "c"]' -> 1"#), None, None).unwrap(),
            Value::Jsonb("\"b\"".to_string())
        );
    }

    #[test]
    fn test_json_long_arrow_array_index() {
        assert_eq!(
            eval_expr(&parse_expr(r#"'["a", "b", "c"]' ->> 1"#), None, None).unwrap(),
            Value::Text("b".to_string())
        );
    }

    #[test]
    fn test_json_nested_access() {
        let intermediate = eval_expr(
            &parse_expr(r#"'{"user": {"name": "Bob"}}' -> 'user'"#),
            None,
            None,
        )
        .unwrap();
        assert_eq!(intermediate, Value::Jsonb("{\"name\":\"Bob\"}".to_string()));

        assert_eq!(
            eval_expr(&parse_expr(r#"'{"name": "Bob"}' ->> 'name'"#), None, None).unwrap(),
            Value::Text("Bob".to_string())
        );

        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"user": {"name": "Bob"}}' -> 'user' ->> 'name'"#),
                None,
                None
            )
            .unwrap(),
            Value::Text("Bob".to_string())
        );
    }

    #[test]
    fn test_json_null_key() {
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"name": "Alice"}' -> 'missing'"#),
                None,
                None
            )
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_json_number_extraction() {
        assert_eq!(
            eval_expr(&parse_expr(r#"'{"count": 42}' ->> 'count'"#), None, None).unwrap(),
            Value::Text("42".to_string())
        );
    }

    #[test]
    fn test_array_literal() {
        assert_eq!(
            eval_expr(&parse_expr("ARRAY[1, 2, 3]"), None, None).unwrap(),
            Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
        );
        assert_eq!(
            eval_expr(&parse_expr("ARRAY['a', 'b', 'c']"), None, None).unwrap(),
            Value::Array(vec![
                Value::Text("a".to_string()),
                Value::Text("b".to_string()),
                Value::Text("c".to_string())
            ])
        );
    }

    #[test]
    fn test_array_indexing() {
        assert_eq!(
            eval_expr(&parse_expr("(ARRAY[10, 20, 30])[2]"), None, None).unwrap(),
            Value::Int32(20)
        );
        assert_eq!(
            eval_expr(&parse_expr("(ARRAY['a', 'b', 'c'])[1]"), None, None).unwrap(),
            Value::Text("a".to_string())
        );
        assert_eq!(
            eval_expr(&parse_expr("(ARRAY[1, 2, 3])[5]"), None, None).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_array_length() {
        assert_eq!(
            eval_expr(&parse_expr("array_length(ARRAY[1, 2, 3], 1)"), None, None).unwrap(),
            Value::Int32(3)
        );
    }

    #[test]
    fn test_array_position() {
        assert_eq!(
            eval_expr(
                &parse_expr("array_position(ARRAY['a', 'b', 'c'], 'b')"),
                None,
                None
            )
            .unwrap(),
            Value::Int32(2)
        );
        assert_eq!(
            eval_expr(&parse_expr("array_position(ARRAY[1, 2, 3], 5)"), None, None).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_array_cat() {
        assert_eq!(
            eval_expr(
                &parse_expr("array_cat(ARRAY[1, 2], ARRAY[3, 4])"),
                None,
                None
            )
            .unwrap(),
            Value::Array(vec![
                Value::Int32(1),
                Value::Int32(2),
                Value::Int32(3),
                Value::Int32(4)
            ])
        );
    }

    #[test]
    fn test_array_append_prepend() {
        assert_eq!(
            eval_expr(&parse_expr("array_append(ARRAY[1, 2], 3)"), None, None).unwrap(),
            Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
        );
        assert_eq!(
            eval_expr(&parse_expr("array_prepend(0, ARRAY[1, 2])"), None, None).unwrap(),
            Value::Array(vec![Value::Int32(0), Value::Int32(1), Value::Int32(2)])
        );
    }

    #[test]
    fn test_cardinality() {
        assert_eq!(
            eval_expr(&parse_expr("cardinality(ARRAY[1, 2, 3, 4])"), None, None).unwrap(),
            Value::Int32(4)
        );
    }

    #[test]
    fn test_json_cast() {
        assert_eq!(
            eval_expr(&parse_expr(r#"'{"a": 1}'::json ->> 'a'"#), None, None).unwrap(),
            Value::Text("1".to_string())
        );
    }

    #[test]
    fn test_jsonb_cast() {
        assert_eq!(
            eval_expr(&parse_expr(r#"'{"b": 2}'::jsonb ->> 'b'"#), None, None).unwrap(),
            Value::Text("2".to_string())
        );
    }

    #[test]
    fn test_json_comparison_blocked() {
        let json_val = Value::Json(r#"{"a":1}"#.to_string());
        let int_val = Value::Int32(1);
        assert!(compare_values(&json_val, &int_val).is_err());
    }

    #[test]
    fn test_jsonb_comparison_blocked() {
        let jsonb_val = Value::Jsonb(r#"{"a":1}"#.to_string());
        let int_val = Value::Int32(1);
        assert!(compare_values(&jsonb_val, &int_val).is_err());
    }

    #[test]
    fn test_json_contains_at_arrow() {
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"a":1,"b":2}'::jsonb @> '{"a":1}'::jsonb"#),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"a":1}'::jsonb @> '{"a":1,"b":2}'::jsonb"#),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"a":1}'::jsonb @> '{"a":1.0}'::jsonb"#),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'[{"a":1,"b":2}]'::jsonb @> '[{"a":1}]'::jsonb"#),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_json_contained_by_arrow_at() {
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"a":1}'::jsonb <@ '{"a":1,"b":2}'::jsonb"#),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_expr(
                &parse_expr(r#"'{"a":1,"b":2}'::jsonb <@ '{"a":1}'::jsonb"#),
                None,
                None
            )
            .unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_parse_vector_literal() {
        let vec = parse_vector_literal("[1.0, 2.0, 3.0]").unwrap();
        assert_eq!(vec, vec![1.0, 2.0, 3.0]);

        let vec2 = parse_vector_literal("[1,2,3]").unwrap();
        assert_eq!(vec2, vec![1.0, 2.0, 3.0]);

        assert!(parse_vector_literal("not a vector").is_err());
        assert!(parse_vector_literal("[1, 2, abc]").is_err());
    }

    #[test]
    fn test_l2_distance() {
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.0, 1.0, 0.0];
        let dist = l2_distance(&v1, &v2).unwrap();
        assert!((dist - 1.414213).abs() < 0.001);

        let v3 = vec![1.0, 2.0, 3.0];
        let v4 = vec![1.0, 2.0, 3.0];
        let dist2 = l2_distance(&v3, &v4).unwrap();
        assert!(dist2.abs() < 0.001); // Same vectors = 0 distance
    }

    #[test]
    fn test_cosine_distance() {
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![1.0, 0.0, 0.0];
        let dist = cosine_distance(&v1, &v2).unwrap();
        assert!(dist.abs() < 0.001); // Same vectors = 0 distance

        let v3 = vec![1.0, 0.0, 0.0];
        let v4 = vec![0.0, 1.0, 0.0];
        let dist2 = cosine_distance(&v3, &v4).unwrap();
        assert!((dist2 - 1.0).abs() < 0.001); // Orthogonal = max distance
    }

    #[test]
    fn test_inner_product() {
        let v1 = vec![1.0, 2.0, 3.0];
        let v2 = vec![4.0, 5.0, 6.0];
        let prod = inner_product(&v1, &v2).unwrap();
        assert_eq!(prod, -(4.0 + 10.0 + 18.0)); // negative for ORDER BY
    }

    #[test]
    fn test_vector_norm() {
        let v1 = vec![3.0, 4.0];
        let norm = vector_norm(&v1);
        assert_eq!(norm, 5.0); // 3-4-5 triangle

        let v2 = vec![1.0, 0.0, 0.0];
        let norm2 = vector_norm(&v2);
        assert_eq!(norm2, 1.0);
    }

    #[test]
    fn test_extract_vector() {
        // Test with Vector value
        let vec_val = Value::Vector(vec![1.0, 2.0, 3.0]);
        let extracted = extract_vector(&vec_val).unwrap();
        assert_eq!(extracted, vec![1.0, 2.0, 3.0]);

        // Test with Array value
        let arr_val = Value::Array(vec![
            Value::Float64(1.0),
            Value::Float64(2.0),
            Value::Float64(3.0),
        ]);
        let extracted2 = extract_vector(&arr_val).unwrap();
        assert_eq!(extracted2, vec![1.0, 2.0, 3.0]);

        // Test with Int32 array
        let arr_int = Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
        let extracted3 = extract_vector(&arr_int).unwrap();
        assert_eq!(extracted3, vec![1.0, 2.0, 3.0]);

        // Test with Text value (NEW - for ORM compatibility)
        let text_val = Value::Text("[1.5, 2.5, 3.5]".to_string());
        let extracted4 = extract_vector(&text_val).unwrap();
        assert_eq!(extracted4, vec![1.5, 2.5, 3.5]);

        // Test with Text value with spaces
        let text_val2 = Value::Text(" [ 1.0 , 2.0 , 3.0 ] ".to_string());
        let extracted5 = extract_vector(&text_val2).unwrap();
        assert_eq!(extracted5, vec![1.0, 2.0, 3.0]);

        // Test with empty text vector
        let text_empty = Value::Text("[]".to_string());
        let extracted6 = extract_vector(&text_empty).unwrap();
        assert_eq!(extracted6, Vec::<f64>::new());
    }

    #[test]
    fn test_format_width_and_identifier_quoting() {
        assert_eq!(
            eval_expr(&parse_expr("FORMAT('%10s', 'test')"), None, None).unwrap(),
            Value::Text("      test".to_string())
        );

        assert_eq!(
            eval_expr(&parse_expr("FORMAT('%I', 'column_name')"), None, None).unwrap(),
            Value::Text("column_name".to_string())
        );

        assert_eq!(
            eval_expr(&parse_expr("FORMAT('%I', 'column name')"), None, None).unwrap(),
            Value::Text("\"column name\"".to_string())
        );

        assert_eq!(
            eval_expr(&parse_expr("FORMAT('%L', 'value''s')"), None, None).unwrap(),
            Value::Text("'value''s'".to_string())
        );
    }

    #[test]
    fn test_format_rejects_precision_like_postgres() {
        let err = eval_expr(&parse_expr("FORMAT('%.3s', 'hello')"), None, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("unrecognized format() type specifier \".\""),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_quote_ident_and_pg_typeof_array() {
        assert_eq!(
            eval_expr(&parse_expr("QUOTE_IDENT('column')"), None, None).unwrap(),
            Value::Text("\"column\"".to_string())
        );

        assert_eq!(
            eval_expr(&parse_expr("PG_TYPEOF(ARRAY[1,2,3])"), None, None).unwrap(),
            Value::Text("integer[]".to_string())
        );
    }

    #[test]
    fn test_pg_encoding_to_char_reports_utf8() {
        let expr = parse_expr("pg_encoding_to_char(6)");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, Value::Text("UTF8".to_string()));
    }

    #[tokio::test]
    async fn test_current_database_reads_task_local_context() {
        let expr = parse_expr("current_database()");
        let val = with_query_context(123, Arc::from("mydb"), async {
            eval_expr(&expr, None, None).unwrap()
        })
        .await;
        assert_eq!(val, Value::Text("mydb".to_string()));
    }

    #[tokio::test]
    async fn test_current_timestamp_precision_truncates_to_second() {
        let fixed = 1_700_000_001_234_i64;
        let expr = parse_expr("CURRENT_TIMESTAMP(0)");
        let val = statement_time::with_statement_timestamp_millis(fixed, async {
            eval_expr(&expr, None, None).unwrap()
        })
        .await;
        assert_eq!(val, Value::Timestamp(1_700_000_001_000));
    }

    #[tokio::test]
    async fn test_now_precision_matches_current_timestamp_precision() {
        let fixed = 1_700_000_001_234_i64;
        let expr = parse_expr("NOW(0) = CURRENT_TIMESTAMP(0)");
        let val = statement_time::with_statement_timestamp_millis(fixed, async {
            eval_expr(&expr, None, None).unwrap()
        })
        .await;
        assert_eq!(val, Value::Boolean(true));
    }

    #[tokio::test]
    async fn test_current_timestamp_equals_date_trunc_second_within_statement() {
        let fixed = 1_700_000_001_234_i64;
        let expr = parse_expr("CURRENT_TIMESTAMP(0) = DATE_TRUNC('second', CURRENT_TIMESTAMP)");
        let val = statement_time::with_statement_timestamp_millis(fixed, async {
            eval_expr(&expr, None, None).unwrap()
        })
        .await;
        assert_eq!(val, Value::Boolean(true));
    }

    #[test]
    fn test_date_trunc_second_handles_negative_timestamps() {
        let expr = parse_expr("DATE_TRUNC('second', TIMESTAMP '1969-12-31 23:59:58.766')");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, parse_timestamp_string("1969-12-31 23:59:58").unwrap());
    }

    #[test]
    fn test_cast_timestamp_to_text_formats_timestamp() {
        let expr = parse_expr("TIMESTAMP '2024-01-15 10:30:00'::text");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, Value::Text("2024-01-15 10:30:00".to_string()));
    }

    #[test]
    fn test_cast_timestamptz_to_text_includes_offset() {
        let expr = parse_expr("TIMESTAMPTZ '2024-01-15T10:00:00Z'::text");
        let val = eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, Value::Text("2024-01-15 02:00:00-08".to_string()));
    }
}
