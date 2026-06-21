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

mod data_keys;
mod metadata_keys;
mod serialization;
mod value_encoding;

#[cfg(test)]
mod tests;

// === Re-exports: preserve the original pub/pub(crate) surface ===

#[allow(unused_imports)]
pub use data_keys::{
    encode_data_key_v2, encode_gin_index_key_v2, encode_gin_index_prefix_v2, encode_index_key_v2,
    encode_index_range_end_v2, encode_index_range_start_v2, encode_pk_values, encode_prefix_end,
    encode_schema_prefix, encode_table_data_range_v2, encode_table_index_range_v2,
};
pub use metadata_keys::decode_worker_queue_fire_time;
#[cfg(test)]
pub use metadata_keys::{decode_worker_queue_task_type, encode_worker_queue_key};
pub use metadata_keys::{
    decode_wq_due_v2_fire_time, decode_wq_index_key, encode_worker_queue_migration_lock_key,
    encode_worker_queue_schema_version_key, encode_wq_due_v2_key, encode_wq_due_v2_prefix,
    encode_wq_due_v2_scan_end, encode_wq_index_key, encode_wq_index_prefix_db,
    encode_wq_index_prefix_db_type, encode_wq_index_prefix_keyspace, encode_wq_index_prefix_task,
    encode_wq_payload_v2_key,
};
pub use metadata_keys::{
    encode_collation_key_v2, encode_collation_prefix_v2, encode_cron_active_key_v2,
    encode_cron_active_prefix_v2, encode_cron_claim_prefix_v2, encode_cron_control_key_v2,
    encode_cron_control_prefix_v2, encode_cron_enabled_key_v2, encode_cron_job_key_v2,
    encode_cron_job_prefix_v2, encode_cron_migrated_key_v3, encode_cron_run_key_v2,
    encode_cron_run_prefix_v2, encode_cron_running_guard_key_v2,
    encode_cron_running_guard_prefix_v2, encode_ddl_journal_key, encode_ddl_journal_prefix,
    encode_embedding_usage_key_v2, encode_extension_config_key_v2, encode_extension_key_v2,
    encode_extension_prefix_v2, encode_function_key_v2, encode_function_prefix_v2,
    encode_gc_instance_state_key, encode_gc_instance_state_prefix,
    encode_hnsw_s3_db_prefix_cleanup_intent_key, encode_hnsw_s3_db_prefix_cleanup_intent_prefix,
    encode_hnsw_s3_graph_upload_intent_key, encode_hnsw_s3_graph_upload_intent_prefix,
    encode_matview_bindings_key_v2, encode_matview_bindings_prefix_v2, encode_matview_key_v2,
    encode_matview_prefix_v2, encode_migration_key, encode_migration_prefix,
    encode_next_cron_job_id_key_v2, encode_next_cron_run_id_key_v2,
    encode_next_function_oid_key_v2, encode_next_policy_oid_key_v2, encode_next_schema_oid_key_v2,
    encode_next_sequence_oid_key_v2, encode_next_table_id_key_v2, encode_next_trigger_oid_key_v2,
    encode_next_type_oid_key_v2, encode_next_view_oid_key_v2, encode_policy_key_v2,
    encode_policy_prefix_v2, encode_policy_table_prefix_v2, encode_procedure_key_v2,
    encode_procedure_prefix_v2, encode_relname_key_v2, encode_schema_def_key_v2,
    encode_schema_def_prefix_v2, encode_schema_key_v2, encode_schema_prefix_v2,
    encode_sequence_def_key_v2, encode_sequence_def_prefix_v2, encode_sequence_value_key_v2,
    encode_stats_column_key, encode_stats_column_prefix, encode_stats_header_key,
    encode_stats_key_v2, encode_storage_stats_key_v2, encode_table_sequence_value_key_v2,
    encode_trigger_key_v2, encode_trigger_prefix_v2, encode_trigger_table_prefix_v2,
    encode_tsc_key_v2, encode_type_key_v2, encode_type_prefix_v2, encode_view_bindings_key_v2,
    encode_view_bindings_prefix_v2, encode_view_key_v2, encode_view_prefix_v2,
    encode_worker_bg_result_key, encode_worker_bg_task_seq_key, encode_worker_claim_key,
    encode_worker_claim_prefix, encode_worker_dropped_db_tombstone_key, encode_worker_queue_prefix,
    encode_worker_queue_scan_end, encode_worker_registry_key, encode_worker_registry_prefix,
    encode_worker_storage_scan_dirty_key,
};
pub(crate) use metadata_keys::{
    encode_comment_column_key_v2, encode_comment_extension_key_v2, encode_comment_function_key_v2,
    encode_comment_prefix_v2, encode_comment_table_key_v2,
};
pub use serialization::{
    deserialize_function_def, deserialize_materialized_view_def, deserialize_row,
    deserialize_schema, deserialize_view_def, serialize_function_def,
    serialize_materialized_view_def, serialize_row, serialize_schema, serialize_view_def,
};
pub use value_encoding::{decode_pk_from_index_suffix, decode_value_memcomparable};

// Re-export internal constants needed by tests.
#[cfg(test)]
use metadata_keys::{
    WORKER_BG_RESULT_PREFIX, WORKER_CLAIM_PREFIX, WORKER_QUEUE_PREFIX, WORKER_REGISTRY_PREFIX,
};

// === Keyspace-level constants ===

/// System key prefixes
pub(super) const SYS_NEXT_TABLE_ID: &[u8] = b"_sys_next_table_id";
pub(super) const SYS_NEXT_DATABASE_ID: &[u8] = b"_sys_next_database_id";
pub(super) const SYS_FORMAT_VERSION: &[u8] = b"_sys_format_version";
pub(super) const SYS_DATABASE_BY_NAME_PREFIX: &[u8] = b"_sys_dbname_";
pub(super) const SYS_DATABASE_BY_ID_PREFIX: &[u8] = b"_sys_dbid_";
pub(super) const SYS_MIGRATION_PREFIX: &[u8] = b"_sys_migration_";

// === Storage format v2 (database-scoped prefixes) ===
//
// Keyspace-level metadata remains under `_sys_*` keys. Database-local metadata
// and user data are partitioned by `database_id` using a fixed binary prefix.
//
// Database prefix format: `d_{db_id:8bytes}_`
pub(super) const DATABASE_DATA_PREFIX: &[u8] = b"d_";

pub(super) const SYS_SCHEMA_PREFIX: &[u8] = b"_sys_schema_";
pub(super) const TABLE_DATA_PREFIX: &[u8] = b"t_";
pub(super) const TABLE_INDEX_PREFIX: &[u8] = b"i_";
pub(super) const TABLE_GIN_MARKER: &[u8] = b"gin_";

// GIN keys end the fixed prefix with a separator byte to allow prefix range scans:
//   ... gin_{hash}[SEP]{pk...}
// We use 0x00 for the scan start and 0x01 for the scan end (exclusive).
pub(super) const GIN_PK_SEP_START: u8 = 0x00;

// === Keyspace-level key construction ===

/// Encode the system key for next table ID
pub fn encode_next_table_id_key() -> Vec<u8> {
    SYS_NEXT_TABLE_ID.to_vec()
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

/// Decode the table ID embedded in a database-scoped row or index mutation key.
///
/// Returns `None` for non-table keys (schema metadata, worker state, etc.).
pub(crate) fn decode_table_id_from_mutation_key_v2(key: &[u8]) -> Option<u64> {
    let rest = key.strip_prefix(DATABASE_DATA_PREFIX)?;
    let (_db_id, rest) = rest.split_at(8);
    let rest = rest.strip_prefix(b"_")?;

    let table_prefix = if rest.starts_with(TABLE_DATA_PREFIX) {
        TABLE_DATA_PREFIX
    } else if rest.starts_with(TABLE_INDEX_PREFIX) {
        TABLE_INDEX_PREFIX
    } else {
        return None;
    };

    let table_bytes: [u8; 8] = rest.strip_prefix(table_prefix)?.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(table_bytes))
}
