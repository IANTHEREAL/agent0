//! PostgreSQL COPY text format encoding.

use crate::types::Value;
use chrono::{TimeZone, Utc};

const TAB: u8 = b'\t';
const NEWLINE: u8 = b'\n';
const BACKSLASH: u8 = b'\\';

/// Encode a row of values into PostgreSQL COPY text format.
///
/// Format rules:
/// - Columns separated by TAB
/// - Rows terminated by NEWLINE
/// - NULL represented as `\N`
/// - Special chars escaped: backslash, tab, newline, carriage return
#[inline]
pub fn encode_row(values: &[Value], buf: &mut Vec<u8>) {
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            buf.push(TAB);
        }
        encode_value(value, buf);
    }
    buf.push(NEWLINE);
}

#[inline]
fn encode_value(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => {
            buf.extend_from_slice(b"\\N");
        }
        Value::Boolean(b) => {
            buf.push(if *b { b't' } else { b'f' });
        }
        Value::Int32(i) => {
            let mut tmp = itoa::Buffer::new();
            buf.extend_from_slice(tmp.format(*i).as_bytes());
        }
        Value::Int64(i) => {
            let mut tmp = itoa::Buffer::new();
            buf.extend_from_slice(tmp.format(*i).as_bytes());
        }
        Value::Float64(f) => {
            if f.is_nan() {
                buf.extend_from_slice(b"NaN");
            } else if f.is_infinite() {
                if f.is_sign_positive() {
                    buf.extend_from_slice(b"Infinity");
                } else {
                    buf.extend_from_slice(b"-Infinity");
                }
            } else {
                let mut tmp = ryu::Buffer::new();
                buf.extend_from_slice(tmp.format(*f).as_bytes());
            }
        }
        Value::Text(s) => {
            escape_text(s.as_bytes(), buf);
        }
        Value::Bytes(b) => {
            buf.extend_from_slice(b"\\\\x");
            for byte in b {
                write_hex_byte(*byte, buf);
            }
        }
        Value::Timestamp(ts) => {
            let dt = Utc.timestamp_millis_opt(*ts).single();
            if let Some(dt) = dt {
                let s = dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
                buf.extend_from_slice(s.as_bytes());
            }
        }
        Value::Date(days) => {
            if let Ok(s) = crate::types::date::format_date_days(*days) {
                buf.extend_from_slice(s.as_bytes());
            }
        }
        Value::Interval(iv) => {
            buf.extend_from_slice(iv.to_string().as_bytes());
        }
        Value::Uuid(bytes) => {
            let s = format!(
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                u16::from_be_bytes([bytes[4], bytes[5]]),
                u16::from_be_bytes([bytes[6], bytes[7]]),
                u16::from_be_bytes([bytes[8], bytes[9]]),
                u64::from_be_bytes([
                    0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
                ])
            );
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Time(micros) => {
            let total_secs = *micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            let frac = *micros % 1_000_000;
            let s = if frac > 0 {
                format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, frac)
            } else {
                format!("{:02}:{:02}:{:02}", hours, mins, secs)
            };
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Array(elems) => {
            buf.push(b'{');
            for (i, elem) in elems.iter().enumerate() {
                if i > 0 {
                    buf.push(b',');
                }
                encode_array_element(elem, buf);
            }
            buf.push(b'}');
        }
        Value::Vector(vec) => {
            buf.push(b'[');
            let mut tmp = ryu::Buffer::new();
            for (i, v) in vec.iter().enumerate() {
                if i > 0 {
                    buf.push(b',');
                }
                buf.extend_from_slice(tmp.format(*v).as_bytes());
            }
            buf.push(b']');
        }
        Value::Json(s) | Value::Jsonb(s) => {
            escape_text(s.as_bytes(), buf);
        }
        Value::Numeric(d) => {
            buf.extend_from_slice(d.to_string().as_bytes());
        }
    }
}

#[inline]
fn encode_array_element(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => buf.extend_from_slice(b"NULL"),
        Value::Text(s) => {
            buf.push(b'"');
            for &c in s.as_bytes() {
                match c {
                    b'"' => buf.extend_from_slice(b"\\\""),
                    b'\\' => buf.extend_from_slice(b"\\\\"),
                    _ => buf.push(c),
                }
            }
            buf.push(b'"');
        }
        _ => encode_value(value, buf),
    }
}

#[inline]
fn escape_text(input: &[u8], buf: &mut Vec<u8>) {
    for &c in input {
        match c {
            BACKSLASH => buf.extend_from_slice(b"\\\\"),
            TAB => buf.extend_from_slice(b"\\t"),
            NEWLINE => buf.extend_from_slice(b"\\n"),
            b'\r' => buf.extend_from_slice(b"\\r"),
            _ => buf.push(c),
        }
    }
}

#[inline]
fn write_hex_byte(byte: u8, buf: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf.push(HEX[(byte >> 4) as usize]);
    buf.push(HEX[(byte & 0xf) as usize]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_null() {
        let mut buf = Vec::new();
        encode_row(&[Value::Null], &mut buf);
        assert_eq!(buf, b"\\N\n");
    }

    #[test]
    fn test_encode_basic_types() {
        let mut buf = Vec::new();
        encode_row(
            &[
                Value::Int32(42),
                Value::Boolean(true),
                Value::Text("hello".to_string()),
            ],
            &mut buf,
        );
        assert_eq!(buf, b"42\tt\thello\n");
    }

    #[test]
    fn test_escape_special_chars() {
        let mut buf = Vec::new();
        encode_row(&[Value::Text("a\tb\nc\\d".to_string())], &mut buf);
        assert_eq!(buf, b"a\\tb\\nc\\\\d\n");
    }

    #[test]
    fn test_encode_bytes() {
        let mut buf = Vec::new();
        encode_row(&[Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])], &mut buf);
        assert_eq!(buf, b"\\\\xdeadbeef\n");
    }

    #[test]
    fn test_encode_array() {
        let mut buf = Vec::new();
        encode_row(
            &[Value::Array(vec![
                Value::Int32(1),
                Value::Int32(2),
                Value::Null,
            ])],
            &mut buf,
        );
        assert_eq!(buf, b"{1,2,NULL}\n");
    }

    #[test]
    fn test_encode_text_array() {
        let mut buf = Vec::new();
        encode_row(
            &[Value::Array(vec![
                Value::Text("a".to_string()),
                Value::Text("b\"c".to_string()),
            ])],
            &mut buf,
        );
        assert_eq!(buf, b"{\"a\",\"b\\\"c\"}\n");
    }

    #[test]
    fn test_encode_float_special() {
        let mut buf = Vec::new();
        encode_row(&[Value::Float64(f64::NAN)], &mut buf);
        assert_eq!(buf, b"NaN\n");

        buf.clear();
        encode_row(&[Value::Float64(f64::INFINITY)], &mut buf);
        assert_eq!(buf, b"Infinity\n");

        buf.clear();
        encode_row(&[Value::Float64(f64::NEG_INFINITY)], &mut buf);
        assert_eq!(buf, b"-Infinity\n");
    }
}
