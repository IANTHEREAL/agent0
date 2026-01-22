//! Expression evaluation logic

use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Context, Result};
use rust_decimal::Decimal;
use sqlparser::ast::{BinaryOperator, Expr, JsonOperator, Value as SqlValue};
use std::cell::Cell;
use std::collections::HashMap;
use std::str::FromStr;

use super::timezone::parse_timezone_offset_seconds;

// Thread-local storage for connection_id used by pg_backend_pid()
thread_local! {
    static CONNECTION_ID: Cell<i32> = const { Cell::new(0) };
}

const VERSION_STRING: &str = concat!(
    "PostgreSQL 16.0 (pg-tikv ",
    env!("CARGO_PKG_VERSION"),
    " on TiKV)"
);

/// Set the connection_id for the current thread (call before query execution)
pub fn set_connection_id(id: i32) {
    CONNECTION_ID.with(|c| c.set(id));
}

fn get_connection_id() -> i32 {
    CONNECTION_ID.with(|c| c.get())
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

fn join_expr_column_type<'a>(expr: &Expr, ctx: &'a JoinContext<'a>) -> Option<&'a DataType> {
    match expr {
        Expr::Identifier(ident) => ctx
            .column_offsets
            .get(&ident.value)
            .and_then(|&offset| ctx.combined_schema.columns.get(offset))
            .map(|col| &col.data_type),
        Expr::CompoundIdentifier(parts) => {
            if parts.len() != 2 {
                return None;
            }
            let table_alias = &parts[0].value;
            let col_name = &parts[1].value;

            let mut key = String::with_capacity(table_alias.len() + 1 + col_name.len());
            key.push_str(table_alias);
            key.push('.');
            key.push_str(col_name);

            if let Some(&offset) = ctx.column_offsets.get(key.as_str()) {
                return ctx
                    .combined_schema
                    .columns
                    .get(offset)
                    .map(|col| &col.data_type);
            }

            for (k, &offset) in &ctx.column_offsets {
                if k.eq_ignore_ascii_case(&key) {
                    return ctx
                        .combined_schema
                        .columns
                        .get(offset)
                        .map(|col| &col.data_type);
                }
            }

            None
        }
        _ => None,
    }
}

fn expr_is_timestamptz_join(expr: &Expr, ctx: &JoinContext) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => join_expr_column_type(expr, ctx)
            .is_some_and(|dt| matches!(dt, DataType::TimestampTz)),
        Expr::Function(func) => func.name.0.last().is_some_and(|ident| {
            ident.value.eq_ignore_ascii_case("NOW")
                || ident.value.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        }),
        Expr::Cast { data_type, .. } | Expr::TypedString { data_type, .. } => {
            sql_datatype_is_timestamptz(data_type).unwrap_or(false)
        }
        Expr::AtTimeZone { timestamp, .. } => !expr_is_timestamptz_join(timestamp, ctx),
        Expr::Nested(inner) => expr_is_timestamptz_join(inner, ctx),
        _ => false,
    }
}

pub fn eval_expr_join(expr: &Expr, ctx: &JoinContext) -> Result<Value> {
    stacker::maybe_grow(32 * 1024, 1024 * 1024, || eval_expr_join_impl(expr, ctx))
}

fn eval_expr_join_impl(expr: &Expr, ctx: &JoinContext) -> Result<Value> {
    match expr {
        Expr::Value(v) => eval_value(v),
        Expr::Identifier(ident) => {
            if let Some(&offset) = ctx.column_offsets.get(&ident.value) {
                Ok(ctx.combined_row.values[offset].clone())
            } else {
                Err(anyhow!("Column '{}' not found or ambiguous", ident.value))
            }
        }
        Expr::CompoundIdentifier(parts) => {
            // Support both 2-part (table.column) and 3-part (schema.table.column) identifiers
            // For 3-part, we ignore the schema and use table.column for lookup
            let (table_alias, col_name) = if parts.len() == 2 {
                (&parts[0].value, &parts[1].value)
            } else if parts.len() == 3 {
                // schema.table.column -> use table.column
                (&parts[1].value, &parts[2].value)
            } else {
                return Err(anyhow!(
                    "Unsupported compound identifier with {} parts",
                    parts.len()
                ));
            };

            let key = format!("{}.{}", table_alias, col_name);
            if let Some(&offset) = ctx.column_offsets.get(&key) {
                return Ok(ctx.combined_row.values[offset].clone());
            }
            let key_lower =
                format!("{}.{}", table_alias.to_lowercase(), col_name.to_lowercase());
            for (k, &offset) in &ctx.column_offsets {
                if k.to_lowercase() == key_lower {
                    return Ok(ctx.combined_row.values[offset].clone());
                }
            }
            // For Sequelize-style aliases with '->' (e.g., "tags->sequelize_post_tags"),
            // try to find a column that ends with ".table_alias.col_name"
            if table_alias.contains("->") {
                let suffix = format!(".{}.{}", table_alias, col_name);
                let suffix_lower = suffix.to_lowercase();
                for (k, &offset) in &ctx.column_offsets {
                    if k.to_lowercase().ends_with(&suffix_lower) {
                        return Ok(ctx.combined_row.values[offset].clone());
                    }
                }
                // Also try matching just "table_alias.col_name" at the end
                let direct_suffix = format!("{}.{}", table_alias, col_name);
                let direct_suffix_lower = direct_suffix.to_lowercase();
                for (k, &offset) in &ctx.column_offsets {
                    if k.to_lowercase() == direct_suffix_lower
                        || k.to_lowercase()
                            .ends_with(&format!(".{}", direct_suffix_lower))
                    {
                        return Ok(ctx.combined_row.values[offset].clone());
                    }
                }
            }
            Err(anyhow!("Column '{}.{}' not found", table_alias, col_name))
        }
        Expr::BinaryOp { left, op, right } => {
            let left_val = eval_expr_join(left, ctx)?;
            let right_val = eval_expr_join(right, ctx)?;
            eval_binary_op(left_val, op, right_val)
        }
        Expr::UnaryOp { op, expr } => {
            let val = eval_expr_join(expr, ctx)?;
            match op {
                sqlparser::ast::UnaryOperator::Minus => match val {
                    Value::Int32(i) => Ok(Value::Int32(-i)),
                    Value::Int64(i) => Ok(Value::Int64(-i)),
                    Value::Float64(f) => Ok(Value::Float64(-f)),
                    Value::Numeric(d) => Ok(Value::Numeric(-d)),
                    _ => Err(anyhow!("Cannot negate {:?}", val)),
                },
                sqlparser::ast::UnaryOperator::Not => match val {
                    Value::Boolean(b) => Ok(Value::Boolean(!b)),
                    Value::Null => Ok(Value::Null),
                    _ => Err(anyhow!("NOT requires boolean")),
                },
                _ => Err(anyhow!("Unsupported unary operator")),
            }
        }
        Expr::Nested(expr) => eval_expr_join(expr, ctx),
        Expr::IsNull(expr) => {
            let val = eval_expr_join(expr, ctx)?;
            Ok(Value::Boolean(matches!(val, Value::Null)))
        }
        Expr::IsNotNull(expr) => {
            let val = eval_expr_join(expr, ctx)?;
            Ok(Value::Boolean(!matches!(val, Value::Null)))
        }
        Expr::IsTrue(expr) => match eval_expr_join(expr, ctx)? {
            Value::Boolean(b) => Ok(Value::Boolean(b)),
            Value::Null => Ok(Value::Boolean(false)),
            other => Err(anyhow!("IS TRUE requires boolean, got {:?}", other)),
        },
        Expr::IsNotTrue(expr) => match eval_expr_join(expr, ctx)? {
            Value::Boolean(true) => Ok(Value::Boolean(false)),
            Value::Boolean(false) | Value::Null => Ok(Value::Boolean(true)),
            other => Err(anyhow!("IS NOT TRUE requires boolean, got {:?}", other)),
        },
        Expr::IsFalse(expr) => match eval_expr_join(expr, ctx)? {
            Value::Boolean(b) => Ok(Value::Boolean(!b)),
            Value::Null => Ok(Value::Boolean(false)),
            other => Err(anyhow!("IS FALSE requires boolean, got {:?}", other)),
        },
        Expr::IsNotFalse(expr) => match eval_expr_join(expr, ctx)? {
            Value::Boolean(false) => Ok(Value::Boolean(false)),
            Value::Boolean(true) | Value::Null => Ok(Value::Boolean(true)),
            other => Err(anyhow!("IS NOT FALSE requires boolean, got {:?}", other)),
        },
        Expr::IsUnknown(expr) => {
            let val = eval_expr_join(expr, ctx)?;
            Ok(Value::Boolean(matches!(val, Value::Null)))
        }
        Expr::IsNotUnknown(expr) => {
            let val = eval_expr_join(expr, ctx)?;
            Ok(Value::Boolean(!matches!(val, Value::Null)))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let val = eval_expr_join(expr, ctx)?;
            if list.is_empty() {
                return Ok(Value::Boolean(*negated));
            }
            if matches!(val, Value::Null) {
                return Ok(Value::Null);
            }
            let mut found = false;
            let mut has_null = false;
            for item in list {
                let item_val = eval_expr_join(item, ctx)?;
                if matches!(item_val, Value::Null) {
                    has_null = true;
                    continue;
                }
                if compare_values(&val, &item_val).unwrap_or(1) == 0 {
                    found = true;
                    break;
                }
            }
            if found {
                Ok(Value::Boolean(!*negated))
            } else if has_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(*negated))
            }
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let val = eval_expr_join(expr, ctx)?;
            let low_val = eval_expr_join(low, ctx)?;
            let high_val = eval_expr_join(high, ctx)?;
            if matches!(val, Value::Null)
                || matches!(low_val, Value::Null)
                || matches!(high_val, Value::Null)
            {
                return Ok(Value::Null);
            }
            let ge_low = compare_values(&val, &low_val).unwrap_or(-1) >= 0;
            let le_high = compare_values(&val, &high_val).unwrap_or(1) <= 0;
            let in_range = ge_low && le_high;
            Ok(Value::Boolean(if *negated { !in_range } else { in_range }))
        }
        Expr::Function(func) => eval_function_join(func, ctx),
        Expr::Interval(interval) => {
            let val = eval_expr_join(&interval.value, ctx)?;
            match val {
                Value::Text(s) => parse_interval_from_expr(&s, interval),
                Value::Int32(n) => interval_from_number(n as i64, interval),
                Value::Int64(n) => interval_from_number(n, interval),
                _ => Err(anyhow!("Invalid interval value")),
            }
        }
        Expr::TypedString { data_type, value } => match data_type {
            sqlparser::ast::DataType::Interval => parse_interval_string(value),
            sqlparser::ast::DataType::Timestamp(_, _) => parse_timestamp_string(value),
            sqlparser::ast::DataType::Date => {
                crate::types::date::parse_date_days(value).map(Value::Date)
            }
            _ => Ok(Value::Text(value.clone())),
        },
        Expr::AtTimeZone { timestamp, time_zone } => {
            let ts = eval_expr_join(timestamp, ctx)?;
            if matches!(ts, Value::Null) {
                return Ok(Value::Null);
            }
            let offset_secs = parse_timezone_offset_seconds(time_zone)?;

            let Value::Timestamp(ts_millis) = ts else {
                return Err(anyhow!("AT TIME ZONE requires timestamp"));
            };

            let offset_ms = i64::from(offset_secs) * 1000;
            if expr_is_timestamptz_join(timestamp, ctx) {
                Ok(Value::Timestamp(ts_millis + offset_ms))
            } else {
                Ok(Value::Timestamp(ts_millis - offset_ms))
            }
        }
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr_join(expr, ctx)?;
            let pat = eval_expr_join(pattern, ctx)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                return Ok(Value::Boolean(false));
            };
            let matched = like_match(s, p, *escape_char, false);
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr_join(expr, ctx)?;
            let pat = eval_expr_join(pattern, ctx)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                return Ok(Value::Boolean(false));
            };
            let matched = like_match(s, p, *escape_char, true);
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr_join(expr, ctx)?;
            let pat = eval_expr_join(pattern, ctx)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                if matches!(val, Value::Null) || matches!(pat, Value::Null) {
                    return Ok(Value::Null);
                }
                return Ok(Value::Boolean(false));
            };
            let matched = similar_to_match(s, p, *escape_char)?;
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                let op_val = eval_expr_join(op, ctx)?;
                for (i, cond) in conditions.iter().enumerate() {
                    let cond_val = eval_expr_join(cond, ctx)?;
                    if compare_values(&op_val, &cond_val).unwrap_or(1) == 0 {
                        return eval_expr_join(&results[i], ctx);
                    }
                }
            } else {
                for (i, cond) in conditions.iter().enumerate() {
                    if matches!(eval_expr_join(cond, ctx)?, Value::Boolean(true)) {
                        return eval_expr_join(&results[i], ctx);
                    }
                }
            }
            if let Some(else_expr) = else_result {
                eval_expr_join(else_expr, ctx)
            } else {
                Ok(Value::Null)
            }
        }
        Expr::Cast {
            expr, data_type, ..
        } => {
            let val = eval_expr_join(expr, ctx)?;
            use sqlparser::ast::DataType as SqlType;
            if matches!(data_type, SqlType::Text | SqlType::Varchar(_) | SqlType::String(_)) {
                if let Value::Timestamp(ts) = val {
                    let is_timestamptz = expr_is_timestamptz_join(expr, ctx);
                    let mut s = crate::types::timestamp::format_timestamp_millis(ts, is_timestamptz)?;
                    match data_type {
                        SqlType::Varchar(Some(sqlparser::ast::CharacterLength::IntegerLength {
                            length,
                            ..
                        })) => {
                            let max_len = *length as usize;
                            if s.chars().count() > max_len {
                                s = s.chars().take(max_len).collect();
                            }
                        }
                        SqlType::String(Some(n)) => {
                            let max_len = *n as usize;
                            if s.chars().count() > max_len {
                                s = s.chars().take(max_len).collect();
                            }
                        }
                        _ => {}
                    }
                    return Ok(Value::Text(s));
                }
            }
            cast_value(val, data_type)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => eval_substring_join(expr, substring_from, substring_for, ctx),
        Expr::Trim {
            expr,
            trim_what,
            trim_where,
            ..
        } => {
            let val = eval_expr_join(expr, ctx)?;
            let Value::Text(s) = val else {
                return Ok(Value::Null);
            };
            let trim_chars: Vec<char> = if let Some(what) = trim_what {
                match eval_expr_join(what, ctx)? {
                    Value::Text(t) => t.chars().collect(),
                    _ => vec![' '],
                }
            } else {
                vec![' ']
            };
            let result = match trim_where {
                Some(sqlparser::ast::TrimWhereField::Leading) => s
                    .trim_start_matches(|c| trim_chars.contains(&c))
                    .to_string(),
                Some(sqlparser::ast::TrimWhereField::Trailing) => {
                    s.trim_end_matches(|c| trim_chars.contains(&c)).to_string()
                }
                Some(sqlparser::ast::TrimWhereField::Both) | None => {
                    s.trim_matches(|c| trim_chars.contains(&c)).to_string()
                }
            };
            Ok(Value::Text(result))
        }
        Expr::Position { expr, r#in } => {
            let substr = eval_expr_join(expr, ctx)?;
            let string = eval_expr_join(r#in, ctx)?;
            let (Value::Text(sub), Value::Text(s)) = (substr, string) else {
                return Ok(Value::Int32(0));
            };
            let pos = s.find(&sub).map(|i| i as i32 + 1).unwrap_or(0);
            Ok(Value::Int32(pos))
        }
        Expr::Extract { field, expr } => eval_extract_join(field, expr, ctx),
        Expr::JsonAccess {
            left,
            operator,
            right,
        } => eval_json_access_expr_join(left, operator, right, ctx),
        Expr::Array(array) => {
            let mut values = Vec::new();
            for elem in &array.elem {
                values.push(eval_expr_join(elem, ctx)?);
            }
            Ok(Value::Array(values))
        }
        Expr::Tuple(exprs) => {
            let mut values = Vec::with_capacity(exprs.len());
            for e in exprs {
                values.push(eval_expr_join(e, ctx)?);
            }
            Ok(Value::Array(values))
        }
        Expr::ArrayIndex { obj, indexes } => {
            let arr_val = eval_expr_join(obj, ctx)?;
            eval_array_index_join(arr_val, indexes, ctx)
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => eval_overlay_join(expr, overlay_what, overlay_from, overlay_for, ctx),
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => {
            let left_val = eval_expr_join(left, ctx)?;
            let right_val = eval_expr_join(right, ctx)?;
            let arr = match right_val {
                Value::Array(arr) => arr,
                _ => return Err(anyhow!("ANY requires an array operand")),
            };
            for elem in arr {
                let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
                if matches!(cmp, Value::Boolean(true)) {
                    return Ok(Value::Boolean(true));
                }
            }
            Ok(Value::Boolean(false))
        }
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => {
            let left_val = eval_expr_join(left, ctx)?;
            let right_val = eval_expr_join(right, ctx)?;
            let arr = match right_val {
                Value::Array(arr) => arr,
                _ => return Err(anyhow!("ALL requires an array operand")),
            };
            if arr.is_empty() {
                return Ok(Value::Boolean(true));
            }
            for elem in arr {
                let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
                if !matches!(cmp, Value::Boolean(true)) {
                    return Ok(Value::Boolean(false));
                }
            }
            Ok(Value::Boolean(true))
        }
        _ => Err(anyhow!(
            "Unsupported expression in JOIN context: {:?}",
            expr
        )),
    }
}

fn eval_function_join(func: &sqlparser::ast::Function, ctx: &JoinContext) -> Result<Value> {
    let func_name = func.name.0.last().map(|i| i.value.as_str()).unwrap_or("");
    let args: Vec<Value> = func
        .args
        .iter()
        .filter_map(|arg| {
            if let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) =
                arg
            {
                eval_expr_join(e, ctx).ok()
            } else {
                None
            }
        })
        .collect();

    match func_name.to_uppercase().as_str() {
        "COALESCE" => {
            for val in args {
                if !matches!(val, Value::Null) {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }
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
        "UPPER" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.to_uppercase())),
            Some(v) => Ok(v),
            None => Ok(Value::Null),
        },
        "LOWER" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.to_lowercase())),
            Some(v) => Ok(v),
            None => Ok(Value::Null),
        },
        "LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Int32(s.chars().count() as i32)),
            Some(Value::Bytes(b)) => Ok(Value::Int32(b.len() as i32)),
            _ => Ok(Value::Null),
        },
        "GET_BIT" => eval_get_bit_from_args(args),
        "SET_BIT" => eval_set_bit_from_args(args),
        "INT8SEND" => eval_int8send_from_args(args),
        "INT4SEND" => eval_int4send_from_args(args),
        "UUID_SEND" => eval_uuid_send_from_args(args),
        "ENCODE" => eval_encode_from_args(args),
        "DECODE" => eval_decode_from_args(args),
        "CONCAT" => {
            let mut result = String::new();
            for val in args {
                match val {
                    Value::Null => {}
                    Value::Text(s) => result.push_str(&s),
                    v => result.push_str(&v.to_string()),
                }
            }
            Ok(Value::Text(result))
        }
        "LEFT" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let n = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Text(s.chars().take(n).collect()))
        }
        "RIGHT" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let n = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            let chars: Vec<char> = s.chars().collect();
            let start = chars.len().saturating_sub(n);
            Ok(Value::Text(chars[start..].iter().collect()))
        }
        "REPLACE" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let from = match iter.next() {
                Some(Value::Text(f)) => f,
                _ => return Ok(Value::Text(s)),
            };
            let to = match iter.next() {
                Some(Value::Text(t)) => t,
                _ => String::new(),
            };
            Ok(Value::Text(s.replace(&from, &to)))
        }
        "ABS" => match args.into_iter().next() {
            Some(Value::Int32(n)) => Ok(Value::Int32(n.abs())),
            Some(Value::Int64(n)) => Ok(Value::Int64(n.abs())),
            Some(Value::Float64(n)) => Ok(Value::Float64(n.abs())),
            Some(Value::Numeric(d)) => Ok(Value::Numeric(d.abs())),
            _ => Ok(Value::Null),
        },
        "CEIL" | "CEILING" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.ceil())),
            Some(Value::Int32(n)) => Ok(Value::Int32(n)),
            Some(Value::Int64(n)) => Ok(Value::Int64(n)),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let f = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(f.ceil()))
            }
            _ => Ok(Value::Null),
        },
        "FLOOR" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.floor())),
            Some(Value::Int32(n)) => Ok(Value::Int32(n)),
            Some(Value::Int64(n)) => Ok(Value::Int64(n)),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let f = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(f.floor()))
            }
            _ => Ok(Value::Null),
        },
        "ROUND" => {
            let mut iter = args.into_iter();
            let val = iter.next();
            let precision = match iter.next() {
                Some(Value::Int32(n)) => n,
                Some(Value::Int64(n)) => n as i32,
                _ => 0,
            };
            match val {
                Some(Value::Float64(n)) => {
                    let factor = 10_f64.powi(precision);
                    Ok(Value::Float64((n * factor).round() / factor))
                }
                Some(Value::Numeric(d)) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let n = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    let factor = 10_f64.powi(precision);
                    Ok(Value::Float64((n * factor).round() / factor))
                }
                Some(Value::Int32(n)) => Ok(Value::Int32(n)),
                Some(Value::Int64(n)) => Ok(Value::Int64(n)),
                _ => Ok(Value::Null),
            }
        }
        "SQRT" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.sqrt())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).sqrt())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).sqrt())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.sqrt()))
            }
            _ => Ok(Value::Null),
        },
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
                    .unwrap()
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
                days += days_in_month(prev_year, prev_month) as i32;
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
        "GEN_RANDOM_UUID" | "UUID_GENERATE_V4" => {
            let uuid = uuid::Uuid::new_v4();
            Ok(Value::Uuid(*uuid.as_bytes()))
        }
        "UUIDV7" => {
            // UUIDv7 is a time-based UUID with millisecond precision timestamp
            // Format: 48-bit timestamp | 4-bit version (7) | 12-bit rand_a | 2-bit variant | 62-bit rand_b
            use std::time::{SystemTime, UNIX_EPOCH};
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            // Start with random bytes
            let mut bytes = [0u8; 16];
            let random_uuid = uuid::Uuid::new_v4();
            bytes.copy_from_slice(random_uuid.as_bytes());

            // Set the 48-bit timestamp in the first 6 bytes (big-endian)
            let ts_bytes = timestamp_ms.to_be_bytes();
            bytes[0..6].copy_from_slice(&ts_bytes[2..8]);

            // Set version to 7 (bits 48-51)
            bytes[6] = (bytes[6] & 0x0F) | 0x70;

            // Set variant to RFC 4122 (bits 64-65)
            bytes[8] = (bytes[8] & 0x3F) | 0x80;

            Ok(Value::Uuid(bytes))
        }
        "JSONB_EXISTS" => {
            if args.len() != 2 {
                return Err(anyhow!("jsonb_exists requires exactly 2 arguments"));
            }
            let mut iter = args.into_iter();
            let json_str = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let key = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            Ok(Value::Boolean(super::jsonb::exists(&json_val, &key)))
        }
        "JSONB_EXISTS_ANY" => {
            if args.len() != 2 {
                return Err(anyhow!("jsonb_exists_any requires exactly 2 arguments"));
            }
            let mut iter = args.into_iter();
            let json_str = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let keys_val = iter.next().unwrap_or(Value::Null);

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

            let exists_any = match keys_val {
                Value::Array(keys) => {
                    if keys.iter().all(|k| matches!(k, Value::Null | Value::Text(_))) {
                        super::jsonb::exists_any(
                            &json_val,
                            keys.iter().filter_map(|k| match k {
                                Value::Text(s) => Some(s.as_str()),
                                _ => None,
                            }),
                        )
                    } else {
                        keys.iter().any(|k| {
                            if let Value::Null = k {
                                return false;
                            }
                            match k {
                                Value::Text(s) => super::jsonb::exists(&json_val, s),
                                other => super::jsonb::exists(&json_val, &other.to_string()),
                            }
                        })
                    }
                }
                Value::Null => return Ok(Value::Null),
                other => super::jsonb::exists(&json_val, &other.to_string()),
            };

            Ok(Value::Boolean(exists_any))
        }
        "JSONB_EXISTS_ALL" => {
            if args.len() != 2 {
                return Err(anyhow!("jsonb_exists_all requires exactly 2 arguments"));
            }
            let mut iter = args.into_iter();
            let json_str = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let keys_val = iter.next().unwrap_or(Value::Null);

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

            let exists_all = match keys_val {
                Value::Array(keys) => {
                    if keys.iter().all(|k| matches!(k, Value::Null | Value::Text(_))) {
                        super::jsonb::exists_all(
                            &json_val,
                            keys.iter().filter_map(|k| match k {
                                Value::Text(s) => Some(s.as_str()),
                                _ => None,
                            }),
                        )
                    } else {
                        keys.iter().all(|k| {
                            if let Value::Null = k {
                                return true;
                            }
                            match k {
                                Value::Text(s) => super::jsonb::exists(&json_val, s),
                                other => super::jsonb::exists(&json_val, &other.to_string()),
                            }
                        })
                    }
                }
                Value::Null => return Ok(Value::Null),
                other => super::jsonb::exists(&json_val, &other.to_string()),
            };

            Ok(Value::Boolean(exists_all))
        }
        "JSONB_ARRAY_LENGTH"
        | "JSON_ARRAY_LENGTH"
        | "JSONB_TYPEOF"
        | "JSON_TYPEOF"
        | "JSONB_BUILD_OBJECT"
        | "JSON_BUILD_OBJECT"
        | "JSONB_BUILD_ARRAY"
        | "JSON_BUILD_ARRAY"
        | "JSONB_OBJECT_KEYS"
        | "JSON_OBJECT_KEYS"
        | "JSONB_EXTRACT_PATH"
        | "JSON_EXTRACT_PATH"
        | "JSONB_EXTRACT_PATH_TEXT"
        | "JSON_EXTRACT_PATH_TEXT"
        | "JSONB_PRETTY"
        | "TO_JSON"
        | "TO_JSONB" => eval_function(func, None, None),
        "COL_DESCRIPTION" => Ok(Value::Null),
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
                _ => {
                    for (col_key, &offset) in &ctx.column_offsets {
                        if col_key.ends_with(".typname") || col_key == "typname" {
                            if let Some(Value::Text(s)) = ctx.combined_row.values.get(offset) {
                                return Ok(Value::Text(s.clone()));
                            }
                        }
                    }
                    "text"
                }
            };
            Ok(Value::Text(type_name.to_string()))
        }
        "PG_GET_CONSTRAINTDEF" => {
            for (col_key, &offset) in &ctx.column_offsets {
                if col_key.ends_with(".constraintdef") || col_key == "constraintdef" {
                    if let Some(val) = ctx.combined_row.values.get(offset) {
                        if !matches!(val, Value::Null) {
                            return Ok(val.clone());
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
        "UNNEST" => match args.into_iter().next() {
            Some(Value::Array(arr)) => Ok(Value::Array(arr)),
            Some(Value::Null) | None => Ok(Value::Null),
            Some(v) => Ok(v),
        },
        "PG_GET_INDEXDEF" => {
            for (col_key, &offset) in &ctx.column_offsets {
                if col_key.ends_with(".indexdef") || col_key == "indexdef" {
                    if let Some(val) = ctx.combined_row.values.get(offset) {
                        if !matches!(val, Value::Null) {
                            return Ok(val.clone());
                        }
                    }
                }
            }
            Ok(Value::Text("CREATE INDEX".to_string()))
        }
        "PG_TABLE_IS_VISIBLE" | "PG_TYPE_IS_VISIBLE" | "PG_FUNCTION_IS_VISIBLE" => {
            Ok(Value::Boolean(true))
        }
        "PG_GET_SERIAL_SEQUENCE" => Ok(Value::Null),
        _ => Err(anyhow!("Unsupported function in JOIN: {}", func_name)),
    }
}

pub fn eval_expr(expr: &Expr, row: Option<&Row>, schema: Option<&TableSchema>) -> Result<Value> {
    stacker::maybe_grow(32 * 1024, 1024 * 1024, || eval_expr_impl(expr, row, schema))
}

fn eval_expr_impl(expr: &Expr, row: Option<&Row>, schema: Option<&TableSchema>) -> Result<Value> {
    match expr {
        Expr::Value(v) => eval_value(v),
        Expr::Identifier(ident) => {
            // Handle DEFAULT keyword specially - return Null which signals to use default value
            if ident.value.to_uppercase() == "DEFAULT" {
                return Ok(Value::Null);
            }

            if let (Some(row), Some(schema)) = (row, schema) {
                let idx = schema
                    .column_index(&ident.value)
                    .ok_or_else(|| anyhow!("Column '{}' not found", ident.value))?;
                Ok(row.values[idx].clone())
            } else {
                Err(anyhow!(
                    "Cannot evaluate identifier '{}' without row context",
                    ident.value
                ))
            }
        }
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                let col_name = &parts[parts.len() - 1].value;
                if let (Some(row), Some(schema)) = (row, schema) {
                    let idx = schema
                        .column_index(col_name)
                        .ok_or_else(|| anyhow!("Column '{}' not found", col_name))?;
                    Ok(row.values[idx].clone())
                } else {
                    Err(anyhow!("Cannot evaluate column without row context"))
                }
            } else {
                Err(anyhow!("Invalid compound identifier"))
            }
        }
        Expr::BinaryOp { left, op, right } => {
            let left_val = eval_expr(left, row, schema)?;
            let right_val = eval_expr(right, row, schema)?;
            eval_binary_op(left_val, op, right_val)
        }
        Expr::UnaryOp { op, expr } => {
            let val = eval_expr(expr, row, schema)?;
            match op {
                sqlparser::ast::UnaryOperator::Minus => match val {
                    Value::Int32(i) => Ok(Value::Int32(-i)),
                    Value::Int64(i) => Ok(Value::Int64(-i)),
                    Value::Float64(f) => Ok(Value::Float64(-f)),
                    Value::Numeric(d) => Ok(Value::Numeric(-d)),
                    _ => Err(anyhow!("Cannot negate {:?}", val)),
                },
                sqlparser::ast::UnaryOperator::Not => match val {
                    Value::Boolean(b) => Ok(Value::Boolean(!b)),
                    Value::Null => Ok(Value::Null),
                    _ => Err(anyhow!("NOT requires boolean, got {:?}", val)),
                },
                _ => Err(anyhow!("Unsupported unary operator: {:?}", op)),
            }
        }
        Expr::Nested(expr) => eval_expr(expr, row, schema),
        Expr::IsNull(expr) => {
            let val = eval_expr(expr, row, schema)?;
            Ok(Value::Boolean(matches!(val, Value::Null)))
        }
        Expr::IsNotNull(expr) => {
            let val = eval_expr(expr, row, schema)?;
            Ok(Value::Boolean(!matches!(val, Value::Null)))
        }
        Expr::IsTrue(expr) => match eval_expr(expr, row, schema)? {
            Value::Boolean(b) => Ok(Value::Boolean(b)),
            Value::Null => Ok(Value::Boolean(false)),
            other => Err(anyhow!("IS TRUE requires boolean, got {:?}", other)),
        },
        Expr::IsNotTrue(expr) => match eval_expr(expr, row, schema)? {
            Value::Boolean(true) => Ok(Value::Boolean(false)),
            Value::Boolean(false) | Value::Null => Ok(Value::Boolean(true)),
            other => Err(anyhow!("IS NOT TRUE requires boolean, got {:?}", other)),
        },
        Expr::IsFalse(expr) => match eval_expr(expr, row, schema)? {
            Value::Boolean(b) => Ok(Value::Boolean(!b)),
            Value::Null => Ok(Value::Boolean(false)),
            other => Err(anyhow!("IS FALSE requires boolean, got {:?}", other)),
        },
        Expr::IsNotFalse(expr) => match eval_expr(expr, row, schema)? {
            Value::Boolean(false) => Ok(Value::Boolean(false)),
            Value::Boolean(true) | Value::Null => Ok(Value::Boolean(true)),
            other => Err(anyhow!("IS NOT FALSE requires boolean, got {:?}", other)),
        },
        Expr::IsUnknown(expr) => {
            let val = eval_expr(expr, row, schema)?;
            Ok(Value::Boolean(matches!(val, Value::Null)))
        }
        Expr::IsNotUnknown(expr) => {
            let val = eval_expr(expr, row, schema)?;
            Ok(Value::Boolean(!matches!(val, Value::Null)))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let val = eval_expr(expr, row, schema)?;
            if matches!(val, Value::Null) {
                return Ok(Value::Null);
            }
            let mut found = false;
            let mut has_null = false;
            for item in list {
                let item_val = eval_expr(item, row, schema)?;
                if matches!(item_val, Value::Null) {
                    has_null = true;
                    continue;
                }
                if compare_values(&val, &item_val).unwrap_or(1) == 0 {
                    found = true;
                    break;
                }
            }
            if found {
                Ok(Value::Boolean(!*negated))
            } else if has_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(*negated))
            }
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let val = eval_expr(expr, row, schema)?;
            let low_val = eval_expr(low, row, schema)?;
            let high_val = eval_expr(high, row, schema)?;
            if matches!(val, Value::Null)
                || matches!(low_val, Value::Null)
                || matches!(high_val, Value::Null)
            {
                return Ok(Value::Null);
            }
            let ge_low = compare_values(&val, &low_val).unwrap_or(-1) >= 0;
            let le_high = compare_values(&val, &high_val).unwrap_or(1) <= 0;
            let in_range = ge_low && le_high;
            Ok(Value::Boolean(if *negated { !in_range } else { in_range }))
        }
        Expr::Function(func) => eval_function(func, row, schema),
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr(expr, row, schema)?;
            let pat = eval_expr(pattern, row, schema)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                return Ok(Value::Boolean(false));
            };
            let matched = like_match(s, p, *escape_char, false);
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr(expr, row, schema)?;
            let pat = eval_expr(pattern, row, schema)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                return Ok(Value::Boolean(false));
            };
            let matched = like_match(s, p, *escape_char, true);
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr(expr, row, schema)?;
            let pat = eval_expr(pattern, row, schema)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                if matches!(val, Value::Null) || matches!(pat, Value::Null) {
                    return Ok(Value::Null);
                }
                return Ok(Value::Boolean(false));
            };
            let matched = similar_to_match(s, p, *escape_char)?;
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                let op_val = eval_expr(op, row, schema)?;
                for (i, cond) in conditions.iter().enumerate() {
                    let cond_val = eval_expr(cond, row, schema)?;
                    if compare_values(&op_val, &cond_val).unwrap_or(1) == 0 {
                        return eval_expr(&results[i], row, schema);
                    }
                }
            } else {
                for (i, cond) in conditions.iter().enumerate() {
                    if matches!(eval_expr(cond, row, schema)?, Value::Boolean(true)) {
                        return eval_expr(&results[i], row, schema);
                    }
                }
            }
            if let Some(else_expr) = else_result {
                eval_expr(else_expr, row, schema)
            } else {
                Ok(Value::Null)
            }
        }
        Expr::Cast {
            expr, data_type, ..
        } => {
            let val = eval_expr(expr, row, schema)?;
            use sqlparser::ast::DataType as SqlType;
            if matches!(data_type, SqlType::Text | SqlType::Varchar(_) | SqlType::String(_)) {
                if let Value::Timestamp(ts) = val {
                    let is_timestamptz = expr_is_timestamptz(expr, schema);
                    let mut s = crate::types::timestamp::format_timestamp_millis(ts, is_timestamptz)?;
                    match data_type {
                        SqlType::Varchar(Some(sqlparser::ast::CharacterLength::IntegerLength {
                            length,
                            ..
                        })) => {
                            let max_len = *length as usize;
                            if s.chars().count() > max_len {
                                s = s.chars().take(max_len).collect();
                            }
                        }
                        SqlType::String(Some(n)) => {
                            let max_len = *n as usize;
                            if s.chars().count() > max_len {
                                s = s.chars().take(max_len).collect();
                            }
                        }
                        _ => {}
                    }
                    return Ok(Value::Text(s));
                }
            }
            cast_value(val, data_type)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => eval_substring(expr, substring_from, substring_for, row, schema),
        Expr::Trim {
            expr,
            trim_what,
            trim_where,
            ..
        } => {
            let val = eval_expr(expr, row, schema)?;
            let Value::Text(s) = val else {
                return Ok(Value::Null);
            };
            let trim_chars: Vec<char> = if let Some(what) = trim_what {
                match eval_expr(what, row, schema)? {
                    Value::Text(t) => t.chars().collect(),
                    _ => vec![' '],
                }
            } else {
                vec![' ']
            };
            let result = match trim_where {
                Some(sqlparser::ast::TrimWhereField::Leading) => s
                    .trim_start_matches(|c| trim_chars.contains(&c))
                    .to_string(),
                Some(sqlparser::ast::TrimWhereField::Trailing) => {
                    s.trim_end_matches(|c| trim_chars.contains(&c)).to_string()
                }
                Some(sqlparser::ast::TrimWhereField::Both) | None => {
                    s.trim_matches(|c| trim_chars.contains(&c)).to_string()
                }
            };
            Ok(Value::Text(result))
        }
        Expr::Position { expr, r#in } => {
            let substr = eval_expr(expr, row, schema)?;
            let string = eval_expr(r#in, row, schema)?;
            let (Value::Text(sub), Value::Text(s)) = (substr, string) else {
                return Ok(Value::Int32(0));
            };
            let pos = s.find(&sub).map(|i| i as i32 + 1).unwrap_or(0);
            Ok(Value::Int32(pos))
        }
        Expr::Extract { field, expr } => eval_extract(field, expr, row, schema),
        Expr::Ceil { expr, .. } => {
            let val = eval_expr(expr, row, schema)?;
            match val {
                Value::Float64(n) => Ok(Value::Float64(n.ceil())),
                Value::Int32(n) => Ok(Value::Int32(n)),
                Value::Int64(n) => Ok(Value::Int64(n)),
                Value::Numeric(d) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let f = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    Ok(Value::Float64(f.ceil()))
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::Floor { expr, .. } => {
            let val = eval_expr(expr, row, schema)?;
            match val {
                Value::Float64(n) => Ok(Value::Float64(n.floor())),
                Value::Int32(n) => Ok(Value::Int32(n)),
                Value::Int64(n) => Ok(Value::Int64(n)),
                Value::Numeric(d) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let f = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    Ok(Value::Float64(f.floor()))
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::Interval(interval) => {
            let val = eval_expr(&interval.value, row, schema)?;
            match val {
                Value::Text(s) => parse_interval_from_expr(&s, interval),
                Value::Int32(n) => interval_from_number(n as i64, interval),
                Value::Int64(n) => interval_from_number(n, interval),
                _ => Err(anyhow!("Invalid interval value")),
            }
        }
        Expr::TypedString { data_type, value } => match data_type {
            sqlparser::ast::DataType::Interval => parse_interval_string(value),
            sqlparser::ast::DataType::Timestamp(_, _) => parse_timestamp_string(value),
            sqlparser::ast::DataType::Date => {
                crate::types::date::parse_date_days(value).map(Value::Date)
            }
            _ => Ok(Value::Text(value.clone())),
        },
        Expr::AtTimeZone { timestamp, time_zone } => {
            let ts = eval_expr(timestamp, row, schema)?;
            if matches!(ts, Value::Null) {
                return Ok(Value::Null);
            }
            let offset_secs = parse_timezone_offset_seconds(time_zone)?;

            let Value::Timestamp(ts_millis) = ts else {
                return Err(anyhow!("AT TIME ZONE requires timestamp"));
            };

            let offset_ms = i64::from(offset_secs) * 1000;
            if expr_is_timestamptz(timestamp, schema) {
                Ok(Value::Timestamp(ts_millis + offset_ms))
            } else {
                Ok(Value::Timestamp(ts_millis - offset_ms))
            }
        }
        Expr::JsonAccess {
            left,
            operator,
            right,
        } => eval_json_access_expr(left, operator, right, row, schema),
        Expr::Array(array) => {
            let mut values = Vec::new();
            for elem in &array.elem {
                values.push(eval_expr(elem, row, schema)?);
            }
            Ok(Value::Array(values))
        }
        Expr::Tuple(exprs) => {
            let mut values = Vec::with_capacity(exprs.len());
            for e in exprs {
                values.push(eval_expr(e, row, schema)?);
            }
            Ok(Value::Array(values))
        }
        Expr::ArrayIndex { obj, indexes } => {
            let arr_val = eval_expr(obj, row, schema)?;
            eval_array_index(arr_val, indexes, row, schema)
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => eval_overlay(expr, overlay_what, overlay_from, overlay_for, row, schema),
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => {
            let left_val = eval_expr(left, row, schema)?;
            let right_val = eval_expr(right, row, schema)?;
            let arr = match right_val {
                Value::Array(arr) => arr,
                _ => return Err(anyhow!("ANY requires an array operand")),
            };
            for elem in arr {
                let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
                if matches!(cmp, Value::Boolean(true)) {
                    return Ok(Value::Boolean(true));
                }
            }
            Ok(Value::Boolean(false))
        }
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => {
            let left_val = eval_expr(left, row, schema)?;
            let right_val = eval_expr(right, row, schema)?;
            let arr = match right_val {
                Value::Array(arr) => arr,
                _ => return Err(anyhow!("ALL requires an array operand")),
            };
            if arr.is_empty() {
                return Ok(Value::Boolean(true));
            }
            for elem in arr {
                let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
                if !matches!(cmp, Value::Boolean(true)) {
                    return Ok(Value::Boolean(false));
                }
            }
            Ok(Value::Boolean(true))
        }
        _ => Err(anyhow!("Unsupported expression: {:?}", expr)),
    }
}

#[inline(never)]
fn eval_substring(
    expr: &Expr,
    substring_from: &Option<Box<Expr>>,
    substring_for: &Option<Box<Expr>>,
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<Value> {
    let val = eval_expr(expr, row, schema)?;
    let from_val = if let Some(from_expr) = substring_from {
        Some(eval_expr(from_expr, row, schema)?)
    } else {
        None
    };

    match val {
        Value::Text(s) => {
            if let (Some(Value::Text(pattern)), None) = (&from_val, substring_for) {
                let re = regex::Regex::new(pattern)
                    .map_err(|e| anyhow!("Invalid regex pattern in SUBSTRING: {}", e))?;
                if let Some(caps) = re.captures(&s) {
                    if caps.len() > 1 {
                        return Ok(caps
                            .get(1)
                            .map(|m| Value::Text(m.as_str().to_string()))
                            .unwrap_or(Value::Null));
                    }
                    return Ok(caps
                        .get(0)
                        .map(|m| Value::Text(m.as_str().to_string()))
                        .unwrap_or(Value::Null));
                }
                return Ok(Value::Null);
            }

            let start = match &from_val {
                Some(Value::Int32(n)) => (n - 1).max(0) as usize,
                Some(Value::Int64(n)) => (n - 1).max(0) as usize,
                Some(Value::Null) => return Ok(Value::Null),
                Some(_) => 0,
                None => 0,
            };
            let len = if let Some(for_expr) = substring_for {
                match eval_expr(for_expr, row, schema)? {
                    Value::Int32(n) => Some(n.max(0) as usize),
                    Value::Int64(n) => Some(n.max(0) as usize),
                    Value::Null => return Ok(Value::Null),
                    _ => None,
                }
            } else {
                None
            };
            let chars: Vec<char> = s.chars().collect();
            let result: String = if let Some(l) = len {
                chars.iter().skip(start).take(l).collect()
            } else {
                chars.iter().skip(start).collect()
            };
            Ok(Value::Text(result))
        }
        Value::Bytes(bytes) => {
            let start = match &from_val {
                Some(Value::Int32(n)) => i64::from(*n),
                Some(Value::Int64(n)) => *n,
                Some(Value::Null) => return Ok(Value::Null),
                Some(_) => return Ok(Value::Null),
                None => 0,
            };
            let count = if let Some(for_expr) = substring_for {
                match eval_expr(for_expr, row, schema)? {
                    Value::Int32(n) => Some(i64::from(n.max(0))),
                    Value::Int64(n) => Some(n.max(0)),
                    Value::Null => return Ok(Value::Null),
                    _ => return Ok(Value::Null),
                }
            } else {
                None
            };
            Ok(Value::Bytes(super::bytea::substring(bytes, start, count)))
        }
        _ => Ok(Value::Null),
    }
}

#[inline(never)]
fn eval_extract(
    field: &sqlparser::ast::DateTimeField,
    expr: &Expr,
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<Value> {
    let val = eval_expr(expr, row, schema)?;
    let ts = match val {
        Value::Timestamp(t) => t,
        Value::Date(days) => {
            use chrono::NaiveDate;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let date = epoch + chrono::Duration::days(days as i64);
            date.and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis()
        }
        Value::Text(s) => {
            use chrono::NaiveDateTime;
            let dt = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S"))
                .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f"))
                .or_else(|_| {
                    chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
                })
                .map_err(|e| anyhow!("Invalid timestamp format: {}", e))?;
            dt.and_utc().timestamp_millis()
        }
        _ => return Ok(Value::Null),
    };
    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let dt = Utc
        .timestamp_millis_opt(ts)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    let result = match field {
        sqlparser::ast::DateTimeField::Year => dt.year() as f64,
        sqlparser::ast::DateTimeField::Month => dt.month() as f64,
        sqlparser::ast::DateTimeField::Day => dt.day() as f64,
        sqlparser::ast::DateTimeField::Hour => dt.hour() as f64,
        sqlparser::ast::DateTimeField::Minute => dt.minute() as f64,
        sqlparser::ast::DateTimeField::Second => dt.second() as f64,
        sqlparser::ast::DateTimeField::Dow => dt.weekday().num_days_from_sunday() as f64,
        sqlparser::ast::DateTimeField::Doy => dt.ordinal() as f64,
        sqlparser::ast::DateTimeField::Week => dt.iso_week().week() as f64,
        sqlparser::ast::DateTimeField::Quarter => ((dt.month() - 1) / 3 + 1) as f64,
        sqlparser::ast::DateTimeField::Epoch => ts as f64 / 1000.0,
        _ => return Err(anyhow!("Unsupported EXTRACT field")),
    };
    Ok(Value::Float64(result))
}

#[inline(never)]
fn eval_overlay(
    expr: &Expr,
    overlay_what: &Expr,
    overlay_from: &Expr,
    overlay_for: &Option<Box<Expr>>,
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<Value> {
    let base = eval_expr(expr, row, schema)?;
    let what = eval_expr(overlay_what, row, schema)?;
    let from = eval_expr(overlay_from, row, schema)?;
    let for_len = match overlay_for {
        Some(e) => Some(eval_expr(e, row, schema)?),
        None => None,
    };
    let start = match from {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let replace_len = match for_len {
        Some(Value::Int32(n)) => Some(i64::from(n.max(0))),
        Some(Value::Int64(n)) => Some(n.max(0)),
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return Ok(Value::Null),
        None => None,
    };

    match (base, what) {
        (Value::Text(base), Value::Text(what)) => {
            if start <= 0 {
                return Ok(Value::Text(base));
            }
            let replace_len = match replace_len {
                Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
                None => what.chars().count(),
            };

            let base_chars: Vec<char> = base.chars().collect();
            let start_idx = usize::try_from(start - 1).unwrap_or(usize::MAX);
            let prefix: String = base_chars.iter().take(start_idx).collect();
            let suffix_start = start_idx.saturating_add(replace_len);
            let suffix: String = base_chars.iter().skip(suffix_start).collect();
            Ok(Value::Text(format!("{prefix}{what}{suffix}")))
        }
        (Value::Bytes(base), Value::Bytes(what)) => Ok(Value::Bytes(super::bytea::overlay(
            base,
            &what,
            start,
            replace_len,
        ))),
        _ => Ok(Value::Null),
    }
}

#[inline(never)]
fn eval_substring_join(
    expr: &Expr,
    substring_from: &Option<Box<Expr>>,
    substring_for: &Option<Box<Expr>>,
    ctx: &JoinContext,
) -> Result<Value> {
    let val = eval_expr_join(expr, ctx)?;
    let from_val = if let Some(from_expr) = substring_from {
        Some(eval_expr_join(from_expr, ctx)?)
    } else {
        None
    };

    match val {
        Value::Text(s) => {
            if let (Some(Value::Text(pattern)), None) = (&from_val, substring_for) {
                let re = regex::Regex::new(pattern)
                    .map_err(|e| anyhow!("Invalid regex pattern in SUBSTRING: {}", e))?;
                if let Some(caps) = re.captures(&s) {
                    if caps.len() > 1 {
                        return Ok(caps
                            .get(1)
                            .map(|m| Value::Text(m.as_str().to_string()))
                            .unwrap_or(Value::Null));
                    }
                    return Ok(caps
                        .get(0)
                        .map(|m| Value::Text(m.as_str().to_string()))
                        .unwrap_or(Value::Null));
                }
                return Ok(Value::Null);
            }

            let start = match &from_val {
                Some(Value::Int32(n)) => (n - 1).max(0) as usize,
                Some(Value::Int64(n)) => (n - 1).max(0) as usize,
                Some(Value::Null) => return Ok(Value::Null),
                Some(_) => 0,
                None => 0,
            };
            let len = if let Some(for_expr) = substring_for {
                match eval_expr_join(for_expr, ctx)? {
                    Value::Int32(n) => Some(n.max(0) as usize),
                    Value::Int64(n) => Some(n.max(0) as usize),
                    Value::Null => return Ok(Value::Null),
                    _ => None,
                }
            } else {
                None
            };
            let chars: Vec<char> = s.chars().collect();
            let result: String = if let Some(l) = len {
                chars.iter().skip(start).take(l).collect()
            } else {
                chars.iter().skip(start).collect()
            };
            Ok(Value::Text(result))
        }
        Value::Bytes(bytes) => {
            let start = match &from_val {
                Some(Value::Int32(n)) => i64::from(*n),
                Some(Value::Int64(n)) => *n,
                Some(Value::Null) => return Ok(Value::Null),
                Some(_) => return Ok(Value::Null),
                None => 0,
            };
            let count = if let Some(for_expr) = substring_for {
                match eval_expr_join(for_expr, ctx)? {
                    Value::Int32(n) => Some(i64::from(n.max(0))),
                    Value::Int64(n) => Some(n.max(0)),
                    Value::Null => return Ok(Value::Null),
                    _ => return Ok(Value::Null),
                }
            } else {
                None
            };
            Ok(Value::Bytes(super::bytea::substring(bytes, start, count)))
        }
        _ => Ok(Value::Null),
    }
}

#[inline(never)]
fn eval_extract_join(field: &sqlparser::ast::DateTimeField, expr: &Expr, ctx: &JoinContext) -> Result<Value> {
    let val = eval_expr_join(expr, ctx)?;
    let ts = match val {
        Value::Timestamp(t) => t,
        Value::Date(days) => {
            use chrono::NaiveDate;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let date = epoch + chrono::Duration::days(days as i64);
            date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
        }
        Value::Text(s) => {
            use chrono::NaiveDateTime;
            let dt = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S"))
                .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f"))
                .or_else(|_| {
                    chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
                })
                .map_err(|e| anyhow!("Invalid timestamp format: {}", e))?;
            dt.and_utc().timestamp_millis()
        }
        _ => return Ok(Value::Null),
    };
    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let dt = Utc.timestamp_millis_opt(ts).single().ok_or_else(|| anyhow!("Invalid timestamp"))?;
    let result = match field {
        sqlparser::ast::DateTimeField::Year => dt.year() as f64,
        sqlparser::ast::DateTimeField::Month => dt.month() as f64,
        sqlparser::ast::DateTimeField::Day => dt.day() as f64,
        sqlparser::ast::DateTimeField::Hour => dt.hour() as f64,
        sqlparser::ast::DateTimeField::Minute => dt.minute() as f64,
        sqlparser::ast::DateTimeField::Second => dt.second() as f64,
        sqlparser::ast::DateTimeField::Dow => dt.weekday().num_days_from_sunday() as f64,
        sqlparser::ast::DateTimeField::Doy => dt.ordinal() as f64,
        sqlparser::ast::DateTimeField::Week => dt.iso_week().week() as f64,
        sqlparser::ast::DateTimeField::Quarter => ((dt.month() - 1) / 3 + 1) as f64,
        sqlparser::ast::DateTimeField::Epoch => ts as f64 / 1000.0,
        _ => return Err(anyhow!("Unsupported EXTRACT field")),
    };
    Ok(Value::Float64(result))
}

#[inline(never)]
fn eval_overlay_join(
    expr: &Expr,
    overlay_what: &Expr,
    overlay_from: &Expr,
    overlay_for: &Option<Box<Expr>>,
    ctx: &JoinContext,
) -> Result<Value> {
    let base = eval_expr_join(expr, ctx)?;
    let what = eval_expr_join(overlay_what, ctx)?;
    let from = eval_expr_join(overlay_from, ctx)?;
    let for_len = match overlay_for {
        Some(e) => Some(eval_expr_join(e, ctx)?),
        None => None,
    };
    let start = match from {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let replace_len = match for_len {
        Some(Value::Int32(n)) => Some(i64::from(n.max(0))),
        Some(Value::Int64(n)) => Some(n.max(0)),
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return Ok(Value::Null),
        None => None,
    };

    match (base, what) {
        (Value::Text(base), Value::Text(what)) => {
            if start <= 0 {
                return Ok(Value::Text(base));
            }
            let replace_len = match replace_len {
                Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
                None => what.chars().count(),
            };
            let base_chars: Vec<char> = base.chars().collect();
            let start_idx = usize::try_from(start - 1).unwrap_or(usize::MAX);
            let prefix: String = base_chars.iter().take(start_idx).collect();
            let suffix_start = start_idx.saturating_add(replace_len);
            let suffix: String = base_chars.iter().skip(suffix_start).collect();
            Ok(Value::Text(format!("{prefix}{what}{suffix}")))
        }
        (Value::Bytes(base), Value::Bytes(what)) => Ok(Value::Bytes(super::bytea::overlay(
            base, &what, start, replace_len,
        ))),
        _ => Ok(Value::Null),
    }
}

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

fn eval_function(
    func: &sqlparser::ast::Function,
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<Value> {
    let func_name = func.name.0.last().map(|i| i.value.as_str()).unwrap_or("");
    let args: Vec<Value> = func
        .args
        .iter()
        .filter_map(|arg| {
            if let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) =
                arg
            {
                eval_expr(e, row, schema).ok()
            } else {
                None
            }
        })
        .collect();

    match func_name.to_uppercase().as_str() {
        "COALESCE" => {
            for val in args {
                if !matches!(val, Value::Null) {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }
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
        "UPPER" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.to_uppercase())),
            Some(v) => Ok(v),
            None => Ok(Value::Null),
        },
        "LOWER" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.to_lowercase())),
            Some(v) => Ok(v),
            None => Ok(Value::Null),
        },
        "LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Int32(s.chars().count() as i32)),
            Some(Value::Bytes(b)) => Ok(Value::Int32(b.len() as i32)),
            _ => Ok(Value::Null),
        },
        "OCTET_LENGTH" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Int32(s.len() as i32)),
            Some(Value::Bytes(b)) => Ok(Value::Int32(b.len() as i32)),
            _ => Ok(Value::Null),
        },
        "BIT_LENGTH" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Int32((s.as_bytes().len() * 8) as i32)),
            Some(Value::Bytes(b)) => Ok(Value::Int32((b.len() * 8) as i32)),
            _ => Ok(Value::Null),
        },
        "GET_BIT" => eval_get_bit_from_args(args),
        "SET_BIT" => eval_set_bit_from_args(args),
        "INT8SEND" => eval_int8send_from_args(args),
        "INT4SEND" => eval_int4send_from_args(args),
        "UUID_SEND" => eval_uuid_send_from_args(args),
        "CONCAT" => {
            let mut result = String::new();
            for val in args {
                match val {
                    Value::Null => {}
                    Value::Text(s) => result.push_str(&s),
                    v => result.push_str(&v.to_string()),
                }
            }
            Ok(Value::Text(result))
        }
        "CONCAT_WS" => {
            let mut iter = args.into_iter();
            let sep = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => String::new(),
            };
            let parts: Vec<String> = iter
                .filter_map(|v| match v {
                    Value::Null => None,
                    Value::Text(s) => Some(s),
                    v => Some(v.to_string()),
                })
                .collect();
            Ok(Value::Text(parts.join(&sep)))
        }
        "LEFT" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let n = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Text(s.chars().take(n).collect()))
        }
        "RIGHT" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let n = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            let chars: Vec<char> = s.chars().collect();
            let start = chars.len().saturating_sub(n);
            Ok(Value::Text(chars[start..].iter().collect()))
        }
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
        "LPAD" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let len = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            let fill = match iter.next() {
                Some(Value::Text(f)) => f,
                _ => " ".to_string(),
            };
            let char_count = s.chars().count();
            if char_count >= len {
                return Ok(Value::Text(s.chars().take(len).collect()));
            }
            let pad_len = len - char_count;
            let mut result = String::new();
            let fill_chars: Vec<char> = fill.chars().collect();
            if !fill_chars.is_empty() {
                for i in 0..pad_len {
                    result.push(fill_chars[i % fill_chars.len()]);
                }
            }
            result.push_str(&s);
            Ok(Value::Text(result))
        }
        "RPAD" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let len = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            let fill = match iter.next() {
                Some(Value::Text(f)) => f,
                _ => " ".to_string(),
            };
            let char_count = s.chars().count();
            if char_count >= len {
                return Ok(Value::Text(s.chars().take(len).collect()));
            }
            let mut result = s.clone();
            let fill_chars: Vec<char> = fill.chars().collect();
            if !fill_chars.is_empty() {
                for i in 0..(len - char_count) {
                    result.push(fill_chars[i % fill_chars.len()]);
                }
            }
            Ok(Value::Text(result))
        }
        "REPLACE" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let from = match iter.next() {
                Some(Value::Text(f)) => f,
                _ => return Ok(Value::Text(s)),
            };
            let to = match iter.next() {
                Some(Value::Text(t)) => t,
                _ => String::new(),
            };
            Ok(Value::Text(s.replace(&from, &to)))
        }
        "REVERSE" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.chars().rev().collect())),
            _ => Ok(Value::Null),
        },
        "TRIM" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.trim().to_string())),
            Some(Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("trim requires text argument")),
        },
        "BTRIM" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("btrim requires text argument")),
            };
            let trim_chars = match iter.next() {
                None => None,
                Some(Value::Text(chars)) => Some(chars),
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => Some(v.to_string()),
            };
            let result = match trim_chars {
                None => s.trim().to_string(),
                Some(chars) => {
                    let trim_set: Vec<char> = chars.chars().collect();
                    if trim_set.is_empty() {
                        s
                    } else {
                        s.trim_matches(|c| trim_set.contains(&c)).to_string()
                    }
                }
            };
            Ok(Value::Text(result))
        }
        "LTRIM" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.trim_start().to_string())),
            Some(Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("ltrim requires text argument")),
        },
        "RTRIM" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(s.trim_end().to_string())),
            Some(Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("rtrim requires text argument")),
        },
        "REPEAT" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let n = match iter.next() {
                Some(Value::Int32(n)) => n.max(0) as usize,
                Some(Value::Int64(n)) => n.max(0) as usize,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Text(s.repeat(n)))
        }
        "SPLIT_PART" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => return Ok(Value::Null),
            };
            let delim = match iter.next() {
                Some(Value::Text(d)) => d,
                _ => return Ok(Value::Null),
            };
            let n = match iter.next() {
                Some(Value::Int32(n)) => n,
                Some(Value::Int64(n)) => n as i32,
                _ => return Ok(Value::Null),
            };
            if n <= 0 {
                return Err(anyhow!("field position must be > 0"));
            }
            let parts: Vec<&str> = s.split(&delim).collect();
            Ok(Value::Text(
                parts
                    .get((n - 1) as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            ))
        }
        "STRPOS" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let substr = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            if substr.is_empty() {
                return Ok(Value::Int32(1));
            }
            match s.find(&substr) {
                Some(pos) => Ok(Value::Int32((s[..pos].chars().count() + 1) as i32)),
                None => Ok(Value::Int32(0)),
            }
        }
        "ASCII" => match args.into_iter().next() {
            Some(Value::Text(s)) => {
                if s.is_empty() {
                    Ok(Value::Int32(0))
                } else {
                    Ok(Value::Int32(s.chars().next().unwrap() as i32))
                }
            }
            Some(Value::Null) => Ok(Value::Null),
            _ => Ok(Value::Null),
        },
        "CHR" => match args.into_iter().next() {
            Some(Value::Int32(n)) => {
                if n < 0 {
                    Err(anyhow!("chr() argument must be >= 0"))
                } else {
                    match char::from_u32(n as u32) {
                        Some(c) => Ok(Value::Text(c.to_string())),
                        None => Err(anyhow!("invalid character code: {}", n)),
                    }
                }
            }
            Some(Value::Int64(n)) => {
                if n < 0 {
                    Err(anyhow!("chr() argument must be >= 0"))
                } else {
                    match char::from_u32(n as u32) {
                        Some(c) => Ok(Value::Text(c.to_string())),
                        None => Err(anyhow!("invalid character code: {}", n)),
                    }
                }
            }
            Some(Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("chr() requires integer argument")),
        },
        "MD5" => match args.into_iter().next() {
            Some(Value::Text(s)) => Ok(Value::Text(format!("{:x}", md5::compute(s.as_bytes())))),
            Some(Value::Null) => Ok(Value::Null),
            Some(v) => Ok(Value::Text(format!(
                "{:x}",
                md5::compute(v.to_string().as_bytes())
            ))),
            None => Ok(Value::Null),
        },
        "ENCODE" => eval_encode_from_args(args),
        "DECODE" => eval_decode_from_args(args),
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
        "TRANSLATE" => {
            let mut iter = args.into_iter();
            let s = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let from = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Text(s)),
            };
            let to = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => String::new(),
            };
            let from_chars: Vec<char> = from.chars().collect();
            let to_chars: Vec<char> = to.chars().collect();
            let result: String = s
                .chars()
                .filter_map(|c| {
                    if let Some(pos) = from_chars.iter().position(|&fc| fc == c) {
                        to_chars.get(pos).copied()
                    } else {
                        Some(c)
                    }
                })
                .collect();
            Ok(Value::Text(result))
        }
        "INITCAP" => match args.into_iter().next() {
            Some(Value::Text(s)) => {
                let mut result = String::new();
                let mut cap_next = true;
                for c in s.chars() {
                    if c.is_alphabetic() {
                        if cap_next {
                            result.push(c.to_uppercase().next().unwrap_or(c));
                        } else {
                            result.push(c.to_lowercase().next().unwrap_or(c));
                        }
                        cap_next = false;
                    } else {
                        result.push(c);
                        cap_next = true;
                    }
                }
                Ok(Value::Text(result))
            }
            _ => Ok(Value::Null),
        },
        "ABS" => match args.into_iter().next() {
            Some(Value::Int32(n)) => Ok(Value::Int32(n.abs())),
            Some(Value::Int64(n)) => Ok(Value::Int64(n.abs())),
            Some(Value::Float64(n)) => Ok(Value::Float64(n.abs())),
            Some(Value::Numeric(d)) => Ok(Value::Numeric(d.abs())),
            _ => Ok(Value::Null),
        },
        "CEIL" | "CEILING" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.ceil())),
            Some(Value::Int32(n)) => Ok(Value::Int32(n)),
            Some(Value::Int64(n)) => Ok(Value::Int64(n)),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let f = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(f.ceil()))
            }
            _ => Ok(Value::Null),
        },
        "FLOOR" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.floor())),
            Some(Value::Int32(n)) => Ok(Value::Int32(n)),
            Some(Value::Int64(n)) => Ok(Value::Int64(n)),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let f = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(f.floor()))
            }
            _ => Ok(Value::Null),
        },
        "ROUND" => {
            let mut iter = args.into_iter();
            let val = iter.next();
            let precision = match iter.next() {
                Some(Value::Int32(n)) => n,
                Some(Value::Int64(n)) => n as i32,
                _ => 0,
            };
            match val {
                Some(Value::Float64(n)) => {
                    let factor = 10_f64.powi(precision);
                    Ok(Value::Float64((n * factor).round() / factor))
                }
                Some(Value::Numeric(d)) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let n = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    let factor = 10_f64.powi(precision);
                    Ok(Value::Float64((n * factor).round() / factor))
                }
                Some(Value::Int32(n)) => Ok(Value::Int32(n)),
                Some(Value::Int64(n)) => Ok(Value::Int64(n)),
                _ => Ok(Value::Null),
            }
        }
        "TRUNC" | "TRUNCATE" => {
            let mut iter = args.into_iter();
            let val = iter.next();
            let precision = match iter.next() {
                Some(Value::Int32(n)) => n,
                Some(Value::Int64(n)) => n as i32,
                _ => 0,
            };
            match val {
                Some(Value::Float64(n)) => {
                    let factor = 10_f64.powi(precision);
                    Ok(Value::Float64((n * factor).trunc() / factor))
                }
                Some(Value::Numeric(d)) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let n = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    let factor = 10_f64.powi(precision);
                    Ok(Value::Float64((n * factor).trunc() / factor))
                }
                Some(Value::Int32(n)) => Ok(Value::Int32(n)),
                Some(Value::Int64(n)) => Ok(Value::Int64(n)),
                _ => Ok(Value::Null),
            }
        }
        "SQRT" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.sqrt())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).sqrt())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).sqrt())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.sqrt()))
            }
            _ => Ok(Value::Null),
        },
        "CBRT" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.cbrt())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).cbrt())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).cbrt())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.cbrt()))
            }
            _ => Ok(Value::Null),
        },
        "POWER" | "POW" => {
            let mut iter = args.into_iter();
            let base = match iter.next() {
                Some(Value::Float64(n)) => n,
                Some(Value::Int32(n)) => n as f64,
                Some(Value::Int64(n)) => n as f64,
                Some(Value::Numeric(d)) => {
                    use rust_decimal::prelude::ToPrimitive;
                    d.to_f64()
                        .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?
                }
                _ => return Ok(Value::Null),
            };
            let exp = match iter.next() {
                Some(Value::Float64(n)) => n,
                Some(Value::Int32(n)) => n as f64,
                Some(Value::Int64(n)) => n as f64,
                Some(Value::Numeric(d)) => {
                    use rust_decimal::prelude::ToPrimitive;
                    d.to_f64()
                        .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?
                }
                _ => return Ok(Value::Null),
            };
            Ok(Value::Float64(base.powf(exp)))
        }
        "EXP" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.exp())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).exp())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).exp())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.exp()))
            }
            _ => Ok(Value::Null),
        },
        "LN" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.ln())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).ln())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).ln())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.ln()))
            }
            _ => Ok(Value::Null),
        },
        "LOG" | "LOG10" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.log10())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).log10())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).log10())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.log10()))
            }
            _ => Ok(Value::Null),
        },
        "SIGN" => match args.into_iter().next() {
            Some(Value::Int32(n)) => Ok(Value::Int32(if n > 0 {
                1
            } else if n < 0 {
                -1
            } else {
                0
            })),
            Some(Value::Int64(n)) => Ok(Value::Int64(if n > 0 {
                1
            } else if n < 0 {
                -1
            } else {
                0
            })),
            Some(Value::Float64(n)) => Ok(Value::Float64(if n > 0.0 {
                1.0
            } else if n < 0.0 {
                -1.0
            } else {
                0.0
            })),
            Some(Value::Numeric(d)) => Ok(Value::Int32(d.cmp(&Decimal::ZERO) as i32)),
            _ => Ok(Value::Null),
        },
        "MOD" => {
            let mut iter = args.into_iter();
            let a = iter.next();
            let b = iter.next();
            match (a, b) {
                (Some(Value::Int32(a)), Some(Value::Int32(b))) if b != 0 => Ok(Value::Int32(a % b)),
                (Some(Value::Int64(a)), Some(Value::Int64(b))) if b != 0 => Ok(Value::Int64(a % b)),
                (Some(Value::Float64(a)), Some(Value::Float64(b))) => Ok(Value::Float64(a % b)),
                _ => Ok(Value::Null),
            }
        }
        "DEGREES" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.to_degrees())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).to_degrees())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).to_degrees())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.to_degrees()))
            }
            _ => Ok(Value::Null),
        },
        "RADIANS" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.to_radians())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).to_radians())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).to_radians())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.to_radians()))
            }
            _ => Ok(Value::Null),
        },
        "SIN" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.sin())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).sin())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).sin())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.sin()))
            }
            _ => Ok(Value::Null),
        },
        "COS" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.cos())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).cos())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).cos())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.cos()))
            }
            _ => Ok(Value::Null),
        },
        "TAN" => match args.into_iter().next() {
            Some(Value::Float64(n)) => Ok(Value::Float64(n.tan())),
            Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).tan())),
            Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).tan())),
            Some(Value::Numeric(d)) => {
                use rust_decimal::prelude::ToPrimitive;
                let n = d
                    .to_f64()
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
                Ok(Value::Float64(n.tan()))
            }
            _ => Ok(Value::Null),
        },
        "PI" => Ok(Value::Float64(std::f64::consts::PI)),
        "RANDOM" => Ok(Value::Float64(rand::random::<f64>())),
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
                    .unwrap()
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
                days += days_in_month(prev_year, prev_month) as i32;
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
        "GEN_RANDOM_UUID" | "UUID_GENERATE_V4" => {
            let uuid = uuid::Uuid::new_v4();
            Ok(Value::Uuid(*uuid.as_bytes()))
        }
        "UUIDV7" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let mut bytes = [0u8; 16];
            let random_uuid = uuid::Uuid::new_v4();
            bytes.copy_from_slice(random_uuid.as_bytes());
            let ts_bytes = timestamp_ms.to_be_bytes();
            bytes[0..6].copy_from_slice(&ts_bytes[2..8]);
            bytes[6] = (bytes[6] & 0x0F) | 0x70;
            bytes[8] = (bytes[8] & 0x3F) | 0x80;
            Ok(Value::Uuid(bytes))
        }
        "NEXTVAL" | "CURRVAL" | "SETVAL" => Err(anyhow!(
            "{} is a sequence function and must be evaluated during execution",
            func_name
        )),
        "SET_CONFIG" => Ok(Value::Text(String::new())),
        "PG_IS_IN_RECOVERY" => Ok(Value::Boolean(false)),
        "PG_BACKEND_PID" => Ok(Value::Int32(get_connection_id())),
        "VERSION" => Ok(Value::Text(VERSION_STRING.to_string())),
        "CURRENT_DATABASE" => Ok(Value::Text("postgres".to_string())),
        "CURRENT_SCHEMA" => Ok(Value::Text("public".to_string())),
        "CURRENT_USER" | "SESSION_USER" | "USER" => Ok(Value::Text("postgres".to_string())),
        "PG_GET_USERBYID" => Ok(Value::Text("postgres".to_string())),
        "HAS_SCHEMA_PRIVILEGE" | "HAS_TABLE_PRIVILEGE" | "HAS_DATABASE_PRIVILEGE" => {
            Ok(Value::Boolean(true))
        }
        "PG_GET_INDEXDEF" => {
            if let (Some(row), Some(schema)) = (row, schema) {
                if let Some(idx) = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case("indexdef"))
                {
                    if let Some(val) = row.values.get(idx) {
                        if !matches!(val, Value::Null) {
                            return Ok(val.clone());
                        }
                    }
                }
            }
            Ok(Value::Text("CREATE INDEX".to_string()))
        }
        "PG_GET_CONSTRAINTDEF" => {
            if let (Some(row), Some(schema)) = (row, schema) {
                if let Some(idx) = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case("constraintdef"))
                {
                    if let Some(val) = row.values.get(idx) {
                        if !matches!(val, Value::Null) {
                            return Ok(val.clone());
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
        "OBJ_DESCRIPTION" | "COL_DESCRIPTION" | "SHOBJ_DESCRIPTION" => Ok(Value::Null),
        "PG_GET_SERIAL_SEQUENCE" => Ok(Value::Null),
        "PG_CATALOG.SET_CONFIG" => Ok(Value::Text(String::new())),
        "UNNEST" => match args.into_iter().next() {
            Some(Value::Array(arr)) => Ok(Value::Array(arr)),
            Some(Value::Null) | None => Ok(Value::Null),
            Some(v) => Ok(v),
        },
        "ARRAY_LENGTH" => {
            let mut iter = args.into_iter();
            let arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let dim = match iter.next() {
                Some(Value::Int32(d)) => d,
                Some(Value::Int64(d)) => d as i32,
                _ => 1,
            };
            if dim == 1 {
                Ok(Value::Int32(arr.len() as i32))
            } else {
                Ok(Value::Null)
            }
        }
        "ARRAY_UPPER" => {
            let mut iter = args.into_iter();
            let arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let dim = match iter.next() {
                Some(Value::Int32(d)) => d,
                Some(Value::Int64(d)) => d as i32,
                _ => 1,
            };
            if dim == 1 && !arr.is_empty() {
                Ok(Value::Int32(arr.len() as i32))
            } else {
                Ok(Value::Null)
            }
        }
        "ARRAY_LOWER" => {
            let mut iter = args.into_iter();
            let arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let dim = match iter.next() {
                Some(Value::Int32(d)) => d,
                Some(Value::Int64(d)) => d as i32,
                _ => 1,
            };
            if dim == 1 && !arr.is_empty() {
                Ok(Value::Int32(1))
            } else {
                Ok(Value::Null)
            }
        }
        "CARDINALITY" => match args.into_iter().next() {
            Some(Value::Array(a)) => Ok(Value::Int32(a.len() as i32)),
            Some(Value::Null) => Ok(Value::Null),
            _ => Ok(Value::Null),
        },
        "ARRAY_POSITION" => {
            let mut iter = args.into_iter();
            let arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let elem = match iter.next() {
                Some(v) => v,
                None => return Ok(Value::Null),
            };
            for (i, v) in arr.iter().enumerate() {
                if compare_values(v, &elem).unwrap_or(1) == 0 {
                    return Ok(Value::Int32((i + 1) as i32));
                }
            }
            Ok(Value::Null)
        }
        "ARRAY_CAT" => {
            let mut result = Vec::new();
            for arg in args {
                match arg {
                    Value::Array(a) => result.extend(a),
                    Value::Null => {}
                    v => result.push(v),
                }
            }
            Ok(Value::Array(result))
        }
        "ARRAY_APPEND" => {
            let mut iter = args.into_iter();
            let mut arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => Vec::new(),
                _ => return Err(anyhow!("ARRAY_APPEND requires array as first argument")),
            };
            if let Some(elem) = iter.next() {
                arr.push(elem);
            }
            Ok(Value::Array(arr))
        }
        "ARRAY_PREPEND" => {
            let mut iter = args.into_iter();
            let elem = iter.next().unwrap_or(Value::Null);
            let mut arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => Vec::new(),
                _ => return Err(anyhow!("ARRAY_PREPEND requires array as second argument")),
            };
            arr.insert(0, elem);
            Ok(Value::Array(arr))
        }
        "ARRAY_REMOVE" => {
            let mut iter = args.into_iter();
            let arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let elem = match iter.next() {
                Some(v) => v,
                None => return Ok(Value::Array(arr)),
            };
            let result: Vec<Value> = arr
                .into_iter()
                .filter(|v| compare_values(v, &elem).unwrap_or(1) != 0)
                .collect();
            Ok(Value::Array(result))
        }
        "ARRAY_TO_STRING" => {
            let mut iter = args.into_iter();
            let arr = match iter.next() {
                Some(Value::Array(a)) => a,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let delimiter = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(v) => v.to_string(),
                None => ",".to_string(),
            };
            let null_str = iter.next().and_then(|v| match v {
                Value::Text(s) => Some(s),
                Value::Null => None,
                v => Some(v.to_string()),
            });
            let parts: Vec<String> = arr
                .into_iter()
                .filter_map(|v| match v {
                    Value::Null => null_str.clone(),
                    v => Some(v.to_string()),
                })
                .collect();
            Ok(Value::Text(parts.join(&delimiter)))
        }
        "STRING_TO_ARRAY" => {
            let mut iter = args.into_iter();
            let text = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            let delimiter = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => {
                    return Ok(Value::Array(
                        text.chars().map(|c| Value::Text(c.to_string())).collect(),
                    ))
                }
                Some(v) => v.to_string(),
                None => return Ok(Value::Array(vec![Value::Text(text)])),
            };
            let null_str = iter.next().and_then(|v| match v {
                Value::Text(s) => Some(s),
                Value::Null => None,
                v => Some(v.to_string()),
            });
            let parts: Vec<Value> = if delimiter.is_empty() {
                text.chars().map(|c| Value::Text(c.to_string())).collect()
            } else {
                text.split(&delimiter)
                    .map(|s| {
                        if null_str.as_ref().map_or(false, |ns| s == ns) {
                            Value::Null
                        } else {
                            Value::Text(s.to_string())
                        }
                    })
                    .collect()
            };
            Ok(Value::Array(parts))
        }
        "JSONB_ARRAY_LENGTH" | "JSON_ARRAY_LENGTH" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_array_length requires json/jsonb argument")),
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            match json_val {
                serde_json::Value::Array(arr) => Ok(Value::Int32(arr.len() as i32)),
                _ => Err(anyhow!("cannot get array length of a non-array")),
            }
        }
        "JSONB_TYPEOF" | "JSON_TYPEOF" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_typeof requires json/jsonb argument")),
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            let type_name = match json_val {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => "object",
            };
            Ok(Value::Text(type_name.to_string()))
        }
        "JSONB_BUILD_OBJECT" | "JSON_BUILD_OBJECT" => {
            let mut obj = serde_json::Map::new();
            let mut iter = args.into_iter();
            while let Some(key) = iter.next() {
                let key_str = match key {
                    Value::Text(s) => s,
                    Value::Null => "null".to_string(),
                    v => v.to_string(),
                };
                let val = iter.next().unwrap_or(Value::Null);
                let json_val = value_to_json(&val);
                obj.insert(key_str, json_val);
            }
            Ok(Value::Jsonb(serde_json::Value::Object(obj).to_string()))
        }
        "JSONB_BUILD_ARRAY" | "JSON_BUILD_ARRAY" => {
            let arr: Vec<serde_json::Value> = args.into_iter().map(|v| value_to_json(&v)).collect();
            Ok(Value::Jsonb(serde_json::Value::Array(arr).to_string()))
        }
        "JSONB_EXISTS" => {
            if args.len() != 2 {
                return Err(anyhow!("jsonb_exists requires exactly 2 arguments"));
            }
            let mut iter = args.into_iter();
            let json_str = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let key = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            Ok(Value::Boolean(super::jsonb::exists(&json_val, &key)))
        }
        "JSONB_EXISTS_ANY" => {
            if args.len() != 2 {
                return Err(anyhow!("jsonb_exists_any requires exactly 2 arguments"));
            }
            let mut iter = args.into_iter();
            let json_str = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let keys_val = iter.next().unwrap_or(Value::Null);

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

            let exists_any = match keys_val {
                Value::Array(keys) => {
                    if keys.iter().all(|k| matches!(k, Value::Null | Value::Text(_))) {
                        super::jsonb::exists_any(
                            &json_val,
                            keys.iter().filter_map(|k| match k {
                                Value::Text(s) => Some(s.as_str()),
                                _ => None,
                            }),
                        )
                    } else {
                        keys.iter().any(|k| {
                            if let Value::Null = k {
                                return false;
                            }
                            match k {
                                Value::Text(s) => super::jsonb::exists(&json_val, s),
                                other => super::jsonb::exists(&json_val, &other.to_string()),
                            }
                        })
                    }
                }
                Value::Null => return Ok(Value::Null),
                other => super::jsonb::exists(&json_val, &other.to_string()),
            };

            Ok(Value::Boolean(exists_any))
        }
        "JSONB_EXISTS_ALL" => {
            if args.len() != 2 {
                return Err(anyhow!("jsonb_exists_all requires exactly 2 arguments"));
            }
            let mut iter = args.into_iter();
            let json_str = match iter.next().unwrap_or(Value::Null) {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let keys_val = iter.next().unwrap_or(Value::Null);

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

            let exists_all = match keys_val {
                Value::Array(keys) => {
                    if keys.iter().all(|k| matches!(k, Value::Null | Value::Text(_))) {
                        super::jsonb::exists_all(
                            &json_val,
                            keys.iter().filter_map(|k| match k {
                                Value::Text(s) => Some(s.as_str()),
                                _ => None,
                            }),
                        )
                    } else {
                        keys.iter().all(|k| {
                            if let Value::Null = k {
                                return true;
                            }
                            match k {
                                Value::Text(s) => super::jsonb::exists(&json_val, s),
                                other => super::jsonb::exists(&json_val, &other.to_string()),
                            }
                        })
                    }
                }
                Value::Null => return Ok(Value::Null),
                other => super::jsonb::exists(&json_val, &other.to_string()),
            };

            Ok(Value::Boolean(exists_all))
        }
        "JSONB_OBJECT_KEYS" | "JSON_OBJECT_KEYS" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_object_keys requires json/jsonb argument")),
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            match json_val {
                serde_json::Value::Object(obj) => {
                    let keys: Vec<Value> = obj.keys().map(|k| Value::Text(k.clone())).collect();
                    Ok(Value::Array(keys))
                }
                _ => Err(anyhow!("cannot call jsonb_object_keys on a non-object")),
            }
        }
        "JSONB_EXTRACT_PATH" | "JSON_EXTRACT_PATH" => {
            let mut iter = args.into_iter();
            let json_str = match iter.next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("json_extract_path requires json/jsonb argument")),
            };
            let mut json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            for path_part in iter {
                let key = match path_part {
                    Value::Text(s) => s,
                    v => v.to_string(),
                };
                json_val = match json_val.get(&key) {
                    Some(v) => v.clone(),
                    None => return Ok(Value::Null),
                };
            }
            Ok(Value::Jsonb(json_val.to_string()))
        }
        "JSONB_EXTRACT_PATH_TEXT" | "JSON_EXTRACT_PATH_TEXT" => {
            let mut iter = args.into_iter();
            let json_str = match iter.next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => {
                    return Err(anyhow!(
                        "json_extract_path_text requires json/jsonb argument"
                    ))
                }
            };
            let mut json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            for path_part in iter {
                let key = match path_part {
                    Value::Text(s) => s,
                    v => v.to_string(),
                };
                json_val = match json_val.get(&key) {
                    Some(v) => v.clone(),
                    None => return Ok(Value::Null),
                };
            }
            match json_val {
                serde_json::Value::Null => Ok(Value::Null),
                serde_json::Value::String(s) => Ok(Value::Text(s)),
                other => Ok(Value::Text(other.to_string())),
            }
        }
        "JSONB_PRETTY" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_pretty requires json/jsonb argument")),
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            let pretty = serde_json::to_string_pretty(&json_val)
                .map_err(|e| anyhow!("Failed to format JSON: {}", e))?;
            Ok(Value::Text(pretty))
        }
        "TO_JSON" => {
            let val = args.into_iter().next().unwrap_or(Value::Null);
            let json_val = value_to_json(&val);
            Ok(Value::Json(json_val.to_string()))
        }
        "TO_JSONB" => {
            fn eval_row_object(
                expr: &Expr,
                row: Option<&Row>,
                schema: Option<&TableSchema>,
            ) -> Result<Option<serde_json::Value>> {
                fn row_values(
                    expr: &Expr,
                    row: Option<&Row>,
                    schema: Option<&TableSchema>,
                ) -> Result<Option<Vec<Value>>> {
                    match expr {
                        Expr::Nested(inner) => row_values(inner, row, schema),
                        Expr::Tuple(exprs) => {
                            let mut vals = Vec::with_capacity(exprs.len());
                            for e in exprs {
                                vals.push(eval_expr(e, row, schema)?);
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
                                    ) => vals.push(eval_expr(e, row, schema)?),
                                    _ => return Ok(None),
                                }
                            }
                            Ok(Some(vals))
                        }
                        _ => Ok(None),
                    }
                }

                let Some(values) = row_values(expr, row, schema)? else {
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
                    if let Some(obj) = eval_row_object(expr, row, schema)? {
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

        "REGEXP_REPLACE" => {
            let mut iter = args.into_iter();
            let source = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            let pattern = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Text(source)),
            };
            let replacement = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => String::new(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            let flags = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => String::new(),
            };
            let global = flags.contains('g');
            let case_insensitive = flags.contains('i');
            let regex_pattern = if case_insensitive {
                format!("(?i){}", pattern)
            } else {
                pattern
            };
            match regex::Regex::new(&regex_pattern) {
                Ok(re) => {
                    let result = if global {
                        re.replace_all(&source, replacement.as_str()).to_string()
                    } else {
                        re.replace(&source, replacement.as_str()).to_string()
                    };
                    Ok(Value::Text(result))
                }
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        "REGEXP_MATCHES" => {
            let mut iter = args.into_iter();
            let source = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            let pattern = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Array(vec![])),
            };
            let flags = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => String::new(),
            };
            let case_insensitive = flags.contains('i');
            let regex_pattern = if case_insensitive {
                format!("(?i){}", pattern)
            } else {
                pattern
            };
            match regex::Regex::new(&regex_pattern) {
                Ok(re) => {
                    if let Some(caps) = re.captures(&source) {
                        let matches: Vec<Value> = caps
                            .iter()
                            .skip(if caps.len() > 1 { 1 } else { 0 })
                            .map(|m| match m {
                                Some(m) => Value::Text(m.as_str().to_string()),
                                None => Value::Null,
                            })
                            .collect();
                        if matches.is_empty() {
                            if let Some(m) = caps.get(0) {
                                Ok(Value::Array(vec![Value::Text(m.as_str().to_string())]))
                            } else {
                                Ok(Value::Null)
                            }
                        } else {
                            Ok(Value::Array(matches))
                        }
                    } else {
                        Ok(Value::Null)
                    }
                }
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        "REGEXP_SPLIT_TO_ARRAY" => {
            let mut iter = args.into_iter();
            let source = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            let pattern = match iter.next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Array(vec![Value::Text(source)])),
                Some(v) => v.to_string(),
                None => return Ok(Value::Array(vec![Value::Text(source)])),
            };
            let flags = match iter.next() {
                Some(Value::Text(s)) => s,
                _ => String::new(),
            };
            let case_insensitive = flags.contains('i');
            let regex_pattern = if case_insensitive {
                format!("(?i){}", pattern)
            } else {
                pattern
            };
            match regex::Regex::new(&regex_pattern) {
                Ok(re) => {
                    let parts: Vec<Value> = re
                        .split(&source)
                        .map(|s| Value::Text(s.to_string()))
                        .collect();
                    Ok(Value::Array(parts))
                }
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        "PG_TYPEOF" => {
            let val = args.into_iter().next().unwrap_or(Value::Null);
            Ok(Value::Text(pg_typeof_name_impl(&val)))
        }

        "QUOTE_IDENT" => {
            let val = match args.into_iter().next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            Ok(Value::Text(quote_ident_impl(&val)))
        }

        "QUOTE_LITERAL" => {
            let val = match args.into_iter().next() {
                Some(Value::Text(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                Some(v) => v.to_string(),
                None => return Ok(Value::Null),
            };
            Ok(Value::Text(format!("'{}'", val.replace('\'', "''"))))
        }

        "QUOTE_NULLABLE" => {
            let val = match args.into_iter().next() {
                Some(Value::Null) => return Ok(Value::Text("NULL".to_string())),
                Some(Value::Text(s)) => s,
                Some(v) => v.to_string(),
                None => return Ok(Value::Text("NULL".to_string())),
            };
            Ok(Value::Text(format!("'{}'", val.replace('\'', "''"))))
        }

        "CLOCK_TIMESTAMP" | "STATEMENT_TIMESTAMP" | "TRANSACTION_TIMESTAMP" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            Ok(Value::Timestamp(ts))
        }

        "TXID_CURRENT" => Ok(Value::Int64(
            std::process::id() as i64 * 1000000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as i64
                    % 1000000,
        )),

        "PG_COLUMN_SIZE" => {
            let val = args.into_iter().next().unwrap_or(Value::Null);
            let size = match &val {
                Value::Null => 0,
                Value::Boolean(_) => 1,
                Value::Int32(_) => 4,
                Value::Int64(_) => 8,
                Value::Float64(_) => 8,
                Value::Numeric(_) => 16,
                Value::Text(s) => s.len() as i32 + 4,
                Value::Bytes(b) => b.len() as i32 + 4,
                Value::Timestamp(_) => 8,
                Value::Date(_) => 4,
                Value::Time(_) => 8,
                Value::Interval(_) => 16,
                Value::Uuid(_) => 16,
                Value::Json(s) | Value::Jsonb(s) => s.len() as i32 + 4,
                Value::Array(a) => a.len() as i32 * 8 + 4,
                Value::Vector(v) => v.len() as i32 * 4 + 4,
            };
            Ok(Value::Int32(size))
        }

        "PG_TABLE_IS_VISIBLE" => Ok(Value::Boolean(true)),

        "JSONB_SET" | "JSON_SET" => {
            let mut iter = args.into_iter();
            let json_str = match iter.next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_set requires json/jsonb as first argument")),
            };
            let path = match iter.next() {
                Some(Value::Array(arr)) => arr,
                Some(Value::Text(s)) => {
                    let trimmed = s.trim().trim_start_matches('{').trim_end_matches('}');
                    trimmed
                        .split(',')
                        .map(|p| Value::Text(p.trim().to_string()))
                        .collect()
                }
                _ => return Err(anyhow!("jsonb_set requires array path as second argument")),
            };
            let new_value = match iter.next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => {
                    serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                }
                Some(Value::Int32(n)) => serde_json::Value::Number(n.into()),
                Some(Value::Int64(n)) => serde_json::Value::Number(n.into()),
                Some(Value::Boolean(b)) => serde_json::Value::Bool(b),
                Some(Value::Null) => serde_json::Value::Null,
                Some(v) => serde_json::Value::String(v.to_string()),
                None => return Err(anyhow!("jsonb_set requires new value as third argument")),
            };
            let create_missing = match iter.next() {
                Some(Value::Boolean(b)) => b,
                _ => true,
            };

            let mut json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

            fn set_at_path(
                val: &mut serde_json::Value,
                path: &[Value],
                new_val: serde_json::Value,
                create_missing: bool,
            ) -> bool {
                if path.is_empty() {
                    *val = new_val;
                    return true;
                }
                let key = match &path[0] {
                    Value::Text(s) => s.clone(),
                    v => v.to_string(),
                };
                match val {
                    serde_json::Value::Object(obj) => {
                        if path.len() == 1 {
                            if create_missing || obj.contains_key(&key) {
                                obj.insert(key, new_val);
                                return true;
                            }
                        } else if let Some(child) = obj.get_mut(&key) {
                            return set_at_path(child, &path[1..], new_val, create_missing);
                        } else if create_missing {
                            let mut child = serde_json::Value::Object(serde_json::Map::new());
                            if set_at_path(&mut child, &path[1..], new_val, create_missing) {
                                obj.insert(key, child);
                                return true;
                            }
                        }
                    }
                    serde_json::Value::Array(arr) => {
                        if let Ok(idx) = key.parse::<usize>() {
                            if path.len() == 1 {
                                if idx < arr.len() {
                                    arr[idx] = new_val;
                                    return true;
                                } else if create_missing {
                                    while arr.len() <= idx {
                                        arr.push(serde_json::Value::Null);
                                    }
                                    arr[idx] = new_val;
                                    return true;
                                }
                            } else if idx < arr.len() {
                                return set_at_path(
                                    &mut arr[idx],
                                    &path[1..],
                                    new_val,
                                    create_missing,
                                );
                            }
                        }
                    }
                    _ => {}
                }
                false
            }

            set_at_path(&mut json_val, &path, new_value, create_missing);
            Ok(Value::Jsonb(json_val.to_string()))
        }

        "JSONB_ARRAY_ELEMENTS" | "JSON_ARRAY_ELEMENTS" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_array_elements requires json/jsonb argument")),
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            match json_val {
                serde_json::Value::Array(arr) => {
                    let elements: Vec<Value> = arr
                        .into_iter()
                        .map(|v| Value::Jsonb(v.to_string()))
                        .collect();
                    Ok(Value::Array(elements))
                }
                _ => Err(anyhow!("cannot extract elements from a non-array")),
            }
        }

        "JSONB_ARRAY_ELEMENTS_TEXT" | "JSON_ARRAY_ELEMENTS_TEXT" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => {
                    return Err(anyhow!(
                        "jsonb_array_elements_text requires json/jsonb argument"
                    ))
                }
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            match json_val {
                serde_json::Value::Array(arr) => {
                    let elements: Vec<Value> = arr
                        .into_iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => Value::Text(s),
                            serde_json::Value::Null => Value::Null,
                            other => Value::Text(other.to_string()),
                        })
                        .collect();
                    Ok(Value::Array(elements))
                }
                _ => Err(anyhow!("cannot extract elements from a non-array")),
            }
        }

        "JSONB_EACH" | "JSON_EACH" | "JSONB_EACH_TEXT" | "JSON_EACH_TEXT" => {
            let json_str = match args.into_iter().next() {
                Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
                Some(Value::Null) => return Ok(Value::Null),
                _ => return Err(anyhow!("jsonb_each requires json/jsonb argument")),
            };
            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            match json_val {
                serde_json::Value::Object(obj) => {
                    let is_text = func_name.to_uppercase().ends_with("_TEXT");
                    let pairs: Vec<Value> = obj
                        .into_iter()
                        .map(|(k, v)| {
                            let val_str = if is_text {
                                match v {
                                    serde_json::Value::String(s) => s,
                                    serde_json::Value::Null => "".to_string(),
                                    other => other.to_string(),
                                }
                            } else {
                                v.to_string()
                            };
                            Value::Text(format!("({},{})", k, val_str))
                        })
                        .collect();
                    Ok(Value::Array(pairs))
                }
                _ => Err(anyhow!("cannot call jsonb_each on a non-object")),
            }
        }

        "ROW_TO_JSON" => {
            fn eval_row_object(
                expr: &Expr,
                row: Option<&Row>,
                schema: Option<&TableSchema>,
            ) -> Result<Option<serde_json::Value>> {
                fn row_values(
                    expr: &Expr,
                    row: Option<&Row>,
                    schema: Option<&TableSchema>,
                ) -> Result<Option<Vec<Value>>> {
                    match expr {
                        Expr::Nested(inner) => row_values(inner, row, schema),
                        Expr::Tuple(exprs) => {
                            let mut vals = Vec::with_capacity(exprs.len());
                            for e in exprs {
                                vals.push(eval_expr(e, row, schema)?);
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
                                    ) => vals.push(eval_expr(e, row, schema)?),
                                    _ => return Ok(None),
                                }
                            }
                            Ok(Some(vals))
                        }
                        _ => Ok(None),
                    }
                }

                let Some(values) = row_values(expr, row, schema)? else {
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
                    if let Some(obj) = eval_row_object(expr, row, schema)? {
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

fn parse_timestamp_string(s: &str) -> Result<Value> {
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
        let datetime = dt.and_hms_opt(0, 0, 0).unwrap();
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

        let ch = fmt[i..].chars().next().unwrap();
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

fn eval_encode_from_args(args: Vec<Value>) -> Result<Value> {
    use base64::Engine;

    if args.len() != 2 {
        return Err(anyhow!("encode requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data = match iter.next().unwrap_or(Value::Null) {
        Value::Bytes(b) => b,
        Value::Text(s) => s.into_bytes(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string().into_bytes(),
    };
    let fmt = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = fmt.trim();

    if fmt.eq_ignore_ascii_case("base64") {
        return Ok(Value::Text(
            base64::engine::general_purpose::STANDARD.encode(&data),
        ));
    }
    if fmt.eq_ignore_ascii_case("hex") {
        return Ok(Value::Text(hex::encode(&data)));
    }
    if fmt.eq_ignore_ascii_case("escape") {
        return Ok(Value::Text(super::bytea::encode_escape(&data)));
    }

    Err(anyhow!("unrecognized encoding: {}", fmt))
}

fn eval_decode_from_args(args: Vec<Value>) -> Result<Value> {
    use base64::Engine;

    if args.len() != 2 {
        return Err(anyhow!("decode requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = fmt.trim();

    if fmt.eq_ignore_ascii_case("base64") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .map_err(|e| anyhow!("invalid base64 data: {}", e))?;
        return Ok(Value::Bytes(bytes));
    }
    if fmt.eq_ignore_ascii_case("hex") {
        let s = data.trim();
        let s = s.strip_prefix("\\x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| anyhow!("invalid hex data: {}", e))?;
        return Ok(Value::Bytes(bytes));
    }
    if fmt.eq_ignore_ascii_case("escape") {
        return Ok(Value::Bytes(super::bytea::decode_escape(&data)?));
    }

    Err(anyhow!("unrecognized encoding: {}", fmt))
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
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc(),
        "month" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc(),
        "day" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc(),
        "hour" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())
            .unwrap()
            .and_hms_opt(dt.hour(), 0, 0)
            .unwrap()
            .and_utc(),
        "minute" => chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())
            .unwrap()
            .and_hms_opt(dt.hour(), dt.minute(), 0)
            .unwrap()
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

fn pg_typeof_name_impl(val: &Value) -> String {
    match val {
        Value::Null => "unknown".to_string(),
        Value::Boolean(_) => "boolean".to_string(),
        Value::Int32(_) => "integer".to_string(),
        Value::Int64(_) => "bigint".to_string(),
        Value::Float64(_) => "double precision".to_string(),
        Value::Numeric(_) => "numeric".to_string(),
        Value::Text(_) => "text".to_string(),
        Value::Bytes(_) => "bytea".to_string(),
        Value::Timestamp(_) => "timestamp with time zone".to_string(),
        Value::Date(_) => "date".to_string(),
        Value::Time(_) => "time".to_string(),
        Value::Interval(_) => "interval".to_string(),
        Value::Uuid(_) => "uuid".to_string(),
        Value::Json(_) => "json".to_string(),
        Value::Jsonb(_) => "jsonb".to_string(),
        Value::Array(arr) => {
            let elem_type = arr
                .iter()
                .find(|v| !matches!(v, Value::Null))
                .map(pg_typeof_name_impl)
                .unwrap_or_else(|| "unknown".to_string());
            format!("{}[]", elem_type)
        }
        Value::Vector(_) => "vector".to_string(),
    }
}

fn eval_binary_op(left: Value, op: &BinaryOperator, right: Value) -> Result<Value> {
    match op {
        // Comparison - SQL three-valued logic: comparison with NULL returns NULL
        BinaryOperator::Eq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? == 0))
            }
        }
        BinaryOperator::NotEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? != 0))
            }
        }
        BinaryOperator::Gt => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? > 0))
            }
        }
        BinaryOperator::Lt => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? < 0))
            }
        }
        BinaryOperator::GtEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? >= 0))
            }
        }
        BinaryOperator::LtEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? <= 0))
            }
        }

        // Logical
        // SQL three-valued logic for boolean operators
        // https://www.postgresql.org/docs/current/functions-logical.html
        BinaryOperator::And => match (left, right) {
            (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Ok(Value::Boolean(false)),
            (Value::Boolean(true), Value::Boolean(true)) => Ok(Value::Boolean(true)),
            (Value::Boolean(true), Value::Null) | (Value::Null, Value::Boolean(true)) => {
                Ok(Value::Null)
            }
            (Value::Null, Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("AND requires boolean operands")),
        },
        BinaryOperator::Or => match (left, right) {
            (Value::Boolean(true), _) | (_, Value::Boolean(true)) => Ok(Value::Boolean(true)),
            (Value::Boolean(false), Value::Boolean(false)) => Ok(Value::Boolean(false)),
            (Value::Boolean(false), Value::Null) | (Value::Null, Value::Boolean(false)) => {
                Ok(Value::Null)
            }
            (Value::Null, Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("OR requires boolean operands")),
        },

        // Arithmetic
        BinaryOperator::Plus => add_values(left, right),
        BinaryOperator::Minus => sub_values(left, right),
        BinaryOperator::Multiply => mul_values(left, right),
        BinaryOperator::Divide => div_values(left, right),
        BinaryOperator::Modulo => mod_values(left, right),

        BinaryOperator::StringConcat => match (&left, &right) {
            (Value::Jsonb(l), Value::Jsonb(r)) => {
                let left_json: serde_json::Value =
                    serde_json::from_str(l).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;
                let right_json: serde_json::Value =
                    serde_json::from_str(r).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;

                let merged = match (left_json, right_json) {
                    (serde_json::Value::Object(mut lo), serde_json::Value::Object(ro)) => {
                        for (k, v) in ro {
                            lo.insert(k, v);
                        }
                        serde_json::Value::Object(lo)
                    }
                    (serde_json::Value::Array(mut la), serde_json::Value::Array(ra)) => {
                        la.extend(ra);
                        serde_json::Value::Array(la)
                    }
                    (l, r) => serde_json::Value::Array(vec![l, r]),
                };

                Ok(Value::Jsonb(merged.to_string()))
            }
            (Value::Array(l), Value::Array(r)) => {
                let mut result = l.clone();
                result.extend(r.iter().cloned());
                Ok(Value::Array(result))
            }
            (Value::Array(arr), other) => {
                let mut result = arr.clone();
                result.push(other.clone());
                Ok(Value::Array(result))
            }
            (other, Value::Array(arr)) => {
                let mut result = vec![other.clone()];
                result.extend(arr.iter().cloned());
                Ok(Value::Array(result))
            }
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            _ => {
                let left_str = match left {
                    Value::Text(s) => s,
                    v => v.to_string(),
                };
                let right_str = match right {
                    Value::Text(s) => s,
                    v => v.to_string(),
                };
                Ok(Value::Text(format!("{}{}", left_str, right_str)))
            }
        },

        BinaryOperator::PGOverlap => match (&left, &right) {
            (Value::Array(l), Value::Array(r)) => {
                for lv in l {
                    for rv in r {
                        if compare_values(lv, rv).unwrap_or(1) == 0 {
                            return Ok(Value::Boolean(true));
                        }
                    }
                }
                Ok(Value::Boolean(false))
            }
            _ => Err(anyhow!("&& operator requires array operands")),
        },

        BinaryOperator::PGRegexMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            match regex::Regex::new(&pattern) {
                Ok(re) => Ok(Value::Boolean(re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        BinaryOperator::PGRegexIMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let case_insensitive_pattern = format!("(?i){}", pattern);
            match regex::Regex::new(&case_insensitive_pattern) {
                Ok(re) => Ok(Value::Boolean(re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        BinaryOperator::PGRegexNotMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            match regex::Regex::new(&pattern) {
                Ok(re) => Ok(Value::Boolean(!re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        BinaryOperator::PGRegexNotIMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let case_insensitive_pattern = format!("(?i){}", pattern);
            match regex::Regex::new(&case_insensitive_pattern) {
                Ok(re) => Ok(Value::Boolean(!re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        // PostgreSQL JSONB existence operator: `jsonb ? text`
        // - For objects: key exists
        // - For arrays: string element exists at top-level
        // sqlparser 0.40 parses `?` as a custom operator.
        BinaryOperator::Custom(op) if op == "?" => {
            let json_str = match left {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let key = match right {
                Value::Text(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            Ok(Value::Boolean(super::jsonb::exists(&json_val, &key)))
        }
        BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "?" => {
            let json_str = match left {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let key = match right {
                Value::Text(s) => s,
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };

            let json_val: serde_json::Value =
                serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
            Ok(Value::Boolean(super::jsonb::exists(&json_val, &key)))
        }

        _ => Err(anyhow!("Unsupported binary operator: {:?}", op)),
    }
}

// --- Arithmetic Helpers ---

fn add_interval_to_timestamp(ts_millis: i64, iv: &crate::types::IntervalValue) -> Result<i64> {
    use chrono::{Datelike, Duration, TimeZone, Utc};

    let dt = Utc
        .timestamp_millis_opt(ts_millis)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;

    let mut result = dt;

    if iv.months != 0 {
        let mut year = result.year();
        let mut month = result.month() as i32 + iv.months;

        while month > 12 {
            month -= 12;
            year += 1;
        }
        while month < 1 {
            month += 12;
            year -= 1;
        }

        let day = result.day().min(days_in_month(year, month as u32));

        result = result
            .with_year(year)
            .and_then(|d| d.with_month(month as u32))
            .and_then(|d| d.with_day(day))
            .ok_or_else(|| anyhow!("Date out of range after adding months"))?;
    }

    if iv.millis != 0 {
        result = result + Duration::milliseconds(iv.millis);
    }

    Ok(result.timestamp_millis())
}

fn sub_interval_from_timestamp(ts_millis: i64, iv: &crate::types::IntervalValue) -> Result<i64> {
    let neg_iv = crate::types::IntervalValue::new(-iv.months, -iv.millis);
    add_interval_to_timestamp(ts_millis, &neg_iv)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn try_coerce_text_to_numeric(v: Value) -> Value {
    match &v {
        Value::Text(s) => {
            if let Ok(i) = s.trim().parse::<i64>() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    return Value::Int32(i as i32);
                }
                return Value::Int64(i);
            }
            if let Ok(f) = s.trim().parse::<f64>() {
                return Value::Float64(f);
            }
            v
        }
        _ => v,
    }
}

fn add_values(left: Value, right: Value) -> Result<Value> {
    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(Value::Int32(l + r)),
        (Value::Int64(l), Value::Int64(r)) => Ok(Value::Int64(l + r)),
        (Value::Int32(l), Value::Int64(r)) => Ok(Value::Int64(l as i64 + r)),
        (Value::Int64(l), Value::Int32(r)) => Ok(Value::Int64(l + r as i64)),
        (Value::Float64(l), Value::Float64(r)) => Ok(Value::Float64(l + r)),
        (Value::Int32(l), Value::Float64(r)) => Ok(Value::Float64(l as f64 + r)),
        (Value::Float64(l), Value::Int32(r)) => Ok(Value::Float64(l + r as f64)),
        (Value::Numeric(l), Value::Numeric(r)) => Ok(Value::Numeric(l + r)),
        (Value::Numeric(l), Value::Int32(r)) => Ok(Value::Numeric(l + Decimal::from(r))),
        (Value::Int32(l), Value::Numeric(r)) => Ok(Value::Numeric(Decimal::from(l) + r)),
        (Value::Numeric(l), Value::Int64(r)) => Ok(Value::Numeric(l + Decimal::from(r))),
        (Value::Int64(l), Value::Numeric(r)) => Ok(Value::Numeric(Decimal::from(l) + r)),
        (Value::Numeric(l), Value::Float64(r)) => {
            use rust_decimal::prelude::ToPrimitive;
            let lf = l
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(lf + r))
        }
        (Value::Float64(l), Value::Numeric(r)) => {
            use rust_decimal::prelude::ToPrimitive;
            let rf = r
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(l + rf))
        }
        (Value::Timestamp(ts), Value::Interval(iv)) => {
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Interval(iv), Value::Timestamp(ts)) => {
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Interval(iv)) => {
            let ts = crate::types::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Interval(iv), Value::Date(days)) => {
            let ts = crate::types::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Int32(n)) => Ok(Value::Date(days + n)),
        (Value::Int32(n), Value::Date(days)) => Ok(Value::Date(days + n)),
        (Value::Date(days), Value::Int64(n)) => {
            let result = (days as i64) + n;
            Ok(Value::Date(
                i32::try_from(result).map_err(|_| anyhow!("date out of range"))?,
            ))
        }
        (Value::Int64(n), Value::Date(days)) => {
            let result = (days as i64) + n;
            Ok(Value::Date(
                i32::try_from(result).map_err(|_| anyhow!("date out of range"))?,
            ))
        }
        (Value::Interval(l), Value::Interval(r)) => Ok(Value::Interval(l + r)),
        _ => Err(anyhow!("Unsupported types for addition")),
    }
}

fn sub_values(left: Value, right: Value) -> Result<Value> {
    if matches!(left, Value::Jsonb(_)) {
        return jsonb_subtract(left, right);
    }

    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(Value::Int32(l - r)),
        (Value::Int64(l), Value::Int64(r)) => Ok(Value::Int64(l - r)),
        (Value::Int32(l), Value::Int64(r)) => Ok(Value::Int64(l as i64 - r)),
        (Value::Int64(l), Value::Int32(r)) => Ok(Value::Int64(l - r as i64)),
        (Value::Float64(l), Value::Float64(r)) => Ok(Value::Float64(l - r)),
        (Value::Numeric(l), Value::Numeric(r)) => Ok(Value::Numeric(l - r)),
        (Value::Numeric(l), Value::Int32(r)) => Ok(Value::Numeric(l - Decimal::from(r))),
        (Value::Int32(l), Value::Numeric(r)) => Ok(Value::Numeric(Decimal::from(l) - r)),
        (Value::Numeric(l), Value::Int64(r)) => Ok(Value::Numeric(l - Decimal::from(r))),
        (Value::Int64(l), Value::Numeric(r)) => Ok(Value::Numeric(Decimal::from(l) - r)),
        (Value::Numeric(l), Value::Float64(r)) => {
            use rust_decimal::prelude::ToPrimitive;
            let lf = l
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(lf - r))
        }
        (Value::Float64(l), Value::Numeric(r)) => {
            use rust_decimal::prelude::ToPrimitive;
            let rf = r
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(l - rf))
        }
        (Value::Timestamp(l), Value::Timestamp(r)) => Ok(Value::Interval(
            crate::types::IntervalValue::from_millis(l - r),
        )),
        (Value::Timestamp(ts), Value::Interval(iv)) => {
            Ok(Value::Timestamp(sub_interval_from_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Interval(iv)) => {
            let ts = crate::types::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(sub_interval_from_timestamp(ts, &iv)?))
        }
        (Value::Date(l), Value::Date(r)) => {
            let diff = (l as i64) - (r as i64);
            let days = i32::try_from(diff).map_err(|_| anyhow!("date difference out of range"))?;
            Ok(Value::Int32(days))
        }
        (Value::Date(days), Value::Int32(n)) => Ok(Value::Date(days - n)),
        (Value::Date(days), Value::Int64(n)) => {
            let result = (days as i64) - n;
            Ok(Value::Date(
                i32::try_from(result).map_err(|_| anyhow!("date out of range"))?,
            ))
        }
        (Value::Interval(l), Value::Interval(r)) => Ok(Value::Interval(l - r)),
        _ => Err(anyhow!("Unsupported types for subtraction")),
    }
}

fn jsonb_subtract(left: Value, right: Value) -> Result<Value> {
    let Value::Jsonb(json_str) = left else {
        return Err(anyhow!("jsonb subtraction requires jsonb left operand"));
    };
    if matches!(right, Value::Null) {
        return Ok(Value::Null);
    }

    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;

    match right {
        Value::Text(key) => match &mut json_val {
            serde_json::Value::Object(obj) => {
                obj.remove(&key);
            }
            serde_json::Value::Array(arr) => {
                arr.retain(|v| v.as_str() != Some(key.as_str()));
            }
            _ => {}
        },
        Value::Int32(idx) => match &mut json_val {
            serde_json::Value::Array(arr) => {
                let len = arr.len() as i32;
                let idx = if idx < 0 { len + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    arr.remove(idx as usize);
                }
            }
            _ => {}
        },
        Value::Int64(idx) => match &mut json_val {
            serde_json::Value::Array(arr) => {
                let len = arr.len() as i64;
                let idx = if idx < 0 { len + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    arr.remove(idx as usize);
                }
            }
            _ => {}
        },
        other => {
            return Err(anyhow!(
                "unsupported right operand for jsonb subtraction: {:?}",
                other
            ))
        }
    }

    Ok(Value::Jsonb(json_val.to_string()))
}

fn mul_values(left: Value, right: Value) -> Result<Value> {
    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(Value::Int32(l * r)),
        (Value::Int64(l), Value::Int64(r)) => Ok(Value::Int64(l * r)),
        (Value::Int32(l), Value::Int64(r)) => Ok(Value::Int64(l as i64 * r)),
        (Value::Int64(l), Value::Int32(r)) => Ok(Value::Int64(l * r as i64)),
        (Value::Float64(l), Value::Float64(r)) => Ok(Value::Float64(l * r)),
        (Value::Int32(l), Value::Float64(r)) => Ok(Value::Float64(l as f64 * r)),
        (Value::Float64(l), Value::Int32(r)) => Ok(Value::Float64(l * r as f64)),
        (Value::Int64(l), Value::Float64(r)) => Ok(Value::Float64(l as f64 * r)),
        (Value::Float64(l), Value::Int64(r)) => Ok(Value::Float64(l * r as f64)),
        (Value::Numeric(l), Value::Numeric(r)) => Ok(Value::Numeric(l * r)),
        (Value::Numeric(l), Value::Int32(r)) => Ok(Value::Numeric(l * Decimal::from(r))),
        (Value::Int32(l), Value::Numeric(r)) => Ok(Value::Numeric(Decimal::from(l) * r)),
        (Value::Numeric(l), Value::Int64(r)) => Ok(Value::Numeric(l * Decimal::from(r))),
        (Value::Int64(l), Value::Numeric(r)) => Ok(Value::Numeric(Decimal::from(l) * r)),
        (Value::Numeric(l), Value::Float64(r)) => {
            use rust_decimal::prelude::ToPrimitive;
            let lf = l
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(lf * r))
        }
        (Value::Float64(l), Value::Numeric(r)) => {
            use rust_decimal::prelude::ToPrimitive;
            let rf = r
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(l * rf))
        }
        _ => Err(anyhow!("Unsupported types for multiplication")),
    }
}

fn div_values(left: Value, right: Value) -> Result<Value> {
    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => {
            if r == 0 {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Int32(l / r))
        }
        (Value::Int64(l), Value::Int64(r)) => {
            if r == 0 {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Int64(l / r))
        }
        (Value::Int32(l), Value::Int64(r)) => {
            if r == 0 {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Int64(l as i64 / r))
        }
        (Value::Int64(l), Value::Int32(r)) => {
            if r == 0 {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Int64(l / r as i64))
        }
        (Value::Float64(l), Value::Float64(r)) => Ok(Value::Float64(l / r)),
        (Value::Int32(l), Value::Float64(r)) => Ok(Value::Float64(l as f64 / r)),
        (Value::Float64(l), Value::Int32(r)) => Ok(Value::Float64(l / r as f64)),
        (Value::Int64(l), Value::Float64(r)) => Ok(Value::Float64(l as f64 / r)),
        (Value::Float64(l), Value::Int64(r)) => Ok(Value::Float64(l / r as f64)),
        (Value::Numeric(l), Value::Numeric(r)) => {
            if r.is_zero() {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Numeric(l / r))
        }
        (Value::Numeric(l), Value::Int32(r)) => {
            if r == 0 {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Numeric(l / Decimal::from(r)))
        }
        (Value::Int32(l), Value::Numeric(r)) => {
            if r.is_zero() {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Numeric(Decimal::from(l) / r))
        }
        (Value::Numeric(l), Value::Int64(r)) => {
            if r == 0 {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Numeric(l / Decimal::from(r)))
        }
        (Value::Int64(l), Value::Numeric(r)) => {
            if r.is_zero() {
                return Err(anyhow!("Division by zero"));
            }
            Ok(Value::Numeric(Decimal::from(l) / r))
        }
        (Value::Numeric(l), Value::Float64(r)) => {
            if r == 0.0 {
                return Err(anyhow!("Division by zero"));
            }
            use rust_decimal::prelude::ToPrimitive;
            let lf = l
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(lf / r))
        }
        (Value::Float64(l), Value::Numeric(r)) => {
            if r.is_zero() {
                return Err(anyhow!("Division by zero"));
            }
            use rust_decimal::prelude::ToPrimitive;
            let rf = r
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(l / rf))
        }
        _ => Err(anyhow!("Unsupported types for division")),
    }
}

fn mod_values(left: Value, right: Value) -> Result<Value> {
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => {
            if r == 0 {
                return Err(anyhow!("Modulo by zero"));
            }
            Ok(Value::Int32(l % r))
        }
        (Value::Int64(l), Value::Int64(r)) => {
            if r == 0 {
                return Err(anyhow!("Modulo by zero"));
            }
            Ok(Value::Int64(l % r))
        }
        (Value::Int32(l), Value::Int64(r)) => {
            if r == 0 {
                return Err(anyhow!("Modulo by zero"));
            }
            Ok(Value::Int64(l as i64 % r))
        }
        (Value::Int64(l), Value::Int32(r)) => {
            if r == 0 {
                return Err(anyhow!("Modulo by zero"));
            }
            Ok(Value::Int64(l % r as i64))
        }
        _ => Err(anyhow!("Unsupported types for modulo")),
    }
}

/// Compare two values. Returns:
/// - 0: equal
/// - 1: left > right
/// - -1: left < right
pub fn compare_values(left: &Value, right: &Value) -> Result<i8> {
    fn compare_text_pg(left: &str, right: &str) -> std::cmp::Ordering {
        // Approximate PostgreSQL's default collation behavior for ASCII:
        // compare case-insensitively first, then order lowercase before uppercase.
        let left_fold = left.to_ascii_lowercase();
        let right_fold = right.to_ascii_lowercase();
        match left_fold.cmp(&right_fold) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }

        for (l, r) in left
            .as_bytes()
            .iter()
            .copied()
            .zip(right.as_bytes().iter().copied())
        {
            if l == r {
                continue;
            }

            let l_fold = l.to_ascii_lowercase();
            let r_fold = r.to_ascii_lowercase();
            if l_fold != r_fold {
                return l_fold.cmp(&r_fold);
            }

            let l_is_upper = l.is_ascii_uppercase();
            let r_is_upper = r.is_ascii_uppercase();
            if l_is_upper != r_is_upper {
                return if l_is_upper {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }

            return l.cmp(&r);
        }

        left.len().cmp(&right.len())
    }

    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(l.cmp(r) as i8),
        (Value::Int64(l), Value::Int64(r)) => Ok(l.cmp(r) as i8),
        (Value::Int32(l), Value::Int64(r)) => Ok((*l as i64).cmp(r) as i8),
        (Value::Int64(l), Value::Int32(r)) => Ok(l.cmp(&(*r as i64)) as i8),
        (Value::Float64(l), Value::Float64(r)) => {
            Ok(l.partial_cmp(r).unwrap_or(std::cmp::Ordering::Equal) as i8)
        }
        (Value::Text(l), Value::Text(r)) => Ok(compare_text_pg(l, r) as i8),
        (Value::Boolean(l), Value::Boolean(r)) => Ok(l.cmp(r) as i8),
        (Value::Timestamp(l), Value::Timestamp(r)) => Ok(l.cmp(r) as i8),
        (Value::Date(l), Value::Date(r)) => Ok(l.cmp(r) as i8),
        (Value::Date(l), Value::Timestamp(r)) => {
            let l_ts = crate::types::date::date_days_to_timestamp_millis(*l)?;
            Ok(l_ts.cmp(r) as i8)
        }
        (Value::Timestamp(l), Value::Date(r)) => {
            let r_ts = crate::types::date::date_days_to_timestamp_millis(*r)?;
            Ok(l.cmp(&r_ts) as i8)
        }
        (Value::Timestamp(l), Value::Text(r)) => match parse_timestamp_string(r)? {
            Value::Timestamp(r_ts) => Ok(l.cmp(&r_ts) as i8),
            _ => Err(anyhow!("Cannot compare")),
        },
        (Value::Text(l), Value::Timestamp(r)) => match parse_timestamp_string(l)? {
            Value::Timestamp(l_ts) => Ok(l_ts.cmp(r) as i8),
            _ => Err(anyhow!("Cannot compare")),
        },
        (Value::Date(l), Value::Text(r)) => {
            let r_days = crate::types::date::parse_date_days(r)?;
            Ok(l.cmp(&r_days) as i8)
        }
        (Value::Text(l), Value::Date(r)) => {
            let l_days = crate::types::date::parse_date_days(l)?;
            Ok(l_days.cmp(r) as i8)
        }
        (Value::Uuid(l), Value::Uuid(r)) => Ok(l.cmp(r) as i8),
        (Value::Bytes(l), Value::Bytes(r)) => Ok(l.cmp(r) as i8),
        (Value::Array(l), Value::Array(r)) => {
            let min_len = l.len().min(r.len());
            for i in 0..min_len {
                let ord = compare_values(&l[i], &r[i])?;
                if ord != 0 {
                    return Ok(ord);
                }
            }
            Ok(l.len().cmp(&r.len()) as i8)
        }
        (Value::Null, Value::Null) => Ok(0),
        (Value::Null, _) => Ok(-1),
        (_, Value::Null) => Ok(1),
        (Value::Text(t), Value::Int32(i)) => {
            if let Ok(n) = t.parse::<i32>() {
                Ok(n.cmp(i) as i8)
            } else {
                Ok(t.cmp(&i.to_string()) as i8)
            }
        }
        (Value::Int32(i), Value::Text(t)) => {
            if let Ok(n) = t.parse::<i32>() {
                Ok(i.cmp(&n) as i8)
            } else {
                Ok(i.to_string().cmp(t) as i8)
            }
        }
        (Value::Text(t), Value::Int64(i)) => {
            if let Ok(n) = t.parse::<i64>() {
                Ok(n.cmp(i) as i8)
            } else {
                Ok(t.cmp(&i.to_string()) as i8)
            }
        }
        (Value::Int64(i), Value::Text(t)) => {
            if let Ok(n) = t.parse::<i64>() {
                Ok(i.cmp(&n) as i8)
            } else {
                Ok(i.to_string().cmp(t) as i8)
            }
        }
        (Value::Text(t), Value::Float64(f)) => {
            if let Ok(n) = t.parse::<f64>() {
                Ok(n.partial_cmp(f).unwrap_or(std::cmp::Ordering::Equal) as i8)
            } else {
                Err(anyhow!("Cannot compare"))
            }
        }
        (Value::Float64(f), Value::Text(t)) => {
            if let Ok(n) = t.parse::<f64>() {
                Ok(f.partial_cmp(&n).unwrap_or(std::cmp::Ordering::Equal) as i8)
            } else {
                Err(anyhow!("Cannot compare"))
            }
        }
        (Value::Int32(i), Value::Float64(f)) => Ok(((*i as f64)
            .partial_cmp(f)
            .unwrap_or(std::cmp::Ordering::Equal))
            as i8),
        (Value::Float64(f), Value::Int32(i)) => {
            Ok(f.partial_cmp(&(*i as f64))
                .unwrap_or(std::cmp::Ordering::Equal) as i8)
        }
        (Value::Int64(i), Value::Float64(f)) => Ok(((*i as f64)
            .partial_cmp(f)
            .unwrap_or(std::cmp::Ordering::Equal))
            as i8),
        (Value::Float64(f), Value::Int64(i)) => {
            Ok(f.partial_cmp(&(*i as f64))
                .unwrap_or(std::cmp::Ordering::Equal) as i8)
        }
        (Value::Numeric(l), Value::Numeric(r)) => Ok(l.cmp(r) as i8),
        (Value::Numeric(d), Value::Int32(i)) => Ok(d.cmp(&Decimal::from(*i)) as i8),
        (Value::Int32(i), Value::Numeric(d)) => Ok(Decimal::from(*i).cmp(d) as i8),
        (Value::Numeric(d), Value::Int64(i)) => Ok(d.cmp(&Decimal::from(*i)) as i8),
        (Value::Int64(i), Value::Numeric(d)) => Ok(Decimal::from(*i).cmp(d) as i8),
        (Value::Numeric(d), Value::Float64(f)) => {
            if let Some(fd) = Decimal::try_from(*f).ok() {
                Ok(d.cmp(&fd) as i8)
            } else {
                use rust_decimal::prelude::ToPrimitive;
                Ok(d.to_f64()
                    .unwrap_or(f64::NAN)
                    .partial_cmp(f)
                    .unwrap_or(std::cmp::Ordering::Equal) as i8)
            }
        }
        (Value::Float64(f), Value::Numeric(d)) => {
            if let Some(fd) = Decimal::try_from(*f).ok() {
                Ok(fd.cmp(d) as i8)
            } else {
                use rust_decimal::prelude::ToPrimitive;
                Ok(f.partial_cmp(&d.to_f64().unwrap_or(f64::NAN))
                    .unwrap_or(std::cmp::Ordering::Equal) as i8)
            }
        }
        (Value::Numeric(d), Value::Text(t)) => {
            if let Ok(td) = Decimal::from_str(t) {
                Ok(d.cmp(&td) as i8)
            } else {
                Err(anyhow!("Cannot compare numeric with non-numeric string"))
            }
        }
        (Value::Text(t), Value::Numeric(d)) => {
            if let Ok(td) = Decimal::from_str(t) {
                Ok(td.cmp(d) as i8)
            } else {
                Err(anyhow!("Cannot compare numeric with non-numeric string"))
            }
        }
        (Value::Json(_), _) | (_, Value::Json(_)) => Err(anyhow!(
            "could not identify a comparison function for type json"
        )),
        (Value::Jsonb(_), _) | (_, Value::Jsonb(_)) => Err(anyhow!(
            "could not identify an ordering operator for type jsonb"
        )),
        (Value::Vector(_), _) | (_, Value::Vector(_)) => Err(anyhow!(
            "Vectors cannot be directly compared. Use vector distance functions instead."
        )),
        _ => Err(anyhow!(
            "Cannot compare distinct types: {:?} vs {:?}",
            left,
            right
        )),
    }
}

/// ORDER BY comparator with PostgreSQL-like NULLS FIRST/LAST semantics.
pub fn compare_order_by_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => {
            if nulls_first {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        _ => {
            let cmp = compare_values(left, right).unwrap_or(0);
            if cmp == 0 {
                std::cmp::Ordering::Equal
            } else if asc {
                if cmp > 0 {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            } else if cmp > 0 {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
    }
}

fn eval_json_access_expr(
    left: &Expr,
    operator: &JsonOperator,
    right: &Expr,
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<Value> {
    if let Expr::InList {
        expr: in_expr,
        list,
        negated,
    } = right
    {
        let json_result = eval_json_access_expr(left, operator, in_expr, row, schema)?;
        let mut found = false;
        for item in list {
            let item_val = eval_expr(item, row, schema)?;
            if compare_values(&json_result, &item_val).unwrap_or(1) == 0 {
                found = true;
                break;
            }
        }
        return Ok(Value::Boolean(if *negated { !found } else { found }));
    }

    if let Expr::BinaryOp {
        left: bin_left,
        op: bin_op,
        right: bin_right,
    } = right
    {
        let json_result = eval_json_access_expr(left, operator, bin_left, row, schema)?;
        let right_val = eval_expr(bin_right, row, schema)?;
        return eval_binary_op(json_result, bin_op, right_val);
    }

    let mut ops: Vec<(&Expr, &JsonOperator)> = Vec::new();
    collect_json_ops(operator, right, &mut ops);

    let mut current = eval_expr(left, row, schema)?;
    for (key_expr, op) in ops {
        let key = eval_expr(key_expr, row, schema)?;
        current = eval_json_access(current, op, key)?;
    }
    Ok(current)
}

fn eval_json_access_expr_join(
    left: &Expr,
    operator: &JsonOperator,
    right: &Expr,
    ctx: &JoinContext,
) -> Result<Value> {
    if let Expr::InList {
        expr: in_expr,
        list,
        negated,
    } = right
    {
        let json_result = eval_json_access_expr_join(left, operator, in_expr, ctx)?;
        let mut found = false;
        for item in list {
            let item_val = eval_expr_join(item, ctx)?;
            if compare_values(&json_result, &item_val).unwrap_or(1) == 0 {
                found = true;
                break;
            }
        }
        return Ok(Value::Boolean(if *negated { !found } else { found }));
    }

    if let Expr::BinaryOp {
        left: bin_left,
        op: bin_op,
        right: bin_right,
    } = right
    {
        let json_result = eval_json_access_expr_join(left, operator, bin_left, ctx)?;
        let right_val = eval_expr_join(bin_right, ctx)?;
        return eval_binary_op(json_result, bin_op, right_val);
    }

    let mut ops: Vec<(&Expr, &JsonOperator)> = Vec::new();
    collect_json_ops(operator, right, &mut ops);

    let mut current = eval_expr_join(left, ctx)?;
    for (key_expr, op) in ops {
        let key = eval_expr_join(key_expr, ctx)?;
        current = eval_json_access(current, op, key)?;
    }
    Ok(current)
}

fn collect_json_ops<'a>(
    operator: &'a JsonOperator,
    right: &'a Expr,
    ops: &mut Vec<(&'a Expr, &'a JsonOperator)>,
) {
    if let Expr::JsonAccess {
        left: inner_left,
        operator: inner_op,
        right: inner_right,
    } = right
    {
        ops.push((inner_left, operator));
        collect_json_ops(inner_op, inner_right, ops);
    } else {
        ops.push((right, operator));
    }
}

fn eval_json_access(left: Value, operator: &JsonOperator, right: Value) -> Result<Value> {
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

fn eval_array_index(
    arr_val: Value,
    indexes: &[Expr],
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<Value> {
    let Value::Array(arr) = arr_val else {
        return Err(anyhow!("Cannot index non-array value"));
    };

    let mut current = Value::Array(arr);
    for idx_expr in indexes {
        let idx_val = eval_expr(idx_expr, row, schema)?;
        let idx = match idx_val {
            Value::Int32(i) => i as i64,
            Value::Int64(i) => i,
            _ => return Err(anyhow!("Array index must be an integer")),
        };

        let Value::Array(arr) = current else {
            return Err(anyhow!("Cannot index non-array value"));
        };

        let pg_idx = (idx - 1) as usize;
        current = arr.get(pg_idx).cloned().unwrap_or(Value::Null);
    }
    Ok(current)
}

fn eval_array_index_join(arr_val: Value, indexes: &[Expr], ctx: &JoinContext) -> Result<Value> {
    let Value::Array(arr) = arr_val else {
        return Err(anyhow!("Cannot index non-array value"));
    };

    let mut current = Value::Array(arr);
    for idx_expr in indexes {
        let idx_val = eval_expr_join(idx_expr, ctx)?;
        let idx = match idx_val {
            Value::Int32(i) => i as i64,
            Value::Int64(i) => i,
            _ => return Err(anyhow!("Array index must be an integer")),
        };

        let Value::Array(arr) = current else {
            return Err(anyhow!("Cannot index non-array value"));
        };

        let pg_idx = (idx - 1) as usize;
        current = arr.get(pg_idx).cloned().unwrap_or(Value::Null);
    }
    Ok(current)
}

// ============================================================================
// Vector Support Functions
// ============================================================================

/// Parse a vector literal from a string like "[1.0, 2.0, 3.0]"
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
    use rust_decimal::Decimal;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
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

        let result = add_values(ts.clone(), iv.clone()).unwrap();
        assert_eq!(result, Value::Timestamp(1000 * 60 * 60 * 25));

        let result = sub_values(ts.clone(), iv.clone()).unwrap();
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
