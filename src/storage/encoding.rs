#![allow(dead_code)]
//! Key encoding for TiKV
//!
//! Key layout:
//! - `_sys_next_table_id` -> u64 (auto-incrementing table ID)
//! - `_sys_schema_{table_name}` -> TableSchema (serialized)
//! - `_sys_schemadef_{schema_name}` -> u32 schema OID (big-endian)
//! - `_sys_ext_{extname}` -> InstalledExtension (bincode)
//! - `_sys_extcfg_{extname}` -> ExtensionConfig (reserved, bincode/json)
//! - `_sys_comment_{kind}\0{payload...}` -> UTF-8 comment text (see `encode_comment_*_key()`)
//! - `t_{table_id}_{row_key}` -> Row (serialized)
//! - `i_{table_id}_{index_id}_{index_values}` -> PK (Unique Index)
//! - `i_{table_id}_{index_id}_{index_values}_{pk}` -> Empty (Non-Unique Index)
//! - `i_{table_id}_{index_id}_gin_{token_hash}_{pk}` -> Empty (GIN-like inverted index)
//!
//! Index keys use memcomparable encoding to preserve lexicographic sort order.

use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{Context, Result};
use memcomparable::Deserializer;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// System key prefixes
const SYS_NEXT_TABLE_ID: &[u8] = b"_sys_next_table_id";
const SYS_NEXT_DATABASE_ID: &[u8] = b"_sys_next_database_id";
const SYS_NEXT_TYPE_OID: &[u8] = b"_sys_next_type_oid";
const SYS_NEXT_SCHEMA_OID: &[u8] = b"_sys_next_schema_oid";
const SYS_NEXT_SEQUENCE_OID: &[u8] = b"_sys_next_sequence_oid";
const SYS_NEXT_FUNCTION_OID: &[u8] = b"_sys_next_function_oid";
const SYS_NEXT_TRIGGER_OID: &[u8] = b"_sys_next_trigger_oid";
const SYS_NEXT_VIEW_OID: &[u8] = b"_sys_next_view_oid";
const SYS_FORMAT_VERSION: &[u8] = b"_sys_format_version";
const SYS_DATABASE_BY_NAME_PREFIX: &[u8] = b"_sys_dbname_";
const SYS_DATABASE_BY_ID_PREFIX: &[u8] = b"_sys_dbid_";

// === Storage format v2 (database-scoped prefixes) ===
//
// Keyspace-level metadata remains under `_sys_*` keys. Database-local metadata
// and user data are partitioned by `database_id` using a fixed binary prefix.
//
// Database prefix format: `d_{db_id:8bytes}_`
const DATABASE_DATA_PREFIX: &[u8] = b"d_";
const DB_SYS_NEXT_TABLE_ID: &[u8] = b"sys_next_table_id";
const DB_SYS_NEXT_TYPE_OID: &[u8] = b"sys_next_type_oid";
const DB_SYS_NEXT_SCHEMA_OID: &[u8] = b"sys_next_schema_oid";
const DB_SYS_NEXT_SEQUENCE_OID: &[u8] = b"sys_next_sequence_oid";
const DB_SYS_NEXT_FUNCTION_OID: &[u8] = b"sys_next_function_oid";
const DB_SYS_NEXT_TRIGGER_OID: &[u8] = b"sys_next_trigger_oid";
const DB_SYS_NEXT_VIEW_OID: &[u8] = b"sys_next_view_oid";
const DB_SYS_SCHEMA_PREFIX: &[u8] = b"sys_schema_";
const DB_SYS_SCHEMADEF_PREFIX: &[u8] = b"sys_schemadef_";
const DB_SYS_VIEW_PREFIX: &[u8] = b"sys_view_";
const DB_SYS_MATVIEW_PREFIX: &[u8] = b"sys_matview_";
const DB_SYS_PROCEDURE_PREFIX: &[u8] = b"sys_proc_";
const DB_SYS_FUNCTION_PREFIX: &[u8] = b"sys_func_";
const DB_SYS_TRIGGER_PREFIX: &[u8] = b"sys_trigger_";
const DB_SYS_TYPE_PREFIX: &[u8] = b"sys_type_";
const DB_SYS_SEQUENCEDEF_PREFIX: &[u8] = b"sys_seqdef_";
const DB_SYS_EXTENSION_PREFIX: &[u8] = b"sys_ext_";
const DB_SYS_EXTENSIONCFG_PREFIX: &[u8] = b"sys_extcfg_";
const DB_SYS_COMMENT_PREFIX: &[u8] = b"sys_comment_";
const DB_SYS_SEQ_PREFIX: &[u8] = b"sys_seq_";
const SYS_SCHEMA_PREFIX: &[u8] = b"_sys_schema_";
const SYS_SCHEMADEF_PREFIX: &[u8] = b"_sys_schemadef_";
const SYS_VIEW_PREFIX: &[u8] = b"_sys_view_";
const SYS_MATVIEW_PREFIX: &[u8] = b"_sys_matview_";
const SYS_PROCEDURE_PREFIX: &[u8] = b"_sys_proc_";
const SYS_FUNCTION_PREFIX: &[u8] = b"_sys_func_";
const SYS_TRIGGER_PREFIX: &[u8] = b"_sys_trigger_";
const SYS_TYPE_PREFIX: &[u8] = b"_sys_type_";
const SYS_SEQUENCE_PREFIX: &[u8] = b"_sys_seqdef_";
const SYS_EXTENSION_PREFIX: &[u8] = b"_sys_ext_";
const SYS_EXTENSIONCFG_PREFIX: &[u8] = b"_sys_extcfg_";
const SYS_COMMENT_PREFIX: &[u8] = b"_sys_comment_";
const TABLE_DATA_PREFIX: &[u8] = b"t_";
const TABLE_INDEX_PREFIX: &[u8] = b"i_";
const TABLE_GIN_MARKER: &[u8] = b"gin_";

// GIN keys end the fixed prefix with a separator byte to allow prefix range scans:
//   ... gin_{hash}[SEP]{pk...}
// We use 0x00 for the scan start and 0x01 for the scan end (exclusive).
const GIN_PK_SEP_START: u8 = 0x00;
const GIN_PK_SEP_END: u8 = 0x01;

/// Encode the system key for next table ID
pub fn encode_next_table_id_key() -> Vec<u8> {
    SYS_NEXT_TABLE_ID.to_vec()
}

/// Encode the system key for next type OID (user-defined types)
pub fn encode_next_type_oid_key() -> Vec<u8> {
    SYS_NEXT_TYPE_OID.to_vec()
}

/// Encode the system key for next schema OID (user-defined schemas)
pub fn encode_next_schema_oid_key() -> Vec<u8> {
    SYS_NEXT_SCHEMA_OID.to_vec()
}

/// Encode the system key for next sequence OID (standalone + implicit sequences)
pub fn encode_next_sequence_oid_key() -> Vec<u8> {
    SYS_NEXT_SEQUENCE_OID.to_vec()
}

/// Encode the system key for next function OID (user-defined functions)
pub fn encode_next_function_oid_key() -> Vec<u8> {
    SYS_NEXT_FUNCTION_OID.to_vec()
}

/// Encode the system key for next trigger OID (user-defined triggers)
pub fn encode_next_trigger_oid_key() -> Vec<u8> {
    SYS_NEXT_TRIGGER_OID.to_vec()
}

/// Encode the system key for next view OID (user-defined views)
pub fn encode_next_view_oid_key() -> Vec<u8> {
    SYS_NEXT_VIEW_OID.to_vec()
}

/// Encode the system key for allocating the next database ID (storage format v2).
pub fn encode_next_database_id_key() -> Vec<u8> {
    SYS_NEXT_DATABASE_ID.to_vec()
}

/// Encode the system key storing the storage-format version for a keyspace.
pub fn encode_format_version_key() -> Vec<u8> {
    SYS_FORMAT_VERSION.to_vec()
}

/// Encode database name -> ID mapping key (keyspace-level, storage format v2).
pub fn encode_database_name_key(db_name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(SYS_DATABASE_BY_NAME_PREFIX.len() + db_name.len());
    key.extend_from_slice(SYS_DATABASE_BY_NAME_PREFIX);
    key.extend_from_slice(db_name.as_bytes());
    key
}

/// Encode database ID -> definition key (keyspace-level, storage format v2).
pub fn encode_database_id_key(db_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(SYS_DATABASE_BY_ID_PREFIX.len() + 8);
    key.extend_from_slice(SYS_DATABASE_BY_ID_PREFIX);
    key.extend_from_slice(&db_id.to_be_bytes());
    key
}

pub fn encode_database_id_prefix() -> Vec<u8> {
    SYS_DATABASE_BY_ID_PREFIX.to_vec()
}

/// Encode the prefix for all keys belonging to a database (storage format v2).
///
/// Format: `d_{db_id:8bytes}_`
pub fn encode_database_data_prefix(db_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(DATABASE_DATA_PREFIX.len() + 8 + 1);
    key.extend_from_slice(DATABASE_DATA_PREFIX);
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key
}

/// Get the raw key range for all data within a database (storage format v2).
///
/// Range: `[d_{db_id}_, d_{db_id+1}_)` (numeric, big-endian ordering). If `db_id == u64::MAX`,
/// fall back to a prefix upper bound by incrementing the trailing separator byte.
pub fn encode_database_data_range(db_id: u64) -> (Vec<u8>, Vec<u8>) {
    let start = encode_database_data_prefix(db_id);
    let end = match db_id.checked_add(1) {
        Some(next) => encode_database_data_prefix(next),
        None => {
            let mut end = start.clone();
            // `encode_database_data_prefix` always ends with '_' (0x5F).
            if let Some(last) = end.last_mut() {
                *last = last.wrapping_add(1);
            }
            end
        }
    };
    (start, end)
}

pub fn encode_next_table_id_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_TABLE_ID);
    key
}

pub fn encode_next_type_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_TYPE_OID);
    key
}

pub fn encode_next_schema_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_SCHEMA_OID);
    key
}

pub fn encode_next_sequence_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_SEQUENCE_OID);
    key
}

pub fn encode_next_function_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_FUNCTION_OID);
    key
}

pub fn encode_next_trigger_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_TRIGGER_OID);
    key
}

pub fn encode_next_view_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_VIEW_OID);
    key
}

pub fn encode_schema_key_v2(db_id: u64, table_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SCHEMA_PREFIX);
    key.extend_from_slice(table_name.as_bytes());
    key
}

pub fn encode_schema_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SCHEMA_PREFIX);
    key
}

pub fn encode_schema_def_key_v2(db_id: u64, schema_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SCHEMADEF_PREFIX);
    key.extend_from_slice(schema_name.as_bytes());
    key
}

pub fn encode_schema_def_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SCHEMADEF_PREFIX);
    key
}

pub fn encode_type_key_v2(db_id: u64, full_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_TYPE_PREFIX);
    key.extend_from_slice(full_name.as_bytes());
    key
}

pub fn encode_type_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_TYPE_PREFIX);
    key
}

pub fn encode_sequence_def_key_v2(db_id: u64, full_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SEQUENCEDEF_PREFIX);
    key.extend_from_slice(full_name.as_bytes());
    key
}

pub fn encode_sequence_def_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SEQUENCEDEF_PREFIX);
    key
}

pub fn encode_sequence_value_key_v2(db_id: u64, sequence_oid: u32) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SEQ_PREFIX);
    key.extend_from_slice(&sequence_oid.to_be_bytes());
    key
}

pub fn encode_table_sequence_value_key_v2(db_id: u64, table_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_SEQ_PREFIX);
    key.extend_from_slice(&table_id.to_be_bytes());
    key
}

pub fn encode_extension_key_v2(db_id: u64, ext_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_EXTENSION_PREFIX);
    key.extend_from_slice(ext_name.as_bytes());
    key
}

pub fn encode_extension_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_EXTENSION_PREFIX);
    key
}

pub fn encode_extension_config_key_v2(db_id: u64, ext_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_EXTENSIONCFG_PREFIX);
    key.extend_from_slice(ext_name.as_bytes());
    key
}

pub fn encode_view_key_v2(db_id: u64, view_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_VIEW_PREFIX);
    key.extend_from_slice(view_name.as_bytes());
    key
}

pub fn encode_view_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_VIEW_PREFIX);
    key
}

pub fn encode_matview_key_v2(db_id: u64, matview_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_MATVIEW_PREFIX);
    key.extend_from_slice(matview_name.as_bytes());
    key
}

pub fn encode_matview_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_MATVIEW_PREFIX);
    key
}

pub fn encode_procedure_key_v2(db_id: u64, proc_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_PROCEDURE_PREFIX);
    key.extend_from_slice(proc_name.as_bytes());
    key
}

pub fn encode_procedure_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_PROCEDURE_PREFIX);
    key
}

pub fn encode_function_key_v2(db_id: u64, full_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_FUNCTION_PREFIX);
    key.extend_from_slice(full_name.as_bytes());
    key
}

pub fn encode_function_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_FUNCTION_PREFIX);
    key
}

pub fn encode_trigger_key_v2(db_id: u64, table_full_name: &str, trigger_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_TRIGGER_PREFIX);
    key.extend_from_slice(table_full_name.as_bytes());
    key.push(b'/');
    key.extend_from_slice(trigger_name.as_bytes());
    key
}

pub fn encode_trigger_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_TRIGGER_PREFIX);
    key
}

pub fn encode_trigger_table_prefix_v2(db_id: u64, table_full_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_TRIGGER_PREFIX);
    key.extend_from_slice(table_full_name.as_bytes());
    key.push(b'/');
    key
}

pub(crate) fn encode_comment_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_COMMENT_PREFIX);
    key
}

pub(crate) fn encode_comment_extension_key_v2(db_id: u64, ext_name: &str) -> Vec<u8> {
    let mut key = encode_comment_prefix_v2(db_id);
    key.push(b'e');
    key.push(0);
    key.extend_from_slice(ext_name.as_bytes());
    key
}

pub(crate) fn encode_comment_function_key_v2(db_id: u64, func_full_name: &str) -> Vec<u8> {
    let mut key = encode_comment_prefix_v2(db_id);
    key.push(b'f');
    key.push(0);
    key.extend_from_slice(func_full_name.as_bytes());
    key
}

pub(crate) fn encode_comment_table_key_v2(db_id: u64, table_full_name: &str) -> Vec<u8> {
    let mut key = encode_comment_prefix_v2(db_id);
    key.push(b't');
    key.push(0);
    key.extend_from_slice(table_full_name.as_bytes());
    key
}

pub(crate) fn encode_comment_column_key_v2(
    db_id: u64,
    table_full_name: &str,
    column_name: &str,
) -> Vec<u8> {
    let mut key = encode_comment_prefix_v2(db_id);
    key.push(b'c');
    key.push(0);
    key.extend_from_slice(table_full_name.as_bytes());
    key.push(0);
    key.extend_from_slice(column_name.as_bytes());
    key
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

/// Encode the key for an installed extension.
pub fn encode_extension_key(ext_name: &str) -> Vec<u8> {
    let mut key = SYS_EXTENSION_PREFIX.to_vec();
    key.extend_from_slice(ext_name.as_bytes());
    key
}

pub fn encode_extension_prefix() -> Vec<u8> {
    SYS_EXTENSION_PREFIX.to_vec()
}

/// Encode the key for an extension config blob.
pub fn encode_extension_config_key(ext_name: &str) -> Vec<u8> {
    let mut key = SYS_EXTENSIONCFG_PREFIX.to_vec();
    key.extend_from_slice(ext_name.as_bytes());
    key
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

pub(crate) fn encode_comment_prefix() -> Vec<u8> {
    SYS_COMMENT_PREFIX.to_vec()
}

pub(crate) fn encode_comment_extension_key(ext_name: &str) -> Vec<u8> {
    let mut key = SYS_COMMENT_PREFIX.to_vec();
    key.push(b'e');
    key.push(0);
    key.extend_from_slice(ext_name.as_bytes());
    key
}

pub(crate) fn encode_comment_function_key(func_full_name: &str) -> Vec<u8> {
    let mut key = SYS_COMMENT_PREFIX.to_vec();
    key.push(b'f');
    key.push(0);
    key.extend_from_slice(func_full_name.as_bytes());
    key
}

pub(crate) fn encode_comment_table_key(table_full_name: &str) -> Vec<u8> {
    let mut key = SYS_COMMENT_PREFIX.to_vec();
    key.push(b't');
    key.push(0);
    key.extend_from_slice(table_full_name.as_bytes());
    key
}

pub(crate) fn encode_comment_column_key(table_full_name: &str, column_name: &str) -> Vec<u8> {
    let mut key = SYS_COMMENT_PREFIX.to_vec();
    key.push(b'c');
    key.push(0);
    key.extend_from_slice(table_full_name.as_bytes());
    key.push(0);
    key.extend_from_slice(column_name.as_bytes());
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

/// Encode a data key for a row (storage format v2, database-scoped).
pub fn encode_data_key_v2(db_id: u64, table_id: u64, row_key: &[u8]) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(TABLE_DATA_PREFIX);
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

/// Encode an index key (storage format v2, database-scoped).
pub fn encode_index_key_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    values: &[Value],
    pk: Option<&[Value]>,
) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(TABLE_INDEX_PREFIX);
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

/// Encode the fixed prefix for a GIN-like inverted index entry.
///
/// The returned key ends with a separator byte so callers can construct a range
/// for scanning all postings for a token hash.
pub fn encode_gin_index_prefix(table_id: u64, index_id: u64, token_hash: u64) -> Vec<u8> {
    let mut key = TABLE_INDEX_PREFIX.to_vec();
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&index_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(TABLE_GIN_MARKER);
    key.extend_from_slice(&token_hash.to_be_bytes());
    key.push(GIN_PK_SEP_START);
    key
}

/// Encode the fixed prefix for a GIN-like inverted index entry (storage format v2, database-scoped).
pub fn encode_gin_index_prefix_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    token_hash: u64,
) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(TABLE_INDEX_PREFIX);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&index_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(TABLE_GIN_MARKER);
    key.extend_from_slice(&token_hash.to_be_bytes());
    key.push(GIN_PK_SEP_START);
    key
}

/// Encode a full GIN-like inverted index key for `token_hash` pointing to the row `pk_key`.
pub fn encode_gin_index_key(
    table_id: u64,
    index_id: u64,
    token_hash: u64,
    pk_key: &[u8],
) -> Vec<u8> {
    let mut key = encode_gin_index_prefix(table_id, index_id, token_hash);
    key.extend_from_slice(pk_key);
    key
}

/// Encode a full GIN-like inverted index key (storage format v2, database-scoped).
pub fn encode_gin_index_key_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    token_hash: u64,
    pk_key: &[u8],
) -> Vec<u8> {
    let mut key = encode_gin_index_prefix_v2(db_id, table_id, index_id, token_hash);
    key.extend_from_slice(pk_key);
    key
}

/// Return the raw key range for scanning all GIN postings for a token hash.
///
/// The range is `[start, end)` in lexicographic order.
pub fn encode_gin_index_token_range(
    table_id: u64,
    index_id: u64,
    token_hash: u64,
) -> (Vec<u8>, Vec<u8>) {
    let start = encode_gin_index_prefix(table_id, index_id, token_hash);
    let mut end = start.clone();
    *end.last_mut().expect("prefix has separator") = GIN_PK_SEP_END;
    (start, end)
}

/// Return the raw key range for scanning all GIN postings for a token hash (storage format v2, database-scoped).
pub fn encode_gin_index_token_range_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    token_hash: u64,
) -> (Vec<u8>, Vec<u8>) {
    let start = encode_gin_index_prefix_v2(db_id, table_id, index_id, token_hash);
    let mut end = start.clone();
    *end.last_mut().expect("prefix has separator") = GIN_PK_SEP_END;
    (start, end)
}

const NULL_TAG: u8 = 0x00;
const NOT_NULL_TAG: u8 = 0x01;
const DECIMAL_SIGN_NEG: u8 = 0x00;
const DECIMAL_SIGN_ZERO: u8 = 0x01;
const DECIMAL_SIGN_POS: u8 = 0x02;
// rust_decimal supports up to 28 significant base-10 digits.
const DECIMAL_MAX_DIGITS: usize = 28;

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
        Value::Interval(iv) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&iv.months).unwrap());
            buf.extend(memcomparable::to_vec(&iv.millis).unwrap());
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
        Value::Numeric(d) => {
            buf.push(NOT_NULL_TAG);
            // Memcomparable encoding for Decimal.
            //
            // We need a stable, order-preserving, *canonical* encoding so that:
            // - Lexicographic order of the encoded bytes matches numeric order.
            // - Equal numeric values encode to identical bytes (e.g. 1.0 == 1.00).
            //
            // Encoding format (32 bytes after NOT_NULL_TAG):
            //   [sign:1][exp:2][digits:28][digits_len:1]
            // - sign: 0x00 negative, 0x01 zero, 0x02 positive
            // - exp: i16 (digits_count - scale), stored as (exp ^ 0x8000) big-endian
            // - digits: absolute mantissa digits, left-aligned, right-padded with ASCII '0'
            // - digits_len: number of mantissa digits (1..=28), needed to decode values whose
            //   mantissa ends in 0 (e.g. 100)
            // For negative values, exp+digits bytes are bitwise inverted to reverse ordering.

            let mut normalized = *d;
            normalized.normalize_assign();
            let unpacked = normalized.unpack();
            let mantissa =
                unpacked.lo as u128 | (unpacked.mid as u128) << 32 | (unpacked.hi as u128) << 64;

            if mantissa == 0 {
                buf.push(DECIMAL_SIGN_ZERO);
                buf.extend_from_slice(&[0u8; 2 + DECIMAL_MAX_DIGITS + 1]);
                return;
            }

            let scale = unpacked.scale as i16;
            let mantissa_str = mantissa.to_string();
            let digits_len =
                u8::try_from(mantissa_str.len()).expect("Decimal mantissa digits fit in u8");
            let digits_count = i16::from(digits_len);
            let exp = digits_count - scale;

            let exp_u16 = (exp as u16) ^ 0x8000;
            let exp_bytes = exp_u16.to_be_bytes();

            let mut digits_buf = [b'0'; DECIMAL_MAX_DIGITS];
            digits_buf[..mantissa_str.len()].copy_from_slice(mantissa_str.as_bytes());

            if unpacked.negative {
                buf.push(DECIMAL_SIGN_NEG);
                buf.extend(exp_bytes.map(|b| !b));
                buf.extend(digits_buf.map(|b| !b));
                buf.push(!digits_len);
            } else {
                buf.push(DECIMAL_SIGN_POS);
                buf.extend(exp_bytes);
                buf.extend(digits_buf);
                buf.push(digits_len);
            }
        }
        Value::Tsvector(s) | Value::Tsquery(s) => {
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
        DataType::Timestamp | DataType::TimestampTz => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Timestamp(v), deserializer.position())
        }
        DataType::Interval => {
            let months: i32 = serde::Deserialize::deserialize(&mut deserializer)?;
            let millis: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (
                Value::Interval(crate::types::IntervalValue::new(months, millis)),
                deserializer.position(),
            )
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
        DataType::Tsvector => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Tsvector(v), deserializer.position())
        }
        DataType::Tsquery => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Tsquery(v), deserializer.position())
        }
        DataType::Numeric { .. } => {
            // Decode memcomparable Numeric:
            //   [sign:1][exp:2][digits:28][digits_len:1]
            let expected = 1 + 2 + DECIMAL_MAX_DIGITS + 1;
            if payload.len() < expected {
                anyhow::bail!(
                    "Numeric decode: expected {} bytes, got {}",
                    expected,
                    payload.len()
                );
            }

            let sign_byte = payload[0];
            if sign_byte == DECIMAL_SIGN_ZERO {
                return Ok((Value::Numeric(Decimal::ZERO), 1 + expected));
            }

            if sign_byte != DECIMAL_SIGN_NEG && sign_byte != DECIMAL_SIGN_POS {
                anyhow::bail!("Numeric decode: invalid sign byte: {}", sign_byte);
            }

            let is_negative = sign_byte == DECIMAL_SIGN_NEG;
            let exp_bytes: [u8; 2] = payload[1..3].try_into().unwrap();
            let digits_bytes: &[u8] = &payload[3..3 + DECIMAL_MAX_DIGITS];
            let digits_len_byte = payload[3 + DECIMAL_MAX_DIGITS];

            let exp_bytes = if is_negative {
                exp_bytes.map(|b| !b)
            } else {
                exp_bytes
            };
            let digits_bytes: Vec<u8> = if is_negative {
                digits_bytes.iter().map(|b| !b).collect()
            } else {
                digits_bytes.to_vec()
            };
            let digits_len_byte = if is_negative {
                !digits_len_byte
            } else {
                digits_len_byte
            };

            let exp_u16 = u16::from_be_bytes(exp_bytes);
            let exp = ((exp_u16 ^ 0x8000) as i16) as i32;

            let digits_len = usize::from(digits_len_byte);
            if !(1..=DECIMAL_MAX_DIGITS).contains(&digits_len) {
                anyhow::bail!("Numeric decode: invalid digits_len: {}", digits_len);
            }
            let digits_str = std::str::from_utf8(&digits_bytes[..digits_len])
                .context("Numeric decode: mantissa digits not utf8")?;
            let mantissa = digits_str
                .parse::<u128>()
                .context("Numeric decode: invalid mantissa digits")?;

            let scale_i32 = (digits_len as i32)
                .checked_sub(exp)
                .ok_or_else(|| anyhow::anyhow!("Numeric decode: invalid scale computation"))?;
            if !(0..=28).contains(&scale_i32) {
                anyhow::bail!("Numeric decode: scale out of range: {}", scale_i32);
            }
            if (mantissa >> 96) != 0 {
                anyhow::bail!("Numeric decode: mantissa out of range");
            }

            let lo = mantissa as u32;
            let mid = (mantissa >> 32) as u32;
            let hi = (mantissa >> 64) as u32;

            let d = Decimal::from_parts(lo, mid, hi, is_negative, scale_i32 as u32);
            (Value::Numeric(d), expected)
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

/// Get the key range for scanning all rows of a table (storage format v2, database-scoped).
pub fn encode_table_data_range_v2(db_id: u64, table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut start = encode_database_data_prefix(db_id);
    start.extend_from_slice(TABLE_DATA_PREFIX);
    start.extend_from_slice(&table_id.to_be_bytes());
    start.push(b'_');

    let mut end = encode_database_data_prefix(db_id);
    end.extend_from_slice(TABLE_DATA_PREFIX);
    end.extend_from_slice(&(table_id + 1).to_be_bytes());

    (start, end)
}

/// Get the key range for scanning all index entries of a table (storage format v2, database-scoped).
pub fn encode_table_index_range_v2(db_id: u64, table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut start = encode_database_data_prefix(db_id);
    start.extend_from_slice(TABLE_INDEX_PREFIX);
    start.extend_from_slice(&table_id.to_be_bytes());
    start.push(b'_');

    let mut end = encode_database_data_prefix(db_id);
    end.extend_from_slice(TABLE_INDEX_PREFIX);
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
    const SCHEMA_MAGIC: &[u8] = b"PGTIKV_SCHEMA_V1\0";
    let payload = bincode::serialize(schema).context("Failed to serialize schema")?;
    let mut out = Vec::with_capacity(SCHEMA_MAGIC.len() + payload.len());
    out.extend_from_slice(SCHEMA_MAGIC);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserialize a table schema
pub fn deserialize_schema(data: &[u8]) -> Result<TableSchema> {
    const SCHEMA_MAGIC: &[u8] = b"PGTIKV_SCHEMA_V1\0";
    const DEFAULT_OWNER: &str = "postgres";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct LegacyTableSchema {
        name: String,
        table_id: u64,
        columns: Vec<crate::types::ColumnDef>,
        version: u64,
        pk_indices: Vec<usize>,
        indexes: Vec<crate::types::IndexDef>,
        #[serde(default)]
        check_constraints: Vec<crate::types::CheckConstraint>,
        #[serde(default)]
        foreign_keys: Vec<crate::types::ForeignKeyConstraint>,
    }

    if let Some(payload) = data.strip_prefix(SCHEMA_MAGIC) {
        return bincode::deserialize(payload).context("Failed to deserialize schema");
    }

    // Backward compatibility: legacy schemas were stored as raw bincode bytes without a header and
    // without an owner field. Decode them and synthesize a default owner.
    let legacy: LegacyTableSchema =
        bincode::deserialize(data).context("Failed to deserialize legacy schema")?;
    let pk_constraint_name = if legacy.pk_indices.is_empty() {
        None
    } else {
        let short = legacy
            .name
            .rsplit('.')
            .next()
            .unwrap_or(legacy.name.as_str());
        Some(format!("{}_pkey", short))
    };
    Ok(TableSchema {
        name: legacy.name,
        table_id: legacy.table_id,
        columns: legacy.columns,
        version: legacy.version,
        pk_constraint_name,
        pk_indices: legacy.pk_indices,
        indexes: legacy.indexes,
        check_constraints: legacy.check_constraints,
        foreign_keys: legacy.foreign_keys,
        owner: DEFAULT_OWNER.to_string(),
    })
}

/// Serialize a row
pub fn serialize_row(row: &Row) -> Result<Vec<u8>> {
    bincode::serialize(row).context("Failed to serialize row")
}

/// Deserialize a row
pub fn deserialize_row(data: &[u8]) -> Result<Row> {
    bincode::deserialize(data).context("Failed to deserialize row")
}

/// Serialize a function definition.
///
/// Stored values are versioned (magic header + bincode payload) to allow future evolution without
/// breaking older clusters.
pub fn serialize_function_def(def: &crate::types::FunctionDef) -> Result<Vec<u8>> {
    const FUNCTION_MAGIC: &[u8] = b"PGTIKV_FUNCTION_V1\0";
    let payload = bincode::serialize(def).context("Failed to serialize function definition")?;
    let mut out = Vec::with_capacity(FUNCTION_MAGIC.len() + payload.len());
    out.extend_from_slice(FUNCTION_MAGIC);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserialize a function definition, supporting legacy (unversioned) payloads.
pub fn deserialize_function_def(data: &[u8]) -> Result<crate::types::FunctionDef> {
    const FUNCTION_MAGIC: &[u8] = b"PGTIKV_FUNCTION_V1\0";
    const DEFAULT_OWNER: &str = "postgres";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct LegacyFunctionDef {
        #[serde(default)]
        oid: u32,
        schema: String,
        name: String,
        arg_types: Vec<String>,
        return_type: String,
        language: String,
        body: String,
    }

    if let Some(payload) = data.strip_prefix(FUNCTION_MAGIC) {
        return bincode::deserialize(payload).context("Failed to deserialize function definition");
    }

    let legacy: LegacyFunctionDef =
        bincode::deserialize(data).context("Failed to deserialize legacy function definition")?;
    Ok(crate::types::FunctionDef {
        oid: legacy.oid,
        schema: legacy.schema,
        name: legacy.name,
        arg_types: legacy.arg_types,
        return_type: legacy.return_type,
        language: legacy.language,
        body: legacy.body,
        owner: DEFAULT_OWNER.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CheckConstraint, ColumnDef, DataType, ForeignKeyConstraint, IndexDef};

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
    fn test_encode_database_name_key() {
        let key = encode_database_name_key("mydb");
        assert_eq!(key, b"_sys_dbname_mydb".to_vec());
    }

    #[test]
    fn test_encode_database_id_key() {
        let key = encode_database_id_key(1);
        assert_eq!(
            &key[..SYS_DATABASE_BY_ID_PREFIX.len()],
            SYS_DATABASE_BY_ID_PREFIX
        );
        assert_eq!(
            &key[SYS_DATABASE_BY_ID_PREFIX.len()..],
            &1_u64.to_be_bytes()
        );
    }

    #[test]
    fn test_encode_database_data_prefix() {
        let key = encode_database_data_prefix(1);
        assert_eq!(key.len(), 11);
        assert_eq!(&key[..2], b"d_");
        assert_eq!(&key[2..10], &1_u64.to_be_bytes());
        assert_eq!(key[10], b'_');
    }

    #[test]
    fn test_encode_database_data_range() {
        let (start, end) = encode_database_data_range(5);
        assert_eq!(start, encode_database_data_prefix(5));
        assert_eq!(end, encode_database_data_prefix(6));
        assert!(start < end);
    }

    #[test]
    fn test_encode_schema_key_v2() {
        let key = encode_schema_key_v2(1, "public.users");
        let mut expected = encode_database_data_prefix(1);
        expected.extend_from_slice(b"sys_schema_public.users");
        assert_eq!(key, expected);
    }

    #[test]
    fn test_encode_extension_keys() {
        let key = encode_extension_key("http");
        assert_eq!(key, b"_sys_ext_http".to_vec());

        let cfg_key = encode_extension_config_key("http");
        assert_eq!(cfg_key, b"_sys_extcfg_http".to_vec());
    }

    #[test]
    fn test_encode_table_data_range() {
        let (start, end) = encode_table_data_range(5);
        assert!(start.starts_with(b"t_"));
        assert!(end.starts_with(b"t_"));
        assert!(start < end);
    }

    #[test]
    fn test_encode_table_data_range_v2() {
        let (start, end) = encode_table_data_range_v2(5, 7);
        assert!(start < end);
        assert!(start.starts_with(&encode_database_data_prefix(5)));
        assert!(end.starts_with(&encode_database_data_prefix(5)));
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
    fn test_serialize_deserialize_numeric() {
        use std::str::FromStr;
        let row = Row::new(vec![
            Value::Int32(1),
            Value::Numeric(Decimal::from_str("123.45").unwrap()),
            Value::Numeric(Decimal::from_str("-999.99").unwrap()),
            Value::Numeric(Decimal::ZERO),
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
            pk_constraint_name: Some("test_table_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
        };
        let serialized = serialize_schema(&schema).unwrap();
        let deserialized = deserialize_schema(&serialized).unwrap();
        assert_eq!(deserialized.name, schema.name);
        assert_eq!(deserialized.table_id, schema.table_id);
        assert_eq!(deserialized.columns.len(), 2);
    }

    #[test]
    fn test_deserialize_schema_legacy_without_owner() {
        #[derive(Serialize, Deserialize)]
        struct LegacyTableSchema {
            name: String,
            table_id: u64,
            columns: Vec<ColumnDef>,
            version: u64,
            pk_indices: Vec<usize>,
            indexes: Vec<IndexDef>,
            #[serde(default)]
            check_constraints: Vec<CheckConstraint>,
            #[serde(default)]
            foreign_keys: Vec<ForeignKeyConstraint>,
        }

        let legacy = LegacyTableSchema {
            name: "public.t".to_string(),
            table_id: 7,
            columns: vec![],
            version: 1,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
        };

        let bytes = bincode::serialize(&legacy).unwrap();
        let decoded = deserialize_schema(&bytes).unwrap();
        assert_eq!(decoded.owner, "postgres");
        assert_eq!(decoded.name, "public.t");
        assert_eq!(decoded.table_id, 7);
    }

    #[test]
    fn test_deserialize_function_def_legacy_without_owner() {
        #[derive(Serialize, Deserialize)]
        struct LegacyFunctionDef {
            #[serde(default)]
            oid: u32,
            schema: String,
            name: String,
            arg_types: Vec<String>,
            return_type: String,
            language: String,
            body: String,
        }

        let legacy = LegacyFunctionDef {
            oid: 42,
            schema: "public".to_string(),
            name: "f".to_string(),
            arg_types: vec![],
            return_type: "int".to_string(),
            language: "sql".to_string(),
            body: "select 1".to_string(),
        };

        let bytes = bincode::serialize(&legacy).unwrap();
        let decoded = deserialize_function_def(&bytes).unwrap();
        assert_eq!(decoded.owner, "postgres");
        assert_eq!(decoded.oid, 42);
        assert_eq!(decoded.schema, "public");
        assert_eq!(decoded.name, "f");
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
    fn test_memcomparable_numeric_ordering() {
        use std::str::FromStr;

        let k_009 = encode_pk_values(&[Value::Numeric(Decimal::from_str("0.09").unwrap())]);
        let k_01 = encode_pk_values(&[Value::Numeric(Decimal::from_str("0.1").unwrap())]);
        let k_119 = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.19").unwrap())]);
        let k_12 = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.2").unwrap())]);
        assert!(k_009 < k_01);
        assert!(k_119 < k_12);

        let k_n100 = encode_pk_values(&[Value::Numeric(Decimal::from_str("-100").unwrap())]);
        let k_n2 = encode_pk_values(&[Value::Numeric(Decimal::from_str("-2").unwrap())]);
        assert!(k_n100 < k_n2);

        let k_zero = encode_pk_values(&[Value::Numeric(Decimal::ZERO)]);
        assert!(k_n2 < k_zero);
        assert!(k_zero < k_01);
    }

    #[test]
    fn test_memcomparable_numeric_canonicalization() {
        use std::str::FromStr;

        let a = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.0").unwrap())]);
        let b = encode_pk_values(&[Value::Numeric(Decimal::from_str("1.00").unwrap())]);
        assert_eq!(a, b);
    }

    #[test]
    fn test_decode_memcomparable_numeric() {
        use std::str::FromStr;

        let values = vec![
            Value::Numeric(Decimal::from_str("100").unwrap()),
            Value::Numeric(Decimal::from_str("-0.09").unwrap()),
            Value::Numeric(Decimal::from_str("1.2").unwrap()),
        ];
        let ty = DataType::Numeric {
            precision: None,
            scale: None,
        };
        for value in values {
            let encoded = encode_pk_values(&[value.clone()]);
            let (decoded, consumed) = decode_value_memcomparable(&encoded, &ty).unwrap();
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, value);
        }
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

    #[test]
    fn test_encode_gin_index_token_range() {
        let (start, end) = encode_gin_index_token_range(1, 2, 0x1122334455667788);
        assert!(start.starts_with(b"i_"));
        assert!(start < end);
    }

    #[test]
    fn test_encode_gin_index_key_starts_with_prefix() {
        let token_hash = 123_u64;
        let pk = encode_pk_values(&[Value::Int32(42)]);
        let prefix = encode_gin_index_prefix(10, 5, token_hash);
        let key = encode_gin_index_key(10, 5, token_hash, &pk);
        assert!(key.starts_with(&prefix));
        assert_eq!(&key[prefix.len()..], pk.as_slice());
    }

    #[test]
    fn test_encode_comment_keys_are_prefixed_and_distinct() {
        let prefix = encode_comment_prefix();

        let ext = encode_comment_extension_key("uuid-ossp");
        let func = encode_comment_function_key("public.f");
        let table = encode_comment_table_key("public.t");
        let col = encode_comment_column_key("public.t", "c");

        assert!(ext.starts_with(&prefix));
        assert!(func.starts_with(&prefix));
        assert!(table.starts_with(&prefix));
        assert!(col.starts_with(&prefix));

        assert_ne!(ext, func);
        assert_ne!(table, col);
    }
}
