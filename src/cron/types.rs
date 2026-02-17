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
}

impl std::fmt::Display for CronRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Succeeded => write!(f, "succeeded"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
