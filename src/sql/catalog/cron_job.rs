use super::helpers::{bool_col, int_col, null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::types::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct CronJobTable;

#[async_trait]
impl VirtualTable for CronJobTable {
    fn name(&self) -> &str {
        "cron.job"
    }

    fn schema_name(&self) -> &str {
        "cron"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "cron.job".to_string(),
            columns: vec![
                int_col("jobid"),
                text_col("schedule"),
                text_col("command"),
                text_col("nodename"),
                int_col("nodeport"),
                text_col("database"),
                text_col("username"),
                bool_col("active"),
                text_col("jobname"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let installed = ctx
            .store
            .get_extension(ctx.txn, ctx.db_id, "pg_cron")
            .await?;
        if installed.is_none() {
            return Ok(vec![]);
        }

        let jobs = ctx.store.list_cron_jobs(ctx.txn, ctx.db_id).await?;

        // Security: non-superusers only see their own jobs
        let filtered: Vec<_> = if ctx.is_superuser {
            jobs
        } else {
            jobs.into_iter()
                .filter(|job| job.username == ctx.current_user)
                .collect()
        };

        let rows = filtered
            .into_iter()
            .map(|job| {
                Row::new(vec![
                    Value::Int64(job.job_id),
                    text_val(&job.schedule),
                    text_val(&job.command),
                    text_val(&job.nodename),
                    Value::Int64(job.nodeport as i64),
                    text_val(&job.database),
                    text_val(&job.username),
                    Value::Boolean(job.active),
                    job.jobname
                        .as_deref()
                        .map(text_val)
                        .unwrap_or_else(null_val),
                ])
            })
            .collect();

        Ok(rows)
    }
}
