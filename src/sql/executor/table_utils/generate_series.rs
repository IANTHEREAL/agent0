//! `generate_series` value generation for all supported type variants.
//!
//! Supports Int32, Int64, mixed-width integers, Float64, Timestamp, Date,
//! and Numeric series with configurable offset, limit, and max-row guards.

use crate::sql::error::SqlError;
use crate::types::{DataType, Value};
use anyhow::{anyhow, Result};

pub(crate) const DEFAULT_MAX_GENERATE_SERIES_ROWS: usize = 1_000_000;
pub(crate) const FLOAT8_OFFSET_ADVANCE_MAX_ITER: usize = 1024;

pub(crate) fn max_generate_series_rows() -> usize {
    std::env::var("DB9_MAX_GENERATE_SERIES_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_GENERATE_SERIES_ROWS)
}

#[cfg(test)]
pub(super) fn generate_series_values(
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<(Vec<Value>, DataType)> {
    generate_series_values_limited(start, stop, step, 0, None, usize::MAX)
}

pub(crate) fn generate_series_values_limited(
    start: &Value,
    stop: &Value,
    step: &Value,
    offset: usize,
    limit: Option<usize>,
    max_rows: usize,
) -> Result<(Vec<Value>, DataType)> {
    let too_many_rows = || {
        anyhow!(
            "generate_series exceeded max rows ({}); set DB9_MAX_GENERATE_SERIES_ROWS to override",
            max_rows
        )
    };

    let mut remaining = limit.unwrap_or(usize::MAX);

    match (start, stop) {
        (Value::Int32(s), Value::Int32(e)) => {
            let step_val = match step {
                Value::Null => 1,
                Value::Int32(st) => *st,
                Value::Int64(st) => i32::try_from(*st)
                    .map_err(|_| anyhow!("step out of range for integer generate_series"))?,
                _ => return Err(anyhow!("Invalid step type for integer generate_series")),
            };
            if step_val == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            if remaining == 0 {
                return Ok((Vec::new(), DataType::Int32));
            }

            let mut current = *s;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_val) * offset_i128;
                let current_i128 = i128::from(*s) + delta_i128;
                let Ok(cur) = i32::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Int32));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > 0 {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int32(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int32(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Int32))
        }
        (Value::Int64(s), Value::Int64(e)) => {
            let step_val = match step {
                Value::Null => 1i64,
                Value::Int32(st) => *st as i64,
                Value::Int64(st) => *st,
                _ => return Err(anyhow!("Invalid step type for bigint generate_series")),
            };
            if step_val == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            if remaining == 0 {
                return Ok((Vec::new(), DataType::Int64));
            }

            let mut current = *s;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_val) * offset_i128;
                let current_i128 = i128::from(*s) + delta_i128;
                let Ok(cur) = i64::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Int64));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > 0 {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Int64))
        }
        (Value::Int32(_), Value::Int64(_)) | (Value::Int64(_), Value::Int32(_)) => {
            let s64 = match start {
                Value::Int32(v) => *v as i64,
                Value::Int64(v) => *v,
                _ => unreachable!(),
            };
            let e64 = match stop {
                Value::Int32(v) => *v as i64,
                Value::Int64(v) => *v,
                _ => unreachable!(),
            };
            let step_val = match step {
                Value::Null => 1i64,
                Value::Int32(st) => *st as i64,
                Value::Int64(st) => *st,
                _ => return Err(anyhow!("Invalid step type for bigint generate_series")),
            };
            if step_val == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            if remaining == 0 {
                return Ok((Vec::new(), DataType::Int64));
            }

            let mut current = s64;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_val) * offset_i128;
                let current_i128 = i128::from(s64) + delta_i128;
                let Ok(cur) = i64::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Int64));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > 0 {
                while current <= e64 {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= e64 {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Int64))
        }
        (Value::Float64(s), Value::Float64(e)) => {
            let step_val = match step {
                Value::Null => 1.0,
                Value::Float64(st) => *st,
                Value::Int32(st) => *st as f64,
                Value::Int64(st) => *st as f64,
                _ => return Err(anyhow!("Invalid step type for float8 generate_series")),
            };
            if step_val == 0.0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            if remaining == 0 {
                return Ok((Vec::new(), DataType::Float64));
            }

            let mut current = *s;
            if offset > 0 {
                if !step_val.is_finite() {
                    return Ok((Vec::new(), DataType::Float64));
                }
                if offset <= FLOAT8_OFFSET_ADVANCE_MAX_ITER {
                    if step_val > 0.0 {
                        for _ in 0..offset {
                            if current > *e + f64::EPSILON {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            if current >= *e {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                            current = next;
                        }
                    } else {
                        for _ in 0..offset {
                            if current < *e - f64::EPSILON {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            if current <= *e {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                            current = next;
                        }
                    }
                } else {
                    if step_val > 0.0 {
                        if current < *e {
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                        }
                    } else {
                        if current > *e {
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                        }
                    }

                    let prev = step_val.mul_add((offset - 1) as f64, current);
                    if step_val > 0.0 {
                        if prev >= *e {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    } else {
                        if prev <= *e {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    }

                    current = step_val.mul_add(offset as f64, current);
                    if step_val > 0.0 {
                        if current > *e + f64::EPSILON {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    } else {
                        if current < *e - f64::EPSILON {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    }
                }
            }

            if step_val > 0.0 {
                if current < *e {
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                }
                let mut values = Vec::new();
                while current <= *e + f64::EPSILON {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Float64(current));
                    remaining = remaining.saturating_sub(1);
                    if current >= *e {
                        break;
                    }
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                    current = next;
                }
                Ok((values, DataType::Float64))
            } else {
                if current > *e {
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                }
                let mut values = Vec::new();
                while current >= *e - f64::EPSILON {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Float64(current));
                    remaining = remaining.saturating_sub(1);
                    if current <= *e {
                        break;
                    }
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                    current = next;
                }
                Ok((values, DataType::Float64))
            }
        }
        (Value::Timestamp(s), Value::Timestamp(e)) => {
            let step_interval = match step {
                Value::Interval(iv) => iv.clone(),
                _ => {
                    return Err(anyhow!(
                        "generate_series with timestamps requires interval step"
                    ))
                }
            };
            let step_ms = interval_to_millis(&step_interval);
            if step_ms == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            if remaining == 0 {
                return Ok((Vec::new(), DataType::Timestamp));
            }

            let mut current = *s;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_ms) * offset_i128;
                let current_i128 = i128::from(*s) + delta_i128;
                let Ok(cur) = i64::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Timestamp));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_ms > 0 {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Timestamp(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_ms) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Timestamp(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_ms) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Timestamp))
        }
        (Value::Date(s), Value::Date(e)) => {
            let step_interval = match step {
                Value::Interval(iv) => iv.clone(),
                _ => return Err(anyhow!("generate_series with dates requires interval step")),
            };
            if step_interval.months == 0 && step_interval.millis == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            // Validate step size before honoring LIMIT/OFFSET.
            let step_ms = interval_to_millis(&step_interval);
            if step_ms == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            if remaining == 0 {
                return Ok((Vec::new(), DataType::TimestampTz));
            }

            let tz = crate::types::timestamp::TimeZoneSpec::parse(
                crate::session_context::current_timezone().as_ref(),
            );
            let naive_date_midnight_timestamptz = |date: chrono::NaiveDate| -> Result<i64> {
                let naive = date
                    .and_hms_opt(0, 0, 0)
                    .ok_or_else(|| anyhow!("Invalid date"))?;
                tz.timestamp_millis_from_local_datetime(naive)
            };
            let date_midnight_timestamptz = |days: i32| -> Result<i64> {
                let date = crate::types::date::date_days_to_naive_date(days)?;
                naive_date_midnight_timestamptz(date)
            };

            let mut values = Vec::new();

            const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
            let has_subday_component = step_interval.millis % MILLIS_PER_DAY != 0;
            // For calendar steps (month/day), advance by calendar months/days instead of fixed
            // milliseconds. This avoids month length drift and DST drift.
            if !has_subday_component {
                let step_days_i64 = step_interval.millis / MILLIS_PER_DAY;
                if step_interval.months == 0 {
                    let Ok(step_days) = i32::try_from(step_days_i64) else {
                        return Ok((Vec::new(), DataType::TimestampTz));
                    };
                    if step_days == 0 {
                        return Err(anyhow!("step size cannot equal zero"));
                    }

                    let mut current = *s;
                    if offset > 0 {
                        let offset_i128 = offset as i128;
                        let delta_i128 = i128::from(step_days) * offset_i128;
                        let current_i128 = i128::from(*s) + delta_i128;
                        let Ok(cur_i32) = i32::try_from(current_i128) else {
                            return Ok((Vec::new(), DataType::TimestampTz));
                        };
                        current = cur_i32;
                    }

                    if step_days > 0 {
                        while current <= *e {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values.push(Value::Timestamp(date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            current = match current.checked_add(step_days) {
                                Some(next) => next,
                                None => break,
                            };
                        }
                    } else {
                        while current >= *e {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values.push(Value::Timestamp(date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            current = match current.checked_add(step_days) {
                                Some(next) => next,
                                None => break,
                            };
                        }
                    }
                } else {
                    use chrono::Datelike;

                    let step_days = step_days_i64;
                    let add_months_clamped =
                        |date: chrono::NaiveDate, months: i32| -> Option<chrono::NaiveDate> {
                            if months == 0 {
                                return Some(date);
                            }

                            let year = i64::from(date.year());
                            let month0 = i64::from(date.month0());
                            let total_months = year
                                .checked_mul(12)?
                                .checked_add(month0)?
                                .checked_add(i64::from(months))?;
                            let new_year = i32::try_from(total_months.div_euclid(12)).ok()?;
                            let new_month0 = total_months.rem_euclid(12);
                            let new_month = u32::try_from(new_month0 + 1).ok()?;

                            let day = date.day();
                            let first_of_next_month = if new_month == 12 {
                                chrono::NaiveDate::from_ymd_opt(new_year.checked_add(1)?, 1, 1)?
                            } else {
                                chrono::NaiveDate::from_ymd_opt(new_year, new_month + 1, 1)?
                            };
                            let last_day = first_of_next_month.pred_opt()?.day();
                            chrono::NaiveDate::from_ymd_opt(new_year, new_month, day.min(last_day))
                        };

                    let apply_step = |date: chrono::NaiveDate| -> Option<chrono::NaiveDate> {
                        let with_months = add_months_clamped(date, step_interval.months)?;
                        with_months.checked_add_signed(chrono::Duration::days(step_days))
                    };

                    let mut current = crate::types::date::date_days_to_naive_date(*s)?;
                    let stop = crate::types::date::date_days_to_naive_date(*e)?;
                    let step_forward =
                        step_interval.months > 0 || (step_interval.months == 0 && step_days > 0);

                    if offset > 0 {
                        for _ in 0..offset {
                            let Some(next) = apply_step(current) else {
                                return Ok((Vec::new(), DataType::TimestampTz));
                            };
                            current = next;
                        }
                    }

                    if step_forward {
                        while current <= stop {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values
                                .push(Value::Timestamp(naive_date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            if current == stop {
                                break;
                            }
                            let next = match apply_step(current) {
                                Some(next) => next,
                                None => break,
                            };
                            if next <= current {
                                return Err(anyhow!(
                                    "generate_series interval step does not make forward progress for date"
                                ));
                            }
                            current = next;
                        }
                    } else {
                        while current >= stop {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values
                                .push(Value::Timestamp(naive_date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            if current == stop {
                                break;
                            }
                            let next = match apply_step(current) {
                                Some(next) => next,
                                None => break,
                            };
                            if next >= current {
                                return Err(anyhow!(
                                    "generate_series interval step does not make backward progress for date"
                                ));
                            }
                            current = next;
                        }
                    }
                }
            } else {
                let start_ms = date_midnight_timestamptz(*s)?;
                let stop_ms = date_midnight_timestamptz(*e)?;

                let mut current = start_ms;
                if offset > 0 {
                    let offset_i128 = offset as i128;
                    let delta_i128 = i128::from(step_ms) * offset_i128;
                    let current_i128 = i128::from(current) + delta_i128;
                    let Ok(cur) = i64::try_from(current_i128) else {
                        return Ok((Vec::new(), DataType::TimestampTz));
                    };
                    current = cur;
                }

                if step_ms > 0 {
                    while current <= stop_ms {
                        if remaining == 0 {
                            break;
                        }
                        if values.len() >= max_rows {
                            return Err(too_many_rows());
                        }
                        values.push(Value::Timestamp(current));
                        remaining = remaining.saturating_sub(1);
                        current = match current.checked_add(step_ms) {
                            Some(next) => next,
                            None => break,
                        };
                    }
                } else {
                    while current >= stop_ms {
                        if remaining == 0 {
                            break;
                        }
                        if values.len() >= max_rows {
                            return Err(too_many_rows());
                        }
                        values.push(Value::Timestamp(current));
                        remaining = remaining.saturating_sub(1);
                        current = match current.checked_add(step_ms) {
                            Some(next) => next,
                            None => break,
                        };
                    }
                }
            }

            Ok((values, DataType::TimestampTz))
        }
        (Value::Numeric(s), Value::Numeric(e)) => {
            let step_val = match step {
                Value::Null => rust_decimal::Decimal::ONE,
                Value::Numeric(st) => *st,
                Value::Float64(st) => rust_decimal::Decimal::try_from(*st).map_err(|_| {
                    SqlError::InvalidInputSyntax {
                        type_name: "numeric".into(),
                        value: st.to_string(),
                    }
                })?,
                Value::Int32(st) => rust_decimal::Decimal::from(*st),
                Value::Int64(st) => rust_decimal::Decimal::from(*st),
                _ => return Err(anyhow!("Invalid step type for numeric generate_series")),
            };
            if step_val.is_zero() {
                return Err(anyhow!("step size cannot equal zero"));
            }

            let data_type = DataType::Numeric {
                precision: None,
                scale: None,
            };

            if remaining == 0 {
                return Ok((Vec::new(), data_type));
            }

            let mut current = *s;
            if offset > 0 {
                let Ok(offset_i128) = i128::try_from(offset) else {
                    return Ok((Vec::new(), data_type));
                };
                let offset_dec = rust_decimal::Decimal::from_i128_with_scale(offset_i128, 0);
                let Some(delta) = step_val.checked_mul(offset_dec) else {
                    return Ok((Vec::new(), data_type));
                };
                let Some(cur) = current.checked_add(delta) else {
                    return Ok((Vec::new(), data_type));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > rust_decimal::Decimal::ZERO {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Numeric(current));
                    remaining = remaining.saturating_sub(1);
                    if current == *e {
                        break;
                    }
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Numeric(current));
                    remaining = remaining.saturating_sub(1);
                    if current == *e {
                        break;
                    }
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, data_type))
        }
        _ => Err(anyhow!(
            "generate_series requires numeric or timestamp arguments, got {:?} and {:?}",
            start,
            stop
        )),
    }
}

pub(crate) fn interval_to_millis(iv: &crate::types::IntervalValue) -> i64 {
    iv.to_millis_approx()
}
