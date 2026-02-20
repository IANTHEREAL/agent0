use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct RunningCronJob {
    pub run_id: i64,
    pub job_id: i64,
    pub keyspace: String,
    pub db_id: u64,
    pub username: String,
    pub command: String,
    pub started_at: i64,
}

struct RunningEntry {
    info: RunningCronJob,
    cancel_signal: Arc<Notify>,
}

pub struct CronProcessList {
    running: RwLock<HashMap<i64, RunningEntry>>,
}

static PROCESS_LIST: OnceLock<Arc<CronProcessList>> = OnceLock::new();

pub fn get_process_list() -> &'static Arc<CronProcessList> {
    PROCESS_LIST.get_or_init(|| Arc::new(CronProcessList::new()))
}

impl CronProcessList {
    fn new() -> Self {
        Self {
            running: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, info: RunningCronJob) -> Arc<Notify> {
        let signal = Arc::new(Notify::new());
        let run_id = info.run_id;
        let entry = RunningEntry {
            info,
            cancel_signal: signal.clone(),
        };
        self.running.write().unwrap().insert(run_id, entry);
        signal
    }

    pub fn deregister(&self, run_id: i64) {
        self.running.write().unwrap().remove(&run_id);
    }

    pub fn list(&self) -> Vec<RunningCronJob> {
        self.running
            .read()
            .unwrap()
            .values()
            .map(|e| e.info.clone())
            .collect()
    }

    pub fn cancel_by_job_id(&self, job_id: i64) -> bool {
        let guard = self.running.read().unwrap();
        for entry in guard.values() {
            if entry.info.job_id == job_id {
                entry.cancel_signal.notify_waiters();
                return true;
            }
        }
        false
    }

    pub fn cancel_by_run_id(&self, run_id: i64) -> bool {
        let guard = self.running.read().unwrap();
        if let Some(entry) = guard.get(&run_id) {
            entry.cancel_signal.notify_waiters();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(run_id: i64, job_id: i64) -> RunningCronJob {
        RunningCronJob {
            run_id,
            job_id,
            keyspace: "default".to_string(),
            db_id: 1,
            username: "admin".to_string(),
            command: "SELECT 1".to_string(),
            started_at: 1700000000000,
        }
    }

    #[test]
    fn register_and_list() {
        let pl = CronProcessList::new();
        let _signal = pl.register(make_info(100, 1));
        let _signal2 = pl.register(make_info(101, 2));
        let list = pl.list();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn deregister_removes_entry() {
        let pl = CronProcessList::new();
        let _signal = pl.register(make_info(100, 1));
        assert_eq!(pl.list().len(), 1);
        pl.deregister(100);
        assert!(pl.list().is_empty());
    }

    #[test]
    fn cancel_by_job_id_returns_true_when_found() {
        let pl = CronProcessList::new();
        let _signal = pl.register(make_info(100, 42));
        assert!(pl.cancel_by_job_id(42));
        assert!(!pl.cancel_by_job_id(999));
    }

    #[test]
    fn cancel_by_run_id_returns_true_when_found() {
        let pl = CronProcessList::new();
        let _signal = pl.register(make_info(100, 1));
        assert!(pl.cancel_by_run_id(100));
        assert!(!pl.cancel_by_run_id(999));
    }
}
