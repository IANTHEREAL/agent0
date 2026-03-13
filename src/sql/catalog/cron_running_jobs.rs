use super::helpers::{format_epoch_ms, int_col, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::cron::process_list::get_process_list;
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct CronRunningJobsTable;

#[async_trait]
impl VirtualTable for CronRunningJobsTable {
    fn name(&self) -> &str {
        "cron.running_jobs"
    }

    fn schema_name(&self) -> &str {
        "cron"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "cron.running_jobs".to_string(),
            columns: vec![
                int_col("run_id"),
                int_col("job_id"),
                text_col("keyspace"),
                int_col("db_id"),
                text_col("username"),
                text_col("command"),
                text_col("started_at"),
                int_col("elapsed_ms"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        if !ctx.is_superuser {
            return Ok(vec![]);
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let jobs = get_process_list().list();

        let rows = jobs
            .into_iter()
            .map(|job| {
                let elapsed = now_ms.saturating_sub(job.started_at);
                Row::new(vec![
                    Value::Int64(job.run_id),
                    Value::Int64(job.job_id),
                    text_val(&job.keyspace),
                    Value::Int64(job.db_id as i64),
                    text_val(&job.username),
                    text_val(&job.command),
                    text_val(&format_epoch_ms(job.started_at)),
                    Value::Int64(elapsed),
                ])
            })
            .collect();

        Ok(rows)
    }
}
