//! System functions and pg_catalog introspection function registrations.

use super::FunctionSignature;
use crate::types::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // System functions
    r.register(
        "CURRENT_USER",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_SCHEMA",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_DATABASE",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_CATALOG",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "SESSION_USER",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "USER",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "PG_TYPEOF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "VERSION",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_SETTING",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "SET_CONFIG",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "PG_BACKEND_PID",
        FunctionSignature::fixed(DataType::Int32).with_args(0, Some(0)),
    );
    r.register(
        "PG_POSTMASTER_START_TIME",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "HAS_TABLE_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "HAS_SCHEMA_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "HAS_DATABASE_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "PG_TABLE_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "PG_RELATION_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(2)),
    );
    r.register(
        "PG_TOTAL_RELATION_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "PG_SIZE_PRETTY",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "OBJ_DESCRIPTION",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "COL_DESCRIPTION",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "SHOBJ_DESCRIPTION",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );

    // pg_catalog introspection functions (used by ORMs for schema discovery)
    r.register(
        "FORMAT_TYPE",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "PG_GET_INDEXDEF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(3)),
    );
    r.register(
        "PG_GET_CONSTRAINTDEF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "PG_GET_EXPR",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "PG_GET_USERBYID",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "PG_GET_SERIAL_SEQUENCE",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "PG_ENCODING_TO_CHAR",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "PG_COLUMN_SIZE",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "PG_IS_IN_RECOVERY",
        FunctionSignature::fixed(DataType::Boolean).with_args(0, Some(0)),
    );
    r.register(
        "TXID_CURRENT",
        FunctionSignature::fixed(DataType::Int64).with_args(0, Some(0)),
    );
}
