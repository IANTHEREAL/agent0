//! TiKV storage layer

use anyhow::Error;
use std::error::Error as StdError;
use std::fmt;

pub(crate) mod backpressure;
mod encoding;
mod kv_stats;
mod tikv_store;

pub(crate) use encoding::{
    decode_pk_from_index_suffix, decode_worker_queue_fire_time, deserialize_row,
    deserialize_schema, encode_database_data_range, encode_embedding_usage_key_v2,
    encode_extension_key_v2, encode_pk_values, encode_schema_key_v2, encode_sequence_value_key_v2,
    encode_storage_stats_key_v2, encode_table_data_range_v2, serialize_row,
};
pub(crate) use kv_stats::{with_kv_read_stats, KvReadStatsSnapshot};
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
