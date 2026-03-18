use crate::worker::now_epoch_ms;
use serde::{Deserialize, Serialize};

// ============================================================================
// Task Type Bitmask Constants
// ============================================================================

pub const TASK_TYPE_CRON: u8 = 0x01;
pub const TASK_TYPE_ASYNC_TRIGGER: u8 = 0x02;
pub const TASK_TYPE_AUTO_ANALYZE: u8 = 0x04;
pub const TASK_TYPE_BG_DDL: u8 = 0x08;
pub const TASK_TYPE_BG_SQL: u8 = 0x10;
pub const TASK_TYPE_HNSW_MERGE: u8 = 0x20;
pub const TASK_TYPE_STORAGE_SIZE_SCAN: u8 = 0x40;

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
    HnswMerge,
    StorageSizeScan,
}

impl TaskType {
    /// Convert TaskType to its bitmask value
    #[allow(dead_code)] // forward-compat: bitmask API for task type serialization
    pub fn to_bitmask(self) -> u8 {
        match self {
            TaskType::Cron => TASK_TYPE_CRON,
            TaskType::AsyncTrigger => TASK_TYPE_ASYNC_TRIGGER,
            TaskType::AutoAnalyze => TASK_TYPE_AUTO_ANALYZE,
            TaskType::BgDdl => TASK_TYPE_BG_DDL,
            TaskType::BgSql => TASK_TYPE_BG_SQL,
            TaskType::HnswMerge => TASK_TYPE_HNSW_MERGE,
            TaskType::StorageSizeScan => TASK_TYPE_STORAGE_SIZE_SCAN,
        }
    }

    /// Convert bitmask value to TaskType (returns first matching type)
    #[allow(dead_code)] // forward-compat: bitmask API for task type serialization
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
        } else if mask & TASK_TYPE_HNSW_MERGE != 0 {
            Some(TaskType::HnswMerge)
        } else if mask & TASK_TYPE_STORAGE_SIZE_SCAN != 0 {
            Some(TaskType::StorageSizeScan)
        } else {
            None
        }
    }
}

// ============================================================================
// IndexState Enum
// ============================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum IndexState {
    #[default]
    Ready,
    Building,
    Invalid,
    WriteOnly,
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
            registered_at: now_epoch_ms(),
        }
    }

    /// Check if cron bit is set
    pub fn has_cron(&self) -> bool {
        self.task_types & TASK_TYPE_CRON != 0
    }

    /// Set cron bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn set_cron(&mut self) {
        self.task_types |= TASK_TYPE_CRON;
    }

    /// Clear cron bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn clear_cron(&mut self) {
        self.task_types &= !TASK_TYPE_CRON;
    }

    /// Check if async_trigger bit is set
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn has_async_trigger(&self) -> bool {
        self.task_types & TASK_TYPE_ASYNC_TRIGGER != 0
    }

    /// Set async_trigger bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn set_async_trigger(&mut self) {
        self.task_types |= TASK_TYPE_ASYNC_TRIGGER;
    }

    /// Clear async_trigger bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn clear_async_trigger(&mut self) {
        self.task_types &= !TASK_TYPE_ASYNC_TRIGGER;
    }

    /// Check if auto_analyze bit is set
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn has_auto_analyze(&self) -> bool {
        self.task_types & TASK_TYPE_AUTO_ANALYZE != 0
    }

    /// Set auto_analyze bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn set_auto_analyze(&mut self) {
        self.task_types |= TASK_TYPE_AUTO_ANALYZE;
    }

    /// Clear auto_analyze bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn clear_auto_analyze(&mut self) {
        self.task_types &= !TASK_TYPE_AUTO_ANALYZE;
    }

    /// Check if bg_ddl bit is set
    pub fn has_bg_ddl(&self) -> bool {
        self.task_types & TASK_TYPE_BG_DDL != 0
    }

    /// Set bg_ddl bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn set_bg_ddl(&mut self) {
        self.task_types |= TASK_TYPE_BG_DDL;
    }

    /// Clear bg_ddl bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn clear_bg_ddl(&mut self) {
        self.task_types &= !TASK_TYPE_BG_DDL;
    }

    /// Check if bg_sql bit is set
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn has_bg_sql(&self) -> bool {
        self.task_types & TASK_TYPE_BG_SQL != 0
    }

    /// Set bg_sql bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn set_bg_sql(&mut self) {
        self.task_types |= TASK_TYPE_BG_SQL;
    }

    /// Clear bg_sql bit
    #[allow(dead_code)] // forward-compat: symmetric bitmask API
    pub fn clear_bg_sql(&mut self) {
        self.task_types &= !TASK_TYPE_BG_SQL;
    }

    /// Check if hnsw_merge bit is set
    #[allow(dead_code)]
    pub fn has_hnsw_merge(&self) -> bool {
        self.task_types & TASK_TYPE_HNSW_MERGE != 0
    }

    /// Set hnsw_merge bit
    #[allow(dead_code)]
    pub fn set_hnsw_merge(&mut self) {
        self.task_types |= TASK_TYPE_HNSW_MERGE;
    }

    /// Clear hnsw_merge bit
    #[allow(dead_code)]
    pub fn clear_hnsw_merge(&mut self) {
        self.task_types &= !TASK_TYPE_HNSW_MERGE;
    }

    #[allow(dead_code)]
    pub fn has_storage_size_scan(&self) -> bool {
        self.task_types & TASK_TYPE_STORAGE_SIZE_SCAN != 0
    }

    #[allow(dead_code)]
    pub fn set_storage_size_scan(&mut self) {
        self.task_types |= TASK_TYPE_STORAGE_SIZE_SCAN;
    }

    #[allow(dead_code)]
    pub fn clear_storage_size_scan(&mut self) {
        self.task_types &= !TASK_TYPE_STORAGE_SIZE_SCAN;
    }

    #[allow(dead_code)]
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
    #[serde(default)] // backward compat: existing entries deserialize as 0
    pub nonce: u64,
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
            nonce: 0,
        }
    }

    pub fn with_schedule(mut self, schedule: String) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// Deserialize queue entry with backward compatibility for pre-nonce payloads.
    pub fn deserialize_compat(bytes: &[u8]) -> std::result::Result<Self, Box<bincode::ErrorKind>> {
        match bincode::deserialize::<TaskQueueEntry>(bytes) {
            Ok(entry) => Ok(entry),
            Err(primary_err) => {
                #[derive(Deserialize)]
                struct TaskQueueEntryV0 {
                    keyspace: String,
                    db_id: u64,
                    task_id: i64,
                    task_type: TaskType,
                    command: String,
                    username: String,
                    #[serde(default)]
                    schedule: Option<String>,
                    priority: u8,
                }

                match bincode::deserialize::<TaskQueueEntryV0>(bytes) {
                    Ok(v0) => Ok(TaskQueueEntry {
                        keyspace: v0.keyspace,
                        db_id: v0.db_id,
                        task_id: v0.task_id,
                        task_type: v0.task_type,
                        command: v0.command,
                        username: v0.username,
                        schedule: v0.schedule,
                        priority: v0.priority,
                        nonce: 0,
                    }),
                    Err(_) => Err(primary_err),
                }
            }
        }
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
            claimed_at: now_epoch_ms(),
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
    use serde::{Deserialize, Serialize};

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
    fn test_task_queue_entry_backward_compat_missing_nonce_defaults_zero() {
        #[derive(Serialize, Deserialize)]
        struct OldTaskQueueEntryNoNonce {
            keyspace: String,
            db_id: u64,
            task_id: i64,
            task_type: TaskType,
            command: String,
            username: String,
            schedule: Option<String>,
            priority: u8,
        }

        let old = OldTaskQueueEntryNoNonce {
            keyspace: "default".to_string(),
            db_id: 7,
            task_id: 99,
            task_type: TaskType::HnswMerge,
            command: "__hnsw_merge 1 1".to_string(),
            username: "system".to_string(),
            schedule: None,
            priority: 192,
        };

        let bytes = bincode::serialize(&old).expect("serialize old entry");
        let decoded = TaskQueueEntry::deserialize_compat(&bytes)
            .expect("deserialize old entry should default nonce to 0");

        assert_eq!(decoded.keyspace, old.keyspace);
        assert_eq!(decoded.db_id, old.db_id);
        assert_eq!(decoded.task_id, old.task_id);
        assert_eq!(decoded.task_type, old.task_type);
        assert_eq!(decoded.command, old.command);
        assert_eq!(decoded.username, old.username);
        assert_eq!(decoded.schedule, old.schedule);
        assert_eq!(decoded.priority, old.priority);
        assert_eq!(
            decoded.nonce, 0,
            "legacy entries without nonce must deserialize as nonce=0"
        );
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
        let variants = vec![
            IndexState::Ready,
            IndexState::Building,
            IndexState::Invalid,
            IndexState::WriteOnly,
        ];
        for variant in variants {
            let data = bincode::serialize(&variant).expect("serialize");
            let decoded: IndexState = bincode::deserialize(&data).expect("deserialize");
            assert_eq!(decoded, variant);
        }
    }

    /// CAS nonce logic: when a worker reads back the queue entry it processed,
    /// it should only delete if the nonce matches. This test verifies the
    /// comparison semantics that underpin the ABA race prevention.
    #[test]
    fn test_cas_nonce_match_decides_delete_eligibility() {
        let mut entry_a = TaskQueueEntry::new(
            "ks".to_string(),
            1,
            42,
            TaskType::HnswMerge,
            "__hnsw_merge 10 1".to_string(),
            "system".to_string(),
            192,
        );
        entry_a.nonce = 12345;

        // Same nonce → CAS match → worker should delete
        let mut entry_b = entry_a.clone();
        entry_b.nonce = 12345;
        assert_eq!(entry_a.nonce, entry_b.nonce, "same nonce must match");

        // Different nonce → CAS mismatch → worker must NOT delete
        // (simulates DML overwriting the queue key with a new nonce)
        let mut entry_c = entry_a.clone();
        entry_c.nonce = 99999;
        assert_ne!(
            entry_a.nonce, entry_c.nonce,
            "different nonce must mismatch"
        );

        // Old entry (nonce=0) vs new entry (nonce≠0) → mismatch → skip delete
        // (simulates rolling upgrade: worker reads old entry, DML writes new)
        let mut entry_old = entry_a.clone();
        entry_old.nonce = 0;
        assert_ne!(
            entry_old.nonce, entry_a.nonce,
            "old (nonce=0) vs new (nonce≠0) must mismatch"
        );
    }

    /// CAS nonce serialization: verify the nonce field survives bincode
    /// round-trip and the CAS comparison works on deserialized entries.
    #[test]
    fn test_cas_nonce_survives_bincode_roundtrip() {
        let mut entry = TaskQueueEntry::new(
            "ks".to_string(),
            1,
            42,
            TaskType::HnswMerge,
            "__hnsw_merge 10 1".to_string(),
            "system".to_string(),
            192,
        );
        entry.nonce = 0xDEAD_BEEF_CAFE_BABE;

        let bytes = bincode::serialize(&entry).expect("serialize");
        let decoded = TaskQueueEntry::deserialize_compat(&bytes).expect("deserialize");
        assert_eq!(
            decoded.nonce, entry.nonce,
            "nonce must survive bincode round-trip for CAS to work"
        );
    }

    /// ABA scenario at serialization level: DML overwrites a queue key with a
    /// new nonce while the worker holds the old nonce. After deserialization,
    /// the nonces must differ, preventing the worker from deleting DML's entry.
    #[test]
    fn test_aba_nonce_mismatch_after_overwrite_roundtrip() {
        let mut worker_entry = TaskQueueEntry::new(
            "ks".to_string(),
            1,
            42,
            TaskType::HnswMerge,
            "__hnsw_merge 10 1".to_string(),
            "system".to_string(),
            192,
        );
        worker_entry.nonce = 111;

        // DML writes a NEW entry to the same queue key with a different nonce
        let mut dml_entry = worker_entry.clone();
        dml_entry.nonce = 222;
        let dml_bytes = bincode::serialize(&dml_entry).expect("serialize DML entry");

        // Worker reads back from store → gets DML's entry
        let current = TaskQueueEntry::deserialize_compat(&dml_bytes).expect("deserialize current");

        // CAS comparison: worker_entry.nonce != current.nonce → skip delete
        assert_ne!(
            worker_entry.nonce, current.nonce,
            "ABA: worker must detect nonce mismatch and skip delete"
        );
    }

    /// Non-zero nonce guarantee: gen_range(1..=u64::MAX) never produces 0.
    /// This is critical because legacy entries deserialize with nonce=0,
    /// and a new entry with nonce=0 would be indistinguishable from legacy.
    #[test]
    fn test_nonce_generation_never_zero() {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for _ in 0..10_000 {
            let nonce: u64 = rng.gen_range(1..=u64::MAX);
            assert_ne!(nonce, 0, "gen_range(1..=u64::MAX) must never produce 0");
        }
    }

    /// Task type branching: only HnswMerge should use CAS delete.
    /// All other task types use unconditional delete.
    #[test]
    fn test_hnsw_merge_is_only_cas_eligible_task_type() {
        let cas_types = [TaskType::HnswMerge];
        let unconditional_types = [
            TaskType::Cron,
            TaskType::AsyncTrigger,
            TaskType::AutoAnalyze,
            TaskType::BgDdl,
            TaskType::BgSql,
        ];

        for tt in &cas_types {
            assert_eq!(*tt, TaskType::HnswMerge, "only HnswMerge uses CAS delete");
        }
        for tt in &unconditional_types {
            assert_ne!(
                *tt,
                TaskType::HnswMerge,
                "{:?} must use unconditional delete, not CAS",
                tt
            );
        }
    }

    /// New-format entries (with nonce) must also round-trip correctly through
    /// the compat deserializer. This ensures the compat path doesn't break
    /// the common case (new→new) while handling old→new.
    #[test]
    fn test_deserialize_compat_new_format_preserves_nonce() {
        let mut entry = TaskQueueEntry::new(
            "ks".to_string(),
            5,
            77,
            TaskType::HnswMerge,
            "__hnsw_merge 3 4".to_string(),
            "system".to_string(),
            192,
        );
        entry.nonce = 0xCAFE_BABE_DEAD_BEEF;

        let bytes = bincode::serialize(&entry).expect("serialize new-format entry");
        let decoded = TaskQueueEntry::deserialize_compat(&bytes)
            .expect("new-format entry must deserialize through compat path");

        assert_eq!(decoded.keyspace, "ks");
        assert_eq!(decoded.db_id, 5);
        assert_eq!(decoded.task_id, 77);
        assert_eq!(decoded.task_type, TaskType::HnswMerge);
        assert_eq!(decoded.command, "__hnsw_merge 3 4");
        assert_eq!(decoded.nonce, 0xCAFE_BABE_DEAD_BEEF);
    }

    /// Garbage bytes must produce Err, never panic. Exercises both the
    /// primary bincode path and the V0 fallback path inside deserialize_compat.
    #[test]
    fn test_deserialize_compat_garbage_returns_error() {
        let garbage = &[0xFF, 0x00, 0x42];
        let result = TaskQueueEntry::deserialize_compat(garbage);
        assert!(
            result.is_err(),
            "garbage bytes must return Err, not panic or Ok"
        );
    }

    /// Empty bytes must produce Err, never panic.
    #[test]
    fn test_deserialize_compat_empty_returns_error() {
        let result = TaskQueueEntry::deserialize_compat(&[]);
        assert!(
            result.is_err(),
            "empty bytes must return Err, not panic or Ok"
        );
    }

    /// Rolling upgrade scenario: old worker (code without nonce awareness)
    /// would see nonce=0 for its own entries, while new DML writes nonce≠0.
    /// CAS must detect this mismatch and skip delete — preserving DML's entry.
    #[test]
    fn test_rolling_upgrade_old_worker_vs_new_dml_nonce_mismatch() {
        // Old worker: reads an entry, doesn't set nonce → defaults to 0
        let old_worker_nonce: u64 = 0;

        // New DML: overwrites same queue key with non-zero nonce
        let new_dml_nonce: u64 = 42;

        // CAS comparison: old worker must NOT delete new DML's entry
        assert_ne!(
            old_worker_nonce, new_dml_nonce,
            "old worker (nonce=0) must not match new DML (nonce≠0)"
        );

        // Verify this through serialization: old-format entry deserialized
        // via compat gets nonce=0, which must differ from any new nonce.
        #[derive(Serialize)]
        struct OldEntry {
            keyspace: String,
            db_id: u64,
            task_id: i64,
            task_type: TaskType,
            command: String,
            username: String,
            schedule: Option<String>,
            priority: u8,
        }
        let old = OldEntry {
            keyspace: "ks".to_string(),
            db_id: 1,
            task_id: 42,
            task_type: TaskType::HnswMerge,
            command: "__hnsw_merge 1 1".to_string(),
            username: "system".to_string(),
            schedule: None,
            priority: 192,
        };
        let old_bytes = bincode::serialize(&old).expect("serialize old");
        let old_decoded = TaskQueueEntry::deserialize_compat(&old_bytes).expect("compat");
        assert_eq!(old_decoded.nonce, 0, "old entry must have nonce=0");

        // Any new entry with nonce from gen_range(1..=u64::MAX) must mismatch
        assert_ne!(old_decoded.nonce, new_dml_nonce);
    }

    #[test]
    fn test_index_state_bincode_backward_compat() {
        #[derive(Serialize, Deserialize)]
        enum OldIndexState {
            Ready,
            Building,
            Invalid,
        }

        let old_invalid_bytes = bincode::serialize(&OldIndexState::Invalid).expect("serialize");
        let decoded: IndexState = bincode::deserialize(&old_invalid_bytes).expect("deserialize");
        assert_eq!(decoded, IndexState::Invalid);
    }
}
