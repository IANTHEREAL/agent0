//! Helper functions for SQL execution
//!
//! This module contains utility functions extracted from executor.rs
//! to reduce code size and improve maintainability.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    ArrayAgg, BinaryOperator, DataType as SqlDataType, Expr, Function, FunctionArg,
    FunctionArgExpr, Ident, Query, SetExpr, TableFactor, Value as SqlValue,
};

#[derive(Debug, Clone)]
pub enum AggExpr {
    Function(Function),
    ArrayAgg(ArrayAgg),
}

pub fn normalize_ident(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_lowercase()
    }
}

use rust_decimal::Decimal;
use std::str::FromStr;

use super::expr::{eval_expr, eval_expr_join, JoinContext};
use super::Aggregator;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

/// Deduplicate rows based on their serialized values
pub fn dedup_rows(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut result = Vec::new();
    for row in rows {
        let key = bincode::serialize(&row.values).unwrap_or_default();
        if seen.insert(key) {
            result.push(row);
        }
    }
    result
}

#[allow(dead_code)]
pub fn distinct_on_rows(
    rows: Vec<Row>,
    on_exprs: &[Expr],
    row_context: Option<&TableSchema>,
) -> Vec<Row> {
    distinct_on_rows_with_indices(rows, on_exprs, row_context).0
}

pub fn distinct_on_rows_with_indices(
    rows: Vec<Row>,
    on_exprs: &[Expr],
    row_context: Option<&TableSchema>,
) -> (Vec<Row>, Vec<usize>) {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut result = Vec::new();
    let mut indices = Vec::new();

    for (idx, row) in rows.into_iter().enumerate() {
        let key_values: Vec<Value> = on_exprs
            .iter()
            .filter_map(|expr| eval_expr(expr, Some(&row), row_context).ok())
            .collect();
        let key = bincode::serialize(&key_values).unwrap_or_default();
        if seen.insert(key) {
            indices.push(idx);
            result.push(row);
        }
    }
    (result, indices)
}

#[allow(dead_code)]
pub fn distinct_on_rows_join(
    rows: Vec<Row>,
    on_exprs: &[Expr],
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_schema: &TableSchema,
) -> Vec<Row> {
    distinct_on_rows_join_with_indices(rows, on_exprs, column_offsets, combined_schema).0
}

pub fn distinct_on_rows_join_with_indices(
    rows: Vec<Row>,
    on_exprs: &[Expr],
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_schema: &TableSchema,
) -> (Vec<Row>, Vec<usize>) {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut result = Vec::new();
    let mut indices = Vec::new();

    for (idx, row) in rows.into_iter().enumerate() {
        let ctx = JoinContext {
            tables: std::collections::HashMap::new(),
            column_offsets: column_offsets.clone(),
            combined_row: &row,
            combined_schema,
        };
        let key_values: Vec<Value> = on_exprs
            .iter()
            .filter_map(|expr| eval_expr_join(expr, &ctx).ok())
            .collect();
        let key = bincode::serialize(&key_values).unwrap_or_default();
        if seen.insert(key) {
            indices.push(idx);
            result.push(row);
        }
    }
    (result, indices)
}

pub fn apply_offset_limit_fetch(mut rows: Vec<Row>, query: &Query) -> Vec<Row> {
    if let Some(offset) = &query.offset {
        if let Ok(v) = eval_expr(&offset.value, None, None) {
            let n = match v {
                Value::Int64(n) => n as usize,
                Value::Int32(n) => n as usize,
                _ => 0,
            };
            rows = rows.into_iter().skip(n).collect();
        }
    }
    if let Some(limit) = &query.limit {
        if let Ok(v) = eval_expr(limit, None, None) {
            let n = match v {
                Value::Int64(n) => n as usize,
                Value::Int32(n) => n as usize,
                _ => usize::MAX,
            };
            rows = rows.into_iter().take(n).collect();
        }
    }

    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            if let Ok(v) = eval_expr(quantity, None, None) {
                let n = match v {
                    Value::Int64(n) => n as usize,
                    Value::Int32(n) => n as usize,
                    _ => 1,
                };
                rows = rows.into_iter().take(n).collect();
            }
        } else {
            rows = rows.into_iter().take(1).collect();
        }
    }
    rows
}

/// Coerce a value to match the expected column type
pub fn coerce_value_for_column(val: Value, col: &ColumnDef) -> Result<Value> {
    match (&val, &col.data_type) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Text(s), DataType::Date) => crate::types::date::parse_date_days(s).map(Value::Date),
        (Value::Timestamp(ts), DataType::Date) => {
            crate::types::date::timestamp_millis_to_date_days(*ts).map(Value::Date)
        }
        (Value::Text(s), DataType::Int32) => s
            .trim()
            .parse::<i32>()
            .map(Value::Int32)
            .map_err(|_| anyhow!("invalid input syntax for type integer: \"{}\"", s)),
        (Value::Int64(n), DataType::Int32) => i32::try_from(*n)
            .map(Value::Int32)
            .map_err(|_| anyhow!("integer out of range: {}", n)),
        (Value::Float64(f), DataType::Int32) => {
            if f.fract() != 0.0 {
                return Err(anyhow!("invalid input syntax for type integer: \"{}\"", f));
            }
            let n = *f as i64;
            i32::try_from(n)
                .map(Value::Int32)
                .map_err(|_| anyhow!("integer out of range: {}", n))
        }
        (Value::Text(s), DataType::Int64) => s
            .trim()
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| anyhow!("invalid input syntax for type bigint: \"{}\"", s)),
        (Value::Int32(n), DataType::Int64) => Ok(Value::Int64(*n as i64)),
        (Value::Text(s), DataType::Float64) => s
            .trim()
            .parse::<f64>()
            .map(Value::Float64)
            .map_err(|_| anyhow!("invalid input syntax for type double precision: \"{}\"", s)),
        (Value::Int32(n), DataType::Float64) => Ok(Value::Float64(*n as f64)),
        (Value::Int64(n), DataType::Float64) => Ok(Value::Float64(*n as f64)),
        (Value::Text(s), DataType::Boolean) => match s.trim().to_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Ok(Value::Boolean(true)),
            "false" | "f" | "no" | "n" | "0" => Ok(Value::Boolean(false)),
            _ => Err(anyhow!("invalid input syntax for type boolean: \"{}\"", s)),
        },
        (Value::Text(s), DataType::Uuid) => uuid::Uuid::parse_str(s.trim())
            .map(|u| Value::Uuid(*u.as_bytes()))
            .map_err(|_| anyhow!("invalid input syntax for type uuid: \"{}\"", s)),
        (Value::Text(s), DataType::Json) => {
            serde_json::from_str::<serde_json::Value>(s)
                .map_err(|e| anyhow!("invalid input syntax for type json: {}", e))?;
            Ok(Value::Json(s.clone()))
        }
        (Value::Text(s), DataType::Jsonb) => {
            let parsed: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| anyhow!("invalid input syntax for type jsonb: {}", e))?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        (Value::Json(s), DataType::Json) => Ok(Value::Json(s.clone())),
        (Value::Json(s), DataType::Jsonb) => {
            let parsed: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| anyhow!("invalid input syntax for type jsonb: {}", e))?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        (Value::Jsonb(s), DataType::Json) => Ok(Value::Json(s.clone())),
        (Value::Jsonb(s), DataType::Jsonb) => Ok(Value::Jsonb(s.clone())),
        (Value::Text(s), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from_str(s.trim())
                .map_err(|_| anyhow!("invalid input syntax for type numeric: \"{}\"", s))?;
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Int32(n), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from(*n);
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Int64(n), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from(*n);
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Float64(f), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::try_from(*f)
                .map_err(|_| anyhow!("invalid input for type numeric: \"{}\"", f))?;
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Numeric(d), DataType::Numeric { scale, .. }) => {
            let mut d = *d;
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Numeric(d), DataType::Float64) => {
            use rust_decimal::prelude::ToPrimitive;
            Ok(Value::Float64(d.to_f64().unwrap_or(f64::NAN)))
        }
        (Value::Numeric(d), DataType::Int32) => {
            use rust_decimal::prelude::ToPrimitive;
            d.to_i32()
                .map(Value::Int32)
                .ok_or_else(|| anyhow!("numeric value out of range for integer"))
        }
        (Value::Numeric(d), DataType::Int64) => {
            use rust_decimal::prelude::ToPrimitive;
            d.to_i64()
                .map(Value::Int64)
                .ok_or_else(|| anyhow!("numeric value out of range for bigint"))
        }
        _ => Ok(val),
    }
}

/// Convert a Value to a SQL expression for re-parsing
pub fn value_to_sql_expr(v: &Value) -> Expr {
    match v {
        Value::Null => Expr::Value(SqlValue::Null),
        Value::Boolean(b) => Expr::Value(SqlValue::Boolean(*b)),
        Value::Int32(i) => Expr::Value(SqlValue::Number(i.to_string(), false)),
        Value::Int64(i) => Expr::Value(SqlValue::Number(i.to_string(), false)),
        Value::Float64(f) => Expr::Value(SqlValue::Number(format!("{:E}", f), false)),
        Value::Text(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Bytes(b) => Expr::Value(SqlValue::SingleQuotedString(format!(
            "\\x{}",
            hex::encode(b)
        ))),
        Value::Timestamp(ts) => Expr::Value(SqlValue::Number(ts.to_string(), false)),
        Value::Interval(iv) => Expr::Value(SqlValue::SingleQuotedString(iv.to_string())),
        Value::Uuid(bytes) => {
            let uuid = uuid::Uuid::from_bytes(*bytes);
            Expr::Value(SqlValue::SingleQuotedString(uuid.to_string()))
        }
        Value::Array(elems) => {
            let elem_exprs: Vec<Expr> = elems.iter().map(value_to_sql_expr).collect();
            Expr::Array(sqlparser::ast::Array {
                elem: elem_exprs,
                named: true,
            })
        }
        Value::Vector(vec) => {
            let vec_str = format!(
                "[{}]",
                vec.iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            Expr::Value(SqlValue::SingleQuotedString(vec_str))
        }
        Value::Json(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Jsonb(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Date(days) => {
            let s =
                crate::types::date::format_date_days(*days).unwrap_or_else(|_| days.to_string());
            Expr::Value(SqlValue::SingleQuotedString(s))
        }
        Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            Expr::Value(SqlValue::SingleQuotedString(format!(
                "{:02}:{:02}:{:02}",
                hours, mins, secs
            )))
        }
        Value::Numeric(d) => Expr::Value(SqlValue::Number(d.to_string(), false)),
    }
}

/// Parse a PostgreSQL array literal string into a Vec<Value>
pub fn parse_pg_array(s: &str) -> Result<Vec<Value>> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err(anyhow!("Invalid array format"));
    }

    let inner = &s[1..s.len() - 1];
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escape_next = false;

    for c in inner.chars() {
        if escape_next {
            current.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' => escape_next = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                let val = parse_array_element(&current);
                result.push(val);
                current.clear();
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() || inner.ends_with(',') {
        let val = parse_array_element(&current);
        result.push(val);
    }

    Ok(result)
}

/// Parse a single array element string into a Value
fn parse_array_element(s: &str) -> Value {
    let s = s.trim();
    if s.eq_ignore_ascii_case("NULL") {
        return Value::Null;
    }

    if let Ok(i) = s.parse::<i32>() {
        return Value::Int32(i);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int64(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float64(f);
    }
    if s.eq_ignore_ascii_case("true") {
        return Value::Boolean(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return Value::Boolean(false);
    }

    Value::Text(s.to_string())
}

/// Convert a SQL data type to our internal DataType
pub fn convert_data_type(sql_type: &SqlDataType) -> Result<DataType> {
    match sql_type {
        SqlDataType::Boolean => Ok(DataType::Boolean),
        SqlDataType::SmallInt(_) | SqlDataType::Int(_) | SqlDataType::Integer(_) => {
            Ok(DataType::Int32)
        }
        SqlDataType::BigInt(_) => Ok(DataType::Int64),
        SqlDataType::Float(_)
        | SqlDataType::Double
        | SqlDataType::DoublePrecision
        | SqlDataType::Real => Ok(DataType::Float64),
        SqlDataType::Numeric(info) | SqlDataType::Decimal(info) => {
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
            Ok(DataType::Numeric { precision, scale })
        }
        SqlDataType::Varchar(_)
        | SqlDataType::Text
        | SqlDataType::String(_)
        | SqlDataType::Char(_)
        | SqlDataType::Character(_)
        | SqlDataType::CharacterVarying(_) => Ok(DataType::Text),
        SqlDataType::Bytea => Ok(DataType::Bytes),
        SqlDataType::Timestamp(_, tz) => match tz {
            sqlparser::ast::TimezoneInfo::WithTimeZone | sqlparser::ast::TimezoneInfo::Tz => {
                Ok(DataType::TimestampTz)
            }
            _ => Ok(DataType::Timestamp),
        },
        SqlDataType::Date => Ok(DataType::Date),
        SqlDataType::Time(_, _) => Ok(DataType::Time),
        SqlDataType::Uuid => Ok(DataType::Uuid),
        SqlDataType::JSON => Ok(DataType::Jsonb),
        SqlDataType::Custom(name, modifiers) => {
            if let Some(ident) = name.0.last() {
                let type_name = ident.value.to_uppercase();
                match type_name.as_str() {
                    "SERIAL" => Ok(DataType::Int32),
                    "BIGSERIAL" => Ok(DataType::Int64),
                    "JSON" => Ok(DataType::Json),
                    "JSONB" => Ok(DataType::Jsonb),
                    "TIMESTAMPTZ" => Ok(DataType::TimestampTz),
                    "VECTOR" => {
                        // Extract dimension from type modifiers if available
                        // sqlparser parses vector(3) as Custom type with Vec<String> modifiers
                        let dim = if !modifiers.is_empty() {
                            // Try to parse first modifier as dimension number
                            modifiers[0].parse::<u32>().unwrap_or(1536)
                        } else {
                            1536 // Default dimension (OpenAI embedding size)
                        };
                        Ok(DataType::Vector(dim))
                    }
                    _ => Ok(DataType::Text),
                }
            } else {
                Ok(DataType::Text)
            }
        }
        SqlDataType::Array(inner) => match inner {
            sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner_type) => {
                convert_data_type(inner_type)
            }
            sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner_type) => {
                convert_data_type(inner_type)
            }
            _ => Ok(DataType::Text),
        },
        _ => Err(anyhow!("Unsupported data type: {:?}", sql_type)),
    }
}

/// Extract equality conditions from a WHERE clause for index lookup
#[allow(dead_code)]
pub fn extract_eq_conditions(expr: &Expr, index_cols: &[String]) -> Option<Vec<Value>> {
    let mut values = vec![None; index_cols.len()];
    extract_conditions_recursive(expr, index_cols, &mut values);

    if values.iter().all(|v| v.is_some()) {
        Some(values.into_iter().map(|v| v.unwrap()).collect())
    } else {
        None
    }
}

#[allow(dead_code)]
fn extract_conditions_recursive(
    expr: &Expr,
    index_cols: &[String],
    values: &mut Vec<Option<Value>>,
) {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                extract_conditions_recursive(left, index_cols, values);
                extract_conditions_recursive(right, index_cols, values);
            }
            BinaryOperator::Eq => {
                if let Expr::Identifier(ident) = &**left {
                    if let Some(idx) = index_cols.iter().position(|c| c == &ident.value) {
                        if let Ok(val) = eval_expr(right, None, None) {
                            values[idx] = Some(val);
                        }
                    }
                } else if let Expr::Identifier(ident) = &**right {
                    if let Some(idx) = index_cols.iter().position(|c| c == &ident.value) {
                        if let Ok(val) = eval_expr(left, None, None) {
                            values[idx] = Some(val);
                        }
                    }
                }
            }
            _ => {}
        },
        Expr::Nested(e) => extract_conditions_recursive(e, index_cols, values),
        _ => {}
    }
}

/// Collect aggregate functions from HAVING clause that aren't already in projection
pub fn collect_having_agg_funcs(
    expr: &Expr,
    agg_funcs: &mut Vec<(usize, AggExpr)>,
    extra_start: usize,
) {
    match expr {
        Expr::Function(f) if f.over.is_none() => {
            let func_name = f
                .name
                .0
                .last()
                .map(|i| i.value.to_uppercase())
                .unwrap_or_default();
            if matches!(
                func_name.as_str(),
                "COUNT"
                    | "SUM"
                    | "AVG"
                    | "MIN"
                    | "MAX"
                    | "STRING_AGG"
                    | "ARRAY_AGG"
                    | "BOOL_AND"
                    | "BOOL_OR"
                    | "EVERY"
            ) {
                let already_exists = agg_funcs.iter().any(|(_, existing)| {
                    if let AggExpr::Function(existing_f) = existing {
                        let existing_name = existing_f
                            .name
                            .0
                            .last()
                            .map(|n| n.value.to_uppercase())
                            .unwrap_or_default();
                        existing_name == func_name && args_match(f, existing_f)
                    } else {
                        false
                    }
                });
                if !already_exists {
                    let new_idx = extra_start
                        + (agg_funcs.len()
                            - agg_funcs
                                .iter()
                                .filter(|(idx, _)| *idx < extra_start)
                                .count());
                    agg_funcs.push((new_idx, AggExpr::Function(f.clone())));
                }
            } else {
                for arg in f.args.iter() {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                    ) = arg
                    {
                        collect_having_agg_funcs(arg_expr, agg_funcs, extra_start);
                    }
                }
            }
        }
        Expr::ArrayAgg(arr) => {
            let already_exists = agg_funcs
                .iter()
                .any(|(_, existing)| matches!(existing, AggExpr::ArrayAgg(_)));
            if !already_exists {
                let new_idx = extra_start
                    + (agg_funcs.len()
                        - agg_funcs
                            .iter()
                            .filter(|(idx, _)| *idx < extra_start)
                            .count());
                agg_funcs.push((new_idx, AggExpr::ArrayAgg(arr.clone())));
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_having_agg_funcs(left, agg_funcs, extra_start);
            collect_having_agg_funcs(right, agg_funcs, extra_start);
        }
        Expr::Nested(e) => collect_having_agg_funcs(e, agg_funcs, extra_start),
        Expr::Cast { expr, .. } => collect_having_agg_funcs(expr, agg_funcs, extra_start),
        _ => {}
    }
}

/// Evaluate HAVING clause expression with aggregated values
pub fn eval_having_expr(
    expr: &Expr,
    row: &Row,
    schema: &TableSchema,
    agg_funcs: &[(usize, AggExpr)],
    aggs: &[Aggregator],
) -> Result<Value> {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            let left_val = eval_having_expr(left, row, schema, agg_funcs, aggs)?;
            let right_val = eval_having_expr(right, row, schema, agg_funcs, aggs)?;
            super::expr::eval_binary_op_public(left_val, op, right_val)
        }
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => {
            let inner_val = eval_having_expr(inner, row, schema, agg_funcs, aggs)?;
            let cast_expr = Expr::Cast {
                expr: Box::new(value_to_sql_expr(&inner_val)),
                data_type: data_type.clone(),
                format: format.clone(),
            };
            eval_expr(&cast_expr, Some(row), Some(schema))
        }
        Expr::Function(f) => {
            let func_name = f
                .name
                .0
                .last()
                .map(|i| i.value.to_uppercase())
                .unwrap_or_default();
            for (i, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                if let AggExpr::Function(agg_f) = agg_expr {
                    let agg_name = agg_f
                        .name
                        .0
                        .last()
                        .map(|n| n.value.to_uppercase())
                        .unwrap_or_default();
                    if agg_name == func_name && args_match(f, agg_f) {
                        return Ok(aggs[i].result());
                    }
                }
            }
            if matches!(
                func_name.as_str(),
                "COUNT"
                    | "SUM"
                    | "AVG"
                    | "MIN"
                    | "MAX"
                    | "STRING_AGG"
                    | "ARRAY_AGG"
                    | "BOOL_AND"
                    | "BOOL_OR"
                    | "EVERY"
            ) {
                let mut temp_agg = Aggregator::new(&func_name)?;
                let arg_expr = if f.args.is_empty() {
                    None
                } else {
                    match &f.args[0] {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                        _ => None,
                    }
                };
                if let Some(e) = arg_expr {
                    let val = eval_expr(e, Some(row), Some(schema))?;
                    temp_agg.update(&val)?;
                } else {
                    temp_agg.update(&Value::Int32(1))?;
                }
                Ok(temp_agg.result())
            } else {
                let args: Vec<Value> = f
                    .args
                    .iter()
                    .filter_map(|arg| {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            eval_having_expr(e, row, schema, agg_funcs, aggs).ok()
                        } else {
                            None
                        }
                    })
                    .collect();
                match func_name.as_str() {
                    "COALESCE" => {
                        for val in args {
                            if !matches!(val, Value::Null) {
                                return Ok(val);
                            }
                        }
                        Ok(Value::Null)
                    }
                    "NULLIF" => {
                        if args.len() >= 2
                            && super::expr::compare_values(&args[0], &args[1]).unwrap_or(1) == 0
                        {
                            Ok(Value::Null)
                        } else {
                            Ok(args.into_iter().next().unwrap_or(Value::Null))
                        }
                    }
                    _ => eval_expr(expr, Some(row), Some(schema)),
                }
            }
        }
        Expr::ArrayAgg(arr) => {
            for (i, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                if matches!(agg_expr, AggExpr::ArrayAgg(_)) {
                    return Ok(aggs[i].result());
                }
            }
            let mut temp_agg = Aggregator::new_array_agg();
            let val = eval_expr(&arr.expr, Some(row), Some(schema))?;
            temp_agg.update(&val)?;
            Ok(temp_agg.result())
        }
        Expr::Nested(e) => eval_having_expr(e, row, schema, agg_funcs, aggs),
        Expr::Value(v) => super::expr::eval_value_public(v),
        _ => eval_expr(expr, Some(row), Some(schema)),
    }
}

/// Evaluate HAVING clause expression for join queries
pub fn eval_having_expr_join(
    expr: &Expr,
    ctx: &JoinContext,
    agg_funcs: &[(usize, AggExpr)],
    aggs: &[Aggregator],
) -> Result<Value> {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            let left_val = eval_having_expr_join(left, ctx, agg_funcs, aggs)?;
            let right_val = eval_having_expr_join(right, ctx, agg_funcs, aggs)?;
            super::expr::eval_binary_op_public(left_val, op, right_val)
        }
        Expr::Cast {
            expr: inner,
            data_type,
            ..
        } => {
            let inner_val = eval_having_expr_join(inner, ctx, agg_funcs, aggs)?;
            super::expr::cast_value_public(inner_val, data_type)
        }
        Expr::Function(f) => {
            let func_name = f
                .name
                .0
                .last()
                .map(|i| i.value.to_uppercase())
                .unwrap_or_default();
            for (i, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                if let AggExpr::Function(agg_f) = agg_expr {
                    let agg_name = agg_f
                        .name
                        .0
                        .last()
                        .map(|n| n.value.to_uppercase())
                        .unwrap_or_default();
                    if agg_name == func_name && args_match(f, agg_f) {
                        return Ok(aggs[i].result());
                    }
                }
            }
            if matches!(
                func_name.as_str(),
                "COUNT"
                    | "SUM"
                    | "AVG"
                    | "MIN"
                    | "MAX"
                    | "STRING_AGG"
                    | "ARRAY_AGG"
                    | "BOOL_AND"
                    | "BOOL_OR"
                    | "EVERY"
            ) {
                let mut temp_agg = Aggregator::new(&func_name)?;
                let arg_expr = if f.args.is_empty() {
                    None
                } else {
                    match &f.args[0] {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                        _ => None,
                    }
                };
                if let Some(e) = arg_expr {
                    let val = eval_expr_join(e, ctx)?;
                    temp_agg.update(&val)?;
                } else {
                    temp_agg.update(&Value::Int32(1))?;
                }
                Ok(temp_agg.result())
            } else {
                let args: Vec<Value> = f
                    .args
                    .iter()
                    .filter_map(|arg| {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            eval_having_expr_join(e, ctx, agg_funcs, aggs).ok()
                        } else {
                            None
                        }
                    })
                    .collect();
                match func_name.as_str() {
                    "COALESCE" => {
                        for val in args {
                            if !matches!(val, Value::Null) {
                                return Ok(val);
                            }
                        }
                        Ok(Value::Null)
                    }
                    "NULLIF" => {
                        if args.len() >= 2
                            && super::expr::compare_values(&args[0], &args[1]).unwrap_or(1) == 0
                        {
                            Ok(Value::Null)
                        } else {
                            Ok(args.into_iter().next().unwrap_or(Value::Null))
                        }
                    }
                    _ => eval_expr_join(expr, ctx),
                }
            }
        }
        Expr::ArrayAgg(arr) => {
            for (i, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                if matches!(agg_expr, AggExpr::ArrayAgg(_)) {
                    return Ok(aggs[i].result());
                }
            }
            let mut temp_agg = Aggregator::new_array_agg();
            let val = eval_expr_join(&arr.expr, ctx)?;
            temp_agg.update(&val)?;
            Ok(temp_agg.result())
        }
        Expr::Nested(e) => eval_having_expr_join(e, ctx, agg_funcs, aggs),
        Expr::Value(v) => super::expr::eval_value_public(v),
        _ => eval_expr_join(expr, ctx),
    }
}

/// Check if two function arguments match
pub fn args_match(f1: &sqlparser::ast::Function, f2: &sqlparser::ast::Function) -> bool {
    if f1.args.len() != f2.args.len() {
        return false;
    }
    for (a1, a2) in f1.args.iter().zip(f2.args.iter()) {
        match (a1, a2) {
            (
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard),
            ) => {}
            (
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e1)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e2)),
            ) => {
                if format!("{:?}", e1) != format!("{:?}", e2) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Infer the DataType from a Value
pub fn infer_data_type(value: &Value) -> DataType {
    match value {
        Value::Int32(_) => DataType::Int32,
        Value::Int64(_) => DataType::Int64,
        Value::Float64(_) => DataType::Float64,
        Value::Boolean(_) => DataType::Boolean,
        Value::Text(_) => DataType::Text,
        Value::Bytes(_) => DataType::Bytes,
        Value::Timestamp(_) => DataType::Timestamp,
        Value::Interval { .. } => DataType::Interval,
        Value::Time(_) => DataType::Time,
        Value::Date(_) => DataType::Date,
        Value::Uuid(_) => DataType::Uuid,
        Value::Vector(vec) => DataType::Vector(vec.len() as u32),
        Value::Json(_) => DataType::Json,
        Value::Jsonb(_) => DataType::Jsonb,
        Value::Array(_) => DataType::Text,
        Value::Null => DataType::Text,
        Value::Numeric(_) => DataType::Numeric {
            precision: None,
            scale: None,
        },
    }
}

/// Check if a SQL statement should be skipped
pub fn get_skip_reason(sql_upper: &str) -> Option<String> {
    if sql_upper.starts_with("DROP DATABASE") {
        return Some("DROP DATABASE not supported".into());
    }
    if sql_upper.starts_with("CREATE DATABASE") {
        return Some("CREATE DATABASE not supported".into());
    }
    if sql_upper.starts_with("ALTER DATABASE") {
        return Some("ALTER DATABASE not supported".into());
    }
    if sql_upper.starts_with("\\") {
        return Some("psql meta-command not supported".into());
    }
    if sql_upper.starts_with("COPY ") || sql_upper.contains(" FROM STDIN") {
        return Some("COPY not supported".into());
    }
    None
}

/// Check if a SQL statement is unsupported
pub fn get_unsupported_reason(sql_upper: &str) -> Option<String> {
    if sql_upper.starts_with("CREATE DOMAIN") {
        return Some("CREATE DOMAIN not supported".into());
    }
    if sql_upper.starts_with("CREATE AGGREGATE") {
        return Some("CREATE AGGREGATE not supported".into());
    }
    if sql_upper.starts_with("ALTER TYPE") {
        return Some("ALTER TYPE not supported".into());
    }
    if sql_upper.starts_with("ALTER DOMAIN") {
        return Some("ALTER DOMAIN not supported".into());
    }
    if sql_upper.starts_with("ALTER AGGREGATE") {
        return Some("ALTER AGGREGATE not supported".into());
    }
    if sql_upper.starts_with("ALTER FUNCTION") {
        if sql_upper.contains(" OWNER TO ") {
            return None;
        }
        return Some("ALTER FUNCTION not supported".into());
    }
    if sql_upper.starts_with("ALTER SEQUENCE") {
        if sql_upper.contains(" OWNER TO ") || sql_upper.contains(" OWNED BY ") {
            return None;
        }
        return Some("ALTER SEQUENCE not supported".into());
    }
    None
}

pub fn get_select_item_name(item: &sqlparser::ast::SelectItem) -> String {
    match item {
        sqlparser::ast::SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        sqlparser::ast::SelectItem::UnnamedExpr(expr) => get_expr_name(expr),
        sqlparser::ast::SelectItem::Wildcard(_) => "*".to_string(),
        _ => "?column?".to_string(),
    }
}

/// Get the name of an expression for column naming
pub fn get_expr_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| p.value.clone())
            .unwrap_or_else(|| "?column?".to_string()),
        Expr::Function(f) => {
            if let Some(last_ident) = f.name.0.last() {
                last_ident.value.to_lowercase()
            } else {
                "?column?".to_string()
            }
        }
        Expr::Case { .. } => "case".to_string(),
        Expr::Cast { data_type, .. } => {
            use sqlparser::ast::DataType;
            match data_type {
                DataType::Int(_) | DataType::Integer(_) => "int4".to_string(),
                DataType::BigInt(_) => "int8".to_string(),
                DataType::SmallInt(_) => "int2".to_string(),
                DataType::Text => "text".to_string(),
                DataType::Varchar(_) | DataType::CharVarying(_) => "varchar".to_string(),
                DataType::Boolean => "bool".to_string(),
                DataType::Float(_) | DataType::Real => "float4".to_string(),
                DataType::Double | DataType::DoublePrecision => "float8".to_string(),
                DataType::Numeric(_) | DataType::Decimal(_) => "numeric".to_string(),
                DataType::Timestamp(_, _) => "timestamp".to_string(),
                DataType::Date => "date".to_string(),
                DataType::Uuid => "uuid".to_string(),
                DataType::JSON => "json".to_string(),
                _ => data_type.to_string().to_lowercase(),
            }
        }
        Expr::Substring { .. } => "substring".to_string(),
        Expr::Trim { .. } => "btrim".to_string(),
        Expr::Position { .. } => "position".to_string(),
        Expr::Extract { .. } => "extract".to_string(),
        Expr::Subquery(_) => "subquery".to_string(),
        Expr::Nested(inner) => get_expr_name(inner),
        _ => "?column?".to_string(),
    }
}

/// Fill default values for missing columns in a row
pub fn fill_row_defaults(row: &mut Row, schema: &TableSchema) -> Result<()> {
    if row.values.len() < schema.columns.len() {
        for i in row.values.len()..schema.columns.len() {
            let col = &schema.columns[i];
            let val = if let Some(expr_str) = &col.default_expr {
                eval_default_expr(expr_str)?
            } else {
                Value::Null
            };
            row.values.push(val);
        }
    }
    Ok(())
}

/// Evaluate a default expression string
pub fn eval_default_expr(expr_str: &str) -> Result<Value> {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let sql = format!("SELECT {}", expr_str);
    let dialect = PostgreSqlDialect {};
    let ast = Parser::parse_sql(&dialect, &sql)
        .map_err(|e| anyhow!("Failed to parse default expr: {}", e))?;

    if let Some(sqlparser::ast::Statement::Query(q)) = ast.into_iter().next() {
        if let sqlparser::ast::SetExpr::Select(s) = *q.body {
            if let Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) =
                s.projection.into_iter().next()
            {
                return eval_expr(&e, None, None);
            }
        }
    }
    Ok(Value::Text(expr_str.to_string()))
}

pub fn infer_expr_type(expr: &Expr, schema: &TableSchema) -> DataType {
    match expr {
        Expr::Identifier(ident) => {
            let col_name = normalize_ident(ident);
            if let Some(col) = schema
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&col_name))
            {
                return col.data_type.clone();
            }

            let suffix = format!(".{col_name}");
            let mut matched: Option<&ColumnDef> = None;
            for col in &schema.columns {
                if col.name.len() >= suffix.len()
                    && col.name[col.name.len() - suffix.len()..].eq_ignore_ascii_case(&suffix)
                {
                    if matched.is_some() {
                        matched = None;
                        break;
                    }
                    matched = Some(col);
                }
            }
            matched
                .map(|c| c.data_type.clone())
                .unwrap_or(DataType::Text)
        }
        Expr::CompoundIdentifier(parts) => {
            if parts.is_empty() {
                return DataType::Text;
            }

            let mut full_name = String::new();
            for (idx, part) in parts.iter().enumerate() {
                if idx > 0 {
                    full_name.push('.');
                }
                full_name.push_str(&normalize_ident(part));
            }
            if let Some(col) = schema
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&full_name))
            {
                return col.data_type.clone();
            }

            if let Some(last) = parts.last() {
                let col_name = normalize_ident(last);
                if let Some(col) = schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&col_name))
                {
                    return col.data_type.clone();
                }
                let suffix = format!(".{col_name}");
                let mut matched: Option<&ColumnDef> = None;
                for col in &schema.columns {
                    if col.name.len() >= suffix.len()
                        && col.name[col.name.len() - suffix.len()..].eq_ignore_ascii_case(&suffix)
                    {
                        if matched.is_some() {
                            matched = None;
                            break;
                        }
                        matched = Some(col);
                    }
                }
                matched
                    .map(|c| c.data_type.clone())
                    .unwrap_or(DataType::Text)
            } else {
                DataType::Text
            }
        }
        Expr::Cast { data_type, .. } => sql_datatype_to_internal(data_type),
        Expr::TypedString { data_type, .. } => sql_datatype_to_internal(data_type),
        Expr::AtTimeZone { timestamp, .. } => match infer_expr_type(timestamp, schema) {
            DataType::TimestampTz => DataType::Timestamp,
            _ => DataType::TimestampTz,
        },
        Expr::Interval(_) => DataType::Interval,
        Expr::Extract { .. } => DataType::Float64,
        Expr::JsonAccess { operator, .. } => match operator {
            sqlparser::ast::JsonOperator::Arrow => DataType::Jsonb,
            sqlparser::ast::JsonOperator::LongArrow => DataType::Text,
            sqlparser::ast::JsonOperator::HashArrow => DataType::Jsonb,
            sqlparser::ast::JsonOperator::HashLongArrow => DataType::Text,
            sqlparser::ast::JsonOperator::HashMinus => DataType::Jsonb,
            sqlparser::ast::JsonOperator::AtArrow | sqlparser::ast::JsonOperator::ArrowAt => {
                DataType::Boolean
            }
            _ => DataType::Text,
        },
        Expr::Substring { expr, .. } => match infer_expr_type(expr, schema) {
            DataType::Bytes => DataType::Bytes,
            _ => DataType::Text,
        },
        Expr::Overlay { expr, .. } => match infer_expr_type(expr, schema) {
            DataType::Bytes => DataType::Bytes,
            _ => DataType::Text,
        },
        Expr::Function(f) => {
            let func_name = f
                .name
                .0
                .last()
                .map(|n| n.value.to_uppercase())
                .unwrap_or_default();
            match func_name.as_str() {
                "COUNT" => DataType::Int64,
                "SUM" => {
                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) =
                        f.args.first()
                    {
                        match infer_expr_type(arg_expr, schema) {
                            DataType::Int32 => DataType::Int64,
                            DataType::Int64 => DataType::Numeric {
                                precision: None,
                                scale: None,
                            },
                            DataType::Numeric { .. } => DataType::Numeric {
                                precision: None,
                                scale: None,
                            },
                            DataType::Float64 => DataType::Float64,
                            _ => DataType::Text,
                        }
                    } else {
                        DataType::Text
                    }
                }
                "AVG" => {
                    let arg_type =
                        if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) =
                            f.args.first()
                        {
                            infer_expr_type(arg_expr, schema)
                        } else {
                            DataType::Text
                        };
                    match arg_type {
                        DataType::Float64 => DataType::Float64,
                        DataType::Numeric { .. } | DataType::Int32 | DataType::Int64 => {
                            DataType::Numeric {
                                precision: None,
                                scale: None,
                            }
                        }
                        _ => DataType::Float64,
                    }
                }
                "MIN" | "MAX" => {
                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) =
                        f.args.first()
                    {
                        infer_expr_type(arg_expr, schema)
                    } else {
                        DataType::Text
                    }
                }
                "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" => DataType::Int64,
                "BOOL_AND" | "BOOL_OR" | "EVERY" => DataType::Boolean,
                "GROUPING" => DataType::Int32,
                "VECTOR_DIMS" => DataType::Int32,
                "LAG" | "LEAD" | "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE" => {
                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) =
                        f.args.first()
                    {
                        infer_expr_type(arg_expr, schema)
                    } else {
                        DataType::Text
                    }
                }
                "NOW" | "CURRENT_TIMESTAMP" => DataType::TimestampTz,
                "CURRENT_DATE" => DataType::Date,
                "DATE" => DataType::Date,
                "NEXTVAL" | "CURRVAL" | "SETVAL" => DataType::Int64,
                "GEN_RANDOM_UUID" | "UUID_GENERATE_V4" => DataType::Uuid,
                "JSONB_BUILD_OBJECT" | "JSONB_BUILD_ARRAY" | "JSONB_SET" | "JSONB_AGG"
                | "TO_JSONB" => DataType::Jsonb,
                "JSON_BUILD_OBJECT" | "JSON_BUILD_ARRAY" | "JSON_SET" | "JSON_AGG" | "TO_JSON" => {
                    DataType::Json
                }
                "JSONB_ARRAY_LENGTH" | "JSON_ARRAY_LENGTH" => DataType::Int32,
                "JSONB_TYPEOF" | "JSON_TYPEOF" => DataType::Text,
                "JSONB_EXISTS" | "JSONB_EXISTS_ANY" | "JSONB_EXISTS_ALL" => DataType::Boolean,
                "ROW_TO_JSON" => DataType::Json,
                "DECODE" => DataType::Bytes,
                "GET_BIT" => DataType::Int32,
                "SET_BIT" | "INT8SEND" | "INT4SEND" | "UUID_SEND" => DataType::Bytes,
                "BIT_LENGTH" | "OCTET_LENGTH" => DataType::Int32,
                "COALESCE" | "NULLIF" | "GREATEST" | "LEAST" => {
                    if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) =
                        f.args.first()
                    {
                        infer_expr_type(arg_expr, schema)
                    } else {
                        DataType::Text
                    }
                }
                _ => DataType::Text,
            }
        }
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::Plus | BinaryOperator::Minus => {
                let left_type = infer_expr_type(left, schema);
                let right_type = infer_expr_type(right, schema);
                match (op, &left_type, &right_type) {
                    (BinaryOperator::Plus, DataType::Timestamp, DataType::Interval)
                    | (BinaryOperator::Plus, DataType::Interval, DataType::Timestamp) => {
                        DataType::Timestamp
                    }
                    (BinaryOperator::Plus, DataType::Date, DataType::Interval)
                    | (BinaryOperator::Plus, DataType::Interval, DataType::Date) => {
                        DataType::Timestamp
                    }
                    (BinaryOperator::Plus, DataType::Interval, DataType::Interval) => {
                        DataType::Interval
                    }
                    (BinaryOperator::Minus, DataType::Timestamp, DataType::Timestamp) => {
                        DataType::Interval
                    }
                    (BinaryOperator::Minus, DataType::Timestamp, DataType::Interval) => {
                        DataType::Timestamp
                    }
                    (BinaryOperator::Minus, DataType::Interval, DataType::Interval) => {
                        DataType::Interval
                    }
                    (BinaryOperator::Minus, DataType::Date, DataType::Interval) => {
                        DataType::Timestamp
                    }
                    (BinaryOperator::Minus, DataType::Date, DataType::Date) => DataType::Int32,
                    _ => {
                        let left_numeric = matches!(
                            left_type,
                            DataType::Int32
                                | DataType::Int64
                                | DataType::Float64
                                | DataType::Numeric { .. }
                        );
                        let right_numeric = matches!(
                            right_type,
                            DataType::Int32
                                | DataType::Int64
                                | DataType::Float64
                                | DataType::Numeric { .. }
                        );
                        if left_numeric && right_numeric {
                            if matches!(left_type, DataType::Float64)
                                || matches!(right_type, DataType::Float64)
                            {
                                DataType::Float64
                            } else if matches!(left_type, DataType::Numeric { .. })
                                || matches!(right_type, DataType::Numeric { .. })
                            {
                                DataType::Numeric {
                                    precision: None,
                                    scale: None,
                                }
                            } else if matches!(left_type, DataType::Int64)
                                || matches!(right_type, DataType::Int64)
                            {
                                DataType::Int64
                            } else {
                                DataType::Int32
                            }
                        } else {
                            DataType::Text
                        }
                    }
                }
            }
            BinaryOperator::Multiply | BinaryOperator::Divide | BinaryOperator::Modulo => {
                let left_type = infer_expr_type(left, schema);
                let right_type = infer_expr_type(right, schema);
                if matches!(left_type, DataType::Float64) || matches!(right_type, DataType::Float64)
                {
                    DataType::Float64
                } else if matches!(left_type, DataType::Numeric { .. })
                    || matches!(right_type, DataType::Numeric { .. })
                {
                    DataType::Numeric {
                        precision: None,
                        scale: None,
                    }
                } else if matches!(left_type, DataType::Int64)
                    || matches!(right_type, DataType::Int64)
                {
                    DataType::Int64
                } else {
                    DataType::Int32
                }
            }
            BinaryOperator::And
            | BinaryOperator::Or
            | BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq => DataType::Boolean,
            BinaryOperator::StringConcat => DataType::Text,
            _ => DataType::Text,
        },
        Expr::UnaryOp { op, .. } => match op {
            sqlparser::ast::UnaryOperator::Not => DataType::Boolean,
            sqlparser::ast::UnaryOperator::Minus | sqlparser::ast::UnaryOperator::Plus => {
                let inner = match expr {
                    Expr::UnaryOp { expr: inner, .. } => inner.as_ref(),
                    _ => expr,
                };
                match infer_expr_type(inner, schema) {
                    DataType::Int32 => DataType::Int32,
                    DataType::Int64 => DataType::Int64,
                    DataType::Numeric { .. } => DataType::Numeric {
                        precision: None,
                        scale: None,
                    },
                    _ => DataType::Float64,
                }
            }
            _ => DataType::Text,
        },
        Expr::Value(val) => match val {
            SqlValue::Number(n, _) => {
                if n.contains(['e', 'E']) {
                    DataType::Float64
                } else if n.contains('.') {
                    DataType::Numeric {
                        precision: None,
                        scale: None,
                    }
                } else if n.parse::<i32>().is_ok() {
                    DataType::Int32
                } else {
                    DataType::Int64
                }
            }
            SqlValue::SingleQuotedString(_)
            | SqlValue::DoubleQuotedString(_)
            | SqlValue::EscapedStringLiteral(_) => DataType::Text,
            SqlValue::Boolean(_) => DataType::Boolean,
            SqlValue::Null => DataType::Text,
            _ => DataType::Text,
        },
        Expr::Case { .. } => DataType::Text,
        Expr::Nested(inner) => infer_expr_type(inner, schema),
        _ => DataType::Text,
    }
}

fn sql_datatype_to_internal(dt: &SqlDataType) -> DataType {
    match dt {
        SqlDataType::Boolean | SqlDataType::Bool => DataType::Boolean,
        SqlDataType::SmallInt(_) | SqlDataType::Int2(_) => DataType::Int32,
        SqlDataType::Int(_)
        | SqlDataType::Integer(_)
        | SqlDataType::Int4(_)
        | SqlDataType::MediumInt(_) => DataType::Int32,
        SqlDataType::BigInt(_) | SqlDataType::Int8(_) => DataType::Int64,
        SqlDataType::Real | SqlDataType::Float4 => DataType::Float64,
        SqlDataType::Double
        | SqlDataType::DoublePrecision
        | SqlDataType::Float8
        | SqlDataType::Float(_) => DataType::Float64,
        SqlDataType::Numeric(info) | SqlDataType::Decimal(info) => {
            let (precision, scale) = match info {
                sqlparser::ast::ExactNumberInfo::None => (None, None),
                sqlparser::ast::ExactNumberInfo::Precision(p) => (Some(*p as u32), Some(0)),
                sqlparser::ast::ExactNumberInfo::PrecisionAndScale(p, s) => {
                    (Some(*p as u32), Some(*s as u32))
                }
            };
            DataType::Numeric { precision, scale }
        }
        SqlDataType::Timestamp(_, tz) => match tz {
            sqlparser::ast::TimezoneInfo::WithTimeZone | sqlparser::ast::TimezoneInfo::Tz => {
                DataType::TimestampTz
            }
            _ => DataType::Timestamp,
        },
        SqlDataType::Date => DataType::Date,
        SqlDataType::Time(_, _) => DataType::Time,
        SqlDataType::Interval => DataType::Interval,
        SqlDataType::Uuid => DataType::Uuid,
        SqlDataType::Bytea => DataType::Bytes,
        SqlDataType::JSON => DataType::Jsonb,
        SqlDataType::Custom(name, _) => {
            if let Some(ident) = name.0.last() {
                let type_name = ident.value.to_uppercase();
                match type_name.as_str() {
                    "JSONB" => DataType::Jsonb,
                    "JSON" => DataType::Json,
                    "TIMESTAMPTZ" => DataType::TimestampTz,
                    _ => DataType::Text,
                }
            } else {
                DataType::Text
            }
        }
        _ => DataType::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Value as SqlValue;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashMap;

    #[test]
    fn test_version_function_name() {
        let dialect = PostgreSqlDialect {};
        let sql = "SELECT version()";
        let statements = Parser::parse_sql(&dialect, sql).unwrap();

        if let sqlparser::ast::Statement::Query(query) = &statements[0] {
            if let sqlparser::ast::SetExpr::Select(select) = &*query.body {
                let item = &select.projection[0];
                let name = get_select_item_name(item);
                assert_eq!(
                    name, "version",
                    "Function name should be 'version', got '{}'",
                    name
                );
            }
        }
    }

    #[test]
    fn test_dedup_rows() {
        let rows = vec![
            Row {
                values: vec![Value::Int32(1), Value::Text("a".to_string())],
            },
            Row {
                values: vec![Value::Int32(1), Value::Text("a".to_string())],
            },
            Row {
                values: vec![Value::Int32(2), Value::Text("b".to_string())],
            },
        ];
        let result = dedup_rows(rows);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_parse_pg_array() {
        let result = parse_pg_array("{1,2,3}").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], Value::Int32(1));
        assert_eq!(result[1], Value::Int32(2));
        assert_eq!(result[2], Value::Int32(3));
    }

    #[test]
    fn test_parse_pg_array_empty() {
        let result = parse_pg_array("{}").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_infer_expr_type_compound_identifier_prefers_full_name() {
        let schema = TableSchema {
            name: "joined".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "o.total".to_string(),
                data_type: DataType::Float64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT o.total").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        assert_eq!(infer_expr_type(expr, &schema), DataType::Float64);
    }

    #[test]
    fn test_infer_expr_type_coalesce_sum_float() {
        let schema = TableSchema {
            name: "joined".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "o.total".to_string(),
                data_type: DataType::Float64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT COALESCE(SUM(o.total), 0)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        assert_eq!(infer_expr_type(expr, &schema), DataType::Float64);
    }

    #[test]
    fn test_infer_expr_type_sum_int32_returns_int64() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT SUM(x)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        assert_eq!(infer_expr_type(expr, &schema), DataType::Int64);
    }

    #[test]
    fn test_infer_expr_type_sum_int64_returns_numeric() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT SUM(x)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        assert_eq!(
            infer_expr_type(expr, &schema),
            DataType::Numeric {
                precision: None,
                scale: None
            }
        );
    }

    #[test]
    fn test_infer_expr_type_grouping_returns_int32() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT GROUPING(x)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        assert_eq!(infer_expr_type(expr, &schema), DataType::Int32);
    }

    #[test]
    fn test_infer_expr_type_at_time_zone_timestamp_to_timestamptz() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(
            &dialect,
            "SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC'",
        )
        .unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        assert_eq!(infer_expr_type(expr, &schema), DataType::TimestampTz);
    }

    #[test]
    fn test_infer_expr_type_at_time_zone_chain_returns_timestamp() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(
            &dialect,
            "SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'",
        )
        .unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        assert_eq!(infer_expr_type(expr, &schema), DataType::Timestamp);
    }

    #[test]
    fn test_infer_expr_type_at_time_zone_now_returns_timestamp() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements =
            Parser::parse_sql(&dialect, "SELECT NOW() AT TIME ZONE 'America/New_York'").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        assert_eq!(infer_expr_type(expr, &schema), DataType::Timestamp);
    }

    #[test]
    fn test_parse_pg_array_strings() {
        let result = parse_pg_array("{hello,world}").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], Value::Text("hello".to_string()));
        assert_eq!(result[1], Value::Text("world".to_string()));
    }

    #[test]
    fn test_infer_expr_type_json_access_contains_returns_boolean() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements =
            Parser::parse_sql(&dialect, "SELECT '{\"a\":1}'::jsonb @> '{\"a\":1}'::jsonb").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        assert_eq!(infer_expr_type(expr, &schema), DataType::Boolean);
    }

    #[test]
    fn test_infer_data_type() {
        assert_eq!(infer_data_type(&Value::Int32(1)), DataType::Int32);
        assert_eq!(infer_data_type(&Value::Int64(1)), DataType::Int64);
        assert_eq!(infer_data_type(&Value::Float64(1.0)), DataType::Float64);
        assert_eq!(infer_data_type(&Value::Boolean(true)), DataType::Boolean);
        assert_eq!(
            infer_data_type(&Value::Text("".to_string())),
            DataType::Text
        );
    }

    #[test]
    fn test_get_skip_reason() {
        assert!(get_skip_reason("DROP DATABASE test").is_some());
        assert!(get_skip_reason("CREATE DATABASE test").is_some());
        assert!(get_skip_reason("SELECT * FROM foo").is_none());
    }

    #[test]
    fn test_get_unsupported_reason() {
        assert!(get_unsupported_reason("CREATE DOMAIN foo").is_some());
        assert!(get_unsupported_reason("SELECT * FROM foo").is_none());
        assert!(get_unsupported_reason("SELECT $$abc$$").is_none());
        assert!(get_unsupported_reason("SELECT $tag$abc$tag$").is_none());
    }

    #[test]
    fn test_distinct_on_rows_with_indices() {
        let schema = TableSchema::new(
            "t".to_string(),
            1,
            vec![
                ColumnDef {
                    name: "a".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "b".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            vec![],
        );
        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Text("x".to_string())]),
            Row::new(vec![Value::Int32(1), Value::Text("y".to_string())]),
            Row::new(vec![Value::Int32(2), Value::Text("z".to_string())]),
        ];

        let on_exprs = vec![sqlparser::ast::Expr::Identifier(
            sqlparser::ast::Ident::new("a"),
        )];
        let (result, indices) = distinct_on_rows_with_indices(rows, &on_exprs, Some(&schema));
        assert_eq!(indices, vec![0, 2]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].values[1], Value::Text("x".to_string()));
        assert_eq!(result[1].values[1], Value::Text("z".to_string()));
    }

    #[test]
    fn test_distinct_on_rows_join_with_indices() {
        let schema = TableSchema::new(
            "t".to_string(),
            1,
            vec![ColumnDef {
                name: "a".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![],
        );
        let mut offsets = HashMap::new();
        offsets.insert("a".to_string(), 0);

        let rows = vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
        ];
        let on_exprs = vec![sqlparser::ast::Expr::Identifier(
            sqlparser::ast::Ident::new("a"),
        )];
        let (result, indices) =
            distinct_on_rows_join_with_indices(rows, &on_exprs, &offsets, &schema);
        assert_eq!(indices, vec![0, 2]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_apply_offset_limit_fetch() {
        let dialect = PostgreSqlDialect {};
        let sql = "SELECT 1 LIMIT 2 OFFSET 1";
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let query = match &statements[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("expected query"),
        };

        let rows = vec![
            Row::new(vec![Value::Int32(10)]),
            Row::new(vec![Value::Int32(11)]),
            Row::new(vec![Value::Int32(12)]),
        ];
        let result = apply_offset_limit_fetch(rows, query);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].values[0], Value::Int32(11));
        assert_eq!(result[1].values[0], Value::Int32(12));
    }

    fn infer_first_expr(sql: &str) -> DataType {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        infer_expr_type(expr, &TableSchema::default())
    }

    #[test]
    fn test_infer_expr_type_bytea_builtins() {
        assert_eq!(
            infer_first_expr("SELECT int8send(0::bigint)"),
            DataType::Bytes
        );
        assert_eq!(
            infer_first_expr(r"SELECT get_bit(E'\\x80'::bytea, 0)"),
            DataType::Int32
        );
        assert_eq!(
            infer_first_expr(r"SELECT set_bit(E'\\x00'::bytea, 0, 1)"),
            DataType::Bytes
        );
        assert_eq!(
            infer_first_expr(r"SELECT substring(E'\\x0102030405060708'::bytea from 3)"),
            DataType::Bytes
        );
        assert_eq!(
            infer_first_expr(
                r"SELECT overlay('\x00001122'::bytea placing '\xaabb'::bytea from 2 for 2)"
            ),
            DataType::Bytes
        );
    }

    #[test]
    fn test_substitute_join_context_values_only_substitutes_qualified() {
        let mut column_offsets: HashMap<String, usize> = HashMap::new();
        column_offsets.insert("id".to_string(), 0);
        column_offsets.insert("o.id".to_string(), 0);

        let combined_row = Row {
            values: vec![Value::Int32(7)],
        };

        let expr = Expr::Identifier(Ident::new("id"));
        let out = substitute_join_context_values(&expr, &column_offsets, &combined_row);
        assert!(matches!(out, Expr::Identifier(_)));

        let expr = Expr::CompoundIdentifier(vec![Ident::new("o"), Ident::new("id")]);
        let out = substitute_join_context_values(&expr, &column_offsets, &combined_row);
        assert!(matches!(
            out,
            Expr::Value(SqlValue::Number(ref n, _)) if n == "7"
        ));
    }

    #[test]
    fn test_substitute_join_context_values_recurses_into_cast() {
        let mut column_offsets: HashMap<String, usize> = HashMap::new();
        column_offsets.insert("pg_attribute.attrelid".to_string(), 0);

        let combined_row = Row {
            values: vec![Value::Int32(42)],
        };

        let expr = Expr::Cast {
            expr: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("pg_catalog"),
                Ident::new("pg_attribute"),
                Ident::new("attrelid"),
            ])),
            data_type: sqlparser::ast::DataType::Regclass,
            format: None,
        };

        let out = substitute_join_context_values(&expr, &column_offsets, &combined_row);
        match out {
            Expr::Cast { expr: inner, .. } => assert!(matches!(
                *inner,
                Expr::Value(SqlValue::Number(ref n, _)) if n == "42"
            )),
            other => panic!("expected cast expression, got {other:?}"),
        }
    }

    #[test]
    fn test_expr_has_outer_reference_schema_qualified_compound_identifier() {
        let expr = Expr::CompoundIdentifier(vec![
            Ident::new("pg_catalog"),
            Ident::new("pg_attribute"),
            Ident::new("attrelid"),
        ]);
        assert!(expr_has_outer_reference(&expr, "pg_attribute"));
        assert!(!expr_has_outer_reference(&expr, "pg_attrdef"));
    }

    #[test]
    fn test_substitute_outer_values_schema_qualified_compound_identifier() {
        let outer_schema = TableSchema {
            name: "o".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };
        let outer_row = Row::new(vec![Value::Int32(9)]);

        let expr = Expr::CompoundIdentifier(vec![
            Ident::new("pg_catalog"),
            Ident::new("o"),
            Ident::new("id"),
        ]);
        let out = substitute_outer_values(&expr, "o", &outer_schema, &outer_row);
        assert!(matches!(
            out,
            Expr::Value(SqlValue::Number(ref n, _)) if n == "9"
        ));
    }
}

pub fn parse_value_for_copy(val: &str, data_type: &DataType) -> Value {
    let unescaped = val
        .replace("\\t", "\t")
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\\\", "\\");

    match data_type {
        DataType::Boolean => match unescaped.to_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Value::Boolean(true),
            "f" | "false" | "0" | "no" | "off" => Value::Boolean(false),
            _ => Value::Text(unescaped),
        },
        DataType::Int32 => unescaped
            .parse::<i32>()
            .map(Value::Int32)
            .unwrap_or(Value::Text(unescaped)),
        DataType::Int64 => unescaped
            .parse::<i64>()
            .map(Value::Int64)
            .unwrap_or(Value::Text(unescaped)),
        DataType::Float64 => unescaped
            .parse::<f64>()
            .map(Value::Float64)
            .unwrap_or(Value::Text(unescaped)),
        DataType::Timestamp | DataType::TimestampTz => {
            if let Ok(ts) =
                chrono::NaiveDateTime::parse_from_str(&unescaped, "%Y-%m-%d %H:%M:%S%.f")
            {
                Value::Timestamp(ts.and_utc().timestamp_millis())
            } else if let Ok(ts) =
                chrono::NaiveDateTime::parse_from_str(&unescaped, "%Y-%m-%d %H:%M:%S")
            {
                Value::Timestamp(ts.and_utc().timestamp_millis())
            } else if let Ok(d) = chrono::NaiveDate::parse_from_str(&unescaped, "%Y-%m-%d") {
                Value::Timestamp(d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Date => crate::types::date::parse_date_days(&unescaped)
            .map(Value::Date)
            .unwrap_or(Value::Text(unescaped)),
        DataType::Uuid => {
            if let Ok(u) = uuid::Uuid::parse_str(&unescaped) {
                Value::Uuid(*u.as_bytes())
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Bytes => {
            if unescaped.starts_with("\\x") {
                hex::decode(&unescaped[2..])
                    .map(Value::Bytes)
                    .unwrap_or(Value::Bytes(unescaped.into_bytes()))
            } else {
                Value::Bytes(unescaped.into_bytes())
            }
        }
        DataType::Time => {
            if let Some(micros) = parse_time_string(&unescaped) {
                Value::Time(micros)
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Text | DataType::Interval | DataType::UserDefined(_) => Value::Text(unescaped),
        DataType::Array(_) => {
            if let Ok(arr) = parse_pg_array(&unescaped) {
                Value::Array(arr)
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Json => {
            if serde_json::from_str::<serde_json::Value>(&unescaped).is_ok() {
                Value::Json(unescaped)
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Jsonb => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&unescaped) {
                Value::Jsonb(parsed.to_string())
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Vector(_) => {
            if unescaped.starts_with('[') && unescaped.ends_with(']') {
                let inner = &unescaped[1..unescaped.len() - 1];
                let elements: Result<Vec<f64>, _> =
                    inner.split(',').map(|s| s.trim().parse::<f64>()).collect();
                if let Ok(vec) = elements {
                    Value::Vector(vec)
                } else {
                    Value::Text(unescaped)
                }
            } else {
                Value::Text(unescaped)
            }
        }
        DataType::Numeric { scale, .. } => {
            if let Ok(mut d) = Decimal::from_str(&unescaped) {
                if let Some(s) = scale {
                    d.rescale(*s);
                }
                Value::Numeric(d)
            } else {
                Value::Text(unescaped)
            }
        }
    }
}

pub fn parse_time_string(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }

    let hours: i64 = parts[0].parse().ok()?;
    let minutes: i64 = parts[1].parse().ok()?;

    let (seconds, micros) = if parts.len() == 3 {
        if let Some(dot_pos) = parts[2].find('.') {
            let secs: i64 = parts[2][..dot_pos].parse().ok()?;
            let frac_str = &parts[2][dot_pos + 1..];
            let padded = format!("{:0<6}", frac_str);
            let micros: i64 = padded[..6].parse().ok()?;
            (secs, micros)
        } else {
            let secs: i64 = parts[2].parse().ok()?;
            (secs, 0)
        }
    } else {
        (0, 0)
    };

    if hours < 0 || hours > 23 || minutes < 0 || minutes > 59 || seconds < 0 || seconds > 59 {
        return None;
    }

    Some(hours * 3_600_000_000 + minutes * 60_000_000 + seconds * 1_000_000 + micros)
}

pub fn cte_is_recursive(query: &Query, cte_name: &str) -> bool {
    if let SetExpr::SetOperation { left, right, .. } = &*query.body {
        set_expr_references_table(right, cte_name) || set_expr_references_table(left, cte_name)
    } else {
        false
    }
}

pub fn set_expr_references_table(expr: &SetExpr, table_name: &str) -> bool {
    match expr {
        SetExpr::Select(select) => {
            for from in &select.from {
                if table_factor_references(&from.relation, table_name) {
                    return true;
                }
                for join in &from.joins {
                    if table_factor_references(&join.relation, table_name) {
                        return true;
                    }
                }
            }
            false
        }
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_references_table(left, table_name)
                || set_expr_references_table(right, table_name)
        }
        SetExpr::Query(q) => set_expr_references_table(&q.body, table_name),
        _ => false,
    }
}

pub fn table_factor_references(factor: &TableFactor, table_name: &str) -> bool {
    match factor {
        TableFactor::Table { name, .. } => {
            name.0.last().map(|i| i.value.to_lowercase()) == Some(table_name.to_lowercase())
        }
        TableFactor::Derived { subquery, .. } => {
            set_expr_references_table(&subquery.body, table_name)
        }
        _ => false,
    }
}

/// Check if an expression contains references to an outer table alias
/// Used to detect correlated subqueries
pub fn expr_has_outer_reference(expr: &Expr, outer_alias: &str) -> bool {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                // Support schema-qualified (schema.table.col) and even db.schema.table.col by
                // treating the second-to-last identifier as the table/alias.
                let table_part = normalize_ident(&parts[parts.len() - 2]);
                table_part.eq_ignore_ascii_case(outer_alias)
            } else {
                false
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_outer_reference(left, outer_alias)
                || expr_has_outer_reference(right, outer_alias)
        }
        Expr::UnaryOp { expr: inner, .. } => expr_has_outer_reference(inner, outer_alias),
        Expr::Nested(inner) => expr_has_outer_reference(inner, outer_alias),
        Expr::Function(f) => {
            for arg in &f.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                    if expr_has_outer_reference(e, outer_alias) {
                        return true;
                    }
                }
            }
            false
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                if expr_has_outer_reference(op, outer_alias) {
                    return true;
                }
            }
            for cond in conditions {
                if expr_has_outer_reference(cond, outer_alias) {
                    return true;
                }
            }
            for res in results {
                if expr_has_outer_reference(res, outer_alias) {
                    return true;
                }
            }
            if let Some(else_expr) = else_result {
                if expr_has_outer_reference(else_expr, outer_alias) {
                    return true;
                }
            }
            false
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            if expr_has_outer_reference(inner, outer_alias) {
                return true;
            }
            for item in list {
                if expr_has_outer_reference(item, outer_alias) {
                    return true;
                }
            }
            false
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_outer_reference(expr, outer_alias)
                || expr_has_outer_reference(low, outer_alias)
                || expr_has_outer_reference(high, outer_alias)
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expr_has_outer_reference(inner, outer_alias)
        }
        Expr::Subquery(q)
        | Expr::InSubquery { subquery: q, .. }
        | Expr::Exists { subquery: q, .. } => query_has_outer_reference(q, outer_alias),
        _ => false,
    }
}

pub fn expr_has_any_outer_reference(expr: &Expr, aliases: &[String]) -> bool {
    aliases.iter().any(|a| expr_has_outer_reference(expr, a))
}

pub fn query_has_outer_reference_in_expr(expr: &Expr, outer_alias: &str) -> bool {
    match expr {
        Expr::Subquery(q) => query_has_outer_reference(q, outer_alias),
        Expr::InSubquery { subquery: q, .. } => query_has_outer_reference(q, outer_alias),
        Expr::Exists { subquery: q, .. } => query_has_outer_reference(q, outer_alias),
        Expr::BinaryOp { left, right, .. } => {
            query_has_outer_reference_in_expr(left, outer_alias)
                || query_has_outer_reference_in_expr(right, outer_alias)
        }
        Expr::UnaryOp { expr: inner, .. } => query_has_outer_reference_in_expr(inner, outer_alias),
        Expr::Nested(inner) => query_has_outer_reference_in_expr(inner, outer_alias),
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            query_has_outer_reference_in_expr(inner, outer_alias)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            query_has_outer_reference_in_expr(expr, outer_alias)
                || query_has_outer_reference_in_expr(low, outer_alias)
                || query_has_outer_reference_in_expr(high, outer_alias)
        }
        Expr::InList { expr, list, .. } => {
            query_has_outer_reference_in_expr(expr, outer_alias)
                || list
                    .iter()
                    .any(|e| query_has_outer_reference_in_expr(e, outer_alias))
        }
        _ => false,
    }
}

/// Check if a query contains references to an outer table alias
pub fn query_has_outer_reference(query: &Query, outer_alias: &str) -> bool {
    match &*query.body {
        SetExpr::Select(select) => {
            // Check selection (WHERE clause)
            if let Some(selection) = &select.selection {
                if expr_has_outer_reference(selection, outer_alias) {
                    return true;
                }
            }
            // Check projection
            for item in &select.projection {
                match item {
                    sqlparser::ast::SelectItem::UnnamedExpr(e)
                    | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } => {
                        if expr_has_outer_reference(e, outer_alias) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            // Check HAVING
            if let Some(having) = &select.having {
                if expr_has_outer_reference(having, outer_alias) {
                    return true;
                }
            }
            false
        }
        SetExpr::SetOperation { left, right, .. } => {
            let left_query = Query {
                with: None,
                body: left.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
            let right_query = Query {
                with: None,
                body: right.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
            query_has_outer_reference(&left_query, outer_alias)
                || query_has_outer_reference(&right_query, outer_alias)
        }
        _ => false,
    }
}

/// Substitute outer table column references with literal values from the current row
pub fn substitute_outer_values(
    expr: &Expr,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
) -> Expr {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                // Support schema-qualified (schema.table.col) and even db.schema.table.col by
                // treating the second-to-last identifier as the table/alias.
                let table_part = normalize_ident(&parts[parts.len() - 2]);
                if table_part.eq_ignore_ascii_case(outer_alias) {
                    let col_name =
                        parts.last().map(normalize_ident).unwrap_or_else(|| "".to_string());
                    // Find column index in outer schema
                    if let Some(col_idx) = outer_schema
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(&col_name))
                    {
                        if let Some(value) = outer_row.values.get(col_idx) {
                            return value_to_sql_expr(value);
                        }
                    }
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_outer_values(
                left,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            op: op.clone(),
            right: Box::new(substitute_outer_values(
                right,
                outer_alias,
                outer_schema,
                outer_row,
            )),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            )),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(substitute_outer_values(
            inner,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::Function(f) => {
            let mut new_args = Vec::new();
            for arg in &f.args {
                let new_arg = match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(substitute_outer_values(
                            e,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        )))
                    }
                    other => other.clone(),
                };
                new_args.push(new_arg);
            }
            Expr::Function(sqlparser::ast::Function {
                name: f.name.clone(),
                args: new_args,
                filter: f.filter.clone(),
                null_treatment: f.null_treatment.clone(),
                over: f.over.clone(),
                distinct: f.distinct,
                special: f.special,
                order_by: f.order_by.clone(),
            })
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            let new_operand = operand.as_ref().map(|op| {
                Box::new(substitute_outer_values(
                    op,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ))
            });
            let new_conditions: Vec<Expr> = conditions
                .iter()
                .map(|c| substitute_outer_values(c, outer_alias, outer_schema, outer_row))
                .collect();
            let new_results: Vec<Expr> = results
                .iter()
                .map(|r| substitute_outer_values(r, outer_alias, outer_schema, outer_row))
                .collect();
            let new_else = else_result.as_ref().map(|e| {
                Box::new(substitute_outer_values(
                    e,
                    outer_alias,
                    outer_schema,
                    outer_row,
                ))
            });
            Expr::Case {
                operand: new_operand,
                conditions: new_conditions,
                results: new_results,
                else_result: new_else,
            }
        }
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => {
            let new_inner = Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            ));
            let new_list: Vec<Expr> = list
                .iter()
                .map(|item| substitute_outer_values(item, outer_alias, outer_schema, outer_row))
                .collect();
            Expr::InList {
                expr: new_inner,
                list: new_list,
                negated: *negated,
            }
        }
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            negated: *negated,
            low: Box::new(substitute_outer_values(
                low,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            high: Box::new(substitute_outer_values(
                high,
                outer_alias,
                outer_schema,
                outer_row,
            )),
        },
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_outer_values(
            inner,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(substitute_outer_values(
            inner,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::Subquery(q) => Expr::Subquery(Box::new(substitute_outer_values_in_query(
            q,
            outer_alias,
            outer_schema,
            outer_row,
        ))),
        Expr::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(substitute_outer_values(
                inner,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            subquery: Box::new(substitute_outer_values_in_query(
                subquery,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            negated: *negated,
        },
        Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(substitute_outer_values_in_query(
                subquery,
                outer_alias,
                outer_schema,
                outer_row,
            )),
            negated: *negated,
        },
        _ => expr.clone(),
    }
}

/// Substitute outer values in a query
pub fn substitute_outer_values_in_query(
    query: &Query,
    outer_alias: &str,
    outer_schema: &TableSchema,
    outer_row: &Row,
) -> Query {
    let new_body = match &*query.body {
        SetExpr::Select(select) => {
            let new_selection = select
                .selection
                .as_ref()
                .map(|sel| substitute_outer_values(sel, outer_alias, outer_schema, outer_row));

            let new_projection: Vec<sqlparser::ast::SelectItem> = select
                .projection
                .iter()
                .map(|item| match item {
                    sqlparser::ast::SelectItem::UnnamedExpr(e) => {
                        sqlparser::ast::SelectItem::UnnamedExpr(substitute_outer_values(
                            e,
                            outer_alias,
                            outer_schema,
                            outer_row,
                        ))
                    }
                    sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                        sqlparser::ast::SelectItem::ExprWithAlias {
                            expr: substitute_outer_values(
                                expr,
                                outer_alias,
                                outer_schema,
                                outer_row,
                            ),
                            alias: alias.clone(),
                        }
                    }
                    other => other.clone(),
                })
                .collect();

            let new_having = select
                .having
                .as_ref()
                .map(|h| substitute_outer_values(h, outer_alias, outer_schema, outer_row));

            Box::new(SetExpr::Select(Box::new(sqlparser::ast::Select {
                distinct: select.distinct.clone(),
                top: select.top.clone(),
                projection: new_projection,
                into: select.into.clone(),
                from: select.from.clone(),
                lateral_views: select.lateral_views.clone(),
                selection: new_selection,
                group_by: select.group_by.clone(),
                cluster_by: select.cluster_by.clone(),
                distribute_by: select.distribute_by.clone(),
                sort_by: select.sort_by.clone(),
                having: new_having,
                named_window: select.named_window.clone(),
                qualify: select.qualify.clone(),
            })))
        }
        _ => query.body.clone(),
    };

    Query {
        with: query.with.clone(),
        body: new_body,
        order_by: query.order_by.clone(),
        limit: query.limit.clone(),
        offset: query.offset.clone(),
        fetch: query.fetch.clone(),
        locks: query.locks.clone(),
        limit_by: query.limit_by.clone(),
        for_clause: query.for_clause.clone(),
    }
}

/// Substitute outer column references in a subquery using JoinContext
/// This handles correlated subqueries in JOIN queries where multiple tables may be referenced
pub fn substitute_join_context_values(
    expr: &Expr,
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_row: &crate::types::Row,
) -> Expr {
    match expr {
        // Only substitute *qualified* outer references (e.g. `outer_alias.col`). Substituting bare
        // identifiers is not scope-aware and can incorrectly rewrite inner-scope columns that share
        // names with outer columns in correlated subqueries.
        Expr::Identifier(_) => expr.clone(),
        Expr::CompoundIdentifier(parts) => {
            let (table_part, col_name) = if parts.len() == 2 {
                (normalize_ident(&parts[0]), normalize_ident(&parts[1]))
            } else if parts.len() == 3 {
                (normalize_ident(&parts[1]), normalize_ident(&parts[2]))
            } else {
                return expr.clone();
            };

            let key = format!("{}.{}", table_part, col_name);
            if let Some(&offset) = column_offsets.get(&key) {
                if let Some(value) = combined_row.values.get(offset) {
                    return value_to_sql_expr(value);
                }
            }
            let key_lower = key.to_lowercase();
            for (k, &offset) in column_offsets {
                if k.to_lowercase() == key_lower {
                    if let Some(value) = combined_row.values.get(offset) {
                        return value_to_sql_expr(value);
                    }
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_join_context_values(
                left,
                column_offsets,
                combined_row,
            )),
            op: op.clone(),
            right: Box::new(substitute_join_context_values(
                right,
                column_offsets,
                combined_row,
            )),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(substitute_join_context_values(
            inner,
            column_offsets,
            combined_row,
        ))),
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Expr::Cast {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::TryCast {
            expr: inner,
            data_type,
            format,
        } => Expr::TryCast {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::SafeCast {
            expr: inner,
            data_type,
            format,
        } => Expr::SafeCast {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::Function(f) => {
            let mut new_args = Vec::new();
            for arg in &f.args {
                let new_arg =
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                substitute_join_context_values(e, column_offsets, combined_row),
                            ))
                        }
                        other => other.clone(),
                    };
                new_args.push(new_arg);
            }
            Expr::Function(sqlparser::ast::Function {
                name: f.name.clone(),
                args: new_args,
                filter: f.filter.clone(),
                null_treatment: f.null_treatment.clone(),
                over: f.over.clone(),
                distinct: f.distinct,
                special: f.special,
                order_by: f.order_by.clone(),
            })
        }
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_join_context_values(
            inner,
            column_offsets,
            combined_row,
        ))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(substitute_join_context_values(
            inner,
            column_offsets,
            combined_row,
        ))),
        Expr::Subquery(q) => Expr::Subquery(Box::new(substitute_join_context_values_in_query(
            q,
            column_offsets,
            combined_row,
        ))),
        Expr::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(substitute_join_context_values(
                inner,
                column_offsets,
                combined_row,
            )),
            subquery: Box::new(substitute_join_context_values_in_query(
                subquery,
                column_offsets,
                combined_row,
            )),
            negated: *negated,
        },
        Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(substitute_join_context_values_in_query(
                subquery,
                column_offsets,
                combined_row,
            )),
            negated: *negated,
        },
        _ => expr.clone(),
    }
}

/// Substitute outer values in a query using JoinContext
pub fn substitute_join_context_values_in_query(
    query: &Query,
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_row: &crate::types::Row,
) -> Query {
    let new_body = match &*query.body {
        SetExpr::Select(select) => {
            let new_selection = select
                .selection
                .as_ref()
                .map(|sel| substitute_join_context_values(sel, column_offsets, combined_row));

            let new_projection: Vec<sqlparser::ast::SelectItem> = select
                .projection
                .iter()
                .map(|item| match item {
                    sqlparser::ast::SelectItem::UnnamedExpr(e) => {
                        sqlparser::ast::SelectItem::UnnamedExpr(substitute_join_context_values(
                            e,
                            column_offsets,
                            combined_row,
                        ))
                    }
                    sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                        sqlparser::ast::SelectItem::ExprWithAlias {
                            expr: substitute_join_context_values(
                                expr,
                                column_offsets,
                                combined_row,
                            ),
                            alias: alias.clone(),
                        }
                    }
                    other => other.clone(),
                })
                .collect();

            let new_having = select
                .having
                .as_ref()
                .map(|h| substitute_join_context_values(h, column_offsets, combined_row));

            Box::new(SetExpr::Select(Box::new(sqlparser::ast::Select {
                distinct: select.distinct.clone(),
                top: select.top.clone(),
                projection: new_projection,
                into: select.into.clone(),
                from: select.from.clone(),
                lateral_views: select.lateral_views.clone(),
                selection: new_selection,
                group_by: select.group_by.clone(),
                cluster_by: select.cluster_by.clone(),
                distribute_by: select.distribute_by.clone(),
                sort_by: select.sort_by.clone(),
                having: new_having,
                named_window: select.named_window.clone(),
                qualify: select.qualify.clone(),
            })))
        }
        _ => query.body.clone(),
    };

    Query {
        with: query.with.clone(),
        body: new_body,
        order_by: query.order_by.clone(),
        limit: query.limit.clone(),
        offset: query.offset.clone(),
        fetch: query.fetch.clone(),
        locks: query.locks.clone(),
        limit_by: query.limit_by.clone(),
        for_clause: query.for_clause.clone(),
    }
}
