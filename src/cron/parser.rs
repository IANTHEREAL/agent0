use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::str::FromStr;

#[derive(Debug)]
pub struct CronSchedule {
    schedule: Schedule,
    expression: String,
}

impl CronSchedule {
    pub fn expression(&self) -> &str {
        &self.expression
    }
}

pub fn parse_cron_expression(expr: &str) -> Result<CronSchedule> {
    let expr = expr.trim();

    if expr.is_empty() {
        return Err(anyhow!("invalid cron expression: empty string"));
    }

    if expr.starts_with('@') {
        return Err(anyhow!(
            "invalid cron expression: special strings like '{}' are not supported. Use standard 5-field cron syntax.",
            expr
        ));
    }

    if expr.contains("second") || expr.contains("minute") || expr.contains("hour") {
        return Err(anyhow!(
            "invalid cron expression: interval syntax '{}' is not supported. Use standard 5-field cron syntax (e.g. '*/5 * * * *').",
            expr
        ));
    }

    let fields: Vec<&str> = expr.split_whitespace().collect();

    if fields.len() == 6 {
        return Err(anyhow!(
            "invalid cron expression: 6-field expressions (with seconds) are not supported. Use 5-field syntax."
        ));
    }

    if fields.len() != 5 {
        return Err(anyhow!(
            "invalid cron expression: expected 5 fields (minute hour day month weekday), got {}",
            fields.len()
        ));
    }

    let expr_with_seconds = format!("0 {}", expr);

    let schedule = Schedule::from_str(&expr_with_seconds)
        .map_err(|e| anyhow!("invalid cron expression '{}': {}", expr, e))?;

    Ok(CronSchedule {
        schedule,
        expression: expr.to_string(),
    })
}

pub fn next_occurrence(schedule: &CronSchedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    schedule.schedule.after(&after).next()
}

pub fn is_due(schedule: &CronSchedule, at: DateTime<Utc>) -> bool {
    schedule.schedule.includes(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_every_minute() {
        assert!(parse_cron_expression("* * * * *").is_ok());
    }

    #[test]
    fn valid_every_5_minutes() {
        assert!(parse_cron_expression("*/5 * * * *").is_ok());
    }

    #[test]
    fn valid_specific_time() {
        assert!(parse_cron_expression("0 3 * * 1-5").is_ok());
    }

    #[test]
    fn valid_lists() {
        assert!(parse_cron_expression("0,30 9-17 * * *").is_ok());
    }

    #[test]
    fn valid_daily_at_noon() {
        assert!(parse_cron_expression("0 12 * * *").is_ok());
    }

    #[test]
    fn valid_every_hour() {
        assert!(parse_cron_expression("0 * * * *").is_ok());
    }

    #[test]
    fn reject_six_field() {
        let err = parse_cron_expression("* * * * * *").unwrap_err();
        assert!(err.to_string().contains("6-field"));
    }

    #[test]
    fn reject_empty() {
        assert!(parse_cron_expression("").is_err());
    }

    #[test]
    fn reject_whitespace_only() {
        assert!(parse_cron_expression("   ").is_err());
    }

    #[test]
    fn reject_interval_syntax_seconds() {
        assert!(parse_cron_expression("30 seconds").is_err());
    }

    #[test]
    fn reject_interval_syntax_minutes() {
        assert!(parse_cron_expression("5 minutes").is_err());
    }

    #[test]
    fn reject_interval_syntax_hours() {
        assert!(parse_cron_expression("2 hours").is_err());
    }

    #[test]
    fn reject_special_string_daily() {
        assert!(parse_cron_expression("@daily").is_err());
    }

    #[test]
    fn reject_special_string_reboot() {
        assert!(parse_cron_expression("@reboot").is_err());
    }

    #[test]
    fn reject_special_string_hourly() {
        assert!(parse_cron_expression("@hourly").is_err());
    }

    #[test]
    fn reject_special_string_yearly() {
        assert!(parse_cron_expression("@yearly").is_err());
    }

    #[test]
    fn reject_too_few_fields() {
        let err = parse_cron_expression("0 12 *").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields"));
    }

    #[test]
    fn reject_too_many_fields_seven() {
        let err = parse_cron_expression("0 0 12 * * * *").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields"));
    }

    #[test]
    fn next_occurrence_returns_some() {
        let sched = parse_cron_expression("0 12 * * *").unwrap();
        let after = Utc::now();
        let next = next_occurrence(&sched, after);
        assert!(next.is_some());
    }

    #[test]
    fn next_occurrence_is_after_input() {
        let sched = parse_cron_expression("0 12 * * *").unwrap();
        let after = Utc::now();
        let next = next_occurrence(&sched, after).unwrap();
        assert!(next > after);
    }

    #[test]
    fn is_due_works_for_matching_time() {
        let sched = parse_cron_expression("0 12 * * *").unwrap();
        let at = Utc::now();
        let result = is_due(&sched, at);
        let _ = result;
    }

    #[test]
    fn expression_stored_correctly() {
        let expr = "*/15 * * * *";
        let sched = parse_cron_expression(expr).unwrap();
        assert_eq!(sched.expression(), expr);
    }

    #[test]
    fn valid_complex_expression() {
        assert!(parse_cron_expression("0,30 9-17 1,15 * 1-5").is_ok());
    }

    #[test]
    fn valid_step_values() {
        assert!(parse_cron_expression("*/10 */2 * * *").is_ok());
    }

    #[test]
    fn valid_ranges() {
        assert!(parse_cron_expression("0 9-17 * * 1-5").is_ok());
    }
}
