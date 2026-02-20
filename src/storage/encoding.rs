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
use std::sync::Once;

/// System key prefixes
const SYS_NEXT_TABLE_ID: &[u8] = b"_sys_next_table_id";
const SYS_NEXT_DATABASE_ID: &[u8] = b"_sys_next_database_id";
const SYS_FORMAT_VERSION: &[u8] = b"_sys_format_version";
const SYS_DATABASE_BY_NAME_PREFIX: &[u8] = b"_sys_dbname_";
const SYS_DATABASE_BY_ID_PREFIX: &[u8] = b"_sys_dbid_";
const SYS_MIGRATION_PREFIX: &[u8] = b"_sys_migration_";

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
const DB_SYS_RELNAME_PREFIX: &[u8] = b"sys_relname_";
const DB_SYS_SEQ_PREFIX: &[u8] = b"sys_seq_";
const DB_SYS_STATS_PREFIX: &[u8] = b"sys_stats_";
const DB_SYS_CRON_JOB_PREFIX_V2: &[u8] = b"sys_cron_job_";
const DB_SYS_CRON_RUN_PREFIX_V2: &[u8] = b"sys_cron_run_";
const DB_SYS_CRON_SEQ_PREFIX_V2: &[u8] = b"sys_next_cron_job_id";
const DB_SYS_CRON_RUN_SEQ_PREFIX_V2: &[u8] = b"sys_next_cron_run_id";
const DB_SYS_CRON_ENABLED_PREFIX_V2: &[u8] = b"sys_cron_enabled";
const DB_SYS_CRON_CLAIM_PREFIX_V2: &[u8] = b"sys_cron_claim_";

// Worker system prefixes (global, not per-database)
const WORKER_REGISTRY_PREFIX: &[u8] = b"_worker_registry_";
const WORKER_QUEUE_PREFIX: &[u8] = b"_worker_queue_";
const WORKER_CLAIM_PREFIX: &[u8] = b"_worker_claim_";
const WORKER_BG_RESULT_PREFIX: &[u8] = b"_worker_bg_result_";

const SYS_SCHEMA_PREFIX: &[u8] = b"_sys_schema_";
const TABLE_DATA_PREFIX: &[u8] = b"t_";
const TABLE_INDEX_PREFIX: &[u8] = b"i_";
const TABLE_GIN_MARKER: &[u8] = b"gin_";

// GIN keys end the fixed prefix with a separator byte to allow prefix range scans:
//   ... gin_{hash}[SEP]{pk...}
// We use 0x00 for the scan start and 0x01 for the scan end (exclusive).
const GIN_PK_SEP_START: u8 = 0x00;
const LEGACY_SCHEMA_DESERIALIZATION_SUNSET_DATE: &str = "2026-12-31";

fn warn_legacy_schema_deserialization_once() {
    static WARN_ONCE: Once = Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            sunset_date = LEGACY_SCHEMA_DESERIALIZATION_SUNSET_DATE,
            commit = "ce73a8a",
            "legacy schema deserialization fallback is active; remove after all persisted schemas include IndexDef.state"
        );
    });
}

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

pub fn encode_migration_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(SYS_MIGRATION_PREFIX.len() + name.len());
    key.extend_from_slice(SYS_MIGRATION_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

pub fn encode_migration_prefix() -> Vec<u8> {
    SYS_MIGRATION_PREFIX.to_vec()
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

/// Encode the key for persisted table statistics (storage format v2, database-scoped).
///
/// Key format: `d_{db_id:8bytes}_sys_stats_{table_id:8bytes}`
pub fn encode_stats_key_v2(db_id: u64, table_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_STATS_PREFIX);
    key.extend_from_slice(&table_id.to_be_bytes());
    key
}

/// Encode a relation-name reservation key (storage format v2, database-scoped).
///
/// Used to enforce schema-wide index name uniqueness via TiKV write-write
/// conflict detection. The value stored is a single-byte tag (e.g. `b'I'`
/// for index).
pub fn encode_relname_key_v2(db_id: u64, full_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_RELNAME_PREFIX);
    key.extend_from_slice(full_name.as_bytes());
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

pub fn encode_cron_job_key_v2(db_id: u64, job_id: i64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_JOB_PREFIX_V2);
    key.extend_from_slice(&job_id.to_be_bytes());
    key
}

pub fn encode_cron_job_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_JOB_PREFIX_V2);
    key
}

pub fn encode_cron_run_key_v2(db_id: u64, run_id: i64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_RUN_PREFIX_V2);
    key.extend_from_slice(&run_id.to_be_bytes());
    key
}

pub fn encode_cron_run_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_RUN_PREFIX_V2);
    key
}

pub fn encode_cron_claim_key_v2(db_id: u64, job_id: i64, scheduled_min: i64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_CLAIM_PREFIX_V2);
    key.extend_from_slice(&job_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&scheduled_min.to_be_bytes());
    key
}

pub fn encode_cron_claim_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_CLAIM_PREFIX_V2);
    key
}

pub fn encode_next_cron_job_id_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_SEQ_PREFIX_V2);
    key
}

pub fn encode_next_cron_run_id_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_RUN_SEQ_PREFIX_V2);
    key
}

pub fn encode_cron_enabled_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_ENABLED_PREFIX_V2);
    key
}

// ============================================================================
// Worker System Keys (Global, not per-database)
// ============================================================================

/// Encode a worker registry key (global).
///
/// Format: `_worker_registry_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}`
pub fn encode_worker_registry_key(keyspace: &str, db_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(WORKER_REGISTRY_PREFIX.len() + 2 + keyspace.len() + 1 + 8);
    key.extend_from_slice(WORKER_REGISTRY_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key
}

/// Encode the prefix for all worker registry keys (global).
pub fn encode_worker_registry_prefix() -> Vec<u8> {
    WORKER_REGISTRY_PREFIX.to_vec()
}

/// Encode a worker queue key (global).
///
/// Format: `_worker_queue_{priority:u8}_{fire_time_ms:memcomparable}_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}`
///
/// Priority byte comes first so lower values (higher priority) sort first.
/// Fire time uses memcomparable encoding so earlier times sort first (handles negative values correctly).
pub fn encode_worker_queue_key(
    priority: u8,
    fire_time_ms: i64,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(WORKER_QUEUE_PREFIX.len() + 1 + 8 + 2 + keyspace.len() + 1 + 8 + 8);
    key.extend_from_slice(WORKER_QUEUE_PREFIX);
    key.push(priority);
    key.extend(memcomparable::to_vec(&fire_time_ms).unwrap());
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&task_id.to_be_bytes());
    key
}

/// Encode the prefix for all worker queue keys (global).
pub fn encode_worker_queue_prefix() -> Vec<u8> {
    WORKER_QUEUE_PREFIX.to_vec()
}

/// Encode the exclusive upper bound for a worker queue range scan.
///
/// Used to scan all queue entries with a given priority and fire_time.
/// Format: `_worker_queue_{priority:u8}_{fire_time_ms:memcomparable}` (no keyspace/db_id/task_id)
pub fn encode_worker_queue_scan_end(priority: u8, fire_time_ms: i64) -> Vec<u8> {
    let mut key = Vec::with_capacity(WORKER_QUEUE_PREFIX.len() + 1 + 8);
    key.extend_from_slice(WORKER_QUEUE_PREFIX);
    key.push(priority);
    key.extend(memcomparable::to_vec(&fire_time_ms).unwrap());
    key
}

/// Decode fire_time_ms from a worker queue key.
///
/// Extracts the fire_time field from a queue key for sorting/filtering.
/// Returns None if the key is too short or malformed.
pub fn decode_worker_queue_fire_time(key: &[u8]) -> Option<i64> {
    if key.len() < WORKER_QUEUE_PREFIX.len() + 1 {
        return None;
    }
    let offset = WORKER_QUEUE_PREFIX.len() + 1;
    let payload = &key[offset..];
    let mut deserializer = Deserializer::new(payload);
    serde::Deserialize::deserialize(&mut deserializer).ok()
}

/// Encode a worker claim key (global).
///
/// Format: `_worker_claim_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}_{fire_time_min:be8}`
pub fn encode_worker_claim_key(
    keyspace: &str,
    db_id: u64,
    task_id: i64,
    fire_time_min: i64,
) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(WORKER_CLAIM_PREFIX.len() + 2 + keyspace.len() + 1 + 8 + 1 + 8 + 8);
    key.extend_from_slice(WORKER_CLAIM_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&task_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&fire_time_min.to_be_bytes());
    key
}

/// Encode the prefix for all worker claim keys (global).
pub fn encode_worker_claim_prefix() -> Vec<u8> {
    WORKER_CLAIM_PREFIX.to_vec()
}

/// Encode a worker background result key (global).
///
/// Format: `_worker_bg_result_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}`
pub fn encode_worker_bg_result_key(keyspace: &str, db_id: u64, task_id: i64) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(WORKER_BG_RESULT_PREFIX.len() + 2 + keyspace.len() + 1 + 8 + 1 + 8);
    key.extend_from_slice(WORKER_BG_RESULT_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&task_id.to_be_bytes());
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
        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::UserDefined(_) => {
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

/// Deserialize a table schema.
///
/// Handles backward compatibility: schemas serialized before the `IndexDef.state`
/// field was added (pre-`ce73a8a`) are transparently upgraded by falling back to
/// a legacy struct layout when the primary deserialization fails.
pub fn deserialize_schema(data: &[u8]) -> Result<TableSchema> {
    const SCHEMA_MAGIC: &[u8] = b"PGTIKV_SCHEMA_V1\0";

    let payload = data.strip_prefix(SCHEMA_MAGIC).context(
        "Schema data missing PGTIKV_SCHEMA_V1 header (V1 legacy format no longer supported)",
    )?;

    // Try current format first.
    if let Ok(schema) = bincode::deserialize::<TableSchema>(payload) {
        return Ok(schema);
    }

    // Fallback: deserialize with legacy IndexDef (no `state` field), then upgrade.
    // Sunset policy: remove after 2026-12-31 once all keyspaces are migrated.
    warn_legacy_schema_deserialization_once();
    let legacy: TableSchemaLegacy = bincode::deserialize(payload)
        .context("Failed to deserialize schema (tried both current and legacy formats)")?;
    Ok(legacy.into())
}

/// Legacy IndexDef without the `state` field (added in ce73a8a).
#[derive(serde::Deserialize)]
struct IndexDefLegacy {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    pub method: Option<String>,
    pub predicate: Option<String>,
    pub expressions: Vec<String>,
}

/// Legacy TableSchema matching the pre-ce73a8a serialization format.
#[derive(serde::Deserialize)]
struct TableSchemaLegacy {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<crate::types::ColumnDef>,
    pub version: u64,
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<IndexDefLegacy>,
    pub check_constraints: Vec<crate::types::CheckConstraint>,
    pub foreign_keys: Vec<crate::types::ForeignKeyConstraint>,
    pub owner: String,
}

impl From<TableSchemaLegacy> for TableSchema {
    fn from(legacy: TableSchemaLegacy) -> Self {
        use crate::types::IndexDef;
        use crate::worker::types::IndexState;

        TableSchema {
            name: legacy.name,
            table_id: legacy.table_id,
            columns: legacy.columns,
            version: legacy.version,
            pk_constraint_name: legacy.pk_constraint_name,
            pk_indices: legacy.pk_indices,
            indexes: legacy
                .indexes
                .into_iter()
                .map(|idx| IndexDef {
                    name: idx.name,
                    id: idx.id,
                    columns: idx.columns,
                    unique: idx.unique,
                    method: idx.method,
                    predicate: idx.predicate,
                    expressions: idx.expressions,
                    state: IndexState::Ready,
                })
                .collect(),
            check_constraints: legacy.check_constraints,
            foreign_keys: legacy.foreign_keys,
            owner: legacy.owner,
            from_alias: None,
        }
    }
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

/// Deserialize a function definition.
pub fn deserialize_function_def(data: &[u8]) -> Result<crate::types::FunctionDef> {
    const FUNCTION_MAGIC: &[u8] = b"PGTIKV_FUNCTION_V1\0";

    let payload = data.strip_prefix(FUNCTION_MAGIC).context(
        "Function data missing PGTIKV_FUNCTION_V1 header (V1 legacy format no longer supported)",
    )?;
    bincode::deserialize(payload).context("Failed to deserialize function definition")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};

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
    fn test_encode_migration_key() {
        let key = encode_migration_key("20260212_000001_init");
        assert_eq!(key, b"_sys_migration_20260212_000001_init".to_vec());
    }

    #[test]
    fn test_encode_migration_prefix() {
        assert_eq!(encode_migration_prefix(), b"_sys_migration_".to_vec());
    }

    #[test]
    fn test_encode_stats_key_v2() {
        let key = encode_stats_key_v2(1, 42);
        let mut expected = encode_database_data_prefix(1);
        expected.extend_from_slice(b"sys_stats_");
        expected.extend_from_slice(&42_u64.to_be_bytes());
        assert_eq!(key, expected);
    }

    #[test]
    fn test_encode_stats_key_v2_different_tables() {
        let key_a = encode_stats_key_v2(1, 10);
        let key_b = encode_stats_key_v2(1, 20);
        assert_ne!(key_a, key_b);
        // Keys for same db should share the database prefix
        let prefix = encode_database_data_prefix(1);
        assert!(key_a.starts_with(&prefix));
        assert!(key_b.starts_with(&prefix));
    }

    #[test]
    fn test_encode_stats_key_v2_different_databases() {
        let key_a = encode_stats_key_v2(1, 42);
        let key_b = encode_stats_key_v2(2, 42);
        assert_ne!(key_a, key_b);
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
            from_alias: None,
        };
        let serialized = serialize_schema(&schema).unwrap();
        let deserialized = deserialize_schema(&serialized).unwrap();
        assert_eq!(deserialized.name, schema.name);
        assert_eq!(deserialized.table_id, schema.table_id);
        assert_eq!(deserialized.columns.len(), 2);
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
    fn test_encode_index_range_start_end_int32() {
        let start = encode_index_range_start_v2(1, 10, 20, &[], Some(&Value::Int32(10)), true);
        let end = encode_index_range_end_v2(1, 10, 20, &[], Some(&Value::Int32(20)), true);

        let key_before = encode_index_key_v2(1, 10, 20, &[Value::Int32(9)], None);
        let key_after = encode_index_key_v2(1, 10, 20, &[Value::Int32(21)], None);
        assert!(key_before < start);
        assert!(key_after >= end);

        for i in 10..=20 {
            let key = encode_index_key_v2(1, 10, 20, &[Value::Int32(i)], None);
            assert!(key >= start, "key should be >= range start for {i}");
            assert!(key < end, "key should be < range end for {i}");
        }
    }

    #[test]
    fn test_encode_index_range_exclusive_bounds() {
        let start = encode_index_range_start_v2(1, 10, 20, &[], Some(&Value::Int32(10)), false);
        let end = encode_index_range_end_v2(1, 10, 20, &[], Some(&Value::Int32(20)), false);

        let key_10 = encode_index_key_v2(1, 10, 20, &[Value::Int32(10)], None);
        let key_11 = encode_index_key_v2(1, 10, 20, &[Value::Int32(11)], None);
        let key_19 = encode_index_key_v2(1, 10, 20, &[Value::Int32(19)], None);
        let key_20 = encode_index_key_v2(1, 10, 20, &[Value::Int32(20)], None);

        assert!(key_10 < start);
        assert!(key_11 >= start);
        assert!(key_19 < end);
        assert!(key_20 >= end);
    }

    #[test]
    fn test_encode_index_range_with_prefix() {
        let start = encode_index_range_start_v2(
            1,
            10,
            20,
            &[Value::Int32(1)],
            Some(&Value::Int32(5)),
            true,
        );

        let key_14 = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(4)], None);
        let key_15 = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(5)], None);
        let key_16 = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(6)], None);

        assert!(key_14 < start);
        assert!(key_15 >= start);
        assert!(start < key_16);
    }

    #[test]
    fn test_encode_index_range_null_sorts_first() {
        let start = encode_index_range_start_v2(1, 10, 20, &[], Some(&Value::Int32(5)), false);

        let key_null = encode_index_key_v2(1, 10, 20, &[Value::Null], None);
        let key_5 = encode_index_key_v2(1, 10, 20, &[Value::Int32(5)], None);
        let key_6 = encode_index_key_v2(1, 10, 20, &[Value::Int32(6)], None);

        assert!(key_null < start);
        assert!(key_5 < start);
        assert!(key_6 >= start);
    }

    #[test]
    fn test_encode_index_range_unbounded() {
        let start = encode_index_range_start_v2(1, 10, 20, &[Value::Int32(1)], None, true);
        let end = encode_index_range_end_v2(1, 10, 20, &[Value::Int32(1)], None, true);

        let key_low = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(-100)], None);
        let key_high = encode_index_key_v2(1, 10, 20, &[Value::Int32(1), Value::Int32(999)], None);

        assert_eq!(
            start,
            encode_index_key_v2(1, 10, 20, &[Value::Int32(1)], None)
        );
        assert!(start <= key_low);
        assert!(key_high < end);
    }

    #[test]
    fn test_composite_key_ordering() {
        let key1 = encode_pk_values(&[Value::Int32(1), Value::Text("a".to_string())]);
        let key2 = encode_pk_values(&[Value::Int32(1), Value::Text("b".to_string())]);
        let key3 = encode_pk_values(&[Value::Int32(2), Value::Text("a".to_string())]);
        assert!(key1 < key2, "same first col, second col determines order");
        assert!(key2 < key3, "first col determines order");
    }

    // ========================================================================
    // Worker System Key Tests
    // ========================================================================

    #[test]
    fn test_encode_worker_registry_key() {
        let key = encode_worker_registry_key("myapp", 42);
        assert!(key.starts_with(WORKER_REGISTRY_PREFIX));
        // Verify keyspace length encoding
        let keyspace_len_bytes =
            &key[WORKER_REGISTRY_PREFIX.len()..WORKER_REGISTRY_PREFIX.len() + 2];
        let keyspace_len = u16::from_be_bytes([keyspace_len_bytes[0], keyspace_len_bytes[1]]);
        assert_eq!(keyspace_len, 5); // "myapp" is 5 bytes
    }

    #[test]
    fn test_encode_worker_registry_prefix() {
        let prefix = encode_worker_registry_prefix();
        assert_eq!(prefix, WORKER_REGISTRY_PREFIX);
    }

    #[test]
    fn test_encode_worker_queue_key() {
        let key = encode_worker_queue_key(10, 1000, "myapp", 42, 100);
        assert!(key.starts_with(WORKER_QUEUE_PREFIX));
        // Verify priority byte is at correct position
        let priority_byte = key[WORKER_QUEUE_PREFIX.len()];
        assert_eq!(priority_byte, 10);
    }

    #[test]
    fn test_worker_queue_key_priority_ordering() {
        let key_p0 = encode_worker_queue_key(0, 1000, "myapp", 42, 100);
        let key_p128 = encode_worker_queue_key(128, 1000, "myapp", 42, 100);
        assert!(key_p0 < key_p128, "lower priority value should sort first");
    }

    #[test]
    fn test_worker_queue_key_fire_time_ordering() {
        let key_t1000 = encode_worker_queue_key(0, 1000, "myapp", 42, 100);
        let key_t2000 = encode_worker_queue_key(0, 2000, "myapp", 42, 100);
        assert!(key_t1000 < key_t2000, "earlier fire_time should sort first");
    }

    #[test]
    fn test_encode_worker_queue_prefix() {
        let prefix = encode_worker_queue_prefix();
        assert_eq!(prefix, WORKER_QUEUE_PREFIX);
    }

    #[test]
    fn test_encode_worker_queue_scan_end() {
        let scan_end = encode_worker_queue_scan_end(10, 1000);
        assert!(scan_end.starts_with(WORKER_QUEUE_PREFIX));
        let priority_byte = scan_end[WORKER_QUEUE_PREFIX.len()];
        assert_eq!(priority_byte, 10);
    }

    #[test]
    fn test_decode_worker_queue_fire_time() {
        let key = encode_worker_queue_key(10, 1234567890, "myapp", 42, 100);
        let fire_time = decode_worker_queue_fire_time(&key).expect("decode");
        assert_eq!(fire_time, 1234567890);
    }

    #[test]
    fn test_decode_worker_queue_fire_time_roundtrip() {
        let original_time = 9876543210i64;
        let key = encode_worker_queue_key(5, original_time, "test", 1, 50);
        let decoded_time = decode_worker_queue_fire_time(&key).expect("decode");
        assert_eq!(decoded_time, original_time);
    }

    #[test]
    fn test_decode_worker_queue_fire_time_invalid_key() {
        let short_key = b"_worker_queue_";
        assert_eq!(decode_worker_queue_fire_time(short_key), None);
    }

    #[test]
    fn test_encode_worker_claim_key() {
        let key = encode_worker_claim_key("myapp", 42, 100, 5000);
        assert!(key.starts_with(WORKER_CLAIM_PREFIX));
        // Verify keyspace length encoding
        let keyspace_len_bytes = &key[WORKER_CLAIM_PREFIX.len()..WORKER_CLAIM_PREFIX.len() + 2];
        let keyspace_len = u16::from_be_bytes([keyspace_len_bytes[0], keyspace_len_bytes[1]]);
        assert_eq!(keyspace_len, 5); // "myapp" is 5 bytes
    }

    #[test]
    fn test_encode_worker_claim_prefix() {
        let prefix = encode_worker_claim_prefix();
        assert_eq!(prefix, WORKER_CLAIM_PREFIX);
    }

    #[test]
    fn test_worker_queue_key_priority_before_time() {
        // Higher priority (lower byte value) at later time should sort before lower priority at earlier time.
        // This proves that priority byte comes BEFORE fire_time in the key encoding.
        let key_high_late = encode_worker_queue_key(0, 2000, "ks", 1, 1);
        let key_low_early = encode_worker_queue_key(128, 1000, "ks", 1, 1);
        assert!(
            key_high_late < key_low_early,
            "priority must take precedence over fire_time"
        );
    }

    #[test]
    fn test_worker_queue_key_big_endian_fire_time() {
        let key_neg = encode_worker_queue_key(0, -1000i64, "app", 1, 1);
        let key_zero = encode_worker_queue_key(0, 0i64, "app", 1, 1);
        let key_pos = encode_worker_queue_key(0, 1000i64, "app", 1, 1);
        assert!(key_neg < key_zero, "negative fire_time should sort first");
        assert!(key_zero < key_pos, "zero should sort before positive");
    }

    #[test]
    fn test_worker_registry_key_different_keyspaces() {
        let key_a = encode_worker_registry_key("app_a", 42);
        let key_b = encode_worker_registry_key("app_b", 42);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_worker_registry_key_different_db_ids() {
        let key_1 = encode_worker_registry_key("myapp", 1);
        let key_2 = encode_worker_registry_key("myapp", 2);
        assert_ne!(key_1, key_2);
    }

    #[test]
    fn test_worker_claim_key_different_task_ids() {
        let key_1 = encode_worker_claim_key("myapp", 42, 100, 5000);
        let key_2 = encode_worker_claim_key("myapp", 42, 200, 5000);
        assert_ne!(key_1, key_2);
    }

    #[test]
    fn test_worker_registry_key_roundtrip_prefix() {
        let key = encode_worker_registry_key("tenant_x", 7);
        assert!(key.starts_with(b"_worker_registry_"));
        let prefix = encode_worker_registry_prefix();
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn test_worker_claim_key_structure() {
        let key = encode_worker_claim_key("demo", 10, 555, 9999);
        assert!(key.starts_with(b"_worker_claim_"));
        let prefix = encode_worker_claim_prefix();
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn test_worker_queue_key_same_time_same_priority_different_keyspace() {
        let key_a = encode_worker_queue_key(5, 1000, "ks_alpha", 1, 1);
        let key_b = encode_worker_queue_key(5, 1000, "ks_beta", 1, 1);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_worker_bg_result_key_structure() {
        let key = encode_worker_bg_result_key("myks", 3, 42);
        assert!(key.starts_with(b"_worker_bg_result_"));
        let ks_len_offset = WORKER_BG_RESULT_PREFIX.len();
        let ks_len = u16::from_be_bytes([key[ks_len_offset], key[ks_len_offset + 1]]);
        assert_eq!(ks_len, 4);
    }

    #[test]
    fn test_worker_queue_scan_end_boundary() {
        let scan_end = encode_worker_queue_scan_end(5, 2000);
        let key_before = encode_worker_queue_key(5, 1999, "ks", 1, 1);
        let key_at = encode_worker_queue_key(5, 2000, "ks", 1, 1);
        assert!(
            key_before < scan_end,
            "key with earlier fire_time should be < scan_end"
        );
        assert!(
            key_at >= scan_end,
            "key at fire_time should be >= scan_end (scan_end is prefix)"
        );
    }
}
