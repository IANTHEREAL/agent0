use crate::sql::error::SqlError;
use crate::types::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("DATE_PART", eval_date_part);
    map.insert("EXTRACT", eval_date_part); // EXTRACT is aliased to DATE_PART
    map.insert("DATE_TRUNC", eval_date_trunc);
    map.insert("DATE", eval_date);
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
        Some(Value::Date(days)) => crate::types::date::date_days_to_timestamp_millis(days)?,
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
            crate::types::date::timestamp_millis_to_date_days(ts).map(Value::Date)
        }
        Some(Value::Text(s)) => crate::types::date::parse_date_days(&s).map(Value::Date),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(other) => Err(anyhow!("DATE() cannot convert {:?} to date", other)),
    }
}
