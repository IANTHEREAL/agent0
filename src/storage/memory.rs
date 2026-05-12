//! Feature-gated in-memory storage backend for tests and local debugging.
//!
//! This module implements the PR-3 slice of issue #2523: keyspace-scoped
//! `MemoryClient`, optimistic MVCC transactions, byte-ordered scans, explicit
//! rollback/commit/drop cleanup, fixture assertions, and lightweight metrics.
//! Pessimistic locking APIs are intentionally deferred to PR-4; the lock table
//! exists here only so drop/fixture invariants have the final shape.

// PR-3 publishes the memory backend core before PR-6 wires the SQL backend
// selector and parity harness. Keep this scoped allowance until those callers
// land; tests in this module exercise the public fixture surface meanwhile.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use tikv_client::KvPair;
use tokio::sync::{Notify, RwLock};
use tokio::time::Instant;

use super::facade::{StorageCapabilities, StorageMutation, StorageRange};
use super::{StorageError, WriteConflictReason};

pub(crate) type TxnId = u64;

const DEFAULT_MEMORY_LOCK_WAIT_TIMEOUT_MS: u64 = 10_000;

/// Shared in-process memory backend state.
///
/// One universe owns a single logical MVCC clock across all keyspaces, matching
/// the TiKV/PD timestamp assumption db9 code relies on. Tests should usually
/// allocate a fresh `Arc<MemoryUniverse>` per fixture.
pub(crate) struct MemoryUniverse {
    clock: AtomicU64,
    next_txn_id: AtomicU64,
    keyspaces: DashMap<String, Arc<KeyspaceHandle>>,
}

/// Keyspace-scoped client handle.
///
/// This mirrors the current `TikvStore::new_with_keyspace` contract: callers
/// do not simulate keyspaces with ad-hoc prefixes; they request a scoped
/// client from [`MemoryUniverse::client_for_keyspace`].
#[derive(Clone)]
pub(crate) struct MemoryClient {
    universe: Arc<MemoryUniverse>,
    keyspace: Arc<KeyspaceHandle>,
}

struct KeyspaceHandle {
    name: Arc<str>,
    reset_epoch: AtomicU64,
    data: RwLock<BTreeMap<Vec<u8>, Vec<Version>>>,
    locks: Mutex<LockTable>,
    active_txn_start_ts: Mutex<HashMap<TxnId, u64>>,
    lock_notify: Notify,
}

#[derive(Default)]
struct LockTable {
    owners: HashMap<Vec<u8>, TxnId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockAttempt {
    Acquired,
    AlreadyOwned,
    Blocked,
}

#[derive(Clone, Debug)]
struct Version {
    commit_ts: u64,
    value: Option<Vec<u8>>,
}

/// In-memory transaction handle.
///
/// Plain reads consult `writes` before committed versions. Commit validation
/// checks optimistic conflicts while holding the keyspace data write lock and
/// applies all staged writes at one commit timestamp.
pub(crate) struct MemoryTxn {
    txn_id: TxnId,
    universe: Arc<MemoryUniverse>,
    keyspace: Arc<KeyspaceHandle>,
    reset_epoch: u64,
    start_ts: u64,
    optimistic: bool,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    held_locks: BTreeMap<Vec<u8>, u64>,
    finished: bool,
}

/// Snapshot reader bound to a fixed memory MVCC timestamp.
pub(crate) struct MemorySnapshot {
    keyspace: Arc<KeyspaceHandle>,
    reset_epoch: u64,
    read_ts: u64,
}

impl MemoryUniverse {
    pub(crate) fn new() -> Self {
        Self {
            clock: AtomicU64::new(0),
            next_txn_id: AtomicU64::new(0),
            keyspaces: DashMap::new(),
        }
    }

    pub(crate) fn client_for_keyspace(self: &Arc<Self>, name: impl AsRef<str>) -> MemoryClient {
        let name = name.as_ref();
        let keyspace = self
            .keyspaces
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(KeyspaceHandle::new(name)))
            .clone();
        MemoryClient {
            universe: Arc::clone(self),
            keyspace,
        }
    }

    pub(crate) fn current_ts(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn next_ts(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn next_txn_id(&self) -> TxnId {
        self.next_txn_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) async fn clear(&self) {
        for keyspace in self.keyspaces.iter() {
            keyspace.clear().await;
        }
        self.clock.store(0, Ordering::SeqCst);
        // Stale transaction handles may still run rollback/drop cleanup after a
        // fixture reset. Keep transaction IDs monotonic across `clear()` so
        // stale handles cannot release locks owned by fresh post-reset txns.
        record_keyspace_bytes(0);
        record_active_txns(0);
    }

    pub(crate) fn assert_clean(&self) {
        for keyspace in self.keyspaces.iter() {
            keyspace.assert_clean();
        }
    }
}

impl Default for MemoryUniverse {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryClient {
    pub(crate) fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::memory()
    }

    pub(crate) fn keyspace_name(&self) -> &str {
        &self.keyspace.name
    }

    pub(crate) async fn begin(&self) -> Result<MemoryTxn, StorageError> {
        self.begin_with_mode(false).await
    }

    pub(crate) async fn begin_optimistic(&self) -> Result<MemoryTxn, StorageError> {
        self.begin_with_mode(true).await
    }

    async fn begin_with_mode(&self, optimistic: bool) -> Result<MemoryTxn, StorageError> {
        let txn_id = self.universe.next_txn_id();
        let start_ts = self.universe.next_ts();
        self.keyspace.register_txn(txn_id, start_ts);
        record_active_txns(self.keyspace.active_txn_count());
        Ok(MemoryTxn {
            txn_id,
            universe: Arc::clone(&self.universe),
            keyspace: Arc::clone(&self.keyspace),
            reset_epoch: self.keyspace.reset_epoch(),
            start_ts,
            optimistic,
            writes: BTreeMap::new(),
            held_locks: BTreeMap::new(),
            finished: false,
        })
    }

    pub(crate) fn snapshot_at_version(&self, read_ts: u64) -> MemorySnapshot {
        MemorySnapshot {
            keyspace: Arc::clone(&self.keyspace),
            reset_epoch: self.keyspace.reset_epoch(),
            read_ts,
        }
    }

    pub(crate) async fn clear(&self) {
        self.keyspace.clear().await;
        record_keyspace_bytes(0);
        record_active_txns(self.keyspace.active_txn_count());
    }

    pub(crate) fn assert_clean(&self) {
        self.keyspace.assert_clean();
    }
}

impl KeyspaceHandle {
    fn new(name: &str) -> Self {
        Self {
            name: Arc::from(name),
            reset_epoch: AtomicU64::new(0),
            data: RwLock::new(BTreeMap::new()),
            locks: Mutex::new(LockTable::default()),
            active_txn_start_ts: Mutex::new(HashMap::new()),
            lock_notify: Notify::new(),
        }
    }

    fn register_txn(&self, txn_id: TxnId, start_ts: u64) {
        self.active_txn_start_ts.lock().insert(txn_id, start_ts);
    }

    fn release_txn(&self, txn_id: TxnId, held_locks: &BTreeMap<Vec<u8>, u64>) {
        self.active_txn_start_ts.lock().remove(&txn_id);
        if !held_locks.is_empty() {
            let mut locks = self.locks.lock();
            for key in held_locks.keys() {
                if locks.owners.get(key) == Some(&txn_id) {
                    locks.owners.remove(key);
                }
            }
        }
        self.lock_notify.notify_waiters();
        record_active_txns(self.active_txn_count());
    }

    fn release_locks(&self, txn_id: TxnId, keys: &[Vec<u8>]) {
        if keys.is_empty() {
            return;
        }
        let mut locks = self.locks.lock();
        for key in keys {
            if locks.owners.get(key) == Some(&txn_id) {
                locks.owners.remove(key);
            }
        }
        drop(locks);
        self.lock_notify.notify_waiters();
    }

    fn try_claim_lock(&self, txn_id: TxnId, key: &[u8]) -> LockAttempt {
        let mut locks = self.locks.lock();
        match locks.owners.get(key) {
            Some(owner) if *owner == txn_id => LockAttempt::AlreadyOwned,
            Some(_) => LockAttempt::Blocked,
            None => {
                locks.owners.insert(key.to_vec(), txn_id);
                LockAttempt::Acquired
            }
        }
    }

    fn lock_owner(&self, key: &[u8]) -> Option<TxnId> {
        self.locks.lock().owners.get(key).copied()
    }

    async fn latest_commit_ts(&self, key: &[u8]) -> u64 {
        let data = self.data.read().await;
        data.get(key)
            .and_then(|versions| versions.last())
            .map_or(0, |version| version.commit_ts)
    }

    fn active_txn_count(&self) -> usize {
        self.active_txn_start_ts.lock().len()
    }

    fn reset_epoch(&self) -> u64 {
        self.reset_epoch.load(Ordering::SeqCst)
    }

    async fn clear(&self) {
        self.reset_epoch.fetch_add(1, Ordering::SeqCst);
        self.data.write().await.clear();
        self.active_txn_start_ts.lock().clear();
        self.locks.lock().owners.clear();
        self.lock_notify.notify_waiters();
    }

    fn assert_clean(&self) {
        let active = self.active_txn_start_ts.lock().len();
        assert_eq!(
            active, 0,
            "memory storage keyspace `{}` leaked {active} active transaction(s)",
            self.name
        );
        let locks = self.locks.lock().owners.len();
        assert_eq!(
            locks, 0,
            "memory storage keyspace `{}` leaked {locks} lock(s)",
            self.name
        );
    }
}

impl MemoryTxn {
    pub(crate) fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::memory()
    }

    pub(crate) fn start_ts_version(&self) -> u64 {
        self.start_ts
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(crate) async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>, StorageError> {
        self.ensure_open()?;
        let data = self.keyspace.data.read().await;
        self.ensure_current_epoch()?;
        if let Some(value) = self.writes.get(&key) {
            return Ok(value.clone());
        }
        Ok(data
            .get(&key)
            .and_then(|versions| visible_value_at(versions, self.start_ts)))
    }

    pub(crate) async fn batch_get(
        &mut self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<KvPair>, StorageError> {
        self.ensure_open()?;
        let mut pairs = Vec::new();
        for key in keys {
            if let Some(value) = self.get(key.clone()).await? {
                pairs.push(KvPair::new(key, value));
            }
        }
        Ok(pairs)
    }

    pub(crate) async fn scan(
        &mut self,
        range: StorageRange,
        limit: u32,
    ) -> Result<Vec<KvPair>, StorageError> {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        scan_visible_with_overlay(
            &self.keyspace,
            self.start_ts,
            Some(&self.writes),
            range,
            limit,
        )
        .await
    }

    pub(crate) async fn get_for_update(
        &mut self,
        key: Vec<u8>,
        timeout: Option<Duration>,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        self.lock_keys([key.clone()], timeout).await?;
        let data = self.keyspace.data.read().await;
        self.ensure_current_epoch()?;
        Ok(data
            .get(&key)
            .and_then(|versions| visible_value_at(versions, u64::MAX)))
    }

    pub(crate) async fn batch_get_for_update<I>(
        &mut self,
        keys: I,
        timeout: Option<Duration>,
    ) -> Result<Vec<KvPair>, StorageError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        let keys: Vec<Vec<u8>> = keys.into_iter().collect();
        self.lock_keys(keys.clone(), timeout).await?;

        let data = self.keyspace.data.read().await;
        self.ensure_current_epoch()?;
        let mut pairs = Vec::new();
        for key in keys {
            if let Some(value) = data
                .get(&key)
                .and_then(|versions| visible_value_at(versions, u64::MAX))
            {
                pairs.push(KvPair::new(key, value));
            }
        }
        Ok(pairs)
    }

    pub(crate) async fn lock_keys<I>(
        &mut self,
        keys: I,
        timeout: Option<Duration>,
    ) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        self.acquire_locks(keys, LockWait::Blocking(timeout)).await
    }

    pub(crate) async fn lock_keys_nowait<I>(&mut self, keys: I) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        self.acquire_locks(keys, LockWait::Nowait).await
    }

    pub(crate) async fn lock_keys_skip_locked<I>(
        &mut self,
        keys: I,
        max_locks: Option<usize>,
    ) -> Result<Vec<Vec<u8>>, StorageError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        let mut locked = Vec::new();
        let max_locks = max_locks.unwrap_or(usize::MAX);
        for key in keys {
            if locked.len() >= max_locks {
                break;
            }
            match self.lock_keys_nowait([key.clone()]).await {
                Ok(()) => locked.push(key),
                Err(StorageError::LockNotAvailable) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(locked)
    }

    pub(crate) async fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), StorageError> {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        if !self.optimistic {
            self.lock_keys([key.clone()], None).await?;
        }
        self.writes.insert(key, Some(value));
        Ok(())
    }

    pub(crate) async fn delete(&mut self, key: Vec<u8>) -> Result<(), StorageError> {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        if !self.optimistic {
            self.lock_keys([key.clone()], None).await?;
        }
        self.writes.insert(key, None);
        Ok(())
    }

    pub(crate) async fn batch_mutate<I>(&mut self, mutations: I) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = StorageMutation>,
    {
        self.ensure_open()?;
        self.ensure_current_epoch()?;
        if self.optimistic {
            for mutation in mutations {
                self.apply_mutation(mutation);
            }
            return Ok(());
        }

        let mutations: Vec<StorageMutation> = mutations.into_iter().collect();
        let lock_keys = mutations.iter().map(mutation_key).collect::<Vec<_>>();
        self.lock_keys(lock_keys, None).await?;
        for mutation in mutations {
            self.apply_mutation(mutation);
        }
        Ok(())
    }

    fn apply_mutation(&mut self, mutation: StorageMutation) {
        match mutation {
            StorageMutation::Put(key, value) => {
                self.writes.insert(key, Some(value));
            }
            StorageMutation::Delete(key) => {
                self.writes.insert(key, None);
            }
        }
    }

    async fn acquire_locks<I>(&mut self, keys: I, wait: LockWait) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        if keys.is_empty() {
            return Ok(());
        }
        keys.sort();
        keys.dedup();

        let deadline = wait.deadline();
        let mut newly_acquired = Vec::new();
        for key in keys {
            match acquire_one_lock(
                Arc::clone(&self.keyspace),
                self.txn_id,
                self.reset_epoch,
                key.clone(),
                wait,
                deadline,
            )
            .await
            {
                Ok(Some(observed_commit_ts)) => {
                    self.held_locks.insert(key.clone(), observed_commit_ts);
                    newly_acquired.push(key);
                    record_lock_acquired("ok");
                }
                Ok(None) => {}
                Err(err) => {
                    self.keyspace.release_locks(self.txn_id, &newly_acquired);
                    for key in newly_acquired {
                        self.held_locks.remove(&key);
                    }
                    record_lock_acquired(lock_error_outcome(&err));
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn validate_lock_conflicts(&self) -> Result<(), StorageError> {
        for key in self.writes.keys() {
            if let Some(owner) = self.keyspace.lock_owner(key) {
                if owner != self.txn_id {
                    return Err(StorageError::LockConflict);
                }
            }
        }
        Ok(())
    }

    fn validate_pessimistic_locks(
        &self,
        data: &BTreeMap<Vec<u8>, Vec<Version>>,
    ) -> Result<(), StorageError> {
        for key in self.writes.keys() {
            let Some(locked_commit_ts) = self.held_locks.get(key) else {
                return Err(StorageError::WriteConflict {
                    reason: WriteConflictReason::Pessimistic,
                });
            };
            if self.keyspace.lock_owner(key) != Some(self.txn_id) {
                return Err(StorageError::WriteConflict {
                    reason: WriteConflictReason::Pessimistic,
                });
            }
            let latest_commit_ts = data
                .get(key)
                .and_then(|versions| versions.last())
                .map_or(0, |version| version.commit_ts);
            if latest_commit_ts > *locked_commit_ts {
                return Err(StorageError::WriteConflict {
                    reason: WriteConflictReason::Pessimistic,
                });
            }
        }
        Ok(())
    }

    fn validate_optimistic_conflicts(
        &self,
        data: &BTreeMap<Vec<u8>, Vec<Version>>,
    ) -> Result<(), StorageError> {
        for key in self.writes.keys() {
            if let Some(latest) = data.get(key).and_then(|versions| versions.last()) {
                if latest.commit_ts > self.start_ts {
                    return Err(StorageError::WriteConflict {
                        reason: WriteConflictReason::Optimistic,
                    });
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn commit(&mut self) -> Result<(), StorageError> {
        self.ensure_open()?;
        let result = self.commit_inner().await;
        self.finish();
        match &result {
            Ok(()) => metrics::counter!("db9_server_storage_memory_commits_total").increment(1),
            Err(StorageError::WriteConflict { .. }) => {
                metrics::counter!("db9_server_storage_memory_write_conflicts_total").increment(1)
            }
            Err(_) => {}
        }
        result
    }

    pub(crate) async fn rollback(&mut self) -> Result<(), StorageError> {
        self.ensure_open()?;
        self.writes.clear();
        self.finish();
        metrics::counter!("db9_server_storage_memory_rollbacks_total").increment(1);
        Ok(())
    }

    async fn commit_inner(&mut self) -> Result<(), StorageError> {
        self.ensure_current_epoch()?;
        if self.writes.is_empty() {
            return Ok(());
        }

        self.validate_lock_conflicts()?;
        let mut data = self.keyspace.data.write().await;
        self.ensure_current_epoch()?;
        if self.optimistic {
            self.validate_optimistic_conflicts(&data)?;
        } else {
            self.validate_pessimistic_locks(&data)?;
        }

        let commit_ts = self.universe.next_ts();
        for (key, value) in &self.writes {
            data.entry(key.clone()).or_default().push(Version {
                commit_ts,
                value: value.clone(),
            });
        }
        record_keyspace_bytes(data_bytes(&data));
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), StorageError> {
        if self.finished {
            return Err(StorageError::Internal(
                "memory transaction already finished".to_string(),
            ));
        }
        Ok(())
    }

    fn ensure_current_epoch(&self) -> Result<(), StorageError> {
        if self.keyspace.reset_epoch() != self.reset_epoch {
            return Err(StorageError::Internal(
                "memory transaction invalidated by fixture reset".to_string(),
            ));
        }
        Ok(())
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.keyspace.release_txn(self.txn_id, &self.held_locks);
    }
}

impl Drop for MemoryTxn {
    fn drop(&mut self) {
        if !self.finished {
            self.finish();
        }
    }
}

#[derive(Clone, Copy)]
enum LockWait {
    Blocking(Option<Duration>),
    Nowait,
}

impl LockWait {
    fn deadline(self) -> Option<Instant> {
        match self {
            LockWait::Blocking(timeout) => {
                let timeout = timeout.unwrap_or_else(memory_lock_wait_timeout);
                Some(Instant::now() + timeout)
            }
            LockWait::Nowait => None,
        }
    }

    fn is_nowait(self) -> bool {
        matches!(self, LockWait::Nowait)
    }
}

async fn acquire_one_lock(
    keyspace: Arc<KeyspaceHandle>,
    txn_id: TxnId,
    reset_epoch: u64,
    key: Vec<u8>,
    wait: LockWait,
    deadline: Option<Instant>,
) -> Result<Option<u64>, StorageError> {
    loop {
        ensure_keyspace_epoch(&keyspace, reset_epoch, "memory transaction")?;
        match keyspace.try_claim_lock(txn_id, &key) {
            LockAttempt::Acquired => {
                let observed_commit_ts = keyspace.latest_commit_ts(&key).await;
                ensure_keyspace_epoch(&keyspace, reset_epoch, "memory transaction")?;
                return Ok(Some(observed_commit_ts));
            }
            LockAttempt::AlreadyOwned => return Ok(None),
            LockAttempt::Blocked if wait.is_nowait() => return Err(StorageError::LockNotAvailable),
            LockAttempt::Blocked => {}
        }

        let notified = keyspace.lock_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        match keyspace.try_claim_lock(txn_id, &key) {
            LockAttempt::Acquired => {
                let observed_commit_ts = keyspace.latest_commit_ts(&key).await;
                ensure_keyspace_epoch(&keyspace, reset_epoch, "memory transaction")?;
                return Ok(Some(observed_commit_ts));
            }
            LockAttempt::AlreadyOwned => return Ok(None),
            LockAttempt::Blocked => {}
        }

        let Some(deadline) = deadline else {
            return Err(StorageError::Internal(
                "memory lock wait deadline missing".to_string(),
            ));
        };
        let now = Instant::now();
        if now >= deadline {
            return Err(StorageError::LockTimeout);
        }
        let wait_for = deadline.saturating_duration_since(now);
        let wait_started = Instant::now();
        match tokio::time::timeout(wait_for, notified).await {
            Ok(()) => record_lock_wait(wait_started.elapsed()),
            Err(_) => {
                record_lock_wait(wait_started.elapsed());
                return Err(StorageError::LockTimeout);
            }
        }
    }
}

fn ensure_keyspace_epoch(
    keyspace: &KeyspaceHandle,
    expected_epoch: u64,
    handle_name: &'static str,
) -> Result<(), StorageError> {
    if keyspace.reset_epoch() != expected_epoch {
        return Err(StorageError::Internal(format!(
            "{handle_name} invalidated by fixture reset"
        )));
    }
    Ok(())
}

fn memory_lock_wait_timeout() -> Duration {
    std::env::var("DB9_MEMORY_LOCK_WAIT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_MEMORY_LOCK_WAIT_TIMEOUT_MS))
}

fn mutation_key(mutation: &StorageMutation) -> Vec<u8> {
    match mutation {
        StorageMutation::Put(key, _) | StorageMutation::Delete(key) => key.clone(),
    }
}

fn lock_error_outcome(error: &StorageError) -> &'static str {
    match error {
        StorageError::LockNotAvailable => "not_available",
        StorageError::LockTimeout => "timeout",
        _ => "error",
    }
}

impl MemorySnapshot {
    pub(crate) fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::memory()
    }

    pub(crate) fn read_ts_version(&self) -> u64 {
        self.read_ts
    }

    pub(crate) async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>, StorageError> {
        let data = self.keyspace.data.read().await;
        self.ensure_current_epoch()?;
        Ok(data
            .get(&key)
            .and_then(|versions| visible_value_at(versions, self.read_ts)))
    }

    pub(crate) async fn batch_get(
        &mut self,
        keys: Vec<Vec<u8>>,
    ) -> Result<Vec<KvPair>, StorageError> {
        let mut pairs = Vec::new();
        for key in keys {
            if let Some(value) = self.get(key.clone()).await? {
                pairs.push(KvPair::new(key, value));
            }
        }
        Ok(pairs)
    }

    pub(crate) async fn scan(
        &mut self,
        range: StorageRange,
        limit: u32,
    ) -> Result<Vec<KvPair>, StorageError> {
        self.ensure_current_epoch()?;
        scan_visible_with_overlay(&self.keyspace, self.read_ts, None, range, limit).await
    }

    fn ensure_current_epoch(&self) -> Result<(), StorageError> {
        if self.keyspace.reset_epoch() != self.reset_epoch {
            return Err(StorageError::Internal(
                "memory snapshot invalidated by fixture reset".to_string(),
            ));
        }
        Ok(())
    }
}

fn visible_value_at(versions: &[Version], read_ts: u64) -> Option<Vec<u8>> {
    versions
        .iter()
        .rev()
        .find(|version| version.commit_ts <= read_ts)
        .and_then(|version| version.value.clone())
}

async fn scan_visible_with_overlay(
    keyspace: &KeyspaceHandle,
    read_ts: u64,
    overlay: Option<&BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
    range: StorageRange,
    limit: u32,
) -> Result<Vec<KvPair>, StorageError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut visible = BTreeMap::new();
    {
        let data = keyspace.data.read().await;
        for (key, versions) in data.range((range.start.clone(), range.end.clone())) {
            if let Some(value) = visible_value_at(versions, read_ts) {
                visible.insert(key.clone(), value);
            }
        }
    }

    if let Some(writes) = overlay {
        for (key, value) in writes.range((range.start.clone(), range.end.clone())) {
            match value {
                Some(value) => {
                    visible.insert(key.clone(), value.clone());
                }
                None => {
                    visible.remove(key);
                }
            }
        }
    }

    Ok(visible
        .into_iter()
        .take(limit as usize)
        .map(KvPair::from)
        .collect())
}

fn data_bytes(data: &BTreeMap<Vec<u8>, Vec<Version>>) -> usize {
    data.iter()
        .map(|(key, versions)| {
            versions
                .iter()
                .map(|version| key.len() + version.value.as_ref().map_or(0, Vec::len))
                .sum::<usize>()
        })
        .sum()
}

fn record_active_txns(count: usize) {
    metrics::gauge!("db9_server_storage_memory_active_txns").set(count as f64);
}

fn record_keyspace_bytes(bytes: usize) {
    metrics::gauge!("db9_server_storage_memory_keyspace_bytes").set(bytes as f64);
}

fn record_lock_acquired(outcome: &'static str) {
    metrics::counter!("db9_server_storage_memory_lock_acquired_total", "outcome" => outcome)
        .increment(1);
}

fn record_lock_wait(duration: Duration) {
    metrics::histogram!("db9_server_storage_memory_lock_wait_seconds").record(duration);
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use proptest::prelude::*;

    use super::*;

    fn key(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    fn pair_bytes(pair: KvPair) -> (Vec<u8>, Vec<u8>) {
        let key: Vec<u8> = pair.key().clone().into();
        (key, pair.value().clone())
    }

    #[tokio::test]
    async fn commits_and_rolls_back_plain_writes() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");
        assert_eq!(client.keyspace_name(), "tenant-a");
        assert!(!client.capabilities().supports_db9_cop);
        assert!(client.capabilities().supports_get_for_update);
        assert!(client.capabilities().supports_lock_skip_locked);
        assert!(client.capabilities().supports_snapshot_at_ts);

        let mut txn = client.begin_optimistic().await.unwrap();
        txn.put(key(b"k1"), key(b"v1")).await.unwrap();
        assert_eq!(txn.get(key(b"k1")).await.unwrap(), Some(key(b"v1")));
        txn.commit().await.unwrap();
        assert!(txn.is_finished());

        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k1")).await.unwrap(), Some(key(b"v1")));
        reader.rollback().await.unwrap();

        let mut rollback = client.begin_optimistic().await.unwrap();
        rollback.put(key(b"k1"), key(b"v2")).await.unwrap();
        rollback.rollback().await.unwrap();

        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k1")).await.unwrap(), Some(key(b"v1")));
        reader.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn fixture_clear_removes_data_and_active_state() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut txn = client.begin_optimistic().await.unwrap();
        txn.put(key(b"k"), key(b"v")).await.unwrap();
        txn.commit().await.unwrap();

        client.clear().await;
        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k")).await.unwrap(), None);
        reader.rollback().await.unwrap();

        universe.clear().await;
        universe.assert_clean();
    }

    #[tokio::test]
    async fn clear_invalidates_pre_existing_transactions() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut stale = client.begin_optimistic().await.unwrap();
        stale.put(key(b"k"), key(b"stale")).await.unwrap();
        client.clear().await;
        client.assert_clean();

        let err = stale.commit().await.unwrap_err();
        assert!(
            matches!(err, StorageError::Internal(message) if message.contains("fixture reset"))
        );
        assert!(stale.is_finished());

        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k")).await.unwrap(), None);
        reader.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn clear_invalidates_pre_existing_snapshots() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut seed = client.begin_optimistic().await.unwrap();
        seed.put(key(b"k"), key(b"v")).await.unwrap();
        seed.commit().await.unwrap();

        let mut stale_snapshot = client.snapshot_at_version(universe.current_ts());
        client.clear().await;
        client.assert_clean();

        let err = stale_snapshot.get(key(b"k")).await.unwrap_err();
        assert!(
            matches!(err, StorageError::Internal(message) if message.contains("fixture reset"))
        );
    }

    #[tokio::test]
    async fn universe_clear_prevents_stale_handles_from_releasing_fresh_locks() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut stale_commit = client.begin().await.unwrap();
        stale_commit
            .lock_keys_nowait([key(b"commit")])
            .await
            .unwrap();
        let stale_commit_id = stale_commit.txn_id;
        universe.clear().await;
        universe.assert_clean();

        let mut fresh_commit = client.begin().await.unwrap();
        fresh_commit
            .lock_keys_nowait([key(b"commit")])
            .await
            .unwrap();
        assert_ne!(stale_commit_id, fresh_commit.txn_id);
        let err = stale_commit.commit().await.unwrap_err();
        assert!(
            matches!(err, StorageError::Internal(message) if message.contains("fixture reset"))
        );
        let mut contender = client.begin().await.unwrap();
        let err = contender
            .lock_keys_nowait([key(b"commit")])
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::LockNotAvailable));
        contender.rollback().await.unwrap();
        fresh_commit.rollback().await.unwrap();

        let mut stale_rollback = client.begin().await.unwrap();
        stale_rollback
            .lock_keys_nowait([key(b"rollback")])
            .await
            .unwrap();
        let stale_rollback_id = stale_rollback.txn_id;
        universe.clear().await;
        universe.assert_clean();

        let mut fresh_rollback = client.begin().await.unwrap();
        fresh_rollback
            .lock_keys_nowait([key(b"rollback")])
            .await
            .unwrap();
        assert_ne!(stale_rollback_id, fresh_rollback.txn_id);
        stale_rollback.rollback().await.unwrap();
        let mut contender = client.begin().await.unwrap();
        let err = contender
            .lock_keys_nowait([key(b"rollback")])
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::LockNotAvailable));
        contender.rollback().await.unwrap();
        fresh_rollback.rollback().await.unwrap();

        let mut stale_drop = client.begin().await.unwrap();
        stale_drop.lock_keys_nowait([key(b"drop")]).await.unwrap();
        let stale_drop_id = stale_drop.txn_id;
        universe.clear().await;
        universe.assert_clean();

        let mut fresh_drop = client.begin().await.unwrap();
        fresh_drop.lock_keys_nowait([key(b"drop")]).await.unwrap();
        assert_ne!(stale_drop_id, fresh_drop.txn_id);
        drop(stale_drop);
        let mut contender = client.begin().await.unwrap();
        let err = contender
            .lock_keys_nowait([key(b"drop")])
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::LockNotAvailable));
        contender.rollback().await.unwrap();
        fresh_drop.rollback().await.unwrap();
        universe.assert_clean();
    }

    #[tokio::test]
    async fn scan_merges_local_write_overlay_in_key_order() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut seed = client.begin_optimistic().await.unwrap();
        seed.put(key(b"a"), key(b"old-a")).await.unwrap();
        seed.put(key(b"b"), key(b"old-b")).await.unwrap();
        seed.put(key(b"d"), key(b"old-d")).await.unwrap();
        seed.commit().await.unwrap();

        let mut txn = client.begin_optimistic().await.unwrap();
        txn.put(key(b"c"), key(b"new-c")).await.unwrap();
        txn.delete(key(b"b")).await.unwrap();
        let pairs = txn
            .scan(StorageRange::from_half_open(key(b"a"), key(b"z")), u32::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(pair_bytes)
            .collect::<Vec<_>>();

        assert_eq!(
            pairs,
            vec![
                (key(b"a"), key(b"old-a")),
                (key(b"c"), key(b"new-c")),
                (key(b"d"), key(b"old-d")),
            ]
        );
        txn.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn for_update_reads_committed_value_not_local_overlay() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut seed = client.begin_optimistic().await.unwrap();
        seed.put(key(b"k"), key(b"committed")).await.unwrap();
        seed.commit().await.unwrap();

        let mut txn = client.begin().await.unwrap();
        txn.put(key(b"k"), key(b"local")).await.unwrap();
        assert_eq!(
            txn.get_for_update(key(b"k"), Some(Duration::from_millis(50)))
                .await
                .unwrap(),
            Some(key(b"committed"))
        );
        assert_eq!(txn.get(key(b"k")).await.unwrap(), Some(key(b"local")));
        txn.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn nowait_conflict_fails_without_blocking_and_self_lock_succeeds() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut holder = client.begin().await.unwrap();
        holder.lock_keys_nowait([key(b"k")]).await.unwrap();
        holder.lock_keys_nowait([key(b"k")]).await.unwrap();

        let mut contender = client.begin().await.unwrap();
        let err = contender.lock_keys_nowait([key(b"k")]).await.unwrap_err();
        assert!(matches!(err, StorageError::LockNotAvailable));
        contender.rollback().await.unwrap();
        holder.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn skip_locked_returns_only_unlocked_keys() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut holder = client.begin().await.unwrap();
        holder.lock_keys_nowait([key(b"k1")]).await.unwrap();

        let mut contender = client.begin().await.unwrap();
        let locked = contender
            .lock_keys_skip_locked([key(b"k1"), key(b"k2"), key(b"k3")], Some(2))
            .await
            .unwrap();
        assert_eq!(locked, vec![key(b"k2"), key(b"k3")]);

        contender.rollback().await.unwrap();
        holder.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn blocking_lock_wait_times_out_and_keeps_partial_batch_atomic() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut holder = client.begin().await.unwrap();
        holder.lock_keys_nowait([key(b"k2")]).await.unwrap();

        let mut contender = client.begin().await.unwrap();
        let err = contender
            .lock_keys([key(b"k1"), key(b"k2")], Some(Duration::from_millis(20)))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::LockTimeout));

        let mut probe = client.begin().await.unwrap();
        probe.lock_keys_nowait([key(b"k1")]).await.unwrap();
        probe.rollback().await.unwrap();
        contender.rollback().await.unwrap();
        holder.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn blocking_lock_wait_wakes_after_release() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut holder = client.begin().await.unwrap();
        holder.lock_keys_nowait([key(b"k")]).await.unwrap();

        let waiter_client = client.clone();
        let waiter = tokio::spawn(async move {
            let mut txn = waiter_client.begin().await.unwrap();
            txn.lock_keys([key(b"k")], Some(Duration::from_secs(1)))
                .await
                .unwrap();
            txn.rollback().await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        holder.rollback().await.unwrap();
        waiter.await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn drop_releases_held_locks() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");
        {
            let mut holder = client.begin().await.unwrap();
            holder.lock_keys_nowait([key(b"k")]).await.unwrap();
        }

        let mut contender = client.begin().await.unwrap();
        contender.lock_keys_nowait([key(b"k")]).await.unwrap();
        contender.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn locks_are_keyspace_scoped() {
        let universe = Arc::new(MemoryUniverse::new());
        let left = universe.client_for_keyspace("left");
        let right = universe.client_for_keyspace("right");

        let mut left_txn = left.begin().await.unwrap();
        left_txn.lock_keys_nowait([key(b"k")]).await.unwrap();

        let mut right_txn = right.begin().await.unwrap();
        right_txn.lock_keys_nowait([key(b"k")]).await.unwrap();

        right_txn.rollback().await.unwrap();
        left_txn.rollback().await.unwrap();
        universe.assert_clean();
    }

    #[tokio::test]
    async fn optimistic_commit_fails_while_other_txn_holds_lock() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut holder = client.begin().await.unwrap();
        holder.lock_keys_nowait([key(b"k")]).await.unwrap();

        let mut writer = client.begin_optimistic().await.unwrap();
        writer.put(key(b"k"), key(b"optimistic")).await.unwrap();
        let err = writer.commit().await.unwrap_err();
        assert!(matches!(err, StorageError::LockConflict));

        holder.rollback().await.unwrap();
        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k")).await.unwrap(), None);
        reader.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn pessimistic_txn_started_before_newer_commit_can_update_after_locking() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut old_txn = client.begin().await.unwrap();

        let mut writer = client.begin_optimistic().await.unwrap();
        writer.put(key(b"k"), key(b"newer")).await.unwrap();
        writer.commit().await.unwrap();

        assert_eq!(
            old_txn
                .get_for_update(key(b"k"), Some(Duration::from_millis(50)))
                .await
                .unwrap(),
            Some(key(b"newer"))
        );
        old_txn.put(key(b"k"), key(b"pessimistic")).await.unwrap();
        old_txn.commit().await.unwrap();

        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(
            reader.get(key(b"k")).await.unwrap(),
            Some(key(b"pessimistic"))
        );
        reader.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn optimistic_conflict_allows_only_first_overlapping_commit() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut older = client.begin_optimistic().await.unwrap();
        let mut newer = client.begin_optimistic().await.unwrap();
        older.put(key(b"k"), key(b"older")).await.unwrap();
        newer.put(key(b"k"), key(b"newer")).await.unwrap();

        newer.commit().await.unwrap();
        let err = older.commit().await.unwrap_err();
        assert!(matches!(
            err,
            StorageError::WriteConflict {
                reason: WriteConflictReason::Optimistic
            }
        ));

        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k")).await.unwrap(), Some(key(b"newer")));
        reader.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn keyspaces_isolate_same_key_bytes() {
        let universe = Arc::new(MemoryUniverse::new());
        let left = universe.client_for_keyspace("left");
        let right = universe.client_for_keyspace("right");

        let mut left_txn = left.begin_optimistic().await.unwrap();
        left_txn.put(key(b"k"), key(b"left")).await.unwrap();
        left_txn.commit().await.unwrap();

        let mut right_txn = right.begin_optimistic().await.unwrap();
        assert_eq!(right_txn.get(key(b"k")).await.unwrap(), None);
        right_txn.put(key(b"k"), key(b"right")).await.unwrap();
        right_txn.commit().await.unwrap();

        let mut left_reader = left.begin_optimistic().await.unwrap();
        let mut right_reader = right.begin_optimistic().await.unwrap();
        assert_eq!(
            left_reader.get(key(b"k")).await.unwrap(),
            Some(key(b"left"))
        );
        assert_eq!(
            right_reader.get(key(b"k")).await.unwrap(),
            Some(key(b"right"))
        );
        left_reader.rollback().await.unwrap();
        right_reader.rollback().await.unwrap();
        universe.assert_clean();
    }

    #[tokio::test]
    async fn drop_releases_active_transaction_entry() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");
        {
            let mut txn = client.begin_optimistic().await.unwrap();
            txn.put(key(b"k"), key(b"v")).await.unwrap();
        }
        client.assert_clean();

        let mut reader = client.begin_optimistic().await.unwrap();
        assert_eq!(reader.get(key(b"k")).await.unwrap(), None);
        reader.rollback().await.unwrap();
        client.assert_clean();
    }

    #[tokio::test]
    async fn snapshot_does_not_observe_later_commit() {
        let universe = Arc::new(MemoryUniverse::new());
        let client = universe.client_for_keyspace("tenant-a");

        let mut seed = client.begin_optimistic().await.unwrap();
        seed.put(key(b"k"), key(b"old")).await.unwrap();
        seed.commit().await.unwrap();

        let read_ts = universe.current_ts();
        let mut snapshot = client.snapshot_at_version(read_ts);

        let mut writer = client.begin_optimistic().await.unwrap();
        writer.put(key(b"k"), key(b"new")).await.unwrap();
        writer.commit().await.unwrap();

        assert_eq!(snapshot.get(key(b"k")).await.unwrap(), Some(key(b"old")));
        client.assert_clean();
    }

    fn run_async<F>(future: F)
    where
        F: Future<Output = ()>,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future);
    }

    fn small_key() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 1..8)
    }

    fn small_value() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..16)
    }

    proptest! {
        #[test]
        fn rollback_property_leaves_no_committed_change(
            ops in proptest::collection::vec((small_key(), small_value()), 1..32)
        ) {
            run_async(async move {
                let universe = Arc::new(MemoryUniverse::new());
                let client = universe.client_for_keyspace("tenant-a");
                let mut txn = client.begin_optimistic().await.unwrap();
                for (key, value) in &ops {
                    txn.put(key.clone(), value.clone()).await.unwrap();
                }
                txn.rollback().await.unwrap();

                let mut reader = client.begin_optimistic().await.unwrap();
                for (key, _) in &ops {
                    assert_eq!(reader.get(key.clone()).await.unwrap(), None);
                }
                reader.rollback().await.unwrap();
                client.assert_clean();
            });
        }

        #[test]
        fn overlapping_optimistic_writes_allow_at_most_one_successful_commit(
            key in small_key(),
            first_value in small_value(),
            second_value in small_value(),
            second_commits_first in any::<bool>(),
        ) {
            run_async(async move {
                let universe = Arc::new(MemoryUniverse::new());
                let client = universe.client_for_keyspace("tenant-a");
                let mut first = client.begin_optimistic().await.unwrap();
                let mut second = client.begin_optimistic().await.unwrap();
                first.put(key.clone(), first_value.clone()).await.unwrap();
                second.put(key.clone(), second_value.clone()).await.unwrap();

                let (first_result, second_result) = if second_commits_first {
                    let second_result = second.commit().await;
                    let first_result = first.commit().await;
                    (first_result, second_result)
                } else {
                    let first_result = first.commit().await;
                    let second_result = second.commit().await;
                    (first_result, second_result)
                };

                let successes = first_result.is_ok() as u8 + second_result.is_ok() as u8;
                assert_eq!(successes, 1);
                let mut reader = client.begin_optimistic().await.unwrap();
                let visible = reader.get(key).await.unwrap();
                let expected = if first_result.is_ok() { first_value } else { second_value };
                assert_eq!(visible, Some(expected));
                reader.rollback().await.unwrap();
                client.assert_clean();
            });
        }

        #[test]
        fn snapshot_property_never_sees_commit_after_read_ts(
            key in small_key(),
            before in small_value(),
            after in small_value(),
        ) {
            run_async(async move {
                let universe = Arc::new(MemoryUniverse::new());
                let client = universe.client_for_keyspace("tenant-a");
                let mut seed = client.begin_optimistic().await.unwrap();
                seed.put(key.clone(), before.clone()).await.unwrap();
                seed.commit().await.unwrap();

                let read_ts = universe.current_ts();
                let mut snapshot = client.snapshot_at_version(read_ts);

                let mut writer = client.begin_optimistic().await.unwrap();
                writer.put(key.clone(), after).await.unwrap();
                writer.commit().await.unwrap();

                assert_eq!(snapshot.get(key).await.unwrap(), Some(before));
                client.assert_clean();
            });
        }
    }
}
