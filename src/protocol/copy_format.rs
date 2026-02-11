//! PostgreSQL COPY format encoding (text and CSV).

use crate::types::Value;
use chrono::{TimeZone, Utc};

const TAB: u8 = b'\t';
const NEWLINE: u8 = b'\n';
const BACKSLASH: u8 = b'\\';

/// COPY format (text or CSV).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Text,
    Csv,
}

/// Parsed COPY options (FORMAT, DELIMITER, NULL, HEADER, QUOTE, ESCAPE).
#[derive(Debug, Clone)]
pub struct CopyOptions {
    pub format: CopyFormat,
    pub delimiter: u8,
    pub null_string: String,
    pub header: bool,
    pub quote: u8,
    pub escape: u8,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            format: CopyFormat::Text,
            delimiter: TAB,
            null_string: "\\N".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        }
    }
}

impl CopyOptions {
    /// Build from sqlparser CopyOption list.
    ///
    /// Returns an error for unsupported formats (e.g. BINARY) or non-ASCII
    /// delimiter/quote/escape characters.
    pub fn from_copy_options(options: &[sqlparser::ast::CopyOption]) -> Result<Self, String> {
        use sqlparser::ast::CopyOption;
        let mut opts = Self::default();
        for opt in options {
            match opt {
                CopyOption::Format(ident) => {
                    let fmt = ident.value.to_uppercase();
                    match fmt.as_str() {
                        "TEXT" => {} // default, nothing to change
                        "CSV" => {
                            opts.format = CopyFormat::Csv;
                            // CSV defaults differ from text
                            if opts.delimiter == TAB {
                                opts.delimiter = b',';
                            }
                            if opts.null_string == "\\N" {
                                opts.null_string = String::new();
                            }
                        }
                        "BINARY" => {
                            return Err("COPY FORMAT binary is not supported".to_string());
                        }
                        other => {
                            return Err(format!("unrecognized COPY FORMAT: \"{}\"", other));
                        }
                    }
                }
                CopyOption::Delimiter(c) => {
                    if !c.is_ascii() {
                        return Err(format!(
                            "COPY delimiter must be a single one-byte character, got: '{}'",
                            c
                        ));
                    }
                    opts.delimiter = *c as u8;
                }
                CopyOption::Null(s) => {
                    opts.null_string = s.clone();
                }
                CopyOption::Header(b) => {
                    opts.header = *b;
                }
                CopyOption::Quote(c) => {
                    if !c.is_ascii() {
                        return Err(format!(
                            "COPY quote must be a single one-byte character, got: '{}'",
                            c
                        ));
                    }
                    opts.quote = *c as u8;
                }
                CopyOption::Escape(c) => {
                    if !c.is_ascii() {
                        return Err(format!(
                            "COPY escape must be a single one-byte character, got: '{}'",
                            c
                        ));
                    }
                    opts.escape = *c as u8;
                }
                _ => {} // Ignore FREEZE, FORCE_QUOTE, etc.
            }
        }
        Ok(opts)
    }
}

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
        encode_value(value, buf, TAB);
    }
    buf.push(NEWLINE);
}

/// Encode a row using the given options (supports text and CSV formats).
#[inline]
pub fn encode_row_with_options(values: &[Value], buf: &mut Vec<u8>, opts: &CopyOptions) {
    match opts.format {
        CopyFormat::Text => {
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    buf.push(opts.delimiter);
                }
                if matches!(value, Value::Null) {
                    buf.extend_from_slice(opts.null_string.as_bytes());
                } else {
                    let start = buf.len();
                    encode_value(value, buf, opts.delimiter);
                    // If the encoded value exactly matches the NULL sentinel,
                    // escape the first byte so it won't be read as NULL on import.
                    if buf[start..] == *opts.null_string.as_bytes() {
                        buf.insert(start, BACKSLASH);
                    }
                }
            }
            buf.push(NEWLINE);
        }
        CopyFormat::Csv => {
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    buf.push(opts.delimiter);
                }
                encode_csv_value(value, buf, opts);
            }
            buf.push(NEWLINE);
        }
    }
}

/// Encode a single value in CSV format with quoting.
fn encode_csv_value(value: &Value, buf: &mut Vec<u8>, opts: &CopyOptions) {
    if matches!(value, Value::Null) {
        buf.extend_from_slice(opts.null_string.as_bytes());
        return;
    }
    // Render value to a temporary buffer, then quote if needed.
    let mut tmp = Vec::new();
    encode_value_raw(value, &mut tmp);
    let needs_quote = tmp == opts.null_string.as_bytes()
        || tmp
            .iter()
            .any(|&c| c == opts.delimiter || c == opts.quote || c == NEWLINE || c == b'\r');
    if needs_quote {
        buf.push(opts.quote);
        for &c in &tmp {
            if c == opts.quote {
                // Double the quote character for escaping.
                if opts.escape == opts.quote {
                    buf.push(opts.quote);
                } else {
                    buf.push(opts.escape);
                }
            }
            buf.push(c);
        }
        buf.push(opts.quote);
    } else {
        buf.extend_from_slice(&tmp);
    }
}

/// Render value to bytes without COPY text escaping (for CSV quoting).
fn encode_value_raw(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => {} // handled by caller
        Value::Boolean(b) => buf.push(if *b { b't' } else { b'f' }),
        Value::Text(s) => buf.extend_from_slice(s.as_bytes()),
        Value::Json(s) | Value::Jsonb(s) => buf.extend_from_slice(s.as_bytes()),
        Value::Tsvector(s) | Value::Tsquery(s) => buf.extend_from_slice(s.as_bytes()),
        Value::Bytes(b) => {
            // CSV uses single-backslash hex (no text-mode double escaping).
            buf.extend_from_slice(b"\\x");
            for byte in b {
                write_hex_byte(*byte, buf);
            }
        }
        _ => {
            // For all other types, use the standard encoder. Numeric/date/etc types
            // don't produce backslashes or special chars, so text escaping is harmless.
            // TAB delimiter is passed because CSV doesn't use this escaping anyway.
            encode_value(value, buf, TAB);
        }
    }
}

#[inline]
fn encode_value(value: &Value, buf: &mut Vec<u8>, delimiter: u8) {
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
            escape_text(s.as_bytes(), buf, delimiter);
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
            escape_text(s.as_bytes(), buf, delimiter);
        }
        Value::Numeric(d) => {
            buf.extend_from_slice(d.to_string().as_bytes());
        }
        Value::Tsvector(s) | Value::Tsquery(s) => {
            escape_text(s.as_bytes(), buf, delimiter);
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
        _ => encode_value(value, buf, TAB),
    }
}

#[inline]
fn escape_text(input: &[u8], buf: &mut Vec<u8>, delimiter: u8) {
    for &c in input {
        match c {
            BACKSLASH => buf.extend_from_slice(b"\\\\"),
            NEWLINE => buf.extend_from_slice(b"\\n"),
            b'\r' => buf.extend_from_slice(b"\\r"),
            TAB => buf.extend_from_slice(b"\\t"),
            _ if c == delimiter => {
                buf.push(BACKSLASH);
                buf.push(c);
            }
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

    #[test]
    fn test_csv_format_basic() {
        let opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(
            &[
                Value::Int32(1),
                Value::Text("hello".to_string()),
                Value::Null,
            ],
            &mut buf,
            &opts,
        );
        assert_eq!(buf, b"1,hello,\n");
    }

    #[test]
    fn test_csv_format_quoting() {
        let opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(
            &[
                Value::Text("a,b".to_string()),
                Value::Text("c\"d".to_string()),
            ],
            &mut buf,
            &opts,
        );
        // "a,b" is quoted because it contains comma; "c""d" has doubled quote
        assert_eq!(buf, b"\"a,b\",\"c\"\"d\"\n");
    }

    #[test]
    fn test_custom_delimiter() {
        let opts = CopyOptions {
            format: CopyFormat::Text,
            delimiter: b'|',
            null_string: "\\N".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(
            &[Value::Int32(1), Value::Int32(2), Value::Null],
            &mut buf,
            &opts,
        );
        assert_eq!(buf, b"1|2|\\N\n");
    }

    #[test]
    fn test_csv_null_string() {
        let opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: "NULL".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(&[Value::Null, Value::Int32(1)], &mut buf, &opts);
        assert_eq!(buf, b"NULL,1\n");
    }

    // --- from_copy_options validation tests ---

    use sqlparser::ast::{CopyOption, Ident};

    #[test]
    fn test_format_text_accepted() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Format(Ident::new("text"))]);
        assert!(opts.is_ok());
        assert_eq!(opts.unwrap().format, CopyFormat::Text);
    }

    #[test]
    fn test_format_csv_accepted() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Format(Ident::new("csv"))]);
        assert!(opts.is_ok());
        let opts = opts.unwrap();
        assert_eq!(opts.format, CopyFormat::Csv);
        assert_eq!(opts.delimiter, b',');
        assert!(opts.null_string.is_empty());
    }

    #[test]
    fn test_format_binary_rejected() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Format(Ident::new("binary"))]);
        assert!(opts.is_err());
        assert!(opts.unwrap_err().contains("binary is not supported"));
    }

    #[test]
    fn test_format_unknown_rejected() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Format(Ident::new("parquet"))]);
        assert!(opts.is_err());
        assert!(opts.unwrap_err().contains("unrecognized COPY FORMAT"));
    }

    #[test]
    fn test_non_ascii_delimiter_rejected() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Delimiter('€')]);
        assert!(opts.is_err());
        assert!(opts
            .unwrap_err()
            .contains("COPY delimiter must be a single one-byte character"));
    }

    #[test]
    fn test_non_ascii_quote_rejected() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Quote('é')]);
        assert!(opts.is_err());
        assert!(opts
            .unwrap_err()
            .contains("COPY quote must be a single one-byte character"));
    }

    #[test]
    fn test_non_ascii_escape_rejected() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Escape('ñ')]);
        assert!(opts.is_err());
        assert!(opts
            .unwrap_err()
            .contains("COPY escape must be a single one-byte character"));
    }

    #[test]
    fn test_ascii_delimiter_accepted() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Delimiter('|')]);
        assert!(opts.is_ok());
        assert_eq!(opts.unwrap().delimiter, b'|');
    }

    #[test]
    fn test_ascii_quote_accepted() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Quote('\'')]);
        assert!(opts.is_ok());
        assert_eq!(opts.unwrap().quote, b'\'');
    }

    #[test]
    fn test_ascii_escape_accepted() {
        let opts = CopyOptions::from_copy_options(&[CopyOption::Escape('\\')]);
        assert!(opts.is_ok());
        assert_eq!(opts.unwrap().escape, b'\\');
    }

    // --- #630: CSV NULL vs empty string disambiguation ---

    #[test]
    fn test_csv_null_vs_empty_string() {
        // With default CSV NULL '' : NULL → unquoted empty, '' → quoted empty
        let opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(&[Value::Null, Value::Text(String::new())], &mut buf, &opts);
        // NULL=unquoted empty, empty string=quoted ""
        assert_eq!(buf, b",\"\"\n");
    }

    #[test]
    fn test_csv_value_equals_null_string_is_quoted() {
        // With NULL 'NULL': literal "NULL" must be quoted to distinguish from NULL
        let opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: "NULL".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(
            &[Value::Null, Value::Text("NULL".to_string())],
            &mut buf,
            &opts,
        );
        // NULL=unquoted NULL, literal "NULL"=quoted "NULL"
        assert_eq!(buf, b"NULL,\"NULL\"\n");
    }

    #[test]
    fn test_csv_bytes_no_double_backslash() {
        // CSV mode should emit \x... not \\x...
        let opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(&[Value::Bytes(vec![0xde, 0xad])], &mut buf, &opts);
        assert_eq!(buf, b"\\xdead\n");
    }

    // --- #631: Text mode custom delimiter escaping + NULL collision ---

    #[test]
    fn test_text_custom_delimiter_escaped_in_value() {
        // With DELIMITER '|', a pipe in a value must be escaped as \|
        let opts = CopyOptions {
            format: CopyFormat::Text,
            delimiter: b'|',
            null_string: "\\N".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(&[Value::Text("a|b".to_string())], &mut buf, &opts);
        assert_eq!(buf, b"a\\|b\n");
    }

    #[test]
    fn test_text_null_string_collision_escaped() {
        // With NULL 'NULL', literal text "NULL" must be escaped to avoid collision
        let opts = CopyOptions {
            format: CopyFormat::Text,
            delimiter: b'\t',
            null_string: "NULL".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let mut buf = Vec::new();
        encode_row_with_options(
            &[Value::Null, Value::Text("NULL".to_string())],
            &mut buf,
            &opts,
        );
        // NULL emits "NULL", literal "NULL" emits "\NULL" (escaped first char)
        assert_eq!(buf, b"NULL\t\\NULL\n");
    }

    #[test]
    fn test_text_default_null_no_collision() {
        // With default NULL '\N', literal "\N" is already escaped as "\\N"
        let opts = CopyOptions::default();
        let mut buf = Vec::new();
        encode_row_with_options(
            &[Value::Null, Value::Text("\\N".to_string())],
            &mut buf,
            &opts,
        );
        // NULL emits \N, literal "\N" is escaped to \\N — no collision
        assert_eq!(buf, b"\\N\t\\\\N\n");
    }
}
