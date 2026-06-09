use crate::model::{DataType, Value};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use chrono::{Datelike, TimeZone, Timelike};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("DATE_PART", eval_date_part);
    map.insert("EXTRACT", eval_extract);
    map.insert("DATE_TRUNC", eval_date_trunc);
    map.insert("DATE", eval_date);
    map.insert("MAKE_DATE", eval_make_date);
    map.insert("MAKE_TIME", eval_make_time);
    map.insert("MAKE_TIMESTAMP", eval_make_timestamp);
    map.insert("AGE", eval_age);
    map.insert("TO_CHAR", eval_to_char);
    map.insert("MAKE_INTERVAL", eval_make_interval);
    map.insert("TO_TIMESTAMP", eval_to_timestamp);
}

fn datetime_field_overflow_error(message: impl Into<String>) -> anyhow::Error {
    SqlError::DatetimeFieldOverflow {
        message: message.into(),
    }
    .into()
}

fn interval_out_of_range_error() -> anyhow::Error {
    datetime_field_overflow_error("interval out of range")
}

fn sql_year_to_chrono_year(year: i32, error_message: &'static str) -> Result<i32> {
    if year == 0 {
        return Err(datetime_field_overflow_error(error_message));
    }
    if year < 0 {
        year.checked_add(1)
            .ok_or_else(|| datetime_field_overflow_error(error_message))
    } else {
        Ok(year)
    }
}

#[derive(Debug, Clone, Copy)]
enum DatePartResult {
    Integer(i64),
    Micros(i128),
    Infinity { negative: bool },
}

impl DatePartResult {
    fn to_float(self) -> f64 {
        match self {
            DatePartResult::Integer(value) => value as f64,
            DatePartResult::Micros(value) => value as f64 / 1_000_000.0,
            DatePartResult::Infinity { negative } => {
                if negative {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            }
        }
    }

    fn to_numeric_value(self) -> Value {
        match self {
            DatePartResult::Integer(value) => Value::Numeric(Decimal::from(value)),
            DatePartResult::Micros(value) => {
                Value::Numeric(Decimal::from_i128_with_scale(value, 6))
            }
            // Runtime NUMERIC is backed by rust_decimal and cannot carry PG
            // special values. Keep the analyzed return type as NUMERIC and use
            // the existing float carrier for the two special text/binary
            // surfaces.
            DatePartResult::Infinity { negative } => Value::Float64(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }),
        }
    }
}

fn sql_year_from_chrono_year(year: i32) -> i64 {
    if year > 0 {
        i64::from(year)
    } else {
        i64::from(year) - 1
    }
}

/// DATE_PART(field, source) — extract a sub-field from a date/time value.
///
/// Returns Float64.
fn eval_date_part(args: Vec<Value>) -> Result<Value> {
    eval_date_part_common(args, false)
}

/// EXTRACT(field FROM source) returns NUMERIC in PostgreSQL.
fn eval_extract(args: Vec<Value>) -> Result<Value> {
    eval_date_part_common(args, true)
}

pub(crate) fn eval_date_part_common(args: Vec<Value>, return_numeric: bool) -> Result<Value> {
    let mut iter = args.into_iter();
    let field = match iter.next() {
        Some(Value::Text(s)) => s.trim().to_uppercase(),
        _ => return Ok(Value::Null),
    };
    let source = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };

    let ts = match source {
        Value::Timestamp(t) => t,
        Value::Date(days) => crate::model::date::date_days_to_timestamp_millis(days)?,
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
            // Interval date_part/extract — PostgreSQL semantics.
            // EPOCH returns total seconds using PostgreSQL's symbolic month
            // semantics: 1 month = 30 days and 1 year = 365.25 days.
            let result = match field.as_str() {
                "EPOCH" => {
                    let years = i128::from(iv.months / 12);
                    let months = i128::from(iv.months % 12);
                    DatePartResult::Micros(
                        years * 31_557_600_000_000_i128
                            + months * 2_592_000_000_000_i128
                            + i128::from(iv.millis) * 1000,
                    )
                }
                "HOUR" => {
                    let total_secs = iv.millis / 1000;
                    let after_days = total_secs % 86400;
                    DatePartResult::Integer(after_days / 3600)
                }
                "MINUTE" => {
                    let total_secs = iv.millis / 1000;
                    let after_hours = total_secs % 3600;
                    DatePartResult::Integer(after_hours / 60)
                }
                "SECOND" => {
                    let remaining_after_minutes = iv.millis % 60_000;
                    DatePartResult::Micros(i128::from(remaining_after_minutes) * 1000)
                }
                "DAY" => DatePartResult::Integer(iv.millis / 1000 / 86400),
                "MONTH" => DatePartResult::Integer(i64::from(iv.months % 12)),
                "YEAR" => DatePartResult::Integer(i64::from(iv.months / 12)),
                _ => {
                    return Err(SqlError::Unsupported(format!(
                        "Unsupported EXTRACT field for interval: {}",
                        field
                    ))
                    .into());
                }
            };
            return if return_numeric {
                Ok(result.to_numeric_value())
            } else {
                Ok(Value::Float64(result.to_float()))
            };
        }
        _ => return Ok(Value::Null),
    };

    use chrono::{Datelike, TimeZone, Timelike, Utc};
    if ts == i64::MAX || ts == i64::MIN {
        return eval_non_finite_timestamp_part(&field, ts == i64::MIN, return_numeric);
    }

    let dt = Utc
        .timestamp_millis_opt(ts)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;

    let result = match field.as_str() {
        "YEAR" => DatePartResult::Integer(sql_year_from_chrono_year(dt.year())),
        "MONTH" => DatePartResult::Integer(i64::from(dt.month())),
        "DAY" => DatePartResult::Integer(i64::from(dt.day())),
        "HOUR" => DatePartResult::Integer(i64::from(dt.hour())),
        "MINUTE" => DatePartResult::Integer(i64::from(dt.minute())),
        "SECOND" => DatePartResult::Micros(
            i128::from(dt.second()) * 1_000_000 + i128::from(dt.timestamp_subsec_millis()) * 1000,
        ),
        "DOW" => DatePartResult::Integer(i64::from(dt.weekday().num_days_from_sunday())),
        "DOY" => DatePartResult::Integer(i64::from(dt.ordinal())),
        "WEEK" => DatePartResult::Integer(i64::from(dt.iso_week().week())),
        "QUARTER" => DatePartResult::Integer(i64::from((dt.month() - 1) / 3 + 1)),
        "EPOCH" => DatePartResult::Micros(i128::from(ts) * 1000),
        _ => {
            return Err(
                SqlError::Unsupported(format!("Unsupported EXTRACT field: {}", field)).into(),
            );
        }
    };
    if return_numeric {
        Ok(result.to_numeric_value())
    } else {
        Ok(Value::Float64(result.to_float()))
    }
}

fn eval_non_finite_timestamp_part(
    field: &str,
    negative: bool,
    return_numeric: bool,
) -> Result<Value> {
    let result = match field {
        "YEAR" | "EPOCH" => Some(DatePartResult::Infinity { negative }),
        "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" | "DOW" | "DOY" | "WEEK" | "QUARTER" => None,
        _ => {
            return Err(
                SqlError::Unsupported(format!("Unsupported EXTRACT field: {}", field)).into(),
            );
        }
    };

    match result {
        None => Ok(Value::Null),
        Some(result) if return_numeric => Ok(result.to_numeric_value()),
        Some(result) => Ok(Value::Float64(result.to_float())),
    }
}

/// DATE_TRUNC(field, source) — truncate a timestamp to specified precision.
pub(crate) fn eval_date_trunc(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let field = match iter.next() {
        Some(Value::Text(s)) => s.trim().to_lowercase(),
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

    if ts == i64::MAX || ts == i64::MIN {
        return match field.as_str() {
            "year" | "month" | "day" | "hour" | "minute" | "second" => Ok(Value::Timestamp(ts)),
            _ => Err(
                SqlError::Unsupported(format!("Unsupported DATE_TRUNC field: {}", field)).into(),
            ),
        };
    }

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
            );
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

fn eval_make_date(args: Vec<Value>) -> Result<Value> {
    if args.len() != 3 {
        return Err(anyhow!("MAKE_DATE requires exactly 3 arguments"));
    }

    let year = value_to_i32(&args[0], "MAKE_DATE", "year")?;
    let month = value_to_i32(&args[1], "MAKE_DATE", "month")?;
    let day = value_to_i32(&args[2], "MAKE_DATE", "day")?;
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Ok(Value::Null);
    };

    let year = sql_year_to_chrono_year(year, "date field value out of range")?;
    let date = chrono::NaiveDate::from_ymd_opt(year, month as u32, day as u32)
        .ok_or_else(|| datetime_field_overflow_error("date field value out of range"))?;
    crate::model::date::naive_date_to_days(date).map(Value::Date)
}

fn eval_make_time(args: Vec<Value>) -> Result<Value> {
    if args.len() != 3 {
        return Err(anyhow!("MAKE_TIME requires exactly 3 arguments"));
    }

    let hour = value_to_i32(&args[0], "MAKE_TIME", "hour")?;
    let minute = value_to_i32(&args[1], "MAKE_TIME", "minute")?;
    let second = value_to_f64(&args[2], "second")?;
    let (Some(hour), Some(minute), Some(second)) = (hour, minute, second) else {
        return Ok(Value::Null);
    };

    let time_micros = time_fields_to_micros("time", hour, minute, second)?;
    if !(0..=86_400_000_000).contains(&time_micros) {
        return Err(datetime_field_overflow_error(
            "time field value out of range",
        ));
    }
    Ok(Value::Time(time_micros))
}

fn eval_make_timestamp(args: Vec<Value>) -> Result<Value> {
    if args.len() != 6 {
        return Err(anyhow!("MAKE_TIMESTAMP requires exactly 6 arguments"));
    }

    let year = value_to_i32(&args[0], "MAKE_TIMESTAMP", "year")?;
    let month = value_to_i32(&args[1], "MAKE_TIMESTAMP", "month")?;
    let day = value_to_i32(&args[2], "MAKE_TIMESTAMP", "day")?;
    let hour = value_to_i32(&args[3], "MAKE_TIMESTAMP", "hour")?;
    let minute = value_to_i32(&args[4], "MAKE_TIMESTAMP", "minute")?;
    let second = value_to_f64(&args[5], "second")?;
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) =
        (year, month, day, hour, minute, second)
    else {
        return Ok(Value::Null);
    };

    let time_micros = time_fields_to_micros("time", hour, minute, second)?;
    if !(0..=86_400_000_000).contains(&time_micros) {
        return Err(datetime_field_overflow_error(
            "time field value out of range",
        ));
    }

    let carry_days = time_micros / 86_400_000_000;
    let day_time_micros = time_micros % 86_400_000_000;
    let seconds = (day_time_micros / 1_000_000) as u32;
    let micros = (day_time_micros % 1_000_000) as u32;
    let year = sql_year_to_chrono_year(year, "date field value out of range")?;
    let date = chrono::NaiveDate::from_ymd_opt(year, month as u32, day as u32)
        .ok_or_else(|| datetime_field_overflow_error("date field value out of range"))?;
    let date = date
        .checked_add_signed(chrono::Duration::days(carry_days))
        .ok_or_else(|| datetime_field_overflow_error("timestamp out of range"))?;
    let ts = date
        .and_hms_micro_opt(seconds / 3600, (seconds % 3600) / 60, seconds % 60, micros)
        .ok_or_else(|| datetime_field_overflow_error("time field value out of range"))?;
    Ok(Value::Timestamp(ts.and_utc().timestamp_millis()))
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

    match format_to_char_value(value, None, None, pattern)? {
        Some(text) => Ok(Value::Text(text)),
        None => Ok(Value::Null),
    }
}

pub(crate) fn format_to_char_value(
    value: &Value,
    data_type: Option<&DataType>,
    timezone: Option<&str>,
    pattern: &str,
) -> Result<Option<String>> {
    if let Some(negative) = temporal_infinity_sign(value)? {
        return Ok(Some(if negative {
            "-infinity".to_owned()
        } else {
            "infinity".to_owned()
        }));
    }

    let source = match value {
        Value::Timestamp(ts) => {
            let dt = if matches!(data_type, Some(DataType::TimestampTz)) {
                let timezone = timezone
                    .ok_or_else(|| anyhow!("TO_CHAR requires timezone context for timestamptz"))?;
                let tz = crate::model::timestamp::TimeZoneSpec::try_parse(timezone)?;
                let utc = chrono::DateTime::from_timestamp_millis(*ts)
                    .ok_or_else(|| anyhow!("invalid timestamp"))?;
                match tz {
                    crate::model::timestamp::TimeZoneSpec::Fixed(offset) => {
                        utc.with_timezone(&offset).naive_local()
                    }
                    crate::model::timestamp::TimeZoneSpec::Named(tz) => {
                        utc.with_timezone(&tz).naive_local()
                    }
                }
            } else {
                chrono::DateTime::from_timestamp_millis(*ts)
                    .ok_or_else(|| anyhow!("invalid timestamp"))?
                    .naive_utc()
            };
            ToCharSource::DateTime(dt)
        }
        Value::Date(days) if crate::model::date::is_infinite_date_days(*days) => return Ok(None),
        Value::Date(days) => {
            let date = crate::model::date::date_days_to_naive_date(*days)?;
            ToCharSource::DateTime(
                date.and_hms_opt(0, 0, 0)
                    .ok_or_else(|| anyhow!("invalid date"))?,
            )
        }
        Value::Text(s) => match crate::sql::expr::parse_timestamp_string(s)? {
            Value::Timestamp(ts) => {
                return format_to_char_value(&Value::Timestamp(ts), data_type, timezone, pattern);
            }
            _ => return Err(anyhow!("TO_CHAR cannot convert text to timestamp")),
        },
        Value::Time(micros) => {
            let total_seconds = micros / 1_000_000;
            ToCharSource::Time {
                hour: total_seconds / 3_600,
                minute: (total_seconds % 3_600) / 60,
                second: total_seconds % 60,
            }
        }
        Value::Interval(iv) => {
            const DAY_MILLIS: i64 = 86_400_000;
            const HOUR_MILLIS: i64 = 3_600_000;
            const MINUTE_MILLIS: i64 = 60_000;

            let months = i64::from(iv.months);
            let millis = iv.millis;
            let time_millis = millis % DAY_MILLIS;

            ToCharSource::Interval {
                year: months / 12,
                month: months % 12,
                day: millis / DAY_MILLIS,
                hour: time_millis / HOUR_MILLIS,
                minute: (time_millis % HOUR_MILLIS) / MINUTE_MILLIS,
                second: (time_millis % MINUTE_MILLIS) / 1_000,
            }
        }
        Value::Null => unreachable!("NULL handled by caller"),
        other => return Err(anyhow!("TO_CHAR cannot format {:?}", other)),
    };

    Ok(Some(format_to_char_pattern(pattern, &source)))
}

enum ToCharSource {
    DateTime(chrono::NaiveDateTime),
    Time {
        hour: i64,
        minute: i64,
        second: i64,
    },
    Interval {
        year: i64,
        month: i64,
        day: i64,
        hour: i64,
        minute: i64,
        second: i64,
    },
}

#[derive(Clone, Copy)]
enum ToCharField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

fn format_to_char_pattern(pattern: &str, source: &ToCharSource) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut in_quotes = false;
    while i < pattern.len() {
        let rest = &pattern[i..];
        if rest.starts_with('"') {
            if in_quotes && rest.starts_with("\"\"") {
                out.push('"');
                i += 2;
            } else {
                in_quotes = !in_quotes;
                i += 1;
            }
            continue;
        }

        if !in_quotes {
            if starts_with_to_char_token(rest, "HH24") {
                out.push_str(&format_to_char_field(source, ToCharField::Hour));
                i += 4;
                continue;
            }
            if starts_with_to_char_token(rest, "YYYY") {
                out.push_str(&format_to_char_field(source, ToCharField::Year));
                i += 4;
                continue;
            }
            if starts_with_to_char_token(rest, "MM") {
                out.push_str(&format_to_char_field(source, ToCharField::Month));
                i += 2;
                continue;
            }
            if starts_with_to_char_token(rest, "DD") {
                out.push_str(&format_to_char_field(source, ToCharField::Day));
                i += 2;
                continue;
            }
            if starts_with_to_char_token(rest, "MI") {
                out.push_str(&format_to_char_field(source, ToCharField::Minute));
                i += 2;
                continue;
            }
            if starts_with_to_char_token(rest, "SS") {
                out.push_str(&format_to_char_field(source, ToCharField::Second));
                i += 2;
                continue;
            }
        }

        let ch = rest.chars().next().expect("non-empty pattern suffix");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn format_to_char_field(source: &ToCharSource, field: ToCharField) -> String {
    if let (ToCharSource::DateTime(dt), ToCharField::Year) = (source, field) {
        return format!(
            "{:0width$}",
            sql_year_from_chrono_year(dt.year()).unsigned_abs(),
            width = 4
        );
    }

    let value = match (source, field) {
        (ToCharSource::DateTime(_), ToCharField::Year) => unreachable!("handled above"),
        (ToCharSource::DateTime(dt), ToCharField::Month) => i64::from(dt.month()),
        (ToCharSource::DateTime(dt), ToCharField::Day) => i64::from(dt.day()),
        (ToCharSource::DateTime(dt), ToCharField::Hour) => i64::from(dt.hour()),
        (ToCharSource::DateTime(dt), ToCharField::Minute) => i64::from(dt.minute()),
        (ToCharSource::DateTime(dt), ToCharField::Second) => i64::from(dt.second()),
        (ToCharSource::Time { .. }, ToCharField::Year)
        | (ToCharSource::Time { .. }, ToCharField::Month)
        | (ToCharSource::Time { .. }, ToCharField::Day) => 0,
        (ToCharSource::Time { hour, .. }, ToCharField::Hour) => *hour,
        (ToCharSource::Time { minute, .. }, ToCharField::Minute) => *minute,
        (ToCharSource::Time { second, .. }, ToCharField::Second) => *second,
        (ToCharSource::Interval { year, .. }, ToCharField::Year) => *year,
        (ToCharSource::Interval { month, .. }, ToCharField::Month) => *month,
        (ToCharSource::Interval { day, .. }, ToCharField::Day) => *day,
        (ToCharSource::Interval { hour, .. }, ToCharField::Hour) => *hour,
        (ToCharSource::Interval { minute, .. }, ToCharField::Minute) => *minute,
        (ToCharSource::Interval { second, .. }, ToCharField::Second) => *second,
    };

    match field {
        ToCharField::Year => format_signed_field(value, 4),
        ToCharField::Month
        | ToCharField::Day
        | ToCharField::Hour
        | ToCharField::Minute
        | ToCharField::Second => format_signed_field(value, 2),
    }
}

fn starts_with_to_char_token(rest: &str, token: &str) -> bool {
    let rest = rest.as_bytes();
    let token = token.as_bytes();
    rest.len() >= token.len() && rest[..token.len()].eq_ignore_ascii_case(token)
}

fn format_signed_field(value: i64, width: usize) -> String {
    if value < 0 {
        format!("-{:0width$}", value.unsigned_abs(), width = width)
    } else {
        format!("{:0width$}", value, width = width)
    }
}

fn eval_to_timestamp(args: Vec<Value>) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("TO_TIMESTAMP(text, format) is not implemented yet"));
    }

    let Some(epoch_seconds) = value_to_f64(&args[0], "epoch")? else {
        return Ok(Value::Null);
    };
    if epoch_seconds.is_nan() {
        return Err(datetime_field_overflow_error("timestamp cannot be NaN"));
    }
    if epoch_seconds == f64::INFINITY {
        return Ok(Value::Timestamp(i64::MAX));
    }
    if epoch_seconds == f64::NEG_INFINITY {
        return Ok(Value::Timestamp(i64::MIN));
    }
    if !epoch_seconds.is_finite() {
        return Err(datetime_field_overflow_error("timestamp out of range"));
    }

    let seconds = epoch_seconds.floor();
    const I64_MIN_AS_F64: f64 = -9_223_372_036_854_775_808.0;
    const I64_MAX_EXCLUSIVE_AS_F64: f64 = 9_223_372_036_854_775_808.0;
    if !(I64_MIN_AS_F64..I64_MAX_EXCLUSIVE_AS_F64).contains(&seconds) {
        return Err(datetime_field_overflow_error("timestamp out of range"));
    }
    let nanos = ((epoch_seconds - seconds) * 1_000_000_000.0).trunc() as u32;
    let dt = chrono::Utc
        .timestamp_opt(seconds as i64, nanos)
        .single()
        .ok_or_else(|| datetime_field_overflow_error("timestamp out of range"))?;
    Ok(Value::Timestamp(dt.timestamp_millis()))
}

fn eval_age(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("AGE requires 1 or 2 arguments"));
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }

    if let Some(interval) = age_infinite_interval(&args)? {
        return Ok(Value::Interval(interval));
    }

    let end_ts = value_to_naive_datetime(&args[0])?;
    let start_ts = if args.len() == 2 {
        value_to_naive_datetime(&args[1])?
    } else {
        chrono::Utc::now().naive_utc()
    };

    Ok(Value::Interval(age_interval(end_ts, start_ts)?))
}

pub(crate) fn age_infinite_interval(args: &[Value]) -> Result<Option<crate::model::IntervalValue>> {
    let end_sign = temporal_infinity_sign(&args[0])?;
    let start_sign = if args.len() == 2 {
        temporal_infinity_sign(&args[1])?
    } else {
        None
    };

    match (end_sign, start_sign) {
        (Some(end_negative), Some(start_negative)) => {
            if end_negative == start_negative {
                return Err(interval_out_of_range_error());
            }
            Ok(Some(crate::model::IntervalValue::infinity(end_negative)))
        }
        (Some(end_negative), None) => Ok(Some(crate::model::IntervalValue::infinity(end_negative))),
        (None, Some(start_negative)) => {
            Ok(Some(crate::model::IntervalValue::infinity(!start_negative)))
        }
        (None, None) => Ok(None),
    }
}

fn temporal_infinity_sign(value: &Value) -> Result<Option<bool>> {
    Ok(match value {
        Value::Timestamp(ts) if *ts == i64::MAX => Some(false),
        Value::Timestamp(ts) if *ts == i64::MIN => Some(true),
        Value::Date(days) if *days == crate::model::date::DATE_POS_INFINITY_DAYS => Some(false),
        Value::Date(days) if *days == crate::model::date::DATE_NEG_INFINITY_DAYS => Some(true),
        Value::Text(s) => match crate::sql::expr::parse_timestamp_string(s)? {
            Value::Timestamp(ts) if ts == i64::MAX => Some(false),
            Value::Timestamp(ts) if ts == i64::MIN => Some(true),
            _ => None,
        },
        _ => None,
    })
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

pub(crate) fn age_interval(
    left: chrono::NaiveDateTime,
    right: chrono::NaiveDateTime,
) -> Result<crate::model::IntervalValue> {
    let (left_year, left_month, left_day, left_hour, left_min, left_sec, left_usec) =
        datetime_fields(left);
    let (right_year, right_month, right_day, right_hour, right_min, right_sec, right_usec) =
        datetime_fields(right);

    let reverse = left < right;
    let mut years = left_year - right_year;
    let mut months = left_month - right_month;
    let mut days = left_day - right_day;
    let mut hours = left_hour - right_hour;
    let mut minutes = left_min - right_min;
    let mut seconds = left_sec - right_sec;
    let mut usecs = left_usec - right_usec;

    if reverse {
        years = -years;
        months = -months;
        days = -days;
        hours = -hours;
        minutes = -minutes;
        seconds = -seconds;
        usecs = -usecs;
    }

    while usecs < 0 {
        usecs += 1_000_000;
        seconds -= 1;
    }
    while seconds < 0 {
        seconds += 60;
        minutes -= 1;
    }
    while minutes < 0 {
        minutes += 60;
        hours -= 1;
    }
    while hours < 0 {
        hours += 24;
        days -= 1;
    }
    while days < 0 {
        let borrow_days = if reverse {
            days_in_month(left_year, left_month as u32)?
        } else {
            days_in_month(right_year, right_month as u32)?
        };
        days += borrow_days as i32;
        months -= 1;
    }
    while months < 0 {
        months += 12;
        years -= 1;
    }

    if reverse {
        years = -years;
        months = -months;
        days = -days;
        hours = -hours;
        minutes = -minutes;
        seconds = -seconds;
        usecs = -usecs;
    }

    let total_months = years
        .checked_mul(12)
        .and_then(|value| value.checked_add(months))
        .ok_or_else(interval_out_of_range_error)?;
    let millis = age_submonth_millis(days, hours, minutes, seconds, usecs)?;
    Ok(crate::model::IntervalValue::new(total_months, millis))
}

fn datetime_fields(dt: chrono::NaiveDateTime) -> (i32, i32, i32, i32, i32, i32, i32) {
    (
        dt.year(),
        dt.month() as i32,
        dt.day() as i32,
        dt.hour() as i32,
        dt.minute() as i32,
        dt.second() as i32,
        dt.and_utc().timestamp_subsec_micros() as i32,
    )
}

fn age_submonth_millis(
    days: i32,
    hours: i32,
    minutes: i32,
    seconds: i32,
    usecs: i32,
) -> Result<i64> {
    const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
    const MILLIS_PER_HOUR: i64 = 60 * 60 * 1000;
    const MILLIS_PER_MINUTE: i64 = 60 * 1000;
    const MILLIS_PER_SECOND: i64 = 1000;

    let day_millis = i64::from(days)
        .checked_mul(MILLIS_PER_DAY)
        .ok_or_else(interval_out_of_range_error)?;
    let hour_millis = i64::from(hours)
        .checked_mul(MILLIS_PER_HOUR)
        .ok_or_else(interval_out_of_range_error)?;
    let minute_millis = i64::from(minutes)
        .checked_mul(MILLIS_PER_MINUTE)
        .ok_or_else(interval_out_of_range_error)?;
    let second_millis = i64::from(seconds)
        .checked_mul(MILLIS_PER_SECOND)
        .ok_or_else(interval_out_of_range_error)?;
    let usec_millis = i64::from(usecs / 1000);

    day_millis
        .checked_add(hour_millis)
        .and_then(|value| value.checked_add(minute_millis))
        .and_then(|value| value.checked_add(second_millis))
        .and_then(|value| value.checked_add(usec_millis))
        .ok_or_else(interval_out_of_range_error)
}

/// MAKE_INTERVAL(years, months, weeks, days, hours, mins, secs) → interval
///
/// PostgreSQL signature: all parameters optional with default 0 (secs is float8).
/// Positional order: years(0), months(1), weeks(2), days(3), hours(4), mins(5), secs(6).
/// Named-arg reordering is handled by the analyzer before args reach here.
fn eval_make_interval(args: Vec<Value>) -> Result<Value> {
    if args.len() > 7 {
        return Err(anyhow!("MAKE_INTERVAL accepts at most 7 arguments"));
    }

    let years = interval_field_i32(args.first(), "years")?;
    let months = interval_field_i32(args.get(1), "months")?;
    let weeks = interval_field_i32(args.get(2), "weeks")?;
    let days = interval_field_i32(args.get(3), "days")?;
    let hours = interval_field_i32(args.get(4), "hours")?;
    let mins = interval_field_i32(args.get(5), "mins")?;
    let (Some(years), Some(months), Some(weeks), Some(days), Some(hours), Some(mins)) =
        (years, months, weeks, days, hours, mins)
    else {
        return Ok(Value::Null);
    };

    let seconds = if args.len() >= 7 {
        let Some(value) = value_to_f64(&args[6], "secs")? else {
            return Ok(Value::Null);
        };
        if !value.is_finite() {
            return Err(interval_out_of_range_error());
        }
        value
    } else {
        0.0
    };

    let months = years
        .checked_mul(12)
        .and_then(|value| value.checked_add(months))
        .ok_or_else(interval_out_of_range_error)?;

    let mut millis = 0_i64;
    millis =
        checked_add_interval_millis(millis, i64::from(weeks), 7 * 24 * 60 * 60 * 1000, "weeks")?;
    millis = checked_add_interval_millis(millis, i64::from(days), 24 * 60 * 60 * 1000, "days")?;
    millis = checked_add_interval_millis(millis, i64::from(hours), 60 * 60 * 1000, "hours")?;
    millis = checked_add_interval_millis(millis, i64::from(mins), 60 * 1000, "mins")?;

    let seconds_millis = seconds_to_interval_millis(seconds)?;
    millis = millis
        .checked_add(seconds_millis)
        .ok_or_else(interval_out_of_range_error)?;

    Ok(Value::Interval(crate::model::IntervalValue::new(
        months, millis,
    )))
}

fn value_to_i32(value: &Value, function_name: &str, field_name: &str) -> Result<Option<i32>> {
    match value {
        Value::Null => Ok(None),
        Value::Int32(value) => Ok(Some(*value)),
        Value::Int64(value) => i32::try_from(*value)
            .map(Some)
            .map_err(|_| anyhow!("{field_name} is out of range for int4")),
        other => Err(anyhow!(
            "{} {} must be integer, got {:?}",
            function_name,
            field_name,
            other
        )),
    }
}

fn value_to_f64(value: &Value, field_name: &str) -> Result<Option<f64>> {
    match value {
        Value::Null => Ok(None),
        Value::Int32(value) => Ok(Some(*value as f64)),
        Value::Int64(value) => Ok(Some(*value as f64)),
        Value::Float64(value) => Ok(Some(*value)),
        Value::Numeric(value) => value
            .to_f64()
            .map(Some)
            .ok_or_else(|| anyhow!("{field_name} is out of range for double precision")),
        other => Err(anyhow!("{field_name} must be numeric, got {:?}", other)),
    }
}

fn split_seconds_field(seconds: f64, field_name: &str) -> Result<i64> {
    if !seconds.is_finite() {
        return Err(datetime_field_overflow_error(format!(
            "{field_name} field value out of range"
        )));
    }

    let micros = pg_round_to_microseconds(seconds);
    if !(0..=60_000_000).contains(&micros) {
        return Err(datetime_field_overflow_error(format!(
            "{field_name} field value out of range"
        )));
    }
    Ok(micros)
}

fn time_fields_to_micros(field_name: &str, hour: i32, minute: i32, second: f64) -> Result<i64> {
    if !(0..=24).contains(&hour) || !(0..60).contains(&minute) {
        return Err(datetime_field_overflow_error(format!(
            "{field_name} field value out of range"
        )));
    }
    let second_micros = split_seconds_field(second, field_name)?;
    i64::from(hour)
        .checked_mul(3_600_000_000)
        .and_then(|value| value.checked_add(i64::from(minute) * 60_000_000))
        .and_then(|value| value.checked_add(second_micros))
        .ok_or_else(|| {
            datetime_field_overflow_error(format!("{field_name} field value out of range"))
        })
}

fn pg_round_to_microseconds(seconds: f64) -> i64 {
    (seconds * 1_000_000.0).round_ties_even() as i64
}

fn checked_add_interval_millis(
    total: i64,
    units: i64,
    unit_millis: i64,
    _field_name: &str,
) -> Result<i64> {
    let delta = units
        .checked_mul(unit_millis)
        .ok_or_else(interval_out_of_range_error)?;
    total
        .checked_add(delta)
        .ok_or_else(interval_out_of_range_error)
}

fn seconds_to_interval_millis(seconds: f64) -> Result<i64> {
    let millis = (seconds * 1000.0).trunc();
    if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
        return Err(interval_out_of_range_error());
    }
    Ok(millis as i64)
}

fn interval_field_i32(value: Option<&Value>, field_name: &str) -> Result<Option<i32>> {
    match value {
        None => Ok(Some(0)),
        Some(value) => value_to_i32(value, "MAKE_INTERVAL", field_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_datetime_field_overflow(result: anyhow::Result<Value>, expected_message: &str) {
        let err = result.unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("expected structured SQL error");
        assert_eq!(sql_err.sqlstate(), "22008");
        assert!(
            matches!(
                sql_err,
                SqlError::DatetimeFieldOverflow { message } if message == expected_message
            ),
            "expected datetime field overflow {expected_message:?}, got {sql_err:?}"
        );
    }

    #[test]
    fn date_part_second_preserves_fractional_timestamp_seconds() {
        let result = eval_date_part(vec![
            Value::Text("second".into()),
            Value::Timestamp(1_704_198_896_789),
        ])
        .unwrap();

        assert_eq!(result, Value::Float64(56.789));
    }

    #[test]
    fn extract_returns_numeric_with_pg_fractional_scale() {
        let result = eval_extract(vec![
            Value::Text("second".into()),
            Value::Timestamp(1_704_198_896_789),
        ])
        .unwrap();

        assert_eq!(result, Value::Numeric(Decimal::new(56_789_000, 6)));

        let result = eval_extract(vec![
            Value::Text("epoch".into()),
            Value::Interval(crate::model::IntervalValue::new(0, 7_200_000)),
        ])
        .unwrap();

        assert_eq!(result, Value::Numeric(Decimal::new(7_200_000_000, 6)));
    }

    #[test]
    fn interval_epoch_uses_pg_symbolic_month_semantics() {
        let epoch_seconds = |months, millis| {
            let result = eval_date_part(vec![
                Value::Text("epoch".into()),
                Value::Interval(crate::model::IntervalValue::new(months, millis)),
            ])
            .unwrap();

            match result {
                Value::Float64(value) => value,
                other => panic!("expected float64 epoch, got {other:?}"),
            }
        };

        assert_eq!(epoch_seconds(1, 0), 2_592_000.0);
        assert_eq!(epoch_seconds(11, 0), 28_512_000.0);
        assert_eq!(epoch_seconds(12, 0), 31_557_600.0);
        assert_eq!(epoch_seconds(13, 0), 34_149_600.0);
        assert_eq!(epoch_seconds(-1, 0), -2_592_000.0);
        assert_eq!(epoch_seconds(-13, 0), -34_149_600.0);

        let result = eval_extract(vec![
            Value::Text("epoch".into()),
            Value::Interval(crate::model::IntervalValue::new(2, 3_723_000)),
        ])
        .unwrap();
        assert_eq!(result, Value::Numeric(Decimal::new(5_187_723_000_000, 6)));
    }

    #[test]
    fn date_part_and_extract_handle_infinite_timestamps_like_pg() {
        assert_eq!(
            eval_date_part(vec![Value::Text("year".into()), Value::Timestamp(i64::MAX)]).unwrap(),
            Value::Float64(f64::INFINITY)
        );
        assert_eq!(
            eval_date_part(vec![
                Value::Text("epoch".into()),
                Value::Timestamp(i64::MIN)
            ])
            .unwrap(),
            Value::Float64(f64::NEG_INFINITY)
        );
        assert_eq!(
            eval_date_part(vec![
                Value::Text("month".into()),
                Value::Timestamp(i64::MAX)
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_date_part(vec![
                Value::Text("quarter".into()),
                Value::Timestamp(i64::MIN),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_extract(vec![Value::Text("year".into()), Value::Timestamp(i64::MAX)]).unwrap(),
            Value::Float64(f64::INFINITY)
        );
        assert_eq!(
            eval_extract(vec![
                Value::Text("epoch".into()),
                Value::Timestamp(i64::MIN)
            ])
            .unwrap(),
            Value::Float64(f64::NEG_INFINITY)
        );
        assert_eq!(
            eval_extract(vec![
                Value::Text("month".into()),
                Value::Timestamp(i64::MAX)
            ])
            .unwrap(),
            Value::Null
        );

        assert_eq!(
            eval_date_part(vec![
                Value::Text("year".into()),
                Value::Date(crate::model::date::DATE_POS_INFINITY_DAYS),
            ])
            .unwrap(),
            Value::Float64(f64::INFINITY)
        );
        assert_eq!(
            eval_extract(vec![
                Value::Text("year".into()),
                Value::Date(crate::model::date::DATE_NEG_INFINITY_DAYS),
            ])
            .unwrap(),
            Value::Float64(f64::NEG_INFINITY)
        );
        assert_eq!(
            eval_date_part(vec![
                Value::Text("month".into()),
                Value::Date(crate::model::date::DATE_POS_INFINITY_DAYS),
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn date_part_and_extract_adjust_bc_years_like_pg() {
        let bc_timestamp = chrono::NaiveDate::from_ymd_opt(0, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();

        assert_eq!(
            eval_date_part(vec![
                Value::Text("year".into()),
                Value::Timestamp(bc_timestamp)
            ])
            .unwrap(),
            Value::Float64(-1.0)
        );
        assert_eq!(
            eval_extract(vec![
                Value::Text("year".into()),
                Value::Timestamp(bc_timestamp)
            ])
            .unwrap(),
            Value::Numeric(Decimal::from(-1))
        );
    }

    #[test]
    fn temporal_field_names_match_pushdown_normalization() {
        let timestamp = 1_704_198_896_000;

        let result = eval_date_part(vec![
            Value::Text(" day ".into()),
            Value::Timestamp(timestamp),
        ])
        .unwrap();
        assert_eq!(result, Value::Float64(2.0));

        let result = eval_date_trunc(vec![
            Value::Text(" hour ".into()),
            Value::Timestamp(timestamp),
        ])
        .unwrap();
        assert_eq!(result, Value::Timestamp(1_704_196_800_000));
    }

    #[test]
    fn infinite_timestamp_trunc_to_char_and_date_match_pg() {
        assert_eq!(
            eval_date_trunc(vec![Value::Text("day".into()), Value::Timestamp(i64::MAX),]).unwrap(),
            Value::Timestamp(i64::MAX)
        );
        assert_eq!(
            eval_date_trunc(vec![
                Value::Text("second".into()),
                Value::Timestamp(i64::MIN),
            ])
            .unwrap(),
            Value::Timestamp(i64::MIN)
        );
        assert_eq!(
            eval_to_char(vec![Value::Timestamp(i64::MAX), Value::Text("YYYY".into()),]).unwrap(),
            Value::Text("infinity".into())
        );
        assert_eq!(
            eval_to_char(vec![Value::Timestamp(i64::MIN), Value::Text("YYYY".into()),]).unwrap(),
            Value::Text("-infinity".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Date(crate::model::date::DATE_POS_INFINITY_DAYS),
                Value::Text("YYYY".into()),
            ])
            .unwrap(),
            Value::Text("infinity".into())
        );
        assert_eq!(
            eval_date(vec![Value::Timestamp(i64::MAX)]).unwrap(),
            Value::Date(crate::model::date::DATE_POS_INFINITY_DAYS)
        );
        assert_eq!(
            eval_date(vec![Value::Timestamp(i64::MIN)]).unwrap(),
            Value::Date(crate::model::date::DATE_NEG_INFINITY_DAYS)
        );
    }

    #[test]
    fn age_handles_infinite_timestamps_like_pg() {
        let finite = Value::Timestamp(1_704_164_645_000);
        assert_eq!(
            eval_age(vec![Value::Timestamp(i64::MAX), finite.clone()]).unwrap(),
            Value::Interval(crate::model::IntervalValue::infinity(false))
        );
        assert_eq!(
            eval_age(vec![finite.clone(), Value::Timestamp(i64::MAX)]).unwrap(),
            Value::Interval(crate::model::IntervalValue::infinity(true))
        );
        assert_eq!(
            eval_age(vec![Value::Timestamp(i64::MIN), Value::Timestamp(i64::MAX)]).unwrap(),
            Value::Interval(crate::model::IntervalValue::infinity(true))
        );
        assert_datetime_field_overflow(
            eval_age(vec![Value::Timestamp(i64::MAX), Value::Timestamp(i64::MAX)]),
            "interval out of range",
        );
    }

    #[test]
    fn to_char_quotes_literals_like_pg() {
        let timestamp = 1_704_164_645_000;

        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(timestamp),
                Value::Text("YYYY\"x\"MM".into()),
            ])
            .unwrap(),
            Value::Text("2024x01".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(timestamp),
                Value::Text("YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"".into()),
            ])
            .unwrap(),
            Value::Text("2024-01-02T03:04:05Z".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(timestamp),
                Value::Text("YYYY\"x".into()),
            ])
            .unwrap(),
            Value::Text("2024x".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(timestamp),
                Value::Text(r#"YYYY"ab""cd"MM"#.into()),
            ])
            .unwrap(),
            Value::Text(r#"2024ab"cd01"#.into())
        );
    }

    #[test]
    fn to_char_formats_sql_year_numbers_like_pg() {
        let bce = chrono::NaiveDate::from_ymd_opt(0, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        let year_10000 = chrono::NaiveDate::from_ymd_opt(10000, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        let timestamp = chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
            .unwrap()
            .and_hms_opt(3, 4, 5)
            .unwrap()
            .and_utc()
            .timestamp_millis();

        assert_eq!(
            eval_to_char(vec![Value::Timestamp(bce), Value::Text("YYYY".into())]).unwrap(),
            Value::Text("0001".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(year_10000),
                Value::Text("YYYY".into())
            ])
            .unwrap(),
            Value::Text("10000".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(bce),
                Value::Text("YYYY-MM-DD HH24:MI:SS".into()),
            ])
            .unwrap(),
            Value::Text("0001-01-01 00:00:00".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Timestamp(timestamp),
                Value::Text("yyyy-mm-dd hh24:mi:ss".into()),
            ])
            .unwrap(),
            Value::Text("2024-01-02 03:04:05".into())
        );
    }

    #[test]
    fn to_char_supports_time_and_interval_values() {
        assert_eq!(
            eval_to_char(vec![
                Value::Time(12 * 3_600_000_000 + 34 * 60 * 1_000_000 + 56 * 1_000_000),
                Value::Text("HH24:MI:SS".into()),
            ])
            .unwrap(),
            Value::Text("12:34:56".into())
        );
        assert_eq!(
            eval_to_char(vec![
                Value::Interval(crate::model::IntervalValue::new(0, 3_723_000)),
                Value::Text("HH24:MI:SS".into()),
            ])
            .unwrap(),
            Value::Text("01:02:03".into())
        );
    }

    #[test]
    fn interval_date_part_second_preserves_fractional_seconds() {
        let result = eval_date_part(vec![
            Value::Text("second".into()),
            Value::Interval(crate::model::IntervalValue::new(0, 273_906_500)),
        ])
        .unwrap();

        assert_eq!(result, Value::Float64(6.5));
    }

    #[test]
    fn interval_date_part_epoch_uses_pg_symbolic_month_semantics() {
        for (months, expected) in [
            (1, 2_592_000.0),
            (11, 28_512_000.0),
            (12, 31_557_600.0),
            (13, 34_149_600.0),
            (-1, -2_592_000.0),
            (25, 65_707_200.0),
        ] {
            let result = eval_date_part(vec![
                Value::Text("epoch".into()),
                Value::Interval(crate::model::IntervalValue::new(months, 0)),
            ])
            .unwrap();

            assert_eq!(result, Value::Float64(expected));
        }
    }

    #[test]
    fn temporal_constructor_overflow_errors_use_pg_22008_sqlstate() {
        assert_datetime_field_overflow(
            eval_make_date(vec![Value::Int32(2024), Value::Int32(2), Value::Int32(30)]),
            "date field value out of range",
        );
        assert_datetime_field_overflow(
            eval_make_time(vec![Value::Int32(0), Value::Int32(60), Value::Float64(0.0)]),
            "time field value out of range",
        );
        assert_datetime_field_overflow(
            eval_make_timestamp(vec![
                Value::Int32(2024),
                Value::Int32(2),
                Value::Int32(30),
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(0.0),
            ]),
            "date field value out of range",
        );
        assert_datetime_field_overflow(
            eval_make_timestamp(vec![
                Value::Int32(2024),
                Value::Int32(1),
                Value::Int32(1),
                Value::Int32(24),
                Value::Int32(0),
                Value::Float64(0.000001),
            ]),
            "time field value out of range",
        );
        assert_datetime_field_overflow(
            eval_to_timestamp(vec![Value::Float64(f64::NAN)]),
            "timestamp cannot be NaN",
        );
        assert_eq!(
            eval_to_timestamp(vec![Value::Float64(f64::INFINITY)]).unwrap(),
            Value::Timestamp(i64::MAX)
        );
        assert_eq!(
            eval_to_timestamp(vec![Value::Float64(f64::NEG_INFINITY)]).unwrap(),
            Value::Timestamp(i64::MIN)
        );
        assert_datetime_field_overflow(
            eval_to_timestamp(vec![Value::Float64(1.0e20)]),
            "timestamp out of range",
        );
        assert_datetime_field_overflow(
            eval_make_interval(vec![
                Value::Int32(0),
                Value::Int32(0),
                Value::Int32(0),
                Value::Int32(0),
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(f64::INFINITY),
            ]),
            "interval out of range",
        );
        assert_datetime_field_overflow(
            eval_make_interval(vec![Value::Int32(i32::MAX), Value::Int32(i32::MAX)]),
            "interval out of range",
        );
    }

    #[test]
    fn make_time_and_timestamp_match_pg_second_boundary_carry() {
        assert_eq!(
            eval_make_time(vec![Value::Int32(0), Value::Int32(0), Value::Float64(60.0)]).unwrap(),
            Value::Time(60_000_000)
        );
        assert_eq!(
            eval_make_time(vec![
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(59.9999995)
            ])
            .unwrap(),
            Value::Time(60_000_000)
        );
        assert_eq!(
            eval_make_time(vec![
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(0.0000005)
            ])
            .unwrap(),
            Value::Time(0)
        );
        assert_eq!(
            eval_make_time(vec![
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(0.0000025)
            ])
            .unwrap(),
            Value::Time(2)
        );
        assert_eq!(
            eval_make_time(vec![
                Value::Int32(23),
                Value::Int32(59),
                Value::Float64(60.0)
            ])
            .unwrap(),
            Value::Time(86_400_000_000)
        );
        assert_eq!(
            eval_make_time(vec![Value::Int32(24), Value::Int32(0), Value::Float64(0.0)]).unwrap(),
            Value::Time(86_400_000_000)
        );
        assert_eq!(
            eval_make_time(vec![
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(-0.0000005)
            ])
            .unwrap(),
            Value::Time(0)
        );
        eval_make_time(vec![
            Value::Int32(0),
            Value::Int32(0),
            Value::Float64(-0.0000006),
        ])
        .unwrap_err();
        eval_make_time(vec![
            Value::Int32(24),
            Value::Int32(0),
            Value::Float64(0.000001),
        ])
        .unwrap_err();
        eval_make_time(vec![Value::Int32(0), Value::Int32(60), Value::Float64(0.0)]).unwrap_err();

        let carried = chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
            .unwrap()
            .and_hms_micro_opt(0, 0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(
            eval_make_timestamp(vec![
                Value::Int32(2024),
                Value::Int32(1),
                Value::Int32(1),
                Value::Int32(23),
                Value::Int32(59),
                Value::Float64(59.9999995),
            ])
            .unwrap(),
            Value::Timestamp(carried)
        );
        assert_eq!(
            eval_make_timestamp(vec![
                Value::Int32(2024),
                Value::Int32(1),
                Value::Int32(1),
                Value::Int32(24),
                Value::Int32(0),
                Value::Float64(0.0),
            ])
            .unwrap(),
            Value::Timestamp(carried)
        );
        eval_make_timestamp(vec![
            Value::Int32(2024),
            Value::Int32(1),
            Value::Int32(1),
            Value::Int32(24),
            Value::Int32(0),
            Value::Float64(0.000001),
        ])
        .unwrap_err();
        eval_make_timestamp(vec![
            Value::Int32(2024),
            Value::Int32(1),
            Value::Int32(1),
            Value::Int32(0),
            Value::Int32(60),
            Value::Float64(0.0),
        ])
        .unwrap_err();
    }

    #[test]
    fn make_date_and_timestamp_reject_year_zero_and_handle_bc_years() {
        eval_make_date(vec![Value::Int32(0), Value::Int32(1), Value::Int32(1)]).unwrap_err();
        eval_make_timestamp(vec![
            Value::Int32(0),
            Value::Int32(1),
            Value::Int32(1),
            Value::Int32(0),
            Value::Int32(0),
            Value::Float64(0.0),
        ])
        .unwrap_err();

        let bc_date = chrono::NaiveDate::from_ymd_opt(0, 1, 1).unwrap();
        assert_eq!(
            eval_make_date(vec![Value::Int32(-1), Value::Int32(1), Value::Int32(1),]).unwrap(),
            Value::Date(crate::model::date::naive_date_to_days(bc_date).unwrap())
        );

        let bc_timestamp = bc_date
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(
            eval_make_timestamp(vec![
                Value::Int32(-1),
                Value::Int32(1),
                Value::Int32(1),
                Value::Int32(0),
                Value::Int32(0),
                Value::Float64(0.0),
            ])
            .unwrap(),
            Value::Timestamp(bc_timestamp)
        );
    }
}
