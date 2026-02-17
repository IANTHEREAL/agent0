use crate::cron::config::CronConfig;
use crate::cron::parser::{is_due, parse_cron_expression};
use crate::cron::types::{CronJob, CronRun, CronRunStatus};
use crate::extensions::context::{with_context_opts, ExtensionContextOpts};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::parse_sql;
use crate::sql::Executor;
use crate::types::DatabaseDef;
use anyhow::Result;
use chrono::{DateTime, Timelike, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::{info, warn};

pub struct CronWorker {
    config: CronConfig,
}

#[derive(Debug, Clone)]
struct DueCronJob {
    keyspace: String,
    db_id: u64,
    scheduled_minute: i64,
    job: CronJob,
}

impl CronWorker {
    fn new() -> Self {
        Self {
            config: CronConfig::from_env(),
        }
    }

    async fn run(&self, pool: Arc<TikvClientPool>) {
        let gc_pool = pool.clone();
        let _gc_handle = tokio::spawn(async move {
            cron_worker().gc_loop(gc_pool).await;
        });

        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_ms));
        loop {
            interval.tick().await;
            if let Err(e) = self.tick(&pool).await {
                warn!("cron tick error: {}", e);
            }
        }
    }

    async fn tick(&self, pool: &Arc<TikvClientPool>) -> Result<()> {
        let keyspaces = pool.list_all_keyspaces().await;
        if keyspaces.is_empty() {
            return Ok(());
        }

        let now = Utc::now();
        let tick_at = now
            .with_second(0)
            .and_then(|ts| ts.with_nanosecond(0))
            .unwrap_or(now);

        let mut due_jobs = Vec::new();
        for keyspace in keyspaces {
            match self.process_keyspace(pool, &keyspace, tick_at).await {
                Ok(mut keyspace_due) => due_jobs.append(&mut keyspace_due),
                Err(e) => {
                    warn!("cron keyspace tick error for {}: {}", keyspace, e);
                    continue;
                }
            }
            if due_jobs.len() >= self.config.max_running_jobs {
                break;
            }
        }

        for due in &due_jobs {
            info!(
                "cron due job detected keyspace={} db_id={} job_id={} minute={} schedule={} jobname={}",
                due.keyspace,
                due.db_id,
                due.job.job_id,
                due.scheduled_minute,
                due.job.schedule,
                due.job.jobname.as_deref().unwrap_or("<none>")
            );
        }

        for due in due_jobs {
            if let Err(e) = self.execute_due_job(pool, &due).await {
                warn!("cron job execution error: {}", e);
            }
        }

        Ok(())
    }

    async fn execute_due_job(&self, pool: &Arc<TikvClientPool>, due: &DueCronJob) -> Result<()> {
        let handle = pool.acquire(Some(due.keyspace.clone())).await?;
        let store = handle.store().clone();

        let Some(run) = self.claim_and_record_run(&store, due).await? else {
            return Ok(());
        };

        let exec = Executor::new(
            store.clone(),
            due.keyspace.clone(),
            observability::registry().tenant(&due.keyspace),
            handle.trigger_cache().clone(),
            handle.stats_cache().clone(),
        );

        let started_at = run.start_time.unwrap_or_else(now_ms);
        let execute_result = self.execute_job_sql(&exec, &store, due).await;

        let (status, message) = match execute_result {
            Ok(completed_commands) => (
                CronRunStatus::Succeeded,
                Some(success_message(completed_commands)),
            ),
            Err(e) => (CronRunStatus::Failed, Some(e.to_string())),
        };

        self.finalize_run(
            &store,
            due.db_id,
            run,
            status,
            message,
            started_at,
            now_ms(),
        )
        .await
    }

    async fn claim_and_record_run(
        &self,
        store: &Arc<crate::storage::TikvStore>,
        due: &DueCronJob,
    ) -> Result<Option<CronRun>> {
        let mut txn = store.begin().await?;
        let claim_result = async {
            if !store.is_cron_enabled(&mut txn, due.db_id).await? {
                return Ok(None);
            }

            let claimed = store
                .try_claim_cron_run(&mut txn, due.db_id, due.job.job_id, due.scheduled_minute)
                .await?;

            if !claimed {
                return Ok(None);
            }

            let run_id = store.next_cron_run_id(due.db_id).await?;
            let run = CronRun {
                run_id,
                job_id: due.job.job_id,
                job_pid: None,
                database: due.job.database.clone(),
                username: due.job.username.clone(),
                command: due.job.command.clone(),
                status: CronRunStatus::Running,
                return_message: None,
                start_time: Some(now_ms()),
                end_time: None,
            };
            store.put_cron_run(&mut txn, due.db_id, &run).await?;
            Ok(Some(run))
        }
        .await;

        match claim_result {
            Ok(Some(run)) => {
                txn.commit().await?;
                Ok(Some(run))
            }
            Ok(None) => {
                let _ = txn.rollback().await;
                Ok(None)
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn execute_job_sql(
        &self,
        exec: &Executor,
        store: &Arc<crate::storage::TikvStore>,
        due: &DueCronJob,
    ) -> Result<usize> {
        let search_path = vec!["public".to_string(), "cron".to_string()];
        let ext_ctx = ExtensionContextOpts {
            is_superuser: true,
            allow_local_fs: false,
        };

        with_context_opts(ext_ctx, async {
            let mut txn = store.begin().await?;
            let mut sequence_values: HashMap<String, i64> = HashMap::new();
            let execute_result = async {
                let statements = parse_sql(&due.job.command)?;
                for stmt in &statements {
                    let _ = exec
                        .execute_statement_on_txn(
                            &mut txn,
                            due.db_id,
                            &mut sequence_values,
                            &search_path,
                            stmt,
                            None,
                        )
                        .await?;
                }
                txn.commit().await?;
                Ok(statements.len())
            }
            .await;

            match execute_result {
                Ok(count) => Ok(count),
                Err(e) => {
                    let _ = txn.rollback().await;
                    Err(e)
                }
            }
        })
        .await
    }

    async fn finalize_run(
        &self,
        store: &Arc<crate::storage::TikvStore>,
        db_id: u64,
        mut run: CronRun,
        status: CronRunStatus,
        return_message: Option<String>,
        start_time: i64,
        end_time: i64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let finalize_result = async {
            run.status = status;
            run.return_message = return_message;
            run.start_time = Some(start_time);
            run.end_time = Some(end_time);
            store.put_cron_run(&mut txn, db_id, &run).await
        }
        .await;

        match finalize_result {
            Ok(()) => {
                txn.commit().await?;
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn gc_loop(&self, pool: Arc<TikvClientPool>) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.config.gc_interval_sec));
        loop {
            interval.tick().await;

            let keyspaces = pool.list_all_keyspaces().await;
            for keyspace in keyspaces {
                if let Err(e) = self.gc_keyspace(&pool, &keyspace).await {
                    warn!("cron GC error for {}: {}", keyspace, e);
                }
            }
        }
    }

    async fn gc_keyspace(&self, pool: &Arc<TikvClientPool>, keyspace: &str) -> Result<()> {
        let handle = pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();

        let mut txn = store.begin().await?;
        let databases = store.list_databases(&mut txn).await?;
        txn.commit().await?;

        for db in databases {
            self.gc_database(&store, db.id).await?;
        }

        Ok(())
    }

    async fn gc_database(&self, store: &Arc<crate::storage::TikvStore>, db_id: u64) -> Result<()> {
        let now = now_ms();
        let orphan_cutoff = now.saturating_sub(
            i64::try_from(self.config.orphan_timeout_sec.saturating_mul(1000)).unwrap_or(i64::MAX),
        );
        let retention_cutoff = now.saturating_sub(
            i64::try_from(
                self.config
                    .run_retention_days
                    .saturating_mul(24)
                    .saturating_mul(3600)
                    .saturating_mul(1000),
            )
            .unwrap_or(i64::MAX),
        );

        let mut txn = store.begin().await?;
        let gc_result = async {
            if !store.is_cron_enabled(&mut txn, db_id).await? {
                return Ok((0usize, 0usize));
            }

            let runs = store
                .list_all_cron_runs(&mut txn, db_id, usize::MAX)
                .await?;
            if runs.is_empty() {
                return Ok((0usize, 0usize));
            }

            let mut recovered = 0usize;
            let mut deleted = 0usize;
            let mut job_ids = HashSet::new();
            let mut keep_by_job: HashMap<i64, Vec<CronRun>> = HashMap::new();

            for mut run in runs {
                job_ids.insert(run.job_id);

                if run.status == CronRunStatus::Running {
                    if let Some(start_ms) = run.start_time {
                        if start_ms < orphan_cutoff {
                            run.status = CronRunStatus::Failed;
                            run.return_message =
                                Some("orphan recovery: execution timed out".to_string());
                            run.end_time = Some(now);
                            recovered = recovered.saturating_add(1);
                        }
                    }
                }

                let record_ts = run.end_time.or(run.start_time).unwrap_or(i64::MAX);
                if record_ts < retention_cutoff {
                    deleted = deleted.saturating_add(1);
                    continue;
                }

                keep_by_job.entry(run.job_id).or_default().push(run);
            }

            if recovered == 0 && deleted == 0 {
                return Ok((0usize, 0usize));
            }

            for job_id in job_ids {
                store
                    .delete_cron_runs_for_job(&mut txn, db_id, job_id)
                    .await?;
            }

            for kept_runs in keep_by_job.into_values() {
                for run in kept_runs {
                    store.put_cron_run(&mut txn, db_id, &run).await?;
                }
            }

            Ok((recovered, deleted))
        }
        .await;

        match gc_result {
            Ok((recovered, deleted)) => {
                txn.commit().await?;
                if recovered > 0 || deleted > 0 {
                    info!(
                        "cron GC db_id={} recovered_orphans={} deleted_runs={}",
                        db_id, recovered, deleted
                    );
                }
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn process_keyspace(
        &self,
        pool: &Arc<TikvClientPool>,
        keyspace: &str,
        tick_at: DateTime<Utc>,
    ) -> Result<Vec<DueCronJob>> {
        let handle = pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();
        let mut txn = store.begin().await?;

        let result = self
            .discover_due_jobs_in_txn(keyspace, &store, &mut txn, tick_at)
            .await;

        match result {
            Ok(jobs) => {
                txn.commit().await?;
                Ok(jobs)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    async fn discover_due_jobs_in_txn(
        &self,
        keyspace: &str,
        store: &Arc<crate::storage::TikvStore>,
        txn: &mut tikv_client::Transaction,
        tick_at: DateTime<Utc>,
    ) -> Result<Vec<DueCronJob>> {
        let databases = store.list_databases(txn).await?;
        let mut due_jobs = Vec::new();

        for db in databases {
            if !store.is_cron_enabled(txn, db.id).await? {
                continue;
            }

            self.collect_due_jobs_for_database(keyspace, db, store, txn, tick_at, &mut due_jobs)
                .await?;

            if due_jobs.len() >= self.config.max_running_jobs {
                break;
            }
        }

        Ok(due_jobs)
    }

    async fn collect_due_jobs_for_database(
        &self,
        keyspace: &str,
        db: DatabaseDef,
        store: &Arc<crate::storage::TikvStore>,
        txn: &mut tikv_client::Transaction,
        tick_at: DateTime<Utc>,
        due_jobs: &mut Vec<DueCronJob>,
    ) -> Result<()> {
        let jobs = store.list_cron_jobs(txn, db.id).await?;
        let mut scanned = 0usize;

        for job in jobs {
            if !job.active {
                continue;
            }

            if scanned >= self.config.max_jobs_per_db {
                break;
            }
            scanned += 1;

            let schedule = match parse_cron_expression(&job.schedule) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "invalid cron schedule keyspace={} db_id={} job_id={} schedule='{}': {}",
                        keyspace, db.id, job.job_id, job.schedule, e
                    );
                    continue;
                }
            };

            if is_due(&schedule, tick_at) {
                due_jobs.push(DueCronJob {
                    keyspace: keyspace.to_string(),
                    db_id: db.id,
                    scheduled_minute: tick_at.timestamp() / 60,
                    job,
                });
                if due_jobs.len() >= self.config.max_running_jobs {
                    break;
                }
            }
        }

        Ok(())
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn success_message(completed_commands: usize) -> String {
    if completed_commands == 1 {
        "1 command completed".to_string()
    } else {
        format!("{} commands completed", completed_commands)
    }
}

static CRON_WORKER: OnceLock<CronWorker> = OnceLock::new();
static CRON_WORKER_STARTED: OnceLock<()> = OnceLock::new();

pub(crate) fn cron_worker() -> &'static CronWorker {
    CRON_WORKER.get_or_init(CronWorker::new)
}

pub(crate) fn spawn_cron_worker(pool: Arc<TikvClientPool>) {
    let worker = cron_worker();
    if !worker.config.enabled {
        info!("CronWorker disabled via PGTIKV_CRON_ENABLED=false");
        return;
    }
    if CRON_WORKER_STARTED.set(()).is_err() {
        return;
    }
    info!("CronWorker starting (poll_ms={})", worker.config.poll_ms);
    tokio::spawn(async move {
        cron_worker().run(pool).await;
    });
}
