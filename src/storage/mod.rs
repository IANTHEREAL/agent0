//! TiKV storage layer

use anyhow::Error;
use std::error::Error as StdError;
use std::fmt;

pub(crate) mod backpressure;
mod encoding;
pub(crate) mod error;
pub(crate) mod facade;
mod kv_stats;
#[cfg(feature = "mock-storage")]
pub(crate) mod memory;
mod tikv_store;

// Re-export the canonical storage error surface so PR-1.5 (#19) and PR-3 (#21)
// callers can `use crate::storage::{StorageError, WriteConflictReason}` without
// reaching through `error::` or `facade::`. PR-1 introduces the surface; first
// callers arrive in PR-1.5.
pub(crate) use encoding::{
    decode_pk_from_index_suffix, decode_table_id_from_mutation_key_v2,
    decode_worker_queue_fire_time, decode_wq_due_v2_fire_time, deserialize_row, deserialize_schema,
    encode_database_data_range, encode_embedding_usage_key_v2, encode_extension_key_v2,
    encode_pk_values, encode_prefix_end, encode_schema_key_v2, encode_sequence_value_key_v2,
    encode_storage_stats_key_v2, encode_table_data_range_v2, is_wq_due_v2_key, serialize_row,
};
pub(crate) use error::StorageError;
#[allow(unused_imports)]
pub(crate) use error::WriteConflictReason;
pub(crate) use kv_stats::{with_kv_read_stats, KvReadStatsSnapshot};
#[cfg(feature = "mock-storage")]
#[allow(unused_imports)]
pub(crate) use memory::{MemoryClient, MemoryUniverse};
pub use tikv_store::*;

#[derive(Debug)]
pub(crate) struct UniqueIndexDuplicateError;

impl fmt::Display for UniqueIndexDuplicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Duplicate entry for unique index")
    }
}

impl StdError for UniqueIndexDuplicateError {}

pub(crate) fn unique_index_duplicate_error() -> Error {
    Error::new(UniqueIndexDuplicateError)
}

pub(crate) fn is_unique_index_duplicate_error(err: &Error) -> bool {
    err.downcast_ref::<UniqueIndexDuplicateError>().is_some()
}
