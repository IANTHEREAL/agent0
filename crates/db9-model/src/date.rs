use anyhow::{anyhow, Result};
use chrono::{Duration, NaiveDate, TimeZone, Utc};

pub const DATE_NEG_INFINITY_DAYS: i32 = i32::MIN;
pub const DATE_POS_INFINITY_DAYS: i32 = i32::MAX;

pub fn is_infinite_date_days(days: i32) -> bool {
    matches!(days, DATE_NEG_INFINITY_DAYS | DATE_POS_INFINITY_DAYS)
}

fn unix_epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 must be a valid date")
}

pub fn naive_date_to_days(date: NaiveDate) -> Result<i32> {
    let days = date.signed_duration_since(unix_epoch_date()).num_days();
    i32::try_from(days).map_err(|_| anyhow!("date is out of supported range"))
}

pub fn date_days_to_naive_date(days: i32) -> Result<NaiveDate> {
    unix_epoch_date()
        .checked_add_signed(Duration::days(days as i64))
        .ok_or_else(|| anyhow!("date is out of supported range"))
}

pub fn parse_date_days(s: &str) -> Result<i32> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("infinity") {
        return Ok(DATE_POS_INFINITY_DAYS);
    }
    if trimmed.eq_ignore_ascii_case("-infinity") {
        return Ok(DATE_NEG_INFINITY_DAYS);
    }

    let date = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .map_err(|_| anyhow!("invalid input syntax for type date: \"{s}\""))?;
    naive_date_to_days(date)
}

pub fn format_date_days(days: i32) -> Result<String> {
    if days == DATE_POS_INFINITY_DAYS {
        return Ok("infinity".to_owned());
    }
    if days == DATE_NEG_INFINITY_DAYS {
        return Ok("-infinity".to_owned());
    }

    Ok(date_days_to_naive_date(days)?
        .format("%Y-%m-%d")
        .to_string())
}

pub fn timestamp_millis_to_date_days(ts_millis: i64) -> Result<i32> {
    if ts_millis == i64::MAX {
        return Ok(DATE_POS_INFINITY_DAYS);
    }
    if ts_millis == i64::MIN {
        return Ok(DATE_NEG_INFINITY_DAYS);
    }

    let dt = Utc
        .timestamp_millis_opt(ts_millis)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    naive_date_to_days(dt.date_naive())
}

pub fn date_days_to_timestamp_millis(days: i32) -> Result<i64> {
    if days == DATE_POS_INFINITY_DAYS {
        return Ok(i64::MAX);
    }
    if days == DATE_NEG_INFINITY_DAYS {
        return Ok(i64::MIN);
    }

    let date = date_days_to_naive_date(days)?;
    let datetime = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("Invalid date"))?;
    Ok(datetime.and_utc().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_date_epoch_is_zero() {
        assert_eq!(parse_date_days("1970-01-01").unwrap(), 0);
        assert_eq!(parse_date_days("1970-01-02").unwrap(), 1);
        assert_eq!(parse_date_days("1969-12-31").unwrap(), -1);
    }

    #[test]
    fn date_round_trip() {
        for days in [-10, -1, 0, 1, 365, 366, 10_000] {
            let s = format_date_days(days).unwrap();
            let parsed = parse_date_days(&s).unwrap();
            assert_eq!(parsed, days);
        }
    }

    #[test]
    fn date_infinity_round_trips_like_pg() {
        assert_eq!(parse_date_days("infinity").unwrap(), DATE_POS_INFINITY_DAYS);
        assert_eq!(
            parse_date_days("-infinity").unwrap(),
            DATE_NEG_INFINITY_DAYS
        );
        assert_eq!(
            format_date_days(DATE_POS_INFINITY_DAYS).unwrap(),
            "infinity"
        );
        assert_eq!(
            format_date_days(DATE_NEG_INFINITY_DAYS).unwrap(),
            "-infinity"
        );
        assert_eq!(
            timestamp_millis_to_date_days(i64::MAX).unwrap(),
            DATE_POS_INFINITY_DAYS
        );
        assert_eq!(
            timestamp_millis_to_date_days(i64::MIN).unwrap(),
            DATE_NEG_INFINITY_DAYS
        );
        assert_eq!(
            date_days_to_timestamp_millis(DATE_POS_INFINITY_DAYS).unwrap(),
            i64::MAX
        );
        assert_eq!(
            date_days_to_timestamp_millis(DATE_NEG_INFINITY_DAYS).unwrap(),
            i64::MIN
        );
    }

    #[test]
    fn timestamp_truncates_to_utc_date() {
        let ts0 = date_days_to_timestamp_millis(0).unwrap();
        assert_eq!(timestamp_millis_to_date_days(ts0).unwrap(), 0);

        let ts_end_of_day0 = ts0 + (24 * 60 * 60 * 1000) - 1;
        assert_eq!(timestamp_millis_to_date_days(ts_end_of_day0).unwrap(), 0);

        let ts1 = date_days_to_timestamp_millis(1).unwrap();
        assert_eq!(timestamp_millis_to_date_days(ts1).unwrap(), 1);
    }
}
