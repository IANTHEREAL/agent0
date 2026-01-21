//! Timestamp utilities.
//!
//! pg-tikv currently represents timestamps as **milliseconds since Unix epoch** in
//! `Value::Timestamp(i64)`. PostgreSQL supports up to microsecond precision for
//! `CURRENT_TIMESTAMP(p)`, but until the internal representation is upgraded,
//! precision finer than milliseconds is handled on a best-effort basis.

use anyhow::Result;

/// Truncate an epoch-millis timestamp to `precision` fractional digits.
///
/// Precision follows PostgreSQL semantics for `CURRENT_TIMESTAMP(p)`:
/// - `precision = 0` truncates to whole seconds (no fractional part).
/// - `precision = 3` truncates to milliseconds.
/// - `precision` is clamped to `0..=6`.
///
/// Note: pg-tikv stores timestamps in milliseconds, so `precision > 3` cannot be
/// represented precisely and is treated as millisecond precision.
pub fn truncate_timestamp_millis(ts_millis: i64, precision: u32) -> i64 {
    let precision = precision.min(6);
    let step_millis = match precision {
        0 => 1000,
        1 => 100,
        2 => 10,
        // Millisecond precision is the finest representable today.
        _ => 1,
    };
    ts_millis.div_euclid(step_millis) * step_millis
}

/// Format an epoch-millis timestamp as a PostgreSQL-compatible text timestamp.
///
/// - When `is_timestamptz` is `false`, formats as `YYYY-MM-DD HH:MM:SS[.ffffff]` in UTC.
/// - When `is_timestamptz` is `true`, formats in America/Los_Angeles and appends the
///   numeric UTC offset (matching pg-tikv's wire encoding behavior).
pub fn format_timestamp_millis(ts_millis: i64, is_timestamptz: bool) -> Result<String> {
    use chrono::{DateTime, Offset, Utc};

    let seconds = ts_millis.div_euclid(1000);
    let millis = ts_millis.rem_euclid(1000) as u32;
    let nanos = millis * 1_000_000;

    let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) else {
        return Ok(ts_millis.to_string());
    };

    if is_timestamptz {
        let local = dt.with_timezone(&chrono_tz::America::Los_Angeles);
        let base = if nanos == 0 {
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            local.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
        };
        let offset_secs = local.offset().fix().local_minus_utc();
        let sign = if offset_secs >= 0 { '+' } else { '-' };
        let abs = offset_secs.unsigned_abs();
        let hours = abs / 3600;
        let minutes = (abs % 3600) / 60;
        let tz = if minutes == 0 {
            format!("{sign}{:02}", hours)
        } else {
            format!("{sign}{:02}:{:02}", hours, minutes)
        };
        Ok(format!("{base}{tz}"))
    } else if nanos == 0 {
        Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string())
    } else {
        Ok(dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_positive_timestamps() {
        let ts = 1_234_567_i64;
        assert_eq!(truncate_timestamp_millis(ts, 0), 1_234_000);
        assert_eq!(truncate_timestamp_millis(ts, 1), 1_234_500);
        assert_eq!(truncate_timestamp_millis(ts, 2), 1_234_560);
        assert_eq!(truncate_timestamp_millis(ts, 3), 1_234_567);
        assert_eq!(truncate_timestamp_millis(ts, 6), 1_234_567);
    }

    #[test]
    fn truncates_negative_timestamps_with_floor_semantics() {
        let ts = -1_234_i64;
        assert_eq!(truncate_timestamp_millis(ts, 0), -2_000);
        assert_eq!(truncate_timestamp_millis(ts, 1), -1_300);
        assert_eq!(truncate_timestamp_millis(ts, 2), -1_240);
        assert_eq!(truncate_timestamp_millis(ts, 3), -1_234);
        assert_eq!(truncate_timestamp_millis(ts, 6), -1_234);
    }

    #[test]
    fn formats_epoch_zero_without_fraction() {
        let s = format_timestamp_millis(0, false).unwrap();
        assert_eq!(s, "1970-01-01 00:00:00");
    }
}

