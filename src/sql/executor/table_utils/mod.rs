//! Table data loading, virtual table dispatch, and generate_series utilities.
//!
//! Extracted from join.rs during Phase 2 refactoring (B.2a).

mod generate_series;
#[cfg(test)]
mod tests;

pub(crate) use generate_series::{generate_series_values_limited, max_generate_series_rows};

use super::super::ddl_export;
use super::super::information_schema::VirtualTableFilter;
use super::core::Executor;
use crate::sql::catalog::virtual_tables::virtual_table_schema;
use crate::sql::error::SqlError;
use crate::types::{ColumnDef, DataType, MigrationRecord, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use chrono::Utc;
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr};
use std::collections::HashMap;
use tikv_client::Transaction;

fn worker_claim_keyspace_matches(key: &[u8], keyspace: &str) -> bool {
    const PREFIX: &[u8] = b"_worker_claim_";
    if !key.starts_with(PREFIX) {
        return false;
    }
    let mut idx = PREFIX.len();
    if idx + 2 > key.len() {
        return false;
    }
    let keyspace_len = u16::from_be_bytes([key[idx], key[idx + 1]]) as usize;
    idx += 2;
    if idx + keyspace_len > key.len() {
        return false;
    }
    let claim_keyspace = &key[idx..idx + keyspace_len];
    claim_keyspace == keyspace.as_bytes()
}

impl Executor {
    pub(crate) async fn get_table_data(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table_name: &str,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        self.get_table_data_filtered(
            txn,
            db_id,
            sequence_values,
            search_path,
            table_name,
            ctes,
            &VirtualTableFilter::default(),
        )
        .await
    }

    pub(crate) async fn get_table_data_filtered(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table_name: &str,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        filter: &VirtualTableFilter,
    ) -> Result<(TableSchema, Vec<Row>)> {
        let t_lower = table_name.to_lowercase();

        // Normalize for virtual tables (some accept optional trailing `()` legacy syntax).
        let t_upper = table_name.trim_end_matches("()").to_uppercase();
        if t_upper == "_DB9_SYS_OBSERVABILITY" || t_upper.ends_with("._DB9_SYS_OBSERVABILITY") {
            let snap = self.observability().snapshot_summary();
            let mut schema = virtual_table_schema("_DB9_SYS_OBSERVABILITY").unwrap();
            schema.name = table_name.to_string();

            let row = Row::new(vec![
                Value::Int64(i64::try_from(snap.window_seconds).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.statement_count).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.txn_commit_count).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.error_count).unwrap_or(i64::MAX)),
                Value::Float64(snap.qps),
                Value::Float64(snap.tps),
                Value::Float64(snap.latency_avg_ms),
                Value::Float64(snap.latency_p99_ms),
                Value::Int64(i64::try_from(snap.active_connections).unwrap_or(i64::MAX)),
            ]);
            return Ok((schema, vec![row]));
        }
        if t_upper == "_DB9_SYS_QUERY_SAMPLES" || t_upper.ends_with("._DB9_SYS_QUERY_SAMPLES") {
            let groups = self.observability().snapshot_query_samples();
            let mut schema = virtual_table_schema("_DB9_SYS_QUERY_SAMPLES").unwrap();
            schema.name = table_name.to_string();

            let rows = groups
                .into_iter()
                .map(|g| {
                    Row::new(vec![
                        Value::Text(g.query),
                        Value::Int64(i64::try_from(g.sample_count).unwrap_or(i64::MAX)),
                        Value::Int64(i64::try_from(g.error_count).unwrap_or(i64::MAX)),
                        Value::Float64(g.latency_avg_ms),
                        Value::Float64(g.latency_p99_ms),
                        Value::Float64(g.latency_max_ms),
                        Value::Int64(i64::try_from(g.last_seen_ms_ago).unwrap_or(i64::MAX)),
                    ])
                })
                .collect();
            return Ok((schema, rows));
        }
        if t_upper == "_DB9_SYS_EXPORT_DDL" || t_upper.ends_with("._DB9_SYS_EXPORT_DDL") {
            let exported = ddl_export::export_all_ddl(self.store().as_ref(), txn, db_id).await?;
            let mut schema = virtual_table_schema("_DB9_SYS_EXPORT_DDL").unwrap();
            schema.name = table_name.to_string();

            let rows = exported
                .into_iter()
                .map(|entry| {
                    Row::new(vec![
                        Value::Text(entry.object_type),
                        Value::Text(entry.object_name),
                        Value::Text(entry.ddl_sql),
                    ])
                })
                .collect();
            return Ok((schema, rows));
        }

        if t_upper == "_DB9_SYS_MIGRATIONS" || t_upper.ends_with("._DB9_SYS_MIGRATIONS") {
            let migrations = self.store().list_migrations(txn).await?;
            let mut schema = virtual_table_schema("_DB9_SYS_MIGRATIONS").unwrap();
            schema.name = table_name.to_string();

            let rows = migrations
                .into_iter()
                .map(|entry| {
                    Row::new(vec![
                        Value::Text(entry.name),
                        Value::Text(entry.applied_at),
                        Value::Text(entry.checksum),
                        Value::Text(entry.sql_preview),
                    ])
                })
                .collect();
            return Ok((schema, rows));
        }

        if t_upper == "_DB9_SYS_TRIGGER_QUEUE_STATS"
            || t_upper.ends_with("._DB9_SYS_TRIGGER_QUEUE_STATS")
        {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let cutoff_recent_ms = now_ms.saturating_sub(60_000);

            let mut pending = 0i64;
            let mut processing = 0i64;
            let failed = 0i64;
            let mut latency_sum_ms: u64 = 0;
            let mut latency_cnt: u64 = 0;
            let mut events_last_min = 0i64;
            let dlq_count = 0i64;

            if let Some(system_store) = crate::worker::get_system_store() {
                let mut sys_txn = system_store.begin().await?;
                let queue_entries = system_store
                    .scan_due_queue_entries(&mut sys_txn, i64::MAX, u32::MAX)
                    .await?;
                for (_key, entry) in queue_entries {
                    if entry.task_type != crate::worker::types::TaskType::AsyncTrigger
                        || entry.keyspace != self.tenant_keyspace()
                    {
                        continue;
                    }
                    pending += 1;
                    if now_ms >= entry.task_id {
                        latency_sum_ms += (now_ms - entry.task_id) as u64;
                        latency_cnt += 1;
                    }
                    if entry.task_id >= cutoff_recent_ms {
                        events_last_min += 1;
                    }
                }

                let claims = system_store.list_worker_claims(&mut sys_txn).await?;
                for (key, claim) in claims {
                    if claim.task_type != crate::worker::types::TaskType::AsyncTrigger {
                        continue;
                    }
                    if worker_claim_keyspace_matches(&key, self.tenant_keyspace()) {
                        processing += 1;
                    }
                }
                sys_txn.rollback().await.ok();
            }

            let avg_latency_ms = if latency_cnt == 0 {
                0.0
            } else {
                (latency_sum_ms as f64) / (latency_cnt as f64)
            };

            let mut schema = virtual_table_schema("_DB9_SYS_TRIGGER_QUEUE_STATS").unwrap();
            schema.name = table_name.to_string();

            let row = Row::new(vec![
                Value::Text(self.tenant_keyspace().to_string()),
                Value::Int64(pending),
                Value::Int64(processing),
                Value::Int64(failed),
                Value::Int64(dlq_count),
                Value::Float64(avg_latency_ms),
                Value::Int64(events_last_min),
            ]);
            return Ok((schema, vec![row]));
        }

        if t_upper == "_DB9_SYS_TRIGGER_DLQ" || t_upper.ends_with("._DB9_SYS_TRIGGER_DLQ") {
            let mut schema = virtual_table_schema("_DB9_SYS_TRIGGER_DLQ").unwrap();
            schema.name = table_name.to_string();

            return Ok((schema, Vec::new()));
        }

        if let Some((schema, rows)) = ctes.get(&t_lower) {
            return Ok((schema.clone(), rows.clone()));
        }

        if super::super::information_schema::get_information_schema_schema(&t_lower).is_some() {
            return super::super::information_schema::get_information_schema_data_filtered(
                &self.store(),
                txn,
                db_id,
                &t_lower,
                filter,
            )
            .await;
        }

        let candidates: Vec<String> = if table_name.contains('.') {
            vec![table_name.to_string()]
        } else if search_path.is_empty() {
            vec![format!("public.{}", table_name)]
        } else {
            search_path
                .iter()
                .map(|schema| format!("{}.{}", schema, table_name))
                .collect()
        };

        for candidate in &candidates {
            if let Some(schema) = self.store().get_schema(txn, db_id, candidate).await? {
                let rows = self.scan_and_fill(txn, db_id, candidate, &schema).await?;
                return Ok((schema, rows));
            }
        }

        Err(SqlError::RelationNotFound(table_name.to_string()).into())
    }

    pub(crate) async fn execute_generate_series(
        &self,
        args: &[FunctionArg],
        alias_name: &str,
        table_alias: Option<&sqlparser::ast::TableAlias>,
        offset: usize,
        limit: Option<usize>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        use super::super::expr::bridge::eval_const_ast_expr;

        fn extract_expr(arg: &FunctionArg) -> Result<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(anyhow!("generate_series requires expression arguments")),
            }
        }

        if args.len() < 2 {
            return Err(anyhow!("generate_series requires at least 2 arguments"));
        }

        let start_val = eval_const_ast_expr(extract_expr(&args[0])?)?;
        let stop_val = eval_const_ast_expr(extract_expr(&args[1])?)?;
        let step_val = if args.len() >= 3 {
            eval_const_ast_expr(extract_expr(&args[2])?)?
        } else {
            Value::Null
        };

        let max_rows = max_generate_series_rows();
        let (values, data_type) = generate_series_values_limited(
            &start_val, &stop_val, &step_val, offset, limit, max_rows,
        )?;

        let col_name = if let Some(ta) = table_alias {
            if !ta.columns.is_empty() {
                ta.columns[0].value.clone()
            } else {
                alias_name.to_string()
            }
        } else {
            "generate_series".to_string()
        };

        let schema = TableSchema {
            table_id: 0,
            name: "generate_series".to_string(),
            columns: vec![ColumnDef {
                name: col_name,
                data_type,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            version: 1,
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let rows: Vec<Row> = values.into_iter().map(|v| Row::new(vec![v])).collect();
        Ok((schema, rows))
    }

    pub(crate) async fn execute_record_migration(
        &self,
        txn: &mut Transaction,
        args: &[FunctionArg],
    ) -> Result<(TableSchema, Vec<Row>)> {
        use super::super::expr::bridge::eval_const_ast_expr;

        fn extract_expr(arg: &FunctionArg) -> Result<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(anyhow!(
                    "_db9_sys_record_migration requires expression arguments"
                )),
            }
        }

        if args.len() != 3 {
            return Err(anyhow!(
                "_db9_sys_record_migration requires exactly 3 arguments"
            ));
        }

        let name = match eval_const_ast_expr(extract_expr(&args[0])?)? {
            Value::Text(v) => v,
            _ => {
                return Err(anyhow!("_db9_sys_record_migration name must be TEXT"));
            }
        };
        let checksum = match eval_const_ast_expr(extract_expr(&args[1])?)? {
            Value::Text(v) => v,
            _ => {
                return Err(anyhow!("_db9_sys_record_migration checksum must be TEXT"));
            }
        };
        let sql_preview = match eval_const_ast_expr(extract_expr(&args[2])?)? {
            Value::Text(v) => v,
            _ => {
                return Err(anyhow!(
                    "_db9_sys_record_migration sql_preview must be TEXT"
                ));
            }
        };

        let applied_at = Utc::now().to_rfc3339();
        let record = MigrationRecord {
            name: name.clone(),
            applied_at: applied_at.clone(),
            checksum,
            sql_preview,
        };
        self.store().record_migration(txn, record).await?;

        let schema = TableSchema {
            table_id: 0,
            name: "_db9_sys_record_migration".to_string(),
            columns: vec![
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "applied_at".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "status".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            version: 1,
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let rows = vec![Row::new(vec![
            Value::Text(name),
            Value::Text(applied_at),
            Value::Text("recorded".to_string()),
        ])];
        Ok((schema, rows))
    }
}
