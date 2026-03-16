//! String function registrations (includes OVERLAY, TIMEZONE, and REGEX functions).

use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    let text = DataType::Text;
    let int = DataType::Int32;

    // String functions — single text arg
    r.register(
        "LENGTH",
        FunctionSignature::fixed(int.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "CHAR_LENGTH",
        FunctionSignature::fixed(int.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "CHARACTER_LENGTH",
        FunctionSignature::fixed(int.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "OCTET_LENGTH",
        FunctionSignature::fixed(int.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "BIT_LENGTH",
        FunctionSignature::fixed(int.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "UPPER",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "LOWER",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "INITCAP",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "REVERSE",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "ASCII",
        FunctionSignature::fixed(int.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "MD5",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "SHA256",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "QUOTE_IDENT",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "QUOTE_LITERAL",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "QUOTE_NULLABLE",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![text.clone()]),
    );
    r.register(
        "CHR",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![int.clone()]),
    );

    // String functions — two text args (with optional second)
    r.register(
        "TRIM",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );
    r.register(
        "BTRIM",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );
    r.register(
        "LTRIM",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );
    r.register(
        "RTRIM",
        FunctionSignature::fixed(text.clone())
            .with_args(1, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );
    r.register(
        "POSITION",
        FunctionSignature::fixed(int.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );
    r.register(
        "STRPOS",
        FunctionSignature::fixed(int.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );
    r.register(
        "DECODE",
        FunctionSignature::fixed(DataType::Bytes)
            .with_args(2, Some(2))
            .with_arg_types(vec![text.clone(), text.clone()]),
    );

    // String functions — text + int
    r.register(
        "LEFT",
        FunctionSignature::fixed(text.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![text.clone(), int.clone()]),
    );
    r.register(
        "RIGHT",
        FunctionSignature::fixed(text.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![text.clone(), int.clone()]),
    );
    r.register(
        "REPEAT",
        FunctionSignature::fixed(text.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![text.clone(), int.clone()]),
    );
    r.register(
        "LPAD",
        FunctionSignature::fixed(text.clone())
            .with_args(2, Some(3))
            .with_arg_types(vec![text.clone(), int.clone(), text.clone()]),
    );
    r.register(
        "RPAD",
        FunctionSignature::fixed(text.clone())
            .with_args(2, Some(3))
            .with_arg_types(vec![text.clone(), int.clone(), text.clone()]),
    );
    r.register(
        "ENCODE",
        FunctionSignature::fixed(text.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![DataType::Bytes, text.clone()]),
    );

    // String functions — three text args
    r.register(
        "REPLACE",
        FunctionSignature::fixed(text.clone())
            .with_args(3, Some(3))
            .with_arg_types(vec![text.clone(), text.clone(), text.clone()]),
    );
    r.register(
        "TRANSLATE",
        FunctionSignature::fixed(text.clone())
            .with_args(3, Some(3))
            .with_arg_types(vec![text.clone(), text.clone(), text.clone()]),
    );
    r.register(
        "SPLIT_PART",
        FunctionSignature::fixed(text.clone())
            .with_args(3, Some(3))
            .with_arg_types(vec![text.clone(), text.clone(), int.clone()]),
    );

    // Regex functions — text args
    r.register(
        "REGEXP_REPLACE",
        FunctionSignature::fixed(text.clone())
            .with_args(3, Some(4))
            .with_arg_types(vec![text.clone(), text.clone(), text.clone(), text.clone()]),
    );
    r.register(
        "REGEXP_MATCH",
        FunctionSignature::fixed(DataType::Array(Box::new(text.clone())))
            .with_args(2, Some(3))
            .with_arg_types(vec![text.clone(), text.clone(), text.clone()]),
    );
    r.register(
        "REGEXP_MATCHES",
        FunctionSignature::fixed(DataType::Array(Box::new(text.clone())))
            .with_args(2, Some(3))
            .with_arg_types(vec![text.clone(), text.clone(), text.clone()]),
    );

    // Variadic functions — no arg_types (can't express variadic)
    r.register(
        "CONCAT",
        FunctionSignature::fixed(text.clone()).with_args(1, None),
    );
    r.register(
        "CONCAT_WS",
        FunctionSignature::fixed(text.clone()).with_args(2, None),
    );
    r.register(
        "FORMAT",
        FunctionSignature::fixed(text.clone()).with_args(1, None),
    );

    // Polymorphic functions — no arg_types (text or bytes)
    r.register(
        "OVERLAY",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Bytes) => DataType::Bytes,
            _ => DataType::Text,
        })
        .with_args(3, Some(4)),
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

    // TIMEZONE — complex type-dependent return
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
}
