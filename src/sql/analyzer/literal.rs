//! Typed literal parsing for the Analyzer.
//!
//! Converts string representations of typed literals (`DATE '2024-01-01'`,
//! `TIMESTAMP '...'`, `INTERVAL '...'`, etc.) into `Value` at analysis time.
//! This satisfies RFC v2 requirement C3: no `TypedLiteral` in the IR.

use sqlparser::ast::{self as ast, Expr};

use crate::model::{DataType, IntervalValue, Value};

use super::error::AnalyzerError;
use super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Parse a typed string literal (e.g., `DATE '2024-01-01'`) to a `Value`.
    ///
    /// Invalid literals produce `AnalyzerError::InvalidLiteral` — caught at
    /// analysis time, not runtime.
    pub(super) fn parse_typed_literal(
        &self,
        value: &str,
        target_type: &DataType,
    ) -> Result<Value, AnalyzerError> {
        match target_type {
            DataType::Date => {
                let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|e| {
                    AnalyzerError::InvalidLiteral {
                        value: value.to_string(),
                        target_type: target_type.clone(),
                        parse_error: e.to_string(),
                    }
                })?;
                let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let days = (date - epoch).num_days() as i32;
                Ok(Value::Date(days))
            }
            DataType::Timestamp => self.parse_timestamp_value(value, target_type),
            DataType::TimestampTz => self.parse_timestamp_value(value, target_type),
            DataType::Time => {
                let t = chrono::NaiveTime::parse_from_str(value, "%H:%M:%S")
                    .or_else(|_| chrono::NaiveTime::parse_from_str(value, "%H:%M:%S%.f"))
                    .map_err(|e| AnalyzerError::InvalidLiteral {
                        value: value.to_string(),
                        target_type: target_type.clone(),
                        parse_error: e.to_string(),
                    })?;
                let micros = t
                    .signed_duration_since(chrono::NaiveTime::MIN)
                    .num_microseconds()
                    .unwrap_or(0);
                Ok(Value::Time(micros))
            }
            DataType::Interval => {
                let iv = self.parse_interval_str(value)?;
                Ok(Value::Interval(iv))
            }
            DataType::Boolean => {
                let b = match value.to_lowercase().as_str() {
                    "true" | "t" | "yes" | "y" | "on" | "1" => true,
                    "false" | "f" | "no" | "n" | "off" | "0" => false,
                    _ => {
                        return Err(AnalyzerError::InvalidLiteral {
                            value: value.to_string(),
                            target_type: target_type.clone(),
                            parse_error: "expected boolean value".to_string(),
                        })
                    }
                };
                Ok(Value::Boolean(b))
            }
            _ => {
                // For other types, store as text — the evaluator handles conversion
                Ok(Value::Text(value.to_string()))
            }
        }
    }

    /// Shared timestamp parsing for both Timestamp and TimestampTz.
    fn parse_timestamp_value(
        &self,
        value: &str,
        target_type: &DataType,
    ) -> Result<Value, AnalyzerError> {
        crate::sql::expr::parse_timestamp_string(value).map_err(|e| AnalyzerError::InvalidLiteral {
            value: value.to_string(),
            target_type: target_type.clone(),
            parse_error: e.to_string(),
        })
    }

    /// Parse an interval expression (wraps parse_interval_str).
    pub(super) fn parse_interval(&mut self, expr: &Expr) -> Result<IntervalValue, AnalyzerError> {
        match expr {
            Expr::Value(ast::Value::SingleQuotedString(s)) => self.parse_interval_str(s),
            _ => Err(AnalyzerError::Unsupported(
                "non-string interval literal".to_string(),
            )),
        }
    }

    /// Parse an interval string like "1 day 2 hours" or "01:30:00".
    pub(super) fn parse_interval_str(&self, s: &str) -> Result<IntervalValue, AnalyzerError> {
        let mut months = 0i32;
        let mut millis = 0i64;

        let s = s.trim();

        if s.contains(':') {
            // Time format: HH:MM:SS or HH:MM:SS.fff
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() >= 2 {
                let hours: i64 = parts[0].trim().parse().unwrap_or(0);
                let mins: i64 = parts[1].trim().parse().unwrap_or(0);
                let secs: f64 = if parts.len() > 2 {
                    parts[2].trim().parse().unwrap_or(0.0)
                } else {
                    0.0
                };
                millis = hours * 3_600_000 + mins * 60_000 + (secs * 1000.0) as i64;
            }
        } else {
            // "N unit" patterns
            let lower = s.to_lowercase();
            let tokens: Vec<&str> = lower.split_whitespace().collect();
            let mut i = 0;
            while i < tokens.len() {
                if let Ok(n) = tokens[i].parse::<i64>() {
                    let unit = tokens.get(i + 1).copied().unwrap_or("");
                    match unit {
                        u if u.starts_with("year") => months += n as i32 * 12,
                        u if u.starts_with("month") || u.starts_with("mon") => months += n as i32,
                        u if u.starts_with("day") => millis += n * 86_400_000,
                        u if u.starts_with("hour") => millis += n * 3_600_000,
                        u if u.starts_with("minute") || u.starts_with("min") => {
                            millis += n * 60_000
                        }
                        u if u.starts_with("second") || u.starts_with("sec") => millis += n * 1_000,
                        u if u.starts_with("millisecond") || u == "ms" => millis += n,
                        _ => {
                            // Just a number without unit, treat as seconds
                            millis += n * 1_000;
                            i += 1;
                            continue;
                        }
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }

        Ok(IntervalValue::new(months, millis))
    }
}
