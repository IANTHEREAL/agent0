//! Tests for `generate_series` value generation.

use super::generate_series::*;
use crate::model::{DataType, Value};
use chrono::{Offset, TimeZone, Timelike};
use std::sync::Arc;

#[test]
fn storage_stats_virtual_tables_lazy_load_persisted_stats_per_tenant() {
    let source = include_str!("mod.rs");

    assert!(
        source.contains("async fn load_persisted_storage_stats("),
        "storage stats virtual tables should use a per-tenant lazy loader"
    );
    assert!(
        source.contains("deserialize_storage_stats"),
        "lazy loader must read the persisted storage stats format"
    );
    assert!(
        source.contains("self.load_persisted_storage_stats(txn, None).await?"),
        "_DB9_SYS_STORAGE_STATS should lazy-load current keyspace stats"
    );
    assert!(
        source.contains("self.load_persisted_storage_stats(txn, Some(db_id)).await?"),
        "_DB9_SYS_TABLE_STORAGE_STATS should lazy-load current database stats"
    );
    assert!(
        !source.contains("list_worker_registry"),
        "table_utils lazy loading must not enumerate the worker registry"
    );
}

fn with_session_timezone<T>(timezone: &str, f: impl FnOnce() -> T) -> T {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(crate::session_context::with_timezone(
        Arc::from(timezone),
        async move { f() },
    ))
}

#[test]
fn generate_series_int32_edge_overflow_does_not_loop() {
    let (values, ty) = generate_series_values(
        &Value::Int32(i32::MAX - 1),
        &Value::Int32(i32::MAX),
        &Value::Int32(1),
    )
    .unwrap();
    assert_eq!(ty, DataType::Int32);
    assert_eq!(
        values,
        vec![Value::Int32(i32::MAX - 1), Value::Int32(i32::MAX)]
    );

    let (values, ty) = generate_series_values(
        &Value::Int32(i32::MIN + 1),
        &Value::Int32(i32::MIN),
        &Value::Int32(-1),
    )
    .unwrap();
    assert_eq!(ty, DataType::Int32);
    assert_eq!(
        values,
        vec![Value::Int32(i32::MIN + 1), Value::Int32(i32::MIN)]
    );
}

#[test]
fn generate_series_limited_applies_offset_limit_int32() {
    let (values, ty) = generate_series_values_limited(
        &Value::Int32(1),
        &Value::Int32(10),
        &Value::Null,
        2,
        Some(3),
        100,
    )
    .unwrap();
    assert_eq!(ty, DataType::Int32);
    assert_eq!(
        values,
        vec![Value::Int32(3), Value::Int32(4), Value::Int32(5)]
    );
}

#[test]
fn generate_series_limited_large_offset_is_fast_and_correct() {
    let (values, ty) = generate_series_values_limited(
        &Value::Int32(1),
        &Value::Int32(1_000_000_000),
        &Value::Null,
        999_999_999,
        Some(1),
        10,
    )
    .unwrap();
    assert_eq!(ty, DataType::Int32);
    assert_eq!(values, vec![Value::Int32(1_000_000_000)]);
}

#[test]
fn generate_series_limited_large_offset_is_fast_and_correct_float8() {
    let (values, ty) = generate_series_values_limited(
        &Value::Float64(1.0),
        &Value::Float64(1e16),
        &Value::Float64(1.0),
        1_000_000_000,
        Some(1),
        10,
    )
    .unwrap();
    assert_eq!(ty, DataType::Float64);
    assert_eq!(values, vec![Value::Float64(1_000_000_001.0)]);
}

#[test]
fn generate_series_limited_large_offset_beyond_range_float8_returns_empty() {
    let (values, ty) = generate_series_values_limited(
        &Value::Float64(0.0),
        &Value::Float64(1e-16),
        &Value::Float64(1e-19),
        2000,
        Some(1),
        10,
    )
    .unwrap();
    assert_eq!(ty, DataType::Float64);
    assert!(values.is_empty());
}

#[test]
fn generate_series_limited_enforces_max_rows() {
    let err = generate_series_values_limited(
        &Value::Int32(1),
        &Value::Int32(100),
        &Value::Null,
        0,
        None,
        10,
    )
    .unwrap_err();
    assert!(err.to_string().contains("exceeded max rows"));
}

#[test]
fn generate_series_limited_limit_avoids_max_rows_error() {
    let (values, ty) = generate_series_values_limited(
        &Value::Int32(1),
        &Value::Int32(1_000_000_000),
        &Value::Null,
        0,
        Some(1),
        10,
    )
    .unwrap();
    assert_eq!(ty, DataType::Int32);
    assert_eq!(values, vec![Value::Int32(1)]);
}

#[test]
fn generate_series_int32_step_out_of_range_errors() {
    let err = generate_series_values(
        &Value::Int32(1),
        &Value::Int32(2),
        &Value::Int64(i64::from(i32::MAX) + 1),
    )
    .unwrap_err();
    assert!(err.to_string().contains("step out of range"));
}

#[test]
fn generate_series_int64_edge_overflow_does_not_loop() {
    let (values, ty) = generate_series_values(
        &Value::Int64(i64::MAX - 1),
        &Value::Int64(i64::MAX),
        &Value::Int64(1),
    )
    .unwrap();
    assert_eq!(ty, DataType::Int64);
    assert_eq!(
        values,
        vec![Value::Int64(i64::MAX - 1), Value::Int64(i64::MAX)]
    );

    let (values, ty) = generate_series_values(
        &Value::Int64(i64::MIN + 1),
        &Value::Int64(i64::MIN),
        &Value::Int64(-1),
    )
    .unwrap();
    assert_eq!(ty, DataType::Int64);
    assert_eq!(
        values,
        vec![Value::Int64(i64::MIN + 1), Value::Int64(i64::MIN)]
    );
}

#[test]
#[cfg(target_pointer_width = "64")]
fn generate_series_limited_offset_mul_overflow_still_returns_rows() {
    let (values, ty) = generate_series_values_limited(
        &Value::Int64(-9_000_000_000_000_000_000i64),
        &Value::Int64(9_000_000_000_000_000_000i64),
        &Value::Int64(2),
        5_000_000_000_000_000_000usize,
        Some(1),
        10,
    )
    .unwrap();
    assert_eq!(ty, DataType::Int64);
    assert_eq!(values, vec![Value::Int64(1_000_000_000_000_000_000i64)]);
}

#[test]
fn generate_series_timestamp_overflow_does_not_loop() {
    let (values, ty) = generate_series_values(
        &Value::Timestamp(1),
        &Value::Timestamp(1),
        &Value::Interval(crate::model::IntervalValue::from_millis(i64::MAX)),
    )
    .unwrap();
    assert_eq!(ty, DataType::Timestamp);
    assert_eq!(values, vec![Value::Timestamp(1)]);
}

#[test]
fn generate_series_date_overflow_does_not_loop() {
    let step = Value::Interval(crate::model::IntervalValue::from_millis(i64::MAX));
    let (values, ty) = generate_series_values(&Value::Date(1), &Value::Date(1), &step).unwrap();
    assert_eq!(ty, DataType::TimestampTz);

    let tz = crate::model::timestamp::TimeZoneSpec::parse(
        crate::session_context::current_timezone().as_ref(),
    );
    let date = crate::model::date::date_days_to_naive_date(1).unwrap();
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    let expected = tz.timestamp_millis_from_local_datetime(naive).unwrap();
    assert_eq!(values, vec![Value::Timestamp(expected)]);
}

#[test]
fn generate_series_date_sub_day_step_includes_intermediate() {
    let start_days = crate::model::date::parse_date_days("2024-01-01").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-01-02").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_millis(
        12 * 60 * 60 * 1000,
    ));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);

    let tz = chrono_tz::America::Los_Angeles;
    let d1 = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let d2 = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
    let t0 = tz
        .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t12 = tz
        .from_local_datetime(&d1.and_hms_opt(12, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t24 = tz
        .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        values,
        vec![
            Value::Timestamp(t0),
            Value::Timestamp(t12),
            Value::Timestamp(t24)
        ]
    );
}

#[test]
fn generate_series_date_sub_day_step_across_dst_start_does_not_error() {
    let start_days = crate::model::date::parse_date_days("2024-03-10").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-03-11").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_millis(60 * 60 * 1000));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);
    assert_eq!(values.len(), 24);

    let tz = chrono_tz::America::Los_Angeles;
    let mut hours = Vec::with_capacity(values.len());
    for v in &values {
        let Value::Timestamp(ms) = v else {
            panic!("expected timestamptz values");
        };
        hours.push(tz.timestamp_millis_opt(*ms).single().unwrap().hour());
    }
    assert_eq!(hours.iter().filter(|&&h| h == 2).count(), 0);
    assert!(hours.contains(&3));
}

#[test]
fn generate_series_date_sub_day_step_across_dst_end_does_not_error() {
    let start_days = crate::model::date::parse_date_days("2024-11-03").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-11-04").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_millis(60 * 60 * 1000));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);
    assert_eq!(values.len(), 26);

    let tz = chrono_tz::America::Los_Angeles;
    let mut offsets = std::collections::BTreeSet::new();
    let mut hour_1_count = 0;
    for v in &values {
        let Value::Timestamp(ms) = v else {
            panic!("expected timestamptz values");
        };
        let dt = tz.timestamp_millis_opt(*ms).single().unwrap();
        if dt.hour() == 1 {
            hour_1_count += 1;
            offsets.insert(dt.offset().fix().local_minus_utc());
        }
    }
    assert_eq!(hour_1_count, 2);
    assert_eq!(offsets.len(), 2);
    assert!(offsets.contains(&(-7 * 3600)));
    assert!(offsets.contains(&(-8 * 3600)));
}

#[test]
fn generate_series_date_interval_does_not_truncate_remainder() {
    let start_days = crate::model::date::parse_date_days("2024-01-01").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-01-03").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_millis(
        36 * 60 * 60 * 1000,
    ));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);

    let tz = chrono_tz::America::Los_Angeles;
    let d1 = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let d2 = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
    let t0 = tz
        .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t36 = tz
        .from_local_datetime(&d2.and_hms_opt(12, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();

    assert_eq!(values, vec![Value::Timestamp(t0), Value::Timestamp(t36)]);
}

#[test]
fn generate_series_date_month_step_across_dst_start_keeps_local_midnight() {
    let start_days = crate::model::date::parse_date_days("2024-03-01").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-05-01").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_months(1));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);

    let tz = chrono_tz::America::Los_Angeles;
    let d1 = chrono::NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
    let d2 = chrono::NaiveDate::from_ymd_opt(2024, 4, 1).unwrap();
    let d3 = chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap();

    let t1 = tz
        .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t2 = tz
        .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t3 = tz
        .from_local_datetime(&d3.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        values,
        vec![
            Value::Timestamp(t1),
            Value::Timestamp(t2),
            Value::Timestamp(t3)
        ]
    );
}

#[test]
fn generate_series_date_day_step_across_dst_start_keeps_local_midnight() {
    let start_days = crate::model::date::parse_date_days("2024-03-09").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-03-11").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_millis(
        24 * 60 * 60 * 1000,
    ));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);

    let tz = chrono_tz::America::Los_Angeles;
    let d1 = chrono::NaiveDate::from_ymd_opt(2024, 3, 9).unwrap();
    let d2 = chrono::NaiveDate::from_ymd_opt(2024, 3, 10).unwrap();
    let d3 = chrono::NaiveDate::from_ymd_opt(2024, 3, 11).unwrap();

    let t1 = tz
        .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t2 = tz
        .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t3 = tz
        .from_local_datetime(&d3.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        values,
        vec![
            Value::Timestamp(t1),
            Value::Timestamp(t2),
            Value::Timestamp(t3)
        ]
    );
}

#[test]
fn generate_series_date_day_step_across_dst_end_keeps_local_midnight() {
    let start_days = crate::model::date::parse_date_days("2024-11-02").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-11-04").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::from_millis(
        24 * 60 * 60 * 1000,
    ));

    let (values, ty) = with_session_timezone("America/Los_Angeles", || {
        generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step).unwrap()
    });

    assert_eq!(ty, DataType::TimestampTz);

    let tz = chrono_tz::America::Los_Angeles;
    let d1 = chrono::NaiveDate::from_ymd_opt(2024, 11, 2).unwrap();
    let d2 = chrono::NaiveDate::from_ymd_opt(2024, 11, 3).unwrap();
    let d3 = chrono::NaiveDate::from_ymd_opt(2024, 11, 4).unwrap();

    let t1 = tz
        .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t2 = tz
        .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    let t3 = tz
        .from_local_datetime(&d3.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();

    assert_eq!(
        values,
        vec![
            Value::Timestamp(t1),
            Value::Timestamp(t2),
            Value::Timestamp(t3)
        ]
    );
}

#[test]
fn generate_series_date_mixed_sign_month_day_step_progress_guard_errors() {
    let start_days = crate::model::date::parse_date_days("2024-01-01").unwrap();
    let stop_days = crate::model::date::parse_date_days("2024-01-03").unwrap();
    let step = Value::Interval(crate::model::IntervalValue::new(
        1,
        -31_i64 * 24 * 60 * 60 * 1000,
    ));

    let err = generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("does not make forward progress for date"));
}

#[test]
fn generate_series_float8_progress_guard_errors() {
    let err = generate_series_values(
        &Value::Float64(1e16),
        &Value::Float64(1e16 + 1e6),
        &Value::Float64(1.0),
    )
    .unwrap_err();
    assert!(err.to_string().contains("too small to make progress"));
}

#[test]
fn generate_series_limited_offset_matches_non_pushdown_float8() {
    let start = Value::Float64(0.0);
    let stop = Value::Float64(2.0);
    let step = Value::Float64(0.1);

    let (all, all_ty) = generate_series_values(&start, &stop, &step).unwrap();
    assert_eq!(all_ty, DataType::Float64);

    let offset = 10;
    let limit = 3;
    let (limited, limited_ty) =
        generate_series_values_limited(&start, &stop, &step, offset, Some(limit), 100).unwrap();
    assert_eq!(limited_ty, DataType::Float64);

    assert_eq!(limited, all[offset..offset + limit].to_vec());
}

#[test]
fn generate_series_float8_progress_guard_errors_even_with_limit() {
    let err = generate_series_values_limited(
        &Value::Float64(1e16),
        &Value::Float64(1e16 + 1e6),
        &Value::Float64(1.0),
        0,
        Some(1),
        10,
    )
    .unwrap_err();
    assert!(err.to_string().contains("too small to make progress"));
}

#[test]
fn generate_series_float8_progress_guard_errors_even_with_large_offset() {
    let err = generate_series_values_limited(
        &Value::Float64(1e16),
        &Value::Float64(1e16 + 1e6),
        &Value::Float64(1.0),
        999_998,
        Some(1),
        10,
    )
    .unwrap_err();
    assert!(err.to_string().contains("too small to make progress"));
}

#[test]
fn generate_series_numeric_max_does_not_panic_or_loop() {
    use std::str::FromStr;

    let max = rust_decimal::Decimal::from_str("79228162514264337593543950335").unwrap();
    let (values, ty) = generate_series_values(
        &Value::Numeric(max),
        &Value::Numeric(max),
        &Value::Numeric(rust_decimal::Decimal::ONE),
    )
    .unwrap();
    assert_eq!(
        ty,
        DataType::Numeric {
            precision: None,
            scale: None,
        }
    );
    assert_eq!(values, vec![Value::Numeric(max)]);
}

#[test]
fn generate_series_numeric_float_step_nan_errors() {
    let err = generate_series_values(
        &Value::Numeric(rust_decimal::Decimal::ONE),
        &Value::Numeric(rust_decimal::Decimal::from(2)),
        &Value::Float64(f64::NAN),
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("invalid input syntax for type numeric"));
}

#[test]
fn generate_series_numeric_float_step_infinite_errors() {
    let err = generate_series_values(
        &Value::Numeric(rust_decimal::Decimal::ONE),
        &Value::Numeric(rust_decimal::Decimal::from(2)),
        &Value::Float64(f64::INFINITY),
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("invalid input syntax for type numeric"));
}
