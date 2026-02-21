//! String function registrations (includes OVERLAY, TIMEZONE, and REGEX functions).

use super::FunctionSignature;
use crate::types::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // String functions
    r.register(
        "LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "CHAR_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "CHARACTER_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "OCTET_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "BIT_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "UPPER",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "LOWER",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "INITCAP",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "TRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "BTRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "OVERLAY",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Bytes) => DataType::Bytes,
            _ => DataType::Text,
        })
        .with_args(3, Some(4)),
    );
    r.register(
        "TIMEZONE",
        FunctionSignature::custom(|args| {
            // AT TIME ZONE: result depends on input type
            match args.get(1) {
                Some(DataType::TimestampTz) => DataType::Timestamp,
                Some(DataType::Timestamp) => DataType::TimestampTz,
                _ => DataType::TimestampTz,
            }
        })
        .with_args(2, Some(2)),
    );
    r.register(
        "LTRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "RTRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "LPAD",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "RPAD",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "REPEAT",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "REVERSE",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "REPLACE",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "TRANSLATE",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "CONCAT",
        FunctionSignature::fixed(DataType::Text).with_args(1, None),
    );
    r.register(
        "CONCAT_WS",
        FunctionSignature::fixed(DataType::Text).with_args(2, None),
    );
    r.register(
        "LEFT",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "RIGHT",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "SUBSTRING",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Bytes) => DataType::Bytes,
            _ => DataType::Text,
        })
        .with_args(2, Some(3)),
    );
    r.register(
        "SUBSTR",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Bytes) => DataType::Bytes,
            _ => DataType::Text,
        })
        .with_args(2, Some(3)),
    );
    r.register(
        "SPLIT_PART",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "POSITION",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "STRPOS",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ASCII",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "CHR",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "MD5",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "SHA256",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "ENCODE",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "DECODE",
        FunctionSignature::fixed(DataType::Bytes).with_args(2, Some(2)),
    );
    r.register(
        "FORMAT",
        FunctionSignature::fixed(DataType::Text).with_args(1, None),
    );
    r.register(
        "QUOTE_IDENT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "QUOTE_LITERAL",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "QUOTE_NULLABLE",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "REGEXP_REPLACE",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(4)),
    );
    r.register(
        "REGEXP_MATCH",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );
    r.register(
        "REGEXP_MATCHES",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );
}
