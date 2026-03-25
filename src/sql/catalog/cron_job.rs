use super::helpers::{bool_col, format_epoch_ms, int_col, null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::cron::parser::{next_occurrence, parse_cron_expression};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;

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
        TableSchema::virtual_table(
            "cron.job",
            vec![
                int_col("jobid"),
                text_col("schedule"),
                text_col("command"),
                text_col("nodename"),
                int_col("nodeport"),
                text_col("database"),
                text_col("username"),
                bool_col("active"),
                text_col("jobname"),
                text_col("max_runtime"),
                text_col("next_run_at"),
            ],
        )
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
                let next_run_at = match parse_cron_expression(&job.schedule) {
                    Ok(parsed) => match next_occurrence(&parsed, Utc::now()) {
                        Some(dt) => text_val(&format_epoch_ms(dt.timestamp_millis())),
                        None => null_val(),
                    },
                    Err(_) => null_val(),
                };
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
                    text_val(&format_runtime_ms(job.max_runtime_ms)),
                    next_run_at,
                ])
            })
            .collect();

        Ok(rows)
    }
}

fn format_runtime_ms(ms: Option<u64>) -> String {
    match ms {
        None | Some(0) => "default".to_string(),
        Some(ms) if ms >= 3_600_000 && ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        Some(ms) if ms >= 60_000 && ms % 60_000 == 0 => format!("{}min", ms / 60_000),
        Some(ms) if ms >= 1_000 && ms % 1_000 == 0 => format!("{}s", ms / 1_000),
        Some(ms) => format!("{}ms", ms),
    }
}
