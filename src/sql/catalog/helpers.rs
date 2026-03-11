use crate::model::{ColumnDef, DataType, IndexDef, Value};
use std::borrow::Cow;
use std::collections::HashMap;

// --- pg_constraint.contype constants ---
pub const CONTYPE_PRIMARY_KEY: &str = "p";
pub const CONTYPE_UNIQUE: &str = "u";
pub const CONTYPE_CHECK: &str = "c";
pub const CONTYPE_FOREIGN_KEY: &str = "f";

// --- pg_class.relkind constants ---
pub const RELKIND_TABLE: &str = "r";
pub const RELKIND_INDEX: &str = "i";
pub const RELKIND_SEQUENCE: &str = "S";
pub const RELKIND_VIEW: &str = "v";
pub const RELKIND_MATVIEW: &str = "m";

// --- pg_class column defaults ---
/// Bootstrap superuser OID used as `relowner`.
pub const BOOTSTRAP_SUPERUSER_OID: i64 = 10;
/// Default replica identity.
pub const RELREPLIDENT_DEFAULT: &str = "d";
/// Btree access-method OID (used for PK indexes).
pub const AM_BTREE_OID: i64 = 403;

pub fn text_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Text,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn name_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Name,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn int_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Int64,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn float_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Float64,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

/// PostgreSQL `"char"` — single-byte internal character type.
pub fn char_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::UserDefined("char".to_string()),
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn bool_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Boolean,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn int2vector_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::UserDefined("int2vector".to_string()),
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn oidvector_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::UserDefined("oidvector".to_string()),
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn int_array_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Array(Box::new(DataType::Int64)),
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn timestamptz_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::TimestampTz,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn text_array_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Array(Box::new(DataType::Text)),
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

pub fn text_val(s: &str) -> Value {
    Value::Text(s.to_string())
}

pub fn null_val() -> Value {
    Value::Null
}

pub fn int_val(i: i64) -> Value {
    Value::Int64(i)
}

pub fn float_val(f: f64) -> Value {
    Value::Float64(f)
}

pub fn split_schema_and_name(full: &str) -> (String, String) {
    match super::super::names::parse_full_name(full) {
        Ok((schema, name)) => (schema, name),
        Err(_) => ("public".to_string(), full.to_string()),
    }
}

/// UNIQUE constraints are represented by a backing unique index that has the
/// constraint bit set. Plain CREATE UNIQUE INDEX entries are not constraints.
pub fn is_unique_constraint_index(idx: &IndexDef) -> bool {
    idx.unique && idx.is_constraint
}

pub fn access_method_oid(method: Option<&str>) -> i64 {
    match method.unwrap_or("btree").to_ascii_lowercase().as_str() {
        "btree" => AM_BTREE_OID,
        "hash" => 405,
        "gist" => 783,
        "gin" => 2742,
        "spgist" => 4000,
        "brin" => 3580,
        _ => AM_BTREE_OID,
    }
}

pub fn access_method_name(method: Option<&str>) -> &str {
    method.unwrap_or("btree")
}

pub fn format_index_columns(idx: &IndexDef) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(idx.columns.iter().cloned());
    // PostgreSQL does not wrap expression-index entries in extra parens.
    parts.extend(idx.expressions.iter().cloned());
    parts.join(", ")
}

pub fn format_indexdef(table_schema: &str, table_name: &str, idx: &IndexDef) -> String {
    let cols = format_index_columns(idx);
    // PostgreSQL's pg_get_indexdef() outputs lowercase keywords.
    let mut indexdef = format!(
        "create {}index {} on {}.{} using {} ({})",
        if idx.unique { "unique " } else { "" },
        idx.name,
        table_schema,
        table_name,
        access_method_name(idx.method.as_deref()),
        cols
    );
    if let Some(pred) = idx.predicate.as_ref() {
        // PostgreSQL wraps the WHERE predicate in parentheses.
        indexdef.push_str(" where (");
        indexdef.push_str(pred);
        indexdef.push(')');
    }
    indexdef
}

pub fn schema_oid(schema_oids: &HashMap<String, u32>, schema: &str) -> i64 {
    schema_oids.get(schema).copied().unwrap_or(2200) as i64
}

pub fn owner_role_oid(owner: Option<&str>, fallback_owner: &str) -> i64 {
    let selected = owner
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let f = fallback_owner.trim();
            if f.is_empty() {
                None
            } else {
                Some(f)
            }
        })
        .unwrap_or("postgres");
    crate::sql::catalog_oids::pg_role_oid(selected)
}

pub fn data_type_to_pg_type(dt: &DataType) -> &'static str {
    match dt {
        DataType::Boolean => "boolean",
        DataType::Int32 => "integer",
        DataType::Int64 => "bigint",
        DataType::Float64 => "double precision",
        DataType::Text => "text",
        DataType::Name => "name",
        DataType::Bytes => "bytea",
        DataType::Timestamp => "timestamp without time zone",
        DataType::TimestampTz => "timestamp with time zone",
        DataType::Date => "date",
        DataType::Interval => "interval",
        DataType::Uuid => "uuid",
        // PostgreSQL reports data_type='ARRAY' for all array columns in
        // information_schema.columns, regardless of element type.
        DataType::Array(_) => "ARRAY",
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
        DataType::Vector(_) => "vector",
        DataType::Time => "time without time zone",
        DataType::UserDefined(_) => "USER-DEFINED",
        DataType::Numeric { .. } => "numeric",
        DataType::Tsvector => "tsvector",
        DataType::Tsquery => "tsquery",
        DataType::Varchar(_) => "character varying",
    }
}

pub fn data_type_to_udt_name(dt: &DataType) -> Cow<'static, str> {
    match dt {
        DataType::Boolean => "bool".into(),
        DataType::Int32 => "int4".into(),
        DataType::Int64 => "int8".into(),
        DataType::Float64 => "float8".into(),
        DataType::Text => "text".into(),
        DataType::Name => "name".into(),
        DataType::Bytes => "bytea".into(),
        DataType::Timestamp => "timestamp".into(),
        DataType::TimestampTz => "timestamptz".into(),
        DataType::Date => "date".into(),
        DataType::Interval => "interval".into(),
        DataType::Uuid => "uuid".into(),
        DataType::Array(inner) => match inner.as_ref() {
            DataType::Int32 => "_int4".into(),
            DataType::Int64 => "_int8".into(),
            DataType::Text => "_text".into(),
            DataType::Name => "_name".into(),
            DataType::Varchar(_) => "_varchar".into(),
            // PostgreSQL uses underscore-prefixed element type name for
            // array UDT names (e.g. mood[] → _mood).
            DataType::UserDefined(full_udt) => {
                let type_name = full_udt
                    .rsplit_once('.')
                    .map(|(_, name)| name)
                    .unwrap_or(full_udt.as_str());
                format!("_{}", type_name).into()
            }
            _ => "_unknown".into(),
        },
        DataType::Json => "json".into(),
        DataType::Jsonb => "jsonb".into(),
        DataType::Vector(_) => "vector".into(),
        DataType::Time => "time".into(),
        DataType::UserDefined(full_udt) => {
            let type_name = full_udt
                .rsplit_once('.')
                .map(|(_, name)| name)
                .unwrap_or(full_udt.as_str());
            type_name.to_string().into()
        }
        DataType::Numeric { .. } => "numeric".into(),
        DataType::Tsvector => "tsvector".into(),
        DataType::Tsquery => "tsquery".into(),
        DataType::Varchar(_) => "varchar".into(),
    }
}

pub fn format_epoch_ms(epoch_ms: i64) -> String {
    let secs = epoch_ms / 1000;
    let nanos = ((epoch_ms % 1000) * 1_000_000) as u32;
    match chrono::DateTime::from_timestamp(secs, nanos) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S%.3f+00").to_string(),
        None => epoch_ms.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{data_type_to_pg_type, data_type_to_udt_name, is_unique_constraint_index};
    use crate::model::{DataType, IndexDef};
    use crate::worker::types::IndexState;

    #[test]
    fn pg_type_for_enum_array_returns_array() {
        let dt = DataType::Array(Box::new(DataType::UserDefined("public.mood".into())));
        assert_eq!(data_type_to_pg_type(&dt), "ARRAY");
    }

    #[test]
    fn pg_type_for_unqualified_enum_array_returns_array() {
        let dt = DataType::Array(Box::new(DataType::UserDefined("mood".into())));
        assert_eq!(data_type_to_pg_type(&dt), "ARRAY");
    }

    #[test]
    fn udt_name_for_enum_array_has_underscore_prefix() {
        let dt = DataType::Array(Box::new(DataType::UserDefined("public.mood".into())));
        assert_eq!(data_type_to_udt_name(&dt).as_ref(), "_mood");
    }

    #[test]
    fn udt_name_for_unqualified_enum_array_has_underscore_prefix() {
        let dt = DataType::Array(Box::new(DataType::UserDefined("mood".into())));
        assert_eq!(data_type_to_udt_name(&dt).as_ref(), "_mood");
    }

    #[test]
    fn pg_type_for_builtin_arrays_returns_array() {
        // PostgreSQL reports data_type='ARRAY' for all array columns in
        // information_schema.columns, regardless of element type.
        assert_eq!(
            data_type_to_pg_type(&DataType::Array(Box::new(DataType::Int32))),
            "ARRAY"
        );
        assert_eq!(
            data_type_to_pg_type(&DataType::Array(Box::new(DataType::Int64))),
            "ARRAY"
        );
        assert_eq!(
            data_type_to_pg_type(&DataType::Array(Box::new(DataType::Text))),
            "ARRAY"
        );
        assert_eq!(
            data_type_to_pg_type(&DataType::Array(Box::new(DataType::Name))),
            "ARRAY"
        );
        assert_eq!(
            data_type_to_pg_type(&DataType::Array(Box::new(DataType::Varchar(255)))),
            "ARRAY"
        );
    }

    #[test]
    fn udt_name_for_builtin_arrays_unchanged() {
        assert_eq!(
            data_type_to_udt_name(&DataType::Array(Box::new(DataType::Int32))).as_ref(),
            "_int4"
        );
        assert_eq!(
            data_type_to_udt_name(&DataType::Array(Box::new(DataType::Int64))).as_ref(),
            "_int8"
        );
        assert_eq!(
            data_type_to_udt_name(&DataType::Array(Box::new(DataType::Text))).as_ref(),
            "_text"
        );
    }

    #[test]
    fn pg_type_for_user_defined_returns_user_defined() {
        let dt = DataType::UserDefined("public.mood".into());
        assert_eq!(data_type_to_pg_type(&dt), "USER-DEFINED");
    }

    #[test]
    fn pg_type_for_unqualified_user_defined_returns_user_defined() {
        let dt = DataType::UserDefined("mood".into());
        assert_eq!(data_type_to_pg_type(&dt), "USER-DEFINED");
    }

    #[test]
    fn udt_name_for_user_defined_extracts_type_name() {
        let dt = DataType::UserDefined("public.mood".into());
        assert_eq!(data_type_to_udt_name(&dt).as_ref(), "mood");
    }

    #[test]
    fn udt_name_for_unqualified_user_defined_returns_name() {
        let dt = DataType::UserDefined("mood".into());
        assert_eq!(data_type_to_udt_name(&dt).as_ref(), "mood");
    }

    #[test]
    fn pg_type_for_internal_user_defined_types() {
        // Internal types like "char", int2vector, oidvector are stored as UserDefined
        assert_eq!(
            data_type_to_pg_type(&DataType::UserDefined("char".into())),
            "USER-DEFINED"
        );
        assert_eq!(
            data_type_to_pg_type(&DataType::UserDefined("int2vector".into())),
            "USER-DEFINED"
        );
        assert_eq!(
            data_type_to_pg_type(&DataType::UserDefined("oidvector".into())),
            "USER-DEFINED"
        );
    }

    #[test]
    fn udt_name_for_internal_user_defined_types() {
        assert_eq!(
            data_type_to_udt_name(&DataType::UserDefined("char".into())).as_ref(),
            "char"
        );
        assert_eq!(
            data_type_to_udt_name(&DataType::UserDefined("int2vector".into())).as_ref(),
            "int2vector"
        );
        assert_eq!(
            data_type_to_udt_name(&DataType::UserDefined("oidvector".into())).as_ref(),
            "oidvector"
        );
    }

    #[test]
    fn unique_constraint_filter_requires_constraint_bit() {
        let plain_unique_index = IndexDef {
            name: "uq_idx".to_string(),
            id: 1,
            columns: vec!["a".to_string()],
            unique: true,
            is_constraint: false,
            method: Some("btree".to_string()),
            predicate: None,
            expressions: vec![],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        assert!(!is_unique_constraint_index(&plain_unique_index));

        let unique_constraint_backing_index = IndexDef {
            is_constraint: true,
            cached_predicate_conjuncts: None,
            ..plain_unique_index
        };
        assert!(is_unique_constraint_index(&unique_constraint_backing_index));
    }
}
