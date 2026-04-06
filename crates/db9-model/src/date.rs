use anyhow::{anyhow, Result};
use chrono::{Duration, NaiveDate, TimeZone, Utc};

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
    let date = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow!("invalid input syntax for type date: \"{}\"", s))?;
    naive_date_to_days(date)
}

pub fn format_date_days(days: i32) -> Result<String> {
    Ok(date_days_to_naive_date(days)?
        .format("%Y-%m-%d")
        .to_string())
}

pub fn timestamp_millis_to_date_days(ts_millis: i64) -> Result<i32> {
    let dt = Utc
        .timestamp_millis_opt(ts_millis)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    naive_date_to_days(dt.date_naive())
}

pub fn date_days_to_timestamp_millis(days: i32) -> Result<i64> {
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
    fn timestamp_truncates_to_utc_date() {
        let ts0 = date_days_to_timestamp_millis(0).unwrap();
        assert_eq!(timestamp_millis_to_date_days(ts0).unwrap(), 0);

        let ts_end_of_day0 = ts0 + (24 * 60 * 60 * 1000) - 1;
        assert_eq!(timestamp_millis_to_date_days(ts_end_of_day0).unwrap(), 0);

        let ts1 = date_days_to_timestamp_millis(1).unwrap();
        assert_eq!(timestamp_millis_to_date_days(ts1).unwrap(), 1);
    }
}
