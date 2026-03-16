//! Math function registrations.

use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    let f64 = DataType::Float64;

    // Polymorphic math functions (SameAsArg) — no arg_types
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

    // Float64 functions — known arg types
    r.register(
        "POWER",
        FunctionSignature::fixed(f64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![f64.clone(), f64.clone()]),
    );
    r.register(
        "POW",
        FunctionSignature::fixed(f64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![f64.clone(), f64.clone()]),
    );
    r.register(
        "SQRT",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "CBRT",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "EXP",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "LN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(2))
            .with_arg_types(vec![f64.clone(), f64.clone()]),
    );
    r.register(
        "LOG10",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "DEGREES",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "RADIANS",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "SIN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "COS",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "TAN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "ASIN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "ACOS",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "ATAN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "ATAN2",
        FunctionSignature::fixed(f64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![f64.clone(), f64.clone()]),
    );

    // Int result functions
    r.register(
        "SIGN",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );

    // Zero-arg functions
    r.register(
        "PI",
        FunctionSignature::fixed(f64.clone()).with_args(0, Some(0)),
    );
    r.register(
        "RANDOM",
        FunctionSignature::fixed(f64.clone()).with_args(0, Some(0)),
    );

    // Polymorphic variadic — no arg_types
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
