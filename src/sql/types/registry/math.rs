//! Math function registrations.

use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // Math functions
    r.register(
        "ABS",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "CEIL",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "CEILING",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "FLOOR",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "ROUND",
        FunctionSignature::same_as_arg(0).with_args(1, Some(2)),
    );
    r.register(
        "TRUNC",
        FunctionSignature::same_as_arg(0).with_args(1, Some(2)),
    );
    r.register(
        "TRUNCATE",
        FunctionSignature::same_as_arg(0).with_args(1, Some(2)),
    );
    r.register(
        "MOD",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "POWER",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "POW",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "SQRT",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "CBRT",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "EXP",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "LN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(2)),
    );
    r.register(
        "LOG10",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "SIGN",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "PI",
        FunctionSignature::fixed(DataType::Float64).with_args(0, Some(0)),
    );
    r.register(
        "RANDOM",
        FunctionSignature::fixed(DataType::Float64).with_args(0, Some(0)),
    );
    r.register(
        "DEGREES",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "RADIANS",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "SIN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "COS",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "TAN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ASIN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ACOS",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ATAN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ATAN2",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "GREATEST",
        FunctionSignature::same_as_arg(0).with_args(1, None),
    );
    r.register(
        "LEAST",
        FunctionSignature::same_as_arg(0).with_args(1, None),
    );
    r.register(
        "WIDTH_BUCKET",
        FunctionSignature::fixed(DataType::Int32).with_args(4, Some(4)),
    );
}
