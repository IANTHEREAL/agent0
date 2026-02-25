//! Value coercion and literal parsing utilities
//!
//! Functions for converting values between types and parsing PostgreSQL literals.

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::str::FromStr;

use sqlparser::ast::Expr;

use crate::model::{ColumnDef, DataType, Value};

/// Coerce a value to match the expected column type
pub fn coerce_value_for_column(val: Value, col: &ColumnDef) -> Result<Value> {
    crate::sql::types::cast::cast(
        val,
        &col.data_type,
        crate::sql::types::CastContext::Assignment,
    )
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
        Value::Tsvector(_) => DataType::Tsvector,
        Value::Tsquery(_) => DataType::Tsquery,
    }
}

pub fn parse_value_for_copy(val: &str, data_type: &DataType) -> Result<Value> {
    let unescaped = unescape_copy_text(val);
    let trimmed = unescaped.trim();

    match data_type {
        DataType::Boolean => match trimmed.to_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Ok(Value::Boolean(true)),
            "f" | "false" | "0" | "no" | "off" => Ok(Value::Boolean(false)),
            _ => Err(SqlError::InvalidInputSyntax {
                type_name: "boolean".into(),
                value: unescaped.clone(),
            }
            .into()),
        },
        DataType::Int32 => trimmed.parse::<i32>().map(Value::Int32).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Int64 => trimmed.parse::<i64>().map(Value::Int64).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "bigint".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Float64 => trimmed.parse::<f64>().map(Value::Float64).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "double precision".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Timestamp => super::expr::parse_timestamp_string(trimmed).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "timestamp".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::TimestampTz => super::expr::parse_timestamp_string(trimmed).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "timestamp with time zone".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Date => crate::model::date::parse_date_days(trimmed)
            .map(Value::Date)
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "date".into(),
                    value: unescaped.clone(),
                })
            }),
        DataType::Uuid => uuid::Uuid::parse_str(trimmed)
            .map(|u| Value::Uuid(*u.as_bytes()))
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "uuid".into(),
                    value: unescaped.clone(),
                })
            }),
        DataType::Bytes => {
            if unescaped.starts_with("\\x") {
                Ok(hex::decode(&unescaped[2..])
                    .map(Value::Bytes)
                    .unwrap_or(Value::Bytes(unescaped.into_bytes())))
            } else {
                Ok(Value::Bytes(unescaped.into_bytes()))
            }
        }
        DataType::Time => parse_time_string(trimmed).map(Value::Time).ok_or_else(|| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "time".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Interval => super::expr::parse_interval_string(trimmed).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "interval".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::UserDefined(_) => {
            Ok(Value::Text(unescaped))
        }
        DataType::Array(_) => parse_pg_array(trimmed).map(Value::Array).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "array".into(),
                value: unescaped.clone(),
            })
        }),
        DataType::Json => {
            serde_json::from_str::<serde_json::Value>(&unescaped).map_err(|e| {
                SqlError::InvalidInputSyntax {
                    type_name: "json".into(),
                    value: e.to_string(),
                }
            })?;
            Ok(Value::Json(unescaped))
        }
        DataType::Jsonb => {
            let parsed: serde_json::Value =
                serde_json::from_str(&unescaped).map_err(|e| SqlError::InvalidInputSyntax {
                    type_name: "jsonb".into(),
                    value: e.to_string(),
                })?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        DataType::Vector(_) => {
            if unescaped.starts_with('[') && unescaped.ends_with(']') {
                let inner = &unescaped[1..unescaped.len() - 1];
                let elements: Result<Vec<f64>, _> =
                    inner.split(',').map(|s| s.trim().parse::<f64>()).collect();
                elements.map(Value::Vector).map_err(|_| {
                    anyhow::Error::from(SqlError::InvalidInputSyntax {
                        type_name: "vector".into(),
                        value: unescaped.clone(),
                    })
                })
            } else {
                Err(SqlError::InvalidInputSyntax {
                    type_name: "vector".into(),
                    value: unescaped.clone(),
                }
                .into())
            }
        }
        DataType::Numeric { scale, .. } => {
            let mut d = Decimal::from_str(trimmed).map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "numeric".into(),
                    value: unescaped.clone(),
                })
            })?;
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        DataType::Tsvector => Ok(Value::Tsvector(unescaped)),
        DataType::Tsquery => {
            crate::sql::fts::validate_tsquery_syntax(&unescaped)?;
            Ok(Value::Tsquery(unescaped))
        }
    }
}

fn unescape_copy_text(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());

    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }

        i += 1;
        if i >= bytes.len() {
            out.push(b'\\');
            break;
        }

        match bytes[i] {
            b'b' => {
                out.push(0x08);
                i += 1;
            }
            b'f' => {
                out.push(0x0c);
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'v' => {
                out.push(0x0b);
                i += 1;
            }
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'0'..=b'7' => {
                let mut oct: u16 = (bytes[i] - b'0') as u16;
                i += 1;
                for _ in 0..2 {
                    if i < bytes.len() && bytes[i].is_ascii_digit() && bytes[i] < b'8' {
                        oct = (oct * 8).saturating_add((bytes[i] - b'0') as u16);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push((oct & 0xff) as u8);
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }

    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned())
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

/// Convert an internal `Value` to a SQL AST `Expr` for re-parsing
pub fn value_to_sql_expr(v: &Value) -> Expr {
    use sqlparser::ast::Value as SqlValue;
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
        Value::Timestamp(ts) => {
            let seconds = ts.div_euclid(1000);
            let millis = ts.rem_euclid(1000) as u32;
            let nanos = millis * 1_000_000;
            if chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, nanos).is_some() {
                let formatted = crate::model::timestamp::format_timestamp_millis(*ts, false)
                    .unwrap_or_else(|_| ts.to_string());
                Expr::TypedString {
                    data_type: sqlparser::ast::DataType::Timestamp(
                        None,
                        sqlparser::ast::TimezoneInfo::None,
                    ),
                    value: formatted,
                }
            } else {
                Expr::Value(SqlValue::Number(ts.to_string(), false))
            }
        }
        Value::Interval(iv) => {
            let mut parts = Vec::new();
            if iv.months != 0 {
                parts.push(format!("{} month", iv.months));
            }
            if iv.millis != 0 || parts.is_empty() {
                parts.push(format!("{} millisecond", iv.millis));
            }
            Expr::TypedString {
                data_type: sqlparser::ast::DataType::Interval,
                value: parts.join(" "),
            }
        }
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
        Value::Vector(vec) => Expr::Value(SqlValue::SingleQuotedString(
            crate::model::format_vector_pg_text(vec),
        )),
        Value::Json(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Jsonb(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Date(days) => match crate::model::date::format_date_days(*days) {
            Ok(s) => Expr::TypedString {
                data_type: sqlparser::ast::DataType::Date,
                value: s,
            },
            Err(_) => Expr::Value(SqlValue::SingleQuotedString(days.to_string())),
        },
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
        Value::Tsvector(s) | Value::Tsquery(s) => {
            Expr::Value(SqlValue::SingleQuotedString(s.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, IntervalValue};
    use sqlparser::ast::BinaryOperator;

    fn test_col(name: &str, data_type: DataType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }
    }

    #[test]
    fn test_coerce_timestamp_time_array_vector_from_text() {
        let ts_col = test_col("ts", DataType::Timestamp);
        let got =
            coerce_value_for_column(Value::Text("2026-01-01 00:00:00".into()), &ts_col).unwrap();
        assert!(matches!(got, Value::Timestamp(_)));

        let tstz_col = test_col("tsz", DataType::TimestampTz);
        let got = coerce_value_for_column(
            Value::Text("2026-02-02 23:39:52.850 +00:00".into()),
            &tstz_col,
        )
        .unwrap();
        assert!(matches!(got, Value::Timestamp(_)));

        let ts_bad = coerce_value_for_column(Value::Text("not-a-ts".into()), &ts_col)
            .unwrap_err()
            .to_string();
        assert!(ts_bad.contains("invalid input syntax for type timestamp"));

        let time_col = test_col("t", DataType::Time);
        let got =
            coerce_value_for_column(Value::Text("01:02:03.004005".into()), &time_col).unwrap();
        assert_eq!(got, Value::Time(3_723_004_005));

        let time_bad = coerce_value_for_column(Value::Text("99:99".into()), &time_col)
            .unwrap_err()
            .to_string();
        assert!(time_bad.contains("invalid input syntax for type time"));

        let arr_col = test_col("a", DataType::Array(Box::new(DataType::Int32)));
        let got = coerce_value_for_column(Value::Text("{1,2}".into()), &arr_col).unwrap();
        assert_eq!(got, Value::Array(vec![Value::Int32(1), Value::Int32(2)]));

        let arr_bad = coerce_value_for_column(Value::Text("not-an-array".into()), &arr_col)
            .unwrap_err()
            .to_string();
        assert!(arr_bad.contains("invalid input syntax for type array"));

        let vec_col = test_col("v", DataType::Vector(3));
        let got = coerce_value_for_column(Value::Text("[1, 2, 3]".into()), &vec_col).unwrap();
        assert_eq!(got, Value::Vector(vec![1.0, 2.0, 3.0]));

        let vec_bad = coerce_value_for_column(Value::Text("not-a-vector".into()), &vec_col)
            .unwrap_err()
            .to_string();
        assert!(vec_bad.contains("invalid input syntax for type vector"));
    }

    #[test]
    fn test_value_to_sql_expr_timestamp_preserves_timestamp_semantics() {
        use chrono::{TimeZone, Utc};

        let ts1 = Utc
            .with_ymd_and_hms(2000, 1, 1, 0, 0, 1)
            .single()
            .unwrap()
            .timestamp_millis();
        let ts2 = Utc
            .with_ymd_and_hms(2000, 1, 1, 0, 0, 2)
            .single()
            .unwrap()
            .timestamp_millis();

        let right_expr = value_to_sql_expr(&Value::Timestamp(ts1));
        let right_val = crate::sql::expr::bridge::eval_const_ast_expr(&right_expr).unwrap();

        let diff = crate::sql::expr::operators::eval_binary_op(
            Value::Timestamp(ts2),
            &BinaryOperator::Minus,
            right_val,
        )
        .unwrap();

        assert_eq!(
            diff,
            Value::Interval(crate::model::IntervalValue::from_millis(1_000))
        );
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
    fn test_parse_pg_array_strings() {
        let result = parse_pg_array("{hello,world}").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], Value::Text("hello".to_string()));
        assert_eq!(result[1], Value::Text("world".to_string()));
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
    fn parse_value_for_copy_trims_common_scalars() {
        assert_eq!(
            parse_value_for_copy(" 1 ", &DataType::Int32).unwrap(),
            Value::Int32(1)
        );
        assert_eq!(
            parse_value_for_copy(" true\t", &DataType::Boolean).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn parse_value_for_copy_rejects_invalid_timestamp_instead_of_falling_back_to_text() {
        let err = parse_value_for_copy("abc", &DataType::Timestamp).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid input syntax for type timestamp"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn parse_value_for_copy_rejects_invalid_time_instead_of_falling_back_to_text() {
        let err = parse_value_for_copy("25:00:00", &DataType::Time).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid input syntax for type time"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn parse_value_for_copy_parses_interval() {
        assert_eq!(
            parse_value_for_copy("1 day", &DataType::Interval).unwrap(),
            Value::Interval(IntervalValue::from_millis(24 * 60 * 60 * 1000))
        );
    }
}
