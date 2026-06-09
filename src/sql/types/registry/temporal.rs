//! Date/time function registrations.

use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // Date/time functions
    r.register(
        "NOW",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(1)),
    );
    r.register(
        "CURRENT_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(1)),
    );
    r.register(
        "CURRENT_DATE",
        FunctionSignature::fixed(DataType::Date).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_TIME",
        FunctionSignature::fixed(DataType::Time).with_args(0, Some(0)),
    );
    r.register(
        "LOCALTIME",
        FunctionSignature::fixed(DataType::Time).with_args(0, Some(0)),
    );
    r.register(
        "LOCALTIMESTAMP",
        FunctionSignature::fixed(DataType::Timestamp).with_args(0, Some(0)),
    );
    r.register(
        "DATE",
        FunctionSignature::fixed(DataType::Date).with_args(1, Some(1)),
    );
    r.register(
        "TIME",
        FunctionSignature::fixed(DataType::Time).with_args(1, Some(1)),
    );
    r.register(
        "DATE_TRUNC",
        FunctionSignature::same_as_arg(1).with_args(2, Some(2)),
    );
    r.register(
        "DATE_PART",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "EXTRACT",
        FunctionSignature::fixed(DataType::Numeric {
            precision: None,
            scale: None,
        })
        .with_args(2, Some(2)),
    );
    r.register(
        "AGE",
        FunctionSignature::fixed(DataType::Interval).with_args(1, Some(2)),
    );
    r.register(
        "TO_CHAR",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "TO_DATE",
        FunctionSignature::fixed(DataType::Date).with_args(2, Some(2)),
    );
    r.register(
        "TO_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(1, Some(1)),
    );
    r.register(
        "TO_NUMBER",
        FunctionSignature::fixed(DataType::Numeric {
            precision: None,
            scale: None,
        })
        .with_args(2, Some(2)),
    );
    r.register(
        "MAKE_DATE",
        FunctionSignature::fixed(DataType::Date).with_args(3, Some(3)),
    );
    r.register(
        "MAKE_TIME",
        FunctionSignature::fixed(DataType::Time).with_args(3, Some(3)),
    );
    r.register(
        "MAKE_TIMESTAMP",
        FunctionSignature::fixed(DataType::Timestamp).with_args(6, Some(6)),
    );
    r.register(
        "MAKE_TIMESTAMPTZ",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(6, Some(7)),
    );
    r.register(
        "MAKE_INTERVAL",
        FunctionSignature::fixed(DataType::Interval).with_args(0, Some(7)),
    );
    r.register(
        "CLOCK_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "STATEMENT_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(1)),
    );
    r.register(
        "TRANSACTION_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(1)),
    );
    r.register(
        "TIMEOFDAY",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
}
