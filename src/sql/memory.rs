//! Memory-size estimation helpers for tenant aggregate quota accounting.

use crate::model::{Row, Value};

/// Estimate in-memory size of one SQL value (best-effort).
pub fn estimate_value_size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Boolean(_) => 0,
        Value::Int32(_) => 4,
        Value::Int64(_) => 8,
        Value::Float64(_) => 8,
        Value::Text(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Timestamp(_) => 8,
        Value::Interval(_) => std::mem::size_of::<crate::model::IntervalValue>(),
        Value::Uuid(_) => 16,
        Value::Array(arr) => {
            std::mem::size_of::<Vec<Value>>()
                + arr.len() * std::mem::size_of::<Value>()
                + arr.iter().map(estimate_value_size).sum::<usize>()
        }
        Value::Vector(vec) => {
            std::mem::size_of::<Vec<f64>>() + vec.len() * std::mem::size_of::<f64>()
        }
        Value::Json(s) | Value::Jsonb(s) => s.len(),
        Value::Time(_) => 8,
        Value::Date(_) => 4,
        Value::Numeric(_) => 16,
        Value::Tsvector(s) | Value::Tsquery(s) => s.len(),
    }
}

/// Estimate in-memory size of one row (best-effort).
pub fn estimate_row_size(row: &Row) -> usize {
    std::mem::size_of::<Row>()
        + std::mem::size_of::<Vec<Value>>()
        + row.values.len() * std::mem::size_of::<Value>()
        + row.values.iter().map(estimate_value_size).sum::<usize>()
}

/// Estimate size of a `Vec<Value>` payload plus pointed-to value contents.
pub fn estimate_values_payload_size(values: &[Value]) -> usize {
    std::mem::size_of_val(values) + values.iter().map(estimate_value_size).sum::<usize>()
}

/// Estimate size of a `Vec<Value>` allocation plus pointed-to value contents.
pub fn estimate_values_size(values: &[Value]) -> usize {
    std::mem::size_of::<Vec<Value>>() + estimate_values_payload_size(values)
}

/// Estimate size of a binary key stored inside hash sets/maps.
pub fn estimate_key_size(key: &[u8]) -> usize {
    std::mem::size_of::<Vec<u8>>() + key.len()
}
