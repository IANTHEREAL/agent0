//! Miscellaneous function registrations: array, sequence, conditional, CAST,
//! FTS, bytea, vector, FS9, background SQL, visibility, and GROUPING.

use super::{FunctionSignature, ReturnType};
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // Array functions
    r.register(
        "ARRAY_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_DIMS",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "ARRAY_LOWER",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_UPPER",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_NDIMS",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "ARRAY_POSITION",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(3)),
    );
    r.register(
        "ARRAY_POSITIONS",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Int32))).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_CAT",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_APPEND",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_PREPEND",
        FunctionSignature::same_as_arg(1).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_REMOVE",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_REPLACE",
        FunctionSignature::same_as_arg(0).with_args(3, Some(3)),
    );
    r.register(
        "ARRAY_TO_STRING",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "STRING_TO_ARRAY",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );
    r.register(
        "UNNEST",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Array(inner)) => inner.as_ref().clone(),
            // INTENTIONAL: non-array input to UNNEST — best-effort type inference
            _ => DataType::Text,
        })
        .with_args(1, Some(1)),
    );

    // Sequence functions
    r.register(
        "NEXTVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "CURRVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "SETVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(2, Some(3)),
    );
    r.register(
        "LASTVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(0, Some(0)),
    );

    // Conditional functions
    r.register(
        "COALESCE",
        FunctionSignature {
            min_args: 1,
            max_args: None,
            return_type: ReturnType::FirstNonNull,
            is_aggregate: false,
            is_window: false,
        },
    );
    r.register(
        "NULLIF",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "IFNULL",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "NVL",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "NVL2",
        FunctionSignature::same_as_arg(1).with_args(3, Some(3)),
    );

    // Type conversion (special handling)
    r.register(
        "CAST",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );

    // Background SQL functions (worker engine)
    r.register(
        "PG_BACKGROUND_LAUNCH",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "PG_BACKGROUND_RESULT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );

    // Full-text search functions
    r.register(
        "TO_TSVECTOR",
        FunctionSignature::fixed(DataType::Tsvector).with_args(1, Some(2)),
    );
    r.register(
        "PLAINTO_TSQUERY",
        FunctionSignature::fixed(DataType::Tsquery).with_args(1, Some(2)),
    );
    r.register(
        "TO_TSQUERY",
        FunctionSignature::fixed(DataType::Tsquery).with_args(1, Some(2)),
    );
    r.register(
        "TS_RANK",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(4)),
    );
    r.register(
        "TS_RANK_CD",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(4)),
    );
    r.register(
        "SETWEIGHT",
        FunctionSignature::fixed(DataType::Tsvector).with_args(2, Some(2)),
    );

    // Array functions (additional)
    r.register(
        "CARDINALITY",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "REGEXP_SPLIT_TO_ARRAY",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );

    // Bytea functions
    r.register(
        "GET_BYTE",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "SET_BYTE",
        FunctionSignature::fixed(DataType::Bytes).with_args(3, Some(3)),
    );
    r.register(
        "GET_BIT",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "SET_BIT",
        FunctionSignature::fixed(DataType::Bytes).with_args(3, Some(3)),
    );
    r.register(
        "INT4SEND",
        FunctionSignature::fixed(DataType::Bytes).with_args(1, Some(1)),
    );
    r.register(
        "INT8SEND",
        FunctionSignature::fixed(DataType::Bytes).with_args(1, Some(1)),
    );
    r.register(
        "UUID_SEND",
        FunctionSignature::fixed(DataType::Bytes).with_args(1, Some(1)),
    );
    r.register(
        "BYTEA_STRING_AGG",
        FunctionSignature::fixed(DataType::Bytes)
            .with_args(2, Some(2))
            .aggregate(),
    );

    // Vector functions (extension)
    r.register(
        "VECTOR_DIMS",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "L2_DISTANCE",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "INNER_PRODUCT",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "COSINE_DISTANCE",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );

    // Special internal functions
    r.register(
        "GROUPING",
        FunctionSignature::fixed(DataType::Int32).with_args(1, None),
    );

    // pg_catalog visibility functions (return boolean)
    r.register(
        "PG_TABLE_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "PG_FUNCTION_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "PG_TYPE_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "PG_OPERATOR_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "HAS_SCHEMA_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "HAS_TABLE_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );

    // FS9 filesystem scalar functions
    r.register(
        "FS9_READ",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "FS9_WRITE",
        FunctionSignature::fixed(DataType::Int64).with_args(2, Some(2)),
    );
    r.register(
        "FS9_EXISTS",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "FS9_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "FS9_MTIME",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );

    // Background SQL functions
    r.register(
        "PG_BACKGROUND_LAUNCH",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "PG_BACKGROUND_RESULT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
}
