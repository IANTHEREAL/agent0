//! Row data, btree/GIN index key construction, and range scanning.
//!
//! These functions build TiKV keys for user data rows and index entries,
//! using the database-scoped prefix from `encode_database_data_prefix()`.

use crate::types::Value;

use super::value_encoding::encode_value_memcomparable;
use super::{
    encode_database_data_prefix, GIN_PK_SEP_START, SYS_SCHEMA_PREFIX, TABLE_DATA_PREFIX,
    TABLE_GIN_MARKER, TABLE_INDEX_PREFIX,
};

/// Encode a data key for a row (storage format v2, database-scoped).
pub fn encode_data_key_v2(db_id: u64, table_id: u64, row_key: &[u8]) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(TABLE_DATA_PREFIX);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(row_key);
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

fn encode_index_prefix_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    prefix_values: &[Value],
) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(TABLE_INDEX_PREFIX);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&index_id.to_be_bytes());
    key.push(b'_');

    for value in prefix_values {
        encode_value_memcomparable(value, &mut key);
    }

    key
}

fn encode_prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    for idx in (0..end.len()).rev() {
        if end[idx] != 0xFF {
            end[idx] = end[idx].wrapping_add(1);
            end.truncate(idx + 1);
            return end;
        }
    }

    let mut end = prefix.to_vec();
    end.push(0xFF);
    end
}

pub fn encode_index_range_start_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    prefix_values: &[Value],
    range_start: Option<&Value>,
    start_inclusive: bool,
) -> Vec<u8> {
    let mut key = encode_index_prefix_v2(db_id, table_id, index_id, prefix_values);

    if let Some(value) = range_start {
        encode_value_memcomparable(value, &mut key);
        if !start_inclusive {
            key.push(0x00);
        }
    }

    key
}

pub fn encode_index_range_end_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    prefix_values: &[Value],
    range_end: Option<&Value>,
    end_inclusive: bool,
) -> Vec<u8> {
    let mut key = encode_index_prefix_v2(db_id, table_id, index_id, prefix_values);

    if let Some(value) = range_end {
        encode_value_memcomparable(value, &mut key);
        if end_inclusive {
            key.push(0xFF);
        }
        return key;
    }

    encode_prefix_end(&key)
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
