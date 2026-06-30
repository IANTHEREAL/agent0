use std::env;

const DEFAULT_POLL_MS: u64 = 60_000;
const MIN_POLL_MS: u64 = 100;
const DEFAULT_MAX_CONCURRENT_JOBS: usize = 32;
const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_CRON_JOB_TIMEOUT_MS: u64 = 1_800_000;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;
// Claim lease span. The owning worker renews at ~lease/3 while it executes, so
// a long-running BgSql/BgDdl task keeps its claim alive instead of being reaped
// mid-flight and double-executed by a second worker. Must be large relative to
// plausible clock skew; sub-second leases are forbidden (design §K4).
const DEFAULT_CLAIM_LEASE_MS: u64 = 60_000;
const MIN_CLAIM_LEASE_MS: u64 = 5_000;
const DEFAULT_EXECUTOR_LEASE_MS: u64 = 30_000;
const MIN_EXECUTOR_LEASE_MS: u64 = 10_000;
const DEFAULT_GC_BATCH_SIZE: usize = 100;
const DEFAULT_AUTO_ANALYZE_THRESHOLD: u64 = 50;
const DEFAULT_GC_INTERVAL_SEC: u64 = 600;
const MIN_GC_INTERVAL_SEC: u64 = 30;
const DEFAULT_HNSW_SWEEP_INTERVAL_SEC: u64 = 600;
const MIN_HNSW_SWEEP_INTERVAL_SEC: u64 = 30;
const DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC: u64 = 60;
const MIN_REGISTRY_SWEEP_INTERVAL_SEC: u64 = 1;
const DEFAULT_SWEEP_PAGE_INTERVAL_SEC: u64 = 30;
const MIN_SWEEP_PAGE_INTERVAL_SEC: u64 = 1;
// Storage-size accounting (StorageSizeScan) refreshes a PD Region/MiB estimate
// for each database. With the storage dirty-marker producer removed, this
// interval is the only automatic refresh cadence; keep it much slower than the
// old 30-min all-tenant scan while still self-healing within the same day.
const DEFAULT_STORAGE_SCAN_INTERVAL_SEC: u64 = 21_600;
const MIN_STORAGE_SCAN_INTERVAL_SEC: u64 = 60;
const DEFAULT_STORAGE_SCAN_JITTER_SEC: u64 = 300;
const DEFAULT_STORAGE_SCAN_PD_RATE_LIMIT_MS: u64 = 100;
const DEFAULT_STORAGE_SCAN_DERIVED_ACTIVE: bool = false;
const DEFAULT_STORAGE_SCAN_DERIVED_SHADOW: bool = false;
const DEFAULT_STORAGE_SCAN_DERIVED_CAPACITY: u16 = 1;
const DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE: usize = DEFAULT_MAX_CONCURRENT_JOBS;
const DEFAULT_SYSTEM_KEYSPACE: &str = "_sys_worker";
const DEFAULT_GC_REGISTRY_KEYSPACE: &str = "";
const DEFAULT_DB_LIFECYCLE_KEYSPACE: &str = "";
const DEFAULT_BG_KEYSPACE: &str = "";
const DEFAULT_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC: u64 = 300;
const MIN_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC: u64 = 30;

// GC safepoint defaults — controls TiKV MVCC version cleanup.
// Without periodic safepoint advancement, TiKV never GCs old MVCC versions,
// causing unbounded storage growth and eventual compaction failure / worker panic.
const DEFAULT_GC_SAFEPOINT_ENABLED: bool = true;
const DEFAULT_GC_SAFEPOINT_INTERVAL_SEC: u64 = 300; // 5 minutes
const MIN_GC_SAFEPOINT_INTERVAL_SEC: u64 = 30;
// 24 hours — conservative default retention window for historical MVCC
// versions. Active foreground and worker transactions are protected directly
// via ActiveTxnRegistry; gc_life_time is not a timeout surrogate.
const DEFAULT_GC_LIFE_TIME_SEC: u64 = 86400;
const MIN_GC_LIFE_TIME_SEC: u64 = 600; // TiDB enforces minimum 10 minutes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcRegistryMode {
    Legacy,
    Migrating,
    New,
}

impl GcRegistryMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Migrating => "migrating",
            Self::New => "new",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "legacy" => Some(Self::Legacy),
            "migrating" => Some(Self::Migrating),
            "new" => Some(Self::New),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbLifecycleMode {
    Legacy,
    Migrating,
    Fenced,
}

impl DbLifecycleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Migrating => "migrating",
            Self::Fenced => "fenced",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "legacy" => Some(Self::Legacy),
            "migrating" => Some(Self::Migrating),
            "fenced" => Some(Self::Fenced),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub enabled: bool,
    pub poll_ms: u64,
    pub max_concurrent_jobs: usize,
    pub worker_id: String,
    pub gc_instance_id: String,
    pub statement_timeout_ms: u64,
    pub cron_job_timeout_ms: u64,
    pub orphan_timeout_sec: u64,
    /// Worker-claim lease span in ms. The executing worker renews its claim at
    /// ~lease/3; GC reaps only leases that have actually expired.
    pub claim_lease_ms: u64,
    /// Cluster-wide executor lease span in ms. Only the holder scans/drains the
    /// worker queues; other db9 processes remain standby SQL-serving nodes.
    pub executor_lease_ms: u64,
    pub gc_batch_size: usize,
    pub auto_analyze_enabled: bool,
    pub auto_analyze_threshold: u64,
    pub gc_interval_sec: u64,
    pub hnsw_sweep_interval_sec: u64,
    pub registry_sweep_interval_sec: u64,
    pub sweep_page_interval_sec: u64,
    pub storage_scan_interval_sec: u64,
    pub storage_scan_jitter_sec: u64,
    pub storage_scan_pd_rate_limit_ms: u64,
    pub storage_scan_derived_active: bool,
    pub storage_scan_derived_shadow: bool,
    pub storage_scan_derived_capacity: u16,
    pub registry_reconcile_batch_size: usize,
    /// Legacy worker/background keyspace. During Phase 0 all domain-specific
    /// keyspaces alias this value unless explicitly overridden.
    pub system_keyspace: String,
    /// Core GC registry keyspace. Startup GC publish is fail-fast against this
    /// domain, which may later move physically away from `_sys_worker`.
    ///
    /// Empty means "inherit `system_keyspace`"; any non-empty value is an
    /// explicit operator override, including `_sys_worker`.
    pub gc_registry_keyspace: String,
    /// Core DB lifecycle keyspace. Tenant incarnation, node lifecycle, and
    /// drop/create correctness move here in Phase 2.
    ///
    /// Empty means "inherit `system_keyspace`"; any non-empty value is an
    /// explicit operator override, including `_sys_worker`.
    pub db_lifecycle_keyspace: String,
    /// DB lifecycle migration mode. Legacy aliases the worker system keyspace.
    /// Migrating physically writes lifecycle rows to `DB9_DB_LIFECYCLE_KEYSPACE`
    /// while readers still tolerate pre-fence tenants with missing stamps.
    /// Fenced is reserved until a durable cluster/version gate proves every
    /// CREATE/DROP writer stamps tenant incarnation before visibility.
    pub db_lifecycle_mode: DbLifecycleMode,
    /// Interval for publishing this process's DB lifecycle process liveness.
    /// In migrating mode this runs against the dedicated Core lifecycle keyspace.
    pub db_lifecycle_publish_interval_sec: u64,
    /// Degradable background/legacy worker keyspace.
    ///
    /// Empty means "inherit `system_keyspace`"; any non-empty value is an
    /// explicit operator override, including `_sys_worker`.
    pub bg_keyspace: String,
    /// GC registry migration mode. Legacy reads/writes the worker system
    /// keyspace; Migrating dual-writes and union-reads legacy + new; New
    /// reads/writes only the dedicated Core GC keyspace.
    pub gc_registry_mode: GcRegistryMode,

    // TiKV MVCC GC safepoint advancement
    pub gc_safepoint_enabled: bool,
    pub gc_safepoint_interval_sec: u64,
    pub gc_life_time_sec: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
        let pid = std::process::id();
        let worker_id = format!("{}:{}", hostname, pid);
        let gc_instance_id = uuid::Uuid::new_v4().to_string();

        Self {
            enabled: true,
            poll_ms: DEFAULT_POLL_MS,
            max_concurrent_jobs: DEFAULT_MAX_CONCURRENT_JOBS,
            worker_id,
            gc_instance_id,
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            cron_job_timeout_ms: DEFAULT_CRON_JOB_TIMEOUT_MS,
            orphan_timeout_sec: DEFAULT_ORPHAN_TIMEOUT_SEC,
            claim_lease_ms: DEFAULT_CLAIM_LEASE_MS,
            executor_lease_ms: DEFAULT_EXECUTOR_LEASE_MS,
            gc_batch_size: DEFAULT_GC_BATCH_SIZE,
            auto_analyze_enabled: true,
            auto_analyze_threshold: DEFAULT_AUTO_ANALYZE_THRESHOLD,
            gc_interval_sec: DEFAULT_GC_INTERVAL_SEC,
            hnsw_sweep_interval_sec: DEFAULT_HNSW_SWEEP_INTERVAL_SEC,
            registry_sweep_interval_sec: DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC,
            sweep_page_interval_sec: DEFAULT_SWEEP_PAGE_INTERVAL_SEC,
            storage_scan_interval_sec: DEFAULT_STORAGE_SCAN_INTERVAL_SEC,
            storage_scan_jitter_sec: DEFAULT_STORAGE_SCAN_JITTER_SEC,
            storage_scan_pd_rate_limit_ms: DEFAULT_STORAGE_SCAN_PD_RATE_LIMIT_MS,
            storage_scan_derived_active: DEFAULT_STORAGE_SCAN_DERIVED_ACTIVE,
            storage_scan_derived_shadow: DEFAULT_STORAGE_SCAN_DERIVED_SHADOW,
            storage_scan_derived_capacity: DEFAULT_STORAGE_SCAN_DERIVED_CAPACITY,
            registry_reconcile_batch_size: DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE,
            system_keyspace: DEFAULT_SYSTEM_KEYSPACE.to_string(),
            gc_registry_keyspace: DEFAULT_GC_REGISTRY_KEYSPACE.to_string(),
            db_lifecycle_keyspace: DEFAULT_DB_LIFECYCLE_KEYSPACE.to_string(),
            db_lifecycle_mode: DbLifecycleMode::Legacy,
            db_lifecycle_publish_interval_sec: DEFAULT_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC,
            bg_keyspace: DEFAULT_BG_KEYSPACE.to_string(),
            gc_registry_mode: GcRegistryMode::Legacy,

            gc_safepoint_enabled: DEFAULT_GC_SAFEPOINT_ENABLED,
            gc_safepoint_interval_sec: DEFAULT_GC_SAFEPOINT_INTERVAL_SEC,
            gc_life_time_sec: DEFAULT_GC_LIFE_TIME_SEC,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Read `var` as a `u64`, clamping to `min`. On a value below `min` or a
/// non-integer value, leave `*current` unchanged and emit the exact same
/// `warn!` text the hand-written blocks used. `unit` is the trailing unit
/// suffix in the message ("ms" or "s").
fn parse_u64_min(var: &str, current: &mut u64, min: u64, unit: &str) {
    if let Ok(v) = env::var(var) {
        match v.parse::<u64>() {
            Ok(parsed) if parsed >= min => {
                *current = parsed;
            }
            Ok(parsed) => {
                tracing::warn!(
                    "{}={} is below minimum {}{}; using default {}{}",
                    var,
                    parsed,
                    min,
                    unit,
                    *current,
                    unit
                );
            }
            Err(_) => {
                tracing::warn!(
                    "{}='{}' is not a valid integer; using default {}{}",
                    var,
                    v,
                    *current,
                    unit
                );
            }
        }
    }
}

/// Read `var` as a `u64` with no minimum. On a non-integer value, leave
/// `*current` unchanged and emit the exact same `warn!` text the hand-written
/// blocks used. `unit` is the trailing unit suffix in the message ("ms" or "s").
fn parse_u64_warn(var: &str, current: &mut u64, unit: &str) {
    if let Ok(v) = env::var(var) {
        match v.parse::<u64>() {
            Ok(parsed) => *current = parsed,
            Err(_) => {
                tracing::warn!(
                    "{}='{}' is not a valid integer; using default {}{}",
                    var,
                    v,
                    *current,
                    unit
                );
            }
        }
    }
}

/// Read `var` as a positive `u64`, leaving `*current` unchanged on a missing,
/// non-integer, or non-positive value. Silent (no `warn!`) — matches the
/// `.parse().ok().filter(|n| *n > 0).unwrap_or(...)` blocks.
fn parse_positive_u64(var: &str, current: &mut u64) {
    if let Ok(v) = env::var(var) {
        *current = v.parse::<u64>().ok().filter(|n| *n > 0).unwrap_or(*current);
    }
}

/// Read `var` as a positive `usize`, leaving `*current` unchanged on a missing,
/// non-integer, or non-positive value. Silent (no `warn!`).
fn parse_positive_usize(var: &str, current: &mut usize) {
    if let Ok(v) = env::var(var) {
        *current = v
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(*current);
    }
}

/// Read `var` as a positive `usize` parsed through a `u64` and capped at
/// `u32::MAX`, leaving `*current` unchanged on a missing, non-integer, or
/// non-positive value. Silent (no `warn!`) — matches the `gc_batch_size` block.
fn parse_positive_usize_u32capped(var: &str, current: &mut usize) {
    if let Ok(v) = env::var(var) {
        *current = v
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .map(|n| n.min(u32::MAX as u64) as usize)
            .unwrap_or(*current);
    }
}

/// Read `var` as a boolean via [`parse_bool`], leaving `*current` unchanged on
/// a missing or unrecognized value. Silent (no `warn!`).
fn parse_bool_var(var: &str, current: &mut bool) {
    if let Ok(v) = env::var(var) {
        *current = parse_bool(&v).unwrap_or(*current);
    }
}

impl WorkerConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        let worker_enabled_raw = env::var("DB9_WORKER_ENABLED").ok();
        if let Some(v) = worker_enabled_raw.as_deref() {
            match parse_bool(v) {
                Some(enabled) => cfg.enabled = enabled,
                None => {
                    tracing::warn!(
                        "DB9_WORKER_ENABLED='{}' is not a valid boolean; disabling worker",
                        v
                    );
                    cfg.enabled = false;
                }
            }
        }
        parse_u64_min("DB9_WORKER_POLL_MS", &mut cfg.poll_ms, MIN_POLL_MS, "ms");
        parse_positive_usize(
            "DB9_WORKER_MAX_CONCURRENT_JOBS",
            &mut cfg.max_concurrent_jobs,
        );
        if let Ok(v) = env::var("DB9_WORKER_ID") {
            cfg.worker_id = v;
        }
        parse_u64_warn(
            "DB9_WORKER_STATEMENT_TIMEOUT_MS",
            &mut cfg.statement_timeout_ms,
            "ms",
        );
        parse_u64_warn(
            "DB9_CRON_JOB_TIMEOUT_MS",
            &mut cfg.cron_job_timeout_ms,
            "ms",
        );
        parse_positive_u64("DB9_WORKER_ORPHAN_TIMEOUT_SEC", &mut cfg.orphan_timeout_sec);
        parse_u64_min(
            "DB9_WORKER_CLAIM_LEASE_MS",
            &mut cfg.claim_lease_ms,
            MIN_CLAIM_LEASE_MS,
            "ms",
        );
        parse_u64_min(
            "DB9_WORKER_EXECUTOR_LEASE_MS",
            &mut cfg.executor_lease_ms,
            MIN_EXECUTOR_LEASE_MS,
            "ms",
        );
        parse_positive_usize_u32capped("DB9_WORKER_GC_BATCH_SIZE", &mut cfg.gc_batch_size);
        parse_bool_var("DB9_AUTO_ANALYZE_ENABLED", &mut cfg.auto_analyze_enabled);
        parse_positive_u64(
            "DB9_AUTO_ANALYZE_THRESHOLD",
            &mut cfg.auto_analyze_threshold,
        );
        parse_u64_min(
            "DB9_WORKER_GC_INTERVAL_SEC",
            &mut cfg.gc_interval_sec,
            MIN_GC_INTERVAL_SEC,
            "s",
        );
        parse_u64_min(
            "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC",
            &mut cfg.hnsw_sweep_interval_sec,
            MIN_HNSW_SWEEP_INTERVAL_SEC,
            "s",
        );
        parse_u64_min(
            "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC",
            &mut cfg.registry_sweep_interval_sec,
            MIN_REGISTRY_SWEEP_INTERVAL_SEC,
            "s",
        );
        parse_u64_min(
            "DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC",
            &mut cfg.sweep_page_interval_sec,
            MIN_SWEEP_PAGE_INTERVAL_SEC,
            "s",
        );
        parse_u64_min(
            "DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC",
            &mut cfg.storage_scan_interval_sec,
            MIN_STORAGE_SCAN_INTERVAL_SEC,
            "s",
        );
        parse_u64_warn(
            "DB9_WORKER_STORAGE_SCAN_JITTER_SEC",
            &mut cfg.storage_scan_jitter_sec,
            "s",
        );
        parse_u64_warn(
            "DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS",
            &mut cfg.storage_scan_pd_rate_limit_ms,
            "ms",
        );
        parse_bool_var(
            "DB9_STORAGE_SCAN_DERIVED_ACTIVE",
            &mut cfg.storage_scan_derived_active,
        );
        parse_bool_var(
            "DB9_STORAGE_SCAN_DERIVED_SHADOW",
            &mut cfg.storage_scan_derived_shadow,
        );
        if let Ok(v) = env::var("DB9_STORAGE_SCAN_DERIVED_CAPACITY") {
            match v.parse::<u16>() {
                Ok(parsed) if parsed > 0 => cfg.storage_scan_derived_capacity = parsed,
                Ok(_) | Err(_) => {
                    tracing::warn!(
                        "DB9_STORAGE_SCAN_DERIVED_CAPACITY='{}' is not a valid positive u16; using default {}",
                        v,
                        cfg.storage_scan_derived_capacity
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE") {
            cfg.registry_reconcile_batch_size = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| n.min(u32::MAX as u64) as usize)
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE='{}' is not a valid positive integer; using default {}",
                        v,
                        cfg.registry_reconcile_batch_size
                    );
                    cfg.registry_reconcile_batch_size
                });
        }
        if let Ok(v) = env::var("DB9_WORKER_SYSTEM_KEYSPACE") {
            cfg.system_keyspace = v;
        }
        if let Ok(v) = env::var("DB9_GC_REGISTRY_KEYSPACE") {
            cfg.gc_registry_keyspace = v;
        }
        if let Ok(v) = env::var("DB9_DB_LIFECYCLE_KEYSPACE") {
            cfg.db_lifecycle_keyspace = v;
        }
        if let Ok(v) = env::var("DB9_DB_LIFECYCLE_MODE") {
            match DbLifecycleMode::parse(&v) {
                Some(mode) => cfg.db_lifecycle_mode = mode,
                None => {
                    tracing::warn!(
                        "DB9_DB_LIFECYCLE_MODE='{}' is not one of legacy|migrating|fenced; using legacy",
                        v
                    );
                    cfg.db_lifecycle_mode = DbLifecycleMode::Legacy;
                }
            }
        }
        parse_u64_min(
            "DB9_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC",
            &mut cfg.db_lifecycle_publish_interval_sec,
            MIN_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC,
            "s",
        );
        if let Ok(v) = env::var("DB9_BG_KEYSPACE") {
            cfg.bg_keyspace = v;
        }
        if let Ok(v) = env::var("DB9_GC_REGISTRY_MODE") {
            match GcRegistryMode::parse(&v) {
                Some(mode) => cfg.gc_registry_mode = mode,
                None => {
                    tracing::warn!(
                        "DB9_GC_REGISTRY_MODE='{}' is not one of legacy|migrating|new; using legacy",
                        v
                    );
                    cfg.gc_registry_mode = GcRegistryMode::Legacy;
                }
            }
        }

        // GC safepoint configuration
        parse_bool_var("DB9_GC_SAFEPOINT_ENABLED", &mut cfg.gc_safepoint_enabled);
        parse_u64_min(
            "DB9_GC_SAFEPOINT_INTERVAL_SEC",
            &mut cfg.gc_safepoint_interval_sec,
            MIN_GC_SAFEPOINT_INTERVAL_SEC,
            "s",
        );
        parse_u64_min(
            "DB9_GC_LIFE_TIME_SEC",
            &mut cfg.gc_life_time_sec,
            MIN_GC_LIFE_TIME_SEC,
            "s",
        );

        tracing::info!(
            "Worker config loaded: DB9_WORKER_ENABLED raw='{}', resolved={}",
            worker_enabled_raw.as_deref().unwrap_or("<unset>"),
            cfg.enabled
        );

        cfg
    }

    /// Minimum ratio of `gc_life_time_sec` to `gc_safepoint_interval_sec`.
    /// At N=3, the system survives 2 consecutive missed heartbeats before
    /// a row is considered stale by the advancer.
    const MIN_LIFE_TIME_TO_INTERVAL_RATIO: u64 = 3;

    /// Validate GC configuration invariants.
    pub fn validate_gc_config(&self) {
        let min_life_time = self
            .gc_safepoint_interval_sec
            .saturating_mul(Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO);
        if self.gc_life_time_sec < min_life_time {
            panic!(
                "UNSAFE CONFIG: DB9_GC_LIFE_TIME_SEC ({}) < DB9_GC_SAFEPOINT_INTERVAL_SEC ({}) * {} = {}. \
                 The GC retention window must be at least {}x the heartbeat interval \
                 so that {} consecutive missed heartbeats do not expose live transactions to GC.",
                self.gc_life_time_sec,
                self.gc_safepoint_interval_sec,
                Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO,
                min_life_time,
                Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO,
                Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO - 1,
            );
        }
    }

    pub fn domains_alias_legacy_system_keyspace(&self) -> bool {
        self.effective_gc_registry_keyspace() == self.system_keyspace
            && self.effective_db_lifecycle_keyspace() == self.system_keyspace
            && self.effective_bg_keyspace() == self.system_keyspace
    }

    pub fn validate_worker_domain_config(&self) -> Result<(), String> {
        let bg_keyspace = self.effective_bg_keyspace();
        let gc_keyspace = self.effective_gc_registry_keyspace();
        let lifecycle_keyspace = self.effective_db_lifecycle_keyspace();

        if bg_keyspace != self.system_keyspace {
            if bg_keyspace == gc_keyspace || bg_keyspace == lifecycle_keyspace {
                return Err(format!(
                    "DB9_BG_KEYSPACE='{}' must be physically separate from Core domains \
                     (gc='{}', lifecycle='{}'). _sys_bg is degradable and must not alias \
                     startup-critical metadata.",
                    bg_keyspace, gc_keyspace, lifecycle_keyspace
                ));
            }
            return Err(format!(
                "DB9_BG_KEYSPACE split is not enabled yet. Legacy V2 worker queue execution \
                 and DROP fencing still require DB9_BG_KEYSPACE to resolve to \
                 DB9_WORKER_SYSTEM_KEYSPACE='{}' (got '{}'). Keep DB9_BG_KEYSPACE unset \
                 until the BG ledger/dispatcher migration lands.",
                self.system_keyspace, bg_keyspace
            ));
        }

        match self.db_lifecycle_mode {
            DbLifecycleMode::Legacy if lifecycle_keyspace != self.system_keyspace => {
                return Err(format!(
                    "DB9_DB_LIFECYCLE_MODE=legacy requires DB9_DB_LIFECYCLE_KEYSPACE \
                     to resolve to DB9_WORKER_SYSTEM_KEYSPACE='{}' (got '{}'). \
                     Use DB9_DB_LIFECYCLE_MODE=migrating for the Phase 2 physical split.",
                    self.system_keyspace, lifecycle_keyspace
                ));
            }
            DbLifecycleMode::Migrating if lifecycle_keyspace == self.system_keyspace => {
                return Err(format!(
                    "DB9_DB_LIFECYCLE_MODE=migrating requires DB9_DB_LIFECYCLE_KEYSPACE \
                     to point at a distinct Core lifecycle keyspace, not the legacy \
                     worker keyspace '{}'.",
                    self.system_keyspace
                ));
            }
            DbLifecycleMode::Fenced => {
                return Err(
                    "DB9_DB_LIFECYCLE_MODE=fenced is not enabled yet. Incarnation-fenced \
                     lifecycle reads require a durable cluster/version gate proving every \
                     CREATE/DROP DATABASE writer allocates and stamps tenant incarnation \
                     before visibility; use migrating until that gate exists."
                        .to_string(),
                );
            }
            _ => {}
        }

        match self.gc_registry_mode {
            GcRegistryMode::New => Err(
                "DB9_GC_REGISTRY_MODE=new is not enabled yet. New-only GC registry reads \
                 require a durable cluster/version gate proving every live SQL-serving \
                 process publishes to the dedicated Core GC registry; use migrating until \
                 that gate exists."
                    .to_string(),
            ),
            GcRegistryMode::Legacy if gc_keyspace != self.system_keyspace => Err(format!(
                "DB9_GC_REGISTRY_MODE=legacy requires DB9_GC_REGISTRY_KEYSPACE to resolve \
                 to DB9_WORKER_SYSTEM_KEYSPACE='{}' (got '{}'). Use migrating for \
                 the GC registry migration.",
                self.system_keyspace, gc_keyspace
            )),
            GcRegistryMode::Migrating if gc_keyspace == self.system_keyspace => Err(format!(
                "DB9_GC_REGISTRY_MODE={} requires DB9_GC_REGISTRY_KEYSPACE to point \
                     at a distinct GC registry keyspace, not the legacy worker keyspace '{}'.",
                self.gc_registry_mode.as_str(),
                self.system_keyspace
            )),
            _ => Ok(()),
        }
    }

    pub fn effective_gc_registry_keyspace(&self) -> &str {
        self.effective_domain_keyspace(&self.gc_registry_keyspace)
    }

    pub fn effective_db_lifecycle_keyspace(&self) -> &str {
        self.effective_domain_keyspace(&self.db_lifecycle_keyspace)
    }

    pub fn effective_bg_keyspace(&self) -> &str {
        self.effective_domain_keyspace(&self.bg_keyspace)
    }

    fn effective_domain_keyspace<'a>(&'a self, domain_keyspace: &'a str) -> &'a str {
        if domain_keyspace.is_empty() {
            &self.system_keyspace
        } else {
            domain_keyspace
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::OnceLock;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn from_env_uses_defaults_when_vars_not_set() {
        let _guard = test_lock().lock();

        let keys = [
            "DB9_WORKER_ENABLED",
            "DB9_WORKER_POLL_MS",
            "DB9_WORKER_MAX_CONCURRENT_JOBS",
            "DB9_WORKER_ID",
            "DB9_WORKER_STATEMENT_TIMEOUT_MS",
            "DB9_CRON_JOB_TIMEOUT_MS",
            "DB9_WORKER_ORPHAN_TIMEOUT_SEC",
            "DB9_WORKER_CLAIM_LEASE_MS",
            "DB9_WORKER_EXECUTOR_LEASE_MS",
            "DB9_WORKER_GC_BATCH_SIZE",
            "DB9_AUTO_ANALYZE_ENABLED",
            "DB9_AUTO_ANALYZE_THRESHOLD",
            "DB9_WORKER_GC_INTERVAL_SEC",
            "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC",
            "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC",
            "DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC",
            "DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC",
            "DB9_WORKER_STORAGE_SCAN_JITTER_SEC",
            "DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS",
            "DB9_STORAGE_SCAN_DERIVED_ACTIVE",
            "DB9_STORAGE_SCAN_DERIVED_SHADOW",
            "DB9_STORAGE_SCAN_DERIVED_CAPACITY",
            "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE",
            "DB9_WORKER_SYSTEM_KEYSPACE",
            "DB9_GC_REGISTRY_KEYSPACE",
            "DB9_DB_LIFECYCLE_KEYSPACE",
            "DB9_DB_LIFECYCLE_MODE",
            "DB9_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC",
            "DB9_BG_KEYSPACE",
            "DB9_GC_REGISTRY_MODE",
        ];

        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| (k.to_string(), env::var(k).ok()))
            .collect();

        for key in &keys {
            unsafe {
                env::remove_var(key);
            }
        }

        let cfg = WorkerConfig::from_env();
        assert!(cfg.enabled);
        assert_eq!(cfg.poll_ms, DEFAULT_POLL_MS);
        assert_eq!(cfg.max_concurrent_jobs, DEFAULT_MAX_CONCURRENT_JOBS);
        assert!(cfg.worker_id.contains(':'));
        assert_eq!(cfg.statement_timeout_ms, DEFAULT_STATEMENT_TIMEOUT_MS);
        assert_eq!(cfg.cron_job_timeout_ms, DEFAULT_CRON_JOB_TIMEOUT_MS);
        assert_eq!(cfg.orphan_timeout_sec, DEFAULT_ORPHAN_TIMEOUT_SEC);
        assert_eq!(cfg.claim_lease_ms, DEFAULT_CLAIM_LEASE_MS);
        assert_eq!(cfg.executor_lease_ms, DEFAULT_EXECUTOR_LEASE_MS);
        assert_eq!(cfg.gc_batch_size, DEFAULT_GC_BATCH_SIZE);
        assert!(cfg.auto_analyze_enabled);
        assert_eq!(cfg.auto_analyze_threshold, DEFAULT_AUTO_ANALYZE_THRESHOLD);
        assert_eq!(cfg.gc_interval_sec, DEFAULT_GC_INTERVAL_SEC);
        assert_eq!(cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC);
        assert_eq!(
            cfg.registry_sweep_interval_sec,
            DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC
        );
        assert_eq!(cfg.sweep_page_interval_sec, DEFAULT_SWEEP_PAGE_INTERVAL_SEC);
        assert_eq!(
            cfg.storage_scan_interval_sec,
            DEFAULT_STORAGE_SCAN_INTERVAL_SEC
        );
        assert_eq!(cfg.storage_scan_jitter_sec, DEFAULT_STORAGE_SCAN_JITTER_SEC);
        assert_eq!(
            cfg.storage_scan_pd_rate_limit_ms,
            DEFAULT_STORAGE_SCAN_PD_RATE_LIMIT_MS
        );
        assert_eq!(
            cfg.storage_scan_derived_active,
            DEFAULT_STORAGE_SCAN_DERIVED_ACTIVE
        );
        assert_eq!(
            cfg.storage_scan_derived_shadow,
            DEFAULT_STORAGE_SCAN_DERIVED_SHADOW
        );
        assert_eq!(
            cfg.storage_scan_derived_capacity,
            DEFAULT_STORAGE_SCAN_DERIVED_CAPACITY
        );
        assert_eq!(
            cfg.registry_reconcile_batch_size,
            DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE
        );
        assert_eq!(cfg.system_keyspace, DEFAULT_SYSTEM_KEYSPACE);
        assert_eq!(cfg.gc_registry_keyspace, DEFAULT_GC_REGISTRY_KEYSPACE);
        assert_eq!(cfg.db_lifecycle_keyspace, DEFAULT_DB_LIFECYCLE_KEYSPACE);
        assert_eq!(cfg.db_lifecycle_mode, DbLifecycleMode::Legacy);
        assert_eq!(
            cfg.db_lifecycle_publish_interval_sec,
            DEFAULT_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC
        );
        assert_eq!(cfg.bg_keyspace, DEFAULT_BG_KEYSPACE);
        assert_eq!(cfg.gc_registry_mode, GcRegistryMode::Legacy);
        assert_eq!(
            cfg.effective_gc_registry_keyspace(),
            DEFAULT_SYSTEM_KEYSPACE
        );
        assert_eq!(
            cfg.effective_db_lifecycle_keyspace(),
            DEFAULT_SYSTEM_KEYSPACE
        );
        assert_eq!(cfg.effective_bg_keyspace(), DEFAULT_SYSTEM_KEYSPACE);
        assert!(cfg.domains_alias_legacy_system_keyspace());

        for (key, value) in saved {
            match value {
                Some(v) => unsafe { env::set_var(key, v) },
                None => unsafe { env::remove_var(key) },
            }
        }
    }

    #[test]
    fn from_env_splits_domain_keyspaces_with_legacy_alias_default() {
        let _guard = test_lock().lock();

        let keys = [
            "DB9_WORKER_SYSTEM_KEYSPACE",
            "DB9_GC_REGISTRY_KEYSPACE",
            "DB9_DB_LIFECYCLE_KEYSPACE",
            "DB9_DB_LIFECYCLE_MODE",
            "DB9_BG_KEYSPACE",
            "DB9_GC_REGISTRY_MODE",
        ];
        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| (k.to_string(), env::var(k).ok()))
            .collect();

        unsafe {
            env::set_var("DB9_WORKER_SYSTEM_KEYSPACE", "_sys_worker_custom");
            env::set_var("DB9_GC_REGISTRY_KEYSPACE", "_sys_gc_registry_custom");
            env::set_var("DB9_DB_LIFECYCLE_KEYSPACE", "_sys_db_lifecycle_custom");
            env::set_var("DB9_DB_LIFECYCLE_MODE", "migrating");
            env::set_var("DB9_BG_KEYSPACE", "_sys_bg_custom");
            env::set_var("DB9_GC_REGISTRY_MODE", "migrating");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.system_keyspace, "_sys_worker_custom");
        assert_eq!(cfg.gc_registry_keyspace, "_sys_gc_registry_custom");
        assert_eq!(cfg.db_lifecycle_keyspace, "_sys_db_lifecycle_custom");
        assert_eq!(cfg.db_lifecycle_mode, DbLifecycleMode::Migrating);
        assert_eq!(cfg.bg_keyspace, "_sys_bg_custom");
        assert_eq!(
            cfg.effective_gc_registry_keyspace(),
            "_sys_gc_registry_custom"
        );
        assert_eq!(
            cfg.effective_db_lifecycle_keyspace(),
            "_sys_db_lifecycle_custom"
        );
        assert_eq!(cfg.effective_bg_keyspace(), "_sys_bg_custom");
        assert_eq!(cfg.gc_registry_mode, GcRegistryMode::Migrating);
        assert!(!cfg.domains_alias_legacy_system_keyspace());

        for (key, value) in saved {
            match value {
                Some(v) => unsafe { env::set_var(key, v) },
                None => unsafe { env::remove_var(key) },
            }
        }
    }

    #[test]
    fn legacy_system_keyspace_override_keeps_domains_aliased_by_default() {
        let cfg = WorkerConfig {
            system_keyspace: "_sys_worker_test_alias".to_string(),
            ..Default::default()
        };

        assert_eq!(
            cfg.effective_gc_registry_keyspace(),
            "_sys_worker_test_alias"
        );
        assert_eq!(
            cfg.effective_db_lifecycle_keyspace(),
            "_sys_worker_test_alias"
        );
        assert_eq!(cfg.effective_bg_keyspace(), "_sys_worker_test_alias");
        assert!(cfg.domains_alias_legacy_system_keyspace());
        assert!(cfg.validate_worker_domain_config().is_ok());
    }

    #[test]
    fn explicit_default_keyspace_override_is_not_treated_as_alias_sentinel() {
        let cfg = WorkerConfig {
            system_keyspace: "_sys_bg_custom".to_string(),
            gc_registry_keyspace: DEFAULT_SYSTEM_KEYSPACE.to_string(),
            db_lifecycle_keyspace: "_sys_bg_custom".to_string(),
            bg_keyspace: "_sys_bg_custom".to_string(),
            ..Default::default()
        };

        assert_eq!(
            cfg.effective_gc_registry_keyspace(),
            DEFAULT_SYSTEM_KEYSPACE
        );
        assert_eq!(cfg.effective_db_lifecycle_keyspace(), "_sys_bg_custom");
        assert_eq!(cfg.effective_bg_keyspace(), "_sys_bg_custom");
        assert!(!cfg.domains_alias_legacy_system_keyspace());
    }

    #[test]
    fn worker_domain_validation_rejects_gc_split_without_migration_mode() {
        let cfg = WorkerConfig {
            gc_registry_keyspace: "_sys_gc_registry_test".to_string(),
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("legacy mode must reject GC split");
        assert!(err.contains("DB9_GC_REGISTRY_MODE=legacy"));
    }

    #[test]
    fn worker_domain_validation_allows_gc_migrating_split_only() {
        let cfg = WorkerConfig {
            gc_registry_keyspace: "_sys_gc_registry_test".to_string(),
            gc_registry_mode: GcRegistryMode::Migrating,
            ..Default::default()
        };

        assert!(cfg.validate_worker_domain_config().is_ok());
    }

    #[test]
    fn worker_domain_validation_rejects_gc_new_only_until_cluster_gate() {
        let cfg = WorkerConfig {
            gc_registry_keyspace: "_sys_gc_registry_test".to_string(),
            gc_registry_mode: GcRegistryMode::New,
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("new-only mode must wait for the cluster/version gate");
        assert!(err.contains("DB9_GC_REGISTRY_MODE=new is not enabled yet"));
        assert!(err.contains("durable cluster/version gate"));
    }

    #[test]
    fn worker_domain_validation_rejects_gc_new_alias_to_legacy_worker() {
        let cfg = WorkerConfig {
            gc_registry_mode: GcRegistryMode::New,
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("new-only mode must wait for the cluster/version gate");
        assert!(err.contains("DB9_GC_REGISTRY_MODE=new is not enabled yet"));
        assert!(err.contains("durable cluster/version gate"));
    }

    #[test]
    fn worker_domain_validation_rejects_bg_split_until_bg_ledger_lands() {
        let cfg = WorkerConfig {
            bg_keyspace: "_sys_bg_test".to_string(),
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("BG split must wait for the BG ledger/dispatcher migration");
        assert!(err.contains("DB9_BG_KEYSPACE split is not enabled yet"));
        assert!(err.contains("Legacy V2 worker queue execution"));
        assert!(err.contains("BG ledger/dispatcher migration"));
    }

    #[test]
    fn worker_domain_validation_rejects_bg_alias_to_split_core_domain() {
        let cfg = WorkerConfig {
            bg_keyspace: "_sys_gc_registry_test".to_string(),
            gc_registry_keyspace: "_sys_gc_registry_test".to_string(),
            gc_registry_mode: GcRegistryMode::Migrating,
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("BG must not alias a split Core keyspace");
        assert!(err.contains("DB9_BG_KEYSPACE"));
        assert!(err.contains("physically separate from Core domains"));
    }

    #[test]
    fn worker_domain_validation_rejects_lifecycle_split_without_migrating_mode() {
        let cfg = WorkerConfig {
            db_lifecycle_keyspace: "_sys_db_lifecycle_test".to_string(),
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("DB lifecycle split must opt into migrating mode");
        assert!(err.contains("DB9_DB_LIFECYCLE_MODE=legacy"));
    }

    #[test]
    fn worker_domain_validation_allows_lifecycle_migrating_split() {
        let cfg = WorkerConfig {
            db_lifecycle_keyspace: "_sys_db_lifecycle_test".to_string(),
            db_lifecycle_mode: DbLifecycleMode::Migrating,
            ..Default::default()
        };

        assert!(cfg.validate_worker_domain_config().is_ok());
    }

    #[test]
    fn worker_domain_validation_rejects_lifecycle_migrating_alias() {
        let cfg = WorkerConfig {
            db_lifecycle_mode: DbLifecycleMode::Migrating,
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("migrating mode must physically split lifecycle keyspace");
        assert!(err.contains("DB9_DB_LIFECYCLE_MODE=migrating"));
        assert!(err.contains("distinct Core lifecycle keyspace"));
    }

    #[test]
    fn worker_domain_validation_rejects_lifecycle_fenced_until_cluster_gate() {
        let cfg = WorkerConfig {
            db_lifecycle_keyspace: "_sys_db_lifecycle_test".to_string(),
            db_lifecycle_mode: DbLifecycleMode::Fenced,
            ..Default::default()
        };

        let err = cfg
            .validate_worker_domain_config()
            .expect_err("fenced lifecycle mode must wait for cluster-version gate");
        assert!(err.contains("DB9_DB_LIFECYCLE_MODE=fenced"));
        assert!(err.contains("durable cluster/version gate"));
    }

    #[test]
    fn from_env_invalid_gc_registry_mode_falls_back_to_legacy() {
        let _guard = test_lock().lock();

        let key = "DB9_GC_REGISTRY_MODE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "split-now");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_registry_mode, GcRegistryMode::Legacy);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_clamps_gc_batch_size_to_u32_max() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_GC_BATCH_SIZE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, (u64::from(u32::MAX) + 1).to_string());
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_batch_size, u32::MAX as usize);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_poll_ms_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_POLL_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "5000");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.poll_ms, 5_000);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_poll_ms_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_POLL_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "99");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.poll_ms, DEFAULT_POLL_MS);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_ignores_non_integer_poll_ms() {
        let _lock = test_lock().lock();
        unsafe { env::set_var("DB9_WORKER_POLL_MS", "not_a_number") };
        let cfg = WorkerConfig::from_env();
        unsafe { env::remove_var("DB9_WORKER_POLL_MS") };
        assert_eq!(
            cfg.poll_ms, DEFAULT_POLL_MS,
            "non-integer should fall back to default"
        );
    }
    #[test]
    fn test_parse_bool_true_variants() {
        for input in &["1", "true", "t", "yes", "y", "on"] {
            assert_eq!(parse_bool(input), Some(true), "input: {}", input);
        }
        for input in &[" TRUE ", " Yes ", " ON "] {
            assert_eq!(parse_bool(input), Some(true), "trimmed input: {}", input);
        }
    }

    #[test]
    fn test_parse_bool_false_variants() {
        for input in &["0", "false", "f", "no", "n", "off"] {
            assert_eq!(parse_bool(input), Some(false), "input: {}", input);
        }
        for input in &[" FALSE ", " No ", " OFF "] {
            assert_eq!(parse_bool(input), Some(false), "trimmed input: {}", input);
        }
    }

    #[test]
    fn test_parse_bool_invalid() {
        for input in &["maybe", "", "2", "yep", "nope", "enabled", "live=false"] {
            assert_eq!(parse_bool(input), None, "input: {}", input);
        }
    }

    #[test]
    fn from_env_applies_worker_enabled_false() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_ENABLED";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "false");
        }

        let cfg = WorkerConfig::from_env();
        assert!(!cfg.enabled);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_invalid_worker_enabled_fails_closed() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_ENABLED";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "live=false");
        }

        let cfg = WorkerConfig::from_env();
        assert!(
            !cfg.enabled,
            "invalid DB9_WORKER_ENABLED must not leave the worker enabled"
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn test_config_default_values() {
        let cfg = WorkerConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.poll_ms, 60_000);
        assert_eq!(cfg.max_concurrent_jobs, 32);
        assert!(uuid::Uuid::parse_str(&cfg.gc_instance_id).is_ok());
        assert_eq!(cfg.statement_timeout_ms, 300_000);
        assert_eq!(cfg.cron_job_timeout_ms, 1_800_000);
        assert_eq!(cfg.orphan_timeout_sec, 300);
        assert_eq!(cfg.claim_lease_ms, 60_000);
        assert_eq!(cfg.gc_batch_size, 100);
        assert!(cfg.auto_analyze_enabled);
        assert_eq!(cfg.auto_analyze_threshold, 50);
        assert_eq!(cfg.gc_interval_sec, 600);
        assert_eq!(cfg.hnsw_sweep_interval_sec, 600);
        assert_eq!(cfg.registry_sweep_interval_sec, 60);
        assert_eq!(cfg.sweep_page_interval_sec, 30);
        assert_eq!(cfg.storage_scan_interval_sec, 21_600);
        assert_eq!(cfg.storage_scan_jitter_sec, 300);
        assert_eq!(cfg.storage_scan_pd_rate_limit_ms, 100);
        assert_eq!(cfg.registry_reconcile_batch_size, 32);
        assert_eq!(cfg.db_lifecycle_publish_interval_sec, 300);
        assert_eq!(cfg.system_keyspace, "_sys_worker");
        assert!(cfg.gc_registry_keyspace.is_empty());
        assert!(cfg.bg_keyspace.is_empty());
        assert_eq!(cfg.gc_registry_mode, GcRegistryMode::Legacy);
        assert_eq!(cfg.effective_gc_registry_keyspace(), "_sys_worker");
        assert_eq!(cfg.effective_bg_keyspace(), "_sys_worker");
        assert!(cfg.domains_alias_legacy_system_keyspace());
    }

    #[test]
    fn from_env_applies_db_lifecycle_publish_interval() {
        let _guard = test_lock().lock();

        let key = "DB9_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "45");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.db_lifecycle_publish_interval_sec, 45);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_db_lifecycle_publish_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "29");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.db_lifecycle_publish_interval_sec,
            DEFAULT_DB_LIFECYCLE_PUBLISH_INTERVAL_SEC
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_storage_scan_jitter_and_pd_rate_limit() {
        let _guard = test_lock().lock();

        let jitter_key = "DB9_WORKER_STORAGE_SCAN_JITTER_SEC";
        let rate_key = "DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS";
        let jitter_saved = env::var(jitter_key).ok();
        let rate_saved = env::var(rate_key).ok();

        unsafe {
            env::set_var(jitter_key, "17");
            env::set_var(rate_key, "250");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.storage_scan_jitter_sec, 17);
        assert_eq!(cfg.storage_scan_pd_rate_limit_ms, 250);

        match jitter_saved {
            Some(v) => unsafe { env::set_var(jitter_key, v) },
            None => unsafe { env::remove_var(jitter_key) },
        }
        match rate_saved {
            Some(v) => unsafe { env::set_var(rate_key, v) },
            None => unsafe { env::remove_var(rate_key) },
        }
    }

    #[test]
    fn from_env_applies_storage_scan_derived_flags() {
        let _guard = test_lock().lock();

        let active_key = "DB9_STORAGE_SCAN_DERIVED_ACTIVE";
        let shadow_key = "DB9_STORAGE_SCAN_DERIVED_SHADOW";
        let capacity_key = "DB9_STORAGE_SCAN_DERIVED_CAPACITY";
        let saved = [
            (active_key, env::var(active_key).ok()),
            (shadow_key, env::var(shadow_key).ok()),
            (capacity_key, env::var(capacity_key).ok()),
        ];

        unsafe {
            env::set_var(active_key, "true");
            env::set_var(shadow_key, "on");
            env::set_var(capacity_key, "3");
        }

        let cfg = WorkerConfig::from_env();
        assert!(cfg.storage_scan_derived_active);
        assert!(cfg.storage_scan_derived_shadow);
        assert_eq!(cfg.storage_scan_derived_capacity, 3);

        for (key, value) in saved {
            match value {
                Some(v) => unsafe { env::set_var(key, v) },
                None => unsafe { env::remove_var(key) },
            }
        }
    }

    #[test]
    fn from_env_applies_claim_lease_ms_and_clamps_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_CLAIM_LEASE_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "120000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.claim_lease_ms, 120_000);

        // Below the minimum: keep the default (sub-second/too-small leases are
        // forbidden by design §K4).
        unsafe {
            env::set_var(key, "1000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.claim_lease_ms, DEFAULT_CLAIM_LEASE_MS);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_executor_lease_ms_and_clamps_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_EXECUTOR_LEASE_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "45000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.executor_lease_ms, 45_000);

        unsafe {
            env::set_var(key, "5000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.executor_lease_ms, DEFAULT_EXECUTOR_LEASE_MS);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_registry_reconcile_batch_size() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "7");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.registry_reconcile_batch_size, 7);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_ignores_zero_registry_reconcile_batch_size() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.registry_reconcile_batch_size,
            DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_hnsw_sweep_interval_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "120");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.hnsw_sweep_interval_sec, 120);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_hnsw_sweep_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "29");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_ignores_non_integer_hnsw_sweep_interval() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "not_a_number");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC,
            "non-integer should fall back to default"
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_registry_sweep_interval_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "7");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.registry_sweep_interval_sec, 7);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_registry_sweep_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.registry_sweep_interval_sec,
            DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_gc_interval_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_GC_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "120");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, 120);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_gc_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_GC_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "29");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, DEFAULT_GC_INTERVAL_SEC);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn gc_hnsw_and_registry_sweep_intervals_are_independent() {
        let _guard = test_lock().lock();

        let gc_key = "DB9_WORKER_GC_INTERVAL_SEC";
        let hnsw_key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let registry_key = "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC";
        let gc_saved = env::var(gc_key).ok();
        let hnsw_saved = env::var(hnsw_key).ok();
        let registry_saved = env::var(registry_key).ok();

        unsafe {
            env::set_var(gc_key, "60");
            env::set_var(hnsw_key, "300");
            env::set_var(registry_key, "9");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, 60);
        assert_eq!(cfg.hnsw_sweep_interval_sec, 300);
        assert_eq!(cfg.registry_sweep_interval_sec, 9);

        match gc_saved {
            Some(v) => unsafe { env::set_var(gc_key, v) },
            None => unsafe { env::remove_var(gc_key) },
        }
        match hnsw_saved {
            Some(v) => unsafe { env::set_var(hnsw_key, v) },
            None => unsafe { env::remove_var(hnsw_key) },
        }
        match registry_saved {
            Some(v) => unsafe { env::set_var(registry_key, v) },
            None => unsafe { env::remove_var(registry_key) },
        }
    }

    #[test]
    fn from_env_preserves_zero_worker_timeouts() {
        let _guard = test_lock().lock();

        let stmt_key = "DB9_WORKER_STATEMENT_TIMEOUT_MS";
        let cron_key = "DB9_CRON_JOB_TIMEOUT_MS";
        let stmt_saved = env::var(stmt_key).ok();
        let cron_saved = env::var(cron_key).ok();

        unsafe {
            env::set_var(stmt_key, "0");
            env::set_var(cron_key, "0");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 0);
        assert_eq!(cfg.cron_job_timeout_ms, 0);

        match stmt_saved {
            Some(v) => unsafe { env::set_var(stmt_key, v) },
            None => unsafe { env::remove_var(stmt_key) },
        }
        match cron_saved {
            Some(v) => unsafe { env::set_var(cron_key, v) },
            None => unsafe { env::remove_var(cron_key) },
        }
    }

    // --- GC safepoint config validation tests ---

    #[test]
    fn gc_config_defaults_pass_validation() {
        let cfg = WorkerConfig::default();
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    fn gc_config_allows_zero_worker_timeouts_when_transactions_are_tracked() {
        let cfg = WorkerConfig {
            enabled: true,
            cron_job_timeout_ms: 0,
            statement_timeout_ms: 0,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_interval_ge_life_time() {
        let cfg = WorkerConfig {
            gc_safepoint_enabled: true,
            gc_safepoint_interval_sec: 86400,
            gc_life_time_sec: 86400, // interval == life_time
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    fn gc_config_advancer_only_node_passes_with_short_life_time() {
        let cfg = WorkerConfig {
            enabled: false,
            gc_safepoint_enabled: true,
            gc_life_time_sec: 3600, // 1h — safe for advancer-only
            gc_safepoint_interval_sec: 300,
            ..Default::default()
        };
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_interval_ge_life_time_when_advancer_disabled() {
        let cfg = WorkerConfig {
            gc_safepoint_enabled: false,
            gc_safepoint_interval_sec: 86400,
            gc_life_time_sec: 86400,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    fn gc_config_default_values() {
        let cfg = WorkerConfig::default();
        assert!(cfg.gc_safepoint_enabled);
        assert_eq!(cfg.gc_safepoint_interval_sec, 300);
        assert_eq!(cfg.gc_life_time_sec, 86400);
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_single_missed_heartbeat_fatal_edge() {
        // interval=599, life_time=600: a single missed publish makes the
        // row stale after just 1 extra second.
        let cfg = WorkerConfig {
            gc_safepoint_interval_sec: 599,
            gc_life_time_sec: 600,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    fn gc_config_accepts_exactly_3x_ratio() {
        let cfg = WorkerConfig {
            gc_safepoint_interval_sec: 200,
            gc_life_time_sec: 600, // exactly 3x
            ..Default::default()
        };
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_just_below_3x_ratio() {
        let cfg = WorkerConfig {
            gc_safepoint_interval_sec: 201,
            gc_life_time_sec: 600, // 600 < 201*3 = 603
            ..Default::default()
        };
        cfg.validate_gc_config();
    }
}
