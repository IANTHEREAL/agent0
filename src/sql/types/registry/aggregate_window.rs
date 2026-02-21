//! Aggregate and window function registrations.

use super::FunctionSignature;
use crate::types::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // Aggregate functions
    r.register(
        "COUNT",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(1))
            .aggregate(),
    );
    r.register(
        "SUM",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Int32) => DataType::Int64,
            Some(DataType::Int64) | Some(DataType::Numeric { .. }) => DataType::Numeric {
                precision: None,
                scale: None,
            },
            Some(DataType::Float64) => DataType::Float64,
            _ => DataType::Numeric {
                precision: None,
                scale: None,
            },
        })
        .with_args(1, Some(1))
        .aggregate(),
    );
    r.register(
        "AVG",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Float64) => DataType::Float64,
            _ => DataType::Numeric {
                precision: None,
                scale: None,
            },
        })
        .with_args(1, Some(1))
        .aggregate(),
    );
    r.register(
        "MIN",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "MAX",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "STRING_AGG",
        FunctionSignature::fixed(DataType::Text)
            .with_args(2, Some(2))
            .aggregate(),
    );
    r.register(
        "ARRAY_AGG",
        FunctionSignature::custom(|args| {
            // INTENTIONAL: unreachable after arg validation (min_args=1)
            DataType::Array(Box::new(args.first().cloned().unwrap_or(DataType::Text)))
        })
        .with_args(1, Some(1))
        .aggregate(),
    );
    r.register(
        "BOOL_AND",
        FunctionSignature::fixed(DataType::Boolean)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "BOOL_OR",
        FunctionSignature::fixed(DataType::Boolean)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "EVERY",
        FunctionSignature::fixed(DataType::Boolean)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "JSONB_AGG",
        FunctionSignature::fixed(DataType::Jsonb)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "JSON_AGG",
        FunctionSignature::fixed(DataType::Json)
            .with_args(1, Some(1))
            .aggregate(),
    );

    // Window functions
    r.register(
        "ROW_NUMBER",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "RANK",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "DENSE_RANK",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "NTILE",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(1, Some(1))
            .window(),
    );
    r.register(
        "LAG",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(3))
            .window(),
    );
    r.register(
        "LEAD",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(3))
            .window(),
    );
    r.register(
        "FIRST_VALUE",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .window(),
    );
    r.register(
        "LAST_VALUE",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .window(),
    );
    r.register(
        "NTH_VALUE",
        FunctionSignature::same_as_arg(0)
            .with_args(2, Some(2))
            .window(),
    );
    r.register(
        "PERCENT_RANK",
        FunctionSignature::fixed(DataType::Float64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "CUME_DIST",
        FunctionSignature::fixed(DataType::Float64)
            .with_args(0, Some(0))
            .window(),
    );
}
