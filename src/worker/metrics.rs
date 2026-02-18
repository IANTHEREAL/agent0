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

    /// Tasks executed with errors (by type)
    pub tasks_executed_err: AtomicU64,
    pub cron_executed_err: AtomicU64,
    pub async_trigger_executed_err: AtomicU64,
    pub auto_analyze_executed_err: AtomicU64,
    pub bg_ddl_executed_err: AtomicU64,
    pub bg_sql_executed_err: AtomicU64,

    /// Claim attempts and successes
    pub claim_attempts: AtomicU64,
    pub claim_successes: AtomicU64,

    /// Gauges (sampled on each tick)
    pub last_tick_queue_depth: AtomicU64,
    pub last_tick_active_jobs: AtomicU64,
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

            tasks_executed_err: AtomicU64::new(0),
            cron_executed_err: AtomicU64::new(0),
            async_trigger_executed_err: AtomicU64::new(0),
            auto_analyze_executed_err: AtomicU64::new(0),
            bg_ddl_executed_err: AtomicU64::new(0),
            bg_sql_executed_err: AtomicU64::new(0),

            claim_attempts: AtomicU64::new(0),
            claim_successes: AtomicU64::new(0),

            last_tick_queue_depth: AtomicU64::new(0),
            last_tick_active_jobs: AtomicU64::new(0),
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
            .store(queue_depth as u64, Ordering::Relaxed);
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
}
