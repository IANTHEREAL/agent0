//! Schema, view, function, trigger, cron, worker, and comment system key construction.
//!
//! All database-scoped metadata keys are built on top of `encode_database_data_prefix()`.
//! Worker system keys are global (not per-database).

use anyhow::Result;

use super::{encode_database_data_prefix, SYS_MIGRATION_PREFIX};

// Database-scoped metadata prefixes (used only in this module).
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
const DB_SYS_VIEW_BINDINGS_PREFIX: &[u8] = b"sys_view_bindings_";
const DB_SYS_MATVIEW_BINDINGS_PREFIX: &[u8] = b"sys_matview_bindings_";
const DB_SYS_PROCEDURE_PREFIX: &[u8] = b"sys_proc_";
const DB_SYS_FUNCTION_PREFIX: &[u8] = b"sys_func_";
const DB_SYS_TRIGGER_PREFIX: &[u8] = b"sys_trigger_";
const DB_SYS_TYPE_PREFIX: &[u8] = b"sys_type_";
const DB_SYS_SEQUENCEDEF_PREFIX: &[u8] = b"sys_seqdef_";
const DB_SYS_EXTENSION_PREFIX: &[u8] = b"sys_ext_";
const DB_SYS_EXTENSIONCFG_PREFIX: &[u8] = b"sys_extcfg_";
const DB_SYS_EMBEDDING_USAGE: &[u8] = b"sys_embedding_usage_";
const DB_SYS_COMMENT_PREFIX: &[u8] = b"sys_comment_";
const DB_SYS_RELNAME_PREFIX: &[u8] = b"sys_relname_";
const DB_SYS_SEQ_PREFIX: &[u8] = b"sys_seq_";
const DB_SYS_STATS_PREFIX: &[u8] = b"sys_stats_";
const DB_SYS_COLLATION_PREFIX: &[u8] = b"sys_collation_";
const DB_SYS_POLICY_PREFIX: &[u8] = b"sys_policy_";
const DB_SYS_NEXT_POLICY_OID: &[u8] = b"sys_next_policy_oid";
const DB_SYS_TSC_PREFIX: &[u8] = b"sys_tsc_";
const DB_SYS_CRON_JOB_PREFIX_V2: &[u8] = b"sys_cron_job_";
const DB_SYS_CRON_RUN_PREFIX_V2: &[u8] = b"sys_cron_run_";
const DB_SYS_CRON_SEQ_PREFIX_V2: &[u8] = b"sys_next_cron_job_id";
const DB_SYS_CRON_RUN_SEQ_PREFIX_V2: &[u8] = b"sys_next_cron_run_id";
const DB_SYS_CRON_ENABLED_PREFIX_V2: &[u8] = b"sys_cron_enabled";
const DB_SYS_CRON_CLAIM_PREFIX_V2: &[u8] = b"sys_cron_claim_";
const DB_SYS_CRON_RUNNING_GUARD_PREFIX_V2: &[u8] = b"sys_cron_running_guard_";

// Worker system prefixes (global, not per-database)
pub(super) const WORKER_REGISTRY_PREFIX: &[u8] = b"_worker_registry_";
pub(super) const WORKER_QUEUE_PREFIX: &[u8] = b"_worker_queue_";
pub(super) const WORKER_CLAIM_PREFIX: &[u8] = b"_worker_claim_";
pub(super) const WORKER_BG_RESULT_PREFIX: &[u8] = b"_worker_bg_result_";
pub(super) const WORKER_BG_TASK_SEQ_PREFIX: &[u8] = b"_worker_bg_task_seq_";

// ============================================================================
// Migration keys
// ============================================================================

pub fn encode_migration_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(SYS_MIGRATION_PREFIX.len() + name.len());
    key.extend_from_slice(SYS_MIGRATION_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

pub fn encode_migration_prefix() -> Vec<u8> {
    SYS_MIGRATION_PREFIX.to_vec()
}

// ============================================================================
// Database-scoped OID allocators
// ============================================================================

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

pub fn encode_next_policy_oid_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_NEXT_POLICY_OID);
    key
}

// ============================================================================
// Schema / table metadata keys
// ============================================================================

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

// ============================================================================
// Type / sequence / stats / relname keys
// ============================================================================

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

// ============================================================================
// Collation keys
// ============================================================================

pub fn encode_collation_key_v2(db_id: u64, name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_COLLATION_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

pub fn encode_collation_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_COLLATION_PREFIX);
    key
}

// ============================================================================
// Text search configuration keys
// ============================================================================

pub fn encode_tsc_key_v2(db_id: u64, config_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_TSC_PREFIX);
    key.extend_from_slice(config_name.as_bytes());
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

// ============================================================================
// Extension keys
// ============================================================================

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

pub fn encode_embedding_usage_key_v2(db_id: u64, date_yyyymmdd: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_EMBEDDING_USAGE);
    key.extend_from_slice(date_yyyymmdd.as_bytes());
    key
}

// ============================================================================
// Cron system keys
// ============================================================================

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

/// Encode a cron running-guard key to prevent overlapping runs of the same job.
///
/// Format: `d_{db_id:8bytes}_sys_cron_running_guard_{job_id:be8}`
/// Value: big-endian i64 of the current run_id
pub fn encode_cron_running_guard_key_v2(db_id: u64, job_id: i64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_RUNNING_GUARD_PREFIX_V2);
    key.extend_from_slice(&job_id.to_be_bytes());
    key
}

pub fn encode_cron_running_guard_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_RUNNING_GUARD_PREFIX_V2);
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
/// Format: `_worker_queue_{priority:u8}_{fire_time_ms:memcomparable}_{task_type:u8}_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}`
///
/// Priority byte comes first so lower values (higher priority) sort first.
/// Fire time uses memcomparable encoding so earlier times sort first (handles negative values correctly).
pub fn encode_worker_queue_key(
    priority: u8,
    fire_time_ms: i64,
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
) -> Result<Vec<u8>> {
    let mut key =
        Vec::with_capacity(WORKER_QUEUE_PREFIX.len() + 1 + 8 + 1 + 2 + keyspace.len() + 1 + 8 + 8);
    key.extend_from_slice(WORKER_QUEUE_PREFIX);
    key.push(priority);
    key.extend(memcomparable::to_vec(&fire_time_ms)?);
    key.push(task_type);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&task_id.to_be_bytes());
    Ok(key)
}

/// Encode the prefix for all worker queue keys (global).
pub fn encode_worker_queue_prefix() -> Vec<u8> {
    WORKER_QUEUE_PREFIX.to_vec()
}

/// Encode the exclusive upper bound for a worker queue range scan.
///
/// Used to scan all queue entries with a given priority and fire_time.
/// Format: `_worker_queue_{priority:u8}_{fire_time_ms:memcomparable}` (no keyspace/db_id/task_id)
pub fn encode_worker_queue_scan_end(priority: u8, fire_time_ms: i64) -> Result<Vec<u8>> {
    let mut key = Vec::with_capacity(WORKER_QUEUE_PREFIX.len() + 1 + 8);
    key.extend_from_slice(WORKER_QUEUE_PREFIX);
    key.push(priority);
    key.extend(memcomparable::to_vec(&fire_time_ms)?);
    Ok(key)
}

/// Decode fire_time_ms from a worker queue key.
#[cfg(test)]
pub fn decode_worker_queue_fire_time(key: &[u8]) -> Option<i64> {
    use memcomparable::Deserializer;
    if key.len() < WORKER_QUEUE_PREFIX.len() + 1 + 8 {
        return None;
    }
    let offset = WORKER_QUEUE_PREFIX.len() + 1;
    let payload = &key[offset..];
    let mut deserializer = Deserializer::new(payload);
    serde::Deserialize::deserialize(&mut deserializer).ok()
}

/// Decode task_type from a worker queue key.
#[cfg(test)]
pub fn decode_worker_queue_task_type(key: &[u8]) -> Option<u8> {
    let offset = WORKER_QUEUE_PREFIX.len() + 1 + 8;
    key.get(offset).copied()
}

/// Encode a worker claim key (global).
///
/// Format: `_worker_claim_{task_type:u8}_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}_{fire_time_min:be8}`
pub fn encode_worker_claim_key(
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
    fire_time_min: i64,
) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(WORKER_CLAIM_PREFIX.len() + 1 + 2 + keyspace.len() + 1 + 8 + 1 + 8 + 8);
    key.extend_from_slice(WORKER_CLAIM_PREFIX);
    key.push(task_type);
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

/// Encode a worker background task sequence key (global, per tenant-db).
///
/// Format: `_worker_bg_task_seq_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}`
///
/// Used as the CAS-protected counter key for collision-free bg task ID allocation.
pub fn encode_worker_bg_task_seq_key(keyspace: &str, db_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(WORKER_BG_TASK_SEQ_PREFIX.len() + 2 + keyspace.len() + 1 + 8);
    key.extend_from_slice(WORKER_BG_TASK_SEQ_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key
}

// ============================================================================
// View / materialized view keys
// ============================================================================

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

pub fn encode_view_bindings_key_v2(db_id: u64, view_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_VIEW_BINDINGS_PREFIX);
    key.extend_from_slice(view_name.as_bytes());
    key
}

pub fn encode_view_bindings_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_VIEW_BINDINGS_PREFIX);
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

pub fn encode_matview_bindings_key_v2(db_id: u64, matview_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_MATVIEW_BINDINGS_PREFIX);
    key.extend_from_slice(matview_name.as_bytes());
    key
}

pub fn encode_matview_bindings_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_MATVIEW_BINDINGS_PREFIX);
    key
}

// ============================================================================
// Procedure / function keys
// ============================================================================

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

// ============================================================================
// Trigger keys
// ============================================================================

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

// ============================================================================
// RLS Policy keys
// ============================================================================

/// Key for a specific policy: `d_{db_id}_sys_policy_{table_id}/{policy_name}`
pub fn encode_policy_key_v2(db_id: u64, table_id: u64, policy_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_POLICY_PREFIX);
    key.extend_from_slice(table_id.to_string().as_bytes());
    key.push(b'/');
    key.extend_from_slice(policy_name.as_bytes());
    key
}

/// Prefix for all policies in a database: `d_{db_id}_sys_policy_`
pub fn encode_policy_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_POLICY_PREFIX);
    key
}

/// Prefix for all policies on a specific table: `d_{db_id}_sys_policy_{table_id}/`
pub fn encode_policy_table_prefix_v2(db_id: u64, table_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_POLICY_PREFIX);
    key.extend_from_slice(table_id.to_string().as_bytes());
    key.push(b'/');
    key
}

// ============================================================================
// Comment keys
// ============================================================================

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_queue_fire_time_roundtrip_positive_and_negative() {
        let key_pos = encode_worker_queue_key(3, 1234567890, 1, "tenant_a", 42, 7).unwrap();
        let key_neg = encode_worker_queue_key(3, -987654321, 1, "tenant_a", 42, 7).unwrap();
        assert_eq!(decode_worker_queue_fire_time(&key_pos), Some(1234567890));
        assert_eq!(decode_worker_queue_fire_time(&key_neg), Some(-987654321));
    }

    #[test]
    fn worker_queue_task_type_roundtrip() {
        let key = encode_worker_queue_key(3, 1234567890, 0x10, "tenant_a", 42, 7).unwrap();
        assert_eq!(decode_worker_queue_task_type(&key), Some(0x10));
    }

    #[test]
    fn decode_worker_queue_fire_time_rejects_short_keys() {
        let too_short = vec![0u8; WORKER_QUEUE_PREFIX.len()];
        assert_eq!(decode_worker_queue_fire_time(&too_short), None);
        let no_payload = {
            let mut k = WORKER_QUEUE_PREFIX.to_vec();
            k.push(1);
            k
        };
        assert_eq!(decode_worker_queue_fire_time(&no_payload), None);
    }

    #[test]
    fn worker_registry_key_encodes_keyspace_length_and_db_id() {
        let key = encode_worker_registry_key("ks", 9);
        assert!(key.starts_with(WORKER_REGISTRY_PREFIX));

        let len_offset = WORKER_REGISTRY_PREFIX.len();
        let len = u16::from_be_bytes([key[len_offset], key[len_offset + 1]]) as usize;
        assert_eq!(len, 2);
        assert_eq!(&key[len_offset + 2..len_offset + 2 + len], b"ks");
        assert_eq!(key[len_offset + 2 + len], b'_');
        assert_eq!(
            &key[len_offset + 2 + len + 1..len_offset + 2 + len + 1 + 8],
            &9u64.to_be_bytes()
        );
    }

    #[test]
    fn worker_queue_scan_end_matches_key_prefix_for_same_priority_and_fire_time() {
        let fire_time = 1000_i64;
        let prefix = encode_worker_queue_scan_end(5, fire_time).unwrap();
        let key = encode_worker_queue_key(5, fire_time, 1, "k", 1, 2).unwrap();
        assert!(key.starts_with(&prefix));

        let later = encode_worker_queue_scan_end(5, fire_time + 1).unwrap();
        assert!(prefix < later);
    }

    #[test]
    fn worker_bg_task_seq_key_encodes_keyspace_and_db_id() {
        let key = encode_worker_bg_task_seq_key("ks", 9);
        assert!(key.starts_with(WORKER_BG_TASK_SEQ_PREFIX));

        let len_offset = WORKER_BG_TASK_SEQ_PREFIX.len();
        let len = u16::from_be_bytes([key[len_offset], key[len_offset + 1]]) as usize;
        assert_eq!(len, 2);
        assert_eq!(&key[len_offset + 2..len_offset + 2 + len], b"ks");
        assert_eq!(key[len_offset + 2 + len], b'_');
        assert_eq!(
            &key[len_offset + 2 + len + 1..len_offset + 2 + len + 1 + 8],
            &9u64.to_be_bytes()
        );
    }

    #[test]
    fn worker_bg_task_seq_key_differs_across_tenant_db_scopes() {
        let key_a = encode_worker_bg_task_seq_key("ks_a", 1);
        let key_b = encode_worker_bg_task_seq_key("ks_b", 1);
        let key_c = encode_worker_bg_task_seq_key("ks_a", 2);
        assert_ne!(key_a, key_b);
        assert_ne!(key_a, key_c);
        assert_ne!(key_b, key_c);
    }

    #[test]
    fn comment_column_key_contains_table_and_column_with_zero_separators() {
        let key = encode_comment_column_key_v2(1, "public.t", "c1");
        let prefix = encode_comment_prefix_v2(1);
        assert!(key.starts_with(&prefix));
        let payload = &key[prefix.len()..];

        // Format: 'c' + 0 + table + 0 + column
        assert_eq!(payload[0], b'c');
        assert_eq!(payload[1], 0);
        let rest = &payload[2..];
        let split_at = rest.iter().position(|b| *b == 0).expect("separator exists");
        assert_eq!(&rest[..split_at], b"public.t");
        assert_eq!(&rest[split_at + 1..], b"c1");
    }
}
