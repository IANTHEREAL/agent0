//! Type coercion and promotion rules

use crate::types::DataType;

pub fn type_precedence(dt: &DataType) -> i32 {
    match dt {
        DataType::Boolean => 10,
        DataType::Int32 => 20,
        DataType::Int64 => 30,
        DataType::Numeric { .. } => 40,
        DataType::Float64 => 50,
        DataType::Text => 100,
        DataType::Name => 100,
        DataType::Date => 60,
        DataType::Time => 61,
        DataType::Timestamp => 70,
        DataType::TimestampTz => 71,
        DataType::Interval => 80,
        DataType::Uuid => 90,
        DataType::Bytes => 95,
        DataType::Json => 110,
        DataType::Jsonb => 111,
        DataType::Array(_) => 120,
        DataType::Vector(_) => 130,
        DataType::UserDefined(_) => 200,
        DataType::Tsvector => 140,
        DataType::Tsquery => 141,
    }
}

pub fn is_numeric(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Numeric { .. }
    )
}

#[allow(dead_code)] // type inference module
pub fn is_temporal(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Date
            | DataType::Time
            | DataType::Timestamp
            | DataType::TimestampTz
            | DataType::Interval
    )
}

#[allow(dead_code)] // type inference module
pub fn can_coerce(from: &DataType, to: &DataType) -> bool {
    if from == to {
        return true;
    }

    match (from, to) {
        // Numeric upcast
        (DataType::Int32, DataType::Int64 | DataType::Float64 | DataType::Numeric { .. }) => true,
        (DataType::Int64, DataType::Float64 | DataType::Numeric { .. }) => true,
        (DataType::Numeric { .. }, DataType::Float64) => true,

        // Temporal casts
        (DataType::Date, DataType::Timestamp | DataType::TimestampTz) => true,
        (DataType::Timestamp, DataType::TimestampTz) => true,

        // Text-like types accept most types
        (_, DataType::Text | DataType::Name) => true,

        // JSON compatibility
        (DataType::Json, DataType::Jsonb) => true,
        (DataType::Jsonb, DataType::Json) => true,

        _ => false,
    }
}

pub fn common_type(a: &DataType, b: &DataType) -> Option<DataType> {
    if a == b {
        return Some(a.clone());
    }

    // Numeric type promotion
    if is_numeric(a) && is_numeric(b) {
        return Some(if type_precedence(a) > type_precedence(b) {
            a.clone()
        } else {
            b.clone()
        });
    }

    // Temporal type rules
    match (a, b) {
        (DataType::Timestamp, DataType::TimestampTz)
        | (DataType::TimestampTz, DataType::Timestamp) => Some(DataType::TimestampTz),
        (DataType::Date, DataType::Timestamp) | (DataType::Timestamp, DataType::Date) => {
            Some(DataType::Timestamp)
        }
        (DataType::Date, DataType::TimestampTz) | (DataType::TimestampTz, DataType::Date) => {
            Some(DataType::TimestampTz)
        }

        // JSON rules
        (DataType::Json, DataType::Jsonb) | (DataType::Jsonb, DataType::Json) => {
            Some(DataType::Jsonb)
        }

        // Text-like as universal fallback
        (DataType::Text, _) | (_, DataType::Text) | (DataType::Name, _) | (_, DataType::Name) => {
            Some(DataType::Text)
        }

        _ => None,
    }
}

pub fn unify_types(types: &[DataType]) -> Option<DataType> {
    if types.is_empty() {
        return None;
    }
    let mut result = types[0].clone();
    for t in &types[1..] {
        result = common_type(&result, t)?;
    }
    Some(result)
}

pub fn binary_op_result_type(op: &str, left: &DataType, right: &DataType) -> Option<DataType> {
    match op {
        // Arithmetic operators
        "Plus" | "Minus" | "+" | "-" => {
            // Temporal type special rules
            match (left, right) {
                (DataType::Timestamp, DataType::Interval)
                | (DataType::Interval, DataType::Timestamp) => Some(DataType::Timestamp),
                (DataType::TimestampTz, DataType::Interval)
                | (DataType::Interval, DataType::TimestampTz) => Some(DataType::TimestampTz),
                (DataType::Date, DataType::Interval) | (DataType::Interval, DataType::Date) => {
                    Some(DataType::Timestamp)
                }
                (DataType::Date, DataType::Int32) | (DataType::Int32, DataType::Date) => {
                    Some(DataType::Date)
                }
                (DataType::Interval, DataType::Interval) => Some(DataType::Interval),
                (DataType::Timestamp, DataType::Timestamp) if op == "Minus" || op == "-" => {
                    Some(DataType::Interval)
                }
                (DataType::Date, DataType::Date) if op == "Minus" || op == "-" => {
                    Some(DataType::Int32)
                }
                _ if is_numeric(left) && is_numeric(right) => common_type(left, right),
                _ => None,
            }
        }
        "Multiply" | "Divide" | "Modulo" | "*" | "/" | "%" => {
            if is_numeric(left) && is_numeric(right) {
                common_type(left, right)
            } else if matches!(
                (left, right),
                (DataType::Interval, _) | (_, DataType::Interval)
            ) {
                // interval * number
                Some(DataType::Interval)
            } else {
                None
            }
        }

        // String concatenation
        "StringConcat" | "||" => Some(DataType::Text),

        // Comparison operators
        "Eq" | "NotEq" | "Lt" | "LtEq" | "Gt" | "GtEq" | "=" | "!=" | "<>" | "<" | "<=" | ">"
        | ">=" => Some(DataType::Boolean),

        // Logical operators
        "And" | "Or" | "AND" | "OR" => Some(DataType::Boolean),

        // JSON operators
        "Arrow" | "->" => Some(DataType::Jsonb),
        "LongArrow" | "->>" => Some(DataType::Text),
        "HashArrow" | "#>" => Some(DataType::Jsonb),
        "HashLongArrow" | "#>>" => Some(DataType::Text),
        "AtArrow" | "ArrowAt" | "@>" | "<@" | "?" | "?|" | "?&" => Some(DataType::Boolean),

        // Regex operators (PostgreSQL-specific, always return boolean)
        "PGRegexMatch" | "PGRegexIMatch" | "PGRegexNotMatch" | "PGRegexNotIMatch" | "~" | "~*"
        | "!~" | "!~*" => Some(DataType::Boolean),

        // Array overlap operator
        "PGOverlap" => Some(DataType::Boolean),
        "&&" if matches!(left, DataType::Array(_)) => Some(DataType::Boolean),

        _ => None,
    }
}
