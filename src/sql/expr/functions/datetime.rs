use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use chrono::Datelike;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("DATE_PART", eval_date_part);
    map.insert("EXTRACT", eval_date_part); // EXTRACT is aliased to DATE_PART
    map.insert("DATE_TRUNC", eval_date_trunc);
    map.insert("DATE", eval_date);
    map.insert("AGE", eval_age);
    map.insert("TO_CHAR", eval_to_char);
    map.insert("MAKE_INTERVAL", eval_make_interval);
}

/// DATE_PART(field, source) — extract a sub-field from a date/time value.
///
/// PostgreSQL: EXTRACT(field FROM source) is compiled to DATE_PART(field, source).
/// Returns Float64.
fn eval_date_part(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let field = match iter.next() {
        Some(Value::Text(s)) => s.to_uppercase(),
        _ => return Ok(Value::Null),
    };
    let source = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };

    let ts = match source {
        Value::Timestamp(t) => t,
        Value::Date(days) => {
            use chrono::NaiveDate;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
                .ok_or_else(|| anyhow!("Failed to create epoch date"))?;
            let date = epoch + chrono::Duration::days(days as i64);
            date.and_hms_opt(0, 0, 0)
                .ok_or_else(|| anyhow!("Failed to create datetime from date"))?
                .and_utc()
                .timestamp_millis()
        }
        Value::Text(s) => {
            use chrono::NaiveDateTime;
            let dt = if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S") {
                dt
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S") {
                dt
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f") {
                dt
            } else if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                d.and_hms_opt(0, 0, 0)
                    .ok_or_else(|| anyhow!("Failed to create datetime from date"))?
            } else {
                return Err(anyhow!("Invalid timestamp format for DATE_PART"));
            };
            dt.and_utc().timestamp_millis()
        }
        Value::Null => return Ok(Value::Null),
        Value::Interval(iv) => {
            // EXTRACT from interval — PostgreSQL semantics.
            // EPOCH returns total seconds (months approximated as 30 days).
            let result = match field.as_str() {
                "EPOCH" => {
                    let month_secs = iv.months as f64 * 30.0 * 86400.0;
                    month_secs + (iv.millis as f64 / 1000.0)
                }
                "HOUR" => {
                    let total_secs = iv.millis / 1000;
                    let after_days = total_secs % 86400;
                    (after_days / 3600) as f64
                }
                "MINUTE" => {
                    let total_secs = iv.millis / 1000;
                    let after_hours = total_secs % 3600;
                    (after_hours / 60) as f64
                }
                "SECOND" => {
                    let total_secs = iv.millis / 1000;
                    (total_secs % 60) as f64
                }
                "DAY" => (iv.millis / 1000 / 86400) as f64,
                "MONTH" => (iv.months % 12) as f64,
                "YEAR" => (iv.months / 12) as f64,
                _ => {
                    return Err(SqlError::Unsupported(format!(
                        "Unsupported EXTRACT field for interval: {}",
                        field
                    ))
                    .into())
                }
            };
            return Ok(Value::Float64(result));
        }
        _ => return Ok(Value::Null),
    };

    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let dt = Utc
        .timestamp_millis_opt(ts)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;

    let result = match field.as_str() {
        "YEAR" => dt.year() as f64,
        "MONTH" => dt.month() as f64,
        "DAY" => dt.day() as f64,
        "HOUR" => dt.hour() as f64,
        "MINUTE" => dt.minute() as f64,
        "SECOND" => dt.second() as f64,
        "DOW" => dt.weekday().num_days_from_sunday() as f64,
        "DOY" => dt.ordinal() as f64,
        "WEEK" => dt.iso_week().week() as f64,
        "QUARTER" => ((dt.month() - 1) / 3 + 1) as f64,
        "EPOCH" => ts as f64 / 1000.0,
        _ => {
            return Err(
                SqlError::Unsupported(format!("Unsupported EXTRACT field: {}", field)).into(),
            )
        }
    };
    Ok(Value::Float64(result))
}

/// DATE_TRUNC(field, source) — truncate a timestamp to specified precision.
fn eval_date_trunc(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let field = match iter.next() {
        Some(Value::Text(s)) => s.to_lowercase(),
        _ => return Ok(Value::Null),
    };
    let ts = match iter.next() {
        Some(Value::Timestamp(t)) => t,
        Some(Value::Date(days)) => crate::model::date::date_days_to_timestamp_millis(days)?,
        Some(Value::Text(s)) => match crate::sql::expr::parse_timestamp_string(&s)? {
            Value::Timestamp(ts) => ts,
            _ => return Ok(Value::Null),
        },
        Some(Value::Null) | None => return Ok(Value::Null),
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
        _ => {
            return Err(
                SqlError::Unsupported(format!("Unsupported DATE_TRUNC field: {}", field)).into(),
            )
        }
    };
    Ok(Value::Timestamp(truncated.timestamp_millis()))
}

/// DATE(source) — cast a value to Date (equivalent to CAST(source AS DATE)).
fn eval_date(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Date(d)) => Ok(Value::Date(d)),
        Some(Value::Timestamp(ts)) => {
            crate::model::date::timestamp_millis_to_date_days(ts).map(Value::Date)
        }
        Some(Value::Text(s)) => crate::model::date::parse_date_days(&s)
            .map(Value::Date)
            .map_err(|_| {
                SqlError::InvalidInputSyntax {
                    type_name: "date".into(),
                    value: s,
                }
                .into()
            }),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(other) => Err(anyhow!("DATE() cannot convert {:?} to date", other)),
    }
}

fn eval_to_char(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("TO_CHAR requires exactly 2 arguments"));
    }

    let value = &args[0];
    let pattern = match &args[1] {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        other => return Err(anyhow!("TO_CHAR format must be text, got {:?}", other)),
    };

    let dt = match value {
        Value::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(*ts)
            .ok_or_else(|| anyhow!("invalid timestamp"))?
            .naive_utc(),
        Value::Date(days) => {
            let ts = crate::model::date::date_days_to_timestamp_millis(*days)?;
            chrono::DateTime::from_timestamp_millis(ts)
                .ok_or_else(|| anyhow!("invalid date"))?
                .naive_utc()
        }
        Value::Text(s) => match crate::sql::expr::parse_timestamp_string(s)? {
            Value::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(ts)
                .ok_or_else(|| anyhow!("invalid timestamp"))?
                .naive_utc(),
            _ => return Err(anyhow!("TO_CHAR cannot convert text to timestamp")),
        },
        Value::Null => return Ok(Value::Null),
        other => return Err(anyhow!("TO_CHAR cannot format {:?}", other)),
    };

    // Minimal PostgreSQL-compatible token subset used by integration tests.
    let chrono_pattern = pattern
        .replace("HH24", "%H")
        .replace("YYYY", "%Y")
        .replace("MM", "%m")
        .replace("DD", "%d")
        .replace("MI", "%M")
        .replace("SS", "%S");

    Ok(Value::Text(dt.format(&chrono_pattern).to_string()))
}

fn eval_age(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("AGE requires 1 or 2 arguments"));
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }

    let end_ts = value_to_naive_datetime(&args[0])?;
    let start_ts = if args.len() == 2 {
        value_to_naive_datetime(&args[1])?
    } else {
        chrono::Utc::now().naive_utc()
    };

    let (end_ts, start_ts, sign) = if end_ts >= start_ts {
        (end_ts, start_ts, 1i32)
    } else {
        (start_ts, end_ts, -1i32)
    };

    let mut months = (end_ts.date().year() - start_ts.date().year()) * 12
        + (end_ts.date().month() as i32 - start_ts.date().month() as i32);
    if (end_ts.date().day(), end_ts.time()) < (start_ts.date().day(), start_ts.time()) {
        months -= 1;
    }

    let anchor = add_months(start_ts, months)?;
    let millis = (end_ts - anchor).num_milliseconds();

    Ok(Value::Interval(crate::model::IntervalValue::new(
        sign * months,
        (sign as i64) * millis,
    )))
}

fn value_to_naive_datetime(v: &Value) -> Result<chrono::NaiveDateTime> {
    match v {
        Value::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(*ts)
            .ok_or_else(|| anyhow!("invalid timestamp"))
            .map(|dt| dt.naive_utc()),
        Value::Date(days) => {
            let ts = crate::model::date::date_days_to_timestamp_millis(*days)?;
            chrono::DateTime::from_timestamp_millis(ts)
                .ok_or_else(|| anyhow!("invalid date"))
                .map(|dt| dt.naive_utc())
        }
        Value::Text(s) => match crate::sql::expr::parse_timestamp_string(s)? {
            Value::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(ts)
                .ok_or_else(|| anyhow!("invalid timestamp"))
                .map(|dt| dt.naive_utc()),
            _ => Err(anyhow!("invalid timestamp text")),
        },
        Value::Null => Err(anyhow!("AGE cannot take NULL directly")),
        other => Err(anyhow!("AGE cannot convert {:?} to timestamp", other)),
    }
}

fn days_in_month(year: i32, month: u32) -> Result<u32> {
    let first_of_next = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .ok_or_else(|| anyhow!("invalid year/month"))?;
    let last_of_month = first_of_next - chrono::Duration::days(1);
    Ok(last_of_month.day())
}

fn add_months(dt: chrono::NaiveDateTime, months: i32) -> Result<chrono::NaiveDateTime> {
    let date = dt.date();
    let total_months = date.year() * 12 + date.month0() as i32 + months;
    let year = total_months.div_euclid(12);
    let month0 = total_months.rem_euclid(12) as u32;
    let month = month0 + 1;
    let day = date.day().min(days_in_month(year, month)?);
    let new_date =
        chrono::NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| anyhow!("invalid date"))?;
    Ok(new_date.and_time(dt.time()))
}

/// MAKE_INTERVAL(years, months, weeks, days, hours, mins, secs) → interval
///
/// PostgreSQL signature: all parameters optional with default 0 (secs is float8).
/// Positional order: years(0), months(1), weeks(2), days(3), hours(4), mins(5), secs(6).
/// Named-arg reordering is handled by the analyzer before args reach here.
fn eval_make_interval(args: Vec<Value>) -> Result<Value> {
    fn arg_to_i32(v: &Value, name: &str) -> Result<i32> {
        match v {
            Value::Int32(i) => Ok(*i),
            Value::Int64(i) => Ok(*i as i32),
            Value::Float64(f) => Ok(*f as i32),
            Value::Null => Ok(0),
            other => Err(anyhow!(
                "make_interval: {} must be integer, got {:?}",
                name,
                other
            )),
        }
    }
    fn arg_to_f64(v: &Value, name: &str) -> Result<f64> {
        match v {
            Value::Float64(f) => Ok(*f),
            Value::Int32(i) => Ok(*i as f64),
            Value::Int64(i) => Ok(*i as f64),
            Value::Null => Ok(0.0),
            other => Err(anyhow!(
                "make_interval: {} must be numeric, got {:?}",
                name,
                other
            )),
        }
    }

    let years = args
        .first()
        .map(|v| arg_to_i32(v, "years"))
        .transpose()?
        .unwrap_or(0);
    let months = args
        .get(1)
        .map(|v| arg_to_i32(v, "months"))
        .transpose()?
        .unwrap_or(0);
    let weeks = args
        .get(2)
        .map(|v| arg_to_i32(v, "weeks"))
        .transpose()?
        .unwrap_or(0);
    let days = args
        .get(3)
        .map(|v| arg_to_i32(v, "days"))
        .transpose()?
        .unwrap_or(0);
    let hours = args
        .get(4)
        .map(|v| arg_to_i32(v, "hours"))
        .transpose()?
        .unwrap_or(0);
    let mins = args
        .get(5)
        .map(|v| arg_to_i32(v, "mins"))
        .transpose()?
        .unwrap_or(0);
    let secs = args
        .get(6)
        .map(|v| arg_to_f64(v, "secs"))
        .transpose()?
        .unwrap_or(0.0);

    let total_months = years * 12 + months;
    let total_days = weeks * 7 + days;
    let millis = (total_days as i64) * 86_400_000
        + (hours as i64) * 3_600_000
        + (mins as i64) * 60_000
        + (secs * 1000.0) as i64;

    Ok(Value::Interval(crate::model::IntervalValue::new(
        total_months,
        millis,
    )))
}
