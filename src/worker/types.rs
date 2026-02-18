use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MAX)
}

// ============================================================================
// Task Type Bitmask Constants
// ============================================================================

pub const TASK_TYPE_CRON: u8 = 0x01;
pub const TASK_TYPE_ASYNC_TRIGGER: u8 = 0x02;
pub const TASK_TYPE_AUTO_ANALYZE: u8 = 0x04;
pub const TASK_TYPE_BG_DDL: u8 = 0x08;
pub const TASK_TYPE_BG_SQL: u8 = 0x10;

// ============================================================================
// TaskType Enum
// ============================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskType {
    Cron,
    AsyncTrigger,
    AutoAnalyze,
    BgDdl,
    BgSql,
}

impl TaskType {
    /// Convert TaskType to its bitmask value
    pub fn to_bitmask(self) -> u8 {
        match self {
            TaskType::Cron => TASK_TYPE_CRON,
            TaskType::AsyncTrigger => TASK_TYPE_ASYNC_TRIGGER,
            TaskType::AutoAnalyze => TASK_TYPE_AUTO_ANALYZE,
            TaskType::BgDdl => TASK_TYPE_BG_DDL,
            TaskType::BgSql => TASK_TYPE_BG_SQL,
        }
    }

    /// Convert bitmask value to TaskType (returns first matching type)
    pub fn from_bitmask(mask: u8) -> Option<Self> {
        if mask & TASK_TYPE_CRON != 0 {
            Some(TaskType::Cron)
        } else if mask & TASK_TYPE_ASYNC_TRIGGER != 0 {
            Some(TaskType::AsyncTrigger)
        } else if mask & TASK_TYPE_AUTO_ANALYZE != 0 {
            Some(TaskType::AutoAnalyze)
        } else if mask & TASK_TYPE_BG_DDL != 0 {
            Some(TaskType::BgDdl)
        } else if mask & TASK_TYPE_BG_SQL != 0 {
            Some(TaskType::BgSql)
        } else {
            None
        }
    }
}

// ============================================================================
// IndexState Enum
// ============================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IndexState {
    Ready,
    Building,
    Invalid,
}

impl Default for IndexState {
    fn default() -> Self {
        IndexState::Ready
    }
}

// ============================================================================
// TaskRegistryEntry
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRegistryEntry {
    pub keyspace: String,
    pub db_id: u64,
    pub task_types: u8, // bitmask: 0x01=cron, 0x02=async_trigger, 0x04=auto_analyze, 0x08=bg_ddl, 0x10=bg_sql
    pub job_count: u32, // hint for load balancing, not required to be exact
    pub registered_at: i64, // epoch ms
}

impl TaskRegistryEntry {
    pub fn new(keyspace: String, db_id: u64) -> Self {
        Self {
            keyspace,
            db_id,
            task_types: 0,
            job_count: 0,
            registered_at: now_ms_i64(),
        }
    }

    /// Check if cron bit is set
    pub fn has_cron(&self) -> bool {
        self.task_types & TASK_TYPE_CRON != 0
    }

    /// Set cron bit
    pub fn set_cron(&mut self) {
        self.task_types |= TASK_TYPE_CRON;
    }

    /// Clear cron bit
    pub fn clear_cron(&mut self) {
        self.task_types &= !TASK_TYPE_CRON;
    }

    /// Check if async_trigger bit is set
    pub fn has_async_trigger(&self) -> bool {
        self.task_types & TASK_TYPE_ASYNC_TRIGGER != 0
    }

    /// Set async_trigger bit
    pub fn set_async_trigger(&mut self) {
        self.task_types |= TASK_TYPE_ASYNC_TRIGGER;
    }

    /// Clear async_trigger bit
    pub fn clear_async_trigger(&mut self) {
        self.task_types &= !TASK_TYPE_ASYNC_TRIGGER;
    }

    /// Check if auto_analyze bit is set
    pub fn has_auto_analyze(&self) -> bool {
        self.task_types & TASK_TYPE_AUTO_ANALYZE != 0
    }

    /// Set auto_analyze bit
    pub fn set_auto_analyze(&mut self) {
        self.task_types |= TASK_TYPE_AUTO_ANALYZE;
    }

    /// Clear auto_analyze bit
    pub fn clear_auto_analyze(&mut self) {
        self.task_types &= !TASK_TYPE_AUTO_ANALYZE;
    }

    /// Check if bg_ddl bit is set
    pub fn has_bg_ddl(&self) -> bool {
        self.task_types & TASK_TYPE_BG_DDL != 0
    }

    /// Set bg_ddl bit
    pub fn set_bg_ddl(&mut self) {
        self.task_types |= TASK_TYPE_BG_DDL;
    }

    /// Clear bg_ddl bit
    pub fn clear_bg_ddl(&mut self) {
        self.task_types &= !TASK_TYPE_BG_DDL;
    }

    /// Check if bg_sql bit is set
    pub fn has_bg_sql(&self) -> bool {
        self.task_types & TASK_TYPE_BG_SQL != 0
    }

    /// Set bg_sql bit
    pub fn set_bg_sql(&mut self) {
        self.task_types |= TASK_TYPE_BG_SQL;
    }

    /// Clear bg_sql bit
    pub fn clear_bg_sql(&mut self) {
        self.task_types &= !TASK_TYPE_BG_SQL;
    }

    /// Check if any task type is registered
    pub fn is_empty(&self) -> bool {
        self.task_types == 0
    }
}

// ============================================================================
// TaskQueueEntry
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskQueueEntry {
    pub keyspace: String,
    pub db_id: u64,
    pub task_id: i64, // job_id / trigger_id / table_id / index_id
    pub task_type: TaskType,
    pub command: String,
    pub username: String,
    #[serde(default)]
    pub schedule: Option<String>, // cron expression (only for Cron type)
    pub priority: u8, // 0-255, higher priority executes first
}

impl TaskQueueEntry {
    pub fn new(
        keyspace: String,
        db_id: u64,
        task_id: i64,
        task_type: TaskType,
        command: String,
        username: String,
        priority: u8,
    ) -> Self {
        Self {
            keyspace,
            db_id,
            task_id,
            task_type,
            command,
            username,
            schedule: None,
            priority,
        }
    }

    pub fn with_schedule(mut self, schedule: String) -> Self {
        self.schedule = Some(schedule);
        self
    }
}

// ============================================================================
// WorkerClaim
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerClaim {
    pub worker_id: String, // instance identifier (hostname:pid or UUID)
    pub claimed_at: i64,   // epoch ms
    pub task_type: TaskType,
}

impl WorkerClaim {
    pub fn new(worker_id: String, task_type: TaskType) -> Self {
        Self {
            worker_id,
            claimed_at: now_ms_i64(),
            task_type,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_type_bitmask_conversion() {
        assert_eq!(TaskType::Cron.to_bitmask(), TASK_TYPE_CRON);
        assert_eq!(TaskType::AsyncTrigger.to_bitmask(), TASK_TYPE_ASYNC_TRIGGER);
        assert_eq!(TaskType::AutoAnalyze.to_bitmask(), TASK_TYPE_AUTO_ANALYZE);
        assert_eq!(TaskType::BgDdl.to_bitmask(), TASK_TYPE_BG_DDL);
        assert_eq!(TaskType::BgSql.to_bitmask(), TASK_TYPE_BG_SQL);
    }

    #[test]
    fn test_task_type_from_bitmask() {
        assert_eq!(TaskType::from_bitmask(TASK_TYPE_CRON), Some(TaskType::Cron));
        assert_eq!(
            TaskType::from_bitmask(TASK_TYPE_ASYNC_TRIGGER),
            Some(TaskType::AsyncTrigger)
        );
        assert_eq!(
            TaskType::from_bitmask(TASK_TYPE_AUTO_ANALYZE),
            Some(TaskType::AutoAnalyze)
        );
        assert_eq!(
            TaskType::from_bitmask(TASK_TYPE_BG_DDL),
            Some(TaskType::BgDdl)
        );
        assert_eq!(
            TaskType::from_bitmask(TASK_TYPE_BG_SQL),
            Some(TaskType::BgSql)
        );
        assert_eq!(TaskType::from_bitmask(0), None);
    }

    #[test]
    fn test_index_state_default() {
        assert_eq!(IndexState::default(), IndexState::Ready);
    }

    #[test]
    fn test_task_registry_entry_bitmask_operations() {
        let mut entry = TaskRegistryEntry::new("default".to_string(), 1);
        assert!(!entry.has_cron());
        assert!(!entry.has_async_trigger());

        entry.set_cron();
        assert!(entry.has_cron());
        assert!(!entry.has_async_trigger());

        entry.set_async_trigger();
        assert!(entry.has_cron());
        assert!(entry.has_async_trigger());

        entry.clear_cron();
        assert!(!entry.has_cron());
        assert!(entry.has_async_trigger());

        entry.clear_async_trigger();
        assert!(entry.is_empty());
    }

    #[test]
    fn test_task_registry_entry_bincode_roundtrip() {
        let mut entry = TaskRegistryEntry::new("myapp".to_string(), 42);
        entry.set_cron();
        entry.set_auto_analyze();
        entry.job_count = 5;

        let data = bincode::serialize(&entry).expect("serialize");
        let decoded: TaskRegistryEntry = bincode::deserialize(&data).expect("deserialize");

        assert_eq!(decoded.keyspace, "myapp");
        assert_eq!(decoded.db_id, 42);
        assert!(decoded.has_cron());
        assert!(decoded.has_auto_analyze());
        assert!(!decoded.has_async_trigger());
        assert_eq!(decoded.job_count, 5);
    }

    #[test]
    fn test_task_queue_entry_bincode_roundtrip() {
        let entry = TaskQueueEntry::new(
            "default".to_string(),
            1,
            100,
            TaskType::Cron,
            "SELECT 1".to_string(),
            "admin".to_string(),
            10,
        )
        .with_schedule("*/5 * * * *".to_string());

        let data = bincode::serialize(&entry).expect("serialize");
        let decoded: TaskQueueEntry = bincode::deserialize(&data).expect("deserialize");

        assert_eq!(decoded.keyspace, "default");
        assert_eq!(decoded.db_id, 1);
        assert_eq!(decoded.task_id, 100);
        assert_eq!(decoded.task_type, TaskType::Cron);
        assert_eq!(decoded.command, "SELECT 1");
        assert_eq!(decoded.username, "admin");
        assert_eq!(decoded.priority, 10);
        assert_eq!(decoded.schedule, Some("*/5 * * * *".to_string()));
    }

    #[test]
    fn test_worker_claim_bincode_roundtrip() {
        let claim = WorkerClaim::new("worker-1".to_string(), TaskType::AsyncTrigger);
        let data = bincode::serialize(&claim).expect("serialize");
        let decoded: WorkerClaim = bincode::deserialize(&data).expect("deserialize");

        assert_eq!(decoded.worker_id, "worker-1");
        assert_eq!(decoded.task_type, TaskType::AsyncTrigger);
    }

    #[test]
    fn test_task_queue_entry_default_schedule_is_none() {
        let entry = TaskQueueEntry::new(
            "ks".to_string(),
            1,
            10,
            TaskType::BgSql,
            "SELECT 1".to_string(),
            "user".to_string(),
            5,
        );
        assert_eq!(entry.schedule, None);
    }

    #[test]
    fn test_task_registry_entry_multiple_bitmask_set() {
        let mut entry = TaskRegistryEntry::new("ks".to_string(), 1);
        entry.set_cron();
        entry.set_bg_ddl();
        assert!(entry.has_cron());
        assert!(entry.has_bg_ddl());
        assert!(!entry.has_bg_sql());
        assert!(!entry.has_async_trigger());
        assert!(!entry.has_auto_analyze());
        assert!(!entry.is_empty());
    }

    #[test]
    fn test_worker_claim_bincode_all_fields() {
        let claim = WorkerClaim::new("host1:9999".to_string(), TaskType::AutoAnalyze);
        let data = bincode::serialize(&claim).expect("serialize");
        let decoded: WorkerClaim = bincode::deserialize(&data).expect("deserialize");

        assert_eq!(decoded.worker_id, "host1:9999");
        assert_eq!(decoded.claimed_at, claim.claimed_at);
        assert_eq!(decoded.task_type, TaskType::AutoAnalyze);
    }

    #[test]
    fn test_task_type_all_variants_roundtrip() {
        let variants = vec![
            TaskType::Cron,
            TaskType::AsyncTrigger,
            TaskType::AutoAnalyze,
            TaskType::BgDdl,
            TaskType::BgSql,
        ];
        for variant in variants {
            let data = bincode::serialize(&variant).expect("serialize");
            let decoded: TaskType = bincode::deserialize(&data).expect("deserialize");
            assert_eq!(decoded, variant);
        }
    }

    #[test]
    fn test_index_state_all_variants_roundtrip() {
        let variants = vec![IndexState::Ready, IndexState::Building, IndexState::Invalid];
        for variant in variants {
            let data = bincode::serialize(&variant).expect("serialize");
            let decoded: IndexState = bincode::deserialize(&data).expect("deserialize");
            assert_eq!(decoded, variant);
        }
    }
}
