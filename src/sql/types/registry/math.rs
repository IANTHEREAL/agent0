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

    // SIGN per PostgreSQL 16.13: two concrete overloads in pg_proc —
    //   sign(double precision) -> double precision
    //   sign(numeric)          -> numeric
    // Registered as two distinct overloads so:
    //   * pg_catalog.pg_proc emits two rows (matching PG exactly);
    //   * overload resolution picks dp for int/bigint/float inputs
    //     (int-to-dp is implicit in PG) and numeric for numeric inputs;
    //   * numeric input resolves to a bare `numeric` return type —
    //     no typmod leak into view/catalog metadata, matching
    //     PG's behavior for CREATE VIEW v AS SELECT sign(x::numeric(p,s)).
    // Registration order matters: the dp overload is registered first
    // so it wins the tie for integer inputs (PG's numeric-category
    // preference routes int → dp rather than int → numeric).
    let numeric = DataType::Numeric {
        precision: None,
        scale: None,
    };
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
