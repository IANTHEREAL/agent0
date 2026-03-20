//! Single source of truth for mapping sqlparser `DataType` to internal `DataType`.

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    ArrayElemTypeDef, CharacterLength, DataType as SqlDataType, ExactNumberInfo, ObjectName,
    TimezoneInfo,
};

use crate::model::DataType;
use crate::sql::error::SqlError;

/// Context in which a `DataType::Custom` is being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeResolutionContext {
    /// DDL column definition — serial pseudo-types expand to Int32/Int64.
    DdlColumn,
    /// All other contexts — serial names are NOT special.
    NonDdl,
}

/// Unified resolution of `DataType::Custom` through the standard 4-step pipeline.
///
/// Steps:
///   1. Serial check — `DdlColumn` context expands SERIAL/BIGSERIAL to (Int32/Int64, true).
///      `NonDdl`: serial names go through normal resolution (steps 2-4).
///   2. Catalog lookup — if `catalog_resolved` is `Some`, return it.
///   3. Built-in mapping — handles jsonb, tsvector, name, timestamptz, vector, etc.
///   4. Unknown — raise `SqlError` 42704 "type X does not exist".
///
/// Returns `(resolved_type, is_serial)`.
pub(crate) fn resolve_custom_type(
    context: TypeResolutionContext,
    name: &ObjectName,
    modifiers: &[String],
    catalog_resolved: Option<DataType>,
) -> Result<(DataType, bool)> {
    let Some(last_ident) = name.0.last() else {
        return Err(SqlError::UndefinedObject("type \"\" does not exist".to_string()).into());
    };
    let type_name = last_ident.value.to_uppercase();

    // Step 1: Serial check — only in DDL column context.
    if context == TypeResolutionContext::DdlColumn {
        match type_name.as_str() {
            "SERIAL" | "SERIAL4" => return Ok((DataType::Int32, true)),
            "BIGSERIAL" | "SERIAL8" => return Ok((DataType::Int64, true)),
            _ => {}
        }
    }

    // Step 2: Catalog lookup — pre-resolved UDT takes precedence.
    if let Some(resolved) = catalog_resolved {
        return Ok((resolved, false));
    }

    // Step 3: Built-in mapping.
    if let Some(dt) = convert_custom_builtin(&type_name, modifiers) {
        return Ok((dt, false));
    }

    // Step 4: Unknown — raise 42704.
    let full_name = name
        .0
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".");
    Err(SqlError::UndefinedObject(format!("type \"{}\" does not exist", full_name)).into())
}

/// Strict type mapping used by DDL — validates numeric precision/scale.
/// Unknown custom types raise 42704.
pub(crate) fn sql_datatype_to_internal_strict(sql_type: &SqlDataType) -> Result<DataType> {
    sql_datatype_to_internal_impl(sql_type, true)
}

/// Type mapping used by type inference — skips numeric validation.
/// Unknown custom types raise 42704.
pub(crate) fn sql_datatype_to_internal(sql_type: &SqlDataType) -> Result<DataType> {
    sql_datatype_to_internal_impl(sql_type, false)
}

fn sql_datatype_to_internal_impl(
    sql_type: &SqlDataType,
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

        // Text-like (with optional length → Varchar(n))
        SqlDataType::Varchar(Some(CharacterLength::IntegerLength { length, .. }))
        | SqlDataType::CharacterVarying(Some(CharacterLength::IntegerLength { length, .. }))
        | SqlDataType::CharVarying(Some(CharacterLength::IntegerLength { length, .. }))
        | SqlDataType::Character(Some(CharacterLength::IntegerLength { length, .. }))
        | SqlDataType::Char(Some(CharacterLength::IntegerLength { length, .. })) => {
            Ok(DataType::Varchar(*length))
        }
        SqlDataType::Nvarchar(Some(length)) => Ok(DataType::Varchar(*length)),
        SqlDataType::Varchar(None)
        | SqlDataType::CharacterVarying(None)
        | SqlDataType::CharVarying(None)
        | SqlDataType::Nvarchar(None) => Ok(DataType::Varchar(0)),

        SqlDataType::Text
        | SqlDataType::String(_)
        | SqlDataType::Character(_)
        | SqlDataType::Char(_)
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
                sql_datatype_to_internal_impl(inner_type, validate_numeric)?,
            ))),
            ArrayElemTypeDef::None => Ok(DataType::Array(Box::new(DataType::Text))),
        },

        // Custom types — use resolve_custom_type() for full pipeline with catalog.
        // This path handles only built-in Custom aliases; unknown types raise 42704.
        SqlDataType::Custom(name, modifiers) => {
            let (dt, _is_serial) =
                resolve_custom_type(TypeResolutionContext::NonDdl, name, modifiers, None)?;
            Ok(dt)
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
    // PostgreSQL typmod boundary: NUMERIC precision can be up to 1000.
    // Runtime arithmetic is still bounded by the internal decimal implementation.
    const PG_NUMERIC_MAX_PRECISION: u32 = 1000;

    if let Some(p) = precision {
        if p > PG_NUMERIC_MAX_PRECISION {
            return Err(anyhow!(
                "NUMERIC precision {} exceeds supported maximum {}",
                p,
                PG_NUMERIC_MAX_PRECISION
            ));
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

/// Maps a Custom type name to a built-in DataType, if recognized.
/// Returns `None` for unrecognized names (caller decides: catalog lookup or 42704).
/// SERIAL/BIGSERIAL are NOT included — they are pseudo-types handled exclusively
/// by `resolve_custom_type` step 1 in DDL column context.
fn convert_custom_builtin(type_name: &str, modifiers: &[String]) -> Option<DataType> {
    match type_name {
        "BOOL" | "BOOLEAN" => Some(DataType::Boolean),
        "INT" | "INTEGER" | "INT4" | "SMALLINT" | "INT2" => Some(DataType::Int32),
        "BIGINT" | "INT8" => Some(DataType::Int64),
        "REAL" | "FLOAT4" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" | "FLOAT" => {
            Some(DataType::Float64)
        }
        "TEXT" | "CHAR" | "CHARACTER" => Some(DataType::Text),
        "VARCHAR" | "CHARACTER VARYING" => Some(
            modifiers
                .first()
                .and_then(|m| m.parse::<u64>().ok())
                .map(DataType::Varchar)
                .unwrap_or(DataType::Varchar(0)),
        ),
        "NUMERIC" | "DECIMAL" => Some(DataType::Numeric {
            precision: None,
            scale: None,
        }),
        "DATE" => Some(DataType::Date),
        "TIME" => Some(DataType::Time),
        "TIMESTAMP" | "TIMESTAMP WITHOUT TIME ZONE" => Some(DataType::Timestamp),
        "TIMESTAMP WITH TIME ZONE" => Some(DataType::TimestampTz),
        "INTERVAL" => Some(DataType::Interval),
        "UUID" => Some(DataType::Uuid),
        "BYTEA" => Some(DataType::Bytes),
        "JSON" => Some(DataType::Json),
        "JSONB" => Some(DataType::Jsonb),
        "TIMESTAMPTZ" => Some(DataType::TimestampTz),
        "TSVECTOR" => Some(DataType::Tsvector),
        "TSQUERY" => Some(DataType::Tsquery),
        "NAME" => Some(DataType::Name),
        "VECTOR" => {
            // 0 means "any dimension" (bare `vector` without `(N)` modifier).
            // DDL CREATE TABLE with bare `vector` and explicit CAST both use 0;
            // the dimension check in cast.rs skips validation when dim == 0.
            let dim = modifiers
                .first()
                .and_then(|m| m.parse::<u32>().ok())
                .unwrap_or(0);
            Some(DataType::Vector(dim))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::sql_datatype_to_internal_strict;
    use crate::model::DataType;
    use sqlparser::ast::{ArrayElemTypeDef, DataType as SqlDataType};

    #[test]
    fn bare_varchar_preserves_varchar_identity() {
        let ty = sql_datatype_to_internal_strict(&SqlDataType::Varchar(None))
            .expect("bare varchar should map");
        assert_eq!(ty, DataType::Varchar(0));
    }

    #[test]
    fn bare_varchar_array_preserves_varchar_element_type() {
        let ty = sql_datatype_to_internal_strict(&SqlDataType::Array(
            ArrayElemTypeDef::AngleBracket(Box::new(SqlDataType::Varchar(None))),
        ))
        .expect("varchar[] should map");
        assert_eq!(ty, DataType::Array(Box::new(DataType::Varchar(0))));
    }
}
