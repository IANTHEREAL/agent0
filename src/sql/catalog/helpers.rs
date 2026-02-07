use crate::types::{ColumnDef, DataType, IndexDef, Value};
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
    parts.extend(idx.expressions.iter().map(|e| format!("({})", e)));
    parts.join(", ")
}

pub fn format_indexdef(table_schema: &str, table_name: &str, idx: &IndexDef) -> String {
    let cols = format_index_columns(idx);
    let mut indexdef = format!(
        "CREATE {}INDEX {} ON {}.{} USING {} ({})",
        if idx.unique { "UNIQUE " } else { "" },
        idx.name,
        table_schema,
        table_name,
        access_method_name(idx.method.as_deref()),
        cols
    );
    if let Some(pred) = idx.predicate.as_ref() {
        indexdef.push_str(" WHERE ");
        indexdef.push_str(pred);
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
        DataType::Text => "character varying",
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
            DataType::Text => "character varying[]",
            DataType::Name => "name[]",
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
    }
}

pub fn data_type_to_udt_name(dt: &DataType) -> &'static str {
    match dt {
        DataType::Boolean => "bool",
        DataType::Int32 => "integer",
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
    }
}
