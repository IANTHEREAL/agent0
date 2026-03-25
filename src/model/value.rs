use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

use super::data_type::{decimal_serde, format_vector_pg_text, DataType, IntervalValue};
use super::date;
use anyhow::{anyhow, Result};

/// A single value
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Text(String),
    Bytes(Vec<u8>),
    Timestamp(i64),
    Interval(IntervalValue),
    Uuid([u8; 16]),
    Array(Vec<Value>),
    Vector(Vec<f64>),
    Json(String),
    Jsonb(String),
    Time(i64),
    Date(i32),
    Numeric(#[serde(with = "decimal_serde")] Decimal),
    Tsvector(String),
    Tsquery(String),
}

impl Value {
    pub fn type_display_name(&self) -> String {
        self.data_type()
            .map(|dt| dt.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::Int32(_) => Some(DataType::Int32),
            Value::Int64(_) => Some(DataType::Int64),
            Value::Float64(_) => Some(DataType::Float64),
            Value::Text(_) => Some(DataType::Text),
            Value::Bytes(_) => Some(DataType::Bytes),
            Value::Timestamp(_) => Some(DataType::Timestamp),
            Value::Interval(_) => Some(DataType::Interval),
            Value::Uuid(_) => Some(DataType::Uuid),
            Value::Array(elems) => {
                let elem_type = elems.first().and_then(|v| v.data_type());
                Some(DataType::Array(Box::new(
                    // INTENTIONAL: empty array defaults element type to Text (PG-compatible)
                    elem_type.unwrap_or(DataType::Text),
                )))
            }
            Value::Vector(vec) => Some(DataType::Vector(vec.len() as u32)),
            Value::Json(_) => Some(DataType::Json),
            Value::Jsonb(_) => Some(DataType::Jsonb),
            Value::Time(_) => Some(DataType::Time),
            Value::Date(_) => Some(DataType::Date),
            Value::Numeric(d) => Some(DataType::Numeric {
                precision: None,
                scale: Some(d.scale()),
            }),
            Value::Tsvector(_) => Some(DataType::Tsvector),
            Value::Tsquery(_) => Some(DataType::Tsquery),
        }
    }

    /// Returns the underlying `BYTEA` contents as a borrowed byte slice.
    ///
    /// This is a zero-copy accessor; it does not allocate.
    #[allow(dead_code)] // framework: value conversion API
    pub fn as_bytea(&self) -> Result<&[u8]> {
        match self {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(anyhow!("expected bytea")),
        }
    }

    /// Returns the value as a `uuid::Uuid`.
    ///
    /// This is a cheap conversion (16 bytes); it does not allocate.
    #[allow(dead_code)] // framework: value conversion API
    pub fn as_uuid(&self) -> Result<uuid::Uuid> {
        match self {
            Value::Uuid(bytes) => Ok(uuid::Uuid::from_bytes(*bytes)),
            _ => Err(anyhow!("expected uuid")),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(b) => write!(f, "{}", b),
            Value::Int32(i) => write!(f, "{}", i),
            Value::Int64(i) => write!(f, "{}", i),
            Value::Float64(v) => write!(f, "{}", v),
            Value::Text(s) => write!(f, "{}", s),
            Value::Bytes(b) => write!(f, "{:?}", b),
            Value::Timestamp(ts) => write!(f, "{}", ts),
            Value::Interval(iv) => write!(f, "{}", iv),
            Value::Time(micros) => {
                let total_secs = *micros / 1_000_000;
                let hours = total_secs / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                let frac = *micros % 1_000_000;
                if frac > 0 {
                    write!(f, "{:02}:{:02}:{:02}.{:06}", hours, mins, secs, frac)
                } else {
                    write!(f, "{:02}:{:02}:{:02}", hours, mins, secs)
                }
            }
            Value::Uuid(bytes) => {
                write!(
                    f,
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                    u16::from_be_bytes([bytes[4], bytes[5]]),
                    u16::from_be_bytes([bytes[6], bytes[7]]),
                    u16::from_be_bytes([bytes[8], bytes[9]]),
                    u64::from_be_bytes([
                        0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
                    ])
                )
            }
            Value::Array(elems) => {
                write!(f, "{{")?;
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    match elem {
                        Value::Text(s) => write!(f, "\"{}\"", s.replace('"', "\\\""))?,
                        v => write!(f, "{}", v)?,
                    }
                }
                write!(f, "}}")
            }
            Value::Vector(vec) => write!(f, "{}", format_vector_pg_text(vec)),
            Value::Json(s) => write!(f, "{}", s),
            Value::Jsonb(s) => write!(f, "{}", s),
            Value::Date(days) => match date::format_date_days(*days) {
                Ok(s) => write!(f, "{s}"),
                Err(_) => write!(f, "{days}"),
            },
            Value::Numeric(d) => write!(f, "{}", d),
            Value::Tsvector(s) => write!(f, "{}", s),
            Value::Tsquery(s) => write!(f, "{}", s),
        }
    }
}
