//! Storage facade: db9-owned types over the underlying TiKV client.
//!
//! ## Why this exists
//!
//! db9-server has historically threaded `tikv_client::Transaction`,
//! `TransactionClient`, `BoundRange`, and `Mutation` through SQL/executor/DDL
//! code. That coupling makes it impossible to plug a different storage backend
//! (e.g. an in-memory mock for tests) without touching every call site. The
//! facade introduces compatibility types that delegate to TiKV today and gain
//! a memory variant in PR-3 under the `mock-storage` cargo feature.
//!
//! ## Scope of PR-1
//!
//! - Define the facade types: [`StorageClient`], [`StorageTxn`],
//!   [`StorageSnapshot`], [`StorageError`], [`StorageMutation`],
//!   [`StorageRange`], [`StorageCapabilities`], [`WriteConflictReason`].
//! - Wire TiKV delegation through these types with **zero behavior change**
//!   for current callers — the facade is additive and call-site migration is
//!   the responsibility of PR-2.
//!
//! ## Invariants (per #2523 Consensus Amendments Section A & C)
//!
//! - [`StorageTxn::commit`] and [`StorageTxn::rollback`] take `&mut self`
//!   instead of consuming `self`. Implementations MUST set the `finished`
//!   invariant so [`Drop`] is idempotent after a successful commit/rollback.
//!   Forgetting to set `finished` is a correctness bug.
//! - [`StorageSnapshot`] read methods take `&mut self` to match the underlying
//!   `tikv_client::Snapshot` API.
//! - [`StorageRange`] uses [`std::ops::Bound`] for endpoints. The single
//!   conversion helper [`StorageRange::to_tikv_bound_range`] is the only path
//!   that translates these bounds into [`tikv_client::BoundRange`].
//! - Timestamp ordering: `start_ts` is allocated by the underlying client at
//!   `begin()` before any reads; `commit_ts` is allocated at commit by the
//!   underlying client; the facade does not introduce its own clock.

// PR-1 introduces these types as the integration surface for PR-1.5 (#19),
// PR-2 (#20), and PR-3 (#21). They have no callers in PR-1; the surface is
// validated by the in-crate `tests` module below. The dead-code allow is
// removed automatically as soon as PR-2 starts migrating call sites.
#![allow(dead_code)]

use std::ops::Bound;

use tikv_client::{
    BoundRange, CheckLevel, Key, KvPair, Timestamp, TimestampExt, Transaction, TransactionClient,
    TransactionOptions,
};

use super::backpressure::tikv_op;
#[cfg(feature = "mock-storage")]
use super::memory::{MemoryClient, MemorySnapshot, MemoryTxn};
// `StorageError` and `WriteConflictReason` are owned by `src/storage/error.rs`
// (canonical surface per #2523 Consensus Amendments §D and architect-2's
// integration ruling on PR-1 / PR-1.5). The facade re-imports them so PR-2
// migration sites and PR-3 backend variants share a single error contract
// with the protocol mapping layer.
#[allow(unused_imports)] // wired by PR-2 / PR-3 callers; re-exported for PR-1.5.
pub(crate) use super::error::{StorageError, WriteConflictReason};

// ─── Mutations ──────────────────────────────────────────────────────────────

/// Backend-neutral mutation kind for batch writes. The underlying engine only
/// uses `Put` and `Delete` today (audit at `vendor/tikv-client/src/transaction/transaction.rs:1389`),
/// so the facade does not over-model lock/check-not-exists variants.
#[derive(Debug, Clone)]
pub(crate) enum StorageMutation {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

impl From<StorageMutation> for tikv_client::transaction::Mutation {
    fn from(value: StorageMutation) -> Self {
        match value {
            StorageMutation::Put(k, v) => tikv_client::transaction::Mutation::Put(k.into(), v),
            StorageMutation::Delete(k) => tikv_client::transaction::Mutation::Delete(k.into()),
        }
    }
}

// ─── Range ──────────────────────────────────────────────────────────────────

/// Backend-neutral byte-range using [`std::ops::Bound`] semantics.
///
/// `from_half_open(start, end)` mirrors the dominant TiKV pattern in this code
/// base — `(start..end).into()` over inclusive `start`, exclusive `end`. All 81
/// existing call sites use this shape (see PR body audit appendix). The single
/// conversion helper [`Self::to_tikv_bound_range`] is the only place that maps
/// these bounds onto `tikv_client::BoundRange`; tests depend on this so the
/// memory backend implementation in PR-3 can mirror the exact endpoint
/// inclusivity rules.
#[derive(Debug, Clone)]
pub(crate) struct StorageRange {
    pub(crate) start: Bound<Vec<u8>>,
    pub(crate) end: Bound<Vec<u8>>,
}

impl StorageRange {
    /// Construct a half-open range `[start, end)`. This matches the dominant
    /// TiKV usage in db9-server: `(start..end).into()`.
    pub(crate) fn from_half_open(start: Vec<u8>, end: Vec<u8>) -> Self {
        Self {
            start: Bound::Included(start),
            end: Bound::Excluded(end),
        }
    }

    /// Construct a fully-bounded range with explicit inclusivity.
    pub(crate) fn from_bounds(start: Bound<Vec<u8>>, end: Bound<Vec<u8>>) -> Self {
        Self { start, end }
    }

    /// Centralized conversion to [`tikv_client::BoundRange`].
    ///
    /// Spec-amendment B requires this conversion to be in one place so the
    /// memory backend cannot drift on inclusivity rules.
    pub(crate) fn to_tikv_bound_range(&self) -> BoundRange {
        let start: Bound<Key> = match &self.start {
            Bound::Included(k) => Bound::Included(k.clone().into()),
            Bound::Excluded(k) => Bound::Excluded(k.clone().into()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end: Bound<Key> = match &self.end {
            Bound::Included(k) => Bound::Included(k.clone().into()),
            Bound::Excluded(k) => Bound::Excluded(k.clone().into()),
            Bound::Unbounded => Bound::Unbounded,
        };
        BoundRange::new(start, end)
    }
}

// ─── Capabilities ───────────────────────────────────────────────────────────

/// Capability flags exposed by a storage backend. SQL planner / executor code
/// branches on these instead of downcasting backend variants. Memory backend
/// (PR-3) reports `supports_db9_cop = false`; production TiKV reports `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StorageCapabilities {
    pub(crate) supports_db9_cop: bool,
    pub(crate) supports_get_for_update: bool,
    pub(crate) supports_lock_skip_locked: bool,
    pub(crate) supports_snapshot_at_ts: bool,
}

impl StorageCapabilities {
    pub(crate) const fn tikv() -> Self {
        Self {
            supports_db9_cop: true,
            supports_get_for_update: true,
            supports_lock_skip_locked: true,
            supports_snapshot_at_ts: true,
        }
    }

    #[cfg(feature = "mock-storage")]
    pub(crate) const fn memory() -> Self {
        Self {
            supports_db9_cop: false,
            // PR-3 exposes optimistic MVCC core only. PR-4 flips these when
            // pessimistic locks / for-update / skip-locked behavior lands.
            supports_get_for_update: false,
            supports_lock_skip_locked: false,
            supports_snapshot_at_ts: true,
        }
    }
}

// ─── Backend variants ───────────────────────────────────────────────────────

/// Storage backend selector enum.
///
/// PR-1 ships the TiKV variant only. PR-3 introduces a `Memory` variant under
/// `#[cfg(feature = "mock-storage")]`. The enum dispatch keeps the call-site
/// surface backend-agnostic without requiring trait objects.
pub(crate) enum StorageClient {
    Tikv(std::sync::Arc<TransactionClient>),
    #[cfg(feature = "mock-storage")]
    Memory(MemoryClient),
}

impl StorageClient {
    pub(crate) fn capabilities(&self) -> StorageCapabilities {
        match self {
            StorageClient::Tikv(_) => StorageCapabilities::tikv(),
            #[cfg(feature = "mock-storage")]
            StorageClient::Memory(client) => client.capabilities(),
        }
    }

    /// Begin a pessimistic transaction wrapped in [`StorageTxn`]. Mirrors
    /// the existing `TikvStore::begin` pattern and keeps the migration window
    /// option open for callers that already hold a `StorageClient`.
    pub(crate) async fn begin(&self) -> Result<StorageTxn, anyhow::Error> {
        match self {
            StorageClient::Tikv(client) => {
                let options = TransactionOptions::new_pessimistic().drop_check(CheckLevel::Warn);
                let txn = tikv_op!(client.begin_with_options(options).await)?;
                Ok(StorageTxn::from_tikv(txn))
            }
            #[cfg(feature = "mock-storage")]
            StorageClient::Memory(client) => client
                .begin()
                .await
                .map(StorageTxn::from_memory)
                .map_err(anyhow::Error::new),
        }
    }

    /// Begin an optimistic transaction wrapped in [`StorageTxn`].
    pub(crate) async fn begin_optimistic(&self) -> Result<StorageTxn, anyhow::Error> {
        match self {
            StorageClient::Tikv(client) => {
                let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
                let txn = tikv_op!(client.begin_with_options(options).await)?;
                Ok(StorageTxn::from_tikv(txn))
            }
            #[cfg(feature = "mock-storage")]
            StorageClient::Memory(client) => client
                .begin_optimistic()
                .await
                .map(StorageTxn::from_memory)
                .map_err(anyhow::Error::new),
        }
    }

    /// Open a snapshot bound to the given timestamp.
    pub(crate) fn snapshot_at(&self, timestamp: Timestamp) -> StorageSnapshot {
        match self {
            StorageClient::Tikv(client) => {
                let read_ts_version = timestamp.version();
                let snap = client.snapshot(timestamp, TransactionOptions::new_optimistic());
                StorageSnapshot::Tikv {
                    snap,
                    read_ts_version,
                }
            }
            #[cfg(feature = "mock-storage")]
            StorageClient::Memory(client) => {
                StorageSnapshot::Memory(client.snapshot_at_version(timestamp.version()))
            }
        }
    }
}

/// Storage transaction handle.
///
/// The TiKV variant holds the underlying `tikv_client::Transaction` directly.
/// Both `tikv_client::Transaction::commit` and `tikv_client::Transaction::rollback`
/// already take `&mut self`, so the facade signatures match without indirection.
/// The companion `finished` flag enforces the spec invariant: once commit or
/// rollback succeeds (or is observed to fail), no subsequent commit / rollback
/// call may re-issue the underlying RPC. Forgetting to set `finished` in
/// commit/rollback is a correctness bug, per #2523 Consensus Amendments A.
#[allow(clippy::large_enum_variant)] // Keep production TiKV handles inline; mock-storage is test-only.
pub(crate) enum StorageTxn {
    Tikv {
        inner: Transaction,
        finished: bool,
    },
    #[cfg(feature = "mock-storage")]
    Memory(MemoryTxn),
}

/// Storage snapshot handle. Read methods take `&mut self` to match the
/// underlying `tikv_client::Snapshot` API (which itself takes `&mut self`).
///
/// The TiKV variant captures the bound `read_ts_version` alongside the
/// snapshot. The underlying `tikv_client::Snapshot` does not expose its
/// timestamp directly, so the facade records it at construction time and
/// returns it from [`StorageSnapshot::read_ts_version`].
#[allow(clippy::large_enum_variant)] // Avoid boxing the TiKV snapshot hot path for a test-only variant.
pub(crate) enum StorageSnapshot {
    Tikv {
        snap: tikv_client::Snapshot,
        read_ts_version: u64,
    },
    #[cfg(feature = "mock-storage")]
    Memory(MemorySnapshot),
}

// ─── Constructors (TiKV) ─────────────────────────────────────────────────────

impl StorageTxn {
    /// Wrap a real [`tikv_client::Transaction`] in the facade.
    pub(crate) fn from_tikv(txn: Transaction) -> Self {
        StorageTxn::Tikv {
            inner: txn,
            finished: false,
        }
    }

    #[cfg(feature = "mock-storage")]
    pub(crate) fn from_memory(txn: MemoryTxn) -> Self {
        StorageTxn::Memory(txn)
    }

    /// Returns true once `commit` or `rollback` has been driven to completion.
    /// Used by tests asserting the `finished` invariant; production callers
    /// should not need this.
    #[cfg(test)]
    pub(crate) fn is_finished(&self) -> bool {
        match self {
            StorageTxn::Tikv { finished, .. } => *finished,
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.is_finished(),
        }
    }
}

impl StorageSnapshot {
    /// Wrap a raw [`tikv_client::Snapshot`] together with the timestamp it was
    /// opened at. Callers should use [`StorageClient::snapshot_at`] in new code;
    /// this helper exists so PR-1's `TikvStore::snapshot_facade` can build a
    /// facade snapshot from a `Timestamp` it already has on hand.
    pub(crate) fn from_tikv(snap: tikv_client::Snapshot, read_ts_version: u64) -> Self {
        StorageSnapshot::Tikv {
            snap,
            read_ts_version,
        }
    }
}

// ─── Transaction operations ──────────────────────────────────────────────────

impl StorageTxn {
    pub(crate) fn capabilities(&self) -> StorageCapabilities {
        match self {
            StorageTxn::Tikv { .. } => StorageCapabilities::tikv(),
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.capabilities(),
        }
    }

    /// Returns the transaction's start timestamp version, matching the
    /// existing `tikv_client::Transaction::start_timestamp().version()` use
    /// at `src/session_context.rs:327`.
    pub(crate) fn start_ts_version(&self) -> u64 {
        match self {
            StorageTxn::Tikv { inner, .. } => inner.start_timestamp().version(),
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.start_ts_version(),
        }
    }

    fn require_open(
        inner: &mut Transaction,
        finished: bool,
    ) -> Result<&mut Transaction, anyhow::Error> {
        if finished {
            return Err(anyhow::anyhow!(
                "storage facade: transaction already finished"
            ));
        }
        Ok(inner)
    }

    pub(crate) async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>, anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                let txn = Self::require_open(inner, *finished)?;
                tikv_op!(txn.get(key).await).map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.get(key).await.map_err(anyhow::Error::new),
        }
    }

    pub(crate) async fn batch_get(
        &mut self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<KvPair>, anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                let txn = Self::require_open(inner, *finished)?;
                let pairs = tikv_op!(txn.batch_get(keys).await)?;
                Ok(pairs.collect())
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.batch_get(keys).await.map_err(anyhow::Error::new),
        }
    }

    pub(crate) async fn scan(
        &mut self,
        range: StorageRange,
        limit: u32,
    ) -> Result<Vec<KvPair>, anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                let txn = Self::require_open(inner, *finished)?;
                let pairs = tikv_op!(txn.scan(range.to_tikv_bound_range(), limit).await)?;
                Ok(pairs.collect())
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.scan(range, limit).await.map_err(anyhow::Error::new),
        }
    }

    pub(crate) async fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                let txn = Self::require_open(inner, *finished)?;
                #[allow(clippy::disallowed_methods)]
                tikv_op!(txn.put(key, value).await).map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.put(key, value).await.map_err(anyhow::Error::new),
        }
    }

    pub(crate) async fn delete(&mut self, key: Vec<u8>) -> Result<(), anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                let txn = Self::require_open(inner, *finished)?;
                tikv_op!(txn.delete(key).await).map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.delete(key).await.map_err(anyhow::Error::new),
        }
    }

    pub(crate) async fn batch_mutate<I>(&mut self, mutations: I) -> Result<(), anyhow::Error>
    where
        I: IntoIterator<Item = StorageMutation>,
    {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                let txn = Self::require_open(inner, *finished)?;
                let tikv_muts: Vec<tikv_client::transaction::Mutation> =
                    mutations.into_iter().map(Into::into).collect();
                tikv_op!(txn.batch_mutate(tikv_muts).await).map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn
                .batch_mutate(mutations)
                .await
                .map_err(anyhow::Error::new),
        }
    }

    /// Commit the transaction. Takes `&mut self` per spec amendment A. The
    /// underlying `tikv_client::Transaction::commit` already takes `&mut self`,
    /// so the facade is a thin wrapper. After commit returns, `finished` is
    /// set to true and any subsequent `commit`/`rollback` call returns an
    /// error rather than re-issuing the RPC. This is the spec's `finished`
    /// invariant.
    pub(crate) async fn commit(&mut self) -> Result<(), anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                if *finished {
                    return Err(anyhow::anyhow!(
                        "storage facade: commit called after finish"
                    ));
                }
                let result = tikv_op!(inner.commit().await).map(|_| ());
                *finished = true;
                result.map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.commit().await.map_err(anyhow::Error::new),
        }
    }

    /// Rollback the transaction. Same `&mut self` and `finished`-invariant
    /// pattern as [`Self::commit`].
    pub(crate) async fn rollback(&mut self) -> Result<(), anyhow::Error> {
        match self {
            StorageTxn::Tikv { inner, finished } => {
                if *finished {
                    return Err(anyhow::anyhow!(
                        "storage facade: rollback called after finish"
                    ));
                }
                let result = tikv_op!(inner.rollback().await);
                *finished = true;
                result.map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageTxn::Memory(txn) => txn.rollback().await.map_err(anyhow::Error::new),
        }
    }
}

// ─── Snapshot operations ─────────────────────────────────────────────────────

impl StorageSnapshot {
    pub(crate) fn capabilities(&self) -> StorageCapabilities {
        match self {
            StorageSnapshot::Tikv { .. } => StorageCapabilities::tikv(),
            #[cfg(feature = "mock-storage")]
            StorageSnapshot::Memory(snapshot) => snapshot.capabilities(),
        }
    }

    /// Returns the bound `read_ts` of this snapshot.
    ///
    /// The TiKV variant captures the timestamp at construction (via
    /// [`StorageClient::snapshot_at`] or [`StorageSnapshot::from_tikv`]) so
    /// callers and parity tests can verify the snapshot resolution boundary
    /// without depending on an internal `tikv_client::Snapshot` accessor.
    pub(crate) fn read_ts_version(&self) -> u64 {
        match self {
            StorageSnapshot::Tikv {
                read_ts_version, ..
            } => *read_ts_version,
            #[cfg(feature = "mock-storage")]
            StorageSnapshot::Memory(snapshot) => snapshot.read_ts_version(),
        }
    }

    pub(crate) async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>, anyhow::Error> {
        match self {
            StorageSnapshot::Tikv { snap, .. } => {
                tikv_op!(snap.get(key).await).map_err(anyhow::Error::from)
            }
            #[cfg(feature = "mock-storage")]
            StorageSnapshot::Memory(snapshot) => {
                snapshot.get(key).await.map_err(anyhow::Error::new)
            }
        }
    }

    pub(crate) async fn batch_get(
        &mut self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<KvPair>, anyhow::Error> {
        match self {
            StorageSnapshot::Tikv { snap, .. } => {
                let pairs = tikv_op!(snap.batch_get(keys).await)?;
                Ok(pairs.collect())
            }
            #[cfg(feature = "mock-storage")]
            StorageSnapshot::Memory(snapshot) => {
                snapshot.batch_get(keys).await.map_err(anyhow::Error::new)
            }
        }
    }

    pub(crate) async fn scan(
        &mut self,
        range: StorageRange,
        limit: u32,
    ) -> Result<Vec<KvPair>, anyhow::Error> {
        match self {
            StorageSnapshot::Tikv { snap, .. } => {
                let pairs = tikv_op!(snap.scan(range.to_tikv_bound_range(), limit).await)?;
                Ok(pairs.collect())
            }
            #[cfg(feature = "mock-storage")]
            StorageSnapshot::Memory(snapshot) => snapshot
                .scan(range, limit)
                .await
                .map_err(anyhow::Error::new),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_range_half_open_uses_inclusive_start_exclusive_end() {
        let r = StorageRange::from_half_open(b"a".to_vec(), b"z".to_vec());
        match r.start {
            Bound::Included(ref k) => assert_eq!(k.as_slice(), b"a"),
            other => panic!("expected Included start, got {other:?}"),
        }
        match r.end {
            Bound::Excluded(ref k) => assert_eq!(k.as_slice(), b"z"),
            other => panic!("expected Excluded end, got {other:?}"),
        }
    }

    #[test]
    fn storage_range_explicit_bounds_are_preserved_through_conversion() {
        let r = StorageRange::from_bounds(
            Bound::Included(b"alpha".to_vec()),
            Bound::Included(b"omega".to_vec()),
        );
        // The conversion must round-trip the inclusivity onto the TiKV side
        // so the memory backend in PR-3 can match these rules byte-for-byte.
        let bound_range = r.to_tikv_bound_range();
        let (from, to) = bound_range.into_keys();
        let from_bytes: Vec<u8> = from.into();
        assert_eq!(from_bytes.as_slice(), b"alpha");
        let to = to.expect("inclusive upper bound must materialize a key");
        // The TiKV BoundRange::into_keys() encodes inclusive upper bounds as
        // the next key after the inclusive endpoint.
        let to_bytes: Vec<u8> = to.into();
        assert!(to_bytes.as_slice().starts_with(b"omega"));
    }

    #[test]
    fn storage_range_unbounded_endpoints_round_trip() {
        let r = StorageRange::from_bounds(Bound::Unbounded, Bound::Unbounded);
        let bound_range = r.to_tikv_bound_range();
        let (from, to) = bound_range.into_keys();
        let from_bytes: Vec<u8> = from.into();
        assert!(from_bytes.is_empty());
        assert!(to.is_none());
    }

    #[test]
    fn storage_mutation_converts_to_tikv() {
        let put: tikv_client::transaction::Mutation =
            StorageMutation::Put(b"k".to_vec(), b"v".to_vec()).into();
        let del: tikv_client::transaction::Mutation = StorageMutation::Delete(b"k".to_vec()).into();
        match put {
            tikv_client::transaction::Mutation::Put(k, v) => {
                assert_eq!(<Key as Into<Vec<u8>>>::into(k), b"k".to_vec());
                assert_eq!(v, b"v".to_vec());
            }
            other => panic!("expected Put, got {other:?}"),
        }
        match del {
            tikv_client::transaction::Mutation::Delete(k) => {
                assert_eq!(<Key as Into<Vec<u8>>>::into(k), b"k".to_vec());
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn storage_capabilities_tikv_advertises_full_surface() {
        let caps = StorageCapabilities::tikv();
        assert!(caps.supports_db9_cop);
        assert!(caps.supports_get_for_update);
        assert!(caps.supports_lock_skip_locked);
        assert!(caps.supports_snapshot_at_ts);
    }

    // `StorageError::is_retryable` classification is tested in
    // `src/storage/error.rs::tests::classifies_retryable_variants`. The
    // facade re-exports the type so consumers in PR-1.5 can `use
    // crate::storage::facade::StorageError` if they prefer that path; the
    // single source of truth for the retry contract lives in
    // `crate::storage::error`.
}
