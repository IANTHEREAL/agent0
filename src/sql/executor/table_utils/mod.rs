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
use crate::model::{ColumnDef, DataType, MigrationRecord, Row, SequenceDef, TableSchema, Value};
use crate::sql::catalog::virtual_tables::virtual_table_schema;
use crate::sql::error::SqlError;
use crate::sql::sequences::SequenceSession;
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
    if idx + 1 > key.len() {
        return false;
    }
    idx += 1; // task_type:u8
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

pub(crate) fn create_sequence_state_table_schema(
    full_name: &str,
    def: &SequenceDef,
) -> TableSchema {
    let mut schema = TableSchema::virtual_table(
        full_name,
        vec![
            ColumnDef::new("last_value", DataType::Int64, false),
            ColumnDef::new("log_cnt", DataType::Int64, false),
            ColumnDef::new("is_called", DataType::Boolean, false),
        ],
    );
    schema.version = 0;
    schema.owner = def.owner.clone();
    schema
}

impl Executor {
    pub(crate) async fn get_table_data(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
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
        _sequence_values: &mut SequenceSession,
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
                Value::Int64(i64::try_from(snap.rate_limited_count).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_attempts).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_budget_exhausted).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_timeout_aborts).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_conflict_by_reason[0]).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_conflict_by_reason[1]).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_conflict_by_reason[2]).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_conflict_by_reason[3]).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_conflict_by_reason[4]).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.retry_conflict_by_reason[5]).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.hnsw_graph_bytes_written).unwrap_or(i64::MAX)),
                Value::Int64(i64::try_from(snap.hnsw_serialize_duration_us).unwrap_or(i64::MAX)),
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
                        Value::Int64(entry.ddl_order),
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
                // Bounded: read pending AsyncTrigger identities from the V2 index
                // (+ gated legacy) — never a global due-queue scan of command
                // payloads. The async-trigger `task_id` doubles as its enqueue
                // timestamp, which is all this metric needs.
                let task_ids = system_store
                    .pending_async_trigger_task_ids(&mut sys_txn, self.tenant_keyspace())
                    .await?;
                for task_id in task_ids {
                    pending += 1;
                    if now_ms >= task_id {
                        latency_sum_ms += (now_ms - task_id) as u64;
                        latency_cnt += 1;
                    }
                    if task_id >= cutoff_recent_ms {
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

        if t_upper == "_DB9_SYS_STORAGE_STATS" || t_upper.ends_with("._DB9_SYS_STORAGE_STATS") {
            let mut schema = virtual_table_schema("_DB9_SYS_STORAGE_STATS").unwrap();
            schema.name = table_name.to_string();

            let cache = crate::storage_stats::global_storage_stats_cache();
            let all_stats = cache.get_all_for_keyspace(self.tenant_keyspace());
            let mut rows = Vec::new();
            for stats in all_stats {
                let db_name = self
                    .store()
                    .get_database_by_id(txn, stats.database_id)
                    .await?
                    .map(|db| db.name)
                    .unwrap_or_else(|| format!("db_{}", stats.database_id));

                let scanned_at = if stats.scanned_at_ms > 0 {
                    Value::Text(
                        chrono::DateTime::from_timestamp_millis(stats.scanned_at_ms)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default(),
                    )
                } else {
                    Value::Null
                };

                rows.push(Row::new(vec![
                    Value::Int64(stats.database_id as i64),
                    Value::Text(db_name),
                    Value::Int64(stats.data_bytes as i64),
                    Value::Int64(stats.index_bytes as i64),
                    Value::Int64(stats.metadata_bytes as i64),
                    Value::Int64(stats.total_bytes() as i64),
                    scanned_at,
                    Value::Int64(stats.scan_duration_ms),
                ]));
            }
            return Ok((schema, rows));
        }

        if t_upper == "_DB9_SYS_TABLE_STORAGE_STATS"
            || t_upper.ends_with("._DB9_SYS_TABLE_STORAGE_STATS")
        {
            let mut schema = virtual_table_schema("_DB9_SYS_TABLE_STORAGE_STATS").unwrap();
            schema.name = table_name.to_string();

            let cache = crate::storage_stats::global_storage_stats_cache();
            let db_stats = cache.get(self.tenant_keyspace(), db_id);

            let mut rows = Vec::new();
            if let Some(stats) = db_stats {
                let scanned_at = if stats.scanned_at_ms > 0 {
                    Value::Text(
                        chrono::DateTime::from_timestamp_millis(stats.scanned_at_ms)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default(),
                    )
                } else {
                    Value::Null
                };

                let table_names = self.store().list_tables(txn, db_id).await?;
                let mut id_to_name: HashMap<u64, String> = HashMap::new();
                for tname in &table_names {
                    if let Some(ts) = self.store().get_schema(txn, db_id, tname).await? {
                        id_to_name.insert(ts.table_id, tname.clone());
                    }
                }

                let mut table_entries: Vec<_> = stats.tables.values().collect();
                table_entries.sort_by_key(|t| t.table_id);

                for ts in table_entries {
                    let tname = id_to_name
                        .get(&ts.table_id)
                        .cloned()
                        .unwrap_or_else(|| format!("table_{}", ts.table_id));

                    rows.push(Row::new(vec![
                        Value::Int64(stats.database_id as i64),
                        Value::Int64(ts.table_id as i64),
                        Value::Text(tname),
                        Value::Int64(ts.data_bytes as i64),
                        Value::Int64(ts.index_bytes as i64),
                        Value::Int64(ts.total_bytes() as i64),
                        scanned_at.clone(),
                    ]));
                }
            }
            return Ok((schema, rows));
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

            if let Some((def, state)) = self
                .store()
                .get_sequence_state_by_name(txn, db_id, &[], candidate)
                .await?
            {
                let full_name = def.full_name();
                let schema = create_sequence_state_table_schema(&full_name, &def);
                let row = Row::new(vec![
                    Value::Int64(state.last_value),
                    Value::Int64(0), /* known divergence: PG log_cnt is a WAL pre-allocation counter; db9 has no WAL, always returns 0 */
                    Value::Boolean(state.is_called),
                ]);
                return Ok((schema, vec![row]));
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

        let schema = TableSchema::virtual_table(
            "generate_series",
            vec![ColumnDef::new(col_name, data_type, false)],
        );

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

        let schema = TableSchema::virtual_table(
            "_db9_sys_record_migration",
            vec![
                ColumnDef::new("name", DataType::Text, false),
                ColumnDef::new("applied_at", DataType::Text, false),
                ColumnDef::new("status", DataType::Text, false),
            ],
        );

        let rows = vec![Row::new(vec![
            Value::Text(name),
            Value::Text(applied_at),
            Value::Text("recorded".to_string()),
        ])];
        Ok((schema, rows))
    }
}
