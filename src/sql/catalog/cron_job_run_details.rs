use super::helpers::{format_epoch_ms, int_col, null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct CronJobRunDetailsTable;

#[async_trait]
impl VirtualTable for CronJobRunDetailsTable {
    fn name(&self) -> &str {
        "cron.job_run_details"
    }

    fn schema_name(&self) -> &str {
        "cron"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "cron.job_run_details".to_string(),
            columns: vec![
                int_col("jobid"),
                int_col("runid"),
                int_col("job_pid"),
                text_col("database"),
                text_col("username"),
                text_col("command"),
                text_col("status"),
                text_col("return_message"),
                text_col("start_time"),
                text_col("end_time"),
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

        let runs = ctx
            .store
            .list_all_cron_runs(ctx.txn, ctx.db_id, 1000)
            .await?;

        // Security: non-superusers only see their own runs
        let filtered: Vec<_> = if ctx.is_superuser {
            runs
        } else {
            runs.into_iter()
                .filter(|run| run.username == ctx.current_user)
                .collect()
        };

        let rows = filtered
            .into_iter()
            .map(|run| {
                Row::new(vec![
                    Value::Int64(run.job_id),
                    Value::Int64(run.run_id),
                    run.job_pid
                        .map(|pid| Value::Int64(pid as i64))
                        .unwrap_or_else(null_val),
                    text_val(&run.database),
                    text_val(&run.username),
                    text_val(&run.command),
                    text_val(&run.status.to_string()),
                    run.return_message
                        .as_deref()
                        .map(text_val)
                        .unwrap_or_else(null_val),
                    run.start_time
                        .map(|ts| text_val(&format_epoch_ms(ts)))
                        .unwrap_or_else(null_val),
                    run.end_time
                        .map(|ts| text_val(&format_epoch_ms(ts)))
                        .unwrap_or_else(null_val),
                ])
            })
            .collect();

        Ok(rows)
    }
}
