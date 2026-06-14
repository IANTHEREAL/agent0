use crate::cron::{parser, types::CronJob};
use crate::model::Value;
use crate::storage::TikvStore;
use crate::worker::get_system_store;
use crate::worker::types::{TaskQueueEntry, TaskType, TASK_TYPE_CRON};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tikv_client::Transaction;

const DEFAULT_CRON_NODENAME: &str = "localhost";
const DEFAULT_CRON_NODEPORT: i32 = 5433;

pub(crate) fn split_cron_scalar_function_name(func_name: &str) -> Option<&str> {
    let (schema, unqualified) = func_name.rsplit_once('.')?;
    if schema.eq_ignore_ascii_case("cron") {
        Some(unqualified)
    } else {
        None
    }
}

pub(crate) async fn execute_cron_scalar_function(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    current_user: &str,
    database_name: &str,
    is_superuser: bool,
    func_name: &str,
    args: &[Value],
    keyspace: &str,
) -> Option<Result<Value>> {
    let unqualified_name = match split_cron_scalar_function_name(func_name) {
        Some(name) => name,
        None if func_name.contains('.') => return None,
        None => func_name,
    };

    match unqualified_name.to_ascii_lowercase().as_str() {
        "schedule" => Some(
            execute_schedule(
                store,
                txn,
                db_id,
                current_user,
                database_name,
                keyspace,
                args,
            )
            .await,
        ),
        "unschedule" => Some(
            execute_unschedule(
                store,
                txn,
                db_id,
                current_user,
                is_superuser,
                keyspace,
                args,
            )
            .await,
        ),
        "alter_job" => Some(
            execute_alter_job(
                store,
                txn,
                db_id,
                current_user,
                is_superuser,
                keyspace,
                args,
            )
            .await,
        ),
        "cancel" => Some(execute_cancel(is_superuser, args).await),
        "schedule_in_database" => Some(Err(anyhow!("cron.schedule_in_database is not supported"))),
        _ => None,
    }
}

async fn execute_schedule(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    current_user: &str,
    database_name: &str,
    keyspace: &str,
    args: &[Value],
) -> Result<Value> {
    let installed = store.get_extension(txn, db_id, "pg_cron").await?;
    let Some(installed) = installed else {
        return Err(crate::extensions::ext_not_installed("pg_cron"));
    };
    if !installed.enabled {
        return Err(crate::extensions::ext_disabled("pg_cron"));
    }
    if !crate::worker::execution_enabled() {
        return Err(anyhow!(
            "cron.schedule requires the worker subsystem (DB9_WORKER_ENABLED=false). \
             Cron jobs cannot execute without the worker engine."
        ));
    }

    let (jobname, schedule, command) = match args.len() {
        2 => {
            let schedule = extract_text(&args[0], "schedule")?;
            let command = extract_text(&args[1], "command")?;
            (None, schedule, command)
        }
        3 => {
            let jobname = extract_text(&args[0], "job_name")?;
            let schedule = extract_text(&args[1], "schedule")?;
            let command = extract_text(&args[2], "command")?;
            (Some(jobname), schedule, command)
        }
        n => {
            return Err(anyhow!(
                "cron.schedule() requires 2 or 3 arguments, got {}",
                n
            ))
        }
    };

    parser::parse_cron_expression(&schedule)?;

    if let Some(jobname) = jobname.as_ref() {
        if let Some(mut existing_job) = store
            .find_cron_job_by_name(txn, db_id, jobname, current_user)
            .await?
        {
            existing_job.schedule = schedule;
            existing_job.command = command;
            existing_job.database = database_name.to_string();
            existing_job.username = current_user.to_string();
            existing_job.active = true;
            store.put_cron_job(txn, db_id, &existing_job).await?;
            enqueue_cron_to_worker(keyspace, db_id, &existing_job, true).await?;
            return Ok(Value::Int64(existing_job.job_id));
        }
    }

    let job_id = store.next_cron_job_id(db_id).await?;
    let job = CronJob {
        job_id,
        schedule,
        command,
        nodename: DEFAULT_CRON_NODENAME.to_string(),
        nodeport: DEFAULT_CRON_NODEPORT,
        database: database_name.to_string(),
        username: current_user.to_string(),
        active: true,
        jobname,
        max_runtime_ms: None,
    };
    store.put_cron_job(txn, db_id, &job).await?;
    enqueue_cron_to_worker(keyspace, db_id, &job, false).await?;
    Ok(Value::Int64(job_id))
}

async fn execute_unschedule(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    current_user: &str,
    is_superuser: bool,
    keyspace: &str,
    args: &[Value],
) -> Result<Value> {
    let installed = store.get_extension(txn, db_id, "pg_cron").await?;
    let Some(installed) = installed else {
        return Err(crate::extensions::ext_not_installed("pg_cron"));
    };
    if !installed.enabled {
        return Err(crate::extensions::ext_disabled("pg_cron"));
    }

    if args.len() != 1 {
        return Err(anyhow!(
            "cron.unschedule() requires exactly 1 argument, got {}",
            args.len()
        ));
    }

    let job = match &args[0] {
        Value::Int64(job_id) => store.get_cron_job(txn, db_id, *job_id).await?,
        Value::Int32(job_id) => store.get_cron_job(txn, db_id, *job_id as i64).await?,
        Value::Text(job_name) => {
            let jobs = store.list_cron_jobs(txn, db_id).await?;
            jobs.into_iter()
                .find(|j| j.jobname.as_deref() == Some(job_name.as_str()))
        }
        Value::Null => return Err(anyhow!("cron.unschedule: argument must not be NULL")),
        _ => {
            return Err(anyhow!(
                "cron.unschedule: argument must be bigint (job_id) or text (job_name)"
            ))
        }
    };

    let Some(job) = job else {
        return Ok(Value::Boolean(false));
    };

    if job.username != current_user && !is_superuser {
        return Err(anyhow!(
            "cron.unschedule: must be superuser or owner of the job"
        ));
    }

    let job_id = job.job_id;
    store.delete_cron_job(txn, db_id, job_id).await?;
    store.delete_cron_runs_for_job(txn, db_id, job_id).await?;
    dequeue_cron_from_worker(keyspace, db_id, job_id).await;

    Ok(Value::Boolean(true))
}

async fn execute_alter_job(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    current_user: &str,
    is_superuser: bool,
    keyspace: &str,
    args: &[Value],
) -> Result<Value> {
    let installed = store.get_extension(txn, db_id, "pg_cron").await?;
    let Some(installed) = installed else {
        return Err(crate::extensions::ext_not_installed("pg_cron"));
    };
    if !installed.enabled {
        return Err(crate::extensions::ext_disabled("pg_cron"));
    }
    if !crate::worker::execution_enabled() {
        return Err(anyhow!(
            "cron.alter_job requires the worker subsystem (DB9_WORKER_ENABLED=false). \
             Cron jobs cannot execute without the worker engine."
        ));
    }

    if args.is_empty() || args.len() > 7 {
        return Err(anyhow!(
            "cron.alter_job() requires 1 to 7 arguments, got {}",
            args.len()
        ));
    }

    let job_id = match &args[0] {
        Value::Int64(id) => *id,
        Value::Int32(id) => *id as i64,
        _ => {
            return Err(anyhow!(
                "cron.alter_job: first argument (job_id) must be bigint"
            ))
        }
    };

    let mut job = match store.get_cron_job(txn, db_id, job_id).await? {
        Some(j) => j,
        None => return Err(anyhow!("cron.alter_job: job {} not found", job_id)),
    };

    if job.username != current_user && !is_superuser {
        return Err(anyhow!(
            "cron.alter_job: must be superuser or owner of the job"
        ));
    }

    if let Some(val) = args.get(1) {
        if !matches!(val, Value::Null) {
            let schedule = match val {
                Value::Text(s) => s.clone(),
                _ => return Err(anyhow!("cron.alter_job: schedule must be text")),
            };
            parser::parse_cron_expression(&schedule)?;
            job.schedule = schedule;
        }
    }

    if let Some(val) = args.get(2) {
        if !matches!(val, Value::Null) {
            let command = match val {
                Value::Text(s) => s.clone(),
                _ => return Err(anyhow!("cron.alter_job: command must be text")),
            };
            job.command = command;
        }
    }

    if let Some(val) = args.get(3) {
        if !matches!(val, Value::Null) {
            return Err(anyhow!(
                "cron.alter_job: cross-database scheduling not supported"
            ));
        }
    }

    if let Some(val) = args.get(4) {
        if !matches!(val, Value::Null) {
            if !is_superuser {
                return Err(anyhow!(
                    "cron.alter_job: must be superuser to change job owner"
                ));
            }
            let username = match val {
                Value::Text(s) => s.clone(),
                _ => return Err(anyhow!("cron.alter_job: username must be text")),
            };
            job.username = username;
        }
    }

    if let Some(val) = args.get(5) {
        if !matches!(val, Value::Null) {
            let active = match val {
                Value::Boolean(b) => *b,
                _ => return Err(anyhow!("cron.alter_job: active must be boolean")),
            };
            job.active = active;
        }
    }

    if let Some(val) = args.get(6) {
        if !matches!(val, Value::Null) {
            let max_runtime_ms = parse_runtime_text(val)?;
            job.max_runtime_ms = max_runtime_ms;
        }
    }

    store.put_cron_job(txn, db_id, &job).await?;
    enqueue_cron_to_worker(keyspace, db_id, &job, true).await?;
    Ok(Value::Null)
}

fn compute_cron_next_fire(schedule: &str) -> Result<i64> {
    let cron_schedule = parser::parse_cron_expression(schedule)?;
    let now = chrono::Utc::now();
    let next = parser::next_occurrence(&cron_schedule, now)
        .ok_or_else(|| anyhow!("no next occurrence for schedule: {}", schedule))?;
    Ok(next.timestamp_millis())
}

async fn enqueue_cron_to_worker(
    keyspace: &str,
    db_id: u64,
    job: &CronJob,
    replace_existing: bool,
) -> Result<()> {
    let system_store = get_system_store()
        .ok_or_else(|| anyhow!("worker subsystem is disabled; cannot enqueue cron job"))?;

    let next_fire = compute_cron_next_fire(&job.schedule)?;

    let entry = TaskQueueEntry::new(
        keyspace.to_string(),
        db_id,
        job.job_id,
        TaskType::Cron,
        job.command.clone(),
        job.username.clone(),
        128,
    )
    .with_schedule(job.schedule.clone());

    let result = async {
        let mut sys_txn = system_store.begin().await?;
        if replace_existing {
            // SQL paths must not scan the global legacy worker queue. A stale
            // legacy cron entry is rejected against the cron catalog by the
            // worker when it becomes due, then deleted in place.
            system_store
                .delete_task_v2_by_identity(
                    &mut sys_txn,
                    keyspace,
                    db_id,
                    job.job_id,
                    TaskType::Cron,
                )
                .await?;
        }
        system_store
            .update_registry_task_types(&mut sys_txn, keyspace, db_id, TASK_TYPE_CRON, 0)
            .await?;
        system_store
            .put_task_v2(&mut sys_txn, &entry, next_fire)
            .await?;
        sys_txn.commit().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    result?;
    crate::worker::wake_worker();
    Ok(())
}

async fn dequeue_cron_from_worker(keyspace: &str, db_id: u64, job_id: i64) {
    let Some(system_store) = get_system_store() else {
        return;
    };

    let result = async {
        let mut sys_txn = system_store.begin().await?;
        system_store
            .delete_task_v2_by_identity(&mut sys_txn, keyspace, db_id, job_id, TaskType::Cron)
            .await?;
        sys_txn.commit().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        tracing::warn!("Failed to dequeue cron job from worker queue: {}", e);
    }
}

fn extract_text(value: &Value, arg_name: &str) -> Result<String> {
    match value {
        Value::Text(s) => Ok(s.clone()),
        Value::Null => Err(anyhow!("cron.schedule: {} must not be NULL", arg_name)),
        _ => Err(anyhow!("cron.schedule: {} must be text", arg_name)),
    }
}

async fn execute_cancel(is_superuser: bool, args: &[Value]) -> Result<Value> {
    if !is_superuser {
        return Err(anyhow!("cron.cancel: must be superuser"));
    }
    if args.len() != 1 {
        return Err(anyhow!(
            "cron.cancel() requires exactly 1 argument (job_id), got {}",
            args.len()
        ));
    }
    let job_id = match &args[0] {
        Value::Int64(id) => *id,
        Value::Int32(id) => *id as i64,
        _ => return Err(anyhow!("cron.cancel: argument must be bigint (job_id)")),
    };

    let found = crate::cron::process_list::get_process_list().cancel_by_job_id(job_id);
    if found {
        Ok(Value::Boolean(true))
    } else {
        Ok(Value::Boolean(false))
    }
}

fn parse_runtime_text(val: &Value) -> Result<Option<u64>> {
    match val {
        Value::Null => Ok(None),
        Value::Int64(0) | Value::Int32(0) => Ok(Some(0)),
        Value::Int64(ms) => {
            if *ms < 0 {
                Err(anyhow!("cron.alter_job: max_runtime must be non-negative"))
            } else {
                Ok(Some(*ms as u64))
            }
        }
        Value::Int32(ms) => {
            if *ms < 0 {
                Err(anyhow!("cron.alter_job: max_runtime must be non-negative"))
            } else {
                Ok(Some(*ms as u64))
            }
        }
        Value::Text(s) => {
            let s = s.trim().to_lowercase();
            if s == "0" {
                return Ok(Some(0));
            }
            if let Some(num) = s.strip_suffix("ms") {
                return num
                    .trim()
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| anyhow!("cron.alter_job: invalid max_runtime '{}'", s));
            }
            if let Some(num) = s.strip_suffix("min") {
                return num
                    .trim()
                    .parse::<u64>()
                    .map(|n| Some(n * 60_000))
                    .map_err(|_| anyhow!("cron.alter_job: invalid max_runtime '{}'", s));
            }
            if let Some(num) = s.strip_suffix('h') {
                return num
                    .trim()
                    .parse::<u64>()
                    .map(|n| Some(n * 3_600_000))
                    .map_err(|_| anyhow!("cron.alter_job: invalid max_runtime '{}'", s));
            }
            if let Some(num) = s.strip_suffix('s') {
                return num
                    .trim()
                    .parse::<u64>()
                    .map(|n| Some(n * 1_000))
                    .map_err(|_| anyhow!("cron.alter_job: invalid max_runtime '{}'", s));
            }
            s.parse::<u64>().map(Some).map_err(|_| {
                anyhow!(
                    "cron.alter_job: invalid max_runtime '{}'. Use format: 30min, 2h, 60s, 5000ms, or integer milliseconds",
                    s
                )
            })
        }
        _ => Err(anyhow!(
            "cron.alter_job: max_runtime must be text or integer"
        )),
    }
}

pub(crate) fn try_execute_cron_scalar_function(
    func_name: &str,
    _args: &[Value],
) -> Option<Result<Value>> {
    let unqualified_name = match split_cron_scalar_function_name(func_name) {
        Some(name) => name,
        None if func_name.contains('.') => return None,
        None => func_name,
    };

    match unqualified_name.to_ascii_lowercase().as_str() {
        "schedule" => Some(Err(anyhow!(
            "cron.schedule must be evaluated during execution"
        ))),
        "unschedule" => Some(Err(anyhow!(
            "cron.unschedule must be evaluated during execution"
        ))),
        "alter_job" => Some(Err(anyhow!(
            "cron.alter_job must be evaluated during execution"
        ))),
        "cancel" => Some(Err(anyhow!(
            "cron.cancel must be evaluated during execution"
        ))),
        "schedule_in_database" => Some(Err(anyhow!("cron.schedule_in_database is not supported"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_cron_name_matches_cron_schema_only() {
        assert_eq!(
            split_cron_scalar_function_name("CRON.SCHEDULE"),
            Some("SCHEDULE")
        );
        assert_eq!(
            split_cron_scalar_function_name("cron.unschedule"),
            Some("unschedule")
        );
        assert_eq!(split_cron_scalar_function_name("extensions.http_get"), None);
        assert_eq!(split_cron_scalar_function_name("schedule"), None);
    }

    #[test]
    fn fallback_dispatch_returns_execution_phase_error_for_schedule() {
        let err = try_execute_cron_scalar_function("CRON.SCHEDULE", &[])
            .expect("cron dispatch should match")
            .expect_err("schedule fallback should error")
            .to_string();
        assert!(err.contains("must be evaluated during execution"));
    }

    #[test]
    fn extract_text_validates_input_type_and_null() {
        assert_eq!(
            extract_text(&Value::Text("abc".to_string()), "schedule").unwrap(),
            "abc"
        );
        assert!(extract_text(&Value::Null, "schedule")
            .unwrap_err()
            .to_string()
            .contains("must not be NULL"));
        assert!(extract_text(&Value::Int64(1), "schedule")
            .unwrap_err()
            .to_string()
            .contains("must be text"));
    }

    #[test]
    fn fallback_dispatch_returns_execution_phase_error_for_unschedule() {
        let err = try_execute_cron_scalar_function("CRON.UNSCHEDULE", &[])
            .expect("cron dispatch should match")
            .expect_err("unschedule fallback should error")
            .to_string();
        assert!(err.contains("must be evaluated during execution"));
    }

    #[test]
    fn unschedule_dispatch_recognized_for_mixed_case() {
        assert!(try_execute_cron_scalar_function("cron.Unschedule", &[]).is_some());
        assert!(try_execute_cron_scalar_function("CRON.UNSCHEDULE", &[]).is_some());
        assert!(try_execute_cron_scalar_function("cron.unschedule", &[]).is_some());
    }

    #[test]
    fn unschedule_not_recognized_for_other_schemas() {
        assert!(try_execute_cron_scalar_function("public.unschedule", &[]).is_none());
        assert!(try_execute_cron_scalar_function("extensions.unschedule", &[]).is_none());
    }

    #[test]
    fn fallback_dispatch_returns_execution_phase_error_for_alter_job() {
        let err = try_execute_cron_scalar_function("CRON.ALTER_JOB", &[])
            .expect("cron dispatch should match")
            .expect_err("alter_job fallback should error")
            .to_string();
        assert!(err.contains("must be evaluated during execution"));
    }

    #[test]
    fn alter_job_dispatch_recognized_for_mixed_case() {
        assert!(try_execute_cron_scalar_function("cron.Alter_Job", &[]).is_some());
        assert!(try_execute_cron_scalar_function("CRON.ALTER_JOB", &[]).is_some());
        assert!(try_execute_cron_scalar_function("cron.alter_job", &[]).is_some());
    }

    #[test]
    fn alter_job_not_recognized_for_other_schemas() {
        assert!(try_execute_cron_scalar_function("public.alter_job", &[]).is_none());
        assert!(try_execute_cron_scalar_function("extensions.alter_job", &[]).is_none());
    }

    #[test]
    fn parse_runtime_text_units() {
        assert_eq!(
            parse_runtime_text(&Value::Text("30min".into())).unwrap(),
            Some(1_800_000)
        );
        assert_eq!(
            parse_runtime_text(&Value::Text("2h".into())).unwrap(),
            Some(7_200_000)
        );
        assert_eq!(
            parse_runtime_text(&Value::Text("60s".into())).unwrap(),
            Some(60_000)
        );
        assert_eq!(
            parse_runtime_text(&Value::Text("5000ms".into())).unwrap(),
            Some(5_000)
        );
        assert_eq!(
            parse_runtime_text(&Value::Text("5000".into())).unwrap(),
            Some(5_000)
        );
        assert_eq!(parse_runtime_text(&Value::Int64(0)).unwrap(), Some(0));
        assert_eq!(parse_runtime_text(&Value::Null).unwrap(), None);
    }

    #[test]
    fn parse_runtime_text_invalid() {
        assert!(parse_runtime_text(&Value::Text("abc".into())).is_err());
        assert!(parse_runtime_text(&Value::Int64(-1)).is_err());
    }

    #[test]
    fn cancel_dispatch_recognized() {
        assert!(try_execute_cron_scalar_function("cron.cancel", &[]).is_some());
        assert!(try_execute_cron_scalar_function("CRON.CANCEL", &[]).is_some());
    }

    #[test]
    fn new_schedule_enqueue_does_not_delete_legacy_queue_layers() {
        let source = include_str!("cron.rs");
        assert!(
            source.contains("enqueue_cron_to_worker(keyspace, db_id, &job, false).await?"),
            "new cron jobs must skip queue deletion; a freshly allocated job_id has no existing worker entry"
        );
        assert!(
            source.contains("enqueue_cron_to_worker(keyspace, db_id, &existing_job, true).await?"),
            "schedule replacement must still clean existing V2 worker entries"
        );
    }

    #[test]
    fn cron_sql_paths_do_not_scan_legacy_worker_queue() {
        let source = include_str!("cron.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("cron.rs must contain #[cfg(test)] tests");
        assert!(
            prod_source.contains(".delete_task_v2_by_identity("),
            "cron replace/unschedule must delete via bounded V2 identity index"
        );
        assert!(
            !prod_source.contains(".delete_task_all_layers("),
            "cron SQL paths must not scan legacy _worker_queue_ on replace/unschedule"
        );
    }
}
