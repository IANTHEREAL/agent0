use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CronJob {
    pub job_id: i64,
    pub schedule: String,
    pub command: String,
    pub nodename: String,
    pub nodeport: i32,
    pub database: String,
    pub username: String,
    pub active: bool,
    pub jobname: Option<String>,
    #[serde(default)]
    pub max_runtime_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJobLegacy {
    pub job_id: i64,
    pub schedule: String,
    pub command: String,
    pub nodename: String,
    pub nodeport: i32,
    pub database: String,
    pub username: String,
    pub active: bool,
    pub jobname: Option<String>,
}

impl From<CronJobLegacy> for CronJob {
    fn from(legacy: CronJobLegacy) -> Self {
        Self {
            job_id: legacy.job_id,
            schedule: legacy.schedule,
            command: legacy.command,
            nodename: legacy.nodename,
            nodeport: legacy.nodeport,
            database: legacy.database,
            username: legacy.username,
            active: legacy.active,
            jobname: legacy.jobname,
            max_runtime_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CronRun {
    pub run_id: i64,
    pub job_id: i64,
    pub job_pid: Option<i32>,
    pub database: String,
    pub username: String,
    pub command: String,
    pub status: CronRunStatus,
    pub return_message: Option<String>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CronRunStatus {
    Starting,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl CronRunStatus {
    /// Whether this user-visible run status is terminal (the run has finished).
    /// Mirrors [`CronRunState::is_terminal`] — `Succeeded`/`Failed`/`Cancelled`
    /// are terminal; `Starting`/`Running` are live. Retention GC keys eligibility
    /// off this so a still-live run's history record (the cross-generation
    /// no-overlap liveness signal) is never deleted out from under the bridge.
    /// (`&self`: `CronRunStatus` is not `Copy`, unlike the `Copy` `CronRunState`.)
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl std::fmt::Display for CronRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Succeeded => write!(f, "succeeded"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Lifecycle state of a single cron fire, stored on the per-(db,job,minute)
/// CONTROL record (design 35). This is the correctness authority; `CronRunStatus`
/// is retained only for the user-visible `CronRun` history projection.
///
/// `Claimed`/`Running` are live; `Succeeded`/`Failed`/`Cancelled` are terminal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CronRunState {
    Claimed,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl CronRunState {
    /// A terminal state means this fire's outcome is decided; the per-minute
    /// CONTROL record then serves purely as a dedup tombstone for that minute.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl From<CronRunState> for CronRunStatus {
    fn from(s: CronRunState) -> Self {
        match s {
            CronRunState::Claimed => CronRunStatus::Starting,
            CronRunState::Running => CronRunStatus::Running,
            CronRunState::Succeeded => CronRunStatus::Succeeded,
            CronRunState::Failed => CronRunStatus::Failed,
            CronRunState::Cancelled => CronRunStatus::Cancelled,
        }
    }
}

impl From<CronRunStatus> for CronRunState {
    /// Map a user-visible terminal `CronRunStatus` back to the CONTROL
    /// authority's `CronRunState` so a reconciliation/cleanup path can
    /// terminalize CONTROL *consistently with* an already-terminal `CronRun`
    /// (design 35 §Reaper reconcile) instead of forcing `Failed` over the
    /// owner's real result. The two live states (`Starting`/`Running`) have no
    /// terminal CONTROL counterpart — a non-terminal `CronRun` means the owner
    /// never finalized, so the reconciler treats it as a genuine orphan and the
    /// caller maps those to `Failed` itself.
    fn from(s: CronRunStatus) -> Self {
        match s {
            CronRunStatus::Succeeded => CronRunState::Succeeded,
            CronRunStatus::Cancelled => CronRunState::Cancelled,
            CronRunStatus::Failed => CronRunState::Failed,
            CronRunStatus::Starting => CronRunState::Claimed,
            CronRunStatus::Running => CronRunState::Running,
        }
    }
}

/// Per-(db,job,minute) CONTROL record — the single source of truth for one cron
/// fire's dedup + state. Written only inside the cron CAS helpers. `fence_token`
/// equals `run_id` and is minted (monotonic, per-db) inside the claim txn; every
/// terminal/takeover transition is gated on "my fence >= stored fence", so a
/// worker that lost its lease cannot commit terminal state (DEFECT 1 fix).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronRunControl {
    pub state: CronRunState,
    pub fence_token: i64,
    pub run_id: i64,
    pub scheduled_min: i64,
    pub started_ms: i64,
    /// Wall-clock instant past which this run is considered orphaned and may be
    /// superseded by a takeover/reaper. Derived from the orphan timeout (>> the
    /// system lease), so a normally-progressing run is never superseded mid-flight.
    pub deadline_ms: i64,
    /// Bumped on each terminal transition; aids debugging / idempotent retries.
    pub finalize_seq: u32,
}

/// Per-(db,job) ACTIVE-RUN pointer — job-level no-overlap across minutes (the
/// semantic successor of the running guard). A claim for a *different* minute is
/// blocked while this pointer is live. Cleared in the same CAS txn that terminates
/// the owning run, so it can never outlive its run (DEFECT 2 fix).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronActiveRun {
    pub job_id: i64,
    pub active_minute: i64,
    pub run_id: i64,
    pub fence_token: i64,
    pub deadline_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_run_control_bincode_roundtrip() {
        for state in [
            CronRunState::Claimed,
            CronRunState::Running,
            CronRunState::Succeeded,
            CronRunState::Failed,
            CronRunState::Cancelled,
        ] {
            let c = CronRunControl {
                state,
                fence_token: 7,
                run_id: 7,
                scheduled_min: 28_900_123,
                started_ms: 1_700_000_000_000,
                deadline_ms: 1_700_000_300_000,
                finalize_seq: 2,
            };
            let data = bincode::serialize(&c).expect("serialize");
            let decoded: CronRunControl = bincode::deserialize(&data).expect("deserialize");
            assert_eq!(decoded, c);
        }
    }

    #[test]
    fn cron_active_run_bincode_roundtrip() {
        let a = CronActiveRun {
            job_id: 42,
            active_minute: 28_900_123,
            run_id: 9,
            fence_token: 9,
            deadline_ms: 1_700_000_300_000,
        };
        let data = bincode::serialize(&a).expect("serialize");
        let decoded: CronActiveRun = bincode::deserialize(&data).expect("deserialize");
        assert_eq!(decoded, a);
    }

    #[test]
    fn cron_run_state_terminal_and_status_mapping() {
        assert!(!CronRunState::Claimed.is_terminal());
        assert!(!CronRunState::Running.is_terminal());
        assert!(CronRunState::Succeeded.is_terminal());
        assert!(CronRunState::Failed.is_terminal());
        assert!(CronRunState::Cancelled.is_terminal());
        assert_eq!(
            CronRunStatus::from(CronRunState::Claimed),
            CronRunStatus::Starting
        );
        assert_eq!(
            CronRunStatus::from(CronRunState::Running),
            CronRunStatus::Running
        );
        assert_eq!(
            CronRunStatus::from(CronRunState::Succeeded),
            CronRunStatus::Succeeded
        );
        assert_eq!(
            CronRunStatus::from(CronRunState::Failed),
            CronRunStatus::Failed
        );
        assert_eq!(
            CronRunStatus::from(CronRunState::Cancelled),
            CronRunStatus::Cancelled
        );
    }

    #[test]
    fn test_cron_job_bincode_roundtrip() {
        let job = CronJob {
            job_id: 42,
            schedule: "*/5 * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "postgres".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("test_job".to_string()),
            max_runtime_ms: None,
        };
        let data = bincode::serialize(&job).expect("serialize");
        let decoded: CronJob = bincode::deserialize(&data).expect("deserialize");
        assert_eq!(decoded, job);
    }

    #[test]
    fn test_cron_job_no_jobname_roundtrip() {
        let job = CronJob {
            job_id: 1,
            schedule: "0 * * * *".to_string(),
            command: "VACUUM".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "mydb".to_string(),
            username: "user1".to_string(),
            active: false,
            jobname: None,
            max_runtime_ms: None,
        };
        let data = bincode::serialize(&job).expect("serialize");
        let decoded: CronJob = bincode::deserialize(&data).expect("deserialize");
        assert_eq!(decoded, job);
    }

    #[test]
    fn test_cron_run_bincode_roundtrip() {
        let run = CronRun {
            run_id: 100,
            job_id: 42,
            job_pid: Some(1234),
            database: "postgres".to_string(),
            username: "admin".to_string(),
            command: "SELECT 1".to_string(),
            status: CronRunStatus::Succeeded,
            return_message: Some("1 row".to_string()),
            start_time: Some(1700000000000),
            end_time: Some(1700000001000),
        };
        let data = bincode::serialize(&run).expect("serialize");
        let decoded: CronRun = bincode::deserialize(&data).expect("deserialize");
        assert_eq!(decoded, run);
    }

    #[test]
    fn test_cron_run_all_statuses() {
        for status in [
            CronRunStatus::Starting,
            CronRunStatus::Running,
            CronRunStatus::Succeeded,
            CronRunStatus::Failed,
            CronRunStatus::Cancelled,
        ] {
            let run = CronRun {
                run_id: 1,
                job_id: 1,
                job_pid: None,
                database: "db".to_string(),
                username: "u".to_string(),
                command: "SELECT 1".to_string(),
                status: status.clone(),
                return_message: None,
                start_time: None,
                end_time: None,
            };
            let data = bincode::serialize(&run).expect("serialize");
            let decoded: CronRun = bincode::deserialize(&data).expect("deserialize");
            assert_eq!(decoded.status, status);
        }
    }

    #[test]
    fn test_cron_run_status_display() {
        assert_eq!(CronRunStatus::Starting.to_string(), "starting");
        assert_eq!(CronRunStatus::Running.to_string(), "running");
        assert_eq!(CronRunStatus::Succeeded.to_string(), "succeeded");
        assert_eq!(CronRunStatus::Failed.to_string(), "failed");
        assert_eq!(CronRunStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_cron_job_bincode_roundtrip_with_max_runtime() {
        let job = CronJob {
            job_id: 42,
            schedule: "*/5 * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "postgres".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("test_job".to_string()),
            max_runtime_ms: Some(1_800_000),
        };
        let data = bincode::serialize(&job).expect("serialize");
        let decoded: CronJob = bincode::deserialize(&data).expect("deserialize");
        assert_eq!(decoded, job);
    }

    #[test]
    fn test_cron_job_legacy_roundtrip_and_convert() {
        let legacy = CronJobLegacy {
            job_id: 7,
            schedule: "0 * * * *".to_string(),
            command: "VACUUM".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "postgres".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("legacy_job".to_string()),
        };

        let data = bincode::serialize(&legacy).expect("serialize");
        let decoded_legacy: CronJobLegacy =
            bincode::deserialize(&data).expect("deserialize legacy");
        let decoded: CronJob = decoded_legacy.into();

        assert_eq!(decoded.job_id, legacy.job_id);
        assert_eq!(decoded.schedule, legacy.schedule);
        assert_eq!(decoded.command, legacy.command);
        assert_eq!(decoded.nodename, legacy.nodename);
        assert_eq!(decoded.nodeport, legacy.nodeport);
        assert_eq!(decoded.database, legacy.database);
        assert_eq!(decoded.username, legacy.username);
        assert_eq!(decoded.active, legacy.active);
        assert_eq!(decoded.jobname, legacy.jobname);
        assert_eq!(decoded.max_runtime_ms, None);
    }

    #[test]
    fn test_cron_run_status_cancelled_display() {
        assert_eq!(CronRunStatus::Cancelled.to_string(), "cancelled");
    }
}
