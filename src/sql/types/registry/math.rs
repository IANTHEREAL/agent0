//! Math function registrations.

use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    let int32 = DataType::Int32;
    let int64 = DataType::Int64;
    let f64 = DataType::Float64;
    let numeric = DataType::Numeric {
        precision: None,
        scale: None,
    };

    // Polymorphic math functions (SameAsArg) — no arg_types
    r.register(
        "ABS",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "CEIL",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "CEIL",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "CEILING",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "CEILING",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "FLOOR",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "FLOOR",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "ROUND",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "ROUND",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "ROUND",
        FunctionSignature::fixed(numeric.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![numeric.clone(), int32.clone()]),
    );
    r.register(
        "TRUNC",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "TRUNC",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "TRUNC",
        FunctionSignature::fixed(numeric.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![numeric.clone(), int32.clone()]),
    );
    r.register(
        "MOD",
        FunctionSignature::fixed(int32.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![int32.clone(), int32.clone()]),
    );
    r.register(
        "MOD",
        FunctionSignature::fixed(int64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![int32.clone(), int64.clone()]),
    );
    r.register(
        "MOD",
        FunctionSignature::fixed(int64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![int64.clone(), int32.clone()]),
    );
    r.register(
        "MOD",
        FunctionSignature::fixed(int64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![int64.clone(), int64.clone()]),
    );
    r.register(
        "MOD",
        FunctionSignature::fixed(numeric.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![numeric.clone(), numeric.clone()]),
    );

    // Float64 functions — known arg types
    for name in ["POWER", "POW"] {
        r.register(
            name,
            FunctionSignature::fixed(f64.clone())
                .with_args(2, Some(2))
                .with_arg_types(vec![f64.clone(), f64.clone()]),
        );
        r.register(
            name,
            FunctionSignature::fixed(numeric.clone())
                .with_args(2, Some(2))
                .with_arg_types(vec![numeric.clone(), numeric.clone()]),
        );
    }
    r.register(
        "SQRT",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "SQRT",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
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
        "EXP",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "LN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "LN",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(f64.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![f64.clone(), f64.clone()]),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(numeric.clone())
            .with_args(2, Some(2))
            .with_arg_types(vec![numeric.clone(), numeric.clone()]),
    );
    r.register(
        "LOG10",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "LOG10",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
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

    r.register(
        "SIGN",
        FunctionSignature::fixed(f64.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![f64.clone()]),
    );
    r.register(
        "SIGN",
        FunctionSignature::fixed(numeric.clone())
            .with_args(1, Some(1))
            .with_arg_types(vec![numeric.clone()]),
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
    // PG17 publishes two 4-arg width_bucket overloads:
    //   width_bucket(float8,  float8,  float8,  int4) → int4
    //   width_bucket(numeric, numeric, numeric, int4) → int4
    // (plus the unrelated 2-arg threshold-array form). Family selection
    // happens in `coerce_width_bucket_signature` in the analyzer (#2469);
    // we only register the float8 form here. The arg_types drive PREPARE
    // inference for the all-untyped-params case (which the analyzer
    // defaults to float8 anyway).
    //
    // Catalog visibility for the numeric overload via pg_proc tracks #2486
    // (polymorphic / second-overload coverage); not in scope for #2469.
    r.register(
        "WIDTH_BUCKET",
        FunctionSignature::fixed(DataType::Int32)
            .with_args(4, Some(4))
            .with_arg_types(vec![f64.clone(), f64.clone(), f64.clone(), DataType::Int32]),
    );
}
