use crate::types::DataType;
use pgwire::api::Type;

pub(in crate::protocol::handler) fn datatype_to_pgtype(dt: Option<&DataType>) -> Type {
    match dt {
        Some(DataType::Boolean) => Type::BOOL,
        Some(DataType::Int32) => Type::INT4,
        Some(DataType::Int64) => Type::INT8,
        Some(DataType::Float64) => Type::FLOAT8,
        Some(DataType::Timestamp) => Type::TIMESTAMP,
        Some(DataType::TimestampTz) => Type::TIMESTAMPTZ,
        Some(DataType::Date) => Type::DATE,
        Some(DataType::Interval) => Type::INTERVAL,
        Some(DataType::Uuid) => Type::UUID,
        Some(DataType::Bytes) => Type::BYTEA,
        Some(DataType::Json) => Type::JSON,
        Some(DataType::Jsonb) => Type::JSONB,
        Some(DataType::Time) => Type::TIME,
        Some(DataType::Numeric { .. }) => Type::NUMERIC,
        Some(DataType::Name) => Type::NAME,
        Some(DataType::Array(inner)) => match inner.as_ref() {
            DataType::Boolean => Type::BOOL_ARRAY,
            DataType::Int32 => Type::INT4_ARRAY,
            DataType::Int64 => Type::INT8_ARRAY,
            DataType::Float64 => Type::FLOAT8_ARRAY,
            DataType::Text => Type::TEXT_ARRAY,
            DataType::Name => Type::NAME_ARRAY,
            DataType::Timestamp => Type::TIMESTAMP_ARRAY,
            DataType::TimestampTz => Type::TIMESTAMPTZ_ARRAY,
            DataType::Date => Type::DATE_ARRAY,
            DataType::Interval => Type::INTERVAL_ARRAY,
            DataType::Uuid => Type::UUID_ARRAY,
            DataType::Bytes => Type::BYTEA_ARRAY,
            DataType::Json => Type::JSON_ARRAY,
            DataType::Jsonb => Type::JSONB_ARRAY,
            DataType::Time => Type::TIME_ARRAY,
            DataType::Numeric { .. } => Type::NUMERIC_ARRAY,
            _ => Type::TEXT_ARRAY,
        },
        Some(DataType::Tsvector) => Type::TS_VECTOR,
        Some(DataType::Tsquery) => Type::TSQUERY,
        Some(DataType::Vector(_))
        | Some(DataType::Text)
        | Some(DataType::UserDefined(_))
        | None => Type::TEXT,
    }
}

