use std::sync::atomic::{AtomicU64, Ordering};

use crate::worker::types::TaskType;

/// Simple in-memory counters for worker engine observability.
/// All counters are atomic and thread-safe.
pub struct WorkerMetrics {
    /// Tasks executed successfully (by type)
    pub tasks_executed_ok: AtomicU64,
    pub cron_executed_ok: AtomicU64,
    pub async_trigger_executed_ok: AtomicU64,
    pub auto_analyze_executed_ok: AtomicU64,
    pub bg_ddl_executed_ok: AtomicU64,
    pub bg_sql_executed_ok: AtomicU64,
    pub hnsw_merge_executed_ok: AtomicU64,

    /// Tasks executed with errors (by type)
    pub tasks_executed_err: AtomicU64,
    pub cron_executed_err: AtomicU64,
    pub async_trigger_executed_err: AtomicU64,
    pub auto_analyze_executed_err: AtomicU64,
    pub bg_ddl_executed_err: AtomicU64,
    pub bg_sql_executed_err: AtomicU64,
    pub hnsw_merge_executed_err: AtomicU64,

    /// Claim attempts and successes
    pub claim_attempts: AtomicU64,
    pub claim_successes: AtomicU64,

    /// Gauges (sampled on each tick)
    pub last_tick_queue_depth: AtomicU64,
    pub last_tick_active_jobs: AtomicU64,

    /// Gauge: number of HNSW indexes with pending deltas, sampled per sweep.
    /// Updated via store() (overwrite), NOT fetch_add.
    pub hnsw_pending_indexes_observed: AtomicU64,

    /// Counter: cumulative merge tasks successfully enqueued by sweeper.
    pub hnsw_sweep_enqueued: AtomicU64,

    /// Counter: cumulative deltas applied during query-time HNSW scans.
    pub hnsw_scan_deltas_applied: AtomicU64,

    /// Counter: cumulative sweeper enqueue failures (system store write errors).
    pub hnsw_sweep_enqueue_errors: AtomicU64,

    /// Gauge: last successfully reported TiKV GC safepoint (TSO version).
    pub gc_safepoint_last_version: AtomicU64,
    /// Counter: successful GC safepoint advancements.
    pub gc_safepoint_advance_ok: AtomicU64,
    /// Counter: failed GC safepoint advancement attempts.
    pub gc_safepoint_advance_err: AtomicU64,
}

impl WorkerMetrics {
    /// Create a new metrics instance with all counters initialized to zero.
    pub fn new() -> Self {
        Self {
            tasks_executed_ok: AtomicU64::new(0),
            cron_executed_ok: AtomicU64::new(0),
            async_trigger_executed_ok: AtomicU64::new(0),
            auto_analyze_executed_ok: AtomicU64::new(0),
            bg_ddl_executed_ok: AtomicU64::new(0),
            bg_sql_executed_ok: AtomicU64::new(0),
            hnsw_merge_executed_ok: AtomicU64::new(0),

            tasks_executed_err: AtomicU64::new(0),
            cron_executed_err: AtomicU64::new(0),
            async_trigger_executed_err: AtomicU64::new(0),
            auto_analyze_executed_err: AtomicU64::new(0),
            bg_ddl_executed_err: AtomicU64::new(0),
            bg_sql_executed_err: AtomicU64::new(0),
            hnsw_merge_executed_err: AtomicU64::new(0),

            claim_attempts: AtomicU64::new(0),
            claim_successes: AtomicU64::new(0),

            last_tick_queue_depth: AtomicU64::new(0),
            last_tick_active_jobs: AtomicU64::new(0),

            hnsw_pending_indexes_observed: AtomicU64::new(0),
            hnsw_sweep_enqueued: AtomicU64::new(0),
            hnsw_scan_deltas_applied: AtomicU64::new(0),
            hnsw_sweep_enqueue_errors: AtomicU64::new(0),

            gc_safepoint_last_version: AtomicU64::new(0),
            gc_safepoint_advance_ok: AtomicU64::new(0),
            gc_safepoint_advance_err: AtomicU64::new(0),
        }
    }

    /// Record a task execution result (success or error) by task type.
    pub fn record_task_result(&self, task_type: TaskType, success: bool) {
        if success {
            self.tasks_executed_ok.fetch_add(1, Ordering::Relaxed);
            match task_type {
                TaskType::Cron => self.cron_executed_ok.fetch_add(1, Ordering::Relaxed),
                TaskType::AsyncTrigger => self
                    .async_trigger_executed_ok
                    .fetch_add(1, Ordering::Relaxed),
                TaskType::AutoAnalyze => self
                    .auto_analyze_executed_ok
                    .fetch_add(1, Ordering::Relaxed),
                TaskType::BgDdl => self.bg_ddl_executed_ok.fetch_add(1, Ordering::Relaxed),
                TaskType::BgSql => self.bg_sql_executed_ok.fetch_add(1, Ordering::Relaxed),
                TaskType::HnswMerge => self.hnsw_merge_executed_ok.fetch_add(1, Ordering::Relaxed),
                TaskType::StorageSizeScan => 0,
            };
        } else {
            self.tasks_executed_err.fetch_add(1, Ordering::Relaxed);
            match task_type {
                TaskType::Cron => self.cron_executed_err.fetch_add(1, Ordering::Relaxed),
                TaskType::AsyncTrigger => self
                    .async_trigger_executed_err
                    .fetch_add(1, Ordering::Relaxed),
                TaskType::AutoAnalyze => self
                    .auto_analyze_executed_err
                    .fetch_add(1, Ordering::Relaxed),
                TaskType::BgDdl => self.bg_ddl_executed_err.fetch_add(1, Ordering::Relaxed),
                TaskType::BgSql => self.bg_sql_executed_err.fetch_add(1, Ordering::Relaxed),
                TaskType::HnswMerge => self.hnsw_merge_executed_err.fetch_add(1, Ordering::Relaxed),
                TaskType::StorageSizeScan => 0,
            };
        }
    }

    /// Record a claim attempt and whether it succeeded.
    pub fn record_claim(&self, success: bool) {
        self.claim_attempts.fetch_add(1, Ordering::Relaxed);
        if success {
            self.claim_successes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sample queue depth and active jobs on each tick.
    pub fn sample_tick(&self, queue_depth: u64, active_jobs: u32) {
        self.last_tick_queue_depth
            .store(queue_depth, Ordering::Relaxed);
        self.last_tick_active_jobs
            .store(active_jobs as u64, Ordering::Relaxed);
    }
}

impl Default for WorkerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_new() {
        let m = WorkerMetrics::new();
        assert_eq!(m.tasks_executed_ok.load(Ordering::Relaxed), 0);
        assert_eq!(m.tasks_executed_err.load(Ordering::Relaxed), 0);
        assert_eq!(m.claim_attempts.load(Ordering::Relaxed), 0);
        assert_eq!(m.claim_successes.load(Ordering::Relaxed), 0);
        assert_eq!(m.hnsw_pending_indexes_observed.load(Ordering::Relaxed), 0);
        assert_eq!(m.hnsw_sweep_enqueued.load(Ordering::Relaxed), 0);
        assert_eq!(m.hnsw_scan_deltas_applied.load(Ordering::Relaxed), 0);
        assert_eq!(m.hnsw_sweep_enqueue_errors.load(Ordering::Relaxed), 0);
        assert_eq!(m.gc_safepoint_last_version.load(Ordering::Relaxed), 0);
        assert_eq!(m.gc_safepoint_advance_ok.load(Ordering::Relaxed), 0);
        assert_eq!(m.gc_safepoint_advance_err.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_record_task_result_success() {
        let m = WorkerMetrics::new();
        m.record_task_result(TaskType::Cron, true);
        assert_eq!(m.tasks_executed_ok.load(Ordering::Relaxed), 1);
        assert_eq!(m.cron_executed_ok.load(Ordering::Relaxed), 1);
        assert_eq!(m.tasks_executed_err.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_record_task_result_error() {
        let m = WorkerMetrics::new();
        m.record_task_result(TaskType::BgDdl, false);
        assert_eq!(m.tasks_executed_ok.load(Ordering::Relaxed), 0);
        assert_eq!(m.tasks_executed_err.load(Ordering::Relaxed), 1);
        assert_eq!(m.bg_ddl_executed_err.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_record_claim() {
        let m = WorkerMetrics::new();
        m.record_claim(true);
        m.record_claim(true);
        m.record_claim(false);
        assert_eq!(m.claim_attempts.load(Ordering::Relaxed), 3);
        assert_eq!(m.claim_successes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_sample_tick() {
        let m = WorkerMetrics::new();
        m.sample_tick(42, 5);
        assert_eq!(m.last_tick_queue_depth.load(Ordering::Relaxed), 42);
        assert_eq!(m.last_tick_active_jobs.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn test_hnsw_sweep_metrics_gauge_and_counter_semantics() {
        let m = WorkerMetrics::new();

        // Gauge semantics: overwrite on each sweep sample.
        m.hnsw_pending_indexes_observed.store(7, Ordering::Relaxed);
        m.hnsw_pending_indexes_observed.store(2, Ordering::Relaxed);
        assert_eq!(m.hnsw_pending_indexes_observed.load(Ordering::Relaxed), 2);

        // Counter semantics: cumulative across sweeps.
        m.hnsw_sweep_enqueued.fetch_add(3, Ordering::Relaxed);
        m.hnsw_sweep_enqueued.fetch_add(5, Ordering::Relaxed);
        assert_eq!(m.hnsw_sweep_enqueued.load(Ordering::Relaxed), 8);

        m.hnsw_sweep_enqueue_errors.fetch_add(1, Ordering::Relaxed);
        m.hnsw_sweep_enqueue_errors.fetch_add(2, Ordering::Relaxed);
        assert_eq!(m.hnsw_sweep_enqueue_errors.load(Ordering::Relaxed), 3);
    }
}
