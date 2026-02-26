use crate::model::{ColumnDef, DataType, IndexDef, Value};
use std::collections::HashMap;

pub fn text_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Text,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
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
        "btree" => 403,
        "hash" => 405,
        "gist" => 783,
        "gin" => 2742,
        "spgist" => 4000,
        "brin" => 3580,
        _ => 403,
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
        DataType::Array(inner) => match inner.as_ref() {
            DataType::Int32 => "integer[]",
            DataType::Int64 => "bigint[]",
            DataType::Text => "text[]",
            DataType::Name => "name[]",
            DataType::Varchar(_) => "character varying[]",
            _ => "anyarray",
        },
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
        DataType::Vector(_) => "vector",
        DataType::Time => "time without time zone",
        DataType::UserDefined(_) => "character varying",
        DataType::Numeric { .. } => "numeric",
        DataType::Tsvector => "tsvector",
        DataType::Tsquery => "tsquery",
        DataType::Varchar(_) => "character varying",
    }
}

pub fn data_type_to_udt_name(dt: &DataType) -> &'static str {
    match dt {
        DataType::Boolean => "bool",
        DataType::Int32 => "int4",
        DataType::Int64 => "int8",
        DataType::Float64 => "float8",
        DataType::Text => "text",
        DataType::Name => "name",
        DataType::Bytes => "bytea",
        DataType::Timestamp => "timestamp",
        DataType::TimestampTz => "timestamptz",
        DataType::Date => "date",
        DataType::Interval => "interval",
        DataType::Uuid => "uuid",
        DataType::Array(inner) => match inner.as_ref() {
            DataType::Int32 => "_int4",
            DataType::Int64 => "_int8",
            DataType::Text => "_text",
            DataType::Name => "_name",
            DataType::Varchar(_) => "_varchar",
            _ => "anyarray",
        },
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
        DataType::Vector(_) => "vector",
        DataType::Time => "time",
        DataType::UserDefined(_) => "text",
        DataType::Numeric { .. } => "numeric",
        DataType::Tsvector => "tsvector",
        DataType::Tsquery => "tsquery",
        DataType::Varchar(_) => "varchar",
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
    use super::is_unique_constraint_index;
    use crate::model::IndexDef;
    use crate::worker::types::IndexState;

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
        };
        assert!(!is_unique_constraint_index(&plain_unique_index));

        let unique_constraint_backing_index = IndexDef {
            is_constraint: true,
            ..plain_unique_index
        };
        assert!(is_unique_constraint_index(&unique_constraint_backing_index));
    }
}
