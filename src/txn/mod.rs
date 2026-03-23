//! Transaction-level helpers.
//!
//! This module implements PostgreSQL-like SAVEPOINT semantics on top of TiKV's
//! transactional API by recording "before images" for mutated keys.

mod savepoints;
mod state;

use anyhow::{anyhow, Result};
use std::future::Future;
use std::sync::{Arc, OnceLock};
use tikv_client::transaction::Mutation;
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::storage::backpressure::tikv_op;

pub(crate) use state::SavepointState;

// ── KV value size guard ───────────────────────────────────────────────
//
// TiKV enforces `raft-entry-max-size` (prod: 16 MiB, default: 8 MiB).
// Writes exceeding this limit fail with `RaftEntryTooLarge` — a cryptic
// region error that gives the user no actionable information.
//
// This guard rejects oversized writes *before* they reach TiKV, with a
// clear error message identifying the subsystem (row data, HNSW index,
// statistics, etc.) from the key prefix.

/// Default per-value size limit: 8 MiB.
/// Our production TiKV is configured with `raft-entry-max-size = 16 MiB`,
/// so 8 MiB provides 50% margin for key + protobuf + raft entry overhead.
const DEFAULT_VALUE_SIZE_LIMIT: usize = 8 * 1024 * 1024;

struct ValueSizeGuard {
    limit: usize, // 0 = disabled
}

static VALUE_SIZE_GUARD: OnceLock<ValueSizeGuard> = OnceLock::new();

fn value_size_guard() -> &'static ValueSizeGuard {
    VALUE_SIZE_GUARD.get_or_init(|| {
        let limit = std::env::var("DB9_TXN_VALUE_SIZE_LIMIT_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_VALUE_SIZE_LIMIT);
        ValueSizeGuard { limit }
    })
}

/// Identify the subsystem from the key prefix for diagnostic messages.
///
/// Real key format is binary, not ASCII:
///   `d_` + 8-byte big-endian db_id + `_` + subsystem prefix + ...
///   `_fs_` + type byte + ...
///   `_worker_` + ...
///   `_sys_` + ...
fn key_subsystem(key: &[u8]) -> &'static str {
    // Database-scoped keys: b"d_" + 8-byte db_id + b"_" + subsystem
    // Offset 11 = 2 (b"d_") + 8 (db_id) + 1 (b"_")
    if key.starts_with(b"d_") && key.len() > 11 {
        let after = &key[11..];
        if after.starts_with(b"hnsw_") {
            return "HNSW index";
        }
        if after.starts_with(b"t_") {
            return "table row";
        }
        if after.starts_with(b"i_") {
            return "index entry";
        }
        if after.starts_with(b"sys_stats_") {
            return "table statistics";
        }
        if after.starts_with(b"sys_schema_") {
            return "schema metadata";
        }
        return "database data";
    }
    if key.starts_with(b"_fs_") {
        return "fs9 file data";
    }
    if key.starts_with(b"_worker_") {
        return "worker task";
    }
    if key.starts_with(b"_sys_") {
        return "system metadata";
    }
    "unknown"
}

/// Reject a single key+value pair if it exceeds the size limit.
/// Public so that code paths that intentionally bypass the `txn_put`
/// wrapper (e.g. savepoint rollback, fs9 inline blob) can still
/// validate writes.
#[inline]
pub(crate) fn check_value_size(key: &[u8], value: &[u8]) -> Result<()> {
    let guard = value_size_guard();
    if guard.limit == 0 {
        return Ok(());
    }
    let total = key.len() + value.len();
    if total <= guard.limit {
        // Warn when a value exceeds 50% of the limit — early signal that
        // a subsystem is producing values that may soon be rejected.
        if total > guard.limit / 2 {
            let subsystem = key_subsystem(key);
            tracing::warn!(
                subsystem,
                total_bytes = total,
                limit_bytes = guard.limit,
                "KV value approaching size limit (>{} of {} bytes)",
                guard.limit / 2,
                guard.limit,
            );
        }
        return Ok(());
    }
    let subsystem = key_subsystem(key);
    // Use hex encoding for key preview — binary keys contain non-UTF-8
    // bytes (e.g. 8-byte db_id) that corrupt pgwire message framing.
    let preview_len = key.len().min(32);
    let key_hex: String = key[..preview_len]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    Err(SqlError::ValueTooLarge {
        message: format!(
            "value too large: {} bytes exceeds limit {} bytes ({}). \
             Reduce payload size or adjust DB9_TXN_VALUE_SIZE_LIMIT_BYTES. \
             [key={}]",
            total, guard.limit, subsystem, key_hex,
        ),
    }
    .into())
}

tokio::task_local! {
    /// Session-scoped savepoint state for the currently executing query.
    static SAVEPOINTS: Arc<SavepointState>;
}

/// Run `future` with the given savepoint manager set as task-local context.
pub(crate) async fn with_savepoints<R>(
    savepoints: Arc<SavepointState>,
    future: impl Future<Output = R>,
) -> R {
    // See `sql::query_context::with_query_context` for rationale.
    #[cfg(debug_assertions)]
    {
        SAVEPOINTS.scope(savepoints, Box::pin(future)).await
    }

    #[cfg(not(debug_assertions))]
    {
        SAVEPOINTS.scope(savepoints, future).await
    }
}

/// TiKV `put` wrapper that records undo information when SAVEPOINT is active.
#[inline]
pub(crate) async fn txn_put(txn: &mut Transaction, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
    check_value_size(&key, &value)?;

    let savepoints = SAVEPOINTS.try_with(|sp| sp.clone()).ok();
    let should_record = match savepoints.as_ref() {
        Some(sp) => sp.should_record_key(&key).await?,
        None => false,
    };

    if should_record {
        let prev = tikv_op!(txn.get(key.clone()).await).map_err(|e| anyhow!(e))?;
        if let Some(sp) = savepoints {
            sp.record_prev_value(key.clone(), prev).await?;
        }
    }

    tikv_op!(txn.put(key, value).await).map_err(|e| anyhow!(e))
}

/// TiKV `batch_mutate` wrapper that records undo information when SAVEPOINT is
/// active and acquires pessimistic locks for **all** keys in a single RPC
/// (vs one lock RPC per key with individual `txn_put` calls).
#[inline]
pub(crate) async fn txn_batch_mutate(
    txn: &mut Transaction,
    mutations: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    if mutations.is_empty() {
        return Ok(());
    }

    let savepoints = SAVEPOINTS.try_with(|sp| sp.clone()).ok();

    if let Some(ref sp) = savepoints {
        for (key, _) in &mutations {
            if sp.should_record_key(key).await? {
                let prev = txn.get(key.clone()).await.map_err(|e| anyhow!(e))?;
                sp.record_prev_value(key.clone(), prev).await?;
            }
        }
    }

    for (k, v) in &mutations {
        check_value_size(k, v)?;
    }

    let tikv_mutations: Vec<Mutation> = mutations
        .into_iter()
        .map(|(k, v)| Mutation::Put(k.into(), v))
        .collect();
    txn.batch_mutate(tikv_mutations)
        .await
        .map_err(|e| anyhow!(e))
}

/// TiKV `delete` wrapper that records undo information when SAVEPOINT is active.
#[inline]
pub(crate) async fn txn_delete(txn: &mut Transaction, key: Vec<u8>) -> Result<()> {
    let savepoints = SAVEPOINTS.try_with(|sp| sp.clone()).ok();
    let should_record = match savepoints.as_ref() {
        Some(sp) => sp.should_record_key(&key).await?,
        None => false,
    };

    if should_record {
        let prev = tikv_op!(txn.get(key.clone()).await).map_err(|e| anyhow!(e))?;
        if let Some(sp) = savepoints {
            sp.record_prev_value(key.clone(), prev).await?;
        }
    }

    tikv_op!(txn.delete(key).await).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a realistic database-scoped key: `d_` + 8-byte db_id + `_` + suffix
    fn db_key(db_id: u64, suffix: &[u8]) -> Vec<u8> {
        let mut key = b"d_".to_vec();
        key.extend_from_slice(&db_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(suffix);
        key
    }

    #[test]
    fn check_value_size_allows_small_values() {
        let key = db_key(1, b"t_100");
        let value = vec![0u8; 1024];
        assert!(check_value_size(&key, &value).is_ok());
    }

    #[test]
    fn check_value_size_rejects_oversized_values() {
        let key = db_key(1, b"t_100");
        let value = vec![0u8; DEFAULT_VALUE_SIZE_LIMIT + 1];
        let err = check_value_size(&key, &value).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("value too large"), "msg: {msg}");
        assert!(msg.contains("table row"), "msg: {msg}");
    }

    #[test]
    fn check_value_size_at_exact_boundary_passes() {
        let key = db_key(1, b"t_100");
        let remaining = DEFAULT_VALUE_SIZE_LIMIT - key.len();
        let value = vec![0u8; remaining];
        assert!(check_value_size(&key, &value).is_ok());
    }

    // ── key_subsystem with real binary key format ─────────────────

    #[test]
    fn key_subsystem_identifies_hnsw() {
        assert_eq!(key_subsystem(&db_key(42, b"hnsw_10_3_graph")), "HNSW index");
    }

    #[test]
    fn key_subsystem_identifies_table_row() {
        assert_eq!(key_subsystem(&db_key(42, b"t_100_data")), "table row");
    }

    #[test]
    fn key_subsystem_identifies_stats() {
        assert_eq!(
            key_subsystem(&db_key(42, b"sys_stats_\x00\x00\x00\x00\x00\x00\x00\x05")),
            "table statistics"
        );
    }

    #[test]
    fn key_subsystem_identifies_schema() {
        assert_eq!(
            key_subsystem(&db_key(42, b"sys_schema_users")),
            "schema metadata"
        );
    }

    #[test]
    fn key_subsystem_identifies_index_entry() {
        assert_eq!(
            key_subsystem(&db_key(42, b"i_\x00\x00\x00\x00\x00\x00\x00\x01")),
            "index entry"
        );
    }

    #[test]
    fn key_subsystem_identifies_fs9() {
        // fs9 inline blob key: _fs_B + 8-byte inode_id
        let mut key = b"_fs_B".to_vec();
        key.extend_from_slice(&42u64.to_be_bytes());
        assert_eq!(key_subsystem(&key), "fs9 file data");
    }

    #[test]
    fn key_subsystem_identifies_worker() {
        assert_eq!(key_subsystem(b"_worker_queue_ks_1"), "worker task");
    }

    #[test]
    fn key_subsystem_fallback_for_unknown() {
        assert_eq!(key_subsystem(b"random_key"), "unknown");
    }

    #[test]
    fn key_subsystem_short_db_key_falls_through() {
        // d_ + 8 bytes db_id + _ with no subsystem suffix (11 bytes exactly)
        // is too short for subsystem detection, falls through
        let key = db_key(1, b"");
        assert_eq!(key_subsystem(&key), "unknown");
    }

    #[test]
    fn key_subsystem_unrecognized_db_subsystem() {
        // d_ + 8 bytes + _ + unknown subsystem = "database data"
        let key = db_key(1, b"something_else");
        assert_eq!(key_subsystem(&key), "database data");
    }

    /// A minimal tracing layer that counts WARN events whose message
    /// contains a given substring.  Used to assert warning emission.
    struct WarnCounter {
        needle: &'static str,
        count: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCounter {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                // Format the event message to check for the needle.
                struct Visitor<'a>(&'a str, bool);
                impl tracing::field::Visit for Visitor<'_> {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "message" {
                            let s = format!("{:?}", value);
                            if s.contains(self.0) {
                                self.1 = true;
                            }
                        }
                    }
                }
                let mut v = Visitor(self.needle, false);
                event.record(&mut v);
                if v.1 {
                    self.count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    #[test]
    fn check_value_size_warns_at_50_percent() {
        use tracing_subscriber::layer::SubscriberExt;

        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let layer = WarnCounter {
            needle: "approaching size limit",
            count: Arc::clone(&count),
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let key = db_key(1, b"t_100");
        let half_limit = DEFAULT_VALUE_SIZE_LIMIT / 2;
        // total = half_limit + 1 → exceeds 50%, should warn
        let value = vec![0u8; half_limit - key.len() + 1];

        tracing::subscriber::with_default(subscriber, || {
            assert!(check_value_size(&key, &value).is_ok());
        });

        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "expected exactly 1 warning for value exceeding 50% of limit"
        );
    }

    #[test]
    fn check_value_size_no_warn_at_50_percent_boundary() {
        use tracing_subscriber::layer::SubscriberExt;

        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let layer = WarnCounter {
            needle: "approaching size limit",
            count: Arc::clone(&count),
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let key = db_key(1, b"t_100");
        let half_limit = DEFAULT_VALUE_SIZE_LIMIT / 2;
        // total = exactly half_limit → not > 50%, should NOT warn
        let value = vec![0u8; half_limit - key.len()];

        tracing::subscriber::with_default(subscriber, || {
            assert!(check_value_size(&key, &value).is_ok());
        });

        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "expected no warning for value at exactly 50% of limit"
        );
    }

    #[test]
    fn check_value_size_returns_sql_error_variant() {
        use crate::sql::error::SqlError;
        let key = db_key(1, b"t_100");
        let value = vec![0u8; DEFAULT_VALUE_SIZE_LIMIT + 1];
        let err = check_value_size(&key, &value).unwrap_err();
        // Must downcast to SqlError::ValueTooLarge — this is what
        // sqlstate_for_executor_error uses to map to SQLSTATE 54000.
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("check_value_size should return SqlError::ValueTooLarge");
        assert_eq!(sql_err.sqlstate(), "54000");
    }

    #[test]
    fn error_message_includes_subsystem_and_env_var() {
        let key = db_key(1, b"sys_stats_\x00\x00\x00\x00\x00\x00\x00\x05");
        let value = vec![0u8; DEFAULT_VALUE_SIZE_LIMIT + 1];
        let err = check_value_size(&key, &value).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("table statistics"), "msg: {msg}");
        assert!(msg.contains("DB9_TXN_VALUE_SIZE_LIMIT_BYTES"), "msg: {msg}");
        // Key preview should be hex-encoded, not raw UTF-8
        assert!(msg.contains("[key="), "msg: {msg}");
        assert!(
            !msg.contains('\u{FFFD}'),
            "key preview should not contain replacement chars: {msg}"
        );
    }
}
