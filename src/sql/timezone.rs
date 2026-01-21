//! Time zone parsing utilities.
//!
//! This module provides a minimal, fast parser for `AT TIME ZONE` support.
//! We intentionally keep an MVP set of named zones and numeric UTC offsets.

use anyhow::{anyhow, Result};

/// Parse a time zone name or numeric offset into a fixed offset in seconds.
///
/// The returned value is `local_minus_utc` (same sign convention as PostgreSQL):
/// - `"Asia/Shanghai"` → `+8 * 3600`
/// - `"America/New_York"` → `-5 * 3600` (MVP: fixed offset, DST not applied)
/// - `"+08:00"` → `+8 * 3600`
/// - `"-05:00"` → `-5 * 3600`
pub(crate) fn parse_timezone_offset_seconds(zone: &str) -> Result<i32> {
    let zone = zone.trim();
    if zone.is_empty() {
        return Err(anyhow!("unknown time zone: {}", zone));
    }

    if let Some(offset) = parse_offset_string_seconds(zone) {
        return Ok(offset);
    }

    let normalized;
    let zone_for_match = if zone.as_bytes().contains(&b' ') {
        normalized = zone.replace(' ', "_");
        normalized.as_str()
    } else {
        zone
    };

    // NOTE: MVP implementation uses fixed offsets and does not apply DST rules.
    // Add new common zones here as needed.
    if zone_for_match.eq_ignore_ascii_case("UTC") || zone_for_match.eq_ignore_ascii_case("GMT") {
        return Ok(0);
    }
    if zone_for_match.eq_ignore_ascii_case("AMERICA/NEW_YORK") {
        return Ok(-5 * 3600);
    }
    if zone_for_match.eq_ignore_ascii_case("AMERICA/LOS_ANGELES") {
        return Ok(-8 * 3600);
    }
    if zone_for_match.eq_ignore_ascii_case("EUROPE/LONDON") {
        return Ok(0);
    }
    if zone_for_match.eq_ignore_ascii_case("EUROPE/PARIS") {
        return Ok(1 * 3600);
    }
    if zone_for_match.eq_ignore_ascii_case("ASIA/SHANGHAI")
        || zone_for_match.eq_ignore_ascii_case("PRC")
    {
        return Ok(8 * 3600);
    }
    if zone_for_match.eq_ignore_ascii_case("ASIA/TOKYO") {
        return Ok(9 * 3600);
    }

    Err(anyhow!("unknown time zone: {}", zone))
}

fn parse_offset_string_seconds(s: &str) -> Option<i32> {
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

    Some(sign * (hours * 3600 + mins * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timezone_offset_seconds_named_zones() {
        assert_eq!(parse_timezone_offset_seconds("UTC").unwrap(), 0);
        assert_eq!(parse_timezone_offset_seconds("GMT").unwrap(), 0);
        assert_eq!(
            parse_timezone_offset_seconds("Asia/Shanghai").unwrap(),
            8 * 3600
        );
        assert_eq!(parse_timezone_offset_seconds("PRC").unwrap(), 8 * 3600);
        assert_eq!(
            parse_timezone_offset_seconds("America/New_York").unwrap(),
            -5 * 3600
        );
    }

    #[test]
    fn test_parse_timezone_offset_seconds_numeric_offsets() {
        assert_eq!(parse_timezone_offset_seconds("+08:00").unwrap(), 8 * 3600);
        assert_eq!(
            parse_timezone_offset_seconds("-05:00").unwrap(),
            -5 * 3600
        );
        assert_eq!(parse_timezone_offset_seconds("+8").unwrap(), 8 * 3600);
        assert_eq!(parse_timezone_offset_seconds("-0").unwrap(), 0);
    }

    #[test]
    fn test_parse_timezone_offset_seconds_unknown() {
        let err = parse_timezone_offset_seconds("No/Such_Zone").unwrap_err();
        assert!(err.to_string().contains("unknown time zone"));
    }
}

