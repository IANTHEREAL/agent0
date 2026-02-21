//! UUID and JSON/JSONB function registrations.

use super::FunctionSignature;
use crate::types::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // UUID functions
    r.register(
        "GEN_RANDOM_UUID",
        FunctionSignature::fixed(DataType::Uuid).with_args(0, Some(0)),
    );
    r.register(
        "UUID_GENERATE_V4",
        FunctionSignature::fixed(DataType::Uuid).with_args(0, Some(0)),
    );
    r.register(
        "UUIDV7",
        FunctionSignature::fixed(DataType::Uuid).with_args(0, Some(0)),
    );

    // JSON/JSONB functions
    r.register(
        "TO_JSON",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(1)),
    );
    r.register(
        "TO_JSONB",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(1)),
    );
    r.register(
        "ROW_TO_JSON",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(2)),
    );
    r.register(
        "JSON_BUILD_OBJECT",
        FunctionSignature::fixed(DataType::Json).with_args(0, None),
    );
    r.register(
        "JSONB_BUILD_OBJECT",
        FunctionSignature::fixed(DataType::Jsonb).with_args(0, None),
    );
    r.register(
        "JSON_BUILD_ARRAY",
        FunctionSignature::fixed(DataType::Json).with_args(0, None),
    );
    r.register(
        "JSONB_BUILD_ARRAY",
        FunctionSignature::fixed(DataType::Jsonb).with_args(0, None),
    );
    r.register(
        "JSON_OBJECT",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(2)),
    );
    r.register(
        "JSONB_OBJECT",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(2)),
    );
    r.register(
        "JSON_ARRAY_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_ARRAY_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "JSON_TYPEOF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_TYPEOF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSON_EXTRACT_PATH",
        FunctionSignature::fixed(DataType::Json).with_args(2, None),
    );
    r.register(
        "JSONB_EXTRACT_PATH",
        FunctionSignature::fixed(DataType::Jsonb).with_args(2, None),
    );
    r.register(
        "JSON_EXTRACT_PATH_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(2, None),
    );
    r.register(
        "JSONB_EXTRACT_PATH_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(2, None),
    );
    r.register(
        "JSONB_SET",
        FunctionSignature::fixed(DataType::Jsonb).with_args(3, Some(4)),
    );
    r.register(
        "JSONB_INSERT",
        FunctionSignature::fixed(DataType::Jsonb).with_args(3, Some(4)),
    );
    r.register(
        "JSONB_PRETTY",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_STRIP_NULLS",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_EXISTS",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_EXISTS_ANY",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_EXISTS_ALL",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_CONTAINS",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_CONTAINED",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );

    // JSON set-returning functions (SRFs)
    r.register(
        "JSONB_OBJECT_KEYS",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSON_OBJECT_KEYS",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_ARRAY_ELEMENTS",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(1)),
    );
    r.register(
        "JSON_ARRAY_ELEMENTS",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_ARRAY_ELEMENTS_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSON_ARRAY_ELEMENTS_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_EACH",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(1)),
    );
    r.register(
        "JSON_EACH",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_EACH_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSON_EACH_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
}
