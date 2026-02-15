//! Single source of truth for mapping sqlparser `DataType` to internal `DataType`.

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    ArrayElemTypeDef, DataType as SqlDataType, ExactNumberInfo, ObjectName, TimezoneInfo,
};

use crate::sql::error::SqlError;
use crate::types::DataType;

#[derive(Debug, Clone, Copy)]
enum UnknownCustomMode {
    /// Unknown custom types are treated as TEXT (DDL compatibility mode).
    Text,
    /// Unknown custom types are preserved as `DataType::UserDefined(name)`.
    UserDefined,
}

/// Strict type mapping used by DDL — validates numeric precision/scale,
/// and treats unknown custom types as Text for backwards compatibility.
pub(crate) fn sql_datatype_to_internal_strict(sql_type: &SqlDataType) -> Result<DataType> {
    sql_datatype_to_internal_impl(sql_type, UnknownCustomMode::Text, true)
}

/// Type mapping used by type inference — preserves user-defined type names
/// and skips numeric validation (inference context, not DDL).
pub(crate) fn sql_datatype_to_internal(sql_type: &SqlDataType) -> Result<DataType> {
    sql_datatype_to_internal_impl(sql_type, UnknownCustomMode::UserDefined, false)
}

fn sql_datatype_to_internal_impl(
    sql_type: &SqlDataType,
    unknown_custom: UnknownCustomMode,
    validate_numeric: bool,
) -> Result<DataType> {
    match sql_type {
        // Boolean
        SqlDataType::Boolean | SqlDataType::Bool => Ok(DataType::Boolean),

        // Integer-ish
        SqlDataType::TinyInt(_)
        | SqlDataType::SmallInt(_)
        | SqlDataType::Int(_)
        | SqlDataType::Integer(_)
        | SqlDataType::Int2(_)
        | SqlDataType::Int4(_)
        | SqlDataType::UnsignedTinyInt(_)
        | SqlDataType::UnsignedSmallInt(_)
        | SqlDataType::UnsignedInt(_)
        | SqlDataType::UnsignedInt2(_)
        | SqlDataType::UnsignedInt4(_)
        | SqlDataType::UnsignedInteger(_) => Ok(DataType::Int32),
        SqlDataType::BigInt(_)
        | SqlDataType::Int8(_)
        | SqlDataType::Int64
        | SqlDataType::UnsignedBigInt(_)
        | SqlDataType::UnsignedInt8(_) => Ok(DataType::Int64),

        // Float-ish
        SqlDataType::Real
        | SqlDataType::Float4
        | SqlDataType::Float8
        | SqlDataType::Float64
        | SqlDataType::Double
        | SqlDataType::DoublePrecision
        | SqlDataType::Float(_) => Ok(DataType::Float64),

        // NUMERIC/DECIMAL
        SqlDataType::Numeric(info)
        | SqlDataType::Decimal(info)
        | SqlDataType::Dec(info)
        | SqlDataType::BigNumeric(info)
        | SqlDataType::BigDecimal(info) => {
            let (precision, scale) = numeric_precision_and_scale(info);
            if validate_numeric {
                validate_numeric_spec(precision, scale)?;
            }
            Ok(DataType::Numeric { precision, scale })
        }

        // Text-like
        SqlDataType::Text
        | SqlDataType::String(_)
        | SqlDataType::Varchar(_)
        | SqlDataType::Nvarchar(_)
        | SqlDataType::Character(_)
        | SqlDataType::Char(_)
        | SqlDataType::CharacterVarying(_)
        | SqlDataType::CharVarying(_)
        | SqlDataType::CharacterLargeObject(_)
        | SqlDataType::CharLargeObject(_)
        | SqlDataType::Clob(_) => Ok(DataType::Text),

        // Bytes-like
        SqlDataType::Bytea
        | SqlDataType::Binary(_)
        | SqlDataType::Varbinary(_)
        | SqlDataType::Blob(_)
        | SqlDataType::Bytes(_) => Ok(DataType::Bytes),

        // Temporal
        SqlDataType::Timestamp(_, tz) => match tz {
            TimezoneInfo::WithTimeZone | TimezoneInfo::Tz => Ok(DataType::TimestampTz),
            _ => Ok(DataType::Timestamp),
        },
        SqlDataType::Datetime(_) => Ok(DataType::Timestamp),
        SqlDataType::Date => Ok(DataType::Date),
        SqlDataType::Time(_, _) => Ok(DataType::Time),
        SqlDataType::Interval => Ok(DataType::Interval),

        // Other scalars
        SqlDataType::Uuid => Ok(DataType::Uuid),
        SqlDataType::JSON => Ok(DataType::Json),

        // Postgres-ish oddballs (best-effort compatibility)
        SqlDataType::Regclass => Ok(DataType::Text),

        // Arrays
        SqlDataType::Array(inner) => match inner {
            ArrayElemTypeDef::AngleBracket(inner_type)
            | ArrayElemTypeDef::SquareBracket(inner_type) => Ok(DataType::Array(Box::new(
                sql_datatype_to_internal_impl(inner_type, unknown_custom, validate_numeric)?,
            ))),
            ArrayElemTypeDef::None => Ok(DataType::Array(Box::new(DataType::Text))),
        },

        // Custom types (including pgvector/FTS/user-defined types)
        SqlDataType::Custom(name, modifiers) => {
            convert_custom_type(name, modifiers, unknown_custom)
        }

        // Everything else is currently unsupported by our engine.
        _ => Err(SqlError::Unsupported(format!("Unsupported data type: {:?}", sql_type)).into()),
    }
}

fn numeric_precision_and_scale(info: &ExactNumberInfo) -> (Option<u32>, Option<u32>) {
    match info {
        ExactNumberInfo::None => (None, None),
        // Postgres: NUMERIC(p) implies scale=0
        ExactNumberInfo::Precision(p) => (Some(*p as u32), Some(0)),
        ExactNumberInfo::PrecisionAndScale(p, s) => (Some(*p as u32), Some(*s as u32)),
    }
}

fn validate_numeric_spec(precision: Option<u32>, scale: Option<u32>) -> Result<()> {
    if let Some(p) = precision {
        if p > 28 {
            return Err(anyhow!(
                "NUMERIC precision {} exceeds supported maximum 28",
                p
            ));
        }
    }
    if let Some(s) = scale {
        if s > 28 {
            return Err(anyhow!("NUMERIC scale {} exceeds supported maximum 28", s));
        }
    }
    if let (Some(p), Some(s)) = (precision, scale) {
        if s > p {
            return Err(anyhow!(
                "NUMERIC scale {} must be between 0 and precision {}",
                s,
                p
            ));
        }
    }
    Ok(())
}

fn convert_custom_type(
    name: &ObjectName,
    modifiers: &[String],
    unknown_custom: UnknownCustomMode,
) -> Result<DataType> {
    let Some(last_ident) = name.0.last() else {
        return Ok(DataType::Text);
    };

    let full_name = name
        .0
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".");
    let type_name = last_ident.value.to_uppercase();

    match type_name.as_str() {
        "SERIAL" => Ok(DataType::Int32),
        "BIGSERIAL" => Ok(DataType::Int64),
        "BYTEA" => Ok(DataType::Bytes),
        "JSON" => Ok(DataType::Json),
        "JSONB" => Ok(DataType::Jsonb),
        "TIMESTAMPTZ" => Ok(DataType::TimestampTz),
        "TSVECTOR" => Ok(DataType::Tsvector),
        "TSQUERY" => Ok(DataType::Tsquery),
        "NAME" => Ok(DataType::Name),
        "VECTOR" => {
            // 0 means "any dimension" (bare `vector` without `(N)` modifier).
            // DDL CREATE TABLE with bare `vector` and explicit CAST both use 0;
            // the dimension check in cast.rs skips validation when dim == 0.
            let dim = modifiers
                .first()
                .and_then(|m| m.parse::<u32>().ok())
                .unwrap_or(0);
            Ok(DataType::Vector(dim))
        }
        _ => match unknown_custom {
            UnknownCustomMode::Text => Ok(DataType::Text),
            UnknownCustomMode::UserDefined => Ok(DataType::UserDefined(full_name)),
        },
    }
}
