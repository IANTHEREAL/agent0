//! Table data loading, view expansion, and generate_series utilities.
//!
//! Extracted from join.rs during Phase 2 refactoring (B.2a).

use super::super::ddl_export;
use super::super::information_schema::VirtualTableFilter;
use super::super::{parse_sql, ExecuteResult};
use super::core::Executor;
use crate::sql::error::SqlError;
use crate::types::{ColumnDef, DataType, MigrationRecord, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use chrono::Utc;
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, Statement};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use tikv_client::Transaction;

tokio::task_local! {
    static VIEW_EXPANSION_STACK: RefCell<Vec<String>>;
}

const MAX_VIEW_EXPANSION_DEPTH: usize = 64;

pub(crate) async fn with_view_expansion_stack<T>(future: impl Future<Output = T>) -> T {
    if VIEW_EXPANSION_STACK.try_with(|_| ()).is_ok() {
        future.await
    } else {
        VIEW_EXPANSION_STACK
            .scope(RefCell::new(Vec::new()), future)
            .await
    }
}

pub(crate) struct ViewExpansionGuard {
    view_name: String,
}

impl ViewExpansionGuard {
    pub(crate) fn push(view_name: String) -> Result<Self> {
        VIEW_EXPANSION_STACK.with(|stack| -> Result<()> {
            let mut stack = stack.borrow_mut();
            if stack.contains(&view_name) {
                return Err(anyhow!("recursive view detected: {}", view_name));
            }
            if stack.len() >= MAX_VIEW_EXPANSION_DEPTH {
                return Err(anyhow!(
                    "view expansion depth exceeded ({}): {}",
                    MAX_VIEW_EXPANSION_DEPTH,
                    view_name
                ));
            }
            stack.push(view_name.clone());
            Ok(())
        })?;
        Ok(Self { view_name })
    }
}

impl Drop for ViewExpansionGuard {
    fn drop(&mut self) {
        let view_name = &self.view_name;
        let _ = VIEW_EXPANSION_STACK.try_with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.last().map(|s| s == view_name).unwrap_or(false) {
                stack.pop();
            } else if let Some(pos) = stack.iter().rposition(|s| s == view_name) {
                stack.remove(pos);
            }
        });
    }
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
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table_name: &str,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        filter: &VirtualTableFilter,
    ) -> Result<(TableSchema, Vec<Row>)> {
        let t_lower = table_name.to_lowercase();

        // Normalize for virtual tables (some accept optional trailing `()` legacy syntax).
        let t_upper = table_name.trim_end_matches("()").to_uppercase();
        if t_upper == "_PGTIKV_SYS_OBSERVABILITY" || t_upper.ends_with("._PGTIKV_SYS_OBSERVABILITY")
        {
            let snap = self.observability().snapshot_summary();
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![
                    ColumnDef {
                        name: "window_seconds".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "statement_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "txn_commit_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "error_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "qps".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "tps".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "latency_avg_ms".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "latency_p99_ms".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "active_connections".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
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
        if t_upper == "_PGTIKV_SYS_QUERY_SAMPLES" || t_upper.ends_with("._PGTIKV_SYS_QUERY_SAMPLES")
        {
            let groups = self.observability().snapshot_query_samples();
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![
                    ColumnDef {
                        name: "query".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "sample_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "error_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "latency_avg_ms".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "latency_p99_ms".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "latency_max_ms".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "last_seen_ms_ago".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
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
        if t_upper == "_PGTIKV_SYS_EXPORT_DDL" || t_upper.ends_with("._PGTIKV_SYS_EXPORT_DDL") {
            let exported = ddl_export::export_all_ddl(self.store().as_ref(), txn, db_id).await?;
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![
                    ColumnDef {
                        name: "object_type".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "object_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "ddl_sql".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
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

        if t_upper == "_PGTIKV_SYS_MIGRATIONS" || t_upper.ends_with("._PGTIKV_SYS_MIGRATIONS") {
            let migrations = self.store().list_migrations(txn).await?;
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![
                    ColumnDef {
                        name: "name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "applied_at".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "checksum".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "sql_preview".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
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

        if t_upper == "_PGTIKV_SYS_TRIGGER_QUEUE_STATS"
            || t_upper.ends_with("._PGTIKV_SYS_TRIGGER_QUEUE_STATS")
        {
            use super::super::trigger_queue::{
                encode_trigger_dlq_prefix, encode_trigger_queue_prefix,
            };
            use super::super::trigger_queue::{now_ms_i64, EventStatus, TriggerEvent};
            use std::ops::Bound;
            use tikv_client::BoundRange;

            let now_ms = now_ms_i64();
            let cutoff_recent_ms = now_ms.saturating_sub(60_000);

            let mut pending = 0i64;
            let mut processing = 0i64;
            let mut failed = 0i64;
            let mut latency_sum_ms: u64 = 0;
            let mut latency_cnt: u64 = 0;
            let mut events_last_min = 0i64;

            let prefix = encode_trigger_queue_prefix();
            let mut end = prefix.clone();
            end.push(0xFF);

            let mut start: Option<Vec<u8>> = None;
            loop {
                let range: BoundRange = match start.as_ref() {
                    None => (prefix.clone()..end.clone()).into(),
                    Some(last) => BoundRange::new(
                        Bound::Excluded(last.clone().into()),
                        Bound::Excluded(end.clone().into()),
                    ),
                };

                let mut batch_last = None;
                let mut scanned = 0usize;
                for pair in txn.scan(range, 256).await? {
                    scanned += 1;
                    let key_slice: &[u8] = pair.key().as_ref().into();
                    let key_vec = key_slice.to_vec();
                    batch_last = Some(key_vec);

                    let ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    match ev.status {
                        EventStatus::Pending => pending += 1,
                        EventStatus::Processing => processing += 1,
                        EventStatus::Failed => failed += 1,
                        EventStatus::Done => {}
                    }

                    if now_ms >= ev.created_at_ms {
                        latency_sum_ms += (now_ms - ev.created_at_ms) as u64;
                        latency_cnt += 1;
                    }

                    // `id >> 22` is the event timestamp in ms.
                    if (ev.id >> 22) as i64 >= cutoff_recent_ms {
                        events_last_min += 1;
                    }
                }

                if scanned < 256 {
                    break;
                }
                start = batch_last;
                if start.is_none() {
                    break;
                }
            }

            // DLQ count (keys only; values not needed).
            let dlq_prefix = encode_trigger_dlq_prefix();
            let mut dlq_end = dlq_prefix.clone();
            dlq_end.push(0xFF);
            let mut dlq_count = 0i64;
            let mut dlq_start: Option<Vec<u8>> = None;
            loop {
                let range: BoundRange = match dlq_start.as_ref() {
                    None => (dlq_prefix.clone()..dlq_end.clone()).into(),
                    Some(last) => BoundRange::new(
                        Bound::Excluded(last.clone().into()),
                        Bound::Excluded(dlq_end.clone().into()),
                    ),
                };

                let mut batch_last = None;
                let mut scanned = 0usize;
                for pair in txn.scan(range, 256).await? {
                    scanned += 1;
                    let key_slice: &[u8] = pair.key().as_ref().into();
                    let key_vec = key_slice.to_vec();
                    batch_last = Some(key_vec);
                    dlq_count += 1;
                }

                if scanned < 256 {
                    break;
                }
                dlq_start = batch_last;
                if dlq_start.is_none() {
                    break;
                }
            }

            let avg_latency_ms = if latency_cnt == 0 {
                0.0
            } else {
                (latency_sum_ms as f64) / (latency_cnt as f64)
            };

            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![
                    ColumnDef {
                        name: "keyspace".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "pending".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "processing".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "failed".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "dlq_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "avg_latency_ms".to_string(),
                        data_type: DataType::Float64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "events_per_min".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
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

        if t_upper == "_PGTIKV_SYS_TRIGGER_DLQ" || t_upper.ends_with("._PGTIKV_SYS_TRIGGER_DLQ") {
            use super::super::trigger_queue::{encode_trigger_dlq_prefix, TriggerEvent, TriggerOp};
            use std::ops::Bound;
            use tikv_client::BoundRange;

            let prefix = encode_trigger_dlq_prefix();
            let mut end = prefix.clone();
            end.push(0xFF);

            let mut rows = Vec::new();
            let mut start: Option<Vec<u8>> = None;
            loop {
                let range: BoundRange = match start.as_ref() {
                    None => (prefix.clone()..end.clone()).into(),
                    Some(last) => BoundRange::new(
                        Bound::Excluded(last.clone().into()),
                        Bound::Excluded(end.clone().into()),
                    ),
                };

                let mut batch_last = None;
                let mut scanned = 0usize;
                for pair in txn.scan(range, 256).await? {
                    scanned += 1;
                    let key_slice: &[u8] = pair.key().as_ref().into();
                    let key_vec = key_slice.to_vec();
                    batch_last = Some(key_vec);

                    let ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let op = match ev.operation {
                        TriggerOp::Insert => "INSERT",
                        TriggerOp::Update => "UPDATE",
                        TriggerOp::Delete => "DELETE",
                    };

                    rows.push(Row::new(vec![
                        Value::Int64(i64::try_from(ev.id).unwrap_or(i64::MAX)),
                        Value::Text(ev.trigger_name),
                        Value::Text(ev.table_name),
                        Value::Text(op.to_string()),
                        ev.error_msg.map(Value::Text).unwrap_or(Value::Null),
                        Value::Int64(i64::from(ev.retry_count)),
                        Value::Int64(ev.created_at_ms),
                    ]));
                }

                if scanned < 256 {
                    break;
                }
                start = batch_last;
                if start.is_none() {
                    break;
                }
            }

            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![
                    ColumnDef {
                        name: "id".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "trigger_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "table_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "operation".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "error_msg".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "retry_count".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "created_at_ms".to_string(),
                        data_type: DataType::Int64,
                        nullable: false,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
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

            return Ok((schema, rows));
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
            if let Some(view_def) = self.store().get_view(txn, db_id, candidate).await? {
                let candidate = candidate.clone();
                return with_view_expansion_stack(async move {
                    let _guard = ViewExpansionGuard::push(candidate.clone())?;
                    let result = self
                        .execute_view_query(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &view_def.query,
                            ctes,
                        )
                        .await?;
                    match result {
                        ExecuteResult::Select {
                            columns,
                            column_types,
                            rows,
                            timezone: _,
                        } => {
                            let inferred_types = column_types.unwrap_or_else(|| {
                                crate::types::infer_column_types_from_rows(&rows, columns.len())
                            });

                            let schema = TableSchema {
                                table_id: 0,
                                name: candidate.clone(),
                                columns: columns
                                    .iter()
                                    .enumerate()
                                    .map(|(i, n)| ColumnDef {
                                        name: n.clone(),
                                        // INTENTIONAL: index guard — unreachable when types match columns
                                        data_type: inferred_types
                                            .get(i)
                                            .cloned()
                                            .unwrap_or(DataType::Text),
                                        nullable: true,
                                        primary_key: false,
                                        unique: false,
                                        is_serial: false,
                                        default_expr: None,
                                    })
                                    .collect(),
                                pk_constraint_name: None,
                                pk_indices: vec![],
                                indexes: vec![],
                                version: 1,
                                check_constraints: vec![],
                                foreign_keys: vec![],
                                owner: String::new(),
                                from_alias: None,
                            };
                            Ok((schema, rows))
                        }
                        _ => Err(anyhow!("View must return SELECT result")),
                    }
                })
                .await;
            }

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
                    "_pgtikv_sys_record_migration requires expression arguments"
                )),
            }
        }

        if args.len() != 3 {
            return Err(anyhow!(
                "_pgtikv_sys_record_migration requires exactly 3 arguments"
            ));
        }

        let name = match eval_const_ast_expr(extract_expr(&args[0])?)? {
            Value::Text(v) => v,
            _ => {
                return Err(anyhow!("_pgtikv_sys_record_migration name must be TEXT"));
            }
        };
        let checksum = match eval_const_ast_expr(extract_expr(&args[1])?)? {
            Value::Text(v) => v,
            _ => {
                return Err(anyhow!(
                    "_pgtikv_sys_record_migration checksum must be TEXT"
                ));
            }
        };
        let sql_preview = match eval_const_ast_expr(extract_expr(&args[2])?)? {
            Value::Text(v) => v,
            _ => {
                return Err(anyhow!(
                    "_pgtikv_sys_record_migration sql_preview must be TEXT"
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
            name: "_pgtikv_sys_record_migration".to_string(),
            columns: vec![
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "applied_at".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "status".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
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

    pub(crate) fn execute_view_query<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        view_query: &'a str,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let ast = parse_sql(view_query)?;
            if let Some(Statement::Query(q)) = ast.into_iter().next() {
                self.execute_query_with_outer_ctes(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &q,
                    ctes,
                )
                .await
            } else {
                Err(anyhow!("Invalid view query"))
            }
        })
    }
}

pub(crate) const DEFAULT_MAX_GENERATE_SERIES_ROWS: usize = 1_000_000;
pub(crate) const FLOAT8_OFFSET_ADVANCE_MAX_ITER: usize = 1024;

pub(crate) fn max_generate_series_rows() -> usize {
    std::env::var("PGTIKV_MAX_GENERATE_SERIES_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_GENERATE_SERIES_ROWS)
}

#[cfg(test)]
fn generate_series_values(
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<(Vec<Value>, DataType)> {
    generate_series_values_limited(start, stop, step, 0, None, usize::MAX)
}

pub(crate) fn generate_series_values_limited(
    start: &Value,
    stop: &Value,
    step: &Value,
    offset: usize,
    limit: Option<usize>,
    max_rows: usize,
) -> Result<(Vec<Value>, DataType)> {
    let too_many_rows = || {
        anyhow!(
            "generate_series exceeded max rows ({}); set PGTIKV_MAX_GENERATE_SERIES_ROWS to override",
            max_rows
        )
    };

    let mut remaining = limit.unwrap_or(usize::MAX);

    match (start, stop) {
        (Value::Int32(s), Value::Int32(e)) => {
            let step_val = match step {
                Value::Null => 1,
                Value::Int32(st) => *st,
                Value::Int64(st) => i32::try_from(*st)
                    .map_err(|_| anyhow!("step out of range for integer generate_series"))?,
                _ => return Err(anyhow!("Invalid step type for integer generate_series")),
            };
            if step_val == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            if remaining == 0 {
                return Ok((Vec::new(), DataType::Int32));
            }

            let mut current = *s;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_val) * offset_i128;
                let current_i128 = i128::from(*s) + delta_i128;
                let Ok(cur) = i32::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Int32));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > 0 {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int32(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int32(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Int32))
        }
        (Value::Int64(s), Value::Int64(e)) => {
            let step_val = match step {
                Value::Null => 1i64,
                Value::Int32(st) => *st as i64,
                Value::Int64(st) => *st,
                _ => return Err(anyhow!("Invalid step type for bigint generate_series")),
            };
            if step_val == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            if remaining == 0 {
                return Ok((Vec::new(), DataType::Int64));
            }

            let mut current = *s;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_val) * offset_i128;
                let current_i128 = i128::from(*s) + delta_i128;
                let Ok(cur) = i64::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Int64));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > 0 {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Int64))
        }
        (Value::Int32(_), Value::Int64(_)) | (Value::Int64(_), Value::Int32(_)) => {
            let s64 = match start {
                Value::Int32(v) => *v as i64,
                Value::Int64(v) => *v,
                _ => unreachable!(),
            };
            let e64 = match stop {
                Value::Int32(v) => *v as i64,
                Value::Int64(v) => *v,
                _ => unreachable!(),
            };
            let step_val = match step {
                Value::Null => 1i64,
                Value::Int32(st) => *st as i64,
                Value::Int64(st) => *st,
                _ => return Err(anyhow!("Invalid step type for bigint generate_series")),
            };
            if step_val == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            if remaining == 0 {
                return Ok((Vec::new(), DataType::Int64));
            }

            let mut current = s64;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_val) * offset_i128;
                let current_i128 = i128::from(s64) + delta_i128;
                let Ok(cur) = i64::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Int64));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > 0 {
                while current <= e64 {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= e64 {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Int64(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Int64))
        }
        (Value::Float64(s), Value::Float64(e)) => {
            let step_val = match step {
                Value::Null => 1.0,
                Value::Float64(st) => *st,
                Value::Int32(st) => *st as f64,
                Value::Int64(st) => *st as f64,
                _ => return Err(anyhow!("Invalid step type for float8 generate_series")),
            };
            if step_val == 0.0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            if remaining == 0 {
                return Ok((Vec::new(), DataType::Float64));
            }

            let mut current = *s;
            if offset > 0 {
                if !step_val.is_finite() {
                    return Ok((Vec::new(), DataType::Float64));
                }
                if offset <= FLOAT8_OFFSET_ADVANCE_MAX_ITER {
                    if step_val > 0.0 {
                        for _ in 0..offset {
                            if current > *e + f64::EPSILON {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            if current >= *e {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                            current = next;
                        }
                    } else {
                        for _ in 0..offset {
                            if current < *e - f64::EPSILON {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            if current <= *e {
                                return Ok((Vec::new(), DataType::Float64));
                            }
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                            current = next;
                        }
                    }
                } else {
                    if step_val > 0.0 {
                        if current < *e {
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                        }
                    } else {
                        if current > *e {
                            let next = current + step_val;
                            if next == current {
                                return Err(anyhow!(
                                    "generate_series step is too small to make progress for float8"
                                ));
                            }
                        }
                    }

                    let prev = step_val.mul_add((offset - 1) as f64, current);
                    if step_val > 0.0 {
                        if prev >= *e {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    } else {
                        if prev <= *e {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    }

                    current = step_val.mul_add(offset as f64, current);
                    if step_val > 0.0 {
                        if current > *e + f64::EPSILON {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    } else {
                        if current < *e - f64::EPSILON {
                            return Ok((Vec::new(), DataType::Float64));
                        }
                    }
                }
            }

            if step_val > 0.0 {
                if current < *e {
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                }
                let mut values = Vec::new();
                while current <= *e + f64::EPSILON {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Float64(current));
                    remaining = remaining.saturating_sub(1);
                    if current >= *e {
                        break;
                    }
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                    current = next;
                }
                Ok((values, DataType::Float64))
            } else {
                if current > *e {
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                }
                let mut values = Vec::new();
                while current >= *e - f64::EPSILON {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Float64(current));
                    remaining = remaining.saturating_sub(1);
                    if current <= *e {
                        break;
                    }
                    let next = current + step_val;
                    if next == current {
                        return Err(anyhow!(
                            "generate_series step is too small to make progress for float8"
                        ));
                    }
                    current = next;
                }
                Ok((values, DataType::Float64))
            }
        }
        (Value::Timestamp(s), Value::Timestamp(e)) => {
            let step_interval = match step {
                Value::Interval(iv) => iv.clone(),
                _ => {
                    return Err(anyhow!(
                        "generate_series with timestamps requires interval step"
                    ))
                }
            };
            let step_ms = interval_to_millis(&step_interval);
            if step_ms == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            if remaining == 0 {
                return Ok((Vec::new(), DataType::Timestamp));
            }

            let mut current = *s;
            if offset > 0 {
                let offset_i128 = offset as i128;
                let delta_i128 = i128::from(step_ms) * offset_i128;
                let current_i128 = i128::from(*s) + delta_i128;
                let Ok(cur) = i64::try_from(current_i128) else {
                    return Ok((Vec::new(), DataType::Timestamp));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_ms > 0 {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Timestamp(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_ms) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Timestamp(current));
                    remaining = remaining.saturating_sub(1);
                    current = match current.checked_add(step_ms) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::Timestamp))
        }
        (Value::Date(s), Value::Date(e)) => {
            let step_interval = match step {
                Value::Interval(iv) => iv.clone(),
                _ => return Err(anyhow!("generate_series with dates requires interval step")),
            };
            if step_interval.months == 0 && step_interval.millis == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            // Validate step size before honoring LIMIT/OFFSET.
            let step_ms = interval_to_millis(&step_interval);
            if step_ms == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }

            if remaining == 0 {
                return Ok((Vec::new(), DataType::TimestampTz));
            }

            let tz = crate::types::timestamp::TimeZoneSpec::parse(
                crate::session_context::current_timezone().as_ref(),
            );
            let naive_date_midnight_timestamptz = |date: chrono::NaiveDate| -> Result<i64> {
                let naive = date
                    .and_hms_opt(0, 0, 0)
                    .ok_or_else(|| anyhow!("Invalid date"))?;
                tz.timestamp_millis_from_local_datetime(naive)
            };
            let date_midnight_timestamptz = |days: i32| -> Result<i64> {
                let date = crate::types::date::date_days_to_naive_date(days)?;
                naive_date_midnight_timestamptz(date)
            };

            let mut values = Vec::new();

            const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
            let has_subday_component = step_interval.millis % MILLIS_PER_DAY != 0;
            // For calendar steps (month/day), advance by calendar months/days instead of fixed
            // milliseconds. This avoids month length drift and DST drift.
            if !has_subday_component {
                let step_days_i64 = step_interval.millis / MILLIS_PER_DAY;
                if step_interval.months == 0 {
                    let Ok(step_days) = i32::try_from(step_days_i64) else {
                        return Ok((Vec::new(), DataType::TimestampTz));
                    };
                    if step_days == 0 {
                        return Err(anyhow!("step size cannot equal zero"));
                    }

                    let mut current = *s;
                    if offset > 0 {
                        let offset_i128 = offset as i128;
                        let delta_i128 = i128::from(step_days) * offset_i128;
                        let current_i128 = i128::from(*s) + delta_i128;
                        let Ok(cur_i32) = i32::try_from(current_i128) else {
                            return Ok((Vec::new(), DataType::TimestampTz));
                        };
                        current = cur_i32;
                    }

                    if step_days > 0 {
                        while current <= *e {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values.push(Value::Timestamp(date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            current = match current.checked_add(step_days) {
                                Some(next) => next,
                                None => break,
                            };
                        }
                    } else {
                        while current >= *e {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values.push(Value::Timestamp(date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            current = match current.checked_add(step_days) {
                                Some(next) => next,
                                None => break,
                            };
                        }
                    }
                } else {
                    use chrono::Datelike;

                    let step_days = step_days_i64;
                    let add_months_clamped =
                        |date: chrono::NaiveDate, months: i32| -> Option<chrono::NaiveDate> {
                            if months == 0 {
                                return Some(date);
                            }

                            let year = i64::from(date.year());
                            let month0 = i64::from(date.month0());
                            let total_months = year
                                .checked_mul(12)?
                                .checked_add(month0)?
                                .checked_add(i64::from(months))?;
                            let new_year = i32::try_from(total_months.div_euclid(12)).ok()?;
                            let new_month0 = total_months.rem_euclid(12);
                            let new_month = u32::try_from(new_month0 + 1).ok()?;

                            let day = date.day();
                            let first_of_next_month = if new_month == 12 {
                                chrono::NaiveDate::from_ymd_opt(new_year.checked_add(1)?, 1, 1)?
                            } else {
                                chrono::NaiveDate::from_ymd_opt(new_year, new_month + 1, 1)?
                            };
                            let last_day = first_of_next_month.pred_opt()?.day();
                            chrono::NaiveDate::from_ymd_opt(new_year, new_month, day.min(last_day))
                        };

                    let apply_step = |date: chrono::NaiveDate| -> Option<chrono::NaiveDate> {
                        let with_months = add_months_clamped(date, step_interval.months)?;
                        with_months.checked_add_signed(chrono::Duration::days(step_days))
                    };

                    let mut current = crate::types::date::date_days_to_naive_date(*s)?;
                    let stop = crate::types::date::date_days_to_naive_date(*e)?;
                    let step_forward =
                        step_interval.months > 0 || (step_interval.months == 0 && step_days > 0);

                    if offset > 0 {
                        for _ in 0..offset {
                            let Some(next) = apply_step(current) else {
                                return Ok((Vec::new(), DataType::TimestampTz));
                            };
                            current = next;
                        }
                    }

                    if step_forward {
                        while current <= stop {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values
                                .push(Value::Timestamp(naive_date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            if current == stop {
                                break;
                            }
                            let next = match apply_step(current) {
                                Some(next) => next,
                                None => break,
                            };
                            if next <= current {
                                return Err(anyhow!(
                                    "generate_series interval step does not make forward progress for date"
                                ));
                            }
                            current = next;
                        }
                    } else {
                        while current >= stop {
                            if remaining == 0 {
                                break;
                            }
                            if values.len() >= max_rows {
                                return Err(too_many_rows());
                            }
                            values
                                .push(Value::Timestamp(naive_date_midnight_timestamptz(current)?));
                            remaining = remaining.saturating_sub(1);
                            if current == stop {
                                break;
                            }
                            let next = match apply_step(current) {
                                Some(next) => next,
                                None => break,
                            };
                            if next >= current {
                                return Err(anyhow!(
                                    "generate_series interval step does not make backward progress for date"
                                ));
                            }
                            current = next;
                        }
                    }
                }
            } else {
                let start_ms = date_midnight_timestamptz(*s)?;
                let stop_ms = date_midnight_timestamptz(*e)?;

                let mut current = start_ms;
                if offset > 0 {
                    let offset_i128 = offset as i128;
                    let delta_i128 = i128::from(step_ms) * offset_i128;
                    let current_i128 = i128::from(current) + delta_i128;
                    let Ok(cur) = i64::try_from(current_i128) else {
                        return Ok((Vec::new(), DataType::TimestampTz));
                    };
                    current = cur;
                }

                if step_ms > 0 {
                    while current <= stop_ms {
                        if remaining == 0 {
                            break;
                        }
                        if values.len() >= max_rows {
                            return Err(too_many_rows());
                        }
                        values.push(Value::Timestamp(current));
                        remaining = remaining.saturating_sub(1);
                        current = match current.checked_add(step_ms) {
                            Some(next) => next,
                            None => break,
                        };
                    }
                } else {
                    while current >= stop_ms {
                        if remaining == 0 {
                            break;
                        }
                        if values.len() >= max_rows {
                            return Err(too_many_rows());
                        }
                        values.push(Value::Timestamp(current));
                        remaining = remaining.saturating_sub(1);
                        current = match current.checked_add(step_ms) {
                            Some(next) => next,
                            None => break,
                        };
                    }
                }
            }

            Ok((values, DataType::TimestampTz))
        }
        (Value::Numeric(s), Value::Numeric(e)) => {
            let step_val = match step {
                Value::Null => rust_decimal::Decimal::ONE,
                Value::Numeric(st) => *st,
                Value::Float64(st) => rust_decimal::Decimal::try_from(*st).map_err(|_| {
                    SqlError::InvalidInputSyntax {
                        type_name: "numeric".into(),
                        value: st.to_string(),
                    }
                })?,
                Value::Int32(st) => rust_decimal::Decimal::from(*st),
                Value::Int64(st) => rust_decimal::Decimal::from(*st),
                _ => return Err(anyhow!("Invalid step type for numeric generate_series")),
            };
            if step_val.is_zero() {
                return Err(anyhow!("step size cannot equal zero"));
            }

            let data_type = DataType::Numeric {
                precision: None,
                scale: None,
            };

            if remaining == 0 {
                return Ok((Vec::new(), data_type));
            }

            let mut current = *s;
            if offset > 0 {
                let Ok(offset_i128) = i128::try_from(offset) else {
                    return Ok((Vec::new(), data_type));
                };
                let offset_dec = rust_decimal::Decimal::from_i128_with_scale(offset_i128, 0);
                let Some(delta) = step_val.checked_mul(offset_dec) else {
                    return Ok((Vec::new(), data_type));
                };
                let Some(cur) = current.checked_add(delta) else {
                    return Ok((Vec::new(), data_type));
                };
                current = cur;
            }

            let mut values = Vec::new();
            if step_val > rust_decimal::Decimal::ZERO {
                while current <= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Numeric(current));
                    remaining = remaining.saturating_sub(1);
                    if current == *e {
                        break;
                    }
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                while current >= *e {
                    if remaining == 0 {
                        break;
                    }
                    if values.len() >= max_rows {
                        return Err(too_many_rows());
                    }
                    values.push(Value::Numeric(current));
                    remaining = remaining.saturating_sub(1);
                    if current == *e {
                        break;
                    }
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, data_type))
        }
        _ => Err(anyhow!(
            "generate_series requires numeric or timestamp arguments, got {:?} and {:?}",
            start,
            stop
        )),
    }
}

pub(crate) fn interval_to_millis(iv: &crate::types::IntervalValue) -> i64 {
    iv.to_millis_approx()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Offset, TimeZone, Timelike};
    use std::sync::Arc;

    fn with_session_timezone<T>(timezone: &str, f: impl FnOnce() -> T) -> T {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(crate::session_context::with_timezone(
            Arc::from(timezone),
            async move { f() },
        ))
    }

    #[test]
    fn generate_series_int32_edge_overflow_does_not_loop() {
        let (values, ty) = generate_series_values(
            &Value::Int32(i32::MAX - 1),
            &Value::Int32(i32::MAX),
            &Value::Int32(1),
        )
        .unwrap();
        assert_eq!(ty, DataType::Int32);
        assert_eq!(
            values,
            vec![Value::Int32(i32::MAX - 1), Value::Int32(i32::MAX)]
        );

        let (values, ty) = generate_series_values(
            &Value::Int32(i32::MIN + 1),
            &Value::Int32(i32::MIN),
            &Value::Int32(-1),
        )
        .unwrap();
        assert_eq!(ty, DataType::Int32);
        assert_eq!(
            values,
            vec![Value::Int32(i32::MIN + 1), Value::Int32(i32::MIN)]
        );
    }

    #[test]
    fn generate_series_limited_applies_offset_limit_int32() {
        let (values, ty) = generate_series_values_limited(
            &Value::Int32(1),
            &Value::Int32(10),
            &Value::Null,
            2,
            Some(3),
            100,
        )
        .unwrap();
        assert_eq!(ty, DataType::Int32);
        assert_eq!(
            values,
            vec![Value::Int32(3), Value::Int32(4), Value::Int32(5)]
        );
    }

    #[test]
    fn generate_series_limited_large_offset_is_fast_and_correct() {
        let (values, ty) = generate_series_values_limited(
            &Value::Int32(1),
            &Value::Int32(1_000_000_000),
            &Value::Null,
            999_999_999,
            Some(1),
            10,
        )
        .unwrap();
        assert_eq!(ty, DataType::Int32);
        assert_eq!(values, vec![Value::Int32(1_000_000_000)]);
    }

    #[test]
    fn generate_series_limited_large_offset_is_fast_and_correct_float8() {
        let (values, ty) = generate_series_values_limited(
            &Value::Float64(1.0),
            &Value::Float64(1e16),
            &Value::Float64(1.0),
            1_000_000_000,
            Some(1),
            10,
        )
        .unwrap();
        assert_eq!(ty, DataType::Float64);
        assert_eq!(values, vec![Value::Float64(1_000_000_001.0)]);
    }

    #[test]
    fn generate_series_limited_large_offset_beyond_range_float8_returns_empty() {
        let (values, ty) = generate_series_values_limited(
            &Value::Float64(0.0),
            &Value::Float64(1e-16),
            &Value::Float64(1e-19),
            2000,
            Some(1),
            10,
        )
        .unwrap();
        assert_eq!(ty, DataType::Float64);
        assert!(values.is_empty());
    }

    #[test]
    fn generate_series_limited_enforces_max_rows() {
        let err = generate_series_values_limited(
            &Value::Int32(1),
            &Value::Int32(100),
            &Value::Null,
            0,
            None,
            10,
        )
        .unwrap_err();
        assert!(err.to_string().contains("exceeded max rows"));
    }

    #[test]
    fn generate_series_limited_limit_avoids_max_rows_error() {
        let (values, ty) = generate_series_values_limited(
            &Value::Int32(1),
            &Value::Int32(1_000_000_000),
            &Value::Null,
            0,
            Some(1),
            10,
        )
        .unwrap();
        assert_eq!(ty, DataType::Int32);
        assert_eq!(values, vec![Value::Int32(1)]);
    }

    #[test]
    fn generate_series_int32_step_out_of_range_errors() {
        let err = generate_series_values(
            &Value::Int32(1),
            &Value::Int32(2),
            &Value::Int64(i64::from(i32::MAX) + 1),
        )
        .unwrap_err();
        assert!(err.to_string().contains("step out of range"));
    }

    #[test]
    fn generate_series_int64_edge_overflow_does_not_loop() {
        let (values, ty) = generate_series_values(
            &Value::Int64(i64::MAX - 1),
            &Value::Int64(i64::MAX),
            &Value::Int64(1),
        )
        .unwrap();
        assert_eq!(ty, DataType::Int64);
        assert_eq!(
            values,
            vec![Value::Int64(i64::MAX - 1), Value::Int64(i64::MAX)]
        );

        let (values, ty) = generate_series_values(
            &Value::Int64(i64::MIN + 1),
            &Value::Int64(i64::MIN),
            &Value::Int64(-1),
        )
        .unwrap();
        assert_eq!(ty, DataType::Int64);
        assert_eq!(
            values,
            vec![Value::Int64(i64::MIN + 1), Value::Int64(i64::MIN)]
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn generate_series_limited_offset_mul_overflow_still_returns_rows() {
        let (values, ty) = generate_series_values_limited(
            &Value::Int64(-9_000_000_000_000_000_000i64),
            &Value::Int64(9_000_000_000_000_000_000i64),
            &Value::Int64(2),
            5_000_000_000_000_000_000usize,
            Some(1),
            10,
        )
        .unwrap();
        assert_eq!(ty, DataType::Int64);
        assert_eq!(values, vec![Value::Int64(1_000_000_000_000_000_000i64)]);
    }

    #[test]
    fn generate_series_timestamp_overflow_does_not_loop() {
        let (values, ty) = generate_series_values(
            &Value::Timestamp(1),
            &Value::Timestamp(1),
            &Value::Interval(crate::types::IntervalValue::from_millis(i64::MAX)),
        )
        .unwrap();
        assert_eq!(ty, DataType::Timestamp);
        assert_eq!(values, vec![Value::Timestamp(1)]);
    }

    #[test]
    fn generate_series_date_overflow_does_not_loop() {
        let step = Value::Interval(crate::types::IntervalValue::from_millis(i64::MAX));
        let (values, ty) = generate_series_values(&Value::Date(1), &Value::Date(1), &step).unwrap();
        assert_eq!(ty, DataType::TimestampTz);

        let tz = crate::types::timestamp::TimeZoneSpec::parse(
            crate::session_context::current_timezone().as_ref(),
        );
        let date = crate::types::date::date_days_to_naive_date(1).unwrap();
        let naive = date.and_hms_opt(0, 0, 0).unwrap();
        let expected = tz.timestamp_millis_from_local_datetime(naive).unwrap();
        assert_eq!(values, vec![Value::Timestamp(expected)]);
    }

    #[test]
    fn generate_series_date_sub_day_step_includes_intermediate() {
        let start_days = crate::types::date::parse_date_days("2024-01-01").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-01-02").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_millis(
            12 * 60 * 60 * 1000,
        ));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);

        let tz = chrono_tz::America::Los_Angeles;
        let d1 = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let t0 = tz
            .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t12 = tz
            .from_local_datetime(&d1.and_hms_opt(12, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t24 = tz
            .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();

        assert_eq!(
            values,
            vec![
                Value::Timestamp(t0),
                Value::Timestamp(t12),
                Value::Timestamp(t24)
            ]
        );
    }

    #[test]
    fn generate_series_date_sub_day_step_across_dst_start_does_not_error() {
        let start_days = crate::types::date::parse_date_days("2024-03-10").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-03-11").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_millis(60 * 60 * 1000));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);
        assert_eq!(values.len(), 24);

        let tz = chrono_tz::America::Los_Angeles;
        let mut hours = Vec::with_capacity(values.len());
        for v in &values {
            let Value::Timestamp(ms) = v else {
                panic!("expected timestamptz values");
            };
            hours.push(tz.timestamp_millis_opt(*ms).single().unwrap().hour());
        }
        assert_eq!(hours.iter().filter(|&&h| h == 2).count(), 0);
        assert!(hours.iter().any(|&h| h == 3));
    }

    #[test]
    fn generate_series_date_sub_day_step_across_dst_end_does_not_error() {
        let start_days = crate::types::date::parse_date_days("2024-11-03").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-11-04").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_millis(60 * 60 * 1000));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);
        assert_eq!(values.len(), 26);

        let tz = chrono_tz::America::Los_Angeles;
        let mut offsets = std::collections::BTreeSet::new();
        let mut hour_1_count = 0;
        for v in &values {
            let Value::Timestamp(ms) = v else {
                panic!("expected timestamptz values");
            };
            let dt = tz.timestamp_millis_opt(*ms).single().unwrap();
            if dt.hour() == 1 {
                hour_1_count += 1;
                offsets.insert(dt.offset().fix().local_minus_utc());
            }
        }
        assert_eq!(hour_1_count, 2);
        assert_eq!(offsets.len(), 2);
        assert!(offsets.contains(&(-7 * 3600)));
        assert!(offsets.contains(&(-8 * 3600)));
    }

    #[test]
    fn generate_series_date_interval_does_not_truncate_remainder() {
        let start_days = crate::types::date::parse_date_days("2024-01-01").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-01-03").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_millis(
            36 * 60 * 60 * 1000,
        ));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);

        let tz = chrono_tz::America::Los_Angeles;
        let d1 = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let t0 = tz
            .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t36 = tz
            .from_local_datetime(&d2.and_hms_opt(12, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();

        assert_eq!(values, vec![Value::Timestamp(t0), Value::Timestamp(t36)]);
    }

    #[test]
    fn generate_series_date_month_step_across_dst_start_keeps_local_midnight() {
        let start_days = crate::types::date::parse_date_days("2024-03-01").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-05-01").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_months(1));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);

        let tz = chrono_tz::America::Los_Angeles;
        let d1 = chrono::NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2024, 4, 1).unwrap();
        let d3 = chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap();

        let t1 = tz
            .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t2 = tz
            .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t3 = tz
            .from_local_datetime(&d3.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();

        assert_eq!(
            values,
            vec![
                Value::Timestamp(t1),
                Value::Timestamp(t2),
                Value::Timestamp(t3)
            ]
        );
    }

    #[test]
    fn generate_series_date_day_step_across_dst_start_keeps_local_midnight() {
        let start_days = crate::types::date::parse_date_days("2024-03-09").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-03-11").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_millis(
            24 * 60 * 60 * 1000,
        ));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);

        let tz = chrono_tz::America::Los_Angeles;
        let d1 = chrono::NaiveDate::from_ymd_opt(2024, 3, 9).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2024, 3, 10).unwrap();
        let d3 = chrono::NaiveDate::from_ymd_opt(2024, 3, 11).unwrap();

        let t1 = tz
            .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t2 = tz
            .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t3 = tz
            .from_local_datetime(&d3.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();

        assert_eq!(
            values,
            vec![
                Value::Timestamp(t1),
                Value::Timestamp(t2),
                Value::Timestamp(t3)
            ]
        );
    }

    #[test]
    fn generate_series_date_day_step_across_dst_end_keeps_local_midnight() {
        let start_days = crate::types::date::parse_date_days("2024-11-02").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-11-04").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::from_millis(
            24 * 60 * 60 * 1000,
        ));

        let (values, ty) = with_session_timezone("America/Los_Angeles", || {
            generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
                .unwrap()
        });

        assert_eq!(ty, DataType::TimestampTz);

        let tz = chrono_tz::America::Los_Angeles;
        let d1 = chrono::NaiveDate::from_ymd_opt(2024, 11, 2).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2024, 11, 3).unwrap();
        let d3 = chrono::NaiveDate::from_ymd_opt(2024, 11, 4).unwrap();

        let t1 = tz
            .from_local_datetime(&d1.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t2 = tz
            .from_local_datetime(&d2.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let t3 = tz
            .from_local_datetime(&d3.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();

        assert_eq!(
            values,
            vec![
                Value::Timestamp(t1),
                Value::Timestamp(t2),
                Value::Timestamp(t3)
            ]
        );
    }

    #[test]
    fn generate_series_date_mixed_sign_month_day_step_progress_guard_errors() {
        let start_days = crate::types::date::parse_date_days("2024-01-01").unwrap();
        let stop_days = crate::types::date::parse_date_days("2024-01-03").unwrap();
        let step = Value::Interval(crate::types::IntervalValue::new(
            1,
            -31_i64 * 24 * 60 * 60 * 1000,
        ));

        let err = generate_series_values(&Value::Date(start_days), &Value::Date(stop_days), &step)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("does not make forward progress for date"));
    }

    #[test]
    fn generate_series_float8_progress_guard_errors() {
        let err = generate_series_values(
            &Value::Float64(1e16),
            &Value::Float64(1e16 + 1e6),
            &Value::Float64(1.0),
        )
        .unwrap_err();
        assert!(err.to_string().contains("too small to make progress"));
    }

    #[test]
    fn generate_series_limited_offset_matches_non_pushdown_float8() {
        let start = Value::Float64(0.0);
        let stop = Value::Float64(2.0);
        let step = Value::Float64(0.1);

        let (all, all_ty) = generate_series_values(&start, &stop, &step).unwrap();
        assert_eq!(all_ty, DataType::Float64);

        let offset = 10;
        let limit = 3;
        let (limited, limited_ty) =
            generate_series_values_limited(&start, &stop, &step, offset, Some(limit), 100).unwrap();
        assert_eq!(limited_ty, DataType::Float64);

        assert_eq!(limited, all[offset..offset + limit].to_vec());
    }

    #[test]
    fn generate_series_float8_progress_guard_errors_even_with_limit() {
        let err = generate_series_values_limited(
            &Value::Float64(1e16),
            &Value::Float64(1e16 + 1e6),
            &Value::Float64(1.0),
            0,
            Some(1),
            10,
        )
        .unwrap_err();
        assert!(err.to_string().contains("too small to make progress"));
    }

    #[test]
    fn generate_series_float8_progress_guard_errors_even_with_large_offset() {
        let err = generate_series_values_limited(
            &Value::Float64(1e16),
            &Value::Float64(1e16 + 1e6),
            &Value::Float64(1.0),
            999_998,
            Some(1),
            10,
        )
        .unwrap_err();
        assert!(err.to_string().contains("too small to make progress"));
    }

    #[test]
    fn generate_series_numeric_max_does_not_panic_or_loop() {
        use std::str::FromStr;

        let max = rust_decimal::Decimal::from_str("79228162514264337593543950335").unwrap();
        let (values, ty) = generate_series_values(
            &Value::Numeric(max),
            &Value::Numeric(max),
            &Value::Numeric(rust_decimal::Decimal::ONE),
        )
        .unwrap();
        assert_eq!(
            ty,
            DataType::Numeric {
                precision: None,
                scale: None,
            }
        );
        assert_eq!(values, vec![Value::Numeric(max)]);
    }

    #[test]
    fn generate_series_numeric_float_step_nan_errors() {
        let err = generate_series_values(
            &Value::Numeric(rust_decimal::Decimal::ONE),
            &Value::Numeric(rust_decimal::Decimal::from(2)),
            &Value::Float64(f64::NAN),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input syntax for type numeric"));
    }

    #[test]
    fn generate_series_numeric_float_step_infinite_errors() {
        let err = generate_series_values(
            &Value::Numeric(rust_decimal::Decimal::ONE),
            &Value::Numeric(rust_decimal::Decimal::from(2)),
            &Value::Float64(f64::INFINITY),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input syntax for type numeric"));
    }

    #[tokio::test]
    async fn view_expansion_guard_detects_recursion() {
        let err = with_view_expansion_stack(async {
            let _outer = ViewExpansionGuard::push("public.v".to_string())?;
            let _inner = ViewExpansionGuard::push("public.v".to_string())?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("recursive view detected"));
    }

    #[tokio::test]
    async fn view_expansion_guard_allows_reuse_after_drop() {
        with_view_expansion_stack(async {
            {
                let _guard = ViewExpansionGuard::push("public.v".to_string())?;
            }
            let _guard = ViewExpansionGuard::push("public.v".to_string())?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap();
    }
}
