//! Timestamp utilities.
//!
//! pg-tikv currently represents timestamps as **milliseconds since Unix epoch** in
//! `Value::Timestamp(i64)`. PostgreSQL supports up to microsecond precision for
//! `CURRENT_TIMESTAMP(p)`, but until the internal representation is upgraded,
//! precision finer than milliseconds is handled on a best-effort basis.

use anyhow::Result;

use chrono::{DateTime, FixedOffset, LocalResult, Offset, TimeZone, Utc};

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
/// - When `is_timestamptz` is `true`, formats in the session `TimeZone` (default UTC)
///   and appends the numeric UTC offset (matching pgwire text encoding behavior).
pub fn format_timestamp_millis(ts_millis: i64, is_timestamptz: bool) -> Result<String> {
    let seconds = ts_millis.div_euclid(1000);
    let millis = ts_millis.rem_euclid(1000) as u32;
    let nanos = millis * 1_000_000;
    let micros = nanos / 1000;

    let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) else {
        return Ok(ts_millis.to_string());
    };

    if is_timestamptz {
        let timezone = crate::session_context::current_timezone();
        let tz = TimeZoneSpec::parse(timezone.as_ref());
        Ok(tz.format_timestamptz(dt, micros))
    } else if micros == 0 {
        Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string())
    } else {
        Ok(dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TimeZoneSpec {
    Fixed(FixedOffset),
    Named(chrono_tz::Tz),
}

impl TimeZoneSpec {
    pub(crate) fn try_parse(setting: &str) -> Result<Self> {
        let setting = setting.trim();
        if setting.is_empty() {
            return Ok(Self::Named(chrono_tz::UTC));
        }
        if setting.eq_ignore_ascii_case("UTC") || setting.eq_ignore_ascii_case("GMT") {
            return Ok(Self::Named(chrono_tz::UTC));
        }

        if let Some(offset) = parse_fixed_offset(setting) {
            return Ok(Self::Fixed(offset));
        }

        let normalized;
        let zone = if setting.as_bytes().contains(&b' ') {
            normalized = setting.replace(' ', "_");
            normalized.as_str()
        } else {
            setting
        };

        zone.parse::<chrono_tz::Tz>()
            .map(Self::Named)
            .map_err(|_| anyhow::anyhow!("time zone \"{}\" not recognized", setting))
    }

    pub(crate) fn parse(setting: &str) -> Self {
        Self::try_parse(setting).unwrap_or(Self::Named(chrono_tz::UTC))
    }

    pub(crate) fn format_timestamptz(self, dt_utc: DateTime<Utc>, micros: u32) -> String {
        match self {
            TimeZoneSpec::Fixed(offset) => format_timestamptz_in_zone(dt_utc, micros, &offset),
            TimeZoneSpec::Named(tz) => format_timestamptz_in_zone(dt_utc, micros, &tz),
        }
    }

    pub(crate) fn timestamp_millis_from_local_datetime(
        self,
        naive: chrono::NaiveDateTime,
    ) -> Result<i64> {
        match self {
            TimeZoneSpec::Fixed(offset) => Ok(offset
                .from_local_datetime(&naive)
                .single()
                .expect("fixed offset must produce a single result")
                .timestamp_millis()),
            TimeZoneSpec::Named(tz) => {
                let local = match tz.from_local_datetime(&naive) {
                    LocalResult::Single(dt) => dt,
                    LocalResult::Ambiguous(dt, _) => dt,
                    LocalResult::None => {
                        return Err(anyhow::anyhow!("Invalid local timestamptz"));
                    }
                };
                Ok(local.timestamp_millis())
            }
        }
    }
}

fn format_timestamptz_in_zone<Tz>(dt_utc: DateTime<Utc>, micros: u32, tz: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Offset + std::fmt::Display,
{
    let local = dt_utc.with_timezone(tz);
    let base = if micros == 0 {
        local.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
    };
    let offset_secs = local.offset().fix().local_minus_utc();
    let tz_suffix = format_offset_suffix(offset_secs);
    format!("{base}{tz_suffix}")
}

fn format_offset_suffix(offset_secs: i32) -> String {
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    let seconds = abs % 60;

    if seconds != 0 {
        format!("{sign}{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else if minutes == 0 {
        format!("{sign}{:02}", hours)
    } else {
        format!("{sign}{:02}:{:02}", hours, minutes)
    }
}

fn parse_fixed_offset(s: &str) -> Option<FixedOffset> {
    // Supported forms:
    // - +HH:MM / -HH:MM
    // - +HH / -HH
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }

    let sign = match bytes[0] {
        b'+' => 1i32,
        b'-' => -1i32,
        _ => return None,
    };

    let rest = &s[1..];
    if rest.is_empty() {
        return None;
    }

    let (hours_str, mins_str_opt) = match rest.find(':') {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };

    if hours_str.is_empty() {
        return None;
    }

    let hours: i32 = hours_str.parse().ok()?;
    let mins: i32 = match mins_str_opt {
        None => 0,
        Some(mins_str) => {
            if mins_str.is_empty() {
                return None;
            }
            mins_str.parse().ok()?
        }
    };

    if hours < 0 || mins < 0 || mins >= 60 {
        return None;
    }

    let total_secs = sign * (hours * 3600 + mins * 60);
    FixedOffset::east_opt(total_secs)
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

    #[test]
    fn formats_timestamptz_in_named_zone() {
        let dt = Utc
            .with_ymd_and_hms(2024, 1, 15, 10, 0, 0)
            .single()
            .unwrap();
        let tz = TimeZoneSpec::parse("America/Los_Angeles");
        assert_eq!(
            tz.format_timestamptz(dt, 0),
            "2024-01-15 02:00:00-08".to_string()
        );
    }

    #[test]
    fn formats_timestamptz_in_fixed_offset() {
        let dt = Utc
            .with_ymd_and_hms(2024, 1, 15, 10, 0, 0)
            .single()
            .unwrap();
        let tz = TimeZoneSpec::parse("+08:00");
        assert_eq!(
            tz.format_timestamptz(dt, 0),
            "2024-01-15 18:00:00+08".to_string()
        );
    }

    #[test]
    fn timezone_spec_try_parse_rejects_unknown_zones() {
        assert!(TimeZoneSpec::try_parse("localtime").is_err());
        assert!(matches!(
            TimeZoneSpec::parse("localtime"),
            TimeZoneSpec::Named(chrono_tz::UTC)
        ));
    }
}
