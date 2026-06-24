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
const DB_SYS_STORAGE_STATS: &[u8] = b"sys_storage_stats";
const DB_SYS_TSC_PREFIX: &[u8] = b"sys_tsc_";
const DB_SYS_CRON_JOB_PREFIX_V2: &[u8] = b"sys_cron_job_";
const DB_SYS_CRON_RUN_PREFIX_V2: &[u8] = b"sys_cron_run_";
const DB_SYS_CRON_SEQ_PREFIX_V2: &[u8] = b"sys_next_cron_job_id";
const DB_SYS_CRON_RUN_SEQ_PREFIX_V2: &[u8] = b"sys_next_cron_run_id";
const DB_SYS_CRON_ENABLED_PREFIX_V2: &[u8] = b"sys_cron_enabled";
const DB_SYS_CRON_CLAIM_PREFIX_V2: &[u8] = b"sys_cron_claim_";
const DB_SYS_CRON_RUNNING_GUARD_PREFIX_V2: &[u8] = b"sys_cron_running_guard_";
// Single-lifecycle control plane (design 35). CONTROL = per-(db,job,minute) per-fire
// state/dedup record; ACTIVE = per-(db,job) job-level no-overlap pointer. These
// supersede the dumb per-minute claim flag + running guard; both are written only
// inside the cron CAS helpers so neither can be orphaned. MIGRATED marks a db whose
// legacy guard/claim keys have been translated (fail-closed claim gate).
const DB_SYS_CRON_CONTROL_PREFIX_V2: &[u8] = b"sys_cron_control_";
const DB_SYS_CRON_ACTIVE_PREFIX_V2: &[u8] = b"sys_cron_active_";
const DB_SYS_CRON_MIGRATED_V3: &[u8] = b"sys_cron_migrated_v3";
const DB_SYS_DDL_JOURNAL_PREFIX: &[u8] = b"sys_ddl_journal_";

// Worker system prefixes (global, not per-database)
pub(super) const WORKER_REGISTRY_PREFIX: &[u8] = b"_worker_registry_";
pub(super) const WORKER_QUEUE_PREFIX: &[u8] = b"_worker_queue_";
pub(super) const WORKER_CLAIM_PREFIX: &[u8] = b"_worker_claim_";
pub(super) const WORKER_BG_RESULT_PREFIX: &[u8] = b"_worker_bg_result_";
pub(super) const GC_INSTANCE_STATE_PREFIX: &[u8] = b"_gc_instance_";
pub(super) const WORKER_BG_TASK_SEQ_PREFIX: &[u8] = b"_worker_bg_task_seq_";
/// V2 due-queue (global). Same key STRUCTURE as `_worker_queue_` so the
/// priority/fire_time ordering, scan bounds, and fire_time decode are reusable,
/// but a distinct prefix: old binaries only read `_worker_queue_`, so the
/// shrunk descriptor VALUE written here is invisible to them — making the
/// rollout/rollback safe. See issue #2576.
pub(super) const WORKER_QUEUE_V2_PREFIX: &[u8] = b"_wq_due_v2_";
/// V2 payload range (global): the large `command`/`username`/`schedule` of
/// split task types, keyed by full due identity so it is 1:1 with the due
/// entry and never participates in a due-queue scan.
pub(super) const WORKER_PAYLOAD_V2_PREFIX: &[u8] = b"_wq_payload_v2_";
/// V2 worker queue secondary index (global). Keyspace/db lead so every
/// task-targeted or tenant-targeted operation is a bounded prefix scan that
/// never reads the (potentially large) due-queue value. See issue #2576.
pub(super) const WORKER_QUEUE_INDEX_V2_PREFIX: &[u8] = b"_wq_idx_v2_";
/// Worker queue storage schema version. Version 2 means normal production
/// paths are V2-only; legacy `_worker_queue_` rows must have been migrated.
pub(super) const WORKER_QUEUE_SCHEMA_VERSION_KEY: &[u8] = b"_wq_schema_version";
/// Explicit migration lock for V1 `_worker_queue_` to V2 due/index/payload
/// conversion. The value stores the lock acquisition time in epoch millis.
pub(super) const WORKER_QUEUE_MIGRATION_LOCK_KEY: &[u8] = b"_wq_migration_lock";
/// Durable dropped-DB tombstone (global, system store). Written by DROP
/// DATABASE's worker reap and read with `get_for_update` in the SAME system
/// transaction as every cross-store next-fire `put_task_v2`. Because `db_id` is
/// monotonic / non-recycled, the tombstone is safe to keep forever: it can
/// never falsely fence a future database. It exists to make the cross-store
/// DROP-vs-enqueue race impossible — the reap's tombstone put and the enqueue's
/// tombstone `get_for_update` conflict under pessimistic txns, so they cannot
/// both commit. See issue #2628 (item 2) and design doc §K8.
pub(super) const WORKER_DROPPED_DB_TOMBSTONE_PREFIX: &[u8] = b"_wq_dropped_db_";
/// HNSW S3 graph uploads that crossed the transactional boundary but have not
/// yet been proven committed or aborted by GC.
pub(super) const HNSW_S3_GRAPH_UPLOAD_INTENT_PREFIX: &[u8] = b"_hnsw_s3_graph_upload_intent_";
/// DROP DATABASE S3 prefix cleanup requests. Stored outside tenant data so a
/// failed inline cleanup keeps a retry target after tenant metadata is gone.
pub(super) const HNSW_S3_DB_PREFIX_CLEANUP_INTENT_PREFIX: &[u8] =
    b"_hnsw_s3_db_prefix_cleanup_intent_";

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
// Storage stats key
// ============================================================================

/// Key: `d_{db_id:8bytes}_sys_storage_stats`
///
/// Stored under the database keyspace so `DROP DATABASE` range-delete cleans
/// up stats automatically.
pub fn encode_storage_stats_key_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_STORAGE_STATS);
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
///
/// Legacy: used for backward-compat reads of the old single-blob format.
pub fn encode_stats_key_v2(db_id: u64, table_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_STATS_PREFIX);
    key.extend_from_slice(&table_id.to_be_bytes());
    key
}

/// Encode the key for the per-table statistics header (row_count, last_analyzed).
///
/// Key format: `d_{db_id:8bytes}_sys_stats_{table_id:8bytes}_h`
pub fn encode_stats_header_key(db_id: u64, table_id: u64) -> Vec<u8> {
    let mut key = encode_stats_key_v2(db_id, table_id);
    key.push(b'_');
    key.push(b'h');
    key
}

/// Encode the key for a single column's statistics.
///
/// Key format: `d_{db_id:8bytes}_sys_stats_{table_id:8bytes}_c_{column_name}`
pub fn encode_stats_column_key(db_id: u64, table_id: u64, column_name: &str) -> Vec<u8> {
    let mut key = encode_stats_key_v2(db_id, table_id);
    key.extend_from_slice(b"_c_");
    key.extend_from_slice(column_name.as_bytes());
    key
}

/// Encode the prefix for scanning all per-column stats keys of a table.
///
/// Key format: `d_{db_id:8bytes}_sys_stats_{table_id:8bytes}_c_`
pub fn encode_stats_column_prefix(db_id: u64, table_id: u64) -> Vec<u8> {
    let mut key = encode_stats_key_v2(db_id, table_id);
    key.extend_from_slice(b"_c_");
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

/// Legacy running-guard prefix (pre design-35). Retained only for the legacy-shim
/// window so the migration sweep, the per-claim straggler check (design 35
/// §Migration), and DROP DATABASE cleanup can read/purge any rolling-window
/// leftovers an old binary may still be writing.
pub fn encode_cron_running_guard_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_RUNNING_GUARD_PREFIX_V2);
    key
}

/// Legacy running-guard key for a single `(db, job)` (pre design-35).
/// Format: `d_{db_id:8bytes}_sys_cron_running_guard_{job_id:be8}` — byte-identical
/// to what the removed #2629 writer used and what `decode_legacy_guard` parses.
/// Re-added for the legacy-shim window so the new claim path can point-get one
/// job's guard (a post-marker straggler an old binary wrote) without a prefix
/// scan; removed in the follow-up release once the fleet is fully upgraded.
pub fn encode_cron_running_guard_key_v2(db_id: u64, job_id: i64) -> Vec<u8> {
    let mut key = encode_cron_running_guard_prefix_v2(db_id);
    key.extend_from_slice(&job_id.to_be_bytes());
    key
}

/// Per-(db,job,minute) cron CONTROL record key (design 35). Both `job_id` and
/// `scheduled_min` are fixed 8-byte big-endian with NO separator, so the prefix
/// range-scans cleanly.
///
/// Value: bincode `CronRunControl` (per-fire state + monotonic fence token).
pub fn encode_cron_control_key_v2(db_id: u64, job_id: i64, scheduled_min: i64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_CONTROL_PREFIX_V2);
    key.extend_from_slice(&job_id.to_be_bytes());
    key.extend_from_slice(&scheduled_min.to_be_bytes());
    key
}

pub fn encode_cron_control_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_CONTROL_PREFIX_V2);
    key
}

/// Per-(db,job) cron ACTIVE-RUN pointer key (design 35) — job-level no-overlap.
///
/// Value: bincode `CronActiveRun`.
pub fn encode_cron_active_key_v2(db_id: u64, job_id: i64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_ACTIVE_PREFIX_V2);
    key.extend_from_slice(&job_id.to_be_bytes());
    key
}

pub fn encode_cron_active_prefix_v2(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_ACTIVE_PREFIX_V2);
    key
}

/// Per-db marker that the legacy guard/claim keys have been migrated to the
/// control/active model. The new claim path is fail-closed until this exists.
pub fn encode_cron_migrated_key_v3(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_CRON_MIGRATED_V3);
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

/// Encode a dropped-DB tombstone key (global, system store).
///
/// Format: `_wq_dropped_db_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}`
///
/// Keyed by the logical tenant keyspace string (the same value every enqueue
/// site embeds as `entry.keyspace`) plus the monotonic `db_id`, so the tombstone
/// is 1:1 with a dropped database and can never collide with a future one.
pub fn encode_worker_dropped_db_tombstone_key(keyspace: &str, db_id: u64) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(WORKER_DROPPED_DB_TOMBSTONE_PREFIX.len() + 2 + keyspace.len() + 1 + 8);
    key.extend_from_slice(WORKER_DROPPED_DB_TOMBSTONE_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key
}

pub fn encode_hnsw_s3_graph_upload_intent_key(
    keyspace: &str,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    version: u64,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(
        HNSW_S3_GRAPH_UPLOAD_INTENT_PREFIX.len() + 2 + keyspace.len() + 1 + 8 * 4,
    );
    key.extend_from_slice(HNSW_S3_GRAPH_UPLOAD_INTENT_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.extend_from_slice(&table_id.to_be_bytes());
    key.extend_from_slice(&index_id.to_be_bytes());
    key.extend_from_slice(&version.to_be_bytes());
    key
}

pub fn encode_hnsw_s3_graph_upload_intent_prefix() -> Vec<u8> {
    HNSW_S3_GRAPH_UPLOAD_INTENT_PREFIX.to_vec()
}

pub fn encode_hnsw_s3_db_prefix_cleanup_intent_key(keyspace: &str, db_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(
        HNSW_S3_DB_PREFIX_CLEANUP_INTENT_PREFIX.len() + 2 + keyspace.len() + 1 + 8,
    );
    key.extend_from_slice(HNSW_S3_DB_PREFIX_CLEANUP_INTENT_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key
}

pub fn encode_hnsw_s3_db_prefix_cleanup_intent_prefix() -> Vec<u8> {
    HNSW_S3_DB_PREFIX_CLEANUP_INTENT_PREFIX.to_vec()
}

/// Encode a worker queue key (global). LEGACY V1 layout.
///
/// Format: `_worker_queue_{priority:u8}_{fire_time_ms:memcomparable}_{task_type:u8}_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}`
///
/// Priority byte comes first so lower values (higher priority) sort first.
/// Fire time uses memcomparable encoding so earlier times sort first (handles negative values correctly).
///
/// Production no longer *writes* V1 keys (all enqueues go through the V2 layout
/// via `put_task_v2`); it only reads/migrates pre-existing V1 keys, for which
/// `encode_worker_queue_prefix` / `_scan_end` / `decode_worker_queue_fire_time`
/// suffice. This full encoder is retained for tests that seed legacy entries.
#[cfg(test)]
pub fn encode_worker_queue_key(
    priority: u8,
    fire_time_ms: i64,
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
) -> Result<Vec<u8>> {
    encode_due_key_with_prefix(
        WORKER_QUEUE_PREFIX,
        priority,
        fire_time_ms,
        task_type,
        keyspace,
        db_id,
        task_id,
    )
}

/// Shared due-key encoder for both the V1 (`_worker_queue_`) and V2
/// (`_wq_due_v2_`) prefixes — identical structure, different namespace.
fn encode_due_key_with_prefix(
    prefix: &[u8],
    priority: u8,
    fire_time_ms: i64,
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
) -> Result<Vec<u8>> {
    let mut key = Vec::with_capacity(prefix.len() + 1 + 8 + 1 + 2 + keyspace.len() + 1 + 8 + 8);
    key.extend_from_slice(prefix);
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

/// Encode the exclusive upper bound for a LEGACY worker queue range scan.
/// Used by the worker tick's byte-safe legacy dequeue during the V2 migration
/// window (`scan_due_legacy_bytesafe`).
/// Format: `_worker_queue_{priority:u8}_{fire_time_ms:memcomparable}` (no keyspace/db_id/task_id)
pub fn encode_worker_queue_scan_end(priority: u8, fire_time_ms: i64) -> Result<Vec<u8>> {
    encode_due_scan_end_with_prefix(WORKER_QUEUE_PREFIX, priority, fire_time_ms)
}

fn encode_due_scan_end_with_prefix(
    prefix: &[u8],
    priority: u8,
    fire_time_ms: i64,
) -> Result<Vec<u8>> {
    let mut key = Vec::with_capacity(prefix.len() + 1 + 8);
    key.extend_from_slice(prefix);
    key.push(priority);
    key.extend(memcomparable::to_vec(&fire_time_ms)?);
    Ok(key)
}

// --- V2 due queue (`_wq_due_v2_`) ---------------------------------------------

/// Encode a V2 due-queue key (same structure as the V1 worker queue key).
pub fn encode_wq_due_v2_key(
    priority: u8,
    fire_time_ms: i64,
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
) -> Result<Vec<u8>> {
    encode_due_key_with_prefix(
        WORKER_QUEUE_V2_PREFIX,
        priority,
        fire_time_ms,
        task_type,
        keyspace,
        db_id,
        task_id,
    )
}

/// Encode the prefix for all V2 due-queue keys (global).
pub fn encode_wq_due_v2_prefix() -> Vec<u8> {
    WORKER_QUEUE_V2_PREFIX.to_vec()
}

pub fn encode_worker_queue_schema_version_key() -> Vec<u8> {
    WORKER_QUEUE_SCHEMA_VERSION_KEY.to_vec()
}

pub fn encode_worker_queue_migration_lock_key() -> Vec<u8> {
    WORKER_QUEUE_MIGRATION_LOCK_KEY.to_vec()
}

/// Exclusive upper bound for a V2 due-queue range scan at a given priority/time.
pub fn encode_wq_due_v2_scan_end(priority: u8, fire_time_ms: i64) -> Result<Vec<u8>> {
    encode_due_scan_end_with_prefix(WORKER_QUEUE_V2_PREFIX, priority, fire_time_ms)
}

/// Decode fire_time_ms from a V2 due-queue key (claim binding).
pub fn decode_wq_due_v2_fire_time(key: &[u8]) -> Option<i64> {
    decode_due_fire_time_with_prefix(WORKER_QUEUE_V2_PREFIX, key)
}

fn decode_due_fire_time_with_prefix(prefix: &[u8], key: &[u8]) -> Option<i64> {
    use memcomparable::Deserializer;
    if key.len() < prefix.len() + 1 + 8 || !key.starts_with(prefix) {
        return None;
    }
    let offset = prefix.len() + 1;
    let payload = &key[offset..];
    let mut deserializer = Deserializer::new(payload);
    serde::Deserialize::deserialize(&mut deserializer).ok()
}

// --- V2 payload range (`_wq_payload_v2_`) -------------------------------------

/// Encode a V2 payload key, identity-keyed by full due identity (incl.
/// fire_time) so it is 1:1 with the due entry and collision-free across the
/// split task types' differing task_id schemes.
pub fn encode_wq_payload_v2_key(
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
    fire_time_ms: i64,
) -> Result<Vec<u8>> {
    let mut key = Vec::with_capacity(
        WORKER_PAYLOAD_V2_PREFIX.len() + 1 + 2 + keyspace.len() + 1 + 8 + 1 + 8 + 8,
    );
    key.extend_from_slice(WORKER_PAYLOAD_V2_PREFIX);
    key.push(task_type);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&task_id.to_be_bytes());
    key.extend(memcomparable::to_vec(&fire_time_ms)?);
    Ok(key)
}

/// Decode fire_time_ms from a worker queue key.
///
/// Used by worker claim logic so a claim is bound to the exact queue entry
/// being executed, not to the worker's current wall-clock minute.
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

// ============================================================================
// V2 worker queue secondary index (issue #2576)
//
// Layout: `_wq_idx_v2_{keyspace_len:u16 be}{keyspace}_{db_id:be8}_{task_type:u8}{task_id:be8}{fire_time:memcomparable}`
//
// Keyspace/db lead so every task-targeted or tenant-targeted operation is a
// bounded prefix scan; `fire_time` in the tail makes the index multi-valued so
// distinct due entries for the same (keyspace, db, task_type, task_id) that
// differ in fire_time — e.g. a cron job's successive requeues — each get their
// own row instead of overwriting. NOTE: this disambiguates only when fire_time
// differs; two enqueues sharing the full (task_type, task_id, fire_time) identity
// still collide (e.g. two async-trigger activations in the same millisecond,
// where task_id and fire_time are both derived from the same wall clock — a
// pre-existing identity collision, not introduced by V2). The index VALUE is a
// single `priority` byte, which together with the key fields reconstructs the
// exact due-queue key without ever reading the (large) due-queue value.
// ============================================================================

/// Length of the fixed tail after the `{keyspace}_{db_id}_` prefix:
/// task_type(1) + task_id(8) + fire_time(memcomparable i64 = 8).
const WQ_INDEX_TAIL_LEN: usize = 1 + 8 + 8;

fn wq_index_key_head(keyspace: &str, db_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(
        WORKER_QUEUE_INDEX_V2_PREFIX.len() + 2 + keyspace.len() + 1 + 8 + 1 + WQ_INDEX_TAIL_LEN,
    );
    key.extend_from_slice(WORKER_QUEUE_INDEX_V2_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key
}

/// Full V2 index key for one due entry.
pub fn encode_wq_index_key(
    keyspace: &str,
    db_id: u64,
    task_type: u8,
    task_id: i64,
    fire_time_ms: i64,
) -> Result<Vec<u8>> {
    let mut key = wq_index_key_head(keyspace, db_id);
    key.push(task_type);
    key.extend_from_slice(&task_id.to_be_bytes());
    key.extend(memcomparable::to_vec(&fire_time_ms)?);
    Ok(key)
}

/// Prefix matching every V2 index row for one keyspace (all dbs, all task
/// types). Used by the per-keyspace async-trigger stats metric.
pub fn encode_wq_index_prefix_keyspace(keyspace: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(WORKER_QUEUE_INDEX_V2_PREFIX.len() + 2 + keyspace.len() + 1);
    key.extend_from_slice(WORKER_QUEUE_INDEX_V2_PREFIX);
    key.extend_from_slice(&(keyspace.len() as u16).to_be_bytes());
    key.extend_from_slice(keyspace.as_bytes());
    key.push(b'_');
    key
}

/// Prefix matching every V2 index row for one (keyspace, db_id) — all task
/// types. Used by DROP DATABASE queue reap.
pub fn encode_wq_index_prefix_db(keyspace: &str, db_id: u64) -> Vec<u8> {
    wq_index_key_head(keyspace, db_id)
}

/// Prefix matching every V2 index row for one (keyspace, db_id, task_type).
/// Used by cron reconciliation.
pub fn encode_wq_index_prefix_db_type(keyspace: &str, db_id: u64, task_type: u8) -> Vec<u8> {
    let mut key = wq_index_key_head(keyspace, db_id);
    key.push(task_type);
    key
}

/// Prefix matching every V2 index row for one (keyspace, db_id, task_type,
/// task_id). Used by per-task lookup (schedule replace / unschedule / dedup).
pub fn encode_wq_index_prefix_task(
    keyspace: &str,
    db_id: u64,
    task_type: u8,
    task_id: i64,
) -> Vec<u8> {
    let mut key = wq_index_key_head(keyspace, db_id);
    key.push(task_type);
    key.extend_from_slice(&task_id.to_be_bytes());
    key
}

/// Decoded components of a V2 index row.
pub struct WqIndexEntry {
    pub keyspace: String,
    pub db_id: u64,
    pub task_type: u8,
    pub task_id: i64,
    pub fire_time_ms: i64,
}

/// Decode a V2 index key back into its components. Returns `None` if the key is
/// malformed or carries a non-UTF-8 keyspace.
pub fn decode_wq_index_key(key: &[u8]) -> Option<WqIndexEntry> {
    let p = WORKER_QUEUE_INDEX_V2_PREFIX.len();
    if key.len() < p + 2 {
        return None;
    }
    if !key.starts_with(WORKER_QUEUE_INDEX_V2_PREFIX) {
        return None;
    }
    let ks_len = u16::from_be_bytes(key[p..p + 2].try_into().ok()?) as usize;
    let ks_start = p + 2;
    let ks_end = ks_start + ks_len;
    // layout after keyspace: '_' db_id(8) '_' task_type(1) task_id(8) fire_time(8)
    let expected_len = ks_end + 1 + 8 + 1 + WQ_INDEX_TAIL_LEN;
    if key.len() != expected_len {
        return None;
    }
    let keyspace = std::str::from_utf8(&key[ks_start..ks_end])
        .ok()?
        .to_string();
    let mut cursor = ks_end;
    if key[cursor] != b'_' {
        return None;
    }
    cursor += 1;
    let db_id = u64::from_be_bytes(key[cursor..cursor + 8].try_into().ok()?);
    cursor += 8;
    if key[cursor] != b'_' {
        return None;
    }
    cursor += 1;
    let task_type = key[cursor];
    cursor += 1;
    let task_id = i64::from_be_bytes(key[cursor..cursor + 8].try_into().ok()?);
    cursor += 8;
    let mut de = memcomparable::Deserializer::new(&key[cursor..cursor + 8]);
    let fire_time_ms: i64 = serde::Deserialize::deserialize(&mut de).ok()?;
    Some(WqIndexEntry {
        keyspace,
        db_id,
        task_type,
        task_id,
        fire_time_ms,
    })
}

// ============================================================================
// DDL journal keys
// ============================================================================

pub fn encode_ddl_journal_key(db_id: u64, journal_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_DDL_JOURNAL_PREFIX);
    key.extend_from_slice(&journal_id.to_be_bytes());
    key
}

pub fn encode_ddl_journal_prefix(db_id: u64) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(DB_SYS_DDL_JOURNAL_PREFIX);
    key
}
/// Decode task_type from a worker queue key.
#[cfg(test)]
pub fn decode_worker_queue_task_type(key: &[u8]) -> Option<u8> {
    let offset = WORKER_QUEUE_PREFIX.len() + 1 + 8;
    key.get(offset).copied()
}

/// Encode a worker claim key (global).
///
/// Format: `_worker_claim_{task_type:u8}_{keyspace_len:u16}{keyspace_bytes}_{db_id:be8}_{task_id:be8}_{fire_time_ms:be8}`
pub fn encode_worker_claim_key(
    task_type: u8,
    keyspace: &str,
    db_id: u64,
    task_id: i64,
    fire_time_ms: i64,
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
    key.extend_from_slice(&fire_time_ms.to_be_bytes());
    key
}

/// Encode the prefix for all worker claim keys (global).
pub fn encode_worker_claim_prefix() -> Vec<u8> {
    WORKER_CLAIM_PREFIX.to_vec()
}

/// Return true when a raw worker-claim key belongs to `keyspace`.
///
/// Keeps claim-key parsing next to the key encoder so observability callers do
/// not duplicate the binary key layout.
pub fn worker_claim_keyspace_matches(key: &[u8], keyspace: &str) -> bool {
    if !key.starts_with(WORKER_CLAIM_PREFIX) {
        return false;
    }
    let mut idx = WORKER_CLAIM_PREFIX.len();
    if idx + 1 > key.len() {
        return false;
    }
    idx += 1; // task_type:u8
    if idx + 2 > key.len() {
        return false;
    }
    let keyspace_len = u16::from_be_bytes([key[idx], key[idx + 1]]) as usize;
    idx += 2;
    if idx + keyspace_len > key.len() {
        return false;
    }
    &key[idx..idx + keyspace_len] == keyspace.as_bytes()
}

/// Encode a GC instance state key.
/// Format: `_gc_instance_{instance_id_bytes}`
pub fn encode_gc_instance_state_key(instance_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(GC_INSTANCE_STATE_PREFIX.len() + instance_id.len());
    key.extend_from_slice(GC_INSTANCE_STATE_PREFIX);
    key.extend_from_slice(instance_id.as_bytes());
    key
}

/// Encode the prefix for all GC instance state keys.
pub fn encode_gc_instance_state_prefix() -> Vec<u8> {
    GC_INSTANCE_STATE_PREFIX.to_vec()
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
#[allow(dead_code)]
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

    // ── V2 worker queue (issue #2576) ────────────────────────────────────

    #[test]
    fn wq_due_v2_key_shares_structure_with_v1_but_distinct_prefix() {
        let v1 = encode_worker_queue_key(3, 1234567890, 0x01, "tenant_a", 42, 7).unwrap();
        let v2 = encode_wq_due_v2_key(3, 1234567890, 0x01, "tenant_a", 42, 7).unwrap();
        // Distinct prefixes so an old binary (reads only `_worker_queue_`) never
        // sees a V2 descriptor value it cannot deserialize.
        assert!(v1.starts_with(WORKER_QUEUE_PREFIX));
        assert!(v2.starts_with(WORKER_QUEUE_V2_PREFIX));
        assert!(!v1.starts_with(WORKER_QUEUE_V2_PREFIX));
        // Bodies after their prefixes are byte-identical (same ordering scheme).
        assert_eq!(
            &v1[WORKER_QUEUE_PREFIX.len()..],
            &v2[WORKER_QUEUE_V2_PREFIX.len()..]
        );
    }

    #[test]
    fn wq_due_v2_fire_time_roundtrips_and_orders() {
        let pos = encode_wq_due_v2_key(3, 1234567890, 1, "t", 42, 7).unwrap();
        let neg = encode_wq_due_v2_key(3, -987654321, 1, "t", 42, 7).unwrap();
        assert_eq!(decode_wq_due_v2_fire_time(&pos), Some(1234567890));
        assert_eq!(decode_wq_due_v2_fire_time(&neg), Some(-987654321));
        // A V1 key must not decode as a V2 key (prefix guard).
        let v1 = encode_worker_queue_key(3, 1000, 1, "t", 42, 7).unwrap();
        assert_eq!(decode_wq_due_v2_fire_time(&v1), None);
        // Earlier fire_time sorts first within a priority band.
        let early = encode_wq_due_v2_key(5, 1000, 1, "t", 1, 1).unwrap();
        let late = encode_wq_due_v2_key(5, 2000, 1, "t", 1, 1).unwrap();
        assert!(early < late);
    }

    #[test]
    fn wq_due_v2_scan_end_bounds_priority_band() {
        let end = encode_wq_due_v2_scan_end(5, 1000).unwrap();
        let inside = encode_wq_due_v2_key(5, 1000, 1, "k", 1, 2).unwrap();
        assert!(inside.starts_with(&end));
        let later = encode_wq_due_v2_scan_end(5, 1001).unwrap();
        assert!(end < later);
    }

    #[test]
    fn wq_index_key_roundtrips() {
        let key = encode_wq_index_key("tenant_a", 42, 0x02, 7, 1234567890).unwrap();
        let decoded = decode_wq_index_key(&key).expect("decode");
        assert_eq!(decoded.keyspace, "tenant_a");
        assert_eq!(decoded.db_id, 42);
        assert_eq!(decoded.task_type, 0x02);
        assert_eq!(decoded.task_id, 7);
        assert_eq!(decoded.fire_time_ms, 1234567890);

        // Negative fire_time roundtrips too (memcomparable tail).
        let neg = encode_wq_index_key("t", 1, 0x01, 9, -5).unwrap();
        assert_eq!(decode_wq_index_key(&neg).unwrap().fire_time_ms, -5);
    }

    #[test]
    fn wq_index_prefix_containment_is_hierarchical() {
        let ks = encode_wq_index_prefix_keyspace("ks");
        let db = encode_wq_index_prefix_db("ks", 7);
        let db_type = encode_wq_index_prefix_db_type("ks", 7, 0x01);
        let task = encode_wq_index_prefix_task("ks", 7, 0x01, 99);
        let full = encode_wq_index_key("ks", 7, 0x01, 99, 1000).unwrap();

        assert!(db.starts_with(&ks));
        assert!(db_type.starts_with(&db));
        assert!(task.starts_with(&db_type));
        assert!(full.starts_with(&task));

        // A different db / task_type / task_id must NOT match the narrower prefix.
        let other_db = encode_wq_index_key("ks", 8, 0x01, 99, 1000).unwrap();
        assert!(!other_db.starts_with(&db));
        let other_type = encode_wq_index_key("ks", 7, 0x02, 99, 1000).unwrap();
        assert!(!other_type.starts_with(&db_type));
        let other_task = encode_wq_index_key("ks", 7, 0x01, 100, 1000).unwrap();
        assert!(!other_task.starts_with(&task));
    }

    #[test]
    fn wq_index_is_multi_valued_by_fire_time() {
        // Same (ks, db, type, task_id), different fire_time ⇒ distinct index rows,
        // both under the per-task prefix. This is what prevents silent overwrite
        // for cron requeues and same-ms async-trigger task_ids.
        let a = encode_wq_index_key("ks", 1, 0x01, 5, 1000).unwrap();
        let b = encode_wq_index_key("ks", 1, 0x01, 5, 2000).unwrap();
        let task_prefix = encode_wq_index_prefix_task("ks", 1, 0x01, 5);
        assert_ne!(a, b);
        assert!(a.starts_with(&task_prefix));
        assert!(b.starts_with(&task_prefix));
    }

    #[test]
    fn wq_payload_key_is_unique_per_due_identity() {
        let p1 = encode_wq_payload_v2_key(0x01, "ks", 1, 5, 1000).unwrap();
        let p2 = encode_wq_payload_v2_key(0x01, "ks", 1, 5, 2000).unwrap();
        let p3 = encode_wq_payload_v2_key(0x10, "ks", 1, 5, 1000).unwrap();
        assert_ne!(p1, p2, "different fire_time ⇒ different payload key");
        assert_ne!(p1, p3, "different task_type ⇒ different payload key");
        assert!(p1.starts_with(WORKER_PAYLOAD_V2_PREFIX));
    }

    #[test]
    fn v2_prefixes_are_mutually_disjoint() {
        // No V2 prefix is a prefix of another, and none collide with V1 — so a
        // scan of one range never bleeds into another.
        let prefixes: [&[u8]; 4] = [
            WORKER_QUEUE_PREFIX,
            WORKER_QUEUE_V2_PREFIX,
            WORKER_PAYLOAD_V2_PREFIX,
            WORKER_QUEUE_INDEX_V2_PREFIX,
        ];
        for (i, a) in prefixes.iter().enumerate() {
            for (j, b) in prefixes.iter().enumerate() {
                if i != j {
                    assert!(!a.starts_with(b), "{a:?} must not start with {b:?}");
                }
            }
        }
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
