// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use crate::sql::rls::cache::RlsPolicyCache;
use crate::sql::stats::TableStatsCache;
use crate::sql::triggers::TriggerBodyCache;
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use parking_lot::Mutex as StdMutex;
use smallvec::SmallVec;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tracing::{debug, info, warn};

/// Index of idle tenants ordered by `(idle_at_epoch_ms, keyspace)`.
///
/// Used by `evict_idle()` to find eviction candidates in O(k) time where k
/// is the number of candidates due for eviction, rather than scanning all tenants.
/// Protected by `parking_lot::Mutex` (not tokio) so it can be updated from
/// `TenantHandle::Drop` without requiring an async runtime.
type IdleIndex = StdMutex<BTreeSet<(u64, String)>>;

/// How long an idle tenant (zero connections) stays cached before eviction.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the reaper scans for idle tenants.
const DEFAULT_REAPER_INTERVAL: Duration = Duration::from_secs(30);
const TENANT_INVENTORY_REPAIR_RETRY_DELAY: Duration = Duration::from_secs(60);
const TENANT_INVENTORY_REPAIR_MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
const TENANT_INVENTORY_REPAIR_MAX_ATTEMPTS: u32 = 10;

#[derive(Clone, Copy, Default)]
struct TenantInventoryRepairState {
    in_flight: bool,
    retry_after_ms: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InventoryRepairSpawnMode {
    Initial,
    RetryOnly,
}

struct TenantInventoryRepairTracker {
    states: StdMutex<HashMap<String, TenantInventoryRepairState>>,
}

impl TenantInventoryRepairTracker {
    fn new() -> Self {
        Self {
            states: StdMutex::new(HashMap::new()),
        }
    }

    fn begin(
        self: &Arc<Self>,
        keyspace: &str,
        mode: InventoryRepairSpawnMode,
        now_ms: u64,
    ) -> Option<TenantInventoryRepairLease> {
        let mut states = self.states.lock();
        match states.get_mut(keyspace) {
            Some(state) => {
                if state.in_flight {
                    debug!(keyspace, "Tenant database inventory repair already running");
                    return None;
                }
                if state.retry_after_ms > now_ms {
                    debug!(
                        keyspace,
                        retry_after_ms = state.retry_after_ms,
                        "Tenant database inventory repair retry cooling down"
                    );
                    return None;
                }
                state.in_flight = true;
                state.retry_after_ms = 0;
            }
            None if mode == InventoryRepairSpawnMode::RetryOnly => return None,
            None => {
                states.insert(
                    keyspace.to_string(),
                    TenantInventoryRepairState {
                        in_flight: true,
                        retry_after_ms: 0,
                    },
                );
            }
        }

        Some(TenantInventoryRepairLease {
            tracker: self.clone(),
            keyspace: keyspace.to_string(),
            completed: false,
        })
    }

    fn complete(&self, keyspace: &str) {
        self.states.lock().remove(keyspace);
    }

    fn schedule_retry(&self, keyspace: &str, retry_after_ms: u64) {
        let mut states = self.states.lock();
        let state = states.entry(keyspace.to_string()).or_default();
        state.in_flight = false;
        state.retry_after_ms = retry_after_ms;
    }

    #[cfg(test)]
    fn state_for(&self, keyspace: &str) -> Option<TenantInventoryRepairState> {
        self.states.lock().get(keyspace).copied()
    }
}

impl Default for TenantInventoryRepairTracker {
    fn default() -> Self {
        Self::new()
    }
}

struct TenantInventoryRepairLease {
    tracker: Arc<TenantInventoryRepairTracker>,
    keyspace: String,
    completed: bool,
}

impl TenantInventoryRepairLease {
    fn complete(&mut self) {
        self.tracker.complete(&self.keyspace);
        self.completed = true;
    }
}

impl Drop for TenantInventoryRepairLease {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.tracker.schedule_retry(
            &self.keyspace,
            tenant_inventory_repair_next_retry_after_ms(),
        );
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn spawn_tenant_database_inventory_repair(
    system_store: Arc<TikvStore>,
    tenant_store: Arc<TikvStore>,
    tracker: Arc<TenantInventoryRepairTracker>,
    keyspace: String,
    mode: InventoryRepairSpawnMode,
) {
    let Some(lease) = tracker.begin(&keyspace, mode, now_epoch_ms()) else {
        return;
    };

    tokio::spawn(async move {
        let mut lease = lease;
        for attempt in 1..=TENANT_INVENTORY_REPAIR_MAX_ATTEMPTS {
            match crate::worker::register_database_inventory(
                &system_store,
                &tenant_store,
                &keyspace,
            )
            .await
            {
                Ok(registered) => {
                    info!(
                        keyspace = %keyspace,
                        registered,
                        attempt,
                        "Tenant database inventory repair completed"
                    );
                    lease.complete();
                    return;
                }
                Err(e) if attempt < TENANT_INVENTORY_REPAIR_MAX_ATTEMPTS => {
                    let retry_delay = tenant_inventory_repair_retry_delay(attempt);
                    warn!(
                        keyspace = %keyspace,
                        attempt,
                        max_attempts = TENANT_INVENTORY_REPAIR_MAX_ATTEMPTS,
                        retry_delay_seconds = retry_delay.as_secs(),
                        error = %e,
                        "Tenant database inventory repair failed; retrying"
                    );
                    tokio::time::sleep(retry_delay).await;
                }
                Err(e) => {
                    warn!(
                        keyspace = %keyspace,
                        attempt,
                        max_attempts = TENANT_INVENTORY_REPAIR_MAX_ATTEMPTS,
                        error = %e,
                        "Tenant database inventory repair failed; giving up after capped retries"
                    );
                    return;
                }
            }
        }
    });
}

fn tenant_inventory_repair_retry_delay(attempt: u32) -> Duration {
    let multiplier = 1_u32 << attempt.saturating_sub(1).min(3);
    TENANT_INVENTORY_REPAIR_RETRY_DELAY
        .saturating_mul(multiplier)
        .min(TENANT_INVENTORY_REPAIR_MAX_RETRY_DELAY)
}

fn tenant_inventory_repair_next_retry_after_ms() -> u64 {
    now_epoch_ms().saturating_add(TENANT_INVENTORY_REPAIR_MAX_RETRY_DELAY.as_millis() as u64)
}

/// Read the per-tenant QPS limit from environment once. 0 = disabled.
fn tenant_qps_limit() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("DB9_TENANT_QPS_LIMIT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    })
}

/// Read the per-tenant aggregate memory quota from environment once.
/// `0` means unlimited.  Default is 1 GiB to prevent a single tenant from
/// OOM-killing the process.  Set `DB9_TENANT_MEMORY_QUOTA_BYTES=0` to disable.
const DEFAULT_TENANT_MEMORY_QUOTA_BYTES: usize = 1024 * 1024 * 1024; // 1 GiB

fn tenant_memory_quota_bytes() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let quota = std::env::var("DB9_TENANT_MEMORY_QUOTA_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_TENANT_MEMORY_QUOTA_BYTES);
        if quota == 0 {
            // The statement-memory accounting (collect_all, hash aggregate,
            // cop buffers, …) all charge against this quota; a zero quota makes
            // every charge a silent no-op, so no precision prevents a pod OOM.
            // This was the #2555 operational root cause — the gate the ledger
            // feeds was disabled. Surface it loudly rather than fail closed
            // (some single-tenant/benchmark deployments intend unlimited).
            tracing::warn!(
                "DB9_TENANT_MEMORY_QUOTA_BYTES=0: per-tenant memory quota DISABLED; \
                 statement memory accounting will NOT prevent pod OOM (see #2555). \
                 Set a non-zero quota (e.g. pod_budget / max_concurrent_tenants) in production."
            );
        }
        quota
    })
}

/// Per-tenant aggregate memory accounting domain.
///
/// All interactive/worker statements for the same keyspace share one
/// `TenantMemoryAccountant` so quota is enforced across concurrent sessions.
///
/// # Memory-accounting contract (the closure condition)
///
/// `charged_bytes` is a **conservative lower bound** on the statement's true
/// live heap, within a fixed factor `C >= 1`: at every instant
/// `charged_bytes >= true_live_heap / C`. Enforcement runs on the exact
/// `charged_bytes` total; the deployment carries `C` in the quota by setting
/// `quota_bytes = pod_budget / (C * max_concurrent_tenants)`. Byte-exactness
/// against the allocator is therefore a **non-goal** (see #2559).
///
/// **In-contract** (absorbed by `C`, NOT a finding): slot-pointer capacity
/// (`size_of::<T>()` per element), `HashMap`/`HashSet` bucket-array index
/// bytes, `Vec`/`String` len-vs-capacity slack, struct/control-byte overhead,
/// one in-flight transient (or release-timing window) bounded by a single
/// row / group / page, a **leaf-source materialization bounded by a fixed cap
/// independent of input size** (one stored value ≤ the TiKV value limit for
/// `UNNEST`/`JSON_*`; a configured row cap such as `generate_series`'s
/// `DB9_MAX_GENERATE_SERIES_ROWS`, which errors when exceeded), and **an
/// operator output/intermediate that is a bounded constant multiple of an
/// already-charged base** — e.g. `WindowOperator.result_rows` is ≤ one composed
/// row per `collect_all`-charged input row, so its peak is ≤ 2× the charged
/// input and cannot independently exceed `2·quota`. None of these scale as an
/// unbounded fraction of input, so none are findings.
///
/// **Blocker** (the only actionable under-count): a site that charges `O(1)`
/// while retaining `O(N)` bytes *not* bounded by a per-element charge already
/// taken — uncharged growth as an unbounded fraction of input. The coverage
/// obligation is **per operator that materializes input-proportional memory**
/// — a finite, `grep`-checkable list, NOT per allocation site. Three classes,
/// each of which must stream or route through a charged buffer
/// ([`crate::sql::operators::charged_rows::ChargedBuf`]):
///   1. child-row materializers (Sort, HashJoin, NLJ inner, SetOperation,
///      Window, CTE) — via `collect_all` (which charges `estimate_row_size`
///      per row) or a `ChargedRowBuffer`; any *additional* dedup keyset built
///      on top must itself be charged (`estimate_key_size` per inserted key —
///      `collect_all` charging the row payloads does NOT cover a second keyset).
///   2. scan PK/candidate materializers (Index/Range/InList scans via
///      `IndexScanBase.pk_queue`, `GinScan` pk_queue + posting-list candidate
///      set) — the O(matches) PK set is produced uncharged by storage and held
///      across `next()`; it must be admitted into a `ChargedPkBuffer` (charged
///      per PK on push, released on `take_next`/`reset`), including pre-stage
///      transients (`InListScan` candidates, GIN byte-sets).
///   3. leaf-source materializers (table functions: `UNNEST`/`JSON_*`) — route
///      the produced rows through a `ChargedRowBuffer` when held resident.
///
/// The #2555 shape was class 1 (`collect_all` into one `Vec`); the #2612 gate
/// review added class 2.
///
/// Scope boundary: charging is applied at the **operator boundary**, which
/// converts over-quota SELECT scan→aggregate→result queries into `53200`. Two
/// deeper/wider materializers are the NAMED SUCCESSOR EPIC, not this change
/// (both pre-existing, neither a regression):
///   - storage index-scan producers (`scan_index*` in
///     `storage/tikv_store/indexes.rs`) return the whole match-set `Vec` before
///     the operator's `ChargedPkBuffer` charges it, so a match set exceeding pod
///     memory can still OOM during storage materialization — the complete fix is
///     a streaming/charged storage cursor like `TableScanOperator`'s page cursor
///     (or charge per PK inside the `scan_range_in_pages` visit closure);
///   - the DML write path (`DELETE`/`UPDATE` full-table scan in `dml_analyzed`)
///     is a separate write path, not the #2555 SELECT path.
///
/// Continuing to fold these into one PR is the unbounded-scope patch-loop that
/// kept #2610/#2612 from converging; the closure condition is per the operator
/// boundary above, verified by the reconciliation test, not an open-ended sweep.
///
/// **Review stop-rule:** a finding is actionable iff it shows an
/// unbounded-fraction-of-input under-count, or a charge path with no scope
/// established. Cite the whole-path reconciliation test, not a byte count.
/// Bounded constant-factor slack is in-contract and is not a finding.
///
/// **Enforcement caveat (the #2555 operational root cause):** the quota must
/// be explicitly configured and non-zero in production.
/// `DB9_TENANT_MEMORY_QUOTA_BYTES=0` disables the gate entirely, so no
/// accounting precision can prevent a pod OOM. `quota_bytes == 0` is logged
/// once at startup (see [`tenant_memory_quota_bytes`]).
#[derive(Clone, Debug)]
pub struct TenantMemoryAccountant {
    #[allow(dead_code)] // observability/test helper
    keyspace: Arc<str>,
    used_bytes: Arc<AtomicUsize>,
    quota_bytes: usize,
}

impl TenantMemoryAccountant {
    pub(crate) fn new_with_quota(keyspace: String, quota_bytes: usize) -> Self {
        Self {
            keyspace: Arc::from(keyspace),
            used_bytes: Arc::new(AtomicUsize::new(0)),
            quota_bytes,
        }
    }

    /// Build an unlimited accountant (used by non-pooled startup paths).
    pub fn unlimited(keyspace: String) -> Self {
        Self::new_with_quota(keyspace, 0)
    }

    #[allow(dead_code)] // test helper
    pub fn quota_bytes(&self) -> usize {
        self.quota_bytes
    }

    #[allow(dead_code)] // test helper
    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Relaxed)
    }

    #[allow(dead_code)] // test helper
    pub fn keyspace(&self) -> &str {
        &self.keyspace
    }

    /// Clone the keyspace handle for embedding in a statement scope.
    pub fn keyspace_arc(&self) -> Arc<str> {
        self.keyspace.clone()
    }

    fn try_charge(
        &self,
        component: &str,
        bytes: usize,
    ) -> std::result::Result<(), crate::sql::error::SqlError> {
        if bytes == 0 {
            return Ok(());
        }

        loop {
            let used = self.used_bytes.load(Ordering::Relaxed);
            let Some(new_used) = used.checked_add(bytes) else {
                return Err(crate::sql::error::SqlError::TenantMemoryQuotaExceeded {
                    component: component.to_string(),
                    requested_bytes: bytes,
                    used_bytes: used,
                    quota_bytes: self.quota_bytes,
                });
            };

            if self.quota_bytes > 0 && new_used > self.quota_bytes {
                return Err(crate::sql::error::SqlError::TenantMemoryQuotaExceeded {
                    component: component.to_string(),
                    requested_bytes: bytes,
                    used_bytes: used,
                    quota_bytes: self.quota_bytes,
                });
            }

            if self
                .used_bytes
                .compare_exchange_weak(used, new_used, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }

        loop {
            let used = self.used_bytes.load(Ordering::Relaxed);
            let new_used = used.saturating_sub(bytes);
            if self
                .used_bytes
                .compare_exchange_weak(used, new_used, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Build a zero-byte reservation owner that can grow over statement/runtime
    /// lifetime and auto-release on drop.
    pub fn reservation(&self) -> TenantMemoryReservation {
        TenantMemoryReservation {
            accountant: self.clone(),
            charged_bytes: 0,
            peak_bytes: 0,
            peak_by_component_grow: SmallVec::new(),
        }
    }
}

/// Inline capacity for per-component peak tracking. Queries typically touch ≤ 8
/// distinct component labels, so this stays heap-free in the common case.
const PEAK_COMPONENT_CAP: usize = 8;

/// RAII reservation over tenant aggregate memory budget.
///
/// The reservation can grow/shrink over time and always releases any remaining
/// bytes on drop.
#[derive(Debug)]
pub struct TenantMemoryReservation {
    accountant: TenantMemoryAccountant,
    charged_bytes: usize,
    /// High-water mark of `charged_bytes` over this reservation's life. Never
    /// decreases on `shrink`; reset only by `split` or drop.
    peak_bytes: usize,
    /// Best-effort per-component attribution for the `expensive_query` log's
    /// `component_grow_top` field — a forensic "which operator likely dominated"
    /// hint, NOT an exact ledger.
    ///
    /// It is cumulative *grows*, not live bytes: `shrink` carries no component
    /// label (memory ownership crosses operators — e.g. `HashAggregate` releases
    /// rows that `collect_all` charged), so a component that grew 200 MB then
    /// shrank 180 MB still reads 200 MB here, and only the dominant labels are
    /// reliable. This is a deliberate trade-off, not a bug: enforcement runs on
    /// the exact `charged_bytes` total, and exact per-component accounting would
    /// require per-owner reservations that only pay off once db9 has disk spill.
    /// See the architecture decision in issue #2559.
    peak_by_component_grow: SmallVec<[(&'static str, usize); PEAK_COMPONENT_CAP]>,
}

impl TenantMemoryReservation {
    #[allow(dead_code)] // test helper
    pub fn charged_bytes(&self) -> usize {
        self.charged_bytes
    }

    /// Charge-only grow with no peak tracking. Kept as a thin helper for tests
    /// and callers that don't need the high-water mark; the hot path
    /// (`try_grow_statement_memory_scope`) uses [`grow_with_peak`].
    #[allow(dead_code)] // test helper
    pub fn grow(
        &mut self,
        component: &str,
        delta: usize,
    ) -> std::result::Result<(), crate::sql::error::SqlError> {
        if delta == 0 {
            return Ok(());
        }
        self.accountant.try_charge(component, delta)?;
        self.charged_bytes = self.charged_bytes.saturating_add(delta);
        Ok(())
    }

    /// Grow the reservation by `delta` and report the peak to use for the
    /// expensive-query threshold check.
    ///
    /// - On **success**: `charged_bytes` and `peak_bytes` advance, the grow is
    ///   recorded in `peak_by_component_grow`, and the returned peak is the new
    ///   physical `peak_bytes`.
    /// - On **quota-exceed**: `charged_bytes`, `peak_bytes`, and the component map
    ///   are all left untouched — so the physical peak (used by the slow-query
    ///   `peak_mb`) and the component breakdown never count un-charged bytes — but
    ///   the returned peak is `max(peak_bytes, attempted)` so the caller still
    ///   fires the log for the grow that hit the quota wall (edge case J).
    pub fn grow_with_peak(
        &mut self,
        component: &'static str,
        delta: usize,
    ) -> (std::result::Result<(), crate::sql::error::SqlError>, usize) {
        if delta == 0 {
            return (Ok(()), self.peak_bytes);
        }
        let attempted = self.charged_bytes.saturating_add(delta);
        match self.accountant.try_charge(component, delta) {
            Ok(()) => {
                self.charged_bytes = attempted;
                self.peak_bytes = self.peak_bytes.max(attempted);
                bump_component(&mut self.peak_by_component_grow, component, delta);
                (Ok(()), self.peak_bytes)
            }
            Err(e) => {
                // Charge rejected: leave charged_bytes, peak_bytes, and the
                // component map untouched, so the physical peak (used by the
                // slow-query `peak_mb`) and the component breakdown never count
                // bytes that were not actually charged. Still report the
                // *attempted* total so the caller's threshold check fires for the
                // grow that hit the quota wall (edge case J) — that statement is
                // exactly the one worth logging.
                (Err(e), self.peak_bytes.max(attempted))
            }
        }
    }

    /// Peak `charged_bytes` reached over this reservation's life.
    pub fn peak_bytes(&self) -> usize {
        self.peak_bytes
    }

    /// Best-effort top-`n` components by cumulative grow, formatted
    /// `label=NMB;label=NMB`. A forensic hint, not an exact breakdown — see
    /// [`TenantMemoryReservation::peak_by_component_grow`] and issue #2559.
    pub fn component_grow_top_n(&self, n: usize) -> String {
        let mut v: Vec<_> = self.peak_by_component_grow.iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        v.into_iter()
            .take(n)
            .map(|entry| format!("{}={}MB", entry.0, entry.1 / (1024 * 1024)))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Reset per-attempt tracking at the start of a statement retry: clear the
    /// component map and re-baseline `peak_bytes` to the current live
    /// `charged_bytes`.
    ///
    /// The autocommit retry loops re-run the whole operator tree within ONE
    /// statement scope. Without this, `peak_by_component_grow` accumulates across
    /// attempts and `component_grow_top` is inflated by the retry count (and
    /// contradicts `peak_mb`). Does NOT touch the one-shot log latch — the log
    /// must still fire at most once per statement, not once per attempt.
    pub fn reset_attempt_tracking(&mut self) {
        self.peak_bytes = self.charged_bytes;
        self.peak_by_component_grow.clear();
    }

    #[allow(dead_code)] // test helper
    pub fn shrink(&mut self, delta: usize) {
        let to_release = delta.min(self.charged_bytes);
        if to_release == 0 {
            return;
        }
        self.charged_bytes -= to_release;
        self.accountant.release(to_release);
    }

    /// Split `bytes` from this reservation into a new owner.
    ///
    /// Used when ownership transfers from statement scope to suspended portal
    /// buffer lifetime.
    pub fn split(&mut self, bytes: usize) -> Option<TenantMemoryReservation> {
        if bytes > self.charged_bytes {
            return None;
        }
        self.charged_bytes -= bytes;
        Some(TenantMemoryReservation {
            accountant: self.accountant.clone(),
            charged_bytes: bytes,
            // Peak is a per-statement high-water mark, not per-portal-lifetime:
            // the split-off owner starts fresh while this reservation keeps its
            // own peak.
            peak_bytes: 0,
            peak_by_component_grow: SmallVec::new(),
        })
    }
}

impl Drop for TenantMemoryReservation {
    fn drop(&mut self) {
        if self.charged_bytes > 0 {
            self.accountant.release(self.charged_bytes);
            self.charged_bytes = 0;
        }
    }
}

struct StatementMemoryScope {
    /// Lock-ordering rule: when the log path needs both the reservation peak and
    /// `sql`/`start_ts`, lock `reservation` first, release it, *then* lock
    /// `sql`/`start_ts`. The grow path only ever locks `reservation`. Never hold
    /// two of these locks at once.
    reservation: std::sync::Mutex<TenantMemoryReservation>,
    /// Normalized + redacted SQL of the running statement; set by the dispatch
    /// layer after scope entry.
    sql: std::sync::Mutex<Option<Arc<str>>>,
    conn_id: i64,
    keyspace: Arc<str>,
    /// TiKV txn `start_ts`; set when the txn begins (see `Session::begin` and the
    /// dispatch layer).
    start_ts: std::sync::Mutex<Option<u64>>,
    started_at: std::time::Instant,
    /// One-shot latch: the first threshold crossing logs, the rest are silent.
    expensive_logged: AtomicBool,
    /// Peak threshold (bytes) read once at scope creation; `0` disables the log.
    threshold_bytes: usize,
}

impl StatementMemoryScope {
    fn new(accountant: TenantMemoryAccountant, conn_id: i64, threshold_bytes: usize) -> Self {
        let keyspace = accountant.keyspace_arc();
        Self {
            reservation: std::sync::Mutex::new(accountant.reservation()),
            sql: std::sync::Mutex::new(None),
            conn_id,
            keyspace,
            start_ts: std::sync::Mutex::new(None),
            started_at: std::time::Instant::now(),
            expensive_logged: AtomicBool::new(false),
            threshold_bytes,
        }
    }
}

tokio::task_local! {
    static STATEMENT_MEMORY_SCOPE: Arc<StatementMemoryScope>;
}

/// Run a future under a statement-scoped memory reservation owner.
///
/// The scope starts with 0 charged bytes and can grow via
/// `try_grow_statement_memory_scope()`. Any remaining bytes are auto-released
/// when the scope future completes.
pub async fn run_with_statement_memory_scope<Fut>(
    accountant: Option<TenantMemoryAccountant>,
    conn_id: i64,
    fut: Fut,
) -> Fut::Output
where
    Fut: std::future::Future,
{
    if let Some(accountant) = accountant {
        let scope = Arc::new(StatementMemoryScope::new(
            accountant,
            conn_id,
            crate::observability::expensive_mem_threshold_bytes(),
        ));
        STATEMENT_MEMORY_SCOPE.scope(scope, fut).await
    } else {
        fut.await
    }
}

/// Charge bytes against the current statement scope, tracking the per-statement
/// peak and firing the `expensive_query` log the first time the peak crosses the
/// configured threshold.
///
/// Returns `Ok(())` when no statement scope is active (e.g. tests calling
/// operator helpers directly).
///
/// `component` is `&'static str` because every call site passes a string literal
/// or a `const &str`; the label is stored verbatim for `component_grow_top`.
///
/// Spawn contract: `STATEMENT_MEMORY_SCOPE` is a `tokio::task_local!`, so it does
/// NOT propagate across `tokio::spawn` / `spawn_blocking`. Any future code that
/// spawns a task and then allocates through this function must re-enter
/// `run_with_statement_memory_scope` in the spawned task, or its memory will not
/// be accounted to the originating statement.
pub fn try_grow_statement_memory_scope(
    component: &'static str,
    bytes: usize,
) -> std::result::Result<(), crate::sql::error::SqlError> {
    if bytes == 0 {
        return Ok(());
    }
    let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) else {
        return Ok(());
    };

    // Single lock acquisition: grow + peak read + (conditional) component
    // snapshot. The no-cross fast path adds only a threshold compare and one
    // relaxed atomic load over the lock that `grow` already required.
    let (grow_result, peak, component_top_for_log) = {
        let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
        let (res, peak) = guard.grow_with_peak(component, bytes);
        let snapshot = if scope.threshold_bytes > 0
            && peak >= scope.threshold_bytes
            && !scope.expensive_logged.load(Ordering::Relaxed)
        {
            Some(guard.component_grow_top_n(3))
        } else {
            None
        };
        (res, peak, snapshot)
    };

    if let Some(top) = component_top_for_log {
        // Latch: only the first crosser emits.
        if !scope.expensive_logged.swap(true, Ordering::AcqRel) {
            emit_expensive_query_log_with_top(&scope, peak, &top);
        }
    }

    grow_result
}

/// Attach the SQL of the current statement to its memory scope (normalized +
/// redacted). Called from the dispatch layer once the SQL is known. No-op when
/// no scope is active or the SQL normalizes to empty.
///
/// Self-healing: if the peak already crossed the threshold before the SQL was
/// known, the log is emitted now (the earlier crossing reset the latch).
pub fn set_current_statement_sql(sql: &str) {
    let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) else {
        return;
    };
    // Hot path: this runs at dispatch for EVERY statement. When the log is
    // disabled, skip normalization (and its env reads) entirely.
    if scope.threshold_bytes == 0 {
        return;
    }
    let normalized = crate::observability::normalize_sql_for_expensive_log(sql);
    if normalized.is_empty() {
        return;
    }
    {
        let mut guard = scope.sql.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::from(normalized));
    }
    maybe_emit_expensive_query_log(&scope);
}

/// Reset per-attempt peak/component tracking on the current statement scope at
/// the start of a statement retry attempt. No-op when no scope is active. See
/// [`TenantMemoryReservation::reset_attempt_tracking`].
pub fn reset_statement_memory_attempt() {
    if let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) {
        let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
        guard.reset_attempt_tracking();
    }
}

/// Publish the TiKV txn `start_ts` onto the current statement scope. No-op when
/// no scope is active. The autocommit simple-query path writes this twice (a `0`
/// placeholder from the dispatch layer, then the real value from
/// `Session::begin`); last-write-wins.
pub fn set_current_statement_start_ts(ts: u64) {
    if let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) {
        let mut guard = scope.start_ts.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(ts);
    }
}

/// Snapshot the current statement scope's peak bytes, if a scope is active.
/// Used to enrich the slow-query log with `peak_mb`.
pub fn peak_bytes_snapshot() -> Option<usize> {
    STATEMENT_MEMORY_SCOPE
        .try_with(|scope| {
            scope
                .reservation
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .peak_bytes()
        })
        .ok()
}

/// Re-check the peak under the reservation lock and emit the `expensive_query`
/// log if it has crossed the threshold and not yet been logged. Shared by the
/// grow path and the `set_current_statement_sql` self-heal.
fn maybe_emit_expensive_query_log(scope: &StatementMemoryScope) {
    if scope.threshold_bytes == 0 || scope.expensive_logged.load(Ordering::Acquire) {
        return;
    }
    let (peak, top) = {
        let g = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
        let peak = g.peak_bytes();
        if peak < scope.threshold_bytes {
            return;
        }
        (peak, g.component_grow_top_n(3))
    };
    if !scope.expensive_logged.swap(true, Ordering::AcqRel) {
        emit_expensive_query_log_with_top(scope, peak, &top);
    }
}

/// Emit one `expensive_query` log line.
///
/// Lock ordering: the caller has already released the `reservation` lock; here
/// we lock only `sql` then `start_ts`, never holding two scope locks at once. If
/// no SQL is attached yet, reset the latch and skip so a later grow or
/// `set_current_statement_sql` can still report.
fn emit_expensive_query_log_with_top(
    scope: &StatementMemoryScope,
    peak_bytes: usize,
    component_grow_top: &str,
) {
    let sql = scope.sql.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let sql_str: &str = sql.as_deref().unwrap_or("");
    if sql_str.is_empty() {
        scope.expensive_logged.store(false, Ordering::Release);
        return;
    }
    let start_ts = scope
        .start_ts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or(0);

    // No `tenant` label: db9 is multi-tenant, so that would be an unbounded
    // Prometheus dimension. The tenant is on the log line below for drill-down.
    metrics::counter!("db9_server_expensive_queries_total").increment(1);
    tracing::warn!(
        target: "expensive_query",
        peak_mb = peak_bytes / (1024 * 1024),
        latency_ms_so_far = scope.started_at.elapsed().as_millis() as u64,
        tenant = %scope.keyspace,
        conn_id = scope.conn_id,
        start_ts = start_ts,
        component_grow_top = %component_grow_top,
        "expensive query (memory): {sql_str}",
    );
}

/// Add `delta` to a component's cumulative grow, appending a new entry for a
/// label not seen before. Linear scan over the live entries.
///
/// The distinct component-label set is small and finite (`&'static str` literals
/// across the operators), so this stays inline for ≤ `PEAK_COMPONENT_CAP` labels
/// and spills to a small, bounded heap `Vec` only for plans that touch more —
/// never unbounded. Appending (rather than folding into the smallest slot) keeps
/// every label's bytes under the *correct* label, including a large component
/// that first appears late in a complex plan.
fn bump_component(
    v: &mut SmallVec<[(&'static str, usize); PEAK_COMPONENT_CAP]>,
    component: &'static str,
    delta: usize,
) {
    for entry in v.iter_mut() {
        if entry.0 == component {
            entry.1 = entry.1.saturating_add(delta);
            return;
        }
    }
    v.push((component, delta));
}

/// Release bytes from the current statement scope.
///
/// No-op when no statement scope is active.
pub fn try_shrink_statement_memory_scope(bytes: usize) {
    if bytes == 0 {
        return;
    }
    if let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) {
        let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
        guard.shrink(bytes);
    }
}

/// Split bytes out of the current statement scope into an independent owner.
///
/// Used to hand off memory ownership from statement lifetime to suspended
/// portal lifetime.
pub fn split_statement_memory_scope(bytes: usize) -> Option<TenantMemoryReservation> {
    if bytes == 0 {
        return None;
    }
    STATEMENT_MEMORY_SCOPE
        .try_with(|scope| {
            let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
            guard.split(bytes)
        })
        .ok()
        .flatten()
}

/// Error returned when a principal's concurrent query limit is reached.
#[derive(Debug)]
pub(crate) struct ConcurrencyRejection {
    pub(crate) limit: u32,
}

/// Per-principal concurrent query limiter.
///
/// Tracks in-flight queries per principal identity (e.g., authenticated username).
/// Uses a `Mutex<HashMap>` to keep per-principal counters. When a principal's
/// counter reaches 0, the entry is removed to avoid unbounded growth.
#[derive(Debug)]
pub(crate) struct PrincipalConcurrencyTracker {
    counters: StdMutex<HashMap<String, u32>>,
    limit: u32,
}

impl PrincipalConcurrencyTracker {
    fn new(limit: u32) -> Self {
        Self {
            counters: StdMutex::new(HashMap::new()),
            limit,
        }
    }

    /// Maximum concurrent queries per principal. 0 = disabled.
    pub(crate) fn limit(&self) -> u32 {
        self.limit
    }

    /// Try to acquire a concurrency slot for the given principal using the
    /// tracker's configured limit.
    pub(crate) fn try_acquire(
        self: &Arc<Self>,
        principal: &str,
    ) -> Result<ConcurrencyGuard, ConcurrencyRejection> {
        self.try_acquire_with_limit(principal, self.limit)
    }

    /// Try to acquire a concurrency slot for the given principal using the
    /// given effective limit (which may be tighter than the tracker's default).
    ///
    /// Returns a [`ConcurrencyGuard`] on success that will automatically
    /// decrement the counter when dropped (RAII pattern).
    pub(crate) fn try_acquire_with_limit(
        self: &Arc<Self>,
        principal: &str,
        effective_limit: u32,
    ) -> Result<ConcurrencyGuard, ConcurrencyRejection> {
        if effective_limit == 0 {
            // Disabled — should not be called, but be safe.
            return Ok(ConcurrencyGuard {
                tracker: Arc::clone(self),
                principal: principal.to_string(),
            });
        }

        let mut map = self.counters.lock();
        let count = map.entry(principal.to_string()).or_insert(0);
        if *count >= effective_limit {
            return Err(ConcurrencyRejection {
                limit: effective_limit,
            });
        }
        *count += 1;
        Ok(ConcurrencyGuard {
            tracker: Arc::clone(self),
            principal: principal.to_string(),
        })
    }

    /// Current in-flight count for a principal (for tests/diagnostics).
    #[cfg(test)]
    pub(crate) fn current_count(&self, principal: &str) -> u32 {
        let map = self.counters.lock();
        map.get(principal).copied().unwrap_or(0)
    }
}

/// RAII guard that decrements the principal's in-flight counter on drop.
#[derive(Debug)]
pub(crate) struct ConcurrencyGuard {
    tracker: Arc<PrincipalConcurrencyTracker>,
    principal: String,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        let mut map = self.tracker.counters.lock();
        if let Some(count) = map.get_mut(&self.principal) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.principal);
            }
        }
    }
}

/// Simple token bucket rate limiter.
///
/// Allows up to `rate` requests per second. Burst capacity defaults to `rate`
/// but can be set independently via `new_with_burst`.
pub(crate) struct TokenBucket {
    state: StdMutex<TokenBucketState>,
    rate: u64,
    burst: u64,
}

struct TokenBucketState {
    tokens: f64,
    last_refill: std::time::Instant,
}

impl TokenBucket {
    fn new(rate: u64) -> Self {
        Self::new_with_burst(rate, rate)
    }

    pub(crate) fn new_with_burst(rate: u64, burst: u64) -> Self {
        Self {
            state: StdMutex::new(TokenBucketState {
                tokens: burst as f64,
                last_refill: std::time::Instant::now(),
            }),
            rate,
            burst,
        }
    }

    /// Try to consume one token. Returns `true` if the request is allowed.
    pub(crate) fn try_acquire(&self) -> bool {
        let mut state = self.state.lock();
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        let refill_rate = self.rate as f64;
        let capacity = self.burst as f64;
        state.tokens = (state.tokens + elapsed * refill_rate).min(capacity);
        state.last_refill = now;
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// The configured rate (queries per second).
    pub(crate) fn rate(&self) -> u64 {
        self.rate
    }

    /// Drain all tokens so the next `try_acquire` returns false.
    #[cfg(test)]
    pub(crate) fn drain(&self) {
        let mut state = self.state.lock();
        state.tokens = 0.0;
        state.last_refill = std::time::Instant::now();
    }
}

/// Budget parameters extracted from JWT connect-token claims.
#[derive(Debug, Clone)]
pub(crate) struct AdmissionBudgetParams {
    pub owner_id: String,
    pub rps: u64,
    pub burst: u64,
}

/// Global registry of per-budget-owner token buckets.
///
/// All sessions sharing the same `budget_owner_id` (= tenant_id) share one
/// token bucket. The registry is process-global so that connect-token sessions
/// across different connections to the same tenant are correctly metered together.
pub(crate) struct AdmissionBudgetRegistry {
    buckets: StdMutex<HashMap<String, Arc<TokenBucket>>>,
}

impl AdmissionBudgetRegistry {
    fn new() -> Self {
        Self {
            buckets: StdMutex::new(HashMap::new()),
        }
    }

    /// Get or create a token bucket for the given budget owner.
    ///
    /// If a bucket already exists for this owner, it is returned as-is (the
    /// first session's budget params win). This is correct because all tokens
    /// for the same tenant carry identical budget claims.
    pub(crate) fn get_or_create(&self, params: &AdmissionBudgetParams) -> Arc<TokenBucket> {
        let mut map = self.buckets.lock();
        map.entry(params.owner_id.clone())
            .or_insert_with(|| Arc::new(TokenBucket::new_with_burst(params.rps, params.burst)))
            .clone()
    }

    /// Remove the admission budget for `owner_id` when the tenant is evicted.
    pub(crate) fn evict(&self, owner_id: &str) {
        self.buckets.lock().remove(owner_id);
    }
}

/// Process-global admission budget registry.
pub(crate) fn admission_budget_registry() -> &'static AdmissionBudgetRegistry {
    static REGISTRY: OnceLock<AdmissionBudgetRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AdmissionBudgetRegistry::new)
}

/// Per-tenant metadata inside the pool.
#[allow(dead_code)] // test: fields accessed by pool lifecycle tests
pub(crate) struct TenantEntry {
    store: Arc<TikvStore>,
    /// Number of active connection-scoped handles (TenantHandle instances).
    active_connections: AtomicU32,
    /// Epoch millis when the last handle was dropped (connections went to zero).
    /// Zero means there are active connections or it was never idle.
    last_idle_at: AtomicU64,
    keyspace: String,
    trigger_cache: Arc<TriggerBodyCache>,
    rls_policy_cache: Arc<RlsPolicyCache>,
    stats_cache: Arc<TableStatsCache>,
    /// Per-tenant QPS rate limiter. `None` when rate limiting is disabled (limit = 0).
    rate_limiter: Option<TokenBucket>,
    /// Shared per-tenant aggregate memory ledger/quota.
    memory_accountant: TenantMemoryAccountant,
    /// Per-principal concurrent query limiter.
    concurrency_tracker: Arc<PrincipalConcurrencyTracker>,
    /// Per-user active connection counts for `rolconnlimit` enforcement.
    user_connections: StdMutex<HashMap<String, u32>>,
    /// Back-reference to the pool-level idle index for Drop-time updates.
    /// `None` only in test entries created outside a pool.
    idle_index: Option<Arc<IdleIndex>>,
}

impl TenantEntry {
    fn new(
        store: Arc<TikvStore>,
        keyspace: String,
        idle_index: Arc<IdleIndex>,
        concurrency_limit: u32,
    ) -> Self {
        let qps_limit = tenant_qps_limit();
        let memory_quota = tenant_memory_quota_bytes();
        Self {
            store,
            active_connections: AtomicU32::new(0),
            last_idle_at: AtomicU64::new(0),
            keyspace: keyspace.clone(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            rls_policy_cache: Arc::new(RlsPolicyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
            rate_limiter: if qps_limit > 0 {
                Some(TokenBucket::new(qps_limit))
            } else {
                None
            },
            concurrency_tracker: Arc::new(PrincipalConcurrencyTracker::new(concurrency_limit)),
            memory_accountant: TenantMemoryAccountant::new_with_quota(keyspace, memory_quota),
            user_connections: StdMutex::new(HashMap::new()),
            idle_index: Some(idle_index),
        }
    }

    /// Try to acquire a per-user connection slot.
    /// `limit < 0` means unlimited (PostgreSQL `rolconnlimit = -1`).
    /// Returns `true` if the slot was acquired, `false` if the limit is reached.
    fn try_acquire_user_slot(&self, username: &str, limit: i32) -> bool {
        let mut map = self.user_connections.lock();
        let count = map.entry(username.to_string()).or_insert(0);
        if limit >= 0 && (*count as i32) >= limit {
            return false;
        }
        *count += 1;
        true
    }

    /// Release a per-user connection slot.
    fn release_user_slot(&self, username: &str) {
        let mut map = self.user_connections.lock();
        if let Some(count) = map.get_mut(username) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(username);
            }
        }
    }

    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn store(&self) -> &Arc<TikvStore> {
        &self.store
    }

    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn active_connections(&self) -> u32 {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Per-user connection count snapshot (for tests).
    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn user_connections_for(&self, username: &str) -> u32 {
        let map = self.user_connections.lock();
        map.get(username).copied().unwrap_or(0)
    }

    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn memory_accountant(&self) -> TenantMemoryAccountant {
        self.memory_accountant.clone()
    }
}

/// RAII handle representing one connection's hold on a tenant's TikvStore.
///
/// When dropped, decrements the tenant's active connection count. If the count
/// reaches zero, the tenant becomes eligible for eviction after the idle timeout.
pub struct TenantHandle {
    entry: Arc<TenantEntry>,
    /// If set, this handle owns a per-user connection slot that will be
    /// released on drop. Clones do NOT inherit user slots.
    user_slot: Option<String>,
}

impl TenantHandle {
    pub fn store(&self) -> &Arc<TikvStore> {
        &self.entry.store
    }

    pub fn trigger_cache(&self) -> &Arc<TriggerBodyCache> {
        &self.entry.trigger_cache
    }

    pub fn rls_policy_cache(&self) -> &Arc<RlsPolicyCache> {
        &self.entry.rls_policy_cache
    }

    pub fn stats_cache(&self) -> &Arc<TableStatsCache> {
        &self.entry.stats_cache
    }

    pub fn rate_limiter(&self) -> Option<&TokenBucket> {
        self.entry.rate_limiter.as_ref()
    }

    pub fn concurrency_tracker(&self) -> &Arc<PrincipalConcurrencyTracker> {
        &self.entry.concurrency_tracker
    }

    pub fn keyspace(&self) -> &str {
        &self.entry.keyspace
    }

    pub fn memory_accountant(&self) -> TenantMemoryAccountant {
        self.entry.memory_accountant.clone()
    }

    /// Bind this handle to a user and acquire a per-user connection slot.
    /// Returns an error message if the user's `connection_limit` is exceeded.
    /// `connection_limit < 0` means unlimited (PostgreSQL `rolconnlimit = -1`).
    pub fn try_bind_user(&mut self, username: String, connection_limit: i32) -> Result<(), String> {
        if self.user_slot.is_some() {
            return Ok(()); // Already bound.
        }
        if self
            .entry
            .try_acquire_user_slot(&username, connection_limit)
        {
            self.user_slot = Some(username);
            Ok(())
        } else {
            Err(format!("too many connections for role \"{}\"", username))
        }
    }

    /// Create a TenantHandle with a rate limiter set to the given QPS limit.
    /// The bucket starts full (all tokens available).
    #[cfg(test)]
    pub(crate) fn new_with_rate_limit(qps_limit: u64) -> Self {
        Self::new_with_limits(qps_limit, 0)
    }

    /// Minimal TenantHandle for unit tests that don't need pool/store access.
    #[cfg(test)]
    pub(crate) fn dummy_for_test() -> Self {
        Self::new_with_limits(0, 0)
    }

    /// Create a TenantHandle with explicit QPS+memory limits.
    #[cfg(test)]
    pub(crate) fn new_with_limits(qps_limit: u64, memory_quota_bytes: usize) -> Self {
        Self::new_with_all_limits(qps_limit, memory_quota_bytes, 0)
    }

    /// Create a TenantHandle with explicit QPS, memory, and concurrency limits.
    #[cfg(test)]
    pub(crate) fn new_with_all_limits(
        qps_limit: u64,
        memory_quota_bytes: usize,
        concurrency_limit: u32,
    ) -> Self {
        let entry = Arc::new(TenantEntry {
            store: TikvStore::new_stub(),
            active_connections: AtomicU32::new(1),
            last_idle_at: AtomicU64::new(0),
            keyspace: "test_ks".to_string(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            rls_policy_cache: Arc::new(RlsPolicyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
            rate_limiter: if qps_limit > 0 {
                Some(TokenBucket::new(qps_limit))
            } else {
                None
            },
            concurrency_tracker: Arc::new(PrincipalConcurrencyTracker::new(concurrency_limit)),
            memory_accountant: TenantMemoryAccountant::new_with_quota(
                "test_ks".to_string(),
                memory_quota_bytes,
            ),
            user_connections: StdMutex::new(HashMap::new()),
            idle_index: None,
        });
        Self {
            entry,
            user_slot: None,
        }
    }
}

impl Clone for TenantHandle {
    fn clone(&self) -> Self {
        let prev = self
            .entry
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
        let prev_idle_at = self.entry.last_idle_at.swap(0, Ordering::Relaxed);
        // If transitioning from idle to active, remove from idle index.
        if prev == 0 && prev_idle_at != 0 {
            if let Some(ref idx) = self.entry.idle_index {
                {
                    let mut set = idx.lock();
                    set.remove(&(prev_idle_at, self.entry.keyspace.clone()));
                }
            }
        }
        Self {
            entry: self.entry.clone(),
            // Clones do NOT inherit the user connection slot — only the
            // original handle owns (and releases) the per-user slot.
            user_slot: None,
        }
    }
}

impl Drop for TenantHandle {
    fn drop(&mut self) {
        if let Some(ref username) = self.user_slot {
            self.entry.release_user_slot(username);
        }
        let prev = self
            .entry
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            // This was the last handle — record idle start time.
            let idle_at = now_epoch_ms();
            self.entry.last_idle_at.store(idle_at, Ordering::Relaxed);
            // Register as idle candidate in the pool-level index.
            if let Some(ref idx) = self.entry.idle_index {
                {
                    let mut set = idx.lock();
                    set.insert((idle_at, self.entry.keyspace.clone()));
                }
            }
        }
    }
}

pub struct TikvClientPool {
    pd_endpoints: Vec<String>,
    tenants: RwLock<HashMap<String, Arc<TenantEntry>>>,
    /// Per-keyspace creation locks to avoid holding the global write lock during
    /// the slow bootstrap (TiKV connect + default DB creation).
    creation_locks: RwLock<HashMap<String, Arc<TokioMutex<()>>>>,
    idle_timeout: Duration,
    reaper_interval: Duration,
    /// Idle-time index: tenants ordered by `(idle_at_epoch_ms, keyspace)`.
    /// Enables O(k) eviction where k = candidates due, instead of O(n) full scan.
    idle_index: Arc<IdleIndex>,
    /// Per-principal concurrent query limit. 0 = disabled.
    /// Sourced from `ServerConfig::max_concurrent_queries_per_principal`.
    concurrency_limit: u32,
    /// Keyspaces with a nonblocking inventory repair task currently running.
    inventory_repairs: Arc<TenantInventoryRepairTracker>,
}

impl TikvClientPool {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self {
            pd_endpoints,
            tenants: RwLock::new(HashMap::new()),
            creation_locks: RwLock::new(HashMap::new()),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            reaper_interval: DEFAULT_REAPER_INTERVAL,
            idle_index: Arc::new(StdMutex::new(BTreeSet::new())),
            concurrency_limit: 0,
            inventory_repairs: Arc::new(TenantInventoryRepairTracker::new()),
        }
    }

    /// Set the per-principal concurrent query limit.
    /// Value comes from `ServerConfig::max_concurrent_queries_per_principal`.
    pub fn with_concurrency_limit(mut self, limit: u32) -> Self {
        self.concurrency_limit = limit;
        self
    }

    /// PD endpoints used by this pool (needed for PD HTTP API calls).
    pub fn pd_endpoints(&self) -> &[String] {
        &self.pd_endpoints
    }

    #[cfg(test)]
    pub(crate) fn new_with_timeouts(
        pd_endpoints: Vec<String>,
        idle_timeout: Duration,
        reaper_interval: Duration,
    ) -> Self {
        Self {
            pd_endpoints,
            tenants: RwLock::new(HashMap::new()),
            creation_locks: RwLock::new(HashMap::new()),
            idle_timeout,
            reaper_interval,
            idle_index: Arc::new(StdMutex::new(BTreeSet::new())),
            concurrency_limit: 0,
            inventory_repairs: Arc::new(TenantInventoryRepairTracker::new()),
        }
    }

    /// Acquire a connection-scoped handle to a tenant's TikvStore.
    ///
    /// The tenant's TikvStore is lazily created on first access and kept alive
    /// as long as at least one TenantHandle exists. Once all handles are dropped,
    /// the entry becomes eligible for eviction after the idle timeout.
    pub async fn acquire(&self, keyspace: Option<String>) -> Result<TenantHandle> {
        let key = keyspace.clone().unwrap_or_else(|| "default".to_string());

        // Fast path: tenant already exists.
        let existing_entry = {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                let prev = entry.active_connections.fetch_add(1, Ordering::Relaxed);
                let prev_idle_at = entry.last_idle_at.swap(0, Ordering::Relaxed);
                // Transitioning from idle to active — remove from idle index.
                if prev == 0 && prev_idle_at != 0 {
                    {
                        let mut set = self.idle_index.lock();
                        set.remove(&(prev_idle_at, key.clone()));
                    }
                }
                Some(entry.clone())
            } else {
                None
            }
        };
        if let Some(entry) = existing_entry {
            if let Some(system_store) = crate::worker::get_system_store() {
                spawn_tenant_database_inventory_repair(
                    system_store.clone(),
                    entry.store.clone(),
                    self.inventory_repairs.clone(),
                    key.clone(),
                    InventoryRepairSpawnMode::RetryOnly,
                );
            }
            return Ok(TenantHandle {
                entry,
                user_slot: None,
            });
        }

        // Slow path: need to create. Use a per-keyspace lock so only one
        // creation runs per keyspace, without blocking other keyspaces.
        let creation_lock = {
            let mut locks = self.creation_locks.write().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };

        let _guard = creation_lock.lock().await;

        // Double-check after acquiring per-keyspace lock.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                let entry = entry.clone();
                let prev = entry.active_connections.fetch_add(1, Ordering::Relaxed);
                let prev_idle_at = entry.last_idle_at.swap(0, Ordering::Relaxed);
                if prev == 0 && prev_idle_at != 0 {
                    {
                        let mut set = self.idle_index.lock();
                        set.remove(&(prev_idle_at, key.clone()));
                    }
                }
                drop(tenants);
                // Construct handle before the `.await` so that async cancellation
                // triggers `TenantHandle::drop`, which decrements `active_connections`.
                let handle = TenantHandle {
                    entry,
                    user_slot: None,
                };
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Ok(handle);
            }
        }

        info!("Creating new TiKV client for keyspace: {}", key);
        let store = match self.create_store(&key, keyspace).await {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Err(e);
            }
        };
        let entry = Arc::new(TenantEntry::new(
            store,
            key.clone(),
            self.idle_index.clone(),
            self.concurrency_limit,
        ));
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        // Construct handle before any `.await` so that async cancellation
        // triggers `TenantHandle::drop`, which decrements `active_connections`.
        let handle = TenantHandle {
            entry: entry.clone(),
            user_slot: None,
        };

        {
            let mut tenants = self.tenants.write().await;
            tenants.insert(key.clone(), entry);
        }
        self.cleanup_creation_lock_if_unused(&key, &creation_lock)
            .await;

        Ok(handle)
    }

    /// Get a TikvStore without a connection-scoped handle.
    ///
    /// Used by background tasks (trigger worker, startup validation) that need
    /// short-lived access to a tenant's store. The returned Arc<TikvStore>
    /// keeps the store alive even if the pool evicts the entry, but does NOT
    /// prevent eviction.
    pub async fn get_client(&self, keyspace: Option<String>) -> Result<Arc<TikvStore>> {
        let key = keyspace.clone().unwrap_or_else(|| "default".to_string());

        // Fast path: tenant already exists.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                return Ok(entry.store.clone());
            }
        }

        // Slow path: create via the same per-keyspace lock mechanism.
        let creation_lock = {
            let mut locks = self.creation_locks.write().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };

        let _guard = creation_lock.lock().await;

        // Double-check.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                let store = entry.store.clone();
                drop(tenants);
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Ok(store);
            }
        }

        info!("Creating new TiKV client for keyspace: {}", key);
        let store = match self.create_store(&key, keyspace).await {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Err(e);
            }
        };
        let entry = Arc::new(TenantEntry::new(
            store,
            key.clone(),
            self.idle_index.clone(),
            self.concurrency_limit,
        ));
        // Mark as idle immediately since no handle is held.
        let idle_at = now_epoch_ms();
        entry.last_idle_at.store(idle_at, Ordering::Relaxed);
        {
            let mut set = self.idle_index.lock();
            set.insert((idle_at, key.clone()));
        }

        let store_clone = entry.store.clone();
        {
            let mut tenants = self.tenants.write().await;
            tenants.insert(key.clone(), entry);
        }
        self.cleanup_creation_lock_if_unused(&key, &creation_lock)
            .await;

        Ok(store_clone)
    }

    /// Remove a per-keyspace creation lock after a slow-path operation
    /// when no concurrent waiters remain.
    async fn cleanup_creation_lock_if_unused(
        &self,
        key: &str,
        creation_lock: &Arc<TokioMutex<()>>,
    ) {
        let mut locks = self.creation_locks.write().await;
        let should_remove = locks
            .get(key)
            .is_some_and(|existing| Arc::ptr_eq(existing, creation_lock))
            && Arc::strong_count(creation_lock) == 2;
        if should_remove {
            locks.remove(key);
        }
    }

    /// Shared TikvStore creation logic. Resolves the "default" keyspace name
    /// and creates the store with bootstrap.
    async fn create_store(&self, key: &str, keyspace: Option<String>) -> Result<Arc<TikvStore>> {
        let actual_keyspace = if key == "default" {
            Some("DEFAULT".to_string())
        } else {
            keyspace
        };

        let result = TikvStore::new_with_keyspace(self.pd_endpoints.clone(), actual_keyspace).await;

        let store = match result {
            Ok(s) => s,
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("keyspace does not exist") {
                    return Err(anyhow!("Tenant '{}' does not exist", key));
                }
                return Err(e);
            }
        };
        let store = Arc::new(store);
        if let Some(system_store) = crate::worker::get_system_store() {
            spawn_tenant_database_inventory_repair(
                system_store.clone(),
                store.clone(),
                self.inventory_repairs.clone(),
                key.to_string(),
                InventoryRepairSpawnMode::Initial,
            );
        }
        Ok(store)
    }

    /// Number of tenants currently cached in the pool.
    #[allow(dead_code)] // test: used in pool tests
    pub async fn tenant_count(&self) -> usize {
        self.tenants.read().await.len()
    }

    /// Number of tenants with at least one active connection handle.
    #[allow(dead_code)] // test: used in pool tests
    pub async fn active_tenant_count(&self) -> usize {
        let tenants = self.tenants.read().await;
        tenants
            .values()
            .filter(|e| e.active_connections.load(Ordering::Relaxed) > 0)
            .count()
    }

    /// Snapshot of the active connection count for a specific keyspace.
    /// Returns None if the keyspace is not in the pool.
    #[allow(dead_code)] // test: used in pool tests
    pub async fn connections_for(&self, keyspace: &str) -> Option<u32> {
        let tenants = self.tenants.read().await;
        tenants
            .get(keyspace)
            .map(|e| e.active_connections.load(Ordering::Relaxed))
    }

    /// Run a single eviction pass. Uses the idle-time index to find candidates
    /// in O(k) time where k is the number of due candidates, instead of scanning
    /// all tenants.
    ///
    /// Stale index entries (tenant reacquired or re-idled at a different timestamp)
    /// are validated against the current tenant state before eviction: a candidate
    /// `(idle_at, keyspace)` is only evicted when the tenant still exists, has zero
    /// active connections, and its current `last_idle_at` matches the candidate.
    pub(crate) async fn evict_idle(&self) -> Vec<String> {
        let now = now_epoch_ms();
        let timeout_ms = self.idle_timeout.as_millis() as u64;
        let deadline = now.saturating_sub(timeout_ms);

        // Collect due candidates from the idle index.
        // The index is ordered by (idle_at, keyspace), so we take all entries
        // with idle_at <= deadline.
        let candidates: Vec<(u64, String)> = {
            let mut idx = self.idle_index.lock();
            let mut due = Vec::new();
            // BTreeSet iteration in ascending order — stop at first entry past deadline.
            while let Some(first) = idx.iter().next().cloned() {
                if first.0 > deadline {
                    break;
                }
                idx.remove(&first);
                due.push(first);
            }
            due
        };

        if candidates.is_empty() {
            return Vec::new();
        }

        let mut tenants = self.tenants.write().await;
        let mut evicted = Vec::new();

        for (candidate_idle_at, keyspace) in candidates {
            let should_evict = tenants.get(&keyspace).is_some_and(|entry| {
                let conns = entry.active_connections.load(Ordering::Relaxed);
                let current_idle_at = entry.last_idle_at.load(Ordering::Relaxed);
                // Only evict if tenant is still idle and the idle timestamp matches
                // the candidate (guards against stale/duplicate index entries).
                conns == 0 && current_idle_at == candidate_idle_at
            });

            if should_evict {
                info!(
                    "Evicting idle tenant '{}' (idle for {}ms)",
                    keyspace,
                    now.saturating_sub(candidate_idle_at)
                );
                tenants.remove(&keyspace);
                evicted.push(keyspace);
            }
        }

        // Clean up creation locks and per-keyspace caches for evicted keyspaces.
        if !evicted.is_empty() {
            let mut locks = self.creation_locks.write().await;
            for ks in &evicted {
                locks.remove(ks);
                crate::auth::invalidate_initialized(ks);
                crate::sql::fts_tokenizers::evict_user_tsc_keyspace(ks);
                crate::extensions::embedding::evict_embedding_semaphore(ks);
                crate::extensions::http::evict_http_limiter(ks);
                crate::extensions::parquet::limits::evict_parquet_limiter(ks);
                admission_budget_registry().evict(ks);
            }
        }

        evicted
    }

    /// Evict one tenant immediately if it is currently idle.
    ///
    /// Worker registry sweeps intentionally acquire many tenants in a bounded
    /// page. Waiting for the normal idle timeout would let those clients
    /// accumulate for up to 300 seconds, so the sweep calls this after dropping
    /// each page's handles.
    pub(crate) async fn evict_if_idle(&self, keyspace: &str) -> bool {
        let removed = {
            let mut tenants = self.tenants.write().await;
            let should_evict = tenants
                .get(keyspace)
                .is_some_and(|entry| entry.active_connections.load(Ordering::Relaxed) == 0);
            if should_evict {
                tenants.remove(keyspace).is_some()
            } else {
                false
            }
        };

        if removed {
            {
                let mut idx = self.idle_index.lock();
                idx.retain(|(_, ks)| ks != keyspace);
            }

            let mut locks = self.creation_locks.write().await;
            locks.remove(keyspace);
            drop(locks);

            crate::auth::invalidate_initialized(keyspace);
            crate::sql::fts_tokenizers::evict_user_tsc_keyspace(keyspace);
            crate::extensions::embedding::evict_embedding_semaphore(keyspace);
            crate::extensions::http::evict_http_limiter(keyspace);
            crate::extensions::parquet::limits::evict_parquet_limiter(keyspace);
            admission_budget_registry().evict(keyspace);
        }

        removed
    }

    /// Spawn a background reaper task that periodically evicts idle tenants.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(pool.reaper_interval);
            loop {
                interval.tick().await;
                let evicted = pool.evict_idle().await;
                if !evicted.is_empty() {
                    debug!("Reaper evicted {} idle tenant(s)", evicted.len());
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_entry_with_index(
        keyspace: &str,
        idle_index: Option<Arc<IdleIndex>>,
    ) -> Arc<TenantEntry> {
        Arc::new(TenantEntry {
            store: TikvStore::new_stub(),
            active_connections: AtomicU32::new(0),
            last_idle_at: AtomicU64::new(0),
            keyspace: keyspace.to_string(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            rls_policy_cache: Arc::new(RlsPolicyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
            rate_limiter: None,
            concurrency_tracker: Arc::new(PrincipalConcurrencyTracker::new(0)),
            memory_accountant: TenantMemoryAccountant::new_with_quota(keyspace.to_string(), 0),
            user_connections: StdMutex::new(HashMap::new()),
            idle_index,
        })
    }

    impl TikvClientPool {
        async fn inject_entry(&self, keyspace: &str) -> Arc<TenantEntry> {
            let entry = make_test_entry_with_index(keyspace, Some(self.idle_index.clone()));
            let mut tenants = self.tenants.write().await;
            tenants.insert(keyspace.to_string(), entry.clone());
            entry
        }
    }

    fn make_handle(entry: &Arc<TenantEntry>) -> TenantHandle {
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        entry.last_idle_at.store(0, Ordering::Relaxed);
        TenantHandle {
            entry: entry.clone(),
            user_slot: None,
        }
    }

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = TikvClientPool::new(vec!["127.0.0.1:2379".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_creation_lock_removed_after_create_store_failure() {
        let pool = TikvClientPool::new(vec!["invalid-pd-endpoint".to_string()]);
        let keyspace = "lock_cleanup_failure";

        let err = match pool.acquire(Some(keyspace.to_string())).await {
            Ok(_) => panic!("acquire should fail when create_store cannot connect"),
            Err(err) => err,
        };
        assert!(
            !err.to_string().is_empty(),
            "failure should return an error message"
        );

        let locks = pool.creation_locks.read().await;
        assert!(
            !locks.contains_key(keyspace),
            "creation lock must be removed after failed create_store"
        );
    }

    #[test]
    fn tenant_store_creation_spawns_inventory_repair_without_awaiting_scan() {
        let source = include_str!("pool.rs");
        let create_store = source
            .split("async fn create_store(")
            .nth(1)
            .and_then(|rest| rest.split("/// Number of tenants currently cached").next())
            .expect("create_store body must exist");
        assert!(
            !create_store.contains("register_database_inventory("),
            "tenant acquisition must not await a full database inventory scan"
        );
        assert!(
            create_store.contains("spawn_tenant_database_inventory_repair("),
            "tenant acquisition should start nonblocking inventory repair"
        );
        assert!(
            create_store.contains("self.inventory_repairs.clone()"),
            "tenant inventory repair should share the pool's per-keyspace singleflight guard"
        );
        assert!(
            create_store.contains("InventoryRepairSpawnMode::Initial"),
            "new tenant store creation should start the first inventory repair"
        );

        let acquire = source
            .split("pub async fn acquire(")
            .nth(1)
            .and_then(|rest| rest.split("/// Remove a per-keyspace creation lock").next())
            .expect("acquire body must exist");
        assert!(
            acquire.contains("InventoryRepairSpawnMode::RetryOnly"),
            "fast-path cached tenant acquisition should retry exhausted repairs when due"
        );

        let repair_helper = source
            .split("fn spawn_tenant_database_inventory_repair(")
            .nth(1)
            .and_then(|rest| rest.split("fn tenant_qps_limit").next())
            .expect("tenant inventory repair helper must exist before tenant config helpers");
        assert!(
            repair_helper.contains("tokio::spawn")
                && repair_helper.contains("register_database_inventory("),
            "tenant inventory repair must run register_database_inventory from a background task"
        );
        assert!(
            repair_helper.contains("tracker.begin(&keyspace, mode, now_epoch_ms())"),
            "tenant inventory repair must acquire a tracker lease before spawning"
        );
        assert!(
            repair_helper.contains("TENANT_INVENTORY_REPAIR_MAX_ATTEMPTS")
                && repair_helper.contains("tenant_inventory_repair_retry_delay"),
            "tenant inventory repair retries must be capped and backed off"
        );
    }

    #[test]
    fn tenant_inventory_repair_retry_delay_is_bounded() {
        assert_eq!(
            tenant_inventory_repair_retry_delay(1),
            TENANT_INVENTORY_REPAIR_RETRY_DELAY
        );
        assert_eq!(
            tenant_inventory_repair_retry_delay(2),
            Duration::from_secs(120)
        );
        assert_eq!(
            tenant_inventory_repair_retry_delay(3),
            Duration::from_secs(240)
        );
        assert_eq!(
            tenant_inventory_repair_retry_delay(4),
            TENANT_INVENTORY_REPAIR_MAX_RETRY_DELAY
        );
        assert_eq!(
            tenant_inventory_repair_retry_delay(100),
            TENANT_INVENTORY_REPAIR_MAX_RETRY_DELAY
        );
    }

    #[test]
    fn tenant_inventory_repair_tracker_dedupes_cools_down_and_clears_on_success() {
        let tracker = Arc::new(TenantInventoryRepairTracker::new());
        let first = tracker
            .begin("ks", InventoryRepairSpawnMode::Initial, now_epoch_ms())
            .expect("initial repair should start");
        assert!(
            tracker
                .begin("ks", InventoryRepairSpawnMode::Initial, now_epoch_ms())
                .is_none(),
            "second repair for the same keyspace must be deduped while in flight"
        );

        let before = now_epoch_ms();
        drop(first);
        let retry_state = tracker
            .state_for("ks")
            .expect("failed repair should stay scheduled");
        assert!(!retry_state.in_flight);
        assert!(
            retry_state.retry_after_ms
                >= before
                    .saturating_add(TENANT_INVENTORY_REPAIR_MAX_RETRY_DELAY.as_millis() as u64),
            "failed repair must set a future retry_after marker"
        );
        assert!(
            tracker
                .begin("ks", InventoryRepairSpawnMode::RetryOnly, now_epoch_ms())
                .is_none(),
            "fast-path retry must respect cooldown"
        );

        let mut retry = tracker
            .begin(
                "ks",
                InventoryRepairSpawnMode::RetryOnly,
                retry_state.retry_after_ms,
            )
            .expect("retry should start when cooldown expires");
        retry.complete();
        assert!(
            tracker.state_for("ks").is_none(),
            "successful repair should clear retry state so fast-path acquire does not rescan"
        );
    }

    #[tokio::test]
    async fn test_tenant_handle_refcount() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("test_ks").await;

        let h1 = make_handle(&entry);
        let h2 = make_handle(&entry);

        assert_eq!(entry.active_connections(), 2);
        assert_eq!(entry.last_idle_at.load(Ordering::Relaxed), 0);

        drop(h1);
        assert_eq!(entry.active_connections(), 1);
        assert_eq!(entry.last_idle_at.load(Ordering::Relaxed), 0);

        drop(h2);
        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_tenant_handle_clone_increments_refcount() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("clone_ks").await;

        let h1 = make_handle(&entry);
        let h2 = h1.clone();
        assert_eq!(entry.active_connections(), 2);

        drop(h1);
        assert_eq!(entry.active_connections(), 1);

        drop(h2);
        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
    }

    /// Helper: mark a test entry as idle and register it in the pool's idle index.
    fn mark_idle(entry: &Arc<TenantEntry>, pool: &TikvClientPool) {
        let idle_at = now_epoch_ms();
        entry.last_idle_at.store(idle_at, Ordering::Relaxed);
        {
            let mut set = pool.idle_index.lock();
            set.insert((idle_at, entry.keyspace.clone()));
        }
    }

    /// Helper: mark a test entry as idle at a specific timestamp.
    fn mark_idle_at(entry: &Arc<TenantEntry>, pool: &TikvClientPool, idle_at: u64) {
        entry.last_idle_at.store(idle_at, Ordering::Relaxed);
        {
            let mut set = pool.idle_index.lock();
            set.insert((idle_at, entry.keyspace.clone()));
        }
    }

    #[tokio::test]
    async fn test_evict_idle_respects_timeout() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(100),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("idle_ks").await;

        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        mark_idle(&entry, &pool);

        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        tokio::time::sleep(Duration::from_millis(150)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["idle_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_evict_preserves_active_tenants() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );

        let active_entry = pool.inject_entry("active_ks").await;
        let idle_entry = pool.inject_entry("idle_ks").await;

        active_entry
            .active_connections
            .fetch_add(1, Ordering::Relaxed);

        mark_idle(&idle_entry, &pool);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["idle_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 1);

        assert!(pool.connections_for("active_ks").await.is_some());
        assert!(pool.connections_for("idle_ks").await.is_none());
    }

    #[tokio::test]
    async fn test_evict_resets_when_handle_reacquired() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(100),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("reacquire_ks").await;

        mark_idle(&entry, &pool);

        tokio::time::sleep(Duration::from_millis(50)).await;
        // Simulate reacquire: transition back to active.
        let prev_idle_at = entry.last_idle_at.swap(0, Ordering::Relaxed);
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        if prev_idle_at != 0 {
            {
                let mut set = pool.idle_index.lock();
                set.remove(&(prev_idle_at, entry.keyspace.clone()));
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);
    }

    #[tokio::test]
    async fn test_get_client_marks_idle_immediately() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("bg_ks").await;
        mark_idle(&entry, &pool);

        assert_eq!(entry.active_connections(), 0);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["bg_ks".to_string()]);
    }

    #[tokio::test]
    async fn test_concurrent_handle_drops() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("concurrent_ks").await;

        let num_handles = 100u32;

        let handles: Vec<TenantHandle> = (0..num_handles).map(|_| make_handle(&entry)).collect();

        assert_eq!(entry.active_connections(), num_handles);

        let mut join_handles = Vec::new();
        for h in handles {
            join_handles.push(tokio::spawn(async move {
                drop(h);
            }));
        }

        for jh in join_handles {
            jh.await.unwrap();
        }

        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_multiple_tenants_independent_lifecycle() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );

        let entry_a = pool.inject_entry("tenant_a").await;
        let entry_b = pool.inject_entry("tenant_b").await;
        let entry_c = pool.inject_entry("tenant_c").await;

        entry_a.active_connections.fetch_add(1, Ordering::Relaxed);

        mark_idle(&entry_b, &pool);

        tokio::time::sleep(Duration::from_millis(100)).await;

        mark_idle(&entry_c, &pool);

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["tenant_b".to_string()]);
        assert_eq!(pool.tenant_count().await, 2);
    }

    #[tokio::test]
    async fn test_active_tenant_count() {
        let pool = TikvClientPool::new(vec![]);

        let entry_a = pool.inject_entry("a").await;
        let entry_b = pool.inject_entry("b").await;
        let _entry_c = pool.inject_entry("c").await;

        entry_a.active_connections.fetch_add(1, Ordering::Relaxed);
        entry_b.active_connections.fetch_add(3, Ordering::Relaxed);

        assert_eq!(pool.active_tenant_count().await, 2);
        assert_eq!(pool.tenant_count().await, 3);
    }

    #[test]
    fn test_token_bucket_allows_up_to_capacity() {
        let bucket = TokenBucket::new(10);
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        // 11th request within the same instant should be denied
        assert!(!bucket.try_acquire());
    }

    #[test]
    fn test_token_bucket_refills_over_time() {
        let bucket = TokenBucket::new(10);
        // Exhaust all tokens
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        assert!(!bucket.try_acquire());

        // Wait 200ms → ~2 tokens should refill
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(bucket.try_acquire());
    }

    #[test]
    fn test_token_bucket_rate_accessor() {
        let bucket = TokenBucket::new(42);
        assert_eq!(bucket.rate(), 42);
    }

    #[tokio::test]
    async fn test_per_user_connection_limit_enforcement() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("limit_ks").await;

        // user_a with limit=1: first connection succeeds.
        let mut h1 = make_handle(&entry);
        assert!(h1.try_bind_user("user_a".to_string(), 1).is_ok());
        assert_eq!(entry.user_connections_for("user_a"), 1);

        // user_a at limit → second connection rejected.
        let mut h2 = make_handle(&entry);
        assert!(h2.try_bind_user("user_a".to_string(), 1).is_err());
        assert_eq!(entry.user_connections_for("user_a"), 1);
        drop(h2); // Never bound, no user slot to release.

        // user_b with limit=-1 (unlimited) → connects fine on same tenant.
        let mut h3 = make_handle(&entry);
        assert!(h3.try_bind_user("user_b".to_string(), -1).is_ok());
        assert_eq!(entry.user_connections_for("user_b"), 1);
        drop(h3);
        assert_eq!(entry.user_connections_for("user_b"), 0);

        // user_a disconnects → user_a can reconnect.
        drop(h1);
        assert_eq!(entry.user_connections_for("user_a"), 0);

        let mut h4 = make_handle(&entry);
        assert!(h4.try_bind_user("user_a".to_string(), 1).is_ok());
        assert_eq!(entry.user_connections_for("user_a"), 1);
        drop(h4);
    }

    #[tokio::test]
    async fn test_per_user_limit_zero_rejects_all() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("zero_ks").await;

        // connection_limit=0 should reject immediately.
        let mut h = make_handle(&entry);
        assert!(h.try_bind_user("blocked_user".to_string(), 0).is_err());
        assert_eq!(entry.user_connections_for("blocked_user"), 0);
        drop(h);
    }

    #[tokio::test]
    async fn test_per_user_clone_does_not_inherit_slot() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("clone_user_ks").await;

        let mut h1 = make_handle(&entry);
        assert!(h1.try_bind_user("user_x".to_string(), 1).is_ok());
        assert_eq!(entry.user_connections_for("user_x"), 1);

        // Clone does NOT inherit the user slot.
        let h2 = h1.clone();
        assert_eq!(entry.user_connections_for("user_x"), 1);
        assert_eq!(entry.active_connections(), 2); // h1 + clone

        // Dropping clone does not release user slot.
        drop(h2);
        assert_eq!(entry.user_connections_for("user_x"), 1);

        // Dropping original releases the user slot.
        drop(h1);
        assert_eq!(entry.user_connections_for("user_x"), 0);
    }

    #[test]
    fn tenant_memory_accountant_unlimited_mode() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_unlimited".to_string(), 0);
        let mut reservation = accountant.reservation();
        reservation
            .grow("test.unlimited", 10 * 1024 * 1024)
            .expect("unlimited quota must allow reservation");
        assert_eq!(reservation.charged_bytes(), 10 * 1024 * 1024);
        assert_eq!(accountant.used_bytes(), 10 * 1024 * 1024);
        drop(reservation);
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn tenant_memory_reservation_no_underflow_or_overrelease() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_underflow".to_string(), 1024);
        let mut reservation = accountant.reservation();
        reservation
            .grow("test.underflow", 512)
            .expect("initial grow should pass");
        reservation.shrink(2048); // deliberate over-release attempt
        assert_eq!(reservation.charged_bytes(), 0);
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn tenant_memory_accountant_concurrent_charge_release_correctness() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_concurrent".to_string(), 0);
        let mut workers = Vec::new();
        for _ in 0..16 {
            let acc = accountant.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..2000 {
                    let mut r = acc.reservation();
                    r.grow("test.concurrent", 64)
                        .expect("unlimited quota should allow charge");
                }
            }));
        }
        for w in workers {
            w.join().expect("thread join");
        }
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn tenant_memory_accountant_is_shared_across_concurrent_handles() {
        // Simulate two concurrent sessions on the same tenant handle domain.
        let h1 = TenantHandle::new_with_limits(0, 100);
        let h2 = h1.clone();
        let acc_a = h1.memory_accountant();
        let acc_b = h2.memory_accountant();

        let mut sess_a = acc_a.reservation();
        sess_a
            .grow("session_a", 80)
            .expect("session A reservation should pass");

        let mut sess_b = acc_b.reservation();
        let err = sess_b
            .grow("session_b", 30)
            .expect_err("combined usage should exceed tenant quota");
        assert_eq!(err.sqlstate(), "53200");

        // Once session A releases, session B should be able to proceed.
        drop(sess_a);
        sess_b
            .grow("session_b", 30)
            .expect("reservation should succeed after A release");
    }

    #[tokio::test]
    async fn statement_scope_runtime_shrink_tracks_live_bytes() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_scope".to_string(), 128);
        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            try_grow_statement_memory_scope("scope.grow", 100).expect("grow should succeed");
            assert_eq!(accountant.used_bytes(), 100);
            try_shrink_statement_memory_scope(40);
            assert_eq!(accountant.used_bytes(), 60);
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }

    // ── expensive_query peak tracking + log trigger ───────────────────

    #[test]
    fn peak_bytes_tracks_high_water_mark_and_ignores_shrink() {
        let acc = TenantMemoryAccountant::unlimited("ks_peak".to_string());
        let mut r = acc.reservation();

        let (res, peak) = r.grow_with_peak("operators.a", 100);
        res.expect("grow ok");
        assert_eq!(peak, 100);
        assert_eq!(r.peak_bytes(), 100);

        let (res, peak) = r.grow_with_peak("operators.a", 50);
        res.expect("grow ok");
        assert_eq!(peak, 150);

        // Shrink frees live bytes but must not lower the high-water mark.
        r.shrink(120);
        assert_eq!(r.charged_bytes(), 30);
        assert_eq!(r.peak_bytes(), 150);
    }

    #[test]
    fn component_grow_top_n_orders_largest_first_and_accumulates() {
        let acc = TenantMemoryAccountant::unlimited("ks_comp".to_string());
        let mut r = acc.reservation();
        let _ = r.grow_with_peak("operators.small", 1024 * 1024);
        let _ = r.grow_with_peak("operators.big", 8 * 1024 * 1024);
        let _ = r.grow_with_peak("operators.mid", 4 * 1024 * 1024);
        // Same label accumulates rather than creating a second entry.
        let _ = r.grow_with_peak("operators.big", 1024 * 1024);

        assert_eq!(
            r.component_grow_top_n(2),
            "operators.big=9MB;operators.mid=4MB"
        );
    }

    #[test]
    fn component_attribution_is_exact_beyond_inline_capacity() {
        // A large component that first appears as the 9th+ distinct label must
        // keep its own label, not fold into a small earlier one (#2559 P1).
        let acc = TenantMemoryAccountant::unlimited("ks_many".to_string());
        let mut r = acc.reservation();
        for label in ["c.a", "c.b", "c.c", "c.d", "c.e", "c.f", "c.g", "c.h"] {
            let _ = r.grow_with_peak(label, 1024 * 1024); // fills the inline slots
        }
        // The dominant component appears only after the inline slots are full.
        let _ = r.grow_with_peak("operators.hash_aggregate.groups", 500 * 1024 * 1024);
        assert_eq!(
            r.component_grow_top_n(1),
            "operators.hash_aggregate.groups=500MB",
            "a large late-appearing component must keep its own label"
        );
    }

    #[test]
    fn grow_with_peak_reports_attempted_but_keeps_physical_peak_on_quota_exceed() {
        // Default config has expensive threshold == tenant quota, so the grow
        // that trips the quota is the one we most want logged. The RETURNED peak
        // reflects the attempted total (so the caller's threshold check fires,
        // edge case J), but the stored physical peak_bytes — fed to the
        // slow-query peak_mb — excludes the rejected bytes, and the rejected grow
        // is not attributed to any component.
        let acc = TenantMemoryAccountant::new_with_quota("ks_quota".to_string(), 10 * 1024 * 1024);
        let mut r = acc.reservation();

        let (res, peak) = r.grow_with_peak("operators.ok", 4 * 1024 * 1024);
        res.expect("first grow fits under quota");
        assert_eq!(peak, 4 * 1024 * 1024);

        let (res, peak) = r.grow_with_peak("operators.rejected", 20 * 1024 * 1024);
        assert!(res.is_err(), "grow must exceed the 10 MiB quota");
        assert_eq!(
            peak,
            24 * 1024 * 1024,
            "returned peak reflects the attempted total"
        );
        assert_eq!(
            r.charged_bytes(),
            4 * 1024 * 1024,
            "rejected grow must not charge"
        );
        assert_eq!(
            r.peak_bytes(),
            4 * 1024 * 1024,
            "physical peak excludes the rejected attempt"
        );
        assert_eq!(
            r.component_grow_top_n(2),
            "operators.ok=4MB",
            "rejected grow is not attributed to a component"
        );
    }

    #[test]
    fn reset_attempt_tracking_clears_components_and_rebaselines_peak() {
        // Models the autocommit retry loop: each attempt re-runs the operators
        // within one scope; reset_attempt_tracking prevents component_grow_top
        // from inflating by the retry count.
        let acc = TenantMemoryAccountant::unlimited("ks_retry".to_string());
        let mut r = acc.reservation();
        let _ = r.grow_with_peak("operators.hash_join.build", 100 * 1024 * 1024);
        r.shrink(100 * 1024 * 1024); // attempt's operators release before retry
        assert_eq!(r.peak_bytes(), 100 * 1024 * 1024);

        r.reset_attempt_tracking();
        assert_eq!(r.peak_bytes(), 0, "peak rebaselined to current live bytes");
        assert_eq!(r.component_grow_top_n(1), "", "components cleared");

        // The retry grows the same operator again — no carryover / no inflation.
        let _ = r.grow_with_peak("operators.hash_join.build", 100 * 1024 * 1024);
        assert_eq!(r.peak_bytes(), 100 * 1024 * 1024);
        assert_eq!(r.component_grow_top_n(1), "operators.hash_join.build=100MB");
    }

    #[test]
    fn split_resets_peak_on_new_owner() {
        let acc = TenantMemoryAccountant::unlimited("ks_split".to_string());
        let mut r = acc.reservation();
        let _ = r.grow_with_peak("operators.a", 200);
        assert_eq!(r.peak_bytes(), 200);

        let split = r.split(50).expect("split within charged bytes");
        assert_eq!(split.peak_bytes(), 0, "split-off owner starts fresh");
        assert_eq!(split.charged_bytes(), 50);
        // The original keeps its own peak.
        assert_eq!(r.peak_bytes(), 200);
        assert_eq!(r.charged_bytes(), 150);
    }

    /// Count `expensive_query`-target tracing events emitted while running `f`
    /// on this thread. Uses a thread-local subscriber so it is isolated from
    /// parallel tests and any global subscriber.
    fn count_expensive_logs<F: FnOnce()>(f: F) -> usize {
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone)]
        struct CountingLayer(Arc<AtomicUsize>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountingLayer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event.metadata().target() == "expensive_query" {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(CountingLayer(count.clone()));
        tracing::subscriber::with_default(subscriber, f);
        count.load(Ordering::SeqCst)
    }

    /// Run `f` inside a statement scope with an explicit threshold (bypassing the
    /// env-driven `expensive_mem_threshold_bytes()` OnceLock so tests are hermetic).
    fn with_log_scope<F: FnOnce()>(threshold_bytes: usize, f: F) {
        let acc = TenantMemoryAccountant::unlimited("ks_log".to_string());
        let scope = Arc::new(StatementMemoryScope::new(acc, 7, threshold_bytes));
        STATEMENT_MEMORY_SCOPE.sync_scope(scope, f);
    }

    #[test]
    fn expensive_log_fires_once_at_threshold() {
        let n = count_expensive_logs(|| {
            with_log_scope(100, || {
                set_current_statement_sql("SELECT count(*) FROM t GROUP BY x");
                // peak: 60 (no log), 120 (crosses → log), 180 (latched).
                try_grow_statement_memory_scope("operators.hash_aggregate.groups", 60).unwrap();
                try_grow_statement_memory_scope("operators.hash_aggregate.groups", 60).unwrap();
                try_grow_statement_memory_scope("operators.hash_aggregate.groups", 60).unwrap();
            });
        });
        assert_eq!(n, 1);
    }

    #[test]
    fn expensive_log_does_not_fire_without_sql() {
        let n = count_expensive_logs(|| {
            with_log_scope(100, || {
                // No set_current_statement_sql: empty-SQL guard suppresses the log.
                try_grow_statement_memory_scope("operators.sort.rows", 200).unwrap();
                try_grow_statement_memory_scope("operators.sort.rows", 200).unwrap();
            });
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn expensive_log_self_heals_when_sql_set_after_crossing() {
        let n = count_expensive_logs(|| {
            with_log_scope(100, || {
                // Peak crosses the threshold before the SQL is known...
                try_grow_statement_memory_scope("operators.sort.rows", 200).unwrap();
                // ...and the setter self-heals by emitting immediately.
                set_current_statement_sql("SELECT * FROM big ORDER BY x");
            });
        });
        assert_eq!(n, 1);
    }

    #[test]
    fn threshold_zero_disables_log() {
        let n = count_expensive_logs(|| {
            with_log_scope(0, || {
                set_current_statement_sql("SELECT 1");
                try_grow_statement_memory_scope("operators.sort.rows", 1_000_000).unwrap();
            });
        });
        assert_eq!(n, 0);
    }

    #[test]
    fn distinct_on_component_is_attributed_in_top_n() {
        // C3 (#2557 v6): operators.distinct_on.hashset (distinct.rs:141) routes
        // through the same hook as every other operator, so its peak must be
        // attributable in component_top. Guards the call site the v5 §1 table missed.
        let acc = TenantMemoryAccountant::unlimited("ks_distinct_on".to_string());
        let mut r = acc.reservation();
        let _ = r.grow_with_peak("operators.distinct.hashset", 2 * 1024 * 1024);
        let _ = r.grow_with_peak("operators.distinct_on.hashset", 6 * 1024 * 1024);
        assert_eq!(
            r.component_grow_top_n(1),
            "operators.distinct_on.hashset=6MB"
        );
    }

    #[test]
    fn expensive_log_carries_last_written_start_ts() {
        // Edge case L: autocommit dual-write — execute_single writes a `0`
        // placeholder, then Session::begin writes the real value; last-write-wins.
        let captured = capture_expensive_log_start_ts(|| {
            with_log_scope(100, || {
                set_current_statement_sql("SELECT count(*) FROM t GROUP BY x");
                set_current_statement_start_ts(0); // execute_single autocommit read (Idle)
                set_current_statement_start_ts(12345); // Session::begin Idle→Active hook
                try_grow_statement_memory_scope("operators.hash_aggregate.groups", 150).unwrap();
            });
        });
        assert_eq!(captured, Some(12345));
    }

    /// Capture the `start_ts` field of the first `expensive_query` event emitted
    /// while running `f` on this thread.
    fn capture_expensive_log_start_ts<F: FnOnce()>(f: F) -> Option<u64> {
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone)]
        struct StartTsLayer(Arc<std::sync::Mutex<Option<u64>>>);
        struct Visitor<'a>(&'a mut Option<u64>);
        impl tracing::field::Visit for Visitor<'_> {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                if field.name() == "start_ts" {
                    *self.0 = Some(value);
                }
            }
            fn record_debug(&mut self, _f: &tracing::field::Field, _v: &dyn std::fmt::Debug) {}
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for StartTsLayer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event.metadata().target() == "expensive_query" {
                    let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
                    event.record(&mut Visitor(&mut slot));
                }
            }
        }

        let slot = Arc::new(std::sync::Mutex::new(None));
        let subscriber = tracing_subscriber::registry().with(StartTsLayer(slot.clone()));
        tracing::subscriber::with_default(subscriber, f);
        let captured = *slot.lock().unwrap_or_else(|e| e.into_inner());
        captured
    }

    // ── idle-index stale-candidate tests ──────────────────────────────

    #[tokio::test]
    async fn test_idle_index_reacquire_before_expiry_prevents_eviction() {
        // A tenant goes idle, gets a candidate in the index, then reacquires
        // before the timeout. The stale candidate must not cause eviction.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(80),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("stale_ks").await;

        // Go idle.
        mark_idle(&entry, &pool);
        let old_idle_at = entry.last_idle_at.load(Ordering::Relaxed);

        // Reacquire before expiry.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let prev_idle = entry.last_idle_at.swap(0, Ordering::Relaxed);
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        if prev_idle != 0 {
            {
                let mut set = pool.idle_index.lock();
                set.remove(&(prev_idle, entry.keyspace.clone()));
            }
        }

        // Wait past the original timeout.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Eviction should find nothing — stale candidate was removed.
        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        // Verify the old candidate is gone from the index.
        let idx_len = pool.idle_index.lock().len();
        assert_eq!(idx_len, 0);

        // Make sure the old idle_at was indeed removed (not left dangling).
        assert_eq!(entry.last_idle_at.load(Ordering::Relaxed), 0);
        let _ = old_idle_at; // used above via mark_idle
    }

    #[tokio::test]
    async fn test_idle_index_duplicate_candidates_are_harmless() {
        // If a tenant goes idle, gets reacquired, goes idle again — the index
        // may briefly have two entries. Only the current one should cause eviction.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(150),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("dup_ks").await;

        // First idle cycle.
        let first_idle = now_epoch_ms();
        mark_idle_at(&entry, &pool, first_idle);

        // Reacquire.
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        entry.last_idle_at.store(0, Ordering::Relaxed);
        // Intentionally do NOT remove the old index entry — simulates a race.

        // Second idle cycle with a large gap so the first candidate expires
        // well before the second.
        tokio::time::sleep(Duration::from_millis(100)).await;
        entry.active_connections.fetch_sub(1, Ordering::Relaxed);
        let second_idle = now_epoch_ms();
        mark_idle_at(&entry, &pool, second_idle);

        // Wait past the first candidate's expiry but not the second.
        // First candidate: first_idle + 150ms. Now is ~ first_idle + 100 + 70 = first_idle + 170.
        // Second candidate: second_idle + 150ms = (first_idle + 100) + 150 = first_idle + 250.
        tokio::time::sleep(Duration::from_millis(70)).await;

        // The first candidate is stale (idle_at mismatch) — should NOT evict.
        // The second candidate is not yet due.
        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        // Wait for the second candidate to expire.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["dup_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_idle_index_evicted_and_recreated_keyspace() {
        // A tenant is evicted, then the same keyspace is recreated.
        // Verify that stale index entries from the old tenant don't interfere.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );

        // First tenant.
        let entry1 = pool.inject_entry("recycle_ks").await;
        mark_idle(&entry1, &pool);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["recycle_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);

        // Recreate tenant with same keyspace.
        let entry2 = pool.inject_entry("recycle_ks").await;
        entry2.active_connections.fetch_add(1, Ordering::Relaxed);

        // No eviction should happen — new tenant is active.
        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);
    }

    #[tokio::test]
    async fn test_idle_index_drop_registers_candidate() {
        // Verify that TenantHandle::Drop correctly registers an idle candidate.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("drop_ks").await;
        let handle = make_handle(&entry);

        assert_eq!(entry.active_connections(), 1);
        assert!(pool.idle_index.lock().is_empty());

        drop(handle);

        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
        let idx = pool.idle_index.lock();
        assert_eq!(idx.len(), 1);
        let (_, ks) = idx.iter().next().unwrap();
        assert_eq!(ks, "drop_ks");
    }

    // --- PrincipalConcurrencyTracker tests ---

    #[test]
    fn test_concurrency_tracker_acquire_under_limit() {
        let tracker = Arc::new(PrincipalConcurrencyTracker::new(3));
        let g1 = tracker.try_acquire("alice").unwrap();
        let g2 = tracker.try_acquire("alice").unwrap();
        assert_eq!(tracker.current_count("alice"), 2);
        drop(g1);
        assert_eq!(tracker.current_count("alice"), 1);
        drop(g2);
        assert_eq!(tracker.current_count("alice"), 0);
    }

    #[test]
    fn test_concurrency_tracker_reject_at_limit() {
        let tracker = Arc::new(PrincipalConcurrencyTracker::new(2));
        let _g1 = tracker.try_acquire("bob").unwrap();
        let _g2 = tracker.try_acquire("bob").unwrap();
        let result = tracker.try_acquire("bob");
        assert!(result.is_err());
        let rejection = result.unwrap_err();
        assert_eq!(rejection.limit, 2);
    }

    #[test]
    fn test_concurrency_tracker_guard_drop_decrements() {
        let tracker = Arc::new(PrincipalConcurrencyTracker::new(2));
        let g1 = tracker.try_acquire("carol").unwrap();
        let _g2 = tracker.try_acquire("carol").unwrap();
        assert_eq!(tracker.current_count("carol"), 2);
        // At limit — reject.
        assert!(tracker.try_acquire("carol").is_err());
        // Drop one guard — should allow new acquisition.
        drop(g1);
        assert_eq!(tracker.current_count("carol"), 1);
        let _g3 = tracker.try_acquire("carol").unwrap();
        assert_eq!(tracker.current_count("carol"), 2);
    }

    #[test]
    fn test_concurrency_tracker_multiple_principals() {
        let tracker = Arc::new(PrincipalConcurrencyTracker::new(1));
        let _g1 = tracker.try_acquire("alice").unwrap();
        // alice is at limit, but bob should still be allowed.
        let _g2 = tracker.try_acquire("bob").unwrap();
        assert!(tracker.try_acquire("alice").is_err());
        assert!(tracker.try_acquire("bob").is_err());
        assert_eq!(tracker.current_count("alice"), 1);
        assert_eq!(tracker.current_count("bob"), 1);
    }

    #[test]
    fn test_concurrency_tracker_zero_limit_disabled() {
        let tracker = Arc::new(PrincipalConcurrencyTracker::new(0));
        // With limit 0, try_acquire should always succeed (disabled).
        let _g1 = tracker.try_acquire("alice").unwrap();
        let _g2 = tracker.try_acquire("alice").unwrap();
        let _g3 = tracker.try_acquire("alice").unwrap();
        // All succeed — no rejection.
    }

    #[test]
    fn test_concurrency_tracker_entry_removed_at_zero() {
        let tracker = Arc::new(PrincipalConcurrencyTracker::new(5));
        let g1 = tracker.try_acquire("dave").unwrap();
        assert_eq!(tracker.current_count("dave"), 1);
        drop(g1);
        assert_eq!(tracker.current_count("dave"), 0);
        // Verify the entry is actually removed from the map.
        let map = tracker.counters.lock();
        assert!(!map.contains_key("dave"));
    }

    // ─── TokenBucket with separate burst ────────────────────────────

    #[test]
    fn token_bucket_with_burst_allows_burst_above_rate() {
        // rate=2, burst=5 → should allow 5 immediate requests, then reject
        let bucket = TokenBucket::new_with_burst(2, 5);
        for _ in 0..5 {
            assert!(bucket.try_acquire(), "should allow up to burst capacity");
        }
        assert!(!bucket.try_acquire(), "should reject after burst exhausted");
    }

    #[test]
    fn token_bucket_with_burst_refills_at_rate_not_burst() {
        let bucket = TokenBucket::new_with_burst(1000, 2000);
        bucket.drain();
        // After draining, immediate acquire should fail
        assert!(!bucket.try_acquire());
        // After a small sleep, refill rate is 1000/sec, so ~10ms → ~10 tokens
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(bucket.try_acquire(), "should refill at rate, not burst");
    }

    // ─── AdmissionBudgetRegistry ────────────────────────────────────

    #[test]
    fn admission_registry_returns_same_bucket_for_same_owner() {
        let registry = AdmissionBudgetRegistry::new();
        let params = AdmissionBudgetParams {
            owner_id: "tenant_1".to_string(),
            rps: 100,
            burst: 200,
        };
        let b1 = registry.get_or_create(&params);
        let b2 = registry.get_or_create(&params);
        assert!(Arc::ptr_eq(&b1, &b2), "same owner should share one bucket");
    }

    #[test]
    fn admission_registry_separate_buckets_for_different_owners() {
        let registry = AdmissionBudgetRegistry::new();
        let p1 = AdmissionBudgetParams {
            owner_id: "tenant_a".to_string(),
            rps: 100,
            burst: 200,
        };
        let p2 = AdmissionBudgetParams {
            owner_id: "tenant_b".to_string(),
            rps: 50,
            burst: 100,
        };
        let b1 = registry.get_or_create(&p1);
        let b2 = registry.get_or_create(&p2);
        assert!(
            !Arc::ptr_eq(&b1, &b2),
            "different owners should have separate buckets"
        );
        assert_eq!(b1.rate(), 100);
        assert_eq!(b2.rate(), 50);
    }

    /// Verify that `evict_idle()` calls per-keyspace cache cleanup hooks.
    ///
    /// Populates `INITIALIZED_KEYSPACES`, `USER_TSC_CACHE`, and the
    /// `AdmissionBudgetRegistry` for a tenant, then evicts the tenant
    /// and confirms all caches are cleaned.
    #[tokio::test]
    async fn test_evict_idle_cleans_per_keyspace_caches() {
        let ks = "hook_cleanup_ks";

        // Pre-populate per-keyspace caches.
        crate::auth::mark_initialized(ks);
        crate::sql::fts_tokenizers::register_user_tsc(ks, 1, "my_config", "simple");
        let budget_params = AdmissionBudgetParams {
            owner_id: ks.to_string(),
            rps: 10,
            burst: 10,
        };
        admission_budget_registry().get_or_create(&budget_params);

        // Confirm caches are populated.
        assert!(
            crate::auth::is_initialized_cached(ks),
            "INITIALIZED_KEYSPACES should contain the keyspace before eviction"
        );
        assert!(
            crate::sql::fts_tokenizers::resolve_user_tsc(ks, 1, "my_config").is_some(),
            "USER_TSC_CACHE should contain the keyspace before eviction"
        );

        // Inject tenant and evict it.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry(ks).await;
        mark_idle(&entry, &pool);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec![ks.to_string()]);

        // Verify cleanup hooks were called.
        assert!(
            !crate::auth::is_initialized_cached(ks),
            "INITIALIZED_KEYSPACES must be cleaned after eviction"
        );
        assert!(
            crate::sql::fts_tokenizers::resolve_user_tsc(ks, 1, "my_config").is_none(),
            "USER_TSC_CACHE must be cleaned after eviction"
        );
    }

    #[test]
    fn admission_registry_shared_bucket_enforces_cross_session() {
        let registry = AdmissionBudgetRegistry::new();
        let params = AdmissionBudgetParams {
            owner_id: "tenant_shared".to_string(),
            rps: 5,
            burst: 5,
        };
        let b1 = registry.get_or_create(&params);
        let b2 = registry.get_or_create(&params);
        // Drain via session 1
        for _ in 0..5 {
            assert!(b1.try_acquire());
        }
        // Session 2 should also be rejected (shared bucket)
        assert!(!b2.try_acquire(), "cross-session enforcement must work");
    }
}
