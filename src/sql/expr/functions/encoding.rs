use crate::model::{DataType, Value};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

const MAX_STRING_OUTPUT_BYTES: usize = 1_073_741_823;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("ENCODE", encode);
    map.insert("DECODE", decode);
    map.insert("MD5", md5);
    map.insert("SHA256", sha256);
    map.insert("DIGEST", digest);
}

fn pg_quoted_symbol(ch: char) -> String {
    format!("\"{}\"", ch.escape_default())
}

fn pg_quoted_symbol_str(value: &str) -> String {
    format!("\"{}\"", value.escape_default())
}

fn invalid_parameter_value_error(message: impl Into<String>) -> anyhow::Error {
    SqlError::InvalidParameterValue {
        message: message.into(),
    }
    .into()
}

fn pg_unknown_encoding_error(fmt: &str) -> anyhow::Error {
    invalid_parameter_value_error(format!(
        "unrecognized encoding: {}",
        pg_quoted_symbol_str(fmt)
    ))
}

fn pg_unknown_hash_algorithm_error(algorithm: &str) -> anyhow::Error {
    SqlError::InvalidParameterValue {
        message: format!(
            "Cannot use {}: No such hash algorithm",
            quote_error_arg(algorithm)
        ),
    }
    .into()
}

fn check_output_byte_size(byte_len: usize) -> Result<()> {
    if byte_len > MAX_STRING_OUTPUT_BYTES {
        anyhow::bail!("requested length too large");
    }
    Ok(())
}

fn base64_decoded_output_upper_bound(data: &str) -> usize {
    let non_whitespace_len = data.chars().filter(|ch| !ch.is_ascii_whitespace()).count();
    non_whitespace_len.saturating_add(3) / 4 * 3
}

fn hex_decoded_output_upper_bound(data: &str) -> usize {
    data.chars().filter(|ch| !ch.is_ascii_whitespace()).count() / 2
}

fn pg_base64_value(ch: char) -> Option<u8> {
    match ch {
        'A'..='Z' => Some(ch as u8 - b'A'),
        'a'..='z' => Some(ch as u8 - b'a' + 26),
        '0'..='9' => Some(ch as u8 - b'0' + 52),
        '+' => Some(62),
        '/' => Some(63),
        _ => None,
    }
}

fn pg_decode_base64(data: &str) -> Result<Vec<u8>> {
    let output_len = base64_decoded_output_upper_bound(data);
    check_output_byte_size(output_len)?;

    let mut out = Vec::with_capacity(output_len);
    let mut buf = 0_u32;
    let mut pos = 0;
    let mut end = 0;
    let mut seen_padded_block = false;

    for ch in data.chars() {
        if seen_padded_block {
            match ch {
                ' ' | '\t' | '\n' | '\r' => continue,
                _ => return Err(invalid_parameter_value_error("invalid base64 end sequence")),
            }
        }

        let value = match ch {
            ' ' | '\t' | '\n' | '\r' => continue,
            '=' => {
                if end == 0 {
                    if pos == 2 {
                        end = 1;
                    } else if pos == 3 {
                        end = 2;
                    } else {
                        return Err(invalid_parameter_value_error(
                            "unexpected \"=\" while decoding base64 sequence",
                        ));
                    }
                }
                0
            }
            _ => pg_base64_value(ch).ok_or_else(|| {
                invalid_parameter_value_error(format!(
                    "invalid symbol {} found while decoding base64 sequence",
                    pg_quoted_symbol(ch)
                ))
            })?,
        };

        buf = (buf << 6) + u32::from(value);
        pos += 1;
        if pos == 4 {
            out.push(((buf >> 16) & 255) as u8);
            if end == 0 || end > 1 {
                out.push(((buf >> 8) & 255) as u8);
            }
            if end == 0 || end > 2 {
                out.push((buf & 255) as u8);
            }
            seen_padded_block = end != 0;
            buf = 0;
            pos = 0;
            end = 0;
        }
    }

    if pos != 0 {
        return Err(invalid_parameter_value_error("invalid base64 end sequence"));
    }

    Ok(out)
}

fn pg_decode_hex_error(err: hex::FromHexError) -> anyhow::Error {
    match err {
        hex::FromHexError::InvalidHexCharacter { c, .. } => invalid_parameter_value_error(format!(
            "invalid hexadecimal digit: {}",
            pg_quoted_symbol(c)
        )),
        hex::FromHexError::OddLength => {
            invalid_parameter_value_error("invalid hexadecimal data: odd number of digits")
        }
        hex::FromHexError::InvalidStringLength => {
            invalid_parameter_value_error("invalid hexadecimal data: invalid string length")
        }
    }
}

fn pg_decode_hex(data: &str) -> Result<Vec<u8>> {
    let output_len = hex_decoded_output_upper_bound(data);
    check_output_byte_size(output_len)?;

    let mut hex_input = String::with_capacity(output_len.saturating_mul(2));
    let mut digits_in_pair = 0usize;

    for ch in data.chars() {
        if ch.is_ascii_whitespace() {
            if digits_in_pair == 1 {
                return Err(invalid_parameter_value_error(format!(
                    "invalid hexadecimal digit: {}",
                    pg_quoted_symbol(ch)
                )));
            }
            continue;
        }

        if !ch.is_ascii_hexdigit() {
            return Err(invalid_parameter_value_error(format!(
                "invalid hexadecimal digit: {}",
                pg_quoted_symbol(ch)
            )));
        }

        hex_input.push(ch);
        digits_in_pair = (digits_in_pair + 1) % 2;
    }

    if digits_in_pair == 1 {
        return Err(invalid_parameter_value_error(
            "invalid hexadecimal data: odd number of digits",
        ));
    }

    hex::decode(hex_input).map_err(pg_decode_hex_error)
}

pub fn encode(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("encode requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data = match iter.next().unwrap_or(Value::Null) {
        Value::Bytes(b) => b,
        Value::Text(s) => s.into_bytes(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string().into_bytes(),
    };
    let fmt = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };

    if fmt.eq_ignore_ascii_case("base64") {
        return Ok(Value::Text(pg_encode_base64(&data)));
    }
    if fmt.eq_ignore_ascii_case("hex") {
        return Ok(Value::Text(hex::encode(&data)));
    }
    if fmt.eq_ignore_ascii_case("escape") {
        return Ok(Value::Text(crate::sql::bytea::encode_escape(&data)));
    }

    Err(pg_unknown_encoding_error(&fmt))
}

fn pg_encode_base64(data: &[u8]) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    if encoded.len() <= 76 {
        return encoded;
    }
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76);
    for (idx, chunk) in encoded.as_bytes().chunks(76).enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(std::str::from_utf8(chunk).expect("base64 output is ASCII"));
    }
    out
}

pub fn decode(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("decode requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };

    if fmt.eq_ignore_ascii_case("base64") {
        let bytes = pg_decode_base64(&data)?;
        return Ok(Value::Bytes(bytes));
    }
    if fmt.eq_ignore_ascii_case("hex") {
        let bytes = pg_decode_hex(&data)?;
        return Ok(Value::Bytes(bytes));
    }
    if fmt.eq_ignore_ascii_case("escape") {
        check_output_byte_size(data.len())?;
        return Ok(Value::Bytes(crate::sql::bytea::decode_escape(&data)?));
    }

    Err(pg_unknown_encoding_error(&fmt))
}

pub fn md5(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(format!("{:x}", md5::compute(s.as_bytes())))),
        Some(Value::Bytes(b)) => Ok(Value::Text(format!("{:x}", md5::compute(&b)))),
        Some(Value::Null) => Ok(Value::Null),
        None => Ok(Value::Null),
        Some(v) => Err(SqlError::FunctionNotFound(format!(
            "md5({})",
            v.data_type().unwrap_or(DataType::Unknown).pg_display_name()
        ))
        .into()),
    }
}

pub(crate) fn hash_input_bytes_with_type(
    value: Value,
    data_type: &DataType,
    timezone: &str,
) -> Vec<u8> {
    match value {
        Value::Bytes(bytes) => bytes,
        Value::Text(text) => text.into_bytes(),
        Value::Jsonb(jsonb) => crate::sql::jsonb::format_jsonb_pg_str(&jsonb).into_bytes(),
        Value::Timestamp(ts) => {
            pg_hash_timestamp_text(ts, matches!(data_type, DataType::TimestampTz), timezone)
                .into_bytes()
        }
        other => other.to_string().into_bytes(),
    }
}

fn pg_hash_timestamp_text(ts: i64, is_timestamptz: bool, timezone: &str) -> String {
    let formatted = crate::model::timestamp::format_timestamp_millis(ts, is_timestamptz, timezone)
        .unwrap_or_else(|_| ts.to_string());
    trim_trailing_fractional_zeros(&formatted, is_timestamptz)
}

fn trim_trailing_fractional_zeros(formatted: &str, has_tz_suffix: bool) -> String {
    let suffix_start = if has_tz_suffix {
        formatted[19..]
            .find(['+', '-'])
            .map(|idx| idx + 19)
            .unwrap_or(formatted.len())
    } else {
        formatted.len()
    };
    let (body, suffix) = formatted.split_at(suffix_start);
    let Some(dot_idx) = body.find('.') else {
        return formatted.to_string();
    };
    let trimmed_fraction = body[dot_idx + 1..].trim_end_matches('0');
    if trimmed_fraction.is_empty() {
        format!("{}{}", &body[..dot_idx], suffix)
    } else {
        format!("{}.{}{}", &body[..dot_idx], trimmed_fraction, suffix)
    }
}

fn quote_error_arg(value: &str) -> String {
    format!("\"{}\"", value.escape_default())
}

pub fn sha256(args: Vec<Value>) -> Result<Value> {
    use sha2::{Digest, Sha256};

    if args.len() != 1 {
        return Err(anyhow!("sha256 requires exactly 1 argument"));
    }

    match args.into_iter().next().unwrap_or(Value::Null) {
        Value::Null => Ok(Value::Null),
        Value::Bytes(value) => Ok(Value::Bytes(Sha256::digest(value).to_vec())),
        value => Err(SqlError::FunctionNotFound(format!(
            "sha256({})",
            value
                .data_type()
                .unwrap_or(DataType::Unknown)
                .pg_display_name()
        ))
        .into()),
    }
}

pub fn digest(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("digest requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data_arg = iter.next().unwrap_or(Value::Null);
    let algorithm = match iter.next().unwrap_or(Value::Null) {
        Value::Text(text) => text,
        Value::Null => return Ok(Value::Null),
        other => other.to_string(),
    };
    let data = match data_arg {
        Value::Null => return Ok(Value::Null),
        Value::Text(text) => text.into_bytes(),
        Value::Bytes(bytes) => bytes,
        value => {
            let data_type = value
                .data_type()
                .unwrap_or(DataType::Unknown)
                .pg_display_name();
            return Err(SqlError::FunctionNotFound(format!("digest({data_type}, text)")).into());
        }
    };
    let algorithm_lower = algorithm.to_ascii_lowercase();

    let digest = match algorithm_lower.as_str() {
        "md5" => md5::compute(&data).0.to_vec(),
        "sha1" => {
            use sha1::{Digest as Sha1Digest, Sha1};
            Sha1::digest(&data).to_vec()
        }
        "sha224" => {
            use sha2::{Digest, Sha224};
            Sha224::digest(&data).to_vec()
        }
        "sha256" => {
            use sha2::{Digest, Sha256};
            Sha256::digest(&data).to_vec()
        }
        "sha384" => {
            use sha2::{Digest, Sha384};
            Sha384::digest(&data).to_vec()
        }
        "sha512" => {
            use sha2::{Digest, Sha512};
            Sha512::digest(&data).to_vec()
        }
        _ => return Err(pg_unknown_hash_algorithm_error(&algorithm)),
    };

    Ok(Value::Bytes(digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_base64() {
        let result = encode(vec![
            Value::Text("hello".into()),
            Value::Text("base64".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Text("aGVsbG8=".into()));
        let wrapped = encode(vec![
            Value::Bytes(vec![0; 58]),
            Value::Text("base64".into()),
        ])
        .unwrap();
        assert_eq!(wrapped, Value::Text(format!("{}\nAA==", "A".repeat(76))));
    }

    #[test]
    fn test_encode_hex() {
        let result = encode(vec![
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Value::Text("hex".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Text("deadbeef".into()));
    }

    #[test]
    fn test_decode_base64() {
        let result = decode(vec![
            Value::Text("aGVsbG8=".into()),
            Value::Text("base64".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Bytes(b"hello".to_vec()));
    }

    #[test]
    fn test_decode_hex() {
        let result = decode(vec![
            Value::Text("deadbeef".into()),
            Value::Text("hex".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn test_decode_matches_pg_base64_and_hex_edge_semantics() {
        assert_eq!(
            decode(vec![
                Value::Text("QQ==".into()),
                Value::Text("base64".into()),
            ])
            .unwrap(),
            Value::Bytes(vec![b'A'])
        );
        assert_eq!(
            decode(vec![
                Value::Text("QQ=Q".into()),
                Value::Text("base64".into()),
            ])
            .unwrap(),
            Value::Bytes(vec![b'A'])
        );
        for input in ["QQ==AA==", "QQ==A===", "QQ==AAAA"] {
            assert_eq!(
                decode(vec![
                    Value::Text(input.into()),
                    Value::Text("base64".into())
                ])
                .unwrap_err()
                .to_string(),
                "invalid base64 end sequence"
            );
        }
        assert_eq!(
            decode(vec![Value::Text("\\x61".into()), Value::Text("hex".into())])
                .unwrap_err()
                .to_string(),
            "invalid hexadecimal digit: \"\\\\\""
        );
        assert_eq!(
            decode(vec![Value::Text("AB CD".into()), Value::Text("hex".into())]).unwrap(),
            Value::Bytes(vec![0xab, 0xcd])
        );
        assert_eq!(
            decode(vec![
                Value::Text("AB\nCD".into()),
                Value::Text("hex".into())
            ])
            .unwrap(),
            Value::Bytes(vec![0xab, 0xcd])
        );
        assert_eq!(
            decode(vec![Value::Text("61 62".into()), Value::Text("hex".into())]).unwrap(),
            Value::Bytes(b"ab".to_vec())
        );
        assert_eq!(
            decode(vec![Value::Text(" 61 ".into()), Value::Text("hex".into())]).unwrap(),
            Value::Bytes(vec![0x61])
        );
        assert_eq!(
            decode(vec![Value::Text("6 1".into()), Value::Text("hex".into())])
                .unwrap_err()
                .to_string(),
            "invalid hexadecimal digit: \" \""
        );
        assert_eq!(
            decode(vec![
                Value::Text("QQ=".into()),
                Value::Text("base64".into())
            ])
            .unwrap_err()
            .to_string(),
            "invalid base64 end sequence"
        );
        for input in ["QQ====", "QQ==AA"] {
            assert_eq!(
                decode(vec![
                    Value::Text(input.into()),
                    Value::Text("base64".into())
                ])
                .unwrap_err()
                .to_string(),
                "invalid base64 end sequence"
            );
        }
        assert_eq!(
            decode(vec![
                Value::Text("Q=Q=".into()),
                Value::Text("base64".into())
            ])
            .unwrap_err()
            .to_string(),
            "unexpected \"=\" while decoding base64 sequence"
        );
    }

    #[test]
    fn test_encode_and_decode_keep_pg_unknown_encoding_semantics() {
        let encode_err = encode(vec![
            Value::Bytes(b"abc".to_vec()),
            Value::Text(" hex ".into()),
        ])
        .unwrap_err();
        assert_eq!(encode_err.to_string(), "unrecognized encoding: \" hex \"");
        let encode_sql_err = encode_err
            .downcast_ref::<SqlError>()
            .expect("encode unknown encoding should preserve SQLSTATE");
        assert_eq!(encode_sql_err.sqlstate(), "22023");

        let decode_err = decode(vec![
            Value::Text("616263".into()),
            Value::Text(" hex ".into()),
        ])
        .unwrap_err();
        assert_eq!(decode_err.to_string(), "unrecognized encoding: \" hex \"");
        let decode_sql_err = decode_err
            .downcast_ref::<SqlError>()
            .expect("decode unknown encoding should preserve SQLSTATE");
        assert_eq!(decode_sql_err.sqlstate(), "22023");
    }

    #[test]
    fn test_decode_invalid_inputs_preserve_22023_sqlstate() {
        let hex_err =
            decode(vec![Value::Text("\\x61".into()), Value::Text("hex".into())]).unwrap_err();
        assert_eq!(hex_err.to_string(), "invalid hexadecimal digit: \"\\\\\"");
        let hex_sql_err = hex_err
            .downcast_ref::<SqlError>()
            .expect("decode invalid hex input should preserve SQLSTATE");
        assert_eq!(hex_sql_err.sqlstate(), "22023");
    }

    #[test]
    fn test_decode_output_size_projection_is_bounded() {
        assert_eq!(base64_decoded_output_upper_bound("AAAA"), 3);
        assert_eq!(base64_decoded_output_upper_bound("A A\nA\tA"), 3);
        assert_eq!(hex_decoded_output_upper_bound("AB CD"), 2);
        assert!(check_output_byte_size(MAX_STRING_OUTPUT_BYTES).is_ok());

        let err = check_output_byte_size(MAX_STRING_OUTPUT_BYTES + 1).unwrap_err();
        assert_eq!(err.to_string(), "requested length too large");
    }

    #[test]
    fn test_decode_escape_invalid_octal_preserves_sqlstate() {
        let err = decode(vec![
            Value::Text("\\400".into()),
            Value::Text("escape".into()),
        ])
        .unwrap_err();
        assert_eq!(err.to_string(), "invalid input syntax for type bytea: \"\"");
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("decode escape invalid octal should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "22P02");
    }

    #[test]
    fn test_md5() {
        let result = md5(vec![Value::Text("hello".into())]).unwrap();
        assert_eq!(
            result,
            Value::Text("5d41402abc4b2a76b9719d911017c592".into())
        );
    }

    #[test]
    fn test_sha256() {
        let result = sha256(vec![Value::Bytes(b"hello".to_vec())]).unwrap();
        assert_eq!(
            result,
            Value::Bytes(
                hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
                    .unwrap()
            )
        );
    }

    #[test]
    fn test_sha256_rejects_non_bytea_input() {
        let err = sha256(vec![Value::Text("hello".into())]).unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("sha256(text) should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "42883");
    }

    #[test]
    fn test_pg_hash_timestamp_text_trims_fractional_trailing_zeros() {
        assert_eq!(
            pg_hash_timestamp_text(1_704_164_645_678, false, "UTC"),
            "2024-01-02 03:04:05.678"
        );
        assert_eq!(
            pg_hash_timestamp_text(1_704_164_645_678, true, "America/Los_Angeles"),
            "2024-01-01 19:04:05.678-08"
        );
    }

    #[test]
    fn test_digest_sha256() {
        let result = digest(vec![
            Value::Text("hello".into()),
            Value::Text("sha256".into()),
        ])
        .unwrap();
        assert_eq!(
            result,
            Value::Bytes(
                hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
                    .unwrap()
            )
        );
    }

    #[test]
    fn test_digest_bytea_sha256() {
        let result = digest(vec![
            Value::Bytes(b"hello".to_vec()),
            Value::Text("sha256".into()),
        ])
        .unwrap();
        assert_eq!(
            result,
            Value::Bytes(
                hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
                    .unwrap()
            )
        );
    }

    #[test]
    fn test_digest_unknown_algorithm_keeps_pg_error_shape() {
        let err = digest(vec![
            Value::Text("hello".into()),
            Value::Text(" sha256 ".into()),
        ])
        .unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert_eq!(
            err.to_string(),
            "Cannot use \" sha256 \": No such hash algorithm"
        );
        assert_eq!(sql_err.sqlstate(), "22023");
    }

    #[test]
    fn test_digest_null_data_returns_null_like_pg() {
        assert_eq!(
            digest(vec![Value::Null, Value::Text("sha256".into())]).unwrap(),
            Value::Null
        );
        assert_eq!(
            digest(vec![Value::Null, Value::Text(" sha256 ".into())]).unwrap(),
            Value::Null
        );
    }
}
