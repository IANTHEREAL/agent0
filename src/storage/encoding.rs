//! Key encoding for TiKV
//!
//! Key layout:
//! - `_sys_next_table_id` -> u64 (auto-incrementing table ID)
//! - `_sys_schema_{table_name}` -> TableSchema (serialized)
//! - `_sys_schemadef_{schema_name}` -> empty (schema catalog entry)
//! - `t_{table_id}_{row_key}` -> Row (serialized)
//! - `i_{table_id}_{index_id}_{index_values}` -> PK (Unique Index)
//! - `i_{table_id}_{index_id}_{index_values}_{pk}` -> Empty (Non-Unique Index)
//!
//! Index keys use memcomparable encoding to preserve lexicographic sort order.

use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{Context, Result};
use memcomparable::Deserializer;

/// System key prefixes
const SYS_NEXT_TABLE_ID: &[u8] = b"_sys_next_table_id";
const SYS_NEXT_TYPE_OID: &[u8] = b"_sys_next_type_oid";
const SYS_SCHEMA_PREFIX: &[u8] = b"_sys_schema_";
const SYS_SCHEMADEF_PREFIX: &[u8] = b"_sys_schemadef_";
const SYS_VIEW_PREFIX: &[u8] = b"_sys_view_";
const SYS_MATVIEW_PREFIX: &[u8] = b"_sys_matview_";
const SYS_PROCEDURE_PREFIX: &[u8] = b"_sys_proc_";
const SYS_FUNCTION_PREFIX: &[u8] = b"_sys_func_";
const SYS_TRIGGER_PREFIX: &[u8] = b"_sys_trigger_";
const SYS_TYPE_PREFIX: &[u8] = b"_sys_type_";
const SYS_SEQUENCE_PREFIX: &[u8] = b"_sys_seqdef_";
const TABLE_DATA_PREFIX: &[u8] = b"t_";
const TABLE_INDEX_PREFIX: &[u8] = b"i_";

/// Encode the system key for next table ID
pub fn encode_next_table_id_key() -> Vec<u8> {
    SYS_NEXT_TABLE_ID.to_vec()
}

/// Encode the system key for next type OID (user-defined types)
pub fn encode_next_type_oid_key() -> Vec<u8> {
    SYS_NEXT_TYPE_OID.to_vec()
}

/// Encode the schema key for a table
pub fn encode_schema_key(table_name: &str) -> Vec<u8> {
    let mut key = SYS_SCHEMA_PREFIX.to_vec();
    key.extend_from_slice(table_name.as_bytes());
    key
}

pub fn encode_schema_def_key(schema_name: &str) -> Vec<u8> {
    let mut key = SYS_SCHEMADEF_PREFIX.to_vec();
    key.extend_from_slice(schema_name.as_bytes());
    key
}

pub fn encode_schema_def_prefix() -> Vec<u8> {
    SYS_SCHEMADEF_PREFIX.to_vec()
}

/// Encode the key for a user-defined type definition.
///
/// `full_name` should be `schema.name` (e.g. `public.role`).
pub fn encode_type_key(full_name: &str) -> Vec<u8> {
    let mut key = SYS_TYPE_PREFIX.to_vec();
    key.extend_from_slice(full_name.as_bytes());
    key
}

pub fn encode_type_prefix() -> Vec<u8> {
    SYS_TYPE_PREFIX.to_vec()
}

/// Encode the key for a sequence definition.
///
/// `full_name` should be `schema.name` (e.g. `public.my_seq`).
pub fn encode_sequence_key(full_name: &str) -> Vec<u8> {
    let mut key = SYS_SEQUENCE_PREFIX.to_vec();
    key.extend_from_slice(full_name.as_bytes());
    key
}

pub fn encode_sequence_prefix() -> Vec<u8> {
    SYS_SEQUENCE_PREFIX.to_vec()
}

pub fn encode_view_key(view_name: &str) -> Vec<u8> {
    let mut key = SYS_VIEW_PREFIX.to_vec();
    key.extend_from_slice(view_name.as_bytes());
    key
}

pub fn encode_view_prefix() -> Vec<u8> {
    SYS_VIEW_PREFIX.to_vec()
}

/// Encode the key for a materialized view definition
pub fn encode_matview_key(matview_name: &str) -> Vec<u8> {
    let mut key = SYS_MATVIEW_PREFIX.to_vec();
    key.extend_from_slice(matview_name.as_bytes());
    key
}

#[allow(dead_code)]
pub fn encode_matview_prefix() -> Vec<u8> {
    SYS_MATVIEW_PREFIX.to_vec()
}

pub fn encode_procedure_key(proc_name: &str) -> Vec<u8> {
    let mut key = SYS_PROCEDURE_PREFIX.to_vec();
    key.extend_from_slice(proc_name.as_bytes());
    key
}

#[allow(dead_code)]
pub fn encode_procedure_prefix() -> Vec<u8> {
    SYS_PROCEDURE_PREFIX.to_vec()
}

/// Encode the key for a function definition.
///
/// `full_name` should be `schema.name` (e.g. `public.last_updated`).
pub fn encode_function_key(full_name: &str) -> Vec<u8> {
    let mut key = SYS_FUNCTION_PREFIX.to_vec();
    key.extend_from_slice(full_name.as_bytes());
    key
}

#[allow(dead_code)]
pub fn encode_function_prefix() -> Vec<u8> {
    SYS_FUNCTION_PREFIX.to_vec()
}

/// Encode the key for a trigger definition.
///
/// Triggers are keyed by `<table_full_name>/<trigger_name>` to avoid collisions
/// between tables (PostgreSQL trigger names are scoped to a table).
pub fn encode_trigger_key(table_full_name: &str, trigger_name: &str) -> Vec<u8> {
    let mut key = SYS_TRIGGER_PREFIX.to_vec();
    key.extend_from_slice(table_full_name.as_bytes());
    key.push(b'/');
    key.extend_from_slice(trigger_name.as_bytes());
    key
}

#[allow(dead_code)]
pub fn encode_trigger_prefix() -> Vec<u8> {
    SYS_TRIGGER_PREFIX.to_vec()
}

#[allow(dead_code)]
pub fn encode_trigger_table_prefix(table_full_name: &str) -> Vec<u8> {
    let mut key = SYS_TRIGGER_PREFIX.to_vec();
    key.extend_from_slice(table_full_name.as_bytes());
    key.push(b'/');
    key
}

/// Encode a data key for a row
pub fn encode_data_key(table_id: u64, row_key: &[u8]) -> Vec<u8> {
    let mut key = TABLE_DATA_PREFIX.to_vec();
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(row_key);
    key
}

/// Encode an index key using memcomparable format for correct sort order.
/// If pk is None, it's a unique index key (Value -> PK)
/// If pk is Some, it's a non-unique index key (Value+PK -> Empty)
pub fn encode_index_key(
    table_id: u64,
    index_id: u64,
    values: &[Value],
    pk: Option<&[Value]>,
) -> Vec<u8> {
    let mut key = TABLE_INDEX_PREFIX.to_vec();
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&index_id.to_be_bytes());
    key.push(b'_');

    for value in values {
        encode_value_memcomparable(value, &mut key);
    }

    if let Some(pk_values) = pk {
        key.push(0x01); // Separator byte (not '_' to avoid collision with encoded data)
        for value in pk_values {
            encode_value_memcomparable(value, &mut key);
        }
    }
    key
}

const NULL_TAG: u8 = 0x00;
const NOT_NULL_TAG: u8 = 0x01;

fn encode_value_memcomparable(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => {
            buf.push(NULL_TAG);
        }
        Value::Boolean(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Int32(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Int64(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Float64(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Text(s) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(s).unwrap());
        }
        Value::Bytes(b) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(b).unwrap());
        }
        Value::Timestamp(ts) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(ts).unwrap());
        }
        Value::Interval(i) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(i).unwrap());
        }
        Value::Time(t) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(t).unwrap());
        }
        Value::Date(d) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(d).unwrap());
        }
        Value::Uuid(bytes) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&bytes.to_vec()).unwrap());
        }
        Value::Array(arr) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&(arr.len() as u32)).unwrap());
            for elem in arr {
                encode_value_memcomparable(elem, buf);
            }
        }
        Value::Vector(vec) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&(vec.len() as u32)).unwrap());
            for f in vec {
                buf.extend(memcomparable::to_vec(f).unwrap());
            }
        }
        Value::Json(s) | Value::Jsonb(s) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(s).unwrap());
        }
    }
}

pub fn decode_value_memcomparable(data: &[u8], data_type: &DataType) -> Result<(Value, usize)> {
    if data.is_empty() {
        anyhow::bail!("Empty data for memcomparable decode");
    }

    if data[0] == NULL_TAG {
        return Ok((Value::Null, 1));
    }

    let payload = &data[1..];
    let mut deserializer = Deserializer::new(payload);

    let (value, consumed) = match data_type {
        DataType::Boolean => {
            let v: bool = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Boolean(v), deserializer.position())
        }
        DataType::Int32 => {
            let v: i32 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Int32(v), deserializer.position())
        }
        DataType::Int64 => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Int64(v), deserializer.position())
        }
        DataType::Float64 => {
            let v: f64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Float64(v), deserializer.position())
        }
        DataType::Text | DataType::UserDefined(_) => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Text(v), deserializer.position())
        }
        DataType::Bytes => {
            let v: Vec<u8> = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Bytes(v), deserializer.position())
        }
        DataType::Timestamp => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Timestamp(v), deserializer.position())
        }
        DataType::Interval => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Interval(v), deserializer.position())
        }
        DataType::Time => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Time(v), deserializer.position())
        }
        DataType::Date => {
            let v: i32 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Date(v), deserializer.position())
        }
        DataType::Uuid => {
            let v: Vec<u8> = serde::Deserialize::deserialize(&mut deserializer)?;
            if v.len() < 16 {
                anyhow::bail!("UUID decode: expected 16 bytes, got {}", v.len());
            }
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&v[..16]);
            (Value::Uuid(bytes), deserializer.position())
        }
        DataType::Json => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Json(v), deserializer.position())
        }
        DataType::Jsonb => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Jsonb(v), deserializer.position())
        }
        DataType::Array(_) | DataType::Vector(_) => {
            anyhow::bail!("Array/Vector decoding not supported in index keys");
        }
    };

    Ok((value, 1 + consumed))
}

/// Decode PK values from a non-unique index key suffix.
/// `pk_bytes` should be the portion after the separator byte (0x01).
/// `pk_types` describes the data types of each PK column.
#[allow(dead_code)]
pub fn decode_pk_from_index_suffix(pk_bytes: &[u8], pk_types: &[DataType]) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(pk_types.len());
    let mut offset = 0;
    for data_type in pk_types {
        let (value, consumed) = decode_value_memcomparable(&pk_bytes[offset..], data_type)?;
        values.push(value);
        offset += consumed;
    }
    Ok(values)
}

/// Get the key range for scanning all rows of a table
pub fn encode_table_data_range(table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut start = TABLE_DATA_PREFIX.to_vec();
    start.extend_from_slice(&table_id.to_be_bytes());
    start.push(b'_');

    let mut end = TABLE_DATA_PREFIX.to_vec();
    end.extend_from_slice(&(table_id + 1).to_be_bytes());

    (start, end)
}

/// Get the raw prefix for schema keys (for scanning all tables)
pub fn encode_schema_prefix() -> Vec<u8> {
    SYS_SCHEMA_PREFIX.to_vec()
}

/// Encode primary key values using memcomparable format for correct sort order.
pub fn encode_pk_values(values: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    for value in values {
        encode_value_memcomparable(value, &mut buf);
    }
    buf
}

/// Serialize a table schema
pub fn serialize_schema(schema: &TableSchema) -> Result<Vec<u8>> {
    bincode::serialize(schema).context("Failed to serialize schema")
}

/// Deserialize a table schema
pub fn deserialize_schema(data: &[u8]) -> Result<TableSchema> {
    bincode::deserialize(data).context("Failed to deserialize schema")
}

/// Serialize a row
pub fn serialize_row(row: &Row) -> Result<Vec<u8>> {
    bincode::serialize(row).context("Failed to serialize row")
}

/// Deserialize a row
pub fn deserialize_row(data: &[u8]) -> Result<Row> {
    bincode::deserialize(data).context("Failed to deserialize row")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};

    #[test]
    fn test_encode_schema_key() {
        let key = encode_schema_key("public.users");
        assert_eq!(key, b"_sys_schema_public.users".to_vec());
    }

    #[test]
    fn test_encode_data_key() {
        let pk = encode_pk_values(&[Value::Int32(42)]);
        let key = encode_data_key(1, &pk);
        assert!(key.starts_with(b"t_"));
    }

    #[test]
    fn test_encode_table_data_range() {
        let (start, end) = encode_table_data_range(5);
        assert!(start.starts_with(b"t_"));
        assert!(end.starts_with(b"t_"));
        assert!(start < end);
    }

    #[test]
    fn test_encode_pk_values_single() {
        let values = vec![Value::Int32(42)];
        let encoded = encode_pk_values(&values);
        let types = vec![DataType::Int32];
        let mut offset = 0;
        let (decoded, consumed) =
            decode_value_memcomparable(&encoded[offset..], &types[0]).unwrap();
        offset += consumed;
        assert_eq!(offset, encoded.len());
        assert_eq!(decoded, values[0]);
    }

    #[test]
    fn test_encode_pk_values_composite() {
        let values = vec![Value::Int32(1), Value::Text("test".to_string())];
        let encoded = encode_pk_values(&values);
        let types = vec![DataType::Int32, DataType::Text];
        let mut offset = 0;
        let (v1, c1) = decode_value_memcomparable(&encoded[offset..], &types[0]).unwrap();
        offset += c1;
        let (v2, c2) = decode_value_memcomparable(&encoded[offset..], &types[1]).unwrap();
        offset += c2;
        assert_eq!(offset, encoded.len());
        assert_eq!(v1, values[0]);
        assert_eq!(v2, values[1]);
    }

    #[test]
    fn test_serialize_deserialize_row() {
        let row = Row::new(vec![
            Value::Int32(1),
            Value::Text("hello".to_string()),
            Value::Boolean(true),
            Value::Null,
        ]);
        let serialized = serialize_row(&row).unwrap();
        let deserialized = deserialize_row(&serialized).unwrap();
        assert_eq!(deserialized.values, row.values);
    }

    #[test]
    fn test_serialize_deserialize_schema() {
        let schema = TableSchema {
            name: "test_table".to_string(),
            table_id: 42,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
        };
        let serialized = serialize_schema(&schema).unwrap();
        let deserialized = deserialize_schema(&serialized).unwrap();
        assert_eq!(deserialized.name, schema.name);
        assert_eq!(deserialized.table_id, schema.table_id);
        assert_eq!(deserialized.columns.len(), 2);
    }

    #[test]
    fn test_encode_index_key_unique() {
        let values = vec![Value::Int32(1)];
        let key = encode_index_key(1, 2, &values, None);
        assert!(key.starts_with(b"i_"));
    }

    #[test]
    fn test_encode_index_key_non_unique() {
        let values = vec![Value::Int32(1)];
        let pk = vec![Value::Int32(100)];
        let key = encode_index_key(1, 2, &values, Some(&pk));
        assert!(key.starts_with(b"i_"));
        assert!(key.len() > encode_index_key(1, 2, &values, None).len());
    }

    #[test]
    fn test_memcomparable_int32_ordering() {
        let key_neg = encode_pk_values(&[Value::Int32(-100)]);
        let key_zero = encode_pk_values(&[Value::Int32(0)]);
        let key_pos = encode_pk_values(&[Value::Int32(100)]);
        assert!(key_neg < key_zero, "negative should be less than zero");
        assert!(key_zero < key_pos, "zero should be less than positive");
    }

    #[test]
    fn test_memcomparable_int64_ordering() {
        let key_neg = encode_pk_values(&[Value::Int64(-1000)]);
        let key_zero = encode_pk_values(&[Value::Int64(0)]);
        let key_pos = encode_pk_values(&[Value::Int64(1000)]);
        assert!(key_neg < key_zero);
        assert!(key_zero < key_pos);
    }

    #[test]
    fn test_memcomparable_text_ordering() {
        let key_a = encode_pk_values(&[Value::Text("apple".to_string())]);
        let key_b = encode_pk_values(&[Value::Text("banana".to_string())]);
        let key_c = encode_pk_values(&[Value::Text("cherry".to_string())]);
        assert!(key_a < key_b);
        assert!(key_b < key_c);
    }

    #[test]
    fn test_memcomparable_null_ordering() {
        let key_null = encode_pk_values(&[Value::Null]);
        let key_value = encode_pk_values(&[Value::Int32(0)]);
        assert!(key_null < key_value, "NULL should sort before any value");
    }

    #[test]
    fn test_memcomparable_float64_ordering() {
        let key_neg = encode_pk_values(&[Value::Float64(-1.5)]);
        let key_zero = encode_pk_values(&[Value::Float64(0.0)]);
        let key_pos = encode_pk_values(&[Value::Float64(1.5)]);
        assert!(key_neg < key_zero);
        assert!(key_zero < key_pos);
    }

    #[test]
    fn test_index_key_ordering() {
        let key1 = encode_index_key(1, 1, &[Value::Int32(-5)], None);
        let key2 = encode_index_key(1, 1, &[Value::Int32(0)], None);
        let key3 = encode_index_key(1, 1, &[Value::Int32(5)], None);
        assert!(key1 < key2);
        assert!(key2 < key3);
    }

    #[test]
    fn test_composite_key_ordering() {
        let key1 = encode_pk_values(&[Value::Int32(1), Value::Text("a".to_string())]);
        let key2 = encode_pk_values(&[Value::Int32(1), Value::Text("b".to_string())]);
        let key3 = encode_pk_values(&[Value::Int32(2), Value::Text("a".to_string())]);
        assert!(key1 < key2, "same first col, second col determines order");
        assert!(key2 < key3, "first col determines order");
    }
}
