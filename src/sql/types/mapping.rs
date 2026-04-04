//! Single source of truth for mapping sqlparser `DataType` to internal `DataType`.

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    ArrayElemTypeDef, CharacterLength, DataType as SqlDataType, ExactNumberInfo, ObjectName,
    TimezoneInfo,
};
use std::sync::Arc;
use tikv_client::Transaction;

use crate::model::DataType;
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::storage::TikvStore;

/// Context in which a `DataType::Custom` is being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeResolutionContext {
    /// DDL column definition — serial pseudo-types expand to Int32/Int64.
    DdlColumn,
    /// DDL context where serial names are not special (array element/composite field/etc).
    DdlOther,
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
    let normalized_parts: Vec<String> = name.0.iter().map(names::normalize_ident).collect();
    let full_name = normalized_parts.join(".");
    let is_schema_qualified = normalized_parts.len() > 1;

    let Some(type_name) = normalized_parts.last().map(|s| s.as_str()) else {
        return Err(SqlError::UndefinedObject("type \"\" does not exist".to_string()).into());
    };

    let is_unqualified = normalized_parts.len() == 1;
    let is_pg_catalog = normalized_parts.len() == 2 && normalized_parts[0] == "pg_catalog";

    // Step 1: Serial check — only in DDL column context AND only for unqualified names.
    // Schema-qualified names (e.g. pg_catalog.serial, s1.serial) NEVER trigger serial expansion.
    // PostgreSQL rejects `pg_catalog.serial` with 42704 "type pg_catalog.serial does not exist".
    if context == TypeResolutionContext::DdlColumn && !is_schema_qualified {
        match type_name {
            "smallserial" | "serial2" => return Ok((DataType::Int32, true)),
            "serial" | "serial4" => return Ok((DataType::Int32, true)),
            "bigserial" | "serial8" => return Ok((DataType::Int64, true)),
            _ => {}
        }
    }

    // Step 2: Built-in mapping for unqualified/pg_catalog names — MUST come before catalog
    // to prevent UDTs from shadowing builtins. PostgreSQL's pg_catalog always has precedence
    // over user schemas for builtins like 'jsonb', 'tsvector', etc.
    if is_unqualified || is_pg_catalog {
        if let Some(dt) = convert_custom_builtin(type_name, modifiers) {
            return Ok((dt, false));
        }
    }

    // Step 3: Catalog lookup — UDTs take precedence for schema-qualified names or when
    // builtin mapping didn't match.
    if let Some(resolved) = catalog_resolved {
        return Ok((resolved, false));
    }

    // Step 4: Unknown — raise 42704.
    Err(SqlError::UndefinedObject(format!("type \"{}\" does not exist", full_name)).into())
}

pub(crate) fn normalized_sql_type_name(sql_type: &SqlDataType) -> Option<String> {
    match sql_type {
        SqlDataType::Custom(name, _) => Some(
            name.0
                .iter()
                .map(names::normalize_ident)
                .collect::<Vec<_>>()
                .join("."),
        ),
        SqlDataType::Array(inner) => {
            let inner_name = match inner {
                ArrayElemTypeDef::AngleBracket(inner_type)
                | ArrayElemTypeDef::SquareBracket(inner_type) => {
                    normalized_sql_type_name(inner_type)?
                }
                ArrayElemTypeDef::None => "text".to_string(),
            };
            Some(format!("{}[]", inner_name))
        }
        SqlDataType::Regclass => Some("regclass".to_string()),
        _ => None,
    }
}

pub(crate) fn wrap_undefined_object_for_sql_type(
    sql_type: &SqlDataType,
    err: anyhow::Error,
) -> anyhow::Error {
    if matches!(
        err.downcast_ref::<SqlError>(),
        Some(SqlError::UndefinedObject(_))
    ) {
        if let Some(type_name) = normalized_sql_type_name(sql_type) {
            return SqlError::UndefinedObject(format!("type \"{}\" does not exist", type_name))
                .into();
        }
    }
    err
}

/// Resolve `DataType::Custom` with search_path-aware catalog lookup, then run it
/// through the unified 4-step pipeline.
pub(crate) async fn resolve_custom_type_with_catalog(
    context: TypeResolutionContext,
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    modifiers: &[String],
) -> Result<(DataType, bool)> {
    let catalog_resolved =
        resolve_catalog_udt(store.as_ref(), txn, db_id, name, search_path).await?;
    resolve_custom_type(context, name, modifiers, catalog_resolved)
}

async fn resolve_catalog_udt(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<DataType>> {
    let resolved_type =
        names::resolve_existing_type_name(store, txn, db_id, name, search_path).await?;
    let Some(resolved_type) = resolved_type else {
        return Ok(None);
    };

    // Built-in types still go through step 3 so PostgreSQL built-ins win only
    // after catalog lookup misses user-defined types on the search_path.
    if resolved_type.is_builtin() {
        return Ok(None);
    }

    let full_name = resolved_type.resolved_name().full.clone();
    match store.get_type(txn, db_id, &full_name).await? {
        Some(def) => match def.kind {
            crate::model::UserTypeKind::Enum { .. }
            | crate::model::UserTypeKind::Composite { .. } => {
                Ok(Some(DataType::UserDefined(full_name)))
            }
        },
        None => Ok(None),
    }
}

/// Resolve any SQL data type (including arrays of custom types) with catalog
/// lookup. Handles multi-dimensional arrays by unwinding all Array layers,
/// resolving the leaf type through the unified pipeline, then re-wrapping.
///
/// This is the shared implementation for composite field resolution (DDL) and
/// PREPARE parameter resolution (non-DDL). The `context` parameter controls
/// serial pseudo-type expansion.
pub(crate) async fn resolve_sql_type_with_catalog(
    context: TypeResolutionContext,
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    sql_type: &SqlDataType,
) -> Result<DataType> {
    match sql_type {
        SqlDataType::Custom(name, modifiers) => {
            let (dt, _) = resolve_custom_type_with_catalog(
                context,
                store,
                txn,
                db_id,
                search_path,
                name,
                modifiers,
            )
            .await?;
            Ok(dt)
        }
        SqlDataType::Array(inner) => {
            // Unwrap all Array layers to find the leaf type, then resolve it
            // with catalog lookup. Handles multi-dimensional arrays like mood[][].
            let mut leaf_type = inner;
            let mut array_depth = 1;
            loop {
                match leaf_type {
                    ArrayElemTypeDef::AngleBracket(t) | ArrayElemTypeDef::SquareBracket(t) => {
                        match t.as_ref() {
                            SqlDataType::Array(nested) => {
                                leaf_type = nested;
                                array_depth += 1;
                            }
                            _ => break,
                        }
                    }
                    ArrayElemTypeDef::None => {
                        return sql_datatype_to_internal_strict(&SqlDataType::Array(
                            ArrayElemTypeDef::None,
                        ));
                    }
                }
            }
            let leaf_sql_type = match leaf_type {
                ArrayElemTypeDef::AngleBracket(t) | ArrayElemTypeDef::SquareBracket(t) => {
                    t.as_ref()
                }
                ArrayElemTypeDef::None => {
                    return sql_datatype_to_internal_strict(&SqlDataType::Array(
                        ArrayElemTypeDef::None,
                    ));
                }
            };
            let leaf_dt = match leaf_sql_type {
                SqlDataType::Custom(name, modifiers) => {
                    let (dt, _) = resolve_custom_type_with_catalog(
                        context,
                        store,
                        txn,
                        db_id,
                        search_path,
                        name,
                        modifiers,
                    )
                    .await
                    .map_err(|e| wrap_undefined_object_for_sql_type(sql_type, e))?;
                    dt
                }
                _ => sql_datatype_to_internal_strict(leaf_sql_type)
                    .map_err(|e| wrap_undefined_object_for_sql_type(sql_type, e))?,
            };
            let mut result_dt = leaf_dt;
            for _ in 0..array_depth {
                result_dt = DataType::Array(Box::new(result_dt));
            }
            Ok(result_dt)
        }
        _ => sql_datatype_to_internal_strict(sql_type),
    }
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
        SqlDataType::Regclass => Ok(DataType::UserDefined("pg_catalog.regclass".to_string())),

        // Arrays
        SqlDataType::Array(inner) => match inner {
            ArrayElemTypeDef::AngleBracket(inner_type)
            | ArrayElemTypeDef::SquareBracket(inner_type) => Ok(DataType::Array(Box::new(
                sql_datatype_to_internal_impl(inner_type, validate_numeric)
                    .map_err(|e| wrap_undefined_object_for_sql_type(sql_type, e))?,
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
        "bool" | "boolean" => Some(DataType::Boolean),
        "int" | "integer" | "int4" | "smallint" | "int2" => Some(DataType::Int32),
        "bigint" | "int8" => Some(DataType::Int64),
        "real" | "float4" | "double" | "double precision" | "float8" | "float" => {
            Some(DataType::Float64)
        }
        "text" | "char" | "character" | "bpchar" | "void" => Some(DataType::Text),
        "varchar" | "character varying" => Some(
            modifiers
                .first()
                .and_then(|m| m.parse::<u64>().ok())
                .map(DataType::Varchar)
                .unwrap_or(DataType::Varchar(0)),
        ),
        "numeric" | "decimal" => Some(DataType::Numeric {
            precision: None,
            scale: None,
        }),
        "date" => Some(DataType::Date),
        "time" => Some(DataType::Time),
        "timestamp" | "timestamp without time zone" => Some(DataType::Timestamp),
        "timestamp with time zone" => Some(DataType::TimestampTz),
        "interval" => Some(DataType::Interval),
        "uuid" => Some(DataType::Uuid),
        "bytea" => Some(DataType::Bytes),
        "json" => Some(DataType::Json),
        "jsonb" => Some(DataType::Jsonb),
        "timestamptz" => Some(DataType::TimestampTz),
        "tsvector" => Some(DataType::Tsvector),
        "tsquery" => Some(DataType::Tsquery),
        "name" => Some(DataType::Name),
        "regclass" => Some(DataType::UserDefined("pg_catalog.regclass".to_string())),
        "regtype" => Some(DataType::UserDefined("pg_catalog.regtype".to_string())),
        "oid" => Some(DataType::Oid),
        "int2vector" => Some(DataType::UserDefined("int2vector".to_string())),
        "oidvector" => Some(DataType::UserDefined("oidvector".to_string())),
        "bit" => {
            // BIT(1) → Boolean (common ORM pattern), BIT(n>1) → Bytes.
            // PostgreSQL treats bare `bit` (no modifier) as `bit(1)`.
            let len = modifiers.first().and_then(|m| m.parse::<u32>().ok());
            match len {
                Some(1) | None => Some(DataType::Boolean),
                Some(_) => Some(DataType::Bytes),
            }
        }
        "varbit" | "bit varying" => Some(DataType::Bytes),
        "vector" => {
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
    use super::{resolve_custom_type, sql_datatype_to_internal_strict, TypeResolutionContext};
    use crate::model::DataType;
    use sqlparser::ast::{ArrayElemTypeDef, DataType as SqlDataType, Ident, ObjectName};

    fn quoted_ident(value: &str) -> Ident {
        Ident {
            value: value.to_string(),
            quote_style: Some('"'),
        }
    }

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

    #[test]
    fn regclass_variant_maps_to_pg_catalog_regclass() {
        let ty =
            sql_datatype_to_internal_strict(&SqlDataType::Regclass).expect("regclass should map");
        assert_eq!(ty, DataType::UserDefined("pg_catalog.regclass".to_string()));
    }

    #[test]
    fn custom_regtype_maps_to_pg_catalog_regtype() {
        let name = ObjectName(vec![Ident::new("regtype")]);
        let (ty, is_serial) = resolve_custom_type(TypeResolutionContext::NonDdl, &name, &[], None)
            .expect("regtype should map");
        assert_eq!(ty, DataType::UserDefined("pg_catalog.regtype".to_string()));
        assert!(!is_serial);
    }

    #[test]
    fn quoted_lowercase_builtin_jsonb_resolves() {
        let name = ObjectName(vec![quoted_ident("jsonb")]);
        let (ty, is_serial) = resolve_custom_type(TypeResolutionContext::NonDdl, &name, &[], None)
            .expect("quoted lowercase jsonb should map");
        assert_eq!(ty, DataType::Jsonb);
        assert!(!is_serial);
    }

    #[test]
    fn quoted_uppercase_builtin_jsonb_does_not_resolve() {
        let name = ObjectName(vec![quoted_ident("JSONB")]);
        let err = resolve_custom_type(TypeResolutionContext::NonDdl, &name, &[], None)
            .expect_err("quoted uppercase JSONB must not map");
        assert_eq!(err.to_string(), "type \"JSONB\" does not exist");
    }

    #[test]
    fn quoted_lowercase_pg_catalog_schema_still_maps_builtin() {
        let name = ObjectName(vec![quoted_ident("pg_catalog"), Ident::new("jsonb")]);
        let (ty, is_serial) = resolve_custom_type(TypeResolutionContext::NonDdl, &name, &[], None)
            .expect("quoted lowercase pg_catalog should map builtin");
        assert_eq!(ty, DataType::Jsonb);
        assert!(!is_serial);
    }

    #[test]
    fn quoted_uppercase_pg_catalog_schema_does_not_match_builtin() {
        let name = ObjectName(vec![quoted_ident("PG_CATALOG"), Ident::new("jsonb")]);
        let err = resolve_custom_type(TypeResolutionContext::NonDdl, &name, &[], None)
            .expect_err("quoted uppercase PG_CATALOG must not map builtin");
        assert_eq!(err.to_string(), "type \"PG_CATALOG.jsonb\" does not exist");
    }

    #[test]
    fn quoted_lowercase_serial_expands_in_ddl_column_context() {
        let name = ObjectName(vec![quoted_ident("serial")]);
        let (ty, is_serial) =
            resolve_custom_type(TypeResolutionContext::DdlColumn, &name, &[], None)
                .expect("quoted lowercase serial should expand in DDL column context");
        assert_eq!(ty, DataType::Int32);
        assert!(is_serial);
    }

    #[test]
    fn quoted_uppercase_serial_does_not_expand_in_ddl_column_context() {
        let name = ObjectName(vec![quoted_ident("SERIAL")]);
        let err = resolve_custom_type(TypeResolutionContext::DdlColumn, &name, &[], None)
            .expect_err("quoted uppercase SERIAL must not expand");
        assert_eq!(err.to_string(), "type \"SERIAL\" does not exist");
    }

    #[test]
    fn quoted_lowercase_oid_resolves_but_uppercase_oid_does_not() {
        let lower = ObjectName(vec![quoted_ident("oid")]);
        let (ty, is_serial) = resolve_custom_type(TypeResolutionContext::NonDdl, &lower, &[], None)
            .expect("quoted lowercase oid should map");
        assert_eq!(ty, DataType::Oid);
        assert!(!is_serial);

        let upper = ObjectName(vec![quoted_ident("OID")]);
        let err = resolve_custom_type(TypeResolutionContext::NonDdl, &upper, &[], None)
            .expect_err("quoted uppercase OID must not map");
        assert_eq!(err.to_string(), "type \"OID\" does not exist");
    }
}
